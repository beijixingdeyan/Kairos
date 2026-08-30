//! Kairos kernel: entry point and boot orchestration.
//!
//! Boot flow
//! ---------
//! BIOS firmware (SeaBIOS in QEMU) → `bootloader` crate (FAT32 MBR image) →
//! long mode + paging → returns a [`BootInfo`] → [`kernel_main`] here.
//!
//! `kernel_main` is deliberately a readable, linear script of subsystem
//! initialisation — each step is a module with a documented contract.

#![no_std]
#![no_main]
#![deny(unsafe_op_in_unsafe_fn)]
// Public API surface kept intentionally (kernel modules, self-tests invoked
// conditionally, educational commands) — not everything is wired up in a
// single build configuration.
#![allow(dead_code)]
#![allow(clippy::missing_safety_doc, clippy::too_many_lines)]

// alloc's macros (vec!, format!, …) are *not* in the edition-2024 prelude
// for no_std crates; `#[macro_use]` injects them crate-wide.
#[macro_use]
extern crate alloc;

mod caps;
mod gdt;
mod interrupts;
mod ipc;
mod logger;
mod memory;
mod serial;
mod shell;
mod syscall;
mod task;
mod user;
mod vga;

use bootloader_api::BootInfo;
use kairos_core::config;

/// Re-export for the logging macros (`$crate::LogLevel`).
pub use kairos_core::config::LogLevel;

bootloader_api::entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);

/// Bootloader configuration: the kernel needs the whole physical memory
/// mapped (KASLR-free, deterministic) so we can adopt its page tables
/// through the reported `physical_memory_offset`.
pub static BOOTLOADER_CONFIG: bootloader_api::BootloaderConfig = {
    let mut config = bootloader_api::BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(bootloader_api::config::Mapping::Dynamic);
    config
};

/// Kernel entry point. Never returns.
#[allow(unreachable_code)] // trailing idle loop guards `shell::start() -> !`
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    // 1. The serial console is the very first hardware we touch: everything
    //    after this point can log.
    serial::init();

    // 2. Physical memory, paging, kernel heap.
    if let Err(e) = memory::init(boot_info) {
        error!("kairos: FATAL memory init failed: {e:?}");
        exit_kernel(config::EXIT_FAILURE);
    }

    // 3. Logging & screen.
    logger::init();
    vga::init();

    // 4. CPU structures: GDT(+TSS), IDT, PIC remap, timer.
    gdt::init();
    interrupts::init_idt();
    interrupts::init_pic_and_timer();

    // 5. Tasking + IPC + capability space for the kernel's own tasks, and
    //    the syscall MSRs (GS area) — *before* interrupts come on, so a
    //    timer IRQ can never observe an unset GS base.
    task::init();
    caps::init();
    syscall::init();

    // 6. Interrupts on: from here on the scheduler may preempt us.
    x86_64::instructions::interrupts::enable();

    // 7. Announce.
    banner();

    // 8. In test mode: run the in-kernel test suite, then exit QEMU.
    if config::TEST_MODE {
        info!("test mode: running kernel self-tests");
        let passed = test_runner();
        info!("kernel self-tests: {}", if passed { "ALL PASSED" } else { "FAILED" });
        exit_kernel(if passed { config::EXIT_SUCCESS } else { config::EXIT_FAILURE });
    }

    // 9. Spawn boot-time tasks and hand over to the scheduler.
    task::set_timer_paused(true);
    task::boot_tasks();

    // 10. The kernel's own task: interactive shell (unless disabled).
    shell::start();

    // The scheduler never returns control here; the idle loop.
    loop {
        x86_64::instructions::hlt();
    }
}

/// Write an exit code to the QEMU debug-exit device and stop.
///
/// Under QEMU (our supported dev environment) `outb(0xf4, code)` terminates
/// the VM with exit status `code`. On real hardware or other hypervisors the
/// port is a no-op and we simply halt forever.
pub fn exit_kernel(code: u32) -> ! {
    unsafe {
        x86_64::instructions::port::Port::new(0xf4).write(code as u8);
    }
    loop {
        x86_64::instructions::hlt();
    }
}

/// Kernel panic handler: serial + VGA message, then fail the VM.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    error!("KERNEL PANIC: {info}");
    vga::panic_message(info);
    exit_kernel(config::EXIT_FAILURE);
}

fn banner() {
    info!("------------------------------------------------------------");
    info!(" Kairos Microkernel v0.1.0 — capability-based, deterministic");
    info!(" policy        : {}", policy_name());
    info!(" quantum       : {} ms", config::QUANTUM_MS);
    info!(" kernel heap   : {} MiB", config::HEAP_MIB);
    let mem = memory::summary();
    info!(
        " memory        : {} MiB total, {} MiB usable, {} frames free",
        mem.total_mib,
        mem.usable_mib,
        mem.frames_free
    );
    info!("------------------------------------------------------------");
}

fn policy_name() -> &'static str {
    match config::SCHED_POLICY {
        config::SchedPolicy::RoundRobin => "round-robin",
        config::SchedPolicy::WeightedRoundRobin => "weighted-round-robin",
        config::SchedPolicy::EarliestDeadlineFirst => "edf",
    }
}

/// The in-kernel test suite, run in test mode.
fn test_runner() -> bool {
    let mut ok = true;
    let mut report = |name: &str, r: bool| {
        serial::write_line(&format!("[tests] {name}: {}", if r { "PASS" } else { "FAIL" }));
        ok &= r;
    };
    report("logger", logger::test_echo());
    report("memory", memory::run_tests());
    report("ipc", ipc::run_tests());
    report("task", task::run_tests());
    report("syscall", syscall::run_tests());
    report("user", user::run_tests());
    report("shell", shell::run_tests());
    ok
}
