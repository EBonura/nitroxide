// SPDX-License-Identifier: GPL-2.0-or-later
//! Locate the versioned gate block in the exact PS-X EXE that will be packed.

use std::io::Write;
use std::path::Path;
use std::{env, fs};

const HEADER_LEN: usize = 0x800;
const MAGIC: &[u8; 4] = b"NITR";
const VERSION: u32 = 1;
const BLOCK_LEN: usize = 256;

fn word(bytes: &[u8], offset: usize) -> Result<u32, String> {
    let slice = bytes
        .get(offset..offset + 4)
        .ok_or("short PS-X EXE header")?;
    Ok(u32::from_le_bytes(
        slice.try_into().map_err(|_| "short word")?,
    ))
}

fn symbol_address(path: &Path) -> Result<u32, String> {
    block_address(path, MAGIC, VERSION, BLOCK_LEN, "NITRO_GATE_STATE")
}

fn block_address(
    path: &Path,
    magic: &[u8; 4],
    version: u32,
    block_len: usize,
    name: &str,
) -> Result<u32, String> {
    let bytes = fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.get(..8) != Some(b"PS-X EXE") {
        return Err("input is not a PS-X EXE".into());
    }
    let load = word(&bytes, 0x18)?;
    let image_len = word(&bytes, 0x1c)? as usize;
    let image = bytes
        .get(HEADER_LEN..HEADER_LEN + image_len)
        .ok_or("PS-X EXE payload is shorter than its header says")?;
    let mut matches = Vec::new();
    for (offset, block) in image.windows(block_len).enumerate() {
        if &block[..4] == magic
            && word(block, 4)? == version
            && word(block, 8)? == block_len as u32
            && block[12..].iter().all(|&byte| byte == 0)
        {
            matches.push(offset);
        }
    }
    if matches.len() != 1 {
        return Err(format!(
            "expected one initialized {name} block, found {}",
            matches.len()
        ));
    }
    load.checked_add(matches[0] as u32)
        .ok_or_else(|| "gate block address overflowed".into())
}

fn main() {
    let mut args = env::args_os().skip(1);
    let Some(exe) = args.next() else {
        eprintln!("usage: gate-symbols PSX_EXE");
        std::process::exit(2);
    };
    if args.next().is_some() {
        eprintln!("usage: gate-symbols PSX_EXE");
        std::process::exit(2);
    }
    match symbol_address(Path::new(&exe)) {
        Ok(addr) => {
            writeln!(std::io::stdout(), "{addr:#010x} NITRO_GATE_STATE").expect("write address");
            match block_address(Path::new(&exe), b"NSET", 1, 36, "NITRO_SETTINGS_STATE") {
                Ok(settings) => {
                    writeln!(std::io::stdout(), "{settings:#010x} NITRO_SETTINGS_STATE")
                        .expect("write settings address")
                }
                Err(error) => {
                    eprintln!("gate-symbols: {error}");
                    std::process::exit(1);
                }
            }
            match block_address(Path::new(&exe), b"NRPL", 2, 56, "NITRO_REPLAY_STATE") {
                Ok(replay) => writeln!(std::io::stdout(), "{replay:#010x} NITRO_REPLAY_STATE")
                    .expect("write replay address"),
                Err(error) => {
                    eprintln!("gate-symbols: {error}");
                    std::process::exit(1);
                }
            }
        }
        Err(error) => {
            eprintln!("gate-symbols: {error}");
            std::process::exit(1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(block_offsets: &[usize], declared_len: usize) -> Vec<u8> {
        let mut bytes = vec![0; HEADER_LEN + 0x300];
        bytes[..8].copy_from_slice(b"PS-X EXE");
        bytes[0x18..0x1c].copy_from_slice(&0x8001_0000u32.to_le_bytes());
        bytes[0x1c..0x20].copy_from_slice(&(declared_len as u32).to_le_bytes());
        for &offset in block_offsets {
            let block = &mut bytes[HEADER_LEN + offset..HEADER_LEN + offset + BLOCK_LEN];
            block[..4].copy_from_slice(MAGIC);
            block[4..8].copy_from_slice(&VERSION.to_le_bytes());
            block[8..12].copy_from_slice(&(BLOCK_LEN as u32).to_le_bytes());
        }
        bytes
    }

    fn check(name: &str, bytes: &[u8]) -> Result<u32, String> {
        let path = env::temp_dir().join(format!("gate-symbols-{}-{name}", std::process::id()));
        fs::write(&path, bytes).unwrap();
        let result = symbol_address(&path);
        fs::remove_file(path).unwrap();
        result
    }

    #[test]
    fn locates_block_in_declared_image_at_load_address() {
        assert_eq!(
            check("valid", &sample(&[0x40], 0x300)).unwrap(),
            0x8001_0040
        );
    }

    #[test]
    fn rejects_ambiguous_initializer() {
        assert_eq!(
            check("duplicate", &sample(&[0x40, 0x180], 0x300)).unwrap_err(),
            "expected one initialized NITRO_GATE_STATE block, found 2"
        );
    }

    #[test]
    fn rejects_truncated_declared_payload() {
        let mut bytes = sample(&[0x40], 0x300);
        bytes.truncate(HEADER_LEN + 0x200);
        assert_eq!(
            check("truncated", &bytes).unwrap_err(),
            "PS-X EXE payload is shorter than its header says"
        );
    }

    #[test]
    fn rejects_unrecognised_images() {
        assert_eq!(
            check("invalid", b"not a PS-X EXE").unwrap_err(),
            "input is not a PS-X EXE"
        );
    }

    #[test]
    fn settings_block_uses_its_own_magic_and_checked_size() {
        let mut bytes = sample(&[0x40], 0x300);
        let block = &mut bytes[HEADER_LEN + 0x180..HEADER_LEN + 0x180 + 36];
        block[..4].copy_from_slice(b"NSET");
        block[4..8].copy_from_slice(&1u32.to_le_bytes());
        block[8..12].copy_from_slice(&36u32.to_le_bytes());
        let path = env::temp_dir().join(format!("settings-symbol-{}", std::process::id()));
        fs::write(&path, &bytes).unwrap();
        assert_eq!(
            block_address(&path, b"NSET", 1, 36, "NITRO_SETTINGS_STATE").unwrap(),
            0x8001_0180
        );
        assert!(block_address(&path, b"NSET", 2, 36, "NITRO_SETTINGS_STATE").is_err());
        assert!(block_address(&path, b"NSET", 1, 40, "NITRO_SETTINGS_STATE").is_err());
        fs::remove_file(path).unwrap();
    }
}
