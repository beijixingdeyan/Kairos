//! Periodic counter — demonstrates preemption, sleep and (indirectly) the
//! scheduler's fairness: every task gets CPU time, even while the counter
//! loops forever.
#![no_std]
#![no_main]

use kairos;

/// # Safety: raw entry point.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_arg: usize) -> ! {
    let mut n: u64 = 0;
    kairos::print("counter: start, pid=");
    kairos::print_num(kairos::getpid());
    kairos::println("");
    loop {
        kairos::print("counter:");
        kairos::print_num(n);
        kairos::print(" @t=");
        kairos::print_num(kairos::time_ms());
        kairos::println(" ms");
        n += 1;
        kairos::sleep(500);
    }
}