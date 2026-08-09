// SPDX-License-Identifier: GPL-2.0-or-later
//! Cook NitroXide's complete 4bpp arena atlas into one PSoXide PSXT asset.

use image::GenericImageView;
use psx_asset::Texture;
use psxed_format::{texture::TextureHeader, AssetHeader};
use psxed_tex::{encode_indexed_psxt_with_clut_rows, quantize_rgb, PsxtDepth};
use std::path::Path;

const GRASS_W: usize = 64;
const TEX_W: usize = 224;
const TEX_H: usize = 132;
const COVER_U0: usize = 96;
const COVER_H: usize = 84;
const NET_U0: usize = COVER_U0;
const NET_V0: usize = COVER_H;
const NET_W: usize = 96;
const NET_H: usize = 48;
const HEX_W: i32 = 8;
const HEX_H: i32 = 7;
const NET_CELL: usize = 8;
const CLUT_ENTRIES: usize = 16;
const COVER_CLUT_ROW: usize = 2;

const ARENA_PALETTE: [[u8; 3]; CLUT_ENTRIES] = [
    [44, 92, 58],
    [38, 80, 50],
    [52, 104, 66],
    [34, 72, 46],
    [60, 116, 74],
    [30, 64, 42],
    [74, 82, 108],
    [58, 64, 88],
    [88, 96, 124],
    [46, 52, 72],
    [104, 112, 142],
    [38, 44, 62],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
];

const COVER_PALETTE: [[u8; 3]; CLUT_ENTRIES] = [
    [0, 0, 0],
    [232, 236, 244],
    [116, 148, 196],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
    [0, 0, 0],
];

fn main() {
    let mut args = std::env::args_os().skip(1);
    let grass_path = args.next().unwrap_or_else(|| usage());
    let output_path = args.next().unwrap_or_else(|| usage());
    if args.next().is_some() {
        usage();
    }

    let grass_bytes = std::fs::read(&grass_path).expect("read grass source");
    let image = image::load_from_memory(&grass_bytes).expect("decode grass source");
    assert_eq!(
        image.dimensions(),
        (GRASS_W as u32, GRASS_W as u32),
        "grass source must be 64x64"
    );
    let grass_pixels: Vec<[u8; 3]> = image
        .to_rgb8()
        .pixels()
        .map(|pixel| [pixel[0], pixel[1], pixel[2]])
        .collect();
    let psxt = cook(&grass_pixels);

    let output_path = Path::new(&output_path);
    if let Some(parent) = output_path.parent() {
        std::fs::create_dir_all(parent).expect("create arena asset directory");
    }
    std::fs::write(output_path, &psxt).expect("write arena PSXT");
    println!(
        "ARENA {}: {} bytes, {}x{} 4bpp, 3 CLUT rows",
        output_path.display(),
        psxt.len(),
        TEX_W,
        TEX_H
    );
}

fn usage() -> ! {
    eprintln!("usage: cook-arena <grass-64x64.bmp> <chunk_1.psxt>");
    std::process::exit(2);
}

fn cook(grass_pixels: &[[u8; 3]]) -> Vec<u8> {
    assert_eq!(grass_pixels.len(), GRASS_W * GRASS_W);
    let (grass_palette, grass_indices) =
        quantize_rgb(grass_pixels, CLUT_ENTRIES).expect("quantize grass");
    assert_eq!(grass_palette.len(), CLUT_ENTRIES);

    let mut indices = vec![0u8; TEX_W * TEX_H];
    for y in 0..TEX_H {
        for x in 0..TEX_W {
            indices[y * TEX_W + x] = if x < GRASS_W && y < GRASS_W {
                grass_indices[y * GRASS_W + x]
            } else if x >= COVER_U0 && y < COVER_H {
                honeycomb_index((x - COVER_U0) as i32, y as i32)
            } else if (NET_U0..NET_U0 + NET_W).contains(&x) && (NET_V0..NET_V0 + NET_H).contains(&y)
            {
                let gx = (x - NET_U0) % NET_CELL;
                let gy = (y - NET_V0) % NET_CELL;
                if gx == 0 || gy == 0 {
                    1
                } else if gx == 1 || gy == 1 {
                    2
                } else {
                    0
                }
            } else if x >= COVER_U0 {
                0
            } else if x >= GRASS_W && y < GRASS_W {
                wall_index(x - GRASS_W, y)
            } else {
                0
            };
        }
    }

    let palette_rows = vec![
        ARENA_PALETTE.to_vec(),
        grass_palette,
        COVER_PALETTE.to_vec(),
    ];
    let mut blob = encode_indexed_psxt_with_clut_rows(
        TEX_W as u16,
        TEX_H as u16,
        PsxtDepth::Bit4,
        &indices,
        &palette_rows,
        false,
    )
    .expect("encode arena PSXT");

    // PS1 semi-transparency is selected twice: the primitive sets ABE and each
    // visible CLUT entry sets STP. PSXT stores raw RGB555+M halfwords, while the
    // generic RGB cooker deliberately leaves M clear, so stamp it on the two
    // net strand colours after the common encoder has built the blob.
    set_clut_mask_bit(&mut blob, COVER_CLUT_ROW, 1);
    set_clut_mask_bit(&mut blob, COVER_CLUT_ROW, 2);
    validate(&blob);
    blob
}

fn honeycomb_index(px: i32, py: i32) -> u8 {
    let mut d1 = i32::MAX;
    let mut d2 = i32::MAX;
    let row = py.div_euclid(HEX_H);
    for j in row - 1..=row + 1 {
        let shift = (HEX_W / 2) * (j & 1);
        let col = (px - shift).div_euclid(HEX_W);
        for i in col - 1..=col + 1 {
            let dx = px - (HEX_W * i + (HEX_W / 2) * (j & 1));
            let dy = py - HEX_H * j;
            let d = isqrt(dx * dx + dy * dy);
            if d < d1 {
                d2 = d1;
                d1 = d;
            } else if d < d2 {
                d2 = d;
            }
        }
    }
    match d2 - d1 {
        0 => 1,
        1 => 2,
        _ => 0,
    }
}

fn wall_index(px: usize, py: usize) -> u8 {
    let edge = px == 0 || py == 0;
    let inner = px == 1 || py == 1;
    let bolt = (27..=29).contains(&px) && (27..=29).contains(&py);
    if edge {
        10
    } else if inner {
        8
    } else if bolt {
        10
    } else if (px + py) & 7 == 0 {
        7
    } else if ((px * py) >> 6) & 1 == 0 {
        9
    } else {
        11
    }
}

fn isqrt(n: i32) -> i32 {
    let mut bit = 1u32 << 30;
    let mut remainder = n as u32;
    let mut root = 0u32;
    while bit > remainder {
        bit >>= 2;
    }
    while bit != 0 {
        if remainder >= root + bit {
            remainder -= root + bit;
            root = (root >> 1) + bit;
        } else {
            root >>= 1;
        }
        bit >>= 2;
    }
    root as i32
}

fn set_clut_mask_bit(blob: &mut [u8], row: usize, entry: usize) {
    let pixel_bytes_offset = AssetHeader::SIZE + 8;
    let pixel_bytes = u32::from_le_bytes(
        blob[pixel_bytes_offset..pixel_bytes_offset + 4]
            .try_into()
            .expect("pixel byte field"),
    ) as usize;
    let clut_start = AssetHeader::SIZE + TextureHeader::SIZE + pixel_bytes;
    let offset = clut_start + (row * CLUT_ENTRIES + entry) * 2;
    let value = u16::from_le_bytes([blob[offset], blob[offset + 1]]) | 0x8000;
    blob[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn validate(blob: &[u8]) {
    let texture = Texture::from_bytes(blob).expect("parse cooked arena PSXT");
    assert_eq!(texture.width(), TEX_W as u16);
    assert_eq!(texture.height(), TEX_H as u16);
    assert_eq!(texture.halfwords_per_row(), (TEX_W / 4) as u16);
    assert_eq!(texture.clut_entries(), (3 * CLUT_ENTRIES) as u16);
    let clut = texture.clut_bytes();
    for entry in [1usize, 2] {
        let offset = (COVER_CLUT_ROW * CLUT_ENTRIES + entry) * 2;
        let value = u16::from_le_bytes([clut[offset], clut[offset + 1]]);
        assert_ne!(value & 0x8000, 0, "cover strand must carry STP");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cooked_atlas_has_the_runtime_contract() {
        let mut pixels = Vec::with_capacity(GRASS_W * GRASS_W);
        for y in 0..GRASS_W {
            for x in 0..GRASS_W {
                let shade = ((x / 4 + y / 4) & 15) as u8;
                pixels.push([32 + shade * 6, 72 + shade * 7, shade]);
            }
        }
        let blob = cook(&pixels);
        validate(&blob);
    }
}
