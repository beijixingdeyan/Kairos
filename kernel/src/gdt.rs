//! Global Descriptor Table + Task State Segment.
//!
//! Layout (index →selector):
//!   0x08 kernel code | 0x10 kernel data | 0x18 user data | 0x20 user code | TSS
//!
//! The TSS holds `rsp0` —the kernel stack used when an interrupt traps from
//! user mode. The scheduler rewrites it on every task switch.

use core::cell::UnsafeCell;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;
use x86_64::VirtAddr;

pub const KERNEL_CODE: SegmentSelector = SegmentSelector::new(1, x86_64::PrivilegeLevel::Ring0);
pub const KERNEL_DATA: SegmentSelector = SegmentSelector::new(2, x86_64::PrivilegeLevel::Ring0);
pub const USER_DATA: SegmentSelector = SegmentSelector::new(3, x86_64::PrivilegeLevel::Ring3);
pub const USER_CODE: SegmentSelector = SegmentSelector::new(4, x86_64::PrivilegeLevel::Ring3);

/// Interior-mutable TSS storage. We are single-CPU: only the scheduler
/// writes `rsp0`, always with interrupts disabled.
struct TssCell(UnsafeCell<TaskStateSegment>);

// # Safety: single-CPU kernel; every mutation of the TSS happens with
// interrupts disabled, so no aliasing can occur.
unsafe impl Sync for TssCell {}
unsafe impl Send for TssCell {}

static TSS: TssCell = TssCell(UnsafeCell::new(TaskStateSegment::new()));

fn tss_ref() -> &'static TaskStateSegment {
    // UnsafeCell::get yields a pointer; reborrowing as & is safe while no
    // &mut exists (single thread + interrupts off during mutation).
    unsafe { &*TSS.0.get() }
}

fn tss_mut() -> &'static mut TaskStateSegment {
    unsafe { &mut *TSS.0.get() }
}

static GDT_TABLE: spin::Lazy<GlobalDescriptorTable> = spin::Lazy::new(|| {
    let mut gdt = GlobalDescriptorTable::new();
    gdt.append(Descriptor::kernel_code_segment());
    gdt.append(Descriptor::kernel_data_segment());
    gdt.append(Descriptor::user_data_segment());
    gdt.append(Descriptor::user_code_segment());
    gdt.append(Descriptor::tss_segment(tss_ref()));
    gdt
});

pub fn init() {
    use x86_64::instructions::tables::load_tss;

    GDT_TABLE.load();
    unsafe {
        load_tss(SegmentSelector::new(5, x86_64::PrivilegeLevel::Ring0));
    }
}

/// Point TSS.rsp0 (kernel stack used when trapping from user mode) at `top`.
pub fn set_rsp0(top: VirtAddr) {
    tss_mut().privilege_stack_table[0] = top;
}
