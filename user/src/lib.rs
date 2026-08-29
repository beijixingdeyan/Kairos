//! Kairos user-side ABI: the thin wrapper every ring-3 program links.
//!
//! A user program is a `#![no_std] #![no_main]` binary whose entry point is
//! `_start(arg)` (the kernel places `arg` in `rdi` at spawn). This crate
//! provides:
//! * `syscall` wrappers (numbers imported from `kairos-core`, one source of
//!   truth with the kernel dispatcher),
//! * the `Message` type (shared with the kernel, `repr(C)`),
//! * a `#[panic_handler]` that works for every binary in this crate.

#![no_std]

pub use kairos_core::ipc::{Message, MsgKind, MSG_WORDS};
use kairos_core::config::{
    SYS_CH_CLOSE, SYS_CH_CREATE, SYS_EXIT, SYS_GETPID, SYS_PRINT, SYS_RECV, SYS_RECV_FRAME,
    SYS_SEND, SYS_SEND_FRAME, SYS_SLEEP, SYS_SPAWN, SYS_TIME, SYS_YIELD,
};

/// Raw `syscall` instruction ABI.
///
/// # Safety
/// Arguments are passed as-is to the kernel; misuse may crash the task.
#[inline(always)]
pub unsafe fn raw_syscall(n: u64, a0: u64, a1: u64, a2: u64, a3: u64) -> u64 {
    let ret: u64;
    // # Safety: caller-guaranteed (see safety docs of this fn).
    unsafe {
        core::arch::asm!(
            "syscall",
            in("rax") n,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            lateout("rax") ret,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    ret
}

/// Terminate the task. Never returns.
pub fn exit(code: u64) -> ! {
    // # Safety: trivial syscall.
    unsafe {
        raw_syscall(SYS_EXIT, code, 0, 0, 0);
    }
    loop {
        core::hint::spin_loop();
    }
}

/// Cooperative yield (re-queue behind all ready tasks).
pub fn yield_now() {
    // # Safety: trivial syscall.
    unsafe {
        raw_syscall(SYS_YIELD, 0, 0, 0, 0);
    }
}

/// Sleep for `ms` milliseconds (measured in scheduler ticks).
pub fn sleep(ms: u64) {
    // # Safety: trivial syscall.
    unsafe {
        raw_syscall(SYS_SLEEP, ms, 0, 0, 0);
    }
}

/// Print a string to the serial console.
pub fn print(s: &str) {
    // # Safety: the kernel copies the bytes; read-only use.
    unsafe {
        raw_syscall(SYS_PRINT, s.as_ptr() as u64, s.len() as u64, 0, 0);
    }
}

pub fn println(s: &str) {
    print(s);
    print("\n");
}

/// Print an unsigned integer (no_std formatting helper).
pub fn print_num(v: u64) {
    let mut buf = [0u8; 20];
    let mut n = v;
    let mut i = 20;
    if n == 0 {
        print("0");
        return;
    }
    while n > 0 {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
    }
    print(core::str::from_utf8(&buf[i..]).unwrap_or("?"));
}

/// Current task id.
pub fn getpid() -> u64 {
    // # Safety: trivial syscall.
    unsafe { raw_syscall(SYS_GETPID, 0, 0, 0, 0) }
}

/// Kernel uptime in ms.
pub fn time_ms() -> u64 {
    // # Safety: trivial syscall.
    unsafe { raw_syscall(SYS_TIME, 0, 0, 0, 0) }
}

/// Ask the kernel to spawn a program. Returns the new task id, or 0 when the
/// caller lacks the spawn capability (capability-system demo — user tasks
/// are denied by default).
pub fn spawn(name: &str) -> u64 {
    // # Safety: name is copied by the kernel.
    unsafe { raw_syscall(SYS_SPAWN, name.as_ptr() as u64, name.len() as u64, 0, 0) }
}

/// Create a channel; returns its 1-based capability slot (0 = failure).
pub fn ch_create() -> u16 {
    // # Safety: trivial syscall.
    let r = unsafe { raw_syscall(SYS_CH_CREATE, 0, 0, 0, 0) };
    r as u16
}

/// Close a channel capability (slot is 1-based).
pub fn ch_close(slot: u16) {
    // # Safety: trivial syscall.
    unsafe {
        raw_syscall(SYS_CH_CLOSE, slot as u64, 0, 0, 0);
    }
}

/// Send a message (blocking when the channel is full). Returns 1 on success.
pub fn send(slot: u16, msg: &Message) -> u64 {
    // # Safety: the kernel reads `msg` from user memory.
    unsafe {
        raw_syscall(
            SYS_SEND,
            slot as u64,
            msg as *const Message as u64,
            0,
            0,
        )
    }
}

/// Receive a message (blocking when empty). The kernel copies it into `buf`.
/// Returns 1 on success.
pub fn recv(slot: u16, buf: &mut Message) -> u64 {
    // # Safety: the kernel writes up to `size_of::<Message>()` bytes.
    unsafe {
        raw_syscall(
            SYS_RECV,
            slot as u64,
            buf as *mut Message as u64,
            0,
            0,
        )
    }
}

/// Send a shared-memory frame group (zero-copy payload). The receiver gets a
/// capability to the same physical frames.
pub fn send_frame(slot: u16, size: usize, tag: u16) -> u64 {
    // # Safety: size is interpreted by the kernel only.
    unsafe { raw_syscall(SYS_SEND_FRAME, slot as u64, size as u64, tag as u64, 0) }
}

/// Receive a shared frame; the capability message lands in `buf` and the
/// return value is the window address where the payload lives (zero-copy).
pub fn recv_frame(slot: u16, buf: &mut Message) -> u64 {
    // # Safety: the kernel writes the message into `buf`.
    unsafe {
        raw_syscall(
            SYS_RECV_FRAME,
            slot as u64,
            buf as *mut Message as u64,
            0,
            0,
        )
    }
}

/// Default panic handler for every user program in this crate.
#[panic_handler]
fn panic(info: &core::panic::PanicInfo) -> ! {
    struct Buf {
        b: [u8; 160],
        n: usize,
    }
    impl core::fmt::Write for Buf {
        fn write_str(&mut self, s: &str) -> core::fmt::Result {
            for ch in s.bytes() {
                if self.n < self.b.len() {
                    self.b[self.n] = ch;
                    self.n += 1;
                }
            }
            Ok(())
        }
    }
    let mut buf = Buf { b: [0; 160], n: 0 };
    let _ = core::fmt::write(&mut buf, format_args!("{}", info.message()));
    print("USER PANIC: ");
    print(core::str::from_utf8(&buf.b[..buf.n]).unwrap_or("?"));
    print("\n");
    exit(1)
}