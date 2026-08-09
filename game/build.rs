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
    println!("cargo:rustc-link-arg=--oformat=binary");
    println!("cargo:rerun-if-changed={}", ld.display());
}
