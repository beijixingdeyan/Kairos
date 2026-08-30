//! Physical frame allocation.
//!
//! A small first-fit **bitmap allocator**: one bit per 4 KiB physical frame,
//! `1 = used`, `0 = free`. The kernel owns a statically sized bitmap that it
//! carves out of the first usable memory region; everything else in here is
//! pure logic and therefore host-testable and fuzzable.
//!
//! Invariants enforced and tested:
//! * `free` counter always equals the number of unset bits.
//! * Frames are never double-allocated (a `free` of a free frame is an
//!   error, not UB).
//! * `alloc_range(n)` returns `n` physically contiguous frames.
//! * Allocations never exceed the managed window (`base .. base+total`).

use core::fmt;

/// Index of a physical 4 KiB frame. May be huge — we run tests against small
/// windows.
pub type FrameIdx = u64;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum AllocError {
    /// No contiguous run of `n` frames available.
    OutOfFrames(usize),
    /// Index outside the managed window.
    OutOfRange,
    /// Frame not currently allocated (double free).
    NotAllocated,
}

impl fmt::Display for AllocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AllocError::OutOfFrames(n) => write!(f, "no run of {n} free frame(s)"),
            AllocError::OutOfRange => write!(f, "frame index out of range"),
            AllocError::NotAllocated => write!(f, "frame is not allocated"),
        }
    }
}

/// Bitmap frame allocator over a caller-provided bitmap.
///
/// Allocation policy: *next-fit with first-fit fallback*. A scan hint keeps
/// the previous allocation's end as the next scan start, so sequential
/// allocators (kernel heap, user image loading) resolve in O(1) per frame
/// instead of rescanning the whole bitmap. When no run is reachable from the
/// hint without straddling the array wrap, a full linear first-fit scan from
/// index 0 runs instead, which is always complete. `free`/`reserve`/accounting
/// are unaffected by the hint.
pub struct BitmapAllocator<'a> {
    bits: &'a mut [u8],
    base: FrameIdx,
    total: usize,
    free: usize,
    hint: usize,
}

impl<'a> BitmapAllocator<'a> {
    /// `bits` must cover at least `total` bits; all bits must start 0 (free).
    /// The kernel reserves the boot image & tables by calling [`reserve`]
    /// afterwards.
    ///
    /// # Panics
    ///
    /// Panics when `bits` is too small to cover `total` frames.
    pub fn new(bits: &'a mut [u8], base: FrameIdx, total: usize) -> Self {
        assert!(
            bits.len().saturating_mul(8) >= total,
            "bitmap too small: {} bytes for {total} frames",
            bits.len()
        );
        let free = total;
        Self {
            bits,
            base,
            total,
            free,
            hint: 0,
        }
    }

    /// Variant for the kernel's "carve out" strategy: the bitmap starts
    /// entirely *used* (`1` bits, e.g. a `[0xFF; N]` static), and the kernel
    /// frees exactly the firmware-usable runs afterwards with
    /// [`clear_range`]. The free counter starts at 0, so it stays consistent
    /// with the bits.
    ///
    /// # Panics
    ///
    /// Panics when `bits` is too small to cover `total` frames.
    pub fn new_all_used(bits: &'a mut [u8], base: FrameIdx, total: usize) -> Self {
        assert!(
            bits.len().saturating_mul(8) >= total,
            "bitmap too small: {} bytes for {total} frames",
            bits.len()
        );
        Self {
            bits,
            base,
            total,
            free: 0,
            hint: 0,
        }
    }

    #[must_use]
    pub const fn base(&self) -> FrameIdx {
        self.base
    }

    #[must_use]
    pub const fn total(&self) -> usize {
        self.total
    }

    #[must_use]
    pub const fn free_count(&self) -> usize {
        self.free
    }

    #[must_use]
    pub fn used_count(&self) -> usize {
        self.total - self.free
    }

    fn bit(&self, idx: usize) -> bool {
        self.bits[idx / 8] & (1 << (idx % 8)) != 0
    }

    fn set_bit(&mut self, idx: usize, used: bool) {
        let mask = 1 << (idx % 8);
        if used {
            self.bits[idx / 8] |= mask;
        } else {
            self.bits[idx / 8] &= !mask;
        }
    }

    /// Allocate a single frame (first fit). `None` if exhausted.
    pub fn alloc(&mut self) -> Option<FrameIdx> {
        self.alloc_range(1)
    }

    /// Allocate `n` contiguous frames. Next fit from the scan hint, falling
    /// back to a full linear first-fit scan when the hint cannot reach a
    /// suitable run (a run straddling the array wrap). `None` if exhausted.
    pub fn alloc_range(&mut self, n: usize) -> Option<FrameIdx> {
        if n == 0 || n > self.free {
            return None;
        }
        // Fast path: resume where the previous allocation stopped. The wrap
        // scan can split a run whose free frames straddle `hint`, so a miss
        // does not mean exhaustion.
        self.scan_from(self.hint, n)
            // Exact fallback: a full linear scan from 0 is always complete
            // and restores first-fit behaviour for fragmented maps.
            .or_else(|| self.scan_from(0, n))
    }

    /// Scan `n`-frame runs, visiting indices `[start..total) ∪ [0..start)` in
    /// order. Runs never straddle the array wrap (the counter resets at the
    /// wrap), so allocations stay inside bounds.
    fn scan_from(&mut self, start: usize, n: usize) -> Option<FrameIdx> {
        let total = self.total;
        let mut run = 0usize;
        let mut run_start = 0usize;
        let mut prev = usize::MAX;
        for step in 0..total {
            let i = (start + step) % total;
            if prev != usize::MAX && i < prev {
                run = 0; // wrapped around: restart the run counter
            }
            prev = i;
            if self.bit(i) {
                run = 0;
                continue;
            }
            if run == 0 {
                run_start = i;
            }
            run += 1;
            if run == n {
                for j in run_start..run_start + n {
                    self.set_bit(j, true);
                }
                self.free -= n;
                self.hint = (run_start + n) % total;
                return Some(self.base + run_start as u64);
            }
        }
        None
    }

    /// Release a previously allocated single frame.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::OutOfRange`] when `idx` lies outside the managed
    /// window and [`AllocError::NotAllocated`] when the frame is currently
    /// free.
    pub fn free(&mut self, idx: FrameIdx) -> Result<(), AllocError> {
        self.free_range(idx, 1)
    }

    /// Release a contiguous run of `n` frames starting at `idx`.
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::OutOfRange`] when the run extends outside the
    /// managed window and [`AllocError::NotAllocated`] when any frame in the
    /// run is currently free.
    // SAFETY: the bounds check above guarantees `idx - base < total`, and
    // `total` is a `usize`, so the `as` narrowing can never lose bits on the
    // supported 64-bit targets.
    #[allow(clippy::cast_possible_truncation)]
    pub fn free_range(&mut self, idx: FrameIdx, n: usize) -> Result<(), AllocError> {
        if idx < self.base || idx + n as u64 > self.base + self.total as u64 {
            return Err(AllocError::OutOfRange);
        }
        let start = (idx - self.base) as usize;
        for j in start..start + n {
            if !self.bit(j) {
                return Err(AllocError::NotAllocated);
            }
        }
        for j in start..start + n {
            self.set_bit(j, false);
        }
        self.free += n;
        Ok(())
    }

    /// Mark `n` frames starting at `idx` as used (boot reservation).
    ///
    /// # Errors
    ///
    /// Returns [`AllocError::OutOfRange`] when the run extends outside the
    /// managed window and [`AllocError::NotAllocated`] when any frame in the
    /// run is already in use.
    // SAFETY: the bounds check above guarantees `idx - base < total`, and
    // `total` is a `usize`, so the `as` narrowing can never lose bits on the
    // supported 64-bit targets.
    #[allow(clippy::cast_possible_truncation)]
    pub fn reserve(&mut self, idx: FrameIdx, n: usize) -> Result<(), AllocError> {
        if idx < self.base || idx + n as u64 > self.base + self.total as u64 {
            return Err(AllocError::OutOfRange);
        }
        let start = (idx - self.base) as usize;
        for j in start..start + n {
            if self.bit(j) {
                return Err(AllocError::NotAllocated);
            }
        }
        for j in start..start + n {
            self.set_bit(j, true);
        }
        self.free -= n;
        Ok(())
    }

    /// Clear `n` bits starting at `idx` regardless of their current value,
    /// adjusting the free counter. Used when building the initial map from
    /// the firmware (a range may already be clear if two regions overlap).
    // SAFETY: the bounds check above guarantees `idx - base < total`, and
    // `total` is a `usize`, so the `as` narrowing can never lose bits on the
    // supported 64-bit targets.
    #[allow(clippy::cast_possible_truncation)]
    pub fn clear_range(&mut self, idx: FrameIdx, n: usize) {
        if idx < self.base || idx + n as u64 > self.base + self.total as u64 {
            return;
        }
        let start = (idx - self.base) as usize;
        for j in start..start + n {
            if self.bit(j) {
                self.free += 1;
            }
            self.set_bit(j, false);
        }
    }

    /// Slow invariant check used by tests and fuzzing: recount unset bits and
    /// compare with `free_count`.
    #[must_use]
    pub fn check_invariants(&self) -> bool {
        let mut unset = 0usize;
        for i in 0..self.total {
            if !self.bit(i) {
                unset += 1;
            }
        }
        unset == self.free
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make(bits: &mut [u8], base: FrameIdx, total: usize) -> BitmapAllocator<'_> {
        for b in bits.iter_mut() {
            *b = 0;
        }
        BitmapAllocator::new(bits, base, total)
    }

    #[test]
    // SAFETY: `i` is non-negative (range 0..256), so the i32→u64 `as`
    // widening never loses a sign.
    #[allow(clippy::cast_sign_loss)]
    fn alloc_full_window_then_exhaust() {
        let mut bits = [0u8; 32]; // 256 frames
        let mut a = make(&mut bits, 0x1000, 256);
        for i in 0..256 {
            assert_eq!(a.alloc(), Some(0x1000 + i as u64));
        }
        assert_eq!(a.alloc(), None);
        assert_eq!(a.free_count(), 0);
        assert!(a.check_invariants());
    }

    #[test]
    fn free_then_realloc_next_fit() {
        let mut bits = [0u8; 8];
        let mut a = make(&mut bits, 0, 64);
        let f0 = a.alloc().unwrap(); // 0, hint now past it
        let _f1 = a.alloc().unwrap();
        a.free(f0).unwrap();
        // Next fit: the hint already passed f0, so the reuse happens later.
        assert_ne!(a.alloc(), Some(f0));
        assert!(a.check_invariants());
        // Exhausting the rest wraps the hint around and reuses f0.
        for _ in 0..61 {
            let _ = a.alloc();
        }
        assert_eq!(a.alloc(), Some(f0));
        assert!(a.check_invariants());
    }

    #[test]
    fn double_free_detected() {
        let mut bits = [0u8; 8];
        let mut a = make(&mut bits, 0, 64);
        let f = a.alloc().unwrap();
        a.free(f).unwrap();
        assert_eq!(a.free(f), Err(AllocError::NotAllocated));
    }

    #[test]
    fn out_of_range_operations_fail() {
        let mut bits = [0u8; 8];
        let mut a = make(&mut bits, 0x100, 64);
        assert_eq!(a.free(0x0), Err(AllocError::OutOfRange));
        assert_eq!(a.alloc_range(1), Some(0x100));
        assert_eq!(a.reserve(0x200, 400), Err(AllocError::OutOfRange));
    }

    #[test]
    // SAFETY: `i` is non-negative (range 0..8), so the i32→u64 `as` widening
    // never loses a sign.
    #[allow(clippy::cast_sign_loss)]
    fn contiguous_range_allocation() {
        let mut bits = [0u8; 8];
        let mut a = make(&mut bits, 0, 64);
        let start = a.alloc_range(8).unwrap();
        assert_eq!(start, 0);
        // The 8 frames are contiguous and the 9th is a different line.
        for i in 0..8 {
            assert_eq!(a.alloc(), Some(8 + i as u64));
        }
        assert_eq!(a.alloc(), Some(16));
        // Free the range back.
        a.free_range(start, 8).unwrap();
        assert!(a.check_invariants());
    }

    #[test]
    fn range_allocation_skips_used() {
        let mut bits = [0u8; 8];
        let mut a = make(&mut bits, 0, 64);
        a.alloc().unwrap(); // frame 0 used
        a.alloc().unwrap(); // frame 1 used
        let r = a.alloc_range(3).unwrap();
        assert_eq!(r, 2); // skips 0,1
    }

    #[test]
    fn big_range_fails_cleanly() {
        let mut bits = [0u8; 8];
        let mut a = make(&mut bits, 0, 64);
        assert_eq!(a.alloc_range(65), None);
        assert_eq!(a.alloc_range(64), Some(0));
        assert_eq!(a.alloc_range(1), None);
    }

    #[test]
    fn reserve_marks_and_counts() {
        let mut bits = [0u8; 8];
        let mut a = make(&mut bits, 0, 64);
        a.reserve(10, 4).unwrap();
        assert_eq!(a.free_count(), 60);
        // The reserved window is not re-allocated.
        assert_eq!(a.alloc_range(4), Some(0));
        assert_eq!(a.alloc_range(4), Some(4));
        assert_eq!(a.alloc_range(4), Some(14));
    }

    #[test]
    fn invariants_hold_after_randomish_mix() {
        let mut bits = [0u8; 64];
        let mut a = make(&mut bits, 0x400_000, 512);
        let mut allocs = [0u64; 16];
        for slot in &mut allocs {
            *slot = a.alloc().unwrap();
        }
        for i in (0..16).step_by(2) {
            a.free(allocs[i]).unwrap();
        }
        for _ in 0..4 {
            assert!(a.alloc().is_some());
        }
        // `reserve` takes *absolute* frame indices (like free_range).
        a.reserve(a.base() + 64, 8).unwrap();
        assert!(a.check_invariants());
    }

    #[test]
    fn all_used_carve_out_accounting() {
        // Kernel strategy: bitmap starts all-used, usable runs are freed.
        let mut bits = [0xFFu8; 8]; // 64 frames, all used
        let mut a = BitmapAllocator::new_all_used(&mut bits, 0, 64);
        assert_eq!(a.free_count(), 0);
        a.clear_range(0, 16);
        a.clear_range(32, 8);
        assert_eq!(a.free_count(), 24);
        assert!(a.check_invariants());
        // Freed frames are allocatable again.
        assert_eq!(a.alloc(), Some(0));
        assert!(a.check_invariants());
    }

    #[test]
    fn base_offset_returns_absolute_indices() {
        let mut bits = [0u8; 8];
        let mut a = make(&mut bits, 0x1234_0000, 64);
        // Indices are absolute (base + relative offset) in *frame* units;
        // the caller multiplies by 4096 to obtain a physical address.
        assert_eq!(a.alloc(), Some(0x1234_0000));
        assert_eq!(a.alloc(), Some(0x1234_0001));
    }
}