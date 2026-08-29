//! QEMU runner: launches the built BIOS image and maps the guest exit code
//! (written via the isa-debug-exit device) back to a process exit status.
//!
//! Exit code mapping used by CI / `make test`:
//! * guest `0x10` (EXIT_SUCCESS) → runner exit 0
//! * guest `0x11` (EXIT_FAILURE) → runner exit 1
//! * anything else (crash/abort)  → runner exit 2

use std::path::PathBuf;
use std::process::Command;

const IMAGE: &str = env!("KAIROS_BIOS_IMAGE");

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

    let mut cmd = Command::new(&qemu);
    cmd.args([
        "-drive",
        &format!("format=raw,file={IMAGE}"),
        "-serial",
        "mon:stdio",
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

    eprintln!("kicking off QEMU: {qemu} (image {IMAGE})");
    let Ok(mut child) = cmd.spawn() else {
        eprintln!(
            "error: could not start QEMU at {qemu}.\n\
             set KAIROS_QEMU to your qemu-system-x86_64 path."
        );
        std::process::exit(2);
    };

    let status = child.wait().expect("failed to wait for QEMU");
    let code = status.code().unwrap_or(-1) as u32;
    let mapped = match code {
        0x10 => 0, // guest SUCCESS
        0x11 => 1, // guest FAILURE
        _ => 2,    // abnormal termination
    };
    eprintln!("QEMU exited with guest code 0x{code:x} → host exit {mapped}");
    std::process::exit(mapped);
}