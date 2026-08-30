//! Deterministic scheduling policies.
//!
//! The scheduler core is a *pure event model*: the kernel feeds it ticks and
//! task lifecycle events, it answers with "keep running" or "preempt to X".
//! It never touches hardware, so every policy can be unit-tested and fuzzed
//! on the host with exact determinism.
//!
//! Policies
//! --------
//! * **Round-robin** (`RoundRobin`)      — fixed quantum.
//! * **Weighted round-robin** (`WeightedRoundRobin`) — credit-based, each
//!   task's share scales with its `weight`.
//! * **EDF** (`EarliestDeadlineFirst`)   — periodic tasks with
//!   (period, budget); executes the ready task whose deadline is soonest and
//!   accounts deadline misses under overload.

use core::fmt;

/// Upper bound on concurrently registered tasks (fixed — no dynamic memory).
pub const MAX_TASKS: usize = 32;

pub type TaskId = u64;

/// State machine a task can be in.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum TaskState {
    #[default]
    Ready,
    Blocked,
    Running,
    Finished,
}

/// Periodic deadline parameters for `EarliestDeadlineFirst`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Deadline {
    /// Period in scheduler ticks.
    pub period: u64,
    /// Execution budget per period in scheduler ticks.
    pub budget: u64,
}

/// Deterministic per-task statistics — the basis of `ps` and of the
/// fairness tests.
#[derive(Clone, Copy, Debug, Default)]
pub struct TaskStats {
    pub runs: u64,
    pub preemptions: u64,
    pub deadline_misses: u64,
    pub budget_exhaustions: u64,
    pub total_ticks: u64,
}

/// Internal bookkeeping record for one task.
pub struct TaskRec {
    pub id: TaskId,
    pub priority: u8,
    pub weight: u32,
    pub deadline: Option<Deadline>,
    pub state: TaskState,
    pub stats: TaskStats,
    // EDF runtime bookkeeping.
    pub next_deadline_ticks: u64,
    pub budget_remaining_ticks: u64,
    // WRR credit bookkeeping.
    pub credits: u32,
    /// Whether the task currently has an entry in the ready ring (prevents
    /// duplicate entries when waking a task whose stale entry is still in
    /// the ring).
    pub in_ring: bool,
}

impl TaskRec {
    fn new(id: TaskId, priority: u8, weight: u32, deadline: Option<Deadline>) -> Self {
        Self {
            id,
            priority,
            weight,
            deadline,
            state: TaskState::Ready,
            stats: TaskStats::default(),
            next_deadline_ticks: 0,
            budget_remaining_ticks: deadline.map_or(0, |d| d.budget),
            credits: weight,
            in_ring: false,
        }
    }
}

/// Which scheduling policy is active.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PolicyKind {
    RoundRobin,
    WeightedRoundRobin,
    EarliestDeadlineFirst,
}

impl PolicyKind {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            PolicyKind::RoundRobin => "round-robin",
            PolicyKind::WeightedRoundRobin => "weighted-round-robin",
            PolicyKind::EarliestDeadlineFirst => "edf",
        }
    }
}

impl From<crate::config::SchedPolicy> for PolicyKind {
    fn from(p: crate::config::SchedPolicy) -> Self {
        match p {
            crate::config::SchedPolicy::RoundRobin => PolicyKind::RoundRobin,
            crate::config::SchedPolicy::WeightedRoundRobin => PolicyKind::WeightedRoundRobin,
            crate::config::SchedPolicy::EarliestDeadlineFirst => PolicyKind::EarliestDeadlineFirst,
        }
    }
}

/// Action a scheduler expects the kernel to take.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchedAction {
    /// Keep dispatching the current task.
    Continue,
    /// Preempt now; run `next` (or the idle loop when `None`).
    Preempt(Option<TaskId>),
}

const NO_ID: TaskId = u64::MAX;

/// The scheduler. Fixed-capacity, allocation-free, deterministic.
pub struct Scheduler {
    kind: PolicyKind,
    quantum_ticks: u64,
    ticks: u64,
    running: Option<TaskId>,
    tasks: [Option<TaskRec>; MAX_TASKS],
    /// Ready queue: ring buffer of task ids.
    ready: [TaskId; MAX_TASKS],
    ready_head: usize,
    ready_tail: usize,
    /// Ticks the current task has consumed in this quantum / credit.
    running_ticks: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SchedError {
    TableFull,
    UnknownTask(TaskId),
    AlreadyRegistered(TaskId),
}

impl fmt::Display for SchedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SchedError::TableFull => write!(f, "task table full"),
            SchedError::UnknownTask(id) => write!(f, "unknown task {id}"),
            SchedError::AlreadyRegistered(id) => write!(f, "task {id} already registered"),
        }
    }
}

impl Scheduler {
    /// `quantum_ticks`: length of one scheduling quantum in timer ticks.
    #[must_use]
    pub fn new(kind: PolicyKind, quantum_ticks: u64) -> Self {
        Self {
            kind,
            quantum_ticks: quantum_ticks.max(1),
            ticks: 0,
            running: None,
            tasks: [const { None }; MAX_TASKS],
            ready: [NO_ID; MAX_TASKS],
            ready_head: 0,
            ready_tail: 0,
            running_ticks: 0,
        }
    }

    #[must_use]
    pub fn kind(&self) -> PolicyKind {
        self.kind
    }

    #[must_use]
    pub fn ticks(&self) -> u64 {
        self.ticks
    }

    #[must_use]
    pub fn running(&self) -> Option<TaskId> {
        self.running
    }

    #[must_use]
    pub fn ready_count(&self) -> usize {
        if self.ready_tail >= self.ready_head {
            self.ready_tail - self.ready_head
        } else {
            MAX_TASKS - self.ready_head + self.ready_tail
        }
    }

    /// Total number of tasks in the table (any state).
    #[must_use]
    pub fn task_count(&self) -> usize {
        self.tasks.iter().filter(|t| t.is_some()).count()
    }

    fn slot_of(&self, id: TaskId) -> Option<usize> {
        self.tasks
            .iter()
            .position(|t| t.as_ref().is_some_and(|r| r.id == id))
    }

    #[must_use]
    pub fn task(&self, id: TaskId) -> Option<&TaskRec> {
        self.slot_of(id).and_then(|i| self.tasks[i].as_ref())
    }

    pub fn iter(&self) -> impl Iterator<Item = &TaskRec> {
        self.tasks.iter().filter_map(|t| t.as_ref())
    }

    /// Register a new task in the Ready state.
    ///
    /// # Errors
    ///
    /// Returns [`SchedError::AlreadyRegistered`] when `id` is already in the
    /// table and [`SchedError::TableFull`] when the task table is at
    /// capacity.
    pub fn register(
        &mut self,
        id: TaskId,
        priority: u8,
        weight: u32,
        deadline: Option<Deadline>,
    ) -> Result<(), SchedError> {
        if let Some(_i) = self.slot_of(id) {
            return Err(SchedError::AlreadyRegistered(id));
        }
        let slot = self
            .tasks
            .iter()
            .position(Option::is_none)
            .ok_or(SchedError::TableFull)?;
        let mut rec = TaskRec::new(id, priority.max(1), weight.max(1), deadline);
        if let Some(deadline) = deadline {
            rec.next_deadline_ticks = self.ticks + deadline.period;
        }
        self.tasks[slot] = Some(rec);
        self.enqueue_ready(id);
        Ok(())
    }

    /// Remove a finished task entirely.
    ///
    /// # Errors
    ///
    /// Returns [`SchedError::UnknownTask`] when `id` is not registered.
    pub fn unregister(&mut self, id: TaskId) -> Result<(), SchedError> {
        let slot = self.slot_of(id).ok_or(SchedError::UnknownTask(id))?;
        if self.running == Some(id) {
            self.running = None;
        }
        self.tasks[slot] = None;
        Ok(())
    }

    fn enqueue_ready(&mut self, id: TaskId) {
        if let Some(r) = self.task_mut(id) {
            if r.state == TaskState::Finished || r.in_ring {
                return;
            }
            r.state = TaskState::Ready;
            r.in_ring = true;
        }
        let tail = self.ready_tail;
        let next = (tail + 1) % MAX_TASKS;
        if next == self.ready_head {
            // Ring full: drop the new entry (shouldn't happen; table is capped).
            return;
        }
        self.ready[tail] = id;
        self.ready_tail = next;
    }

    fn task_mut(&mut self, id: TaskId) -> Option<&mut TaskRec> {
        self.slot_of(id).and_then(|i| self.tasks[i].as_mut())
    }

    fn dequeue_ready(&mut self) -> Option<TaskId> {
        if self.ready_head == self.ready_tail {
            return None;
        }
        let id = self.ready[self.ready_head];
        self.ready_head = (self.ready_head + 1) % MAX_TASKS;
        if let Some(r) = self.task_mut(id) {
            r.in_ring = false;
        }
        Some(id)
    }

    /// Mark a task blocked (waiting for IPC etc.). If it was running,
    /// returns the next task to dispatch (possibly `None` → idle).
    pub fn block(&mut self, id: TaskId) -> Option<TaskId> {
        if let Some(r) = self.task_mut(id) {
            r.state = TaskState::Blocked;
        }
        if self.running == Some(id) {
            self.running = None;
            self.running_ticks = 0;
            return self.dispatch();
        }
        None
    }

    /// Wake a blocked task.
    pub fn wake(&mut self, id: TaskId) {
        if let Some(r) = self.task_mut(id)
            && r.state == TaskState::Blocked
        {
            r.state = TaskState::Ready;
            self.enqueue_ready(id);
        }
    }

    /// A task finished (exited). If it was running, dispatch the next one.
    pub fn finish(&mut self, id: TaskId) -> Option<TaskId> {
        if let Some(r) = self.task_mut(id) {
            r.state = TaskState::Finished;
        }
        if self.running == Some(id) {
            self.running = None;
            self.running_ticks = 0;
            return self.dispatch();
        }
        None
    }

    /// Advance the system clock by one tick. Returns the scheduler action.
    pub fn on_tick(&mut self) -> SchedAction {
        self.ticks = self.ticks.wrapping_add(1);
        let now = self.ticks;

        // EDF bookkeeping: deadlines roll over, misses are counted.
        if self.kind == PolicyKind::EarliestDeadlineFirst {
            for t in self.tasks.iter_mut().flatten() {
                if let Some(d) = t.deadline
                    && t.state != TaskState::Finished
                    && now >= t.next_deadline_ticks
                {
                    t.stats.deadline_misses += 1;
                    t.next_deadline_ticks = now + d.period;
                    t.budget_remaining_ticks = d.budget;
                }
            }
        }

        let running = self.running;
        let Some(rid) = running else {
            // Nothing running: dispatch immediately.
            return SchedAction::Preempt(self.dispatch());
        };

        let is_edf = self.kind == PolicyKind::EarliestDeadlineFirst;

        // Account execution time for the running task.
        if let Some(r) = self.task_mut(rid) {
            r.stats.total_ticks += 1;
            if r.stats.total_ticks == 0 {
                r.stats.total_ticks = 1; // avoid pathological wrap at exactly 0
            }
            if is_edf
                && r.budget_remaining_ticks > 0
            {
                r.budget_remaining_ticks -= 1;
            }
        }

        self.running_ticks += 1;

        match self.kind {
            PolicyKind::RoundRobin => {
                if self.running_ticks >= self.quantum_ticks {
                    // Rotate: put current back, dispatch next.
                    self.running_ticks = 0;
                    if let Some(r) = self.task_mut(rid) {
                        r.state = TaskState::Ready;
                        r.stats.preemptions += 1;
                    }
                    self.enqueue_ready(rid);
                    return SchedAction::Preempt(self.dispatch());
                }
                SchedAction::Continue
            }
            PolicyKind::WeightedRoundRobin => {
                // A task runs for `weight` ticks per round (credit scheme).
                let weight = self.task(rid).map_or(1, |r| r.weight);
                let bankrupt =
                    self.kind == PolicyKind::WeightedRoundRobin
                        && self.running_ticks >= u64::from(weight);
                if bankrupt {
                    self.running_ticks = 0;
                    if let Some(r) = self.task_mut(rid) {
                        r.state = TaskState::Ready;
                        r.stats.preemptions += 1;
                    }
                    self.enqueue_ready(rid);
                    // Refill credits of any starved tasks.
                    for t in self.tasks.iter_mut().flatten() {
                        if t.state == TaskState::Ready && t.credits == 0 {
                            t.credits = t.weight;
                        }
                    }
                    return SchedAction::Preempt(self.dispatch());
                }
                SchedAction::Continue
            }
            PolicyKind::EarliestDeadlineFirst => {
                let exhausted = self
                    .task(rid)
                    .is_some_and(|r| r.budget_remaining_ticks == 0);
                if exhausted {
                    self.running_ticks = 0;
                    if let Some(r) = self.task_mut(rid) {
                        r.stats.budget_exhaustions += 1;
                        r.state = TaskState::Ready;
                    }
                    self.enqueue_ready(rid);
                    return SchedAction::Preempt(self.dispatch());
                }
                SchedAction::Continue
            }
        }
    }

    /// Select the next task to dispatch. Never blocks.
    fn dispatch(&mut self) -> Option<TaskId> {
        loop {
            let id = self.dequeue_ready()?;
            let Some(rec) = self.task_mut(id) else {
                continue; // stale entry
            };
            // A ring entry can be stale (a blocked/finished task left in the
            // queue while it waited on IPC) — only Ready tasks run.
            if rec.state != TaskState::Ready {
                continue;
            }
            let chosen = match self.kind {
                PolicyKind::EarliestDeadlineFirst => self.choose_edf(id),
                _ => Some(id),
            };
            // EDF may pick a *different* task: give the head back its place
            // in the ring so it is not lost (fairness among equal deadlines).
            if chosen.is_some() && chosen != Some(id) {
                self.enqueue_ready(id);
            }
            let is_edf = self.kind == PolicyKind::EarliestDeadlineFirst;
            if let Some(c) = chosen {
                if let Some(r) = self.task_mut(c) {
                    r.state = TaskState::Running;
                    r.stats.runs += 1;
                    // EDF: snapshot budget for the new run.
                    if is_edf
                        && let Some(d) = r.deadline
                    {
                        r.budget_remaining_ticks = d.budget;
                    }
                }
                self.running = Some(c);
                self.running_ticks = 0;
                return Some(c);
            }
        }
    }

    /// EDF tie-break: among EDF tasks, pick the earliest deadline.
    /// Non-EDF tasks get lowest priority (they run only when no EDF task
    /// is ready) — this keeps ARINC-style temporal isolation demonstrable.
    fn choose_edf(&mut self, first: TaskId) -> Option<TaskId> {
        let mut best = first;
        let mut best_deadline = self
            .task(first)
            .and_then(|r| r.deadline.map(|_| r.next_deadline_ticks))
            .unwrap_or(u64::MAX);
        for (i, t) in self.tasks.iter().enumerate() {
            let Some(rec) = t.as_ref() else { continue };
            if rec.id == first || rec.state != TaskState::Ready {
                continue;
            }
            let dl = rec.deadline.map_or(u64::MAX, |_| rec.next_deadline_ticks);
            if dl < best_deadline
                || (dl == best_deadline && rec.priority > self.task(best).unwrap().priority)
            {
                best = rec.id;
                best_deadline = dl;
                let _ = i;
            }
        }
        // Non-EDF idle tasks run only if nothing with a deadline is ready.
        let has_edf_ready = self
            .tasks
            .iter()
            .flatten()
            .any(|r| r.state == TaskState::Ready && r.deadline.is_some());
        if self.task(best).and_then(|r| r.deadline).is_none() && has_edf_ready {
            // look again strictly among EDF tasks
            let mut edf_best = None;
            let mut edf_dl = u64::MAX;
            for t in self.tasks.iter().flatten() {
                if t.state == TaskState::Ready
                    && t.deadline.is_some()
                    && t.next_deadline_ticks < edf_dl
                {
                    edf_dl = t.next_deadline_ticks;
                    edf_best = Some(t.id);
                }
            }
            return edf_best;
        }
        Some(best)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sched_rr() -> Scheduler {
        Scheduler::new(PolicyKind::RoundRobin, 5)
    }

    #[test]
    fn round_robin_is_fair() {
        let mut s = sched_rr();
        s.register(1, 1, 1, None).unwrap();
        s.register(2, 1, 1, None).unwrap();
        s.register(3, 1, 1, None).unwrap();
        assert_eq!(s.ready_count(), 3);

        // First dispatch is task 1 (FIFO order of the ready queue).
        assert_eq!(s.on_tick(), SchedAction::Preempt(Some(1)));

        // Run 90 ticks; each task should have run ~30 ticks.
        for _ in 0..90 {
            s.on_tick();
        }
        let t1 = s.task(1).unwrap().stats.total_ticks;
        let t2 = s.task(2).unwrap().stats.total_ticks;
        let t3 = s.task(3).unwrap().stats.total_ticks;
        // Fairness: no task gets more than quantum+1 ticks of advantage.
        let max = t1.max(t2).max(t3);
        let min = t1.min(t2).min(t3);
        assert!(
            max - min <= 5,
            "unfair round-robin: {t1}/{t2}/{t3}"
        );
    }

    #[test]
    fn blocking_yields_cpu() {
        let mut s = sched_rr();
        s.register(1, 1, 1, None).unwrap();
        s.register(2, 1, 1, None).unwrap();
        assert_eq!(s.on_tick(), SchedAction::Preempt(Some(1)));
        // Task 1 blocks immediately → task 2 runs even though 1 did not
        // consume its quantum.
        let next = s.block(1).unwrap();
        assert_eq!(next, 2);
        assert_eq!(s.task(1).unwrap().state, TaskState::Blocked);
        // Wake 1: it is requeued.
        s.wake(1);
        assert_eq!(s.task(1).unwrap().state, TaskState::Ready);
    }

    #[test]
    fn finish_runs_next() {
        let mut s = sched_rr();
        s.register(1, 1, 1, None).unwrap();
        s.register(2, 1, 1, None).unwrap();
        s.on_tick(); // run 1
        assert_eq!(s.finish(1), Some(2));
        assert_eq!(s.task(1).unwrap().state, TaskState::Finished);
        assert_eq!(s.running(), Some(2));
    }

    #[test]
    fn idle_when_nobody_ready() {
        let mut s = sched_rr();
        assert_eq!(s.on_tick(), SchedAction::Preempt(None));
        s.register(1, 1, 1, None).unwrap();
        s.on_tick();
        assert_eq!(s.block(1), None); // nothing else to run
    }

    #[test]
    fn weighted_rr_respects_weights() {
        let mut s = Scheduler::new(PolicyKind::WeightedRoundRobin, 1);
        // Task A weight 3, task B weight 1 → A should get ~3x the ticks.
        s.register(1, 1, 3, None).unwrap();
        s.register(2, 1, 1, None).unwrap();
        s.on_tick(); // first dispatch
        let mut a = 0u64;
        let mut b = 0u64;
        for _ in 0..200 {
            let running = s.running();
            s.on_tick();
            match running {
                Some(1) => a += 1,
                Some(2) => b += 1,
                _ => {}
            }
        }
        assert!(a > b * 2, "weighted share violated: a={a} b={b}");
    }

    #[test]
    fn edf_runs_earliest_deadline_first() {
        let mut s = Scheduler::new(PolicyKind::EarliestDeadlineFirst, 1);
        // Task low has period 100, budget 10 → deadline at tick 100.
        s.register(1, 1, 1, Some(Deadline { period: 100, budget: 10 }))
            .unwrap();
        // Task hi has period 50 → much earlier deadline.
        s.register(2, 1, 1, Some(Deadline { period: 50, budget: 10 }))
            .unwrap();
        // First dispatch is the earliest deadline → task 2.
        assert_eq!(s.on_tick(), SchedAction::Preempt(Some(2)));
    }

    #[test]
    fn edf_counts_misses_under_overload() {
        let mut s = Scheduler::new(PolicyKind::EarliestDeadlineFirst, 1);
        // One task with tiny budget → it must miss its deadline often.
        s.register(1, 1, 1, Some(Deadline { period: 20, budget: 2 }))
            .unwrap();
        let mut ticks = 0u64;
        let mut running = None;
        while ticks < 120 {
            s.on_tick();
            if s.running() != running {
                running = s.running();
            }
            ticks = s.ticks();
        }
        assert!(s.task(1).unwrap().stats.deadline_misses > 0);
        // Budget exhaustion must have forced preemptions.
        assert!(s.task(1).unwrap().stats.budget_exhaustions > 0);
    }

    #[test]
    fn non_edf_tasks_run_only_when_idle() {
        let mut s = Scheduler::new(PolicyKind::EarliestDeadlineFirst, 1);
        s.register(1, 1, 1, None).unwrap(); // best-effort
        s.register(2, 1, 1, Some(Deadline { period: 30, budget: 5 })).unwrap();
        // With both ready at the dispatch point the EDF task wins.
        assert_eq!(s.on_tick(), SchedAction::Preempt(Some(2)));
        // When the EDF task blocks, best-effort takes over.
        let next = s.block(2);
        assert_eq!(next, Some(1));
        // The woken EDF task preempts at the next scheduler tick.
        s.wake(2);
        assert_eq!(s.on_tick(), SchedAction::Preempt(Some(2)));
    }

    #[test]
    fn duplicate_register_rejected() {
        let mut s = sched_rr();
        s.register(1, 1, 1, None).unwrap();
        assert_eq!(s.register(1, 1, 1, None), Err(SchedError::AlreadyRegistered(1)));
    }

    #[test]
    fn table_cap_respected() {
        let mut s = sched_rr();
        for i in 0..MAX_TASKS as u64 {
            s.register(i, 1, 1, None).unwrap();
        }
        assert_eq!(
            s.register(MAX_TASKS as u64, 1, 1, None),
            Err(SchedError::TableFull)
        );
    }

    #[test]
    fn stats_are_monotonic() {
        let mut s = sched_rr();
        s.register(1, 1, 1, None).unwrap();
        s.register(2, 1, 1, None).unwrap();
        s.on_tick();
        let r = s.task(1).unwrap();
        assert_eq!(r.state, TaskState::Running);
        assert_eq!(r.stats.runs, 1);
    }
}