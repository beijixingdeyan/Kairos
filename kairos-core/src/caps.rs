//! Capability-based access control (seL4-inspired, minimal).
//!
//! Kairos does **not** base permissions on user identities (UID). Every task
//! holds a *capability space* (`CNode`): a fixed set of typed, rights-bearing
//! tokens, each pointing at exactly one kernel object. Access to an
//! object (send on a channel, map a frame, spawn a task) is only possible
//! while a capability for it — with the required rights — is held.
//!
//! Rules enforced by this module (and tested here):
//!
//! 1. A capability is typed: `lookup` returns the kind, so a channel
//!    capability can never be used as a task capability (强类型 API,
//!    禁止 void* 传递不透明数据).
//! 2. Rights can only be *narrowed* when duplicating, never widened
//!    (protection against confused-deputy attacks).
//! 3. `revoke` removes a capability atomically and frees its slot; after a
//!    revoke the slot is reusable and lookups fail.
//! 4. A `CNode` has a fixed capacity; insertion beyond it fails cleanly.

use core::fmt;

/// Object kinds that can be the target of a capability.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
#[repr(u8)]
pub enum ObjectKind {
    /// A schedulable task.
    Task,
    /// A message channel used with [`crate::ipc`].
    Channel,
    /// A shared data frame (zero-copy IPC payload).
    Frame,
    /// A physical frame (mapping authority).
    PhysFrame,
    /// An interrupt notification endpoint.
    Irq,
}

impl fmt::Display for ObjectKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            ObjectKind::Task => "task",
            ObjectKind::Channel => "channel",
            ObjectKind::Frame => "frame",
            ObjectKind::PhysFrame => "physframe",
            ObjectKind::Irq => "irq",
        };
        f.write_str(s)
    }
}

/// Access rights attached to a capability.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct CapRights(u8);

impl CapRights {
    pub const NONE: CapRights = CapRights(0);
    pub const READ: CapRights = CapRights(1 << 0);
    pub const WRITE: CapRights = CapRights(1 << 1);
    pub const CALL: CapRights = CapRights(1 << 2); // can invoke (send/recv/spawn)
    pub const ALL: CapRights = CapRights(Self::READ.0 | Self::WRITE.0 | Self::CALL.0);

    pub const fn new(read: bool, write: bool, call: bool) -> Self {
        let mut r = 0u8;
        if read {
            r |= Self::READ.0;
        }
        if write {
            r |= Self::WRITE.0;
        }
        if call {
            r |= Self::CALL.0;
        }
        Self(r)
    }

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// Raw rights mask (for serialising capabilities).
    pub const fn bits(self) -> u8 {
        self.0
    }

    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Narrow: keep only the rights that both `self` and `mask` allow.
    pub const fn intersect(self, mask: Self) -> Self {
        Self(self.0 & mask.0)
    }
}

impl fmt::Display for CapRights {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let r = if self.contains(Self::READ) { 'r' } else { '-' };
        let w = if self.contains(Self::WRITE) { 'w' } else { '-' };
        let c = if self.contains(Self::CALL) { 'c' } else { '-' };
        write!(f, "{r}{w}{c}")
    }
}

/// A single capability: a typed reference to a kernel object.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Capability {
    /// Kernel-object identifier (opaque to users: an interned handle).
    pub object: u32,
    /// Type of the referenced object.
    pub kind: ObjectKind,
    /// Access rights granted by this token.
    pub rights: CapRights,
}

impl Capability {
    pub const fn new(object: u32, kind: ObjectKind, rights: CapRights) -> Self {
        Self {
            object,
            kind,
            rights,
        }
    }

    pub fn has_rights(&self, required: CapRights) -> bool {
        self.rights.contains(required)
    }
}

/// A slot index inside a capability space.
pub type Slot = u16;

/// Sentinel: no capability (equivalent of a null pointer, but typed).
pub const NULL_SLOT: Slot = 0xFFFF;

/// Capacity of a capability space: fixed and small enough to keep the TCB
/// tiny while still covering realistic teaching workloads.
pub const CNODE_CAPACITY: usize = 64;

/// Capability space ("CNode"): a fixed-capacity array of typed tokens.
///
/// This is deliberately a plain `[Option<Capability>; CNODE_CAPACITY]`
/// internally; the kernel serializes access with a spin-lock around it.
#[derive(Clone, Debug)]
pub struct CNode {
    slots: [Option<Capability>; CNODE_CAPACITY],
}

impl Default for CNode {
    fn default() -> Self {
        Self {
            slots: [None; CNODE_CAPACITY],
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CapError {
    SpaceFull,
    NoSuchSlot(Slot),
    AlreadyOccupied(Slot),
    RightsTooNarrow(CapRights),
}

impl fmt::Display for CapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CapError::SpaceFull => write!(f, "capability space is full"),
            CapError::NoSuchSlot(s) => write!(f, "no capability in slot {s}"),
            CapError::AlreadyOccupied(s) => write!(f, "slot {s} is already occupied"),
            CapError::RightsTooNarrow(r) => write!(f, "insufficient rights ({r})"),
        }
    }
}

impl CNode {
    pub const fn new() -> Self {
        Self {
            slots: [None; CNODE_CAPACITY],
        }
    }

    /// Insert a capability into the first free slot; returns its slot index.
    pub fn insert(&mut self, cap: Capability) -> Result<Slot, CapError> {
        for (i, slot) in self.slots.iter_mut().enumerate() {
            if slot.is_none() {
                *slot = Some(cap);
                return Ok(i as Slot);
            }
        }
        Err(CapError::SpaceFull)
    }

    /// Insert at a specific slot; fails if the slot is occupied.
    pub fn insert_at(&mut self, slot: Slot, cap: Capability) -> Result<(), CapError> {
        let s = self
            .slots
            .get_mut(slot as usize)
            .ok_or(CapError::NoSuchSlot(slot))?;
        if s.is_some() {
            return Err(CapError::AlreadyOccupied(slot));
        }
        *s = Some(cap);
        Ok(())
    }

    /// Look up a capability without consuming it.
    pub fn lookup(&self, slot: Slot) -> Result<&Capability, CapError> {
        self.slots
            .get(slot as usize)
            .ok_or(CapError::NoSuchSlot(slot))?
            .as_ref()
            .ok_or(CapError::NoSuchSlot(slot))
    }

    /// Look up a capability and check that it has `required` rights.
    pub fn lookup_with(&self, slot: Slot, required: CapRights) -> Result<&Capability, CapError> {
        let cap = self.lookup(slot)?;
        if cap.has_rights(required) {
            Ok(cap)
        } else {
            Err(CapError::RightsTooNarrow(required))
        }
    }

    /// Remove and return the capability at `slot`.
    pub fn revoke(&mut self, slot: Slot) -> Result<Capability, CapError> {
        let s = self
            .slots
            .get_mut(slot as usize)
            .ok_or(CapError::NoSuchSlot(slot))?;
        s.take().ok_or(CapError::NoSuchSlot(slot))
    }

    /// Duplicate a capability into this space with rights *narrowed* by
    /// `mask` — the seL4-style "derive" without `GRANT`, so a receiver can
    /// never hand out more rights than it holds itself.
    pub fn derive(&mut self, source: &CNode, slot: Slot, mask: CapRights) -> Result<Slot, CapError> {
        let src = source.lookup(slot)?;
        let narrowed = Capability {
            rights: src.rights.intersect(mask),
            ..*src
        };
        if narrowed.rights.is_empty() {
            return Err(CapError::RightsTooNarrow(CapRights::NONE));
        }
        self.insert(narrowed)
    }

    /// Number of occupied slots.
    pub fn occupied(&self) -> usize {
        self.slots.iter().filter(|s| s.is_some()).count()
    }

    /// First free slot, if any.
    pub fn first_free(&self) -> Option<Slot> {
        self.slots.iter().position(|s| s.is_none()).map(|i| i as Slot)
    }

    /// Iterate over occupied slots in index order.
    pub fn iter(&self) -> impl Iterator<Item = (Slot, &Capability)> {
        self.slots
            .iter()
            .enumerate()
            .filter_map(|(i, c)| c.as_ref().map(|c| (i as Slot, c)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cap(kind: ObjectKind) -> Capability {
        Capability::new(1, kind, CapRights::ALL)
    }

    #[test]
    fn insert_lookup_revoke_cycle() {
        let mut c = CNode::new();
        let s = c.insert(cap(ObjectKind::Channel)).unwrap();
        assert_eq!(c.lookup(s).unwrap().kind, ObjectKind::Channel);
        assert_eq!(c.occupied(), 1);
        let taken = c.revoke(s).unwrap();
        assert_eq!(taken.object, 1);
        assert!(c.lookup(s).is_err());
        assert_eq!(c.occupied(), 0);
    }

    #[test]
    fn cnode_fills_up_cleanly() {
        let mut c = CNode::new();
        for i in 0..CNODE_CAPACITY {
            let s = c
                .insert(Capability::new(i as u32, ObjectKind::Task, CapRights::ALL))
                .unwrap();
            assert_eq!(s as usize, i);
        }
        assert_eq!(c.insert(cap(ObjectKind::Task)), Err(CapError::SpaceFull));
    }

    #[test]
    fn revoke_frees_slot_for_reuse() {
        let mut c = CNode::new();
        let a = c.insert(cap(ObjectKind::Frame)).unwrap();
        let b = c.insert(cap(ObjectKind::Frame)).unwrap();
        c.revoke(a).unwrap();
        // The free slot is reused; occupied count stays at 1.
        assert_eq!(c.insert(cap(ObjectKind::Irq)).unwrap(), a);
        assert_eq!(c.occupied(), 2);
        assert!(c.lookup(b).is_ok());
    }

    #[test]
    fn insert_at_occupied_fails() {
        let mut c = CNode::new();
        let s = c.insert(cap(ObjectKind::Task)).unwrap();
        assert_eq!(
            c.insert_at(s, cap(ObjectKind::Frame)),
            Err(CapError::AlreadyOccupied(s))
        );
    }

    #[test]
    fn derive_narrows_rights_only() {
        let mut src = CNode::new();
        let s = src.insert(Capability::new(7, ObjectKind::Channel, CapRights::ALL)).unwrap();

        let mut dst = CNode::new();
        let d = dst
            .derive(&src, s, CapRights::new(true, false, true))
            .unwrap();
        let cap = dst.lookup(d).unwrap();
        assert!(!cap.has_rights(CapRights::WRITE));
        assert!(cap.has_rights(CapRights::READ));

        // Deriving with no rights grants nothing and must fail.
        assert_eq!(dst.derive(&src, s, CapRights::NONE), Err(CapError::RightsTooNarrow(CapRights::NONE)));
    }

    #[test]
    fn derived_cap_never_widens() {
        let mut src = CNode::new();
        let s = src
            .insert(Capability::new(9, ObjectKind::Frame, CapRights::new(true, false, false)))
            .unwrap();
        let mut dst = CNode::new();
        let d = dst
            .derive(&src, s, CapRights::ALL)
            .expect("derive with full mask");
        assert_eq!(
            dst.lookup(d).unwrap().rights.bits(),
            CapRights::READ.bits()
        );
    }

    #[test]
    fn lookup_checks_rights() {
        let mut c = CNode::new();
        let s = c
            .insert(Capability::new(3, ObjectKind::Channel, CapRights::new(true, false, false)))
            .unwrap();
        assert!(c.lookup_with(s, CapRights::READ).is_ok());
        assert!(c.lookup_with(s, CapRights::WRITE).is_err());
        assert!(c.lookup_with(s, CapRights::CALL).is_err());
        assert!(c.lookup(NULL_SLOT).is_err());
    }

    #[test]
    fn out_of_bounds_slot_is_err() {
        let mut c = CNode::new();
        let bad = CNODE_CAPACITY as Slot;
        assert_eq!(c.lookup(bad), Err(CapError::NoSuchSlot(bad)));
        assert_eq!(c.revoke(bad), Err(CapError::NoSuchSlot(bad)));
    }

    #[test]
    fn iter_visits_occupied_slots() {
        let mut c = CNode::new();
        let a = c.insert(cap(ObjectKind::Task)).unwrap();
        let b = c.insert(cap(ObjectKind::Irq)).unwrap();
        let mut slots = [0u16; 4];
        let mut count = 0usize;
        for (s, _) in c.iter() {
            if count < slots.len() {
                slots[count] = s;
            }
            count += 1;
        }
        assert_eq!(count, 2);
        assert_eq!(slots[0], a);
        assert_eq!(slots[1], b);
    }
}