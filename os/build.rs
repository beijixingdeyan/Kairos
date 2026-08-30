//! Build the bootable BIOS disk image: feed the kernel ELF to the
//! `bootloader` crate, which produces a FAT32 MBR image SeaBIOS can boot.

use std::path::PathBuf;
use std::{env, fs};

fn main() {
    let kernel_path = env::var("CARGO_BIN_FILE_KERNEL_kernel")
        .expect("artifact dependency kernel missing (bindeps?)");

    let out_dir = PathBuf::from(env::var("OUT_DIR").unwrap());
    let image_path = out_dir.join("kairos-bios.img");
    bootloader::BiosBoot::new(std::path::Path::new(&kernel_path))
        .create_disk_image(&image_path)
        .expect("failed to create BIOS disk image");

    // Guard against a silently-empty image (a previous version of this
    // script copied the image onto itself, which truncates it to zero
    // bytes on Linux and made every CI boot fail: the BIOS never found a
    // boot sector). Fail loudly instead of shipping an unbootable image.
    let size = fs::metadata(&image_path)
        .unwrap_or_else(|e| panic!("cannot stat {image_path:?}: {e}"))
        .len();
    assert!(
        size > 0,
        "BIOS disk image is empty ({size} bytes) — refusing to proceed"
    );
    eprintln!("[build] boot image ready: {size} bytes at {image_path:?}");

    // Publish the image location to the runner.
    println!("cargo:rustc-env=KAIROS_BIOS_IMAGE={}", image_path.display());

    println!("cargo:rerun-if-changed={kernel_path}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=KAIROS_TEST");
}