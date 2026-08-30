//! Kernel build script.
//!
//! Responsibilities:
//! 1. Build the user-space program binaries (ring-3) for the bare-metal
//!    target and expose their paths via `cargo:rustc-env`, so the kernel can
//!    `include_bytes!` them into the image (loadable by the kernel's ELF
//!    loader at boot).
//! 2. Forward the `KAIROS_*` configuration variables to `kairos-core`'s
//!    build script (it re-reads the environment).

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=../user/src");
    println!("cargo:rerun-if-changed=../user/Cargo.toml");

    // Build all user binaries. They are separate workspace members, so this
    // nested invocation reuses the workspace lock and cache. Each program is
    // linked at its own virtual base, 16 MiB apart, inside the kernel-declared
    // user region (kernel/src/user.rs: USER_BASE = 0x10_0000_0000), so all
    // baked programs coexist in the single shared user address space; the
    // kernel loader maps them on demand. No spaces allowed in RUSTFLAGS
    // (cargo splits on whitespace), hence `-Clink-arg=-Ttext=...`.
    let user_target = std::path::Path::new("..")
        .join("target")
        .join("x86_64-unknown-none")
        .join("release");
    let bases = [
        ("hello", "0x1000000000"),
        ("echo_server", "0x1001000000"),
        ("echo_client", "0x1002000000"),
        ("counter", "0x1003000000"),
        ("deadline", "0x1004000000"),
    ];
    for (bin, base) in bases {
        // `--section-start=.text=` (long form) instead of `-Ttext=`: recent
        // LLVM lld dropped the `-Ttext` short alias, and CI rebuilds from a
        // cold cache exercise the linker where incremental local builds may
        // not. The long option is accepted by every lld release.
        let arg = format!("--section-start=.text={base}");
        let encoded = format!("-Clink-arg={arg}\n");
        let status = Command::new("cargo")
            // Modern cargo consults `CARGO_ENCODED_RUSTFLAGS` (newline-
            // separated flags) and ignores plain RUSTFLAGS when it is set;
            // the parent cargo may export either, so set both forms to the
            // same value to guarantee the link base reaches the linker.
            .env("RUSTFLAGS", format!("-Clink-arg={arg}"))
            .env("CARGO_ENCODED_RUSTFLAGS", &encoded)
            .args([
                "build",
                "--manifest-path",
                "../user/Cargo.toml",
                "--target",
                "x86_64-unknown-none",
                "--release",
                "--bin",
                bin,
            ])
            .status()
            .expect("failed to spawn nested cargo for user programs");
        assert!(status.success(), "failed to build user program {bin}");
    }

    // Expose each built binary to the kernel crate. The workspace shares one
    // target dir, so the binaries land in ../target/x86_64-unknown-none/
    // release (resolved from this build script's CWD = the kernel crate).
    for (var, bin) in [
        ("USER_BIN_HELLO", "hello"),
        ("USER_BIN_ECHO_SERVER", "echo_server"),
        ("USER_BIN_ECHO_CLIENT", "echo_client"),
        ("USER_BIN_COUNTER", "counter"),
        ("USER_BIN_DEADLINE", "deadline"),
    ] {
        let path = user_target.join(bin).canonicalize().unwrap_or_else(|e| {
            panic!("user binary {bin} missing ({e}) — nested build failed?")
        });
        println!("cargo:rustc-env={var}={}", path.display());
    }
}