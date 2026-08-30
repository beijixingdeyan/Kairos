//! Task management: preemptive, deterministic scheduling.
//!
//! Model
//! -----
//! A *task* is the unit of scheduling. Each task has:
//! * a kernel stack (heap-allocated, page-roundable),
//! * a **save area**: a [`CpuFrame`] that holds its registers whenever it is
//!   not running —either the frame built at spawn (its first run) or the
//!   frame an interrupt/syscall stored,
//! * a capability space ([`CNode`]).
//!
//! Preemption: the PIT fires at 1 kHz; the timer ISR runs the policy
//! ([`kairos_core::sched`]), and if the policy says "switch", the ISR simply
//! *restores a different frame* —that *is* the context switch. No
//! memcpy of register files, no extra stacks.
//!
//! Blocking (sleep / IPC park / yield) from user mode reuses the same
//! mechanism: the syscall builds the current frame explicitly and returns
//! the next task's frame, exactly like a timer preemption.

pub mod switch;

use alloc::vec::Vec;
use kairos_core::caps::CNode;
use kairos_core::sched::{Deadline, SchedAction, Scheduler};
use spin::Mutex;
use x86_64::VirtAddr;

pub use kairos_core::sched::TaskId;

use crate::interrupts::{set_user_ret_flag, CpuFrame};
use crate::{gdt, serial, user};

// # Safety: the task table is single-CPU state (interrupts are disabled
// around every access); the interior raw pointer (a task's save area) is
// only valid while the task exists and never crosses threads.
unsafe impl Send for TaskEntry {}

/// Per-task record in the kernel task table.
pub struct TaskEntry {
    pub id: TaskId,
    pub name: &'static str,
    /// Kernel stack allocation (kept alive for the task's lifetime).
    _kstack_alloc: Vec<u8>,
    /// Top of the kernel stack (virtual).
    pub kstack_top: usize,
    /// Where this task's registers live while it is not running.
    pub save_area: *mut CpuFrame,
    /// Whether the task executes in ring 3.
    pub is_user: bool,
    /// Capability space of the task.
    pub cspace: CNode,
    pub priority: u8,
    pub weight: u32,
    pub deadline: Option<Deadline>,
    /// Baked user program name (user tasks only).
    pub prog: Option<&'static str>,
}

const KSTACK_SIZE: usize = 64 * 1024;

/// Task table. All access happens with interrupts disabled.
static TASKS: Mutex<Vec<TaskEntry>> = Mutex::new(Vec::new());

/// The scheduler (from `kairos-core`).
static SCHED: Mutex<Option<Scheduler>> = Mutex::new(None);

/// Sleeping tasks: (task id, wake-up tick).
static SLEEPING: Mutex<Vec<(TaskId, u64)>> = Mutex::new(Vec::new());

/// Exited tasks whose kernel stacks must stay alive (we cannot free the
/// stack we are currently executing on); recycled on future exits.
static GRAVEYARD: Mutex<Vec<TaskEntry>> = Mutex::new(Vec::new());

static NEXT_TASK_ID: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(1);

/// Run `f` with interrupts disabled and the scheduler locked. Every scheduler
/// transition from *task context* (interrupts enabled) goes through here so
/// the timer ISR can never race with it (the ISR itself runs with interrupts
/// already off and uses [`with_sched_locked`]).
fn with_sched<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R {
    x86_64::instructions::interrupts::disable();
    let mut opt = SCHED.lock();
    let sched = opt.as_mut().expect("scheduler not initialised");
    let r = f(sched);
    drop(opt);
    x86_64::instructions::interrupts::enable();
    r
}

fn entry_mut(id: TaskId) -> Option<&'static mut TaskEntry> {
    // # Safety: returns a borrow with 'static lifetime; callers must not
    // hold other borrows of TASKS (we always re-lock). Interrupts are off
    // when switching so the borrow cannot be aliased by the ISR.
    unsafe {
        let mut guard = TASKS.lock();
        let idx = guard.iter().position(|t| t.id == id)?;
        let ptr: *mut TaskEntry = &mut guard[idx];
        Some(&mut *ptr)
    }
}

pub fn name_of(id: TaskId) -> &'static str {
    TASKS.lock()
        .iter()
        .find(|t| t.id == id)
        .map_or("?", |t| t.name)
}

/// Initialise the scheduler from compile-time configuration.
pub fn init() {
    // WRR vs quota: 1 tick = 1 ms (PIT @ 1 kHz).
    let quantum = kairos_core::config::QUANTUM_MS;
    let sched = Scheduler::new(kairos_core::config::SCHED_POLICY.into(), quantum);
    assert!(sched.kind().name().len() > 0);
    *SCHED.lock() = Some(sched);
}

/// Allocate a kernel stack and build the task's initial frame.
fn new_task(
    name: &'static str,
    priority: u8,
    weight: u32,
    deadline: Option<Deadline>,
    is_user: bool,
    frame_builder: impl FnOnce(&mut TaskEntry),
) -> TaskId {
    let id = NEXT_TASK_ID.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
    let kstack_alloc = alloc::vec![0u8; KSTACK_SIZE];
    let kstack_top = kstack_alloc.as_ptr() as usize + KSTACK_SIZE;

    let mut entry = TaskEntry {
        id,
        name,
        _kstack_alloc: kstack_alloc,
        kstack_top,
        save_area: core::ptr::null_mut(),
        is_user,
        cspace: CNode::new(),
        priority,
        weight,
        deadline,
        prog: None,
    };

    // Build the initial frame at the top of the kernel stack.
    entry.save_area = switch::build_initial_frame(&entry);
    frame_builder(&mut entry);

    with_sched(|s| {
        s.register(id, priority, weight, deadline)
            .expect("task register failed");
    });

    TASKS.lock().push(entry);
    id
}

/// Spawn a kernel-mode task (runs a function in ring 0).
pub fn spawn_kernel(
    name: &'static str,
    priority: u8,
    weight: u32,
    entry: extern "C" fn(usize) -> !,
    arg: usize,
) -> TaskId {
    new_task(name, priority, weight, None, false, |e| {
        // # Safety: writing fields of a frame owned by this task.
        unsafe {
            let fr = &mut *e.save_area;
            fr.rip = entry as u64;
            fr.cs = crate::gdt::KERNEL_CODE.0 as u64;
            fr.rflags = 0x202; // IF | reserved bit 1
            fr.err = 0;
            fr.vec = 0;
            fr.rdi = arg as u64;
            // Sanity values for the (spec-optional) rsp/ss slots of a
            // ring-0 frame: some QEMU versions reload them on iretq even
            // for a CPL-0→CPL-0 return, so they must point at this task's
            // own kernel stack, never zero.
            fr.user_rsp = e.kstack_top as u64;
            fr.user_ss = crate::gdt::KERNEL_DATA.0 as u64;
        }
    })
}

/// Spawn a user-mode program from a baked ELF (`name` is matched against
/// `user::PROGRAMS`; the canonical static name is used for the task).
pub fn spawn_user(name: &str, priority: u8, weight: u32) -> Result<TaskId, ()> {
    spawn_user_arg(name, priority, weight, 0)
}

/// Spawn a user-mode task with an initial argument (delivered in `rdi`).
pub fn spawn_user_arg(name: &str, priority: u8, weight: u32, arg: usize) -> Result<TaskId, ()> {
    spawn_user_full(name, priority, weight, None, arg)
}

/// Spawn a user-mode task, optionally with a realtime deadline.
pub fn spawn_user_rt(
    name: &str,
    priority: u8,
    weight: u32,
    deadline: Option<Deadline>,
) -> Result<TaskId, ()> {
    spawn_user_full(name, priority, weight, deadline, 0)
}

fn spawn_user_full(
    name: &str,
    priority: u8,
    weight: u32,
    deadline: Option<Deadline>,
    arg: usize,
) -> Result<TaskId, ()> {
    // Load the ELF (maps user pages into the shared address space). Returns
    // the canonical static name so the task never borrows caller memory.
    let (canonical, entry, stack_top) = user::load_user_program(name)?;

    let id = new_task(canonical, priority, weight, deadline, true, |e| {
        // # Safety: initialising our own frame.
        let fr = unsafe { &mut *e.save_area };
        fr.rip = entry;
        fr.cs = (crate::gdt::USER_CODE.0 | 3) as u64; // 0x23
        fr.user_ss = (crate::gdt::USER_DATA.0 | 3) as u64; // 0x1B
        fr.rflags = 0x202;
        fr.user_rsp = stack_top;
        fr.err = 0;
        fr.vec = 0;
        fr.rdi = arg as u64;
    });

    let mut t = TASKS.lock();
    if let Some(e) = t.iter_mut().find(|t| t.id == id) {
        e.prog = Some(canonical);
    }
    drop(t);
    Ok(id)
}

pub fn e_kstack_top(id: TaskId) -> u64 {
    TASKS.lock()
        .iter()
        .find(|t| t.id == id)
        .map_or(0, |t| t.kstack_top as u64)
}

/// Overwrite the initial `rdi` of a task that has not run yet (used to hand
/// a capability slot to a freshly spawned user program). No-op if the task
/// already ran — spawn-time caller, so that cannot happen in practice.
pub fn set_user_arg(id: TaskId, arg: u64) {
    let mut guard = TASKS.lock();
    if let Some(e) = guard.iter_mut().find(|t| t.id == id) {
        // # Safety: single CPU; the task has not been dispatched yet, so its
        // save area is only touched here.
        unsafe {
            if !e.save_area.is_null() {
                (*e.save_area).rdi = arg;
            }
        }
    }
}

pub fn is_user_task(id: TaskId) -> bool {
    TASKS.lock()
        .iter()
        .find(|t| t.id == id)
        .is_some_and(|t| t.is_user)
}

/// Capability space of a task (locks TASKS).
pub fn with_cspace<R>(id: TaskId, f: impl FnOnce(&mut CNode) -> R) -> Option<R> {
    let mut guard = TASKS.lock();
    let e = guard.iter_mut().find(|t| t.id == id)?;
    Some(f(&mut e.cspace))
}

// -------------------------------------------------------------------------
// Preemption & switching
// -------------------------------------------------------------------------

/// Called from the timer IRQ handler (interrupts off).
/// The name is the one `interrupts.rs` calls.
pub fn on_irq_after_eoi(frame: *mut CpuFrame) -> *mut CpuFrame {
    // Before the scheduler exists (early boot), keep executing.
    if SCHED.lock().is_none() {
        return frame;
    }
    // Bootstrap pause: while set, the timer only acks and returns so the
    // first task entry (enter_task_frame) cannot be disturbed by a dispatch.
    if PAUSE_TIMER.load(core::sync::atomic::Ordering::SeqCst) {
        return frame;
    }

    let interrupted = running_id();

    // Wake tasks whose sleep expired (1 tick = 1 ms).
    let now = tick_count();
    {
        let mut sleeping = SLEEPING.lock();
        let mut i = 0;
        while i < sleeping.len() {
            if sleeping[i].1 <= now {
                let id = sleeping[i].0;
                sleeping.swap_remove(i);
                with_sched_locked(|s| s.wake(id));
            } else {
                i += 1;
            }
        }
    }

    // A kernel task requested a voluntary yield (<= 1 ms latency): rotate
    // it to the back of the queue so the next tick preempts.
    let action = if KERNEL_YIELD.swap(false, core::sync::atomic::Ordering::SeqCst) {
        if let Some(cur) = interrupted {
            with_sched_locked(|s| {
                s.block(cur);
                s.wake(cur);
            });
        }
        SchedAction::Preempt(None)
    } else {
        with_sched_locked(|s| s.on_tick())
    };

    match action {
        SchedAction::Continue => {
            set_restore_flag(interrupted);
            frame
        }
        SchedAction::Preempt(_) => {
            let next = running_id();
            if next == interrupted {
                set_restore_flag(interrupted);
                return frame;
            }
            // Save where the interrupted task's registers live.
            if let Some(interrupted) = interrupted {
                if let Some(e) = entry_mut(interrupted) {
                    e.save_area = frame;
                }
            }
            switch_to(next, interrupted)
        }
    }
}

/// Called from the syscall path: park the current task (via `block`), then
/// switch to whatever `block` dispatched. Interrupts are off.
/// Returns the frame the syscall stub must restore.
pub fn syscall_park(
    current_frame: *mut CpuFrame,
    block: impl FnOnce(&mut Scheduler, TaskId),
) -> *mut CpuFrame {
    let current = running_id().expect("syscall outside task");
    // record our current frame (it will be restored later)
    if let Some(e) = entry_mut(current) {
        e.save_area = current_frame;
    }
    with_sched_locked(|s| block(s, current));
    let next = running_id().expect("dispatch after park");
    switch_to(Some(next), Some(current))
}

/// Kernel tasks (e.g. the shell) yield voluntarily: set a flag the next
/// timer tick honours, then sleep until that tick. The tick handler rotates
/// the task to the back of the ready queue *and clears the flag*, so the
/// loop returns and the caller may re-poll its input (the shell's console).
static KERNEL_YIELD: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

pub fn kernel_yield() {
    KERNEL_YIELD.store(true, core::sync::atomic::Ordering::SeqCst);
    loop {
        // Re-check with interrupts off so the flag cannot be consumed and
        // re-set in the window between the load and the HLT (which would
        // sleep forever). Once a tick has rotated us back, return so the
        // caller (e.g. the shell) can re-poll its input.
        x86_64::instructions::interrupts::disable();
        if !KERNEL_YIELD.load(core::sync::atomic::Ordering::SeqCst) {
            x86_64::instructions::interrupts::enable();
            return;
        }
        x86_64::instructions::interrupts::enable_and_hlt();
    }
}

/// Wake a task parked by IPC (re-enters the ready queue) and rewind its
/// syscall frame so the *same* operation is retried when it runs again:
/// the syscall entry stub marks every syscall frame with `vec == 256`, and
/// the `syscall` instruction is always 2 bytes (retry-rip = rip - 2).
/// Without the rewind a woken sender/receiver would resume *after* its
/// syscall with the syscall number in `rax` — i.e. the message is lost.
pub fn wake_parked(id: TaskId) {
    with_sched_locked(|s| s.wake(id));
    if let Some(e) = entry_mut(id) {
        if e.is_user && !e.save_area.is_null() {
            // # Safety: the save area belongs to this task; interrupts are
            // off in every call site (IPC wake happens on syscall/IRQ path).
            let fr = unsafe { &mut *e.save_area };
            if fr.vec == 256 {
                fr.rip = fr.rip.wrapping_sub(2);
            }
        }
    }
}

/// The task currently holding the CPU (from the scheduler's view).
pub fn running_id() -> Option<TaskId> {
    SCHED
        .lock()
        .as_ref()
        .and_then(|s| s.running())
}

/// Called from the syscall path for a plain yield.
pub fn syscall_yield(current_frame: *mut CpuFrame) -> *mut CpuFrame {
    syscall_park(current_frame, |s, id| {
        s.block(id);
        s.wake(id);
    })
}

/// Called from the syscall path: the task terminates. Its kernel stack must
/// stay alive (we are executing on it right now), so the entry is moved to
/// the graveyard instead of freed.
pub fn syscall_exit(_current_frame: *mut CpuFrame) -> *mut CpuFrame {
    let current = running_id().expect("exit outside task");
    with_sched_locked(|s| s.finish(current));

    let mut guard = TASKS.lock();
    if let Some(idx) = guard.iter().position(|t| t.id == current) {
        let entry = guard.remove(idx);
        GRAVEYARD.lock().push(entry);
    }
    drop(guard);

    let next = running_id();
    switch_to(next, Some(current))
}

/// Switch the CPU to `next`'s save area; `current` is the task whose frame
/// we are leaving (already saved by the caller, or None during bootstrap).
fn switch_to(next: Option<TaskId>, current: Option<TaskId>) -> *mut CpuFrame {
    let _ = current;
    match next {
        Some(id) => {
            // Point the hardware at the task that will run:
            //  * TSS.rsp0 —kernel stack for ring-3→ interrupts,
            //  * GS area  —kernel stack for the `syscall` instruction,
            //  * restore flag —whether the stub must `swapgs` back.
            let top = e_kstack_top(id);
            gdt::set_rsp0(VirtAddr::new(top));
            crate::syscall::gs_area().kstack.store(top, core::sync::atomic::Ordering::SeqCst);
            set_user_ret_flag(is_user_task(id));

            let area = entry_mut(id)
                .map(|e| e.save_area)
                .unwrap_or(core::ptr::null_mut());
            area
        }
        None => {
            // Nothing ready: keep executing the current frame (the ISR
            // simply returns to what it interrupted).
            set_restore_flag(current);
            current_frame_none()
        }
    }
}

fn current_frame_none() -> *mut CpuFrame {
    // Cannot happen in practice (idle task always exists); return a dummy.
    core::ptr::null_mut()
}

fn set_restore_flag(id: Option<TaskId>) {
    let user = id.is_some_and(is_user_task);
    set_user_ret_flag(user);
}

fn tick_count() -> u64 {
    SCHED.lock().as_ref().map_or(0, |s| s.ticks())
}

/// with_sched variant usable from within ISR context (does not touch IF).
fn with_sched_locked<R>(f: impl FnOnce(&mut Scheduler) -> R) -> R {
    let mut opt = SCHED.lock();
    let sched = opt.as_mut().expect("scheduler not initialised");
    f(sched)
}

/// Sleep the calling task for `ms` milliseconds (user-mode syscall path).
pub fn syscall_sleep(current_frame: *mut CpuFrame, ms: u64) -> *mut CpuFrame {
    let now = tick_count();
    let wake_at = now + ms;
    syscall_park(current_frame, move |s, id| {
        SLEEPING.lock().push((id, wake_at));
        // Park and dispatch the next task.
        s.block(id);
    })
}

// -------------------------------------------------------------------------
// Boot
// -------------------------------------------------------------------------

pub fn boot_tasks() {
    // Idle task: always runnable, lowest priority; hlt's when nothing else.
    spawn_kernel("idle", 0, 1, idle_loop, 0);
    if kairos_core::config::REALTIME_DEMO {
        // Two realtime user tasks with different periods (requires the
        // EDF policy at build time; safe under any policy).
        crate::user::spawn_deadline_demo();
    }
}

/// Bootstrap pause flag: while set, the timer IRQ handler only acks and
/// returns without dispatching. `kernel_main` sets it right before spawning
/// the first task so that the very first `enter_task_frame` cannot race a
/// context switch; the idle task clears it as its first act, after which
/// normal preemption applies.
static PAUSE_TIMER: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Set/clear the timer pause flag.
pub fn set_timer_paused(p: bool) {
    PAUSE_TIMER.store(p, core::sync::atomic::Ordering::SeqCst);
}

extern "C" fn idle_loop(_arg: usize) -> ! {
    // End the bootstrap pause: from here on normal preemption applies.
    PAUSE_TIMER.store(false, core::sync::atomic::Ordering::SeqCst);
    loop {
        x86_64::instructions::hlt();
    }
}

/// Hand execution to the scheduler's first dispatch (the idle task). The
/// CPU *must* enter via a scheduler dispatch so that `running()` is correct
/// from the very first instruction of user code — otherwise preemption
/// bookkeeping (whose frame is on the stack) breaks immediately. Never
/// returns.
pub fn bootstrap_first_task() -> ! {
    // One tick with nothing running forces `dispatch()`; with idle + shell
    // registered the result is always `Some`.
    let first = with_sched(|s| match s.on_tick() {
        SchedAction::Preempt(next) => next,
        _ => None,
    });
    let id = first.expect("bootstrap: no runnable task");
    let area = entry_mut(id).map(|e| e.save_area).unwrap_or(core::ptr::null_mut());
    set_user_ret_flag(false);
    unsafe {
        crate::interrupts::enter_task_frame(area);
    }
}

// -------------------------------------------------------------------------
// ps-style dump
// -------------------------------------------------------------------------

pub fn dump_tasks() {
    serial::write_line("  ID  NAME                  MODE   PRI  WGT  RUNS  PRE  MISS  TICKS");
    // Snapshot the scheduler's table under the canonical interrupt-safe
    // accessor: locking SCHED directly would let a timer IRQ spin forever on
    // the same lock (the ISR also takes SCHED), freezing the shell.
    let rows: alloc::vec::Vec<(TaskId, u8, u32, u64, u64, u64, u64)> = {
        let mut rows = alloc::vec::Vec::new();
        x86_64::instructions::interrupts::without_interrupts(|| {
            let guard = SCHED.lock();
            if let Some(s) = guard.as_ref() {
                for t in s.iter() {
                    // Exited tasks are moved to the graveyard at exit; do not
                    // show them in the live table.
                    if t.state == kairos_core::sched::TaskState::Finished {
                        continue;
                    }
                    rows.push((
                        t.id,
                        t.priority,
                        t.weight,
                        t.stats.runs,
                        t.stats.preemptions,
                        t.stats.deadline_misses,
                        t.stats.total_ticks,
                    ));
                }
            }
        });
        rows
    };
    for (id, pri, wgt, runs, pre, miss, ticks) in rows {
        let mode = if TASKS.lock().iter().any(|e| e.id == id && e.is_user) {
            "user"
        } else {
            "kern"
        };
        serial::write_line(&format!(
            "  {:>3}  {:<20}  {:<4}  {:>3}  {:>3}  {:>5} {:>4}  {:>4}  {:>5}",
            id,
            name_of(id),
            mode,
            pri,
            wgt,
            runs,
            pre,
            miss,
            ticks
        ));
    }
}

/// Kernel self-test for the task layer (runs in VM test mode).
pub fn run_tests() -> bool {
    let mut ok = true;
    // Scheduler policy sanity: a fresh WRR scheduler must accept tasks.
    let mut s = Scheduler::new(kairos_core::config::SCHED_POLICY.into(), 10);
    ok &= s.register(1, 1, 1, None).is_ok();
    ok &= s.register(2, 1, 1, None).is_ok();
    let action = s.on_tick();
    ok &= matches!(action, SchedAction::Preempt(Some(_)));
    // Yield one task and check the other takes over.
    s.block(1);
    ok &= s.running() == Some(2);
    serial::write_line("task:self-test:ok");
    ok
}

/// Called by the shell's `shutdown` and by tests to stop the VM.
pub fn tick_now() -> u64 {
    tick_count()
}
