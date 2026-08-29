//! Switch helpers: initial task frames live at the top of each task's
//! kernel stack; a task's *registers* are exactly the [`CpuFrame`] that the
//! interrupt/syscall stubs pushed, so switching = restoring a different
//! frame.

use crate::interrupts::CpuFrame;
use core::mem::size_of;

/// Bytes one [`CpuFrame`] occupies on the stack.
pub const FRAME_SIZE: usize = size_of::<CpuFrame>();

/// Build the initial frame for `entry` at the top of its kernel stack and
/// return its address (this becomes the task's first save area).
pub fn build_initial_frame(entry: &super::TaskEntry) -> *mut CpuFrame {
    let top = entry.kstack_top;
    let addr = (top - FRAME_SIZE) & !15;
    let frame = addr as *mut CpuFrame;
    // # Safety: the frame memory belongs exclusively to this freshly created
    // task; nothing else references it yet.
    unsafe {
        frame.write(CpuFrame::zeroed());
    }
    frame
}