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
    fs::copy(&image_path, out_dir.join("kairos-bios.img")).ok();

    // Publish the image location to the runner.
    println!("cargo:rustc-env=KAIROS_BIOS_IMAGE={}", image_path.display());

    println!("cargo:rerun-if-changed={kernel_path}");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=KAIROS_TEST");
}