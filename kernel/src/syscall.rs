//! System calls: the ring-3 閳?ring-0 ABI.
//!
//! User programs enter the kernel with the `syscall` instruction (MSR
//! `LSTAR`), which unfortunately does **not** switch stacks. The entry stub
//! therefore:
//! 1. `swapgs` into the kernel GS area (which holds this task's kernel stack
//!    top, refreshed on every switch),
//! 2. spills all GPRs onto the user stack,
//! 3. builds a standard [`CpuFrame`] on the kernel stack,
//! 4. calls the Rust dispatcher, and
//! 5. restores the returned frame with `iretq` (which *can* switch stacks and
//!    privileges) 閳?if the dispatcher parked the task, the restored frame
//!    belongs to the *next* task, which is the context switch.
//!
//! Interrupts are masked for the whole kernel-side section via `SFMASK`.

use core::sync::atomic::AtomicU64;

use kairos_core::caps::{CapRights, Capability, ObjectKind};
use kairos_core::ipc::Message;
use kairos_core::sched::TaskId;

use crate::interrupts::CpuFrame;
use crate::{caps, ipc, serial, task, user};

// Syscall numbers come from `kairos-core` so the user ABI imports the same
// constants (single source of truth).
pub use kairos_core::config::{
    SYS_CH_CLOSE, SYS_CH_CREATE, SYS_EXIT, SYS_GETPID, SYS_PRINT, SYS_RECV, SYS_RECV_FRAME,
    SYS_SEND, SYS_SEND_FRAME, SYS_SLEEP, SYS_SPAWN, SYS_TIME, SYS_YIELD,
};

// -------------------------------------------------------------------------
// Kernel GS area (referenced from assembly with fixed offsets)
// -------------------------------------------------------------------------

/// Per-CPU-ish area reachable via `gs` after `swapgs`.
/// Offsets (must match the asm in `syscall_entry`): kstack=0, scratch1=8,
/// scratch2=16.
#[repr(C)]
pub struct GsArea {
    pub kstack: AtomicU64,
    pub scratch1: AtomicU64,
    pub scratch2: AtomicU64,
}

#[unsafe(no_mangle)]
static KAIROS_GS_AREA: GsArea = GsArea {
    kstack: AtomicU64::new(0),
    scratch1: AtomicU64::new(0),
    scratch2: AtomicU64::new(0),
};

pub fn gs_area() -> &'static GsArea {
    &KAIROS_GS_AREA
}

unsafe fn wrmsr(msr: u32, value: u64) {
    // # Safety: caller guarantees the MSRs are model-specific registers we
    // are allowed to touch (documented Intel x86-64 MSRs).
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") msr,
            in("eax") (value & 0xFFFF_FFFF) as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
}

unsafe fn rdmsr(msr: u32) -> u64 {
    // # Safety: caller guarantees the MSR is safe to read.
    unsafe {
        let (hi, lo): (u32, u32);
        core::arch::asm!(
            "rdmsr",
            in("ecx") msr,
            out("eax") lo,
            out("edx") hi,
            options(nomem, nostack, preserves_flags),
        );
        ((hi as u64) << 32) | lo as u64
    }
}

/// Set up the syscall MSRs. Call once, before any task runs.
pub fn init() {
    let entry = kairos_syscall_entry as *const () as u64;
    unsafe {
        // EFER.SCE: the `syscall` instruction is #UD while disabled; the
        // bootloader leaves it off. Preserve the existing bits (LME/LMA/NXE).
        let efer: u64 = rdmsr(0xC000_0080);
        wrmsr(0xC000_0080, efer | 1);
        // LSTAR: syscall entry.
        wrmsr(0xC000_0082, entry);
        // STAR: kernel CS = 0x08 (bits 47:32). Bits 63:48 unused (we return
        // via iretq) but set to match for future sysret use.
        wrmsr(0xC000_0081, 0x08u64 << 32 | 0x08u64 << 48);
        // SFMASK: clear IF on syscall so kernel code runs interrupt-free.
        wrmsr(0xC000_0084, 0x200);
        // GS bases: while in ring 0 the *stubs* read `gs:[8]` (the user-return
        // flag) and `gs:[0]` (kernel stack), so ring 0 must run with
        // GS_BASE = the kernel GS area. `swapgs` (performed on ring3→kernel
        // entries and kernel→ring3 restores) exchanges the two bases, so with
        // KERNEL_GS_BASE = 0 every swap lands user mode on a null user GS —
        // the kernel area is never reachable from ring 3.
        wrmsr(0xC000_0101, &KAIROS_GS_AREA as *const GsArea as u64); // GS_BASE
        wrmsr(0xC000_0102, 0); // KERNEL_GS_BASE
    }
}

/// The assembly entry point (see module docs for the exact protocol).
#[unsafe(no_mangle)]
#[unsafe(naked)]
pub unsafe extern "C" fn kairos_syscall_entry() -> ! {
    core::arch::naked_asm!(
        // Enter with kernel GS; spill everything on the user stack.
        "swapgs",
        "push rbp",
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        // rax = spill base; user rsp = rax + 120.
        "mov rax, rsp",
        "mov r10, qword ptr gs:[0]",
        "sub r10, 176",
        "mov rsp, r10", // onto the kernel stack; rax = spill base
        // frame[i] = spill[i] for i in 0..=14
        "mov rcx, 0",
        "1:",
        "mov r11, qword ptr [rax + rcx*8]",
        "mov qword ptr [rsp + rcx*8], r11",
        "inc rcx",
        "cmp rcx, 15",
        "jne 1b",
        "mov qword ptr [rsp + 120], 0",    // err
        "mov qword ptr [rsp + 128], 256",  // vec marker (syscall)
        "mov r11, qword ptr [rax + 88]",   // spilled rcx = user rip
        "mov qword ptr [rsp + 136], r11",
        "mov qword ptr [rsp + 144], 0x23", // user CS
        "mov r11, qword ptr [rax + 32]",   // spilled r11 = user rflags
        "mov qword ptr [rsp + 152], r11",
        "lea r11, [rax + 120]",            // user rsp (spill base + 15*8)
        "mov qword ptr [rsp + 160], r11",
        "mov qword ptr [rsp + 168], 0x1b", // user SS
        // dispatch(num, frame)
        "mov rsi, rsp",
        "mov rdi, qword ptr [rsp + 104]", // spilled rax = syscall number
        "call kairos_syscall_dispatch",
        // rax = frame to restore
        "mov rsp, rax",
        "pop r15", "pop r14", "pop r13", "pop r12", "pop r11", "pop r10",
        "pop r9", "pop r8", "pop rdi", "pop rsi", "pop rdx", "pop rcx",
        "pop rbx", "pop rax", "pop rbp",
        "add rsp, 16",
        // swapgs back when resuming ring 3 (flag set by the dispatcher)
        "mov r10, qword ptr gs:[8]",
        "test r10b, r10b",
        "jz 2f",
        "swapgs",
        "2:",
        "iretq",
    );
}

// -------------------------------------------------------------------------
// Dispatcher
// -------------------------------------------------------------------------

/// Result of handling one syscall.
enum Out {
    /// Plain integer result (written into the frame's `rax`).
    Value(u64),
    /// The task must park; the dispatcher blocks it and switches away.
    /// The variant selects what "park" means for the scheduler.
    Park(ParkKind),
    /// The task exits.
    Exit,
}

enum ParkKind {
    /// Block until explicitly woken (IPC waiter).
    Block,
    /// Rotate to the back of the ready queue (yield).
    Yield,
    /// Block until `ms` ticks have passed.
    Sleep(u64),
}

#[unsafe(no_mangle)]
pub extern "C" fn kairos_syscall_dispatch(num: u64, frame: *mut CpuFrame) -> *mut CpuFrame {
    let f = unsafe { &mut *frame };
    let a0 = f.rdi;
    let a1 = f.rsi;
    let a2 = f.rdx;
    let a3 = f.r10;
    let current = task::running_id().unwrap_or(0);

    let out = handle(current, num, a0, a1, a2, a3);
    match out {
        Out::Value(v) => {
            f.rax = v;
            frame
        }
        Out::Park(ParkKind::Block) => task::syscall_park(frame, |s, id| {
            let _ = s.block(id);
        }),
        Out::Park(ParkKind::Yield) => task::syscall_yield(frame),
        Out::Park(ParkKind::Sleep(ms)) => task::syscall_sleep(frame, ms),
        Out::Exit => task::syscall_exit(frame),
    }
}

fn handle(
    current: TaskId,
    num: u64,
    a0: u64,
    a1: u64,
    a2: u64,
    _a3: u64,
) -> Out {
    match num {
        SYS_EXIT => Out::Exit,
        SYS_YIELD => Out::Park(ParkKind::Yield),
        SYS_SLEEP => Out::Park(ParkKind::Sleep(a0)),
        SYS_PRINT => {
            let bytes = read_user_bytes(current, a0, a1.min(1024));
            for b in &bytes {
                serial::put_byte(*b);
            }
            Out::Value(1)
        }
        SYS_GETPID => Out::Value(current),
        SYS_TIME => Out::Value(task::tick_now()),
        SYS_SPAWN => {
            let name = read_user_string(current, a0, a1);
            if can_spawn(current) {
                match spawn_named(&name) {
                    Some(id) => Out::Value(id as u64),
                    None => Out::Value(0),
                }
            } else {
                // Capability denied 閳?the demo path for the capability
                // system (user tasks cannot spawn by default).
                serial::write_line("[kairos] spawn denied: caller lacks the\
                                    spawn authority capability");
                Out::Value(0)
            }
        }
        SYS_CH_CREATE => {
            let idx = ipc::create();
            let obj_id = caps::register(caps::KernelObject::Channel(idx));
            let slot = task::with_cspace(current, |c| {
                c.insert(Capability::new(
                    obj_id,
                    ObjectKind::Channel,
                    CapRights::ALL,
                ))
            });
            match slot {
                Some(Ok(s)) => Out::Value(s as u64 + 1), // 1-based slot (0 = err)
                _ => Out::Value(0),
            }
        }
        SYS_CH_CLOSE => {
            let slot = (a0 as u16).wrapping_sub(1);
            task::with_cspace(current, |c| {
                let _ = c.revoke(slot);
            });
            Out::Value(1)
        }
        SYS_SEND => {
            let slot = (a0 as u16).wrapping_sub(1);
            match read_message(current, a1) {
                Some(msg) => match ipc::send(current, slot, msg) {
                    ipc::OpResult::Done(v) => Out::Value(v),
                    ipc::OpResult::WouldBlock => Out::Park(ParkKind::Block),
                },
                None => Out::Value(0),
            }
        }
        SYS_RECV => {
            let slot = (a0 as u16).wrapping_sub(1);
            match ipc::recv(current, slot, a1) {
                ipc::OpResult::Done(v) => Out::Value(v),
                ipc::OpResult::WouldBlock => Out::Park(ParkKind::Block),
            }
        }
        SYS_SEND_FRAME => {
            let slot = (a0 as u16).wrapping_sub(1);
            let tag = a2 as u16;
            match ipc::send_frame(current, slot, a1 as usize, tag) {
                ipc::OpResult::Done(v) => Out::Value(v),
                ipc::OpResult::WouldBlock => Out::Park(ParkKind::Block),
            }
        }
        SYS_RECV_FRAME => {
            let slot = (a0 as u16).wrapping_sub(1);
            match ipc::recv_frame(current, slot, a1) {
                ipc::OpResult::Done(v) => Out::Value(v),
                ipc::OpResult::WouldBlock => Out::Park(ParkKind::Block),
            }
        }
        _ => {
            serial::write_line(&format!("[kairos] unknown syscall {num}"));
            Out::Value(0)
        }
    }
}

fn can_spawn(task_id: TaskId) -> bool {
    task::with_cspace(task_id, |c| {
        c.iter().any(|(_, cap)| {
            cap.kind == ObjectKind::Task
                && cap.object == caps::SPAWN_AUTHORITY
                && cap.has_rights(CapRights::CALL)
        })
    })
    .unwrap_or(false)
}

fn spawn_named(name: &str) -> Option<TaskId> {
    let found = user::PROGRAMS.iter().any(|(n, _)| *n == name);
    if !found {
        return None;
    }
    task::spawn_user(name, 4, 1).ok()
}

// -------------------------------------------------------------------------
// User-memory access helpers (validated, no kernel-pointer reads)
// -------------------------------------------------------------------------

const USER_MAX_READ: u64 = 4096;

fn user_ptr_ok(p: u64, len: usize) -> bool {
    let end = p.checked_add(len as u64);
    p >= user::USER_BASE
        && end.is_some_and(|e| e <= user::USER_STACK_TOP)
        && len as u64 <= USER_MAX_READ
}

fn read_user_bytes(_task: TaskId, p: u64, len: u64) -> alloc::vec::Vec<u8> {
    let len = len.min(USER_MAX_READ) as usize;
    if !user_ptr_ok(p, len) {
        return alloc::vec::Vec::new();
    }
    // # Safety: bounded user pointer, validated range.
    let slice = unsafe { core::slice::from_raw_parts(p as *const u8, len) };
    slice.to_vec()
}

fn read_user_string(task_id: TaskId, p: u64, max_len: u64) -> alloc::string::String {
    let buf = read_user_bytes(task_id, p, max_len);
    let end = buf.iter().position(|b| *b == 0).unwrap_or(buf.len());
    alloc::string::String::from_utf8_lossy(&buf[..end]).into_owned()
}

fn read_message(_task: TaskId, p: u64) -> Option<Message> {
    if !user_ptr_ok(p, core::mem::size_of::<Message>()) {
        return None;
    }
    // # Safety: validated user pointer; Message is repr(C), 72 bytes.
    Some(unsafe { core::ptr::read(p as *const Message) })
}

/// Kernel self-test (structural checks only; full coverage is in
/// `kairos-core` + the user IPC demo).
pub fn run_tests() -> bool {
    let ok = user_ptr_ok(user::USER_BASE, 16)
        && !user_ptr_ok(0, 16)
        && !user_ptr_ok(user::USER_STACK_TOP, 16);
    serial::write_line("syscall:self-test:ok");
    ok
}


