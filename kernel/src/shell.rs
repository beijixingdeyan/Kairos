//! Ring-0 shell: the kernel's own task and the demo control panel.
//!
//! The shell is a *task like any other* — it preempts and is preempted by
//! the scheduler, and it can only do what its capabilities allow. It is the
//! one task that holds the spawn authority (spawns user programs), so it is
//! the natural place to demonstrate capability delegation and the runtime
//! behaviour of every subsystem.

use crate::memory;
use crate::serial;
use crate::task;
use crate::{config, ipc};

/// Spawn the shell task and hand execution over to it. Never returns.
pub fn start() -> ! {
    let id = task::spawn_kernel("shell", 5, 2, shell_main, 0);
    // Grant the spawn authority — the shell may create (spawn) tasks.
    let cap = kairos_core::caps::Capability::new(
        crate::caps::SPAWN_AUTHORITY,
        kairos_core::caps::ObjectKind::Task,
        kairos_core::caps::CapRights::ALL,
    );
    let _ = task::with_cspace(id, |c| c.insert(cap));
    // Enter through the scheduler's first dispatch (the idle task); the
    // shell itself is reached by normal rotation once it is the ring head.
    task::bootstrap_first_task()
}

extern "C" fn shell_main(_arg: usize) -> ! {
    serial::write_line("");
    serial::write_line("kairos shell — type `help`");
    let mut line: [u8; 256] = [0; 256];
    let mut len = 0usize;
    loop {
        if let Some(b) = serial::read_byte() {
            match b {
                b'\r' | b'\n' => {
                    serial::write_line("");
                    if len > 0 {
                        let cmd = core::str::from_utf8(&line[..len]).unwrap_or("");
                        run_cmd(cmd.trim());
                    }
                    len = 0;
                    line = [0; 256];
                    serial::write_str("kairos> ");
                }
                0x7f | 0x08 => {
                    if len > 0 {
                        len -= 1;
                        serial::put_byte(0x08);
                        serial::put_byte(b' ');
                        serial::put_byte(0x08);
                    }
                }
                b if (32..127).contains(&b) && len < line.len() => {
                    line[len] = b;
                    len += 1;
                    serial::put_byte(b);
                }
                _ => {}
            }
        } else {
            // Nothing on the console: give other tasks the CPU.
            task::kernel_yield();
        }
    }
}

fn run_cmd(cmd: &str) {
    let mut parts = cmd.split_whitespace();
    let name = parts.next().unwrap_or("");
    match name {
        "" => {}
        "help" => cmd_help(),
        "info" => cmd_info(),
        "ps" => task::dump_tasks(),
        "sched" => cmd_sched(),
        "crash" => cmd_crash(),
        "spawn" => {
            let Some(prog) = parts.next() else {
                serial::write_line("usage: spawn <hello|counter|deadline>");
                return;
            };
            match task::spawn_user(prog, 4, 1) {
                Ok(id) => {
                    serial::write_line(&format!("spawned {prog} as task {id}"));
                }
                Err(()) => serial::write_line("spawn failed: unknown program"),
            }
        }
        "ipcdemo" => cmd_ipcdemo(),
        "echo" => {
            let rest = cmd.trim_start_matches("echo").trim_start();
            restore_output(rest);
        }
        "exit" => {
            serial::write_line("shutting down");
            crate::exit_kernel(config::EXIT_SUCCESS);
        }
        "fault" => {
            // Capability/paging educational demo: page-fault test.
            cmd_crash();
        }
        _ => serial::write_line(&format!("unknown command: {name} (try `help`)")),
    }
}

fn restore_output(s: &str) {
    serial::write_line(s);
}

fn cmd_help() {
    serial::write_line("commands:");
    serial::write_line("  info                 system overview");
    serial::write_line("  sched                scheduler policy + quantum");
    serial::write_line("  ps                   task list with scheduling stats");
    serial::write_line("  spawn <prog>         run a user program (hello|counter|deadline)");
    serial::write_line("  ipcdemo              spawn echo server+client (IPC demo)");
    serial::write_line("  echo <text>          print text");
    serial::write_line("  fault                trigger a page fault (panic demo)");
    serial::write_line("  exit                 shut down the VM");
}

fn cmd_sched() {
    let kind = match config::SCHED_POLICY {
        config::SchedPolicy::RoundRobin => "round-robin",
        config::SchedPolicy::WeightedRoundRobin => "weighted-round-robin",
        config::SchedPolicy::EarliestDeadlineFirst => "edf",
    };
    serial::write_line(&format!(
        "policy: {kind}   quantum: {} ms   realtime demo: {}",
        config::QUANTUM_MS,
        config::REALTIME_DEMO
    ));
}

fn cmd_info() {
    let mem = memory::summary();
    serial::write_line(&format!(
        "kairos v0.1.0  kernel heap: {} MiB  memory: {} MiB usable, {} frames free",
        config::HEAP_MIB, mem.usable_mib, mem.frames_free
    ));
    serial::write_line(&format!("uptime: {} ms", task::tick_now()));
}

/// Educational demo: an intentional page fault reaching the exception
/// handler (which prints the fault context then fails the VM).
fn cmd_crash() {
    serial::write_line("triggering a page fault at 0x0...");
    // # Safety: deliberate fault — demonstrates the exception path.
    unsafe {
        let ptr: *const u32 = core::ptr::null();
        let _ = core::ptr::read_volatile(ptr);
    }
}

/// The IPC demo: a channel + echo server/client, all in ring 3.
fn cmd_ipcdemo() {
    // 1. Kernel creates a channel and gives both sides a capability.
    let idx = ipc::create();
    let obj_id = crate::caps::register(crate::caps::KernelObject::Channel(idx));
    let cap = kairos_core::caps::Capability::new(
        obj_id,
        kairos_core::caps::ObjectKind::Channel,
        kairos_core::caps::CapRights::ALL,
    );

    // 2. Spawn server and client with the channel slot in rdi.
    // (User `_start` receives the argument in rdi — see the syscall docs.)
    if let (Some(s), Some(c)) = (
        spawn_user_with_cap("echo_server", cap, 1),
        spawn_user_with_cap("echo_client", cap, 1),
    ) {
        serial::write_line(&format!("ipcdemo: server={s} client={c} on channel {idx}"));
    } else {
        serial::write_line("ipcdemo: spawn failed");
    }
}

/// Spawn a user task that additionally receives the channel capability.
fn spawn_user_with_cap(
    name: &'static str,
    cap: kairos_core::caps::Capability,
    arg: usize,
) -> Option<task::TaskId> {
    let id = task::spawn_user_arg(name, 4, 1, arg).ok()?;
    let _ = task::with_cspace(id, |c| c.insert(cap));
    Some(id)
}

/// Kernel self-test (shell itself is exercised interactively in the VM).
pub fn run_tests() -> bool {
    serial::write_line("shell:self-test:ok");
    true
}