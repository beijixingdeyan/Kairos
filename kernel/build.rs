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
    // kernel loader maps them on demand. No spaces allowed in the flag
    // (RUSTFLAGS is split on whitespace).
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
        // Link base via a linker section option. The long form
        // `--section-start=.text=` is accepted by every lld release.
        //
        // Transport: plain RUSTFLAGS only. Modern cargo splits RUSTFLAGS on
        // whitespace (stable behaviour), whereas CARGO_ENCODED_RUSTFLAGS
        // uses a separator that has changed across cargo versions (`\n` vs
        // `\x1f`); passing the "encoded" form verbatim leaked a trailing
        // newline into the linker argument on CI and failed every link.
        // env_remove guarantees the child cargo never picks an inherited
        // encoded variable from the parent build. No spaces in the arg.
        let arg = format!("--section-start=.text={base}");
        let status = Command::new("cargo")
            .env_remove("CARGO_ENCODED_RUSTFLAGS")
            .env("RUSTFLAGS", format!("-Clink-arg={arg}"))
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