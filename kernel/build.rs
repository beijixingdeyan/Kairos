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
    // nested invocation reuses the workspace lock and cache. We pin the user
    // image base via `-Ttext` so the kernel loader can hard-code its user
    // address space layout (see kernel/src/user.rs). No spaces allowed in
    // RUSTFLAGS (cargo splits on whitespace), hence `-Clink-arg=-Ttext=...`.
    let status = Command::new("cargo")
        .env(
            "RUSTFLAGS",
            "-Clink-arg=-Ttext=0x100000000",
        )
        .args([
            "build",
            "--manifest-path",
            "../user/Cargo.toml",
            "--target",
            "x86_64-unknown-none",
            "--release",
            "--bins",
        ])
        .status()
        .expect("failed to spawn nested cargo for user programs");
    assert!(status.success(), "failed to build user programs");

    // Expose each built binary to the kernel crate. The workspace shares one
    // target dir, so the binaries land in ../target/x86_64-unknown-none/
    // release (resolved from this build script's CWD = the kernel crate).
    let user_target = std::path::Path::new("..")
        .join("target")
        .join("x86_64-unknown-none")
        .join("release");
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