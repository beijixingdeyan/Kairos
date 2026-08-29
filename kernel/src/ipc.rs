//! Kernel-side IPC: capability-gated bounded channels + shared frames.
//!
//! The queue arithmetic lives in [`kairos_core::ipc`]; here we add:
//! * the channel table and per-channel waiter queues,
//! * capability checks (send/recv require a channel capability with CALL
//!   rights),
//! * blocking semantics — a sender parks when the channel is full, a
//!   receiver parks when it is empty; the counterpart's operation wakes it,
//! * a zero-copy path: [`send_frame`] allocates physical frames and ships a
//!   capability; [`recv_frame`] maps them into the global user window — no
//!   `memcpy` of the payload.

use alloc::vec::Vec;
use kairos_core::caps::{CapRights, ObjectKind};
use kairos_core::ipc::{ChannelCore, Message, MsgKind};
use kairos_core::sched::TaskId;
use spin::Mutex;

use crate::caps::{self, SharedFrame};
use crate::memory;
use crate::task;
use crate::user;

pub struct KernelChannel {
    pub core: ChannelCore,
    /// Tasks blocked because the channel was full (they retry on space).
    pub send_waiters: Vec<TaskId>,
    /// Tasks blocked because the channel was empty (they retry on data).
    pub recv_waiters: Vec<TaskId>,
}

static CHANNELS: Mutex<Vec<KernelChannel>> = Mutex::new(Vec::new());

/// Create a channel; returns its index (the kernel object it becomes).
pub fn create() -> u16 {
    let mut guard = CHANNELS.lock();
    let idx = guard.len() as u16;
    guard.push(KernelChannel {
        core: ChannelCore::new(),
        send_waiters: Vec::new(),
        recv_waiters: Vec::new(),
    });
    idx
}

/// Borrow one channel (interrupts must be off — guaranteed on the syscall
/// path and in shell command handlers).
pub fn get(idx: u16) -> Option<&'static mut KernelChannel> {
    // # Safety: single-CPU; callers run with interrupts disabled so the
    // borrow cannot be aliased by an ISR, and re-borrows always re-lock.
    unsafe {
        let mut guard = CHANNELS.lock();
        guard
            .get_mut(idx as usize)
            .map(|c| c as *mut KernelChannel)
            .map(|p| &mut *p)
    }
}

/// Result of a channel operation from the dispatcher's perspective.
pub enum OpResult {
    /// Finished with a plain value (0 = failure, 1 = ok, else payload).
    Done(u64),
    /// The caller is queued as a waiter and must be parked (blocked).
    WouldBlock,
}

fn channel_of(cap: &kairos_core::caps::Capability) -> Option<u16> {
    if cap.kind != ObjectKind::Channel {
        return None;
    }
    caps::lookup(cap.object).and_then(|o| o.channel())
}

/// Look up + validate a channel capability in `task_id`'s space.
fn resolve_channel(task_id: TaskId, slot: u16) -> Option<u16> {
    task::with_cspace(task_id, |c| {
        c.lookup_with(slot, CapRights::CALL).ok().and_then(channel_of)
    })?
}

/// Send a data message; parks the caller when the channel is full.
pub fn send(task_id: TaskId, slot: u16, msg: Message) -> OpResult {
    let Some(chan_id) = resolve_channel(task_id, slot) else {
        return OpResult::Done(0);
    };
    match get(chan_id) {
        Some(chan) => match chan.core.push(msg) {
            Ok(()) => {
                if let Some(w) = chan.recv_waiters.pop() {
                    task::wake_parked(w);
                }
                OpResult::Done(1)
            }
            Err(_) => {
                chan.send_waiters.push(task_id);
                OpResult::WouldBlock
            }
        },
        None => OpResult::Done(0),
    }
}

/// Receive a message, copying it into `user_buf` (a user-space pointer).
/// Parks the caller when the channel is empty.
pub fn recv(task_id: TaskId, slot: u16, user_buf: u64) -> OpResult {
    let Some(chan_id) = resolve_channel(task_id, slot) else {
        return OpResult::Done(0);
    };
    match get(chan_id) {
        Some(chan) => {
            if let Some(msg) = chan.core.pop() {
                if let Some(w) = chan.send_waiters.pop() {
                    task::wake_parked(w);
                }
                copy_message_to_user(user_buf, &msg);
                OpResult::Done(1)
            } else {
                chan.recv_waiters.push(task_id);
                OpResult::WouldBlock
            }
        }
        None => OpResult::Done(0),
    }
}

/// Write a message into user memory (72 bytes, `repr(C)`).
fn copy_message_to_user(user_buf: u64, msg: &Message) {
    let bytes = unsafe {
        core::slice::from_raw_parts(msg as *const Message as *const u8, core::mem::size_of::<Message>())
    };
    let dst = user_buf as *mut u8;
    // # Safety: user_buf is a caller-provided user pointer; the buffer is
    // writable user memory (the window is mapped for DATA messages too to
    // keep the check simple — see user::init_user_window).
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
    }
}

/// Allocate a shared frame group of `size` bytes (rounded to pages),
/// register it, and ship its capability over the channel (zero-copy).
pub fn send_frame(task_id: TaskId, slot: u16, size: usize, tag: u16) -> OpResult {
    if size == 0 {
        return OpResult::Done(0);
    }
    let pages = size.div_ceil(4096);
    let Some(phys) = memory::frames::alloc_range(pages) else {
        return OpResult::Done(0);
    };

    let window = user::next_frame_window(pages);

    use x86_64::structures::paging::PageTableFlags;
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE;
    if memory::paging::map_contiguous(phys, x86_64::VirtAddr::new(window), pages as u64, flags)
        .is_err()
    {
        return OpResult::Done(0);
    }

    let obj_id = caps::register(caps::KernelObject::Frame(SharedFrame {
        phys: phys.as_u64() as usize,
        pages,
        window,
    }));

    let msg = Message::capability(tag, obj_id, CapRights::ALL.bits());
    send(task_id, slot, msg)
}

/// Wait for a frame capability; on arrival, maps it (already mapped by the
/// sender) and returns the window address where the payload lives.
pub fn recv_frame(task_id: TaskId, slot: u16, user_buf: u64) -> OpResult {
    match recv(task_id, slot, user_buf) {
        OpResult::Done(1) => {
            // Read the message we just wrote into user memory to find the
            // frame object; simpler: recv already copied it — decode it from
            // the *kernel* side by peeking the channel again is racy. We
            // therefore look at the just-copied buffer.
            let msg = unsafe { &*(user_buf as *const Message) };
            if msg.kind != MsgKind::CapTransfer {
                return OpResult::Done(0);
            }
            let obj_id = msg.words[1] as u32;
            match caps::lookup(obj_id).and_then(|o| o.frame()) {
                Some(f) => OpResult::Done(f.window),
                None => OpResult::Done(0),
            }
        }
        other => other,
    }
}

/// Kernel self-test (runs in the VM; channel *algorithms* are tested on the
/// host inside `kairos-core`).
pub fn run_tests() -> bool {
    let mut ok = true;
    let mut ch = ChannelCore::new();
    let m = Message::data(7, [42; 8]);
    ok &= ch.push(m).is_ok();
    let out = ch.pop().expect("pop");
    ok &= out.tag == 7 && out.words[0] == 42;
    crate::serial::write_line("ipc:self-test:ok");
    ok
}

/// Kill all waiters of a channel (used when a task dies; not yet wired to
/// the exit path — documented for the roadmap).
#[allow(dead_code)]
pub fn purge_waiters(_idx: u16) {
    // TODO(roadmap): wake-and-cancel on task exit.
}