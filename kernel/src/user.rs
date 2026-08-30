//! User-space programs: baked ELF payloads + a minimal loader.
//!
//! The user programs (see `user/`) are regular static ELF binaries linked at
//! [`USER_BASE`]. `kernel/build.rs` compiles them and the kernel embeds the
//! bytes with `include_bytes!`. At spawn time [`load_user_program`] parses
//! the ELF (PT_LOAD segments only —no dynamic linking, no relocations),
//! maps them into fresh physical frames with `USER` page flags and returns
//! the entry point.
//!
//! Address space layout (shared, single-page-table kernel):
//! ```text
//! 0x10_0000_0000  USER_BASE           hello (16 MiB apart per program)
//! 0x10_0100_0000  USER_BASE+0x1000000  echo_server
//! 0x10_0200_0000                      echo_client
//! 0x10_0300_0000                      counter
//! 0x10_0400_0000                      deadline
//! 0x11_0000_0000  USER_STACK_BASE   user stack (4 MiB, grows down)
//! 0x11_0040_0000  USER_STACK_TOP
//! 0x12_0000_0000  USER_FRAME_WINDOW zero-copy shared frames
//! 0x12_0800_0000  USER_WINDOW_END
//! 0x40_0000_0000_0000  kernel heap (2^46)
//! 0x80_0000_0000_0000  physical-memory offset (2^47, bootloader)
//! ```
//!
//! The user programs (see `user/`) are static ELF binaries, each linked at
//! its own base (kernel/build.rs passes the per-bin `-Ttext`); the loader
//! maps them on demand and reuses already-mapped regions.

use core::sync::atomic::AtomicU64;

use x86_64::structures::paging::{Page, PageTableFlags, Size4KiB};
use x86_64::{PhysAddr, VirtAddr};

use crate::memory;
use crate::serial;
use crate::task;

pub const USER_BASE: u64 = 0x10_0000_0000;
pub const USER_STACK_BASE: u64 = 0x11_0000_0000;
pub const USER_STACK_SIZE: u64 = 4 * 1024 * 1024;
pub const USER_STACK_TOP: u64 = USER_STACK_BASE + USER_STACK_SIZE;
pub const USER_FRAME_WINDOW: u64 = 0x12_0000_0000;
pub const USER_WINDOW_END: u64 = USER_FRAME_WINDOW + 0x0800_0000; // 128 MiB window

/// Cursor for allocating window slots for shared frames.
static FRAME_WINDOW_CURSOR: AtomicU64 = AtomicU64::new(USER_FRAME_WINDOW);

/// Allocate the next slot in the user frame window.
pub fn next_frame_window(pages: usize) -> u64 {
    FRAME_WINDOW_CURSOR.fetch_add((pages * 4096) as u64, core::sync::atomic::Ordering::Relaxed)
}

// -------------------------------------------------------------------------
// Baked user programs (paths set by kernel/build.rs)
// -------------------------------------------------------------------------

pub static PROGRAMS: &[(&str, &[u8])] = &[
    ("hello", include_bytes!(env!("USER_BIN_HELLO"))),
    ("echo_server", include_bytes!(env!("USER_BIN_ECHO_SERVER"))),
    ("echo_client", include_bytes!(env!("USER_BIN_ECHO_CLIENT"))),
    ("counter", include_bytes!(env!("USER_BIN_COUNTER"))),
    ("deadline", include_bytes!(env!("USER_BIN_DEADLINE"))),
];

fn program_bytes(name: &str) -> Option<&'static [u8]> {
    PROGRAMS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, b)| *b)
}

/// The canonical static name for a program (matches `PROGRAMS`), for use as
/// the task's display name. `None` when `name` is not a baked program.
pub fn program_name(name: &str) -> Option<&'static str> {
    PROGRAMS.iter().find(|(n, _)| *n == name).map(|(n, _)| *n)
}

// -------------------------------------------------------------------------
// Minimal ELF64 loader
// -------------------------------------------------------------------------

struct Seg {
    vaddr: u64,
    memsz: u64,
    filesz: u64,
    offset: u64,
    writable: bool,
    executable: bool,
}

/// Parse ELF64 program headers (LOAD only).
fn load_segments(b: &[u8]) -> Option<(u64, alloc::vec::Vec<Seg>)> {
    if b.len() < 64 || &b[0..4] != b"\x7fELF" || b[4] != 2 {
        return None;
    }
    let entry = u64::from_ne_bytes(b[24..32].try_into().ok()?);
    let phoff = u64::from_ne_bytes(b[32..40].try_into().ok()?) as usize;
    let phentsize = u16::from_ne_bytes(b[54..56].try_into().ok()?) as usize;
    let phnum = u16::from_ne_bytes(b[56..58].try_into().ok()?) as usize;

    let mut segs = alloc::vec::Vec::new();
    for i in 0..phnum {
        let off = phoff + i * phentsize;
        if off + 56 > b.len() {
            break;
        }
        let ty = u32::from_ne_bytes(b[off..off + 4].try_into().ok()?);
        if ty != 1 {
            continue; // PT_LOAD only
        }
        let flags = u32::from_ne_bytes(b[off + 4..off + 8].try_into().ok()?);
        segs.push(Seg {
            vaddr: u64::from_ne_bytes(b[off + 16..off + 24].try_into().ok()?),
            filesz: u64::from_ne_bytes(b[off + 32..off + 40].try_into().ok()?),
            memsz: u64::from_ne_bytes(b[off + 40..off + 48].try_into().ok()?),
            offset: u64::from_ne_bytes(b[off + 8..off + 16].try_into().ok()?),
            writable: flags & 2 != 0,
            executable: flags & 1 != 0,
        });
    }
    Some((entry, segs))
}

/// Map frames for one segment (writable during image copy), copy the image,
/// zero the tail, then tighten the flags (drop WRITABLE for pure text).
fn install_segment(seg: &Seg, elf: &[u8]) -> Result<(), ()> {
    let start = VirtAddr::new(seg.vaddr);
    let end = seg.vaddr + seg.memsz;
    let first = Page::<Size4KiB>::containing_address(start).start_address();
    let pages = ((end.saturating_sub(first.as_u64())).div_ceil(4096)) as u64;
    if pages == 0 {
        return Ok(());
    }

    let copy_flags = {
        let mut f = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        if seg.writable {
            f |= PageTableFlags::WRITABLE;
        } else {
            f |= PageTableFlags::WRITABLE; // needed while copying the image
        }
        if !seg.executable {
            f |= PageTableFlags::NO_EXECUTE;
        }
        f
    };

    // Already loaded (this or an earlier task mapped the same base): the
    // image bytes are still resident, reuse them — programs coexist in the
    // single shared address space at distinct bases.
    if memory::paging::is_mapped(first) {
        return Ok(());
    }

    for i in 0..pages {
        let phys = memory::frames::alloc().ok_or(())?;
        memory::paging::map_page(phys, first + i * 4096, copy_flags).map_err(|_| ())?;
    }

    // Copy file image.
    if seg.filesz > 0 {
        let dst = seg.vaddr as *mut u8;
        let src = elf.get(seg.offset as usize..).ok_or(())?;
        let n = core::cmp::min(seg.filesz as usize, src.len());
        // # Safety: pages just mapped writable; `src` within the ELF.
        unsafe {
            core::ptr::copy_nonoverlapping(src.as_ptr(), dst, n);
        }
    }
    // Zero the rest (bss).
    if seg.memsz > seg.filesz {
        let dst = (seg.vaddr + seg.filesz) as *mut u8;
        // # Safety: mapped writable; tail within the segment.
        unsafe {
            core::ptr::write_bytes(dst, 0, (seg.memsz - seg.filesz) as usize);
        }
    }

    // Tighten protections on pure-execute segments (R+X, non-writable).
    if seg.executable && !seg.writable {
        let final_flags = {
            let mut f = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
            if seg.writable {
                f |= PageTableFlags::WRITABLE;
            }
            f
        };
        for i in 0..pages {
            memory::paging::update_flags(first + i * 4096, final_flags).map_err(|_| ())?;
        }
    }
    Ok(())
}

fn map_user_stack() -> Result<(), ()> {
    if memory::paging::is_mapped(VirtAddr::new(USER_STACK_BASE)) {
        return Ok(());
    }
    let pages = (USER_STACK_SIZE / 4096) as u64;
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE
        | PageTableFlags::NO_EXECUTE;
    for i in 0..pages {
        let phys = memory::frames::alloc().ok_or(())?;
        memory::paging::map_page(phys, VirtAddr::new(USER_STACK_BASE + i * 4096), flags)
            .map_err(|_| ())?;
    }
    Ok(())
}

// Map the (single, interior-mutable) user frame window so frame
/// capabilities can hand out writable user memory. Mapped exactly once.
fn map_frame_window() -> Result<(), ()> {
    if memory::paging::is_mapped(VirtAddr::new(USER_FRAME_WINDOW)) {
        return Ok(());
    }
    let pages = (USER_WINDOW_END - USER_FRAME_WINDOW) / 4096;
    let flags = PageTableFlags::PRESENT
        | PageTableFlags::WRITABLE
        | PageTableFlags::USER_ACCESSIBLE;
    for i in 0..pages {
        let phys = memory::frames::alloc().ok_or(())?;
        memory::paging::map_page(phys, VirtAddr::new(USER_FRAME_WINDOW + i * 4096), flags)
            .map_err(|_| ())?;
    }
    Ok(())
}

/// Load a baked program. Returns (canonical name, entry, user stack top).
pub fn load_user_program(name: &str) -> Result<(&'static str, u64, u64), ()> {
    let bytes = program_bytes(name).ok_or(())?;
    let canonical = program_name(name).ok_or(())?;
    let (entry, segs) = load_segments(bytes).ok_or(())?;
    if segs.is_empty() {
        return Err(());
    }
    for seg in &segs {
        install_segment(seg, bytes)?;
    }
    map_user_stack()?;
    // The zero-copy frame window is shared address space: map it once for
    // every user task (cheap —pages are demand-like mapped in advance).
    map_frame_window()?;
    Ok((canonical, entry, USER_STACK_TOP))
}

/// Spawn the realtime demo group (EDF policy recommended at build time).
pub fn spawn_deadline_demo() {
    let dl1 = kairos_core::sched::Deadline {
        period: 200,
        budget: 20,
    };
    let dl2 = kairos_core::sched::Deadline {
        period: 100,
        budget: 15,
    };
    let _ = task::spawn_user_rt("deadline", 3, 1, Some(dl1));
    let _ = task::spawn_user_rt("deadline", 3, 1, Some(dl2));
}

/// Kernel self-test: the baked ELFs must parse and have loadable segments.
pub fn run_tests() -> bool {
    let mut ok = true;
    for (name, bytes) in PROGRAMS {
        match load_segments(bytes) {
            Some((entry, segs)) if !segs.is_empty() => {
                serial::write_line(&format!("user: {name}: entry=0x{entry:x} OK"));
            }
            _ => {
                serial::write_line(&format!("user: {name}: bad ELF"));
                ok = false;
            }
        }
    }
    ok
}

/// Silence "unused" warnings for PhysAddr import (used by doc only).
#[allow(dead_code)]
fn _phys(_: PhysAddr) {}
