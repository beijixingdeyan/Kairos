//! Compile-time configuration ("配置即代码").
//!
//! Every kernel policy knob is baked at build time from a plain `KAIROS_*`
//! environment variable by `build.rs`, which writes the final values as
//! literals into `$OUT_DIR/kairos_cfg.rs` (included below). Nothing here is
//! hard-coded in kernel source: changing a variable and rebuilding changes
//! behaviour.

/// Scheduling policy compiled into the kernel.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum SchedPolicy {
    #[default]
    /// Plain round-robin with a fixed time quantum.
    RoundRobin,
    /// Round-robin where each task's share scales with its weight.
    WeightedRoundRobin,
    /// Earliest-deadline-first for tasks with deadlines.
    EarliestDeadlineFirst,
}

/// Log level compiled into the kernel.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Default)]
pub enum LogLevel {
    Error,
    Warn,
    #[default]
    Info,
    Debug,
    Trace,
}

// -------------------------------------------------------------------------
// Generated constants (literals — no const string parsing needed).
// -------------------------------------------------------------------------

include!(concat!(env!("OUT_DIR"), "/kairos_cfg.rs"));

/// Kernel heap size in bytes (mapped virtual range).
pub const HEAP_SIZE: usize = HEAP_MIB * 1024 * 1024;

/// Well-known exit codes written to the QEMU ISA debug-exit port (0xf4).
/// QEMU maps `outb(0xf4, n)` to guest exit code `n`.
pub const EXIT_SUCCESS: u32 = 0x10;
pub const EXIT_FAILURE: u32 = 0x11;

// -------------------------------------------------------------------------
// Syscall ABI numbers — the single source of truth shared by the kernel
// dispatcher (`kairos-syscall`) and the user ABI (`user` crate).
// -------------------------------------------------------------------------

pub const SYS_EXIT: u64 = 0;
pub const SYS_YIELD: u64 = 1;
pub const SYS_SLEEP: u64 = 2;
pub const SYS_PRINT: u64 = 3;
pub const SYS_GETPID: u64 = 4;
pub const SYS_TIME: u64 = 5;
pub const SYS_SPAWN: u64 = 6;
pub const SYS_CH_CREATE: u64 = 7;
pub const SYS_CH_CLOSE: u64 = 8;
pub const SYS_SEND: u64 = 9;
pub const SYS_RECV: u64 = 10;
pub const SYS_SEND_FRAME: u64 = 11;
pub const SYS_RECV_FRAME: u64 = 12;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_is_valid() {
        // Whatever the build env says, the enum must be constructible.
        let _ = SCHED_POLICY;
    }

    #[test]
    fn default_quantum_is_sane() {
        assert!((1..=1000).contains(&QUANTUM_MS));
    }

    #[test]
    fn heap_size_is_sane() {
        assert!(HEAP_SIZE >= 4 * 1024 * 1024);
    }

    #[test]
    fn syscall_numbers_are_unique() {
        let nums = [
            SYS_EXIT, SYS_YIELD, SYS_SLEEP, SYS_PRINT, SYS_GETPID, SYS_TIME, SYS_SPAWN,
            SYS_CH_CREATE, SYS_CH_CLOSE, SYS_SEND, SYS_RECV, SYS_SEND_FRAME, SYS_RECV_FRAME,
        ];
        for (i, a) in nums.iter().enumerate() {
            for b in &nums[i + 1..] {
                assert_ne!(a, b);
            }
        }
    }
}