//! IPC core: bounded message channels with capability transfer.
//!
//! Kairos IPC is **message passing over bounded channels** — no shared
//! mutable state except what a pair of endpoints deliberately share. A
//! message is a fixed-size word array (`8 × u64` = 64 bytes): small enough
//! to copy in a few cycles, large enough for a pointer, a length or a small
//! record.
//!
//! The "zero-copy" path is explicit and capability-driven: instead of
//! copying a large payload, a sender can transfer a *capability* to a shared
//! [`caps::ObjectKind::Frame`] inside a message. The receiver then maps that
//! frame — nothing is copied, and access is revoked when the sender revokes.
//!
//! The kernel layer (see `kernel/src/ipc.rs`) adds blocking semantics
//! (senders block on a full channel, receivers on an empty one); this module
//! is the allocation-free, deterministic core.

use crate::caps::{ObjectKind};
use core::fmt;

/// Words per message payload.
pub const MSG_WORDS: usize = 8;

/// Message queue capacity per channel.
pub const CHANNEL_CAPACITY: usize = 16;

pub type Word = u64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum MsgKind {
    /// Plain data payload in `words`.
    Data = 0,
    /// `words[0]` = capability slot of the sender being transferred;
    /// `words[1]` = kernel object id.
    CapTransfer = 1,
}

/// A single IPC message. `repr(C)` so the kernel can ship the struct to
/// user space verbatim through the syscall boundary (72 bytes on x86-64).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct Message {
    /// Sender-chosen tag (used by protocols, e.g. request/response ids).
    pub tag: u16,
    pub kind: MsgKind,
    pub words: [Word; MSG_WORDS],
}

impl Message {
    #[must_use]
    pub const fn data(tag: u16, words: [Word; MSG_WORDS]) -> Self {
        Self {
            tag,
            kind: MsgKind::Data,
            words,
        }
    }

    #[must_use]
    pub const fn capability(tag: u16, object: u32, rights_byte: u8) -> Self {
        let mut words = [0; MSG_WORDS];
        words[1] = object as Word;
        words[2] = rights_byte as Word;
        Self {
            tag,
            kind: MsgKind::CapTransfer,
            words,
        }
    }

    #[must_use]
    pub const fn payload(&self) -> &[Word; MSG_WORDS] {
        &self.words
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "msg(tag={}, kind={}, words[0..2]=[{}, {}, …])",
            self.tag,
            if self.kind == MsgKind::Data {
                "data"
            } else {
                "cap"
            },
            self.words[0],
            self.words[1]
        )
    }
}

/// A bounded FIFO channel. All operations are allocation-free and O(1).
pub struct ChannelCore {
    buf: [Message; CHANNEL_CAPACITY],
    head: usize,
    count: usize,
}

impl Default for ChannelCore {
    fn default() -> Self {
        Self {
            buf: [Message::data(0, [0; MSG_WORDS]); CHANNEL_CAPACITY],
            head: 0,
            count: 0,
        }
    }
}

impl ChannelCore {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            buf: [Message::data(0, [0; MSG_WORDS]); CHANNEL_CAPACITY],
            head: 0,
            count: 0,
        }
    }

    #[must_use]
    pub const fn len(&self) -> usize {
        self.count
    }

    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.count == 0
    }

    #[must_use]
    pub const fn is_full(&self) -> bool {
        self.count == CHANNEL_CAPACITY
    }

    /// Push a message. `Err(Full)` when the channel is full.
    ///
    /// # Errors
    ///
    /// Returns [`ChannelError::Full`] when the channel already holds
    /// [`CHANNEL_CAPACITY`] messages.
    pub fn push(&mut self, msg: Message) -> Result<(), ChannelError> {
        if self.is_full() {
            return Err(ChannelError::Full);
        }
        let idx = (self.head + self.count) % CHANNEL_CAPACITY;
        self.buf[idx] = msg;
        self.count += 1;
        Ok(())
    }

    /// Pop the oldest message.
    pub fn pop(&mut self) -> Option<Message> {
        if self.is_empty() {
            return None;
        }
        let msg = self.buf[self.head];
        self.head = (self.head + 1) % CHANNEL_CAPACITY;
        self.count -= 1;
        Some(msg)
    }

    /// Peek the oldest message without consuming it.
    #[must_use]
    pub fn peek(&self) -> Option<&Message> {
        if self.is_empty() {
            return None;
        }
        Some(&self.buf[self.head])
    }

    /// A message with the given tag (e.g. a synchronous reply that arrived
    /// while we were polling). Linear scan — small channel, fine.
    pub fn pop_tag(&mut self, tag: u16) -> Option<Message> {
        for i in 0..self.count {
            let idx = (self.head + i) % CHANNEL_CAPACITY;
            if self.buf[idx].tag == tag {
                let msg = self.buf[idx];
                // close the hole by shifting
                for j in (i..self.count - 1).rev() {
                    let from = (self.head + j + 1) % CHANNEL_CAPACITY;
                    let to = (self.head + j) % CHANNEL_CAPACITY;
                    self.buf[to] = self.buf[from];
                }
                self.count -= 1;
                return Some(msg);
            }
        }
        None
    }

    /// Drop all buffered messages.
    pub fn clear(&mut self) {
        self.head = 0;
        self.count = 0;
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ChannelError {
    Full,
}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChannelError::Full => write!(f, "channel is full"),
        }
    }
}

/// Validate a capability transfer request: the target object kind must match
/// what the message says it carries. Pure validation logic, used by the
/// kernel before mutating any capability space.
#[must_use]
pub fn check_cap_transfer(msg: &Message, kind: ObjectKind) -> bool {
    if msg.kind != MsgKind::CapTransfer {
        return false;
    }
    // SAFETY (of the "truncation"): the CapTransfer ABI deliberately stores
    // the 32-bit object id in a 64-bit word, so decoding with `as u32` is the
    // documented, intentional narrowing.
    #[allow(clippy::cast_possible_truncation)]
    let object = msg.words[1] as u32;
    // Kinds are small positive ids; object 0 is reserved (invalid).
    object != 0 && matches!(
        kind,
        ObjectKind::Frame | ObjectKind::Channel | ObjectKind::PhysFrame
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data(tag: u16, a: Word, b: Word, c: Word) -> Message {
        let mut w = [0; MSG_WORDS];
        w[0] = a;
        w[1] = b;
        w[2] = c;
        Message::data(tag, w)
    }

    #[test]
    fn fifo_order_preserved() {
        let mut ch = ChannelCore::new();
        ch.push(data(1, 10, 20, 30)).unwrap();
        ch.push(data(2, 40, 50, 60)).unwrap();
        ch.push(data(3, 70, 80, 90)).unwrap();
        assert_eq!(ch.pop().unwrap().tag, 1);
        assert_eq!(ch.pop().unwrap().tag, 2);
        assert_eq!(ch.pop().unwrap().tag, 3);
        assert!(ch.pop().is_none());
    }

    #[test]
    // SAFETY: loop indices are < CHANNEL_CAPACITY (16), values
    // 100..103 — far below u16::MAX; the `as` narrowing is nominal in tests.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn capacity_enforced() {
        let mut ch = ChannelCore::new();
        for i in 0..CHANNEL_CAPACITY {
            ch.push(data(i as u16, i as Word, 0, 0)).unwrap();
        }
        assert!(ch.is_full());
        assert_eq!(ch.push(data(99, 0, 0, 0)), Err(ChannelError::Full));
        let _ = ch.pop().unwrap();
        assert!(!ch.is_full());
        ch.push(data(99, 0, 0, 0)).unwrap();
    }

    #[test]
    // SAFETY: loop indices are < CHANNEL_CAPACITY (16), values
    // 100..103 — far below u16::MAX; the `as` narrowing is nominal in tests.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn wrap_around_keeps_fifo() {
        let mut ch = ChannelCore::new();
        for i in 0..CHANNEL_CAPACITY {
            ch.push(data(i as u16, i as Word, 0, 0)).unwrap();
        }
        // Drain 3, then push 3 more; head wraps around the ring.
        for _ in 0..3 {
            ch.pop().unwrap();
        }
        for i in 0..3 {
            ch.push(data((100 + i) as u16, 0, 0, 0)).unwrap();
        }
        let mut tags = [0u16; CHANNEL_CAPACITY];
        let mut n = 0;
        while let Some(m) = ch.pop() {
            tags[n] = m.tag;
            n += 1;
        }
        assert_eq!(n, CHANNEL_CAPACITY);
        for (i, t) in tags.iter().enumerate() {
            if i < CHANNEL_CAPACITY - 3 {
                assert_eq!(*t as usize, i + 3);
            } else {
                assert_eq!(*t as usize, i + 100 - (CHANNEL_CAPACITY - 3));
            }
        }
    }

    #[test]
    fn peek_does_not_consume() {
        let mut ch = ChannelCore::new();
        ch.push(data(7, 1, 2, 3)).unwrap();
        assert_eq!(ch.peek().unwrap().tag, 7);
        assert_eq!(ch.len(), 1);
        ch.pop().unwrap();
        assert!(ch.peek().is_none());
    }

    #[test]
    fn pop_tag_selective() {
        let mut ch = ChannelCore::new();
        ch.push(data(1, 0, 0, 0)).unwrap();
        ch.push(data(2, 0, 0, 0)).unwrap();
        ch.push(data(3, 0, 0, 0)).unwrap();
        let m = ch.pop_tag(2).unwrap();
        assert_eq!(m.tag, 2);
        assert_eq!(ch.len(), 2);
        assert_eq!(ch.pop().unwrap().tag, 1);
        assert_eq!(ch.pop().unwrap().tag, 3);
    }

    #[test]
    fn capability_transfer_messages_carry_object() {
        let m = Message::capability(9, 42, 0b111);
        assert_eq!(m.kind, MsgKind::CapTransfer);
        assert_eq!(m.words[1], 42);
        assert!(check_cap_transfer(&m, ObjectKind::Frame));
        assert!(!check_cap_transfer(&m, ObjectKind::Task));
        let d = Message::data(0, [0; MSG_WORDS]);
        assert!(!check_cap_transfer(&d, ObjectKind::Frame));
    }

    #[test]
    fn clear_drops_all() {
        let mut ch = ChannelCore::new();
        ch.push(data(1, 0, 0, 0)).unwrap();
        ch.push(data(2, 0, 0, 0)).unwrap();
        ch.clear();
        assert!(ch.is_empty());
        assert_eq!(ch.len(), 0);
    }
}