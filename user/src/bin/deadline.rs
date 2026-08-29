//! Realtime deadline demo — runs with an EDF deadline when the kernel is
//! built with `KAIROS_SCHED_POLICY=EarliestDeadlineFirst`. Prints a
//! DEADLINE MISS marker whenever it fails to wake on time, so the (mis)be-
//! haviour is observable in the serial log.
#![no_std]
#![no_main]

use kairos;

/// # Safety: raw entry point.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_arg: usize) -> ! {
    let pid = kairos::getpid();
    kairos::print("deadline: task ");
    kairos::print_num(pid);
    kairos::println(" running (EDF demo)");
    // Depending on which of the two demo tasks we are, choose a period:
    // even task id → 200 ms, odd → 100 ms (see user::spawn_deadline_demo).
    let period: u64 = if pid % 2 == 0 { 200 } else { 100 };
    let max_latency: u64 = if period == 200 { 400 } else { 220 };
    loop {
        let t0 = kairos::time_ms();
        kairos::print("deadline:");
        kairos::print_num(pid);
        kairos::print(" wake @t=");
        kairos::print_num(t0);
        kairos::println(" ms");
        kairos::sleep(period);
        let t1 = kairos::time_ms();
        if t1.wrapping_sub(t0) > max_latency {
            kairos::print("deadline:");
            kairos::print_num(pid);
            kairos::println(" DEADLINE MISS (latency exceeds budget)");
        }
    }
}