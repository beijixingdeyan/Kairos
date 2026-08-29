//! Fuzz targets for the frame allocator: random alloc/free sequences must
//! keep the bitmap coherent (never double-free, never overlap, invariants).

use proptest::prelude::*;

use kairos_core::mem::{AllocError, BitmapAllocator};

#[derive(Clone, Copy, Debug)]
enum Op {
    Alloc,
    AllocRange(u8),
    Free(u16),
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    #[test]
    fn allocator_frees_stay_disjoint(ops in prop::collection::vec(
        prop_oneof![
            Just(Op::Alloc),
            any::<u8>().prop_map(Op::AllocRange),
            any::<u16>().prop_map(Op::Free),
        ],
        0..1024,
    )) {
        let mut bits = [0u8; 512]; // 4096 bits → 4096 frames
        let mut alloc = BitmapAllocator::new(&mut bits, 0, 4096);

        // Model: which frames are currently allocated (as a sorted set).
        let mut held: Vec<u64> = Vec::new();

        for op in ops {
            match op {
                Op::Alloc => {
                    if let Some(f) = alloc.alloc() {
                        assert!(!held.contains(&f));
                        held.push(f);
                    }
                }
                Op::AllocRange(n) => {
                    let n = usize::from(n.max(1));
                    match alloc.alloc_range(n) {
                        Some(base) => {
                            for i in 0..n {
                                let f = base + i as u64;
                                assert!(!held.contains(&f));
                                held.push(f);
                            }
                        }
                        None => {}
                    }
                }
                Op::Free(f) => {
                    // Keep f inside the managed window to exercise the
                    // double-free check rather than the out-of-range check.
                    let f = u64::from(f) % 4096;
                    if held.contains(&f) {
                        alloc.free(f).expect("must free held frame");
                        held.retain(|x| *x != f);
                    } else {
                        assert_eq!(alloc.free(f), Err(AllocError::NotAllocated),
                            "double-free must be rejected");
                    }
                }
            }
            // Invariant: bitmap never reports nonsense.
            assert!(alloc.check_invariants());
        }

        // Free everything; the allocator must be fully empty again.
        for f in held {
            alloc.free(f).unwrap();
        }
        assert_eq!(alloc.free_count(), 4096);
    }
}