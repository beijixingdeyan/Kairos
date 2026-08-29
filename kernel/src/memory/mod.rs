//! Memory subsystem: physical frames, paging, kernel heap.
//!
//! The *algorithmic* parts (bitmap allocation) live in `kairos-core::mem`;
//! this module is only the hardware glue: reading the firmware memory map,
//! manipulating page tables, and installing the global kernel allocator.

pub mod frames;
pub mod heap;
pub mod paging;

use bootloader_api::info::{MemoryRegionKind, Optional};
use bootloader_api::BootInfo;
use x86_64::{PhysAddr, VirtAddr};

/// Virtual base of the kernel heap (2^46; below the physical-memory offset
/// mapping at 2^47, above any user mappings).
pub const HEAP_START: u64 = 0x4000_0000_0000;

#[derive(Debug)]
pub enum InitError {
    NoPhysicalMemoryOffset,
    FrameInit(frames::FrameInitError),
    HeapInit(heap::HeapInitError),
    Paging(PagingErrorKind),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PagingErrorKind {
    NotMapped,
    AlreadyMapped,
    ParentNotMapped,
    FrameAllocationFailed,
}

pub struct MemSummary {
    pub total_mib: u64,
    pub usable_mib: u64,
    pub frames_free: u64,
}

/// Initialise everything memory-related. Called very early, before logging
/// is available, so errors are returned rather than logged.
pub fn init(boot_info: &'static mut BootInfo) -> Result<(), InitError> {
    let phys_offset = match boot_info.physical_memory_offset {
        Optional::Some(o) => VirtAddr::new(o),
        Optional::None => return Err(InitError::NoPhysicalMemoryOffset),
    };

    // 1. Paging: adopt the bootloader's page tables via the physical-memory
    //    offset mapping.
    paging::init(phys_offset).map_err(InitError::Paging)?;

    // 2. Physical frame allocator derived from the firmware map.
    frames::init(boot_info).map_err(InitError::FrameInit)?;

    // 3. Map and install the kernel heap.
    heap::init().map_err(InitError::HeapInit)?;

    // 4. Map the VGA text buffer (identity) so the screen works.
    paging::map_identity(PhysAddr::new(0xb8000), PageTableFlags::WRITABLE | PageTableFlags::PRESENT)
        .map_err(InitError::Paging)?;

    Ok(())
}

pub fn summary() -> MemSummary {
    let total_mib = frames::total_bytes() / (1024 * 1024);
    let usable_mib = frames::usable_bytes() / (1024 * 1024);
    let free = frames::free_count();
    MemSummary {
        total_mib,
        usable_mib,
        frames_free: free,
    }
}

pub use x86_64::structures::paging::PageTableFlags;

/// Run the kernel's memory self-tests (also exercised by the host test
/// suite via `kairos-core`).
pub fn run_tests() -> bool {
    let mut ok = true;
    ok &= frames::test_frames();
    ok &= heap::test_heap();
    ok &= paging::test_paging();
    ok
}

/// Total physical memory reported by the firmware.
pub fn total_physical_mib() -> u64 {
    frames::total_bytes() / (1024 * 1024)
}

/// Iterate usable memory regions (for the banner / ps).
pub fn usable_region_count(boot_info: &BootInfo) -> usize {
    boot_info
        .memory_regions
        .iter()
        .filter(|r| r.kind == MemoryRegionKind::Usable)
        .count()
}