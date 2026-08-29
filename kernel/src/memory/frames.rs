//! Physical frame allocation glue.
//!
//! The allocation algorithm is `kairos_core::mem::BitmapAllocator`; here we
//! provide the static bitmap storage and derive the managed window from the
//! firmware memory map (usable regions only).

use bootloader_api::info::MemoryRegionKind;
use bootloader_api::BootInfo;
use kairos_core::mem::{AllocError, BitmapAllocator, FrameIdx};
use spin::Mutex;
use x86_64::PhysAddr;

/// Bitmap capacity: 256 KiB → 2 097 152 frames → up to 8 GiB of RAM.
const BITMAP_BYTES: usize = 256 * 1024;

static mut BITMAP_STORAGE: [u8; BITMAP_BYTES] = [0xFF; BITMAP_BYTES];

static FRAME_ALLOCATOR: Mutex<Option<BitmapAllocator<'static>>> = Mutex::new(None);

/// Sum of usable region bytes (for the banner).
static USABLE_BYTES: Mutex<u64> = Mutex::new(0);

#[derive(Debug)]
pub enum FrameInitError {
    /// RAM larger than the static bitmap can describe.
    RamTooLarge(usize),
    NoUsableMemory,
}

type Result<T> = core::result::Result<T, FrameInitError>;

/// Initialise the allocator from the boot memory map.
///
/// Strategy: start from a fully-used bitmap, then clear exactly the usable
/// runs. Everything else (BIOS area, MMIO, kernel image, bootloader data,
/// boot stack) stays reserved automatically — no fragile manual carving.
pub fn init(boot_info: &BootInfo) -> Result<()> {
    let mut max_frame: FrameIdx = 0;
    let mut any_usable = false;
    for region in boot_info.memory_regions.iter() {
        let start = region.start;
        let end = region.end;
        if start >= end {
            continue;
        }
        let frame_end = FrameIdx::from(end.div_ceil(4096));
        if frame_end > max_frame {
            max_frame = frame_end;
        }
        if region.kind == MemoryRegionKind::Usable {
            any_usable = true;
        }
    }
    if !any_usable || max_frame == 0 {
        return Err(FrameInitError::NoUsableMemory);
    }

    let total = usize::try_from(max_frame).map_err(|_| FrameInitError::RamTooLarge(0))?;
    let need_bytes = total.div_ceil(8);
    if need_bytes > BITMAP_BYTES {
        return Err(FrameInitError::RamTooLarge(need_bytes));
    }

    // # Safety: single-threaded early init; the Mutex guarantees exclusive
    // use afterwards, and the &'static mut is never re-taken while borrowed.
    let bits: &'static mut [u8] = unsafe { &mut *core::ptr::addr_of_mut!(BITMAP_STORAGE) };

    let mut alloc = BitmapAllocator::new(bits, 0, total);

    // Clear usable runs (bitmap starts all-used).
    let mut usable_bytes = 0u64;
    for region in boot_info.memory_regions.iter() {
        if region.kind != MemoryRegionKind::Usable {
            continue;
        }
        let start = FrameIdx::from(region.start.div_ceil(4096));
        let end = FrameIdx::from(region.end.div_ceil(4096));
        if end <= start || end > max_frame {
            continue;
        }
        let n = usize::try_from(end - start).unwrap_or(0);
        alloc.clear_range(start, n);
        usable_bytes += region.end - region.start;
    }

    *FRAME_ALLOCATOR.lock() = Some(alloc);
    *USABLE_BYTES.lock() = usable_bytes;
    Ok(())
}

/// Allocate one frame as a physical address.
pub fn alloc() -> Option<PhysAddr> {
    FRAME_ALLOCATOR
        .lock()
        .as_mut()
        .and_then(|a| a.alloc())
        .map(|idx| PhysAddr::new(idx * 4096))
}

/// Allocate `n` contiguous frames, returns base physical address.
pub fn alloc_range(n: usize) -> Option<PhysAddr> {
    FRAME_ALLOCATOR
        .lock()
        .as_mut()
        .and_then(|a| a.alloc_range(n))
        .map(|idx| PhysAddr::new(idx * 4096))
}

/// Release a frame allocated via [`alloc`].
pub fn free(addr: PhysAddr) -> core::result::Result<(), AllocError> {
    FRAME_ALLOCATOR
        .lock()
        .as_mut()
        .ok_or(AllocError::OutOfRange)?
        .free(addr.as_u64() / 4096)
}

pub fn free_count() -> u64 {
    FRAME_ALLOCATOR
        .lock()
        .as_ref()
        .map_or(0, |a| a.free_count() as u64)
}

pub fn total_bytes() -> u64 {
    FRAME_ALLOCATOR
        .lock()
        .as_ref()
        .map_or(0, |a| a.total() as u64 * 4096)
}

pub fn usable_bytes() -> u64 {
    *USABLE_BYTES.lock()
}

pub fn check_invariants() -> bool {
    FRAME_ALLOCATOR
        .lock()
        .as_ref()
        .is_none_or(|a| a.check_invariants())
}

/// Kernel self-test for the frame allocator (runs inside the VM).
pub fn test_frames() -> bool {
    let mut guard = FRAME_ALLOCATOR.lock();
    let Some(alloc) = guard.as_mut() else {
        return false;
    };
    let a = alloc.alloc();
    let b = alloc.alloc();
    let (Some(a), Some(b)) = (a, b) else {
        return false;
    };
    if a == b {
        return false;
    }
    alloc.free(a / 4096).is_ok() && alloc.check_invariants() && alloc.free(b / 4096).is_ok()
}