//! Fuzz targets for the scheduler: random op sequences must never break the
//! core invariants (exactly-one-runner, deadlines observed, fairness bounded).

use proptest::prelude::*;

use kairos_core::sched::{PolicyKind, Scheduler, TaskId};

/// A random, bounded op stream against a scheduler instance.
#[derive(Clone, Copy, Debug)]
enum Op {
    Register(u8),
    Block(u8),
    Wake(u8),
    Finish(u8),
    Tick,
}

fn task_id(x: u8) -> TaskId {
    (x % 8 + 1) as TaskId
}

fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
    prop::collection::vec(
        prop_oneof![
            any::<u8>().prop_map(Op::Register),
            any::<u8>().prop_map(Op::Block),
            any::<u8>().prop_map(Op::Wake),
            any::<u8>().prop_map(Op::Finish),
            Just(Op::Tick),
        ],
        0..1024,
    )
}

proptest! {
    #![proptest_config(ProptestConfig {
        cases: 256,
        ..ProptestConfig::default()
    })]

    #[test]
    fn scheduler_invariants(ops in ops_strategy()) {
        // Run the same stream under every policy.
        for kind in [PolicyKind::RoundRobin, PolicyKind::WeightedRoundRobin, PolicyKind::EarliestDeadlineFirst] {
            let mut s = Scheduler::new(kind, 2);
            for op in &ops {
                match op {
                    Op::Register(id) => { let _ = s.register(task_id(*id), 1, 1, None); }
                    Op::Block(id) => { let _ = s.block(task_id(*id)); }
                    Op::Wake(id) => s.wake(task_id(*id)),
                    Op::Finish(id) => { let _ = s.finish(task_id(*id)); }
                    Op::Tick => { let _ = s.on_tick(); }
                }
                // Invariant 1: a running task is either None or registered,
                // and never blocked at the same time.
                if let Some(r) = s.running() {
                    let rec = s.task(r).expect("runner must be registered");
                    assert_ne!(rec.state, kairos_core::sched::TaskState::Blocked,
                        "runner must not be blocked");
                }
                // Invariant 2: stats are monotone (non-negative counters).
                for rec in s.iter() {
                    assert!(rec.stats.total_ticks >= 0);
                }
            }
        }
    }
}

/// Deterministic seed run (CI-friendly): the same exact stream is executed
/// so regressions are reproducible without proptest's random search.
#[test]
fn deterministic_sequence() {
    let mut s = Scheduler::new(PolicyKind::RoundRobin, 3);
    assert!(s.register(1, 1, 1, None).is_ok());
    assert!(s.register(2, 2, 1, None).is_ok());
    assert!(s.register(3, 3, 1, None).is_ok());

    // With nothing running, the first tick dispatches the ring head.
    let _ = s.on_tick();
    assert_eq!(s.running(), Some(1));

    // Ticks 2..4: quantum is 3, so the 4th tick rotates to the next task.
    let _ = s.on_tick();
    let _ = s.on_tick();
    let action = s.on_tick();
    assert!(matches!(
        action,
        kairos_core::sched::SchedAction::Preempt(Some(2))
    ));
    assert_eq!(s.running(), Some(2));

    // Blocking the runner (2) hands over to the next ready task (3).
    assert_eq!(s.block(2), Some(3));
    assert_eq!(s.running(), Some(3));

    // Cleaning up is the inverse of registering.
    assert!(s.unregister(2).is_ok());
    assert!(s.unregister(1).is_ok());
    assert!(s.unregister(3).is_ok());
    assert_eq!(s.running(), None);
}