//! Kernel heap: a `linked_list_allocator` heap over frames allocated from our
//! physical allocator and mapped above user space.

use linked_list_allocator::LockedHeap;
use x86_64::VirtAddr;

use super::frames;
use super::{HEAP_START, PagingErrorKind};
use kairos_core::config::HEAP_SIZE;

/// The global allocator (Rust `alloc` via `#[global_allocator]`).
#[global_allocator]
static ALLOCATOR: LockedHeap = LockedHeap::empty();

#[derive(Debug)]
pub enum HeapInitError {
    OutOfMemory,
    Mapping(PagingErrorKind),
}

type Result<T> = core::result::Result<T, HeapInitError>;

/// Map `HEAP_SIZE` bytes of frames and install the allocator.
pub fn init() -> Result<()> {
    let heap_start = VirtAddr::new(HEAP_START);
    let page_count = (HEAP_SIZE + 4095) / 4096;

    for i in 0..page_count {
        let phys = frames::alloc().ok_or(HeapInitError::OutOfMemory)?;
        super::paging::map_page(
            phys,
            heap_start + (i as u64) * 4096,
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE,
        )
        .map_err(HeapInitError::Mapping)?;
    }

    // # Safety: the pages above are freshly mapped and exclusively ours.
    unsafe {
        ALLOCATOR.lock().init(heap_start.as_mut_ptr(), HEAP_SIZE);
    }
    Ok(())
}

/// Kernel self-test: allocate through the global allocator and verify data.
pub fn test_heap() -> bool {
    let mut v = alloc::vec::Vec::new();
    for i in 0..128 {
        v.push(i as u64);
    }
    let ok = v.len() == 128 && v.iter().sum::<u64>() == (0..128).sum::<u64>();
    drop(v);
    ok
}

pub use x86_64::structures::paging::PageTableFlags;