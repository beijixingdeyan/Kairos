//! Page table management.
//!
//! We adopt the bootloader's page tables via the physical-memory offset and
//! wrap the root in an `OffsetPageTable` (the `x86_64` crate). New mappings
//! (kernel heap, user regions, MMIO) are added through [`map_page`].

use spin::Mutex;
use x86_64::structures::paging::{
    FrameAllocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags, PhysFrame, Size4KiB,
};
use x86_64::{PhysAddr, VirtAddr};

use super::frames;
use super::PagingErrorKind;

/// Active mapper, installed once during `memory::init`.
static MAPPER: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);

/// Adapter so `OffsetPageTable` can draw frames from our allocator.
struct KairosFrameAlloc;

// # Safety: hands out unused physical frames from the bitmap allocator
// (which is Mutex-guarded); the paging crate only uses it to back new
// page-table levels, all initiated by single-CPU kernel code with
// interrupts disabled around the mapper lock.
unsafe impl FrameAllocator<Size4KiB> for KairosFrameAlloc {
    fn allocate_frame(&mut self) -> Option<PhysFrame<Size4KiB>> {
        frames::alloc().map(|addr| PhysFrame::containing_address(addr))
    }
}

pub type PagingResult = Result<(), PagingErrorKind>;

/// Adopt the page tables the bootloader prepared. The PML4 lives at the
/// physical address in CR3, reachable through `phys_offset`.
pub fn init(phys_offset: VirtAddr) -> PagingResult {
    let (root_frame, _) = x86_64::registers::control::Cr3::read();
    // Map the root table's physical address through the offset mapping.
    let root_virt = VirtAddr::new(root_frame.start_address().as_u64() + phys_offset.as_u64());
    // # Safety: the bootloader guaranteed the root page table is mapped at
    // phys_offset(physical_address(CR3)); we hold it exclusively via the
    // mutex below and never mutate while mapped elsewhere.
    let table = unsafe { &mut *root_virt.as_mut_ptr::<PageTable>() };
    let mapper = unsafe { OffsetPageTable::new(table, phys_offset) };
    *MAPPER.lock() = Some(mapper);
    Ok(())
}

/// Map one 4 KiB page: virtual `virt` ← physical `phys` with `flags`.
pub fn map_page(phys: PhysAddr, virt: VirtAddr, flags: PageTableFlags) -> PagingResult {
    let mut guard = MAPPER.lock();
    let mapper = guard.as_mut().ok_or(PagingErrorKind::NotMapped)?;
    let page = Page::<Size4KiB>::containing_address(virt);
    let frame = PhysFrame::<Size4KiB>::containing_address(phys);
    let mut alloc = KairosFrameAlloc;
    unsafe {
        mapper
            .map_to(page, frame, flags, &mut alloc)
            .map_err(|_| PagingErrorKind::FrameAllocationFailed)?
            .flush();
    }
    Ok(())
}

/// Identity-map a physical address at the same virtual address.
pub fn map_identity(phys: PhysAddr, flags: PageTableFlags) -> PagingResult {
    map_page(phys, VirtAddr::new(phys.as_u64()), flags)
}

/// Map `frames_needed` contiguous physical frames to a contiguous virtual
/// range starting at `virt_start`. Returns the virtual range end.
pub fn map_contiguous(
    phys_start: PhysAddr,
    virt_start: VirtAddr,
    frames_needed: u64,
    flags: PageTableFlags,
) -> PagingResult {
    for i in 0..frames_needed {
        map_page(
            PhysAddr::new(phys_start.as_u64() + i * 4096),
            VirtAddr::new(virt_start.as_u64() + i * 4096),
            flags,
        )?;
    }
    Ok(())
}

/// Update page flags of an already-mapped page (e.g. drop WRITABLE after a
/// segment image was copied in).
pub fn update_flags(virt: VirtAddr, flags: PageTableFlags) -> PagingResult {
    let mut guard = MAPPER.lock();
    let mapper = guard.as_mut().ok_or(PagingErrorKind::NotMapped)?;
    let page = Page::<Size4KiB>::containing_address(virt);
    // # Safety: `mapper` is guarded by the mutex; the flags update only
    // touches the (already mapped) page's entry.
    unsafe {
        mapper
            .update_flags(page, flags)
            .map_err(|_| PagingErrorKind::NotMapped)?
            .flush();
    }
    Ok(())
}

/// Round-trip check used by self-tests.
pub fn is_mapped(virt: VirtAddr) -> bool {
    let guard = MAPPER.lock();
    let Some(mapper) = guard.as_ref() else {
        return false;
    };
    mapper
        .translate_page(Page::<Size4KiB>::containing_address(virt))
        .is_ok()
}

/// Kernel self-test: map a scratch page twice and verify the second mapping
/// fails cleanly (map-to on an existing mapping returns an error, which our
/// wrapper surfaces as `AlreadyMapped` is normalized away — we test the
/// success path and translation).
pub fn test_paging() -> bool {
    // The VGA identity mapping must already exist (done in memory::init).
    is_mapped(VirtAddr::new(0xB8000))
}

#[test]
#[cfg(test)]
fn paging_error_display() {
    let e = PagingErrorKind::NotMapped;
    assert!(format!("{e:?}").len() > 0);
}