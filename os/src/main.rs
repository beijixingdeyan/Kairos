//! QEMU runner: launches the built BIOS image and maps the guest exit code
//! (written via the isa-debug-exit device) back to a process exit status.
//!
//! Exit code mapping used by CI / `make test`:
//! * guest `0x10` (EXIT_SUCCESS) → runner exit 0
//! * guest `0x11` (EXIT_FAILURE) → runner exit 1
//! * anything else (crash/abort)  → runner exit 2
//!
//! The guest serial console is wired to a loopback TCP socket on every
//! host: QEMU listens (`-serial tcp:127.0.0.1:<port>,server=on,wait=on`) and
//! the runner connects before the VM starts, so no console bytes are ever
//! lost. `-serial stdio` was dropped because on Linux CI the guest output
//! never made it through QEMU's stdio backend (pipelined C-stdio
//! buffering and terminal-mode quirks), while the socket streams straight
//! into the runner, which pumps it to stdout and forwards stdin to QEMU.

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const IMAGE: &str = env!("KAIROS_BIOS_IMAGE");

/// A console channel: readable from the guest UART, writable into it.
trait Console: Read + Write + Send {}

/// A TCP-wired console. QEMU is the socket server (`wait=on`), so the
/// guest does not start until we connect and no console bytes are lost.
struct SocketConsole(std::net::TcpStream);

impl Read for SocketConsole {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for SocketConsole {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

impl Console for SocketConsole {}

fn main() {
    // Windows host timers default to a ~15.6 ms quantum. Raising this
    // process's timer resolution to 1 ms is standard hygiene for QEMU
    // runners; on this host it did not measurably change the guest clock
    // (the underlying limit is QEMU's own virtual-clock delivery), but it
    // is harmless and helps other host paths. Best-effort: ignore errors.
    #[cfg(windows)]
    {
        #[link(name = "winmm")]
        unsafe extern "system" {
            fn timeBeginPeriod(uPeriod: u32) -> u32;
        }
        unsafe {
            let _ = timeBeginPeriod(1);
        }
    }

    // QEMU binary: $KAIROS_QEMU, or the in-repo copy under tools/qemu.
    let qemu = std::env::var("KAIROS_QEMU").unwrap_or_else(|_| {
        let repo = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..");
        repo.join("tools")
            .join("qemu")
            .join("qemu-system-x86_64.exe")
            .to_string_lossy()
            .into_owned()
    });

    // Extra command lines (e.g. `cargo run -p os -- -d int`) pass through.
    let extra: Vec<String> = std::env::args().skip(1).collect();

    // QEMU listens on a loopback socket (wait=on: the VM does not start
    // until we connect, so no console bytes are ever lost). The port is
    // picked by binding ephemeral then freeing it for QEMU.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .expect("no free loopback port")
        .local_addr()
        .expect("no local addr")
        .port();
    let serial_arg = format!("tcp:127.0.0.1:{port},server=on,wait=on");
    let console_port = Some(port);

    let mut cmd = Command::new(&qemu);
    cmd.args([
        "-drive",
        &format!("format=raw,file={IMAGE}"),
        "-serial",
        &serial_arg,
        "-display",
        "none",
        "-device",
        "isa-debug-exit,iobase=0xf4,iosize=0x04",
        "-no-reboot",
        "-m",
        "256M",
        "-smp",
        "1",
    ]);
    cmd.args(&extra);
    // Note: `-icount` was evaluated for deterministic wall timing but, on
    // this QEMU build (Windows TCG), it makes guest timer delivery fast
    // enough (~600 Hz) to expose rare lock-holder-preemption races in the
    // scheduler, wedging task registration. The default (no icount) is
    // stable; see docs/ARCHITECTURE.md "Timing and emulation" for the
    // trade-offs and how to pass `-icount shift=auto` for wall-accurate
    // timing when emphasizing responsiveness over robustness.
    // The console travels over the socket, so QEMU's own stdio is unused.
    cmd.stdin(Stdio::null()).stdout(Stdio::null());
    cmd.stderr(Stdio::inherit());

    eprintln!(
        "kicking off QEMU: {qemu} (image {IMAGE}, serial {serial_arg})"
    );
    let Ok(mut child) = cmd.spawn() else {
        eprintln!(
            "error: could not start QEMU at {qemu}.\n\
             set KAIROS_QEMU to your qemu-system-x86_64 path."
        );
        std::process::exit(2);
    };

    // Connect to QEMU's listener (it waits for us before starting the VM).
    let console: Box<dyn Console> = match console_port {
        Some(port) => {
            let mut conn = None;
            let mut last_err = String::from("never attempted");
            for attempt in 0..200 {
                // `connect_timeout` caps each attempt so a misbehaving
                // listener (dropped SYNs) cannot block the loop forever.
                let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
                match std::net::TcpStream::connect_timeout(
                    &addr,
                    std::time::Duration::from_millis(250),
                ) {
                    Ok(s) => {
                        conn = Some(s);
                        eprintln!("[runner] serial console connected");
                        break;
                    }
                    Err(e) => {
                        last_err = e.to_string();
                        if attempt % 25 == 0 {
                            eprintln!("[runner] serial connect attempt {attempt}: {e}");
                        }
                        std::thread::sleep(std::time::Duration::from_millis(25));
                    }
                }
            }
            match conn {
                Some(s) => {
                    // A short read timeout on the drain side is essential:
                    // without it the drain thread parks in `read` while
                    // holding the console mutex, starving the stdin-forward
                    // thread exactly when the guest is idle waiting for
                    // input (the shell would never see any command).
                    let _ = s.set_read_timeout(Some(std::time::Duration::from_millis(50)));
                    Box::new(SocketConsole(s))
                }
                None => {
                    eprintln!(
                        "error: could not connect to QEMU serial console (last: {last_err})"
                    );
                    let _ = child.kill();
                    let _ = child.wait();
                    std::process::exit(2);
                }
            }
        }
        None => {
            eprintln!("error: no console port configured");
            let _ = child.kill();
            let _ = child.wait();
            std::process::exit(2);
        }
    };

    // Two threads share the console: one drains guest output to our stdout,
    // the other forwards our stdin into the guest.
    let console = std::sync::Arc::new(std::sync::Mutex::new(console));
    let drain_console = std::sync::Arc::clone(&console);
    let out = std::thread::spawn(move || {
        let mut buf = [0u8; 1024];
        let stdout = std::io::stdout();
        let mut lock = stdout.lock();
        loop {
            let n = {
                let mut c = drain_console.lock().unwrap();
                let n = c.read(&mut buf);
                match n {
                    Ok(0) => break,
                    // Read timeouts are the drain's liveness heartbeat: they
                    // release the mutex so the stdin-forward thread can write
                    // (see the console setup above).
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::TimedOut =>
                    {
                        continue
                    }
                    Err(_) => break,
                    Ok(n) => n,
                }
            };
            let _ = lock.write_all(&buf[..n]);
            let _ = lock.flush();
        }
    });

    let stdin = std::io::stdin();
    let mut lock = stdin.lock();
    let mut buf = [0u8; 256];
    let mut first_byte = true;
    let started = std::time::Instant::now();
    loop {
        match lock.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                // Console input is only latched by the 16550 once the kernel
                // has initialised it; forward the first chunk only after the
                // guest is up (tests feed no stdin, so they stay fast).
                if first_byte {
                    first_byte = false;
                    let wait =
                        std::time::Duration::from_millis(12_000).saturating_sub(started.elapsed());
                    if !wait.is_zero() {
                        std::thread::sleep(wait);
                    }
                }
                let mut c = console.lock().unwrap();
                if c.write_all(&buf[..n]).is_err() {
                    break;
                }
            }
        }
    }

    let status = child.wait().expect("failed to wait for QEMU");
    let _ = out.join();
    finish(status.code().unwrap_or(-1) as u32);
}

fn finish(code: u32) -> ! {
    // The isa-debug-exit device packs the byte written to port 0xf4 into
    // QEMU's exit status as `(val << 1) | 1` (verified against QEMU 9.2.0);
    // decode it back before matching against the guest constants.
    let guest = code.checked_sub(1).map(|c| c >> 1).unwrap_or(0xFF);
    let mapped = match guest {
        0x10 => 0, // guest SUCCESS
        0x11 => 1, // guest FAILURE
        _ => 2,    // abnormal termination
    };
    eprintln!("QEMU exited with guest code 0x{guest:x} → host exit {mapped}");
    std::process::exit(mapped);
}