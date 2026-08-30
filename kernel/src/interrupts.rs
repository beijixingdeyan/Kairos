//! Interrupt & exception handling: IDT, PIC remapping, PIT timer.
//!
//! Every vector runs through a small assembly stub that:
//! 1. pushes the general-purpose registers (see [`CpuFrame`]),
//! 2. calls a Rust handler, and
//! 3. restores the registers —possibly **from a different task's frame**,
//!    which is exactly how preemptive context switching happens.
//!
//! Layout on the stack at the Rust handler (low →high):
//! `[r15 —rbp][err][vec][rip][cs][rflags][rsp][ss]` where the
//! `[rip .. ss]` tail is pushed by the CPU (only `[rip][cs][rflags]` on a
//! ring-0 interrupt).

use x86_64::instructions::port::Port;
use x86_64::structures::idt::InterruptDescriptorTable;
use x86_64::VirtAddr;

use crate::serial;

/// Register snapshot pushed by the interrupt stubs.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct CpuFrame {
    pub r15: u64,
    pub r14: u64,
    pub r13: u64,
    pub r12: u64,
    pub r11: u64,
    pub r10: u64,
    pub r9: u64,
    pub r8: u64,
    pub rdi: u64,
    pub rsi: u64,
    pub rdx: u64,
    pub rcx: u64,
    pub rbx: u64,
    pub rax: u64,
    pub rbp: u64,
    /// Error code pushed by the stub (0 when the CPU provides none).
    pub err: u64,
    /// Vector number pushed by the stub.
    pub vec: u64,
    /// Interrupted instruction pointer (CPU-pushed).
    pub rip: u64,
    pub cs: u64,
    pub rflags: u64,
    /// Present only for ring-3 →ring-0 transitions.
    pub user_rsp: u64,
    pub user_ss: u64,
}

impl CpuFrame {
    /// Zeroed frame used to build initial task contexts.
    pub const fn zeroed() -> Self {
        Self {
            r15: 0,
            r14: 0,
            r13: 0,
            r12: 0,
            r11: 0,
            r10: 0,
            r9: 0,
            r8: 0,
            rdi: 0,
            rsi: 0,
            rdx: 0,
            rcx: 0,
            rbx: 0,
            rax: 0,
            rbp: 0,
            err: 0,
            vec: 0,
            rip: 0,
            cs: 0,
            rflags: 0,
            user_rsp: 0,
            user_ss: 0,
        }
    }
}

static IDT: spin::Lazy<InterruptDescriptorTable> = spin::Lazy::new(|| {
    let mut idt = InterruptDescriptorTable::new();
    // Our handlers are hand-written asm stubs (`extern "C" fn()`); point the
    // IDT entries at their addresses directly (the x86_64 crate's typed
    // `set_handler_fn` expects the x86-interrupt ABI, which we do not use).
    // # Safety: every address below is a valid, executable stub that runs to
    // `ud2` (exceptions) or `iretq` (IRQs) and never returns normally.
    unsafe {
        // Vectors without error code (indexable). 15/22..=27/31 are reserved
        // (the x86_64 crate panics on indexing them), 18 is index-panicking
        // too and set below via the `machine_check` field.
        for i in [0u8, 1, 2, 3, 4, 5, 6, 7, 9, 16, 19, 20, 28] {
            let addr = VirtAddr::new(match i {
                0 => exception_0 as *const () as u64,
                1 => exception_1 as *const () as u64,
                2 => exception_2 as *const () as u64,
                3 => exception_3 as *const () as u64,
                4 => exception_4 as *const () as u64,
                5 => exception_5 as *const () as u64,
                6 => exception_6 as *const () as u64,
                7 => exception_7 as *const () as u64,
                9 => exception_9 as *const () as u64,
                16 => exception_16 as *const () as u64,
                19 => exception_19 as *const () as u64,
                20 => exception_20 as *const () as u64,
                28 => exception_28 as *const () as u64,
                _ => unreachable!(),
            });
            idt[i].set_handler_addr(addr);
        }
        // Error-code and diverging exceptions are NOT indexable in the
        // x86_64 crate; use the named fields.
        idt.double_fault
            .set_handler_addr(VirtAddr::new(exception_8 as *const () as u64));
        idt.invalid_tss
            .set_handler_addr(VirtAddr::new(exception_10 as *const () as u64));
        idt.segment_not_present
            .set_handler_addr(VirtAddr::new(exception_11 as *const () as u64));
        idt.stack_segment_fault
            .set_handler_addr(VirtAddr::new(exception_12 as *const () as u64));
        idt.general_protection_fault
            .set_handler_addr(VirtAddr::new(exception_13 as *const () as u64));
        idt.page_fault
            .set_handler_addr(VirtAddr::new(exception_14 as *const () as u64));
        idt.alignment_check
            .set_handler_addr(VirtAddr::new(exception_17 as *const () as u64));
        idt.machine_check
            .set_handler_addr(VirtAddr::new(exception_18 as *const () as u64));
        idt.cp_protection_exception
            .set_handler_addr(VirtAddr::new(exception_21 as *const () as u64));
        idt.vmm_communication_exception
            .set_handler_addr(VirtAddr::new(exception_29 as *const () as u64));
        idt.security_exception
            .set_handler_addr(VirtAddr::new(exception_30 as *const () as u64));
        idt[32].set_handler_addr(VirtAddr::new(irq_timer as *const () as u64));
        for v in 33..=47u8 {
            idt[v].set_handler_addr(VirtAddr::new(irq_other as *const () as u64));
        }
    }
    idt
});

pub fn init_idt() {
    IDT.load();
}

// -------------------------------------------------------------------------
// Exception names (vector →human-readable)
// -------------------------------------------------------------------------

const EXCEPTION_NAMES: [&str; 32] = [
    "divide by zero",
    "debug",
    "non-maskable interrupt",
    "breakpoint",
    "overflow",
    "bound range exceeded",
    "invalid opcode",
    "device not available",
    "double fault",
    "coprocessor segment overrun",
    "invalid TSS",
    "segment not present",
    "stack-segment fault",
    "general protection fault",
    "page fault",
    "reserved",
    "x87 floating-point exception",
    "alignment check",
    "machine check",
    "SIMD floating-point exception",
    "virtualization exception",
    "control protection exception",
    "reserved",
    "reserved",
    "reserved",
    "reserved",
    "reserved",
    "reserved",
    "hypervisor injection exception",
    "VMM communication exception",
    "security exception",
    "reserved",
];

/// Push layout for the stubs below (two words below the 15 GPRs):
///   no-error vectors: `push 0; push vec` →[gprs][vec][0][rip][cs][flags]
///   error-code vectors: `push vec` only →[gprs][vec][err][rip][cs][flags]
/// So the handler always sees `vec` at offset 15*8 and `err` at 16*8.
macro_rules! exception_stub {
    ($name:ident, $vec:literal, no_err) => {
        core::arch::global_asm!(
            concat!(
                ".pushsection .text\n",
                ".balign 16\n",
                ".global ", stringify!($name), "\n",
                stringify!($name), ":\n",
                // Layout `[….][err@120][vec@128]`: push vec first (lands at
                // the higher offset), then the dummy error code below it.
                "push ", $vec, "\n",
                "push 0\n", // dummy error code
                "push rbp\npush rax\npush rbx\npush rcx\npush rdx\n",
                "push rsi\npush rdi\npush r8\npush r9\npush r10\n",
                "push r11\npush r12\npush r13\npush r14\npush r15\n",
                "mov rdi, rsp\n",
                "call kairos_exception_handler\n",
                "ud2\n",
            ),
            options(),
        );

        unsafe extern "C" { fn $name(); }
    };
    ($name:ident, $vec:literal, with_err) => {
        core::arch::global_asm!(
            concat!(
                ".pushsection .text\n",
                ".balign 16\n",
                ".global ", stringify!($name), "\n",
                stringify!($name), ":\n",
                // CPU has pushed the error code already.
                "push ", $vec, "\n",
                "push rbp\npush rax\npush rbx\npush rcx\npush rdx\n",
                "push rsi\npush rdi\npush r8\npush r9\npush r10\n",
                "push r11\npush r12\npush r13\npush r14\npush r15\n",
                "mov rdi, rsp\n",
                "call kairos_exception_handler\n",
                "ud2\n",
            ),
            options(),
        );

        unsafe extern "C" { fn $name(); }
    };
}

exception_stub!(exception_0, 0, no_err);
exception_stub!(exception_1, 1, no_err);
exception_stub!(exception_2, 2, no_err);
exception_stub!(exception_3, 3, no_err);
exception_stub!(exception_4, 4, no_err);
exception_stub!(exception_5, 5, no_err);
exception_stub!(exception_6, 6, no_err);
exception_stub!(exception_7, 7, no_err);
exception_stub!(exception_8, 8, with_err);
exception_stub!(exception_9, 9, no_err);
exception_stub!(exception_10, 10, with_err);
exception_stub!(exception_11, 11, with_err);
exception_stub!(exception_12, 12, with_err);
exception_stub!(exception_13, 13, with_err);
exception_stub!(exception_14, 14, with_err);
exception_stub!(exception_15, 15, no_err);
exception_stub!(exception_16, 16, no_err);
exception_stub!(exception_17, 17, with_err);
exception_stub!(exception_18, 18, no_err);
exception_stub!(exception_19, 19, no_err);
exception_stub!(exception_20, 20, no_err);
exception_stub!(exception_21, 21, with_err);
exception_stub!(exception_22, 22, no_err);
exception_stub!(exception_23, 23, no_err);
exception_stub!(exception_24, 24, no_err);
exception_stub!(exception_25, 25, no_err);
exception_stub!(exception_26, 26, no_err);
exception_stub!(exception_27, 27, no_err);
exception_stub!(exception_28, 28, no_err);
exception_stub!(exception_29, 29, with_err);
exception_stub!(exception_30, 30, with_err);
exception_stub!(exception_31, 31, no_err);

/// Called from the exception stubs; prints a diagnostic and dies.
#[unsafe(no_mangle)]
extern "C" fn kairos_exception_handler(frame: &CpuFrame) -> ! {
    let name = EXCEPTION_NAMES
        .get(frame.vec as usize)
        .copied()
        .unwrap_or("unknown");

    serial::write_line("------------------------------------------------");
    serial::write_line("KERNEL EXCEPTION (unhandled)");
    serial::write_line(&format!("  vector : {}", frame.vec));
    serial::write_line(&format!("  name   : {name}"));
    serial::write_line(&format!(
        "  rip    = 0x{:016x}, cs = 0x{:04x}, rflags = 0x{:016x}",
        frame.rip, frame.cs, frame.rflags
    ));
    serial::write_line(&format!(
        "  rax    = 0x{:016x}, rbx = 0x{:016x}, rcx = 0x{:016x}",
        frame.rax, frame.rbx, frame.rcx
    ));
    serial::write_line(&format!(
        "  rdi    = 0x{:016x}, rsi = 0x{:016x}, rbp = 0x{:016x}",
        frame.rdi, frame.rsi, frame.rbp
    ));
    if frame.vec == 14 {
        let cr2 = x86_64::registers::control::Cr2::read();
        serial::write_line(&format!("  fault addr (cr2) = 0x{:016x}", cr2.map_or(0, |a| a.as_u64())));
        let err = frame.err as u16;
        serial::write_line(&format!(
            "  page fault: present={} write={} user={}",
            err & 1 != 0,
            err & 2 != 0,
            err & 4 != 0
        ));
    }
    serial::write_line("------------------------------------------------");
    crate::exit_kernel(kairos_core::config::EXIT_FAILURE)
}

// -------------------------------------------------------------------------
// IRQ stubs
// -------------------------------------------------------------------------

/// Set by the scheduler shortly before the IRQ stub restores a frame:
/// `true` →the restored task runs in ring 3, so the stub must `swapgs`
/// back (GS base: kernel →user) right before `iretq`.
///
/// The value is mirrored into the kernel GS area (scratch1, offset 8) so the
/// assembly stubs can read it without a RIP-relative symbol reference.
#[unsafe(no_mangle)]
pub static KAIROS_USER_RET: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

pub fn set_user_ret_flag(user: bool) {
    KAIROS_USER_RET.store(user, core::sync::atomic::Ordering::SeqCst);
    crate::syscall::gs_area()
        .scratch1
        .store(user as u64, core::sync::atomic::Ordering::SeqCst);
}

macro_rules! irq_stub {
    ($name:ident, $vec:literal) => {
        core::arch::global_asm!(
            concat!(
                ".pushsection .text\n",
                ".balign 16\n",
                ".global ", stringify!($name), "\n",
                stringify!($name), ":\n",
                // If the interrupt came from ring 3, enter with kernel GS
                // (so kernel code and the syscall path see the kernel area).
                "test byte ptr [rsp + 8], 3\n", // CS slot at rsp+8 (ring0/ring3)
                "jz 1f\n",
                "swapgs\n",
                "1:\n",
                "push ", $vec, "\n", // vec lands at 128 (see CpuFrame)
                "push 0\n",          // err at 120
                "push rbp\npush rax\npush rbx\npush rcx\npush rdx\n",
                "push rsi\npush rdi\npush r8\npush r9\npush r10\n",
                "push r11\npush r12\npush r13\npush r14\npush r15\n",
                "mov rdi, rsp\n",
                "call kairos_irq_handler\n",
                // rax = frame to restore (possibly another task's)
                "mov rsp, rax\n",
                "pop r15\npop r14\npop r13\npop r12\npop r11\npop r10\n",
                "pop r9\npop r8\npop rdi\npop rsi\npop rdx\npop rcx\n",
                "pop rbx\npop rax\npop rbp\n",
                "add rsp, 16\n",
                // swapgs back to user GS if we restore a ring-3 task; the
                // flag lives in the kernel GS area (scratch1, offset 8).
                "mov r10, qword ptr gs:[8]\n",
                "test r10b, r10b\n",
                "jz 2f\n",
                "swapgs\n",
                "2:\n",
                "iretq\n",
            ),
            options(),
        );

        unsafe extern "C" { fn $name(); }
    };
}

irq_stub!(irq_timer, 32);
irq_stub!(irq_other, 33);

/// Jump the CPU into a pre-built task frame without an interrupt (used to
/// start the very first task). Never returns.


/// Jump the CPU into a pre-built task frame without an interrupt (used to
/// start the very first task). Never returns.
#[unsafe(naked)]
pub unsafe extern "C" fn enter_task_frame(frame: *mut CpuFrame) -> ! {
    core::arch::naked_asm!(
        "mov rsp, rdi",
        "pop r15", "pop r14", "pop r13", "pop r12", "pop r11", "pop r10",
        "pop r9", "pop r8", "pop rdi", "pop rsi", "pop rdx", "pop rcx",
        "pop rbx", "pop rax", "pop rbp",
        "add rsp, 16",
        "mov r10, qword ptr gs:[8]",
        "test r10b, r10b",
        "jz 1f",
        "swapgs",
        "1:",
        "iretq",
    )
}

/// Kept for future IRQ flexibility; the IDT entries 33..=47 all use
/// `irq_other`, which just acks and logs (they stay masked anyway).
#[unsafe(no_mangle)]
extern "C" fn kairos_irq_handler(frame: *mut CpuFrame) -> *mut CpuFrame {
    // Ack the master PIC unconditionally (EOI), then dispatch.
    unsafe { pic_eoi() };
    crate::task::on_irq_after_eoi(frame)
}

// -------------------------------------------------------------------------
// PIC (8259) remapping + PIT (8254) setup
// -------------------------------------------------------------------------

fn io_wait() {
    unsafe {
        Port::<u8>::new(0x80).write(0);
    }
}

/// Remap IRQs 0-15 to vectors 32-47 and mask everything but the timer.
fn pic_remap() {
    let mut cmd_m = Port::<u8>::new(0x20);
    let mut data_m = Port::<u8>::new(0x21);
    let mut cmd_s = Port::<u8>::new(0xA0);
    let mut data_s = Port::<u8>::new(0xA1);

    unsafe {
        cmd_m.write(0x11);
        io_wait();
        cmd_s.write(0x11);
        io_wait();

        data_m.write(0x20); // master offset 32
        io_wait();
        data_s.write(0x28); // slave offset 40
        io_wait();

        data_m.write(0x04); // slave on IRQ2
        io_wait();
        data_s.write(0x02);
        io_wait();

        data_m.write(0x01);
        io_wait();
        data_s.write(0x01);
        io_wait();

        // Mask all except IRQ0 (timer) on the master.
        data_m.write(0xFE);
        data_s.write(0xFF);
    }
}

/// Start PIT channel 0 at the compiled tick rate (default 1 kHz → 1 ms
/// tick). The rate is `kairos_core::config::TICK_HZ`（内核编译期配置
/// `KAIROS_TICK_HZ`），可在模拟器拖慢虚拟时钟的宿主上补偿——见
/// docs/ARCHITECTURE.md 第 8 节。
fn pit_init() {
    const PIT_FREQ: u32 = 1_193_182;
    let target_hz = kairos_core::config::TICK_HZ.clamp(1, PIT_FREQ as u64 / 2) as u32;
    let divisor = (PIT_FREQ / target_hz) as u16;

    let mut cmd = Port::<u8>::new(0x43);
    let mut ch0 = Port::<u8>::new(0x40);
    unsafe {
        cmd.write(0x36); // channel 0, lobyte/hibyte, mode 3, binary
        ch0.write(divisor as u8);
        ch0.write((divisor >> 8) as u8);
    }
}

/// Init PIC + PIT. Call before enabling interrupts.
pub fn init_pic_and_timer() {
    pic_remap();
    pit_init();
    x86_64::instructions::interrupts::disable();
}

/// Send end-of-interrupt to the master PIC.
pub unsafe fn pic_eoi() {
    // # Safety: called only with the master PIC masked appropriately.
    unsafe {
        Port::<u8>::new(0x20).write(0x20);
    }
}

/// Convenience for tests: does the IDT contain our timer handler?
pub fn sanity() -> bool {
    // 0.15 exposes no `options()` reader; the timer entry having a non-zero
    // handler address is a sufficient sanity check.
    IDT[32].handler_addr() != x86_64::VirtAddr::zero()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_layout_is_stable() {
        // CpuFrame is used from assembly; fields must be in the pushed order.
        assert_eq!(core::mem::offset_of!(CpuFrame, r15), 0);
        assert_eq!(core::mem::offset_of!(CpuFrame, rbp), 14 * 8);
        assert_eq!(core::mem::offset_of!(CpuFrame, err), 15 * 8);
        assert_eq!(core::mem::offset_of!(CpuFrame, vec), 16 * 8);
        assert_eq!(core::mem::offset_of!(CpuFrame, rip), 17 * 8);
        assert_eq!(core::mem::size_of::<CpuFrame>(), 22 * 8);
    }
}




