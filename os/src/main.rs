//! QEMU runner: launches the built BIOS image and maps the guest exit code
//! (written via the isa-debug-exit device) back to a process exit status.
//!
//! Exit code mapping used by CI / `make test`:
//! * guest `0x10` (EXIT_SUCCESS) → runner exit 0
//! * guest `0x11` (EXIT_FAILURE) → runner exit 1
//! * anything else (crash/abort)  → runner exit 2
//!
//! The guest serial console is wired to a side channel instead of plain
//! `-serial stdio`: QEMU's stdio backend does not reliably forward piped
//! (non-console) input on Windows. The runner pumps guest console output to
//! its stdout and forwards its stdin into the guest.
//!
//! * Windows: a named pipe (`-serial pipe:kairos-console`), the native way
//!   to hand a console through the QEMU process boundary.
//! * Other OSes: plain stdio inheritance (works for pipes and TTYs alike).

use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const IMAGE: &str = env!("KAIROS_BIOS_IMAGE");

/// A console channel: readable from the guest UART, writable into it.
trait Console: Read + Write + Send {}

#[cfg(windows)]
const CONSOLE_PIPE: &str = r"\\.\pipe\kairos-console";

/// A console that reads from stdin and writes to stdout (non-Windows).
#[cfg(not(windows))]
struct StdioConsole;

#[cfg(not(windows))]
impl Read for StdioConsole {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        std::io::stdin().read(buf)
    }
}

#[cfg(not(windows))]
impl Write for StdioConsole {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        std::io::stdout().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        std::io::stdout().flush()
    }
}

#[cfg(not(windows))]
impl Console for StdioConsole {}

/// A TCP-wired console (Windows: QEMU is the socket server).
#[cfg(windows)]
struct SocketConsole(std::net::TcpStream);

#[cfg(windows)]
impl Read for SocketConsole {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

#[cfg(windows)]
impl Write for SocketConsole {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[cfg(windows)]
impl Console for SocketConsole {}

/// Open the guest console side channel.
#[cfg(windows)]
fn open_console() -> std::io::Result<Box<dyn Console>> {
    use std::fs::OpenOptions;
    // Wait for QEMU to create the pipe server (it does so at startup).
    for _ in 0..200 {
        match OpenOptions::new().read(true).write(true).open(CONSOLE_PIPE) {
            Ok(f) => {
                let boxed: Box<dyn Console> = Box::new(PipeConsole(f));
                return Ok(boxed);
            }
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    }
    Err(std::io::Error::new(
        std::io::ErrorKind::NotFound,
        format!("named pipe {CONSOLE_PIPE} never appeared"),
    ))
}

/// A Windows named pipe endpoint (QEMU is the pipe server).
#[cfg(windows)]
struct PipeConsole(std::fs::File);

#[cfg(windows)]
impl Read for PipeConsole {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

#[cfg(windows)]
impl Write for PipeConsole {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

#[cfg(windows)]
impl Console for PipeConsole {}

#[cfg(not(windows))]
fn open_console() -> std::io::Result<Box<dyn Console>> {
    let boxed: Box<dyn Console> = Box::new(StdioConsole);
    Ok(boxed)
}

fn main() {
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

    let serial_arg: String = if cfg!(windows) {
        // Windows: QEMU listens on a loopback socket (wait=on: the VM does
        // not start until we connect, so no console bytes are ever lost).
        // The port is picked by binding ephemeral then freeing it for QEMU.
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("no free loopback port")
            .local_addr()
            .expect("no local addr")
            .port();
        format!("tcp:127.0.0.1:{port},server=on,wait=on")
    } else {
        // Other OSes: plain stdio inheritance (works with pipes and TTYs).
        "stdio".to_string()
    };
    let console_port: Option<u16> = {
        if cfg!(windows) {
            Some(
                serial_arg
                    .trim_start_matches("tcp:127.0.0.1:")
                    .split(',')
                    .next()
                    .unwrap()
                    .parse()
                    .unwrap(),
            )
        } else {
            None
        }
    };

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
    // On Windows the console travels over the socket; otherwise QEMU
    // inherits our stdio directly and must not be double-wired.
    if cfg!(windows) {
        cmd.stdin(Stdio::null()).stdout(Stdio::null());
    }
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

    // Windows: connect to QEMU's listener (it waits for us before starting
    // the VM). Other OSes: the console is our own stdio.
    let console: Box<dyn Console> = match console_port {
        Some(port) => {
            let mut conn = None;
            for _ in 0..200 {
                if let Ok(s) = std::net::TcpStream::connect(("127.0.0.1", port)) {
                    conn = Some(s);
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
            match conn {
                Some(s) => Box::new(SocketConsole(s)),
                None => {
                    eprintln!("error: could not connect to QEMU serial console");
                    let _ = child.kill();
                    let _ = child.wait();
                    std::process::exit(2);
                }
            }
        }
        None => open_console().unwrap_or_else(|e| {
            eprintln!("error: could not open the guest console channel: {e}");
            let _ = child.kill();
            let _ = child.wait();
            std::process::exit(2);
        }),
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
                    Ok(0) | Err(_) => break,
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