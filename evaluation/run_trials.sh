#!/bin/bash

# ------------------------------------------------------------
# Configuration parameters for experiment orchestration.
# NUM_RUNS controls iteration count; CONCURRENCY determines
# how many trigger processes execute in parallel.
# The mountpoint, output directories, and binary paths are
# defined statically for repeatable collection.
# ------------------------------------------------------------
NUM_RUNS=${1:-100}
CONCURRENCY=${2:-2}
MOUNT="mnt"
RESULTS_DIR="artefacts"

DAEMON_BIN="./target/release/reentryfs"
TRIGGER_BIN="./client"
TARGET_FILE="target_file"
TRIGGER_NAME="client"

# ------------------------------------------------------------
# Environment setup: create mountpoint and results directory.
# Initialize output files and headers for CSV and stack logs.
# Job control messages are disabled to avoid noise.
# ------------------------------------------------------------
mkdir -p $MOUNT $RESULTS_DIR

LOG_FILE="$RESULTS_DIR/abba_metrics.csv"
STACK_LOG="$RESULTS_DIR/stack_log.txt"
DAEMON_LOG="$RESULTS_DIR/daemon_output.log"

echo "Run,Status,BlockedThreads,WaitQueue" > $LOG_FILE
echo "=== D-State Stack Traces ===" > $STACK_LOG

set +m

echo "Starting collection: $NUM_RUNS runs with concurrency $CONCURRENCY..."
echo "---------------------------------------------------------------"
printf "%-5s | %-15s | %-10s | %-10s\n" "Run" "Status" "Blocked" "WaitQ"
echo "---------------------------------------------------------------"

# ------------------------------------------------------------
# Main experimental loop conducting NUM_RUNS iterations.
# Each iteration launches the FUSE daemon, triggers the
# userfaultfd-driven race, collects system-level metrics,
# and cleans up to return to a known-good baseline.
# ------------------------------------------------------------
for ((i=1; i<=NUM_RUNS; i++)); do

  # Launch the FUSE daemon with fault injection enabled.
  sudo BLOCK_FAULT=1 $DAEMON_BIN $MOUNT > $DAEMON_LOG 2>&1 &
  DAEMON_PID=$!
  disown $DAEMON_PID

  sleep 2

  # Execute trigger binaries under concurrency settings.
  # Triggers operate in background subshells; disown is used
  # to suppress shell notifications and isolate failures.
  for ((j=1; j<=CONCURRENCY; j++)); do
    ( $TRIGGER_BIN "$MOUNT/$TARGET_FILE" >/dev/null 2>&1 ) &
    PID=$!
    disown $PID
  done

  # Allow time for potential deadlock formation and fault
  # handling paths to execute within the kernel.
  sleep 2

  # --------------------------------------------------------
  # Metric Collection Phase
  # --------------------------------------------------------

  # Identify trigger processes stuck in D-state. These
  # represent kernel threads blocked on the AB-BA ordering.
  BLOCKED_PIDS=$(ps -eo pid,state,comm | grep "$TRIGGER_NAME" | grep "D" | awk '{print $1}')
  BLOCKED_COUNT=$(echo "$BLOCKED_PIDS" | wc -w | xargs)

  # Derive iteration status based on observed blockage,
  # daemon health, or extracted hold timing.
  if [ "$BLOCKED_COUNT" -gt 0 ]; then
      STATUS="DEADLOCK"
  else
      TIME_VAL=$(grep "Reentrancy hold" $DAEMON_LOG | tail -1 | awk '{print $5}' | sed 's/ms//')
      if [ -z "$TIME_VAL" ]; then
          if ! kill -0 $DAEMON_PID 2>/dev/null; then
             STATUS="CRASH"
          else
             STATUS="HANG"
          fi
      else
          STATUS="${TIME_VAL}ms"
      fi
  fi

  # Capture kernel stack traces for tasks detected in D-state.
  if [ ! -z "$BLOCKED_PIDS" ]; then
      for pid in $BLOCKED_PIDS; do
          echo "--- Run $i: PID $pid is Deadlocked (D-State) ---" | sudo tee -a $STACK_LOG >/dev/null
          sudo cat "/proc/$pid/stack" 2>/dev/null | sudo tee -a $STACK_LOG >/dev/null
          echo "" | sudo tee -a $STACK_LOG >/dev/null
      done
  fi

  # Extract current FUSE wait queue depth directly from
  # kernel connection state, with fallback to zero.
  WAITQ=$(sudo cat /sys/fs/fuse/connections/*/waiting 2>/dev/null || echo 0)

  # Log iteration metrics in CSV format.
  echo "$i,$STATUS,$BLOCKED_COUNT,$WAITQ" >> $LOG_FILE

  # Emit formatted status summary for interactive use.
  printf "%-5s | %-15s | %-10s | %-10s\n" "$i" "$STATUS" "$BLOCKED_COUNT" "$WAITQ"

  # --------------------------------------------------------
  # Cleanup Phase
  # Reset system state by unmounting, terminating daemon,
  # removing triggers, and discarding transient logs.
  # --------------------------------------------------------
  sudo fusermount -uz $MOUNT >/dev/null 2>&1
  sudo kill -9 $DAEMON_PID >/dev/null 2>&1
  sudo pkill -9 -x "$TRIGGER_NAME" >/dev/null 2>&1
  rm -f $DAEMON_LOG
done

echo "---------------------------------------------------------------"
echo "Done."
echo "CSV Data: $LOG_FILE"
echo "Stack Traces: $STACK_LOG"
