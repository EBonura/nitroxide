// SPDX-License-Identifier: GPL-2.0-or-later
//! Inject PSoXide's PSX linker script into the final link, by absolute path
//! derived from this crate's location. Keeps the crate buildable from anywhere
//! (no brittle relative `-T` paths in RUSTFLAGS) while the script itself lives
//! in the pinned submodule. Mirrors gh-psx / zelda3-psx / oot-psx.

use std::path::PathBuf;

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = manifest.parent().expect("crate must live at <repo>/game");
    let ld = repo_root.join(".psoxide/sdk/psoxide.ld");
    let ld = ld.canonicalize().unwrap_or(ld);
    println!("cargo:rustc-link-arg=-T{}", ld.display());
    // `PSOXIDE_LINK_ELF=1` (set by the SDK's psoxide-pgo driver) keeps the ELF
    // instead of the flat PSX-EXE, so the driver can read its DWARF. Same code
    // at the same addresses; it does not boot. Cargo passes build-script link
    // arguments after rustflags, so the driver's own --oformat=elf cannot
    // override this one.
    println!("cargo:rerun-if-env-changed=PSOXIDE_LINK_ELF");
    if std::env::var_os("PSOXIDE_LINK_ELF").is_none() {
        println!("cargo:rustc-link-arg=--oformat=binary");
    }
    println!("cargo:rerun-if-changed={}", ld.display());
    // `PSOXIDE_LINK_ORDER` (set by the SDK's psoxide-pgo driver for a
    // `+order` variant) names a symbol-ordering file for this link only. The
    // driver checks the relinked map follows it.
    println!("cargo:rerun-if-env-changed=PSOXIDE_LINK_ORDER");
    if let Some(order) = std::env::var_os("PSOXIDE_LINK_ORDER") {
        println!(
            "cargo:rustc-link-arg=--symbol-ordering-file={}",
            order.to_string_lossy()
        );
    }
}
