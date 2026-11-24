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
    // NitroXide runs on the scene core (SCENES.md): the scripts go in the order the core's ABI
    // gives, the RAM size, psoxide-core.ld, psoxide-boot.ld (the engine's boot splash in boot-only
    // memory), then psoxide.ld. 0x1FE000 puts STACK_INIT at 0x801FDF00 and leaves
    // 0x801FDF00..0x80200000 to the core's window (8,192 B) and its 256 B handoff block. Sections
    // run apart from where they are stored, so the link is an ELF and the driver (psoxide-pgo)
    // writes the EXE by load address.
    const RAM_SIZE_OVERRIDE: u32 = 0x1f_e000;
    let ram_size = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("ram-size.ld");
    std::fs::write(
        &ram_size,
        format!("RAM_SIZE_OVERRIDE = {RAM_SIZE_OVERRIDE:#X};\n"),
    )
    .expect("write ram-size.ld");
    println!("cargo:rustc-link-arg=-T{}", ram_size.display());
    for name in [
        ".psoxide/sdk/psoxide-core.ld",
        ".psoxide/sdk/psoxide-boot.ld",
    ] {
        let script = repo_root.join(name);
        let script = script.canonicalize().unwrap_or(script);
        println!("cargo:rustc-link-arg=-T{}", script.display());
        println!("cargo:rerun-if-changed={}", script.display());
    }
    println!("cargo:rustc-link-arg=-T{}", ld.display());
    // Always an ELF (see above): `PSOXIDE_LINK_ELF`, which the driver sets, changes nothing here.
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
