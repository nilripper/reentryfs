FROM ubuntu:24.04
LABEL description="FUSE Reentrancy Deadlock (AB-BA)"

ENV DEBIAN_FRONTEND=noninteractive
ENV RUSTUP_HOME=/opt/rust
ENV CARGO_HOME=/opt/cargo
ENV PATH=/opt/cargo/bin:$PATH

# -----------------------------------------------------------------------------
# System Dependencies
# -----------------------------------------------------------------------------
RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential \
    ca-certificates \
    curl \
    fuse3 \
    libfuse3-dev \
    liburing-dev \
    pkg-config \
    strace \
    trace-cmd \
    linux-tools-common \
    linux-tools-generic \
    git \
    sudo \
    vim-tiny \
    && rm -rf /var/lib/apt/lists/*

# -----------------------------------------------------------------------------
# Rust Toolchain
# -----------------------------------------------------------------------------
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal

# -----------------------------------------------------------------------------
# FUSE Configuration and User Setup
# -----------------------------------------------------------------------------
RUN echo "user_allow_other" >> /etc/fuse.conf && \
    useradd -m -s /bin/bash fuseuser && \
    echo "fuseuser ALL=(root) NOPASSWD: ALL" >> /etc/sudoers && \
    chown -R fuseuser:fuseuser /opt/cargo /opt/rust

USER fuseuser
WORKDIR /home/fuseuser/poc

# -----------------------------------------------------------------------------
# Source and Build Steps
# -----------------------------------------------------------------------------
COPY --chown=fuseuser:fuseuser . .

# Rust FUSE daemon
RUN cargo build --release

# C exploit binary
RUN gcc -O2 -o client ./exploit/client.c -luring

# Ensure harness is runnable
RUN chmod +x ./evaluation/run_trials.sh

# -----------------------------------------------------------------------------
# Default Shell
# -----------------------------------------------------------------------------
CMD ["/bin/bash"]

# =============================================================================
# Usage
# =============================================================================
# Build:
#   docker build -t reentryfs .
#
# Run (requires --privileged for FUSE):
#   docker run --rm -it --privileged --cap-add SYS_ADMIN \
#       -v /dev/fuse:/dev/fuse \
#       reentryfs
#
# Inside container:
#   mkdir -p mnt
#   sudo BLOCK_FAULT=1 ./target/release/reentryfs mnt & sleep 1 &&
#   ./client mnt/target_file
#
# Automated runs:
#   ./evaluation/run_trials.sh 100 2
# =============================================================================
