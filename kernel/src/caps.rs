//! Kernel-object registry (the targets of capabilities).
//!
//! User-visible strongly-typed handles. A capability in a task's CNode
//! references exactly one `KernelObject`; syscall handlers *look up* the
//! object through the registry and check the capability's kind + rights, so
//! the kernel can never confuse a channel with a frame.

use alloc::vec::Vec;
use spin::Mutex;

use kairos_core::caps::ObjectKind;

/// A shared-memory frame group (zero-copy IPC payload).
pub struct SharedFrame {
    /// Physical base address.
    pub phys: usize,
    /// Number of contiguous 4 KiB pages.
    pub pages: usize,
    /// Virtual address (global window) where the frames are mapped.
    pub window: u64,
}

/// The objects capabilities can point at.
pub enum KernelObject {
    /// A message channel (index into the channel table).
    Channel(u16),
    /// A shared frame group.
    Frame(SharedFrame),
    /// A task (used e.g. for the spawn authority).
    Task(kairos_core::sched::TaskId),
    /// Synthetic authority object for `spawn` (id 1).
    SpawnAuthority,
}

static OBJECTS: Mutex<Vec<KernelObject>> = Mutex::new(Vec::new());
static NEXT_OBJECT: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1);

/// Object id 1 is the spawn authority (only its holder may spawn tasks).
pub const SPAWN_AUTHORITY: u32 = 1;

/// Register a new object, returning its kernel-object id.
pub fn register(obj: KernelObject) -> u32 {
    let id = NEXT_OBJECT.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    OBJECTS.lock().push(obj);
    id
}

/// Register the spawn authority at boot (after the heap exists).
pub fn init() {
    // Id 1 is hard-coded as the spawn authority.
    let _ = SPAWN_AUTHORITY;
    OBJECTS.lock().push(KernelObject::SpawnAuthority);
}

/// Look up an object by id. Returns a safe handle that re-locks the registry
/// for each concrete access (the registry never leaks raw pointers).
pub fn lookup(id: u32) -> Option<ObjectRef> {
    let guard = OBJECTS.lock();
    guard
        .get(id as usize - 1)
        .map(|_| ObjectRef { id })
}

/// Opaque handle that resolves when a handler needs the concrete object.
pub struct ObjectRef {
    id: u32,
}

impl ObjectRef {
    const fn new(id: u32) -> Self {
        Self { id }
    }

    pub fn id(&self) -> u32 {
        self.id
    }

    /// Kind of the referenced object.
    pub fn kind(&self) -> Option<ObjectKind> {
        let guard = OBJECTS.lock();
        let obj = guard.get(self.id as usize - 1)?;
        Some(match obj {
            KernelObject::Channel(_) => ObjectKind::Channel,
            KernelObject::Frame(_) => ObjectKind::Frame,
            KernelObject::Task(_) => ObjectKind::Task,
            KernelObject::SpawnAuthority => ObjectKind::Task,
        })
    }

    /// Borrow the frame data (taken under the registry lock).
    pub fn frame(&self) -> Option<SharedFrame> {
        let guard = OBJECTS.lock();
        match guard.get(self.id as usize - 1)? {
            KernelObject::Frame(f) => Some(SharedFrame {
                phys: f.phys,
                pages: f.pages,
                window: f.window,
            }),
            _ => None,
        }
    }

    /// Channel index if this object is a channel.
    pub fn channel(&self) -> Option<u16> {
        let guard = OBJECTS.lock();
        match guard.get(self.id as usize - 1)? {
            KernelObject::Channel(idx) => Some(*idx),
            _ => None,
        }
    }
}

/// Remove an object (when a shared frame is released).
pub fn remove(id: u32) {
    let mut guard = OBJECTS.lock();
    let idx = id as usize - 1;
    if idx < guard.len() {
        guard.remove(idx);
    }
}

/// Kernel self-test: register + lookup round trip.
pub fn test_caps() -> bool {
    let id = register(KernelObject::Channel(0));
    let r = lookup(id).expect("lookup failed");
    r.kind() == Some(ObjectKind::Channel) && r.channel() == Some(0)
}