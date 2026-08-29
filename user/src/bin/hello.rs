//! First user program: prints its pid and exits.
#![no_std]
#![no_main]

use kairos;

/// # Safety: raw entry point — the kernel jumps here with `arg` in `rdi`.
#[unsafe(no_mangle)]
pub extern "C" fn _start(_arg: usize) -> ! {
    kairos::println("hello: Hello from ring 3!");
    kairos::print("hello: pid=");
    kairos::print_num(kairos::getpid());
    kairos::println(" — first user-space program on Kairos");
    kairos::exit(0)
}