//! Fail the `pipewire` feature early when libpipewire is older than 1.2.
//!
//! pipewire-rs 0.10 built with `v1_2_0` references `spa_meta_sync_timeline`,
//! which is absent from libspa 1.0.x and surfaces as E0425. Cargo builds this
//! script in parallel with that crate. In a normal parallel build this check
//! is the message you see. A `-j1` build can still print the libspa error
//! first. The check runs only when the feature is on, so `cargo test
//! --workspace` stays free of libpipewire.

use std::process::Command;

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG");
    println!("cargo:rerun-if-env-changed=PKG_CONFIG_PATH");

    if std::env::var_os("CARGO_FEATURE_PIPEWIRE").is_none() {
        return;
    }

    let pkg_config = std::env::var("PKG_CONFIG").unwrap_or_else(|_| "pkg-config".to_string());

    match Command::new(&pkg_config)
        .args(["--atleast-version=1.2", "libpipewire-0.3"])
        .status()
    {
        Ok(status) if status.success() => {}
        Ok(_) => {
            let found = Command::new(&pkg_config)
                .args(["--modversion", "libpipewire-0.3"])
                .output()
                .ok()
                .filter(|output| output.status.success())
                .and_then(|output| String::from_utf8(output.stdout).ok())
                .map(|version| version.trim().to_string())
                .filter(|version| !version.is_empty());
            let found = found.unwrap_or_else(|| "not found".to_string());
            panic!(
                "omamix --features pipewire needs libpipewire >= 1.2 (pkg-config name libpipewire-0.3).\n\
                 Found: {found}.\n\
                 pipewire-rs 0.10 with feature v1_2_0 fails to compile against 1.0.x \
                 (E0425, missing spa_meta_sync_timeline).\n\
                 Arch: sudo pacman -S pipewire libpipewire pkgconf clang\n\
                 Debian/Ubuntu: sudo apt install libpipewire-0.3-dev pkg-config clang libclang-dev"
            );
        }
        Err(error) => {
            panic!(
                "{pkg_config} is required to verify libpipewire >= 1.2 ({error}).\n\
                 Arch: sudo pacman -S pkgconf\n\
                 Debian/Ubuntu: sudo apt install pkg-config"
            );
        }
    }
}
