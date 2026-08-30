//! User-space programs: baked ELF payloads + a minimal loader.
//!
//! The user programs (see `user/`) are regular static ELF binaries linked at
//! [`USER_BASE`]. `kernel/build.rs` compiles them and the kernel embeds the
//! bytes with `include_bytes!`. At spawn time [`load_user_program`] parses
//! the ELF (PT_LOAD segments, plus `.rela.dyn` for the few
//! `R_X86_64_RELATIVE` GOT entries Rust emits), maps fresh physical frames
//! with `USER` page flags, applies the relocations and returns the entry
//! point.
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
//!
//! *Why relocations:* Rust's codegen emits `R_X86_64_RELATIVE` entries in
//! `.rela.dyn` (GOT slots for functions/pointers referenced through the PLT
//! or `dynsym`). A static `no-pie` executable links them to their final
//! address in the *addend*, so the loader simply writes the addend into the
//! slot. Skipping this left the slots at 0 and user code jumped to address
//! zero (observed as `#PF` at `rip=0` right after a syscall return).

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

/// Phase 1: map the segment's frames (writable — image copy happens here),
/// copy the file image and zero the tail. Final protections are applied in
/// [`tighten_segments`] after relocations have been patched.
fn map_segment(seg: &Seg, elf: &[u8]) -> Result<(), ()> {
    // The first LOAD of every bare-metal ELF is the ELF-header page at
    // vaddr 0 (file bytes before the linked text); it carries nothing the
    // program references at runtime and must not be mapped.
    if seg.vaddr == 0 || seg.memsz == 0 {
        return Ok(());
    }
    let start = VirtAddr::new(seg.vaddr);
    let end = seg.vaddr + seg.memsz;
    let first = Page::<Size4KiB>::containing_address(start).start_address();
    let pages = ((end.saturating_sub(first.as_u64())).div_ceil(4096)) as u64;
    if pages == 0 {
        return Ok(());
    }

    // Writable for the image copy; NX set for non-executable segments.
    let copy_flags = {
        let mut f = PageTableFlags::PRESENT
            | PageTableFlags::USER_ACCESSIBLE
            | PageTableFlags::WRITABLE;
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
    Ok(())
}

/// Phase 3: drop WRITABLE from pure text (R+X) segments after all
/// relocations have been applied.
fn tighten_segments(segs: &[Seg]) -> Result<(), ()> {
    for seg in segs {
        if seg.vaddr == 0 || seg.memsz == 0 || !seg.executable || seg.writable {
            continue;
        }
        let start = VirtAddr::new(seg.vaddr);
        let end = seg.vaddr + seg.memsz;
        let first = Page::<Size4KiB>::containing_address(start).start_address();
        let pages = ((end.saturating_sub(first.as_u64())).div_ceil(4096)) as u64;
        let flags = {
            let mut f = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
            if seg.writable {
                f |= PageTableFlags::WRITABLE;
            }
            f
        };
        for i in 0..pages {
            memory::paging::update_flags(first + i * 4096, flags).map_err(|_| ())?;
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

/// Apply `.rela.dyn` relocations of a baked user ELF.
///
/// Each `R_X86_64_RELATIVE` entry (type 8) writes its *addend* into the slot
/// at `r_offset` — for a static no-pie binary the addend already holds the
/// final linked address (load bias is 0). Only slots inside an installed
/// segment are patched; anything else is skipped defensively. Runs while
/// every mapped segment is still writable (see [`load_user_program`]).
fn apply_relocations(elf: &[u8], segs: &[Seg]) {
    if elf.len() < 64 {
        return;
    }
    let shoff = {
        let a: [u8; 8] = elf[40..48].try_into().unwrap_or([0; 8]);
        u64::from_ne_bytes(a)
    } as usize;
    let shentsize = {
        let a: [u8; 2] = elf[58..60].try_into().unwrap_or([0; 2]);
        u16::from_ne_bytes(a)
    } as usize;
    let shnum = {
        let a: [u8; 2] = elf[60..62].try_into().unwrap_or([0; 2]);
        u16::from_ne_bytes(a)
    } as usize;
    if shoff == 0 || shentsize == 0 {
        return;
    }
    for i in 0..shnum {
        let off = shoff + i * shentsize;
        if off + 64 > elf.len() {
            break;
        }
        let ty = {
            let a: [u8; 4] = elf[off + 4..off + 8].try_into().unwrap_or([0; 4]);
            u32::from_ne_bytes(a)
        };
        if ty != 4 {
            continue; // SHT_RELA (sh_type at shdr+4; sh_name at shdr+0)
        }
        let sh_offset = {
            let a: [u8; 8] = elf[off + 24..off + 32].try_into().unwrap_or([0; 8]);
            u64::from_ne_bytes(a)
        } as usize;
        let sh_size = {
            let a: [u8; 8] = elf[off + 32..off + 40].try_into().unwrap_or([0; 8]);
            u64::from_ne_bytes(a)
        } as usize;
        let end = sh_offset.saturating_add(sh_size).min(elf.len());
        let mut pos = sh_offset;
        while pos + 24 <= end {
            let r_offset = {
                let a: [u8; 8] = elf[pos..pos + 8].try_into().unwrap_or([0; 8]);
                u64::from_ne_bytes(a)
            };
            let r_info = {
                let a: [u8; 8] = elf[pos + 8..pos + 16].try_into().unwrap_or([0; 8]);
                u64::from_ne_bytes(a)
            };
            let r_addend = {
                let a: [u8; 8] = elf[pos + 16..pos + 24].try_into().unwrap_or([0; 8]);
                u64::from_ne_bytes(a)
            };
            pos += 24;
            let rty = (r_info & 0xffff_ffff) as u32;
            if rty != 8 {
                continue; // R_X86_64_RELATIVE only
            }
            let in_seg = segs.iter().any(|s| {
                r_offset >= s.vaddr && r_offset + 8 <= s.vaddr + s.memsz
            });
            if !in_seg {
                continue;
            }
            // # Safety: r_offset validated to lie inside a mapped writable
            // user segment (installed just before this runs).
            unsafe {
                core::ptr::write_volatile(r_offset as *mut u64, r_addend);
            }
        }
    }
}

/// Load a baked program. Returns (canonical name, entry, user stack top).
pub fn load_user_program(name: &str) -> Result<(&'static str, u64, u64), ()> {
    let bytes = program_bytes(name).ok_or(())?;
    let canonical = program_name(name).ok_or(())?;
    let (entry, segs) = load_segments(bytes).ok_or(())?;
    if segs.is_empty() {
        return Err(());
    }
    // Phase 1: map every segment writable and copy the image (+zero bss).
    for seg in &segs {
        map_segment(seg, bytes)?;
    }
    // Phase 2: patch R_X86_64_RELATIVE slots (pages still writable).
    apply_relocations(bytes, &segs);
    // Phase 3: drop WRITABLE from pure text segments.
    tighten_segments(&segs)?;
    map_user_stack()?;
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
