// SPDX-License-Identifier: GPL-2.0-or-later
//! Cook NitroXide's complete 4bpp arena atlas into one PSoXide PSXT asset.

use image::GenericImageView;
use psx_asset::Texture;
use psxed_format::{texture::TextureHeader, AssetHeader};
use psxed_tex::{encode_indexed_psxt_with_clut_rows, quantize_rgb, PsxtDepth};
use std::path::Path;

const GRASS_W: usize = 64;
const TEX_W: usize = 256;
const TEX_H: usize = 256 * 3;
const COVER_U0: usize = 128;
const COVER_H: usize = 84;
const NET_U0: usize = 0;
const NET_V0: usize = GRASS_W;
const NET_W: usize = 96;
const NET_H: usize = 48;
const MARKED_U0: usize = 0;
const MARKED_V0: usize = 256;
const MARKED_PAGE_H: usize = 256;
const MARKED_TILE_W: usize = 64;
const MARKED_TILES_PER_PAGE: usize = 4;
const MARKED_FIRST_Z: i32 = 3;
const PITCH_HALF_X: i32 = 4096;
const PITCH_HALF_Z: i32 = 5120;
const PITCH_TILE_UU: i32 = 1024;
const HALFWAY_HALF_W: i32 = 40;
const HALFWAY_END_INSET: i32 = 300;
const CIRCLE_R_IN: i32 = 1092;
const CIRCLE_R_OUT: i32 = 1152;
const HEX_W: i32 = 8;
const HEX_H: i32 = 7;
const NET_CELL: usize = 8;
const CLUT_ENTRIES: usize = 16;
const COVER_CLUT_ROW: usize = 2;
const MARKED_CLUT_ROW: usize = 3;
const PAD_CLUT_ROW: usize = 4;
const SPENT_CLUT_ROW: usize = 5;
const GLOW_CLUT_ROW: usize = 6;
const RING_CLUT_ROW: usize = 7;
const CLUT_ROWS: usize = 8;
/// A 32x32 radial glow below the goal net: index 15 at the centre falling to
/// 1 at the rim, 0 (a hole) outside it. Every light in the arena that is not
/// baked into a vertex tint (boost pads, orbs, halos, goal glow, the ball's
/// ground ring) is this one tile drawn through one of three palettes.
const GLOW_U0: usize = 0;
const GLOW_V0: usize = NET_V0 + NET_H;
const GLOW_W: usize = 32;
const CHALK_INDEX: u8 = 15;
/// The markings at each end: a goal box, a larger box and the arc on its
/// front edge, the way Mannfield's pitch reads from the chase camera. Both
/// ends and both halves of each end are mirror images, and the floor draws
/// them from six unique tiles by flipping UVs, so they fit in the base page's
/// free space instead of two more 64 KB marked pages.
const END_TILE_W: usize = 64;
/// Base-page texel origin of each unique end tile, indexed `row * 3 + col - 1`
/// for pitch columns 1..=3 of rows 0..=1 at the -Z end.
const END_TILE_ORIGINS: [(usize, usize); 6] = [
    (128, 128),
    (192, 128),
    (0, 144),
    (128, 192),
    (192, 192),
    (64, 144),
];
/// The crowd behind the enclosure (draw.rs `Builder::stands`): tiers of
/// fans below the honeycomb's rows, drawn through one of two per-frame
/// palettes whose entries 12..=14 are the team's colour and 15 the lit fascia
/// along the front tier. Four texel rows a tier: heads, two of shirts, the
/// step behind them. No entry is black, so nothing in it is a hole.
const CROWD_U0: usize = 128;
const CROWD_V0: usize = 88;
const CROWD_W: usize = 128;
const CROWD_H: usize = 40;
const GOAL_BOX_HALF_W: i32 = 1300;
const GOAL_BOX_DEPTH: i32 = 700;
const BIG_BOX_HALF_W: i32 = 2300;
const BIG_BOX_DEPTH: i32 = 1650;
const ARC_CENTRE: i32 = 1100;
const ARC_R: i32 = 860;
const END_LINE_HALF_W: i32 = 40;
/// The floor's own geometry, which the texels have to land on: the pitch
/// stops a ramp radius in from the walls except across a goal mouth, and the
/// corners are chamfered (draw.rs `Builder::chamfer`).
const GOAL_HALF_W: i32 = 893;
const RAMP_R: i32 = 260;
const CORNER: i32 = 8064;
const FLOOR_CELL_UU: i32 = 256;
const CHALK: [u8; 3] = [205, 220, 210];

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

/// Brightness of each ring of the glow tile, centre last. Greyscale: the
/// vertex tint gives it its colour. Drawn additively, so this is light added
/// to whatever is behind it and entry 0 is the hole around the disc.
fn glow_palette() -> Vec<[u8; 3]> {
    (0..CLUT_ENTRIES)
        .map(|i| {
            let t = i as i32 * 255 / 15;
            let v = (t * t / 255) as u8;
            [v, v, v]
        })
        .collect()
}

/// A live boost pad at rest: a bright rim a ring in from the edge, a dimmer
/// bowl inside it and a hot centre. The game rewrites this row every frame to
/// run a ripple outward through it (`draw::pad_clut`); this is the frame the
/// atlas starts with.
fn pad_palette() -> Vec<[u8; 3]> {
    const RINGS: [u8; CLUT_ENTRIES] = [
        0, 110, 230, 190, 90, 60, 52, 52, 60, 76, 96, 120, 146, 172, 200, 224,
    ];
    RINGS.iter().map(|&v| [v, v, v]).collect()
}

/// A spent pad: an opaque dark kerb and a darker floor, the plate the pad
/// sits on while it is recharging. No STP bits, so it draws solid even
/// through the additive packet the live pad uses.
fn spent_palette() -> Vec<[u8; 3]> {
    (0..CLUT_ENTRIES)
        .map(|i| match i {
            0 => [0, 0, 0],
            1..=3 => [72, 76, 92],
            _ => [44, 48, 62],
        })
        .collect()
}

/// The ball's ground ring: only the outer rings of the glow tile are lit,
/// the rest are holes, so one quad draws a hoop.
fn ring_palette() -> Vec<[u8; 3]> {
    (0..CLUT_ENTRIES)
        .map(|i| match i {
            1 => [170, 170, 170],
            2 | 3 => [255, 255, 255],
            4 => [110, 110, 110],
            _ => [0, 0, 0],
        })
        .collect()
}

fn glow_index(px: usize, py: usize) -> u8 {
    let (dx, dy) = (px as i32 * 2 + 1 - GLOW_W as i32, py as i32 * 2 + 1 - GLOW_W as i32);
    let r2 = dx * dx + dy * dy;
    let rim = GLOW_W as i32;
    if r2 >= rim * rim {
        return 0;
    }
    (15 - isqrt(r2) * 15 / rim).clamp(1, 15) as u8
}

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
        "ARENA {}: {} bytes, {}x{} 4bpp, {} CLUT rows",
        output_path.display(),
        psxt.len(),
        TEX_W,
        TEX_H,
        CLUT_ROWS
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
    let (mut marked_palette, marked_grass_indices) =
        quantize_rgb(grass_pixels, CLUT_ENTRIES - 1).expect("quantize marked grass");
    marked_palette.push(CHALK);
    assert_eq!(marked_palette.len(), CLUT_ENTRIES);

    let mut indices = vec![0u8; TEX_W * TEX_H];
    for y in 0..TEX_H {
        for x in 0..TEX_W {
            indices[y * TEX_W + x] = if x < GRASS_W && y < GRASS_W {
                grass_indices[y * GRASS_W + x]
            } else if y >= MARKED_V0 {
                marked_pitch_index(x, y, &marked_grass_indices)
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
            } else if (GLOW_U0..GLOW_U0 + GLOW_W).contains(&x)
                && (GLOW_V0..GLOW_V0 + GLOW_W).contains(&y)
            {
                glow_index(x - GLOW_U0, y - GLOW_V0)
            } else if (CROWD_U0..CROWD_U0 + CROWD_W).contains(&x)
                && (CROWD_V0..CROWD_V0 + CROWD_H).contains(&y)
            {
                crowd_index(x - CROWD_U0, y - CROWD_V0)
            } else if let Some(index) = end_marked_index(x, y, &marked_grass_indices) {
                index
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
        marked_palette,
        pad_palette(),
        spent_palette(),
        glow_palette(),
        ring_palette(),
    ];
    assert_eq!(palette_rows.len(), CLUT_ROWS);
    let mut blob = encode_indexed_psxt_with_clut_rows(
        TEX_W as u16,
        TEX_H as u16,
        PsxtDepth::Bit4,
        &indices,
        &palette_rows,
        // Preserve the cover row's black index zero as a hole. Other rows
        // have a nonblack entry zero, which the encoder preserves unchanged.
        true,
    )
    .expect("encode arena PSXT");

    // PS1 semi-transparency is selected twice: the primitive sets ABE and each
    // visible CLUT entry sets STP. PSXT stores raw RGB555+M halfwords, while the
    // generic RGB cooker deliberately leaves M clear, so stamp it on the two
    // net strand colours after the common encoder has built the blob.
    set_clut_mask_bit(&mut blob, COVER_CLUT_ROW, 1);
    set_clut_mask_bit(&mut blob, COVER_CLUT_ROW, 2);
    // The glow rows are drawn additively, and a texel only blends if its
    // CLUT entry carries STP: every visible ring gets it.
    for entry in 1..CLUT_ENTRIES {
        set_clut_mask_bit(&mut blob, PAD_CLUT_ROW, entry);
        set_clut_mask_bit(&mut blob, GLOW_CLUT_ROW, entry);
    }
    for entry in 1..=4 {
        set_clut_mask_bit(&mut blob, RING_CLUT_ROW, entry);
    }
    validate(&blob);
    blob
}

/// One texel of the two full-resolution marked-pitch pages. Each page holds
/// four columns by four rows of 64x64 grass tiles; the second page continues
/// at pitch column four.
fn marked_pitch_index(px: usize, py: usize, grass: &[u8]) -> u8 {
    let atlas_x = px - MARKED_U0;
    let atlas_z = py - MARKED_V0;
    let page = atlas_z / MARKED_PAGE_H;
    let page_z = atlas_z % MARKED_PAGE_H;
    let tile_x = page * MARKED_TILES_PER_PAGE + atlas_x / MARKED_TILE_W;
    let tile_z = page_z / MARKED_TILE_W;
    let local_x = atlas_x % MARKED_TILE_W;
    let local_z = page_z % MARKED_TILE_W;
    let grass_index = grass[local_z * GRASS_W + local_x];

    let world_x = -PITCH_HALF_X
        + tile_x as i32 * PITCH_TILE_UU
        + local_x as i32 * PITCH_TILE_UU / MARKED_TILE_W as i32
        + PITCH_TILE_UU / MARKED_TILE_W as i32 / 2;
    let world_z = -PITCH_HALF_Z
        + (MARKED_FIRST_Z + tile_z as i32) * PITCH_TILE_UU
        + local_z as i32 * PITCH_TILE_UU / MARKED_TILE_W as i32
        + PITCH_TILE_UU / MARKED_TILE_W as i32 / 2;

    let halfway =
        world_z.abs() <= HALFWAY_HALF_W && world_x.abs() <= PITCH_HALF_X - HALFWAY_END_INSET;
    let radius = isqrt(world_x * world_x + world_z * world_z);
    let circle = (CIRCLE_R_IN..=CIRCLE_R_OUT).contains(&radius);
    if halfway || circle {
        CHALK_INDEX
    } else {
        grass_index
    }
}

/// A texel of the crowd tile. A fixed hash picks each seat's fan, so the cook
/// is reproducible.
fn crowd_index(px: usize, py: usize) -> u8 {
    if py >= CROWD_H - 2 {
        return 15;
    }
    let (tier, sub) = (py / 4, py % 4);
    let seat = px / 2;
    let hash = |salt: usize| {
        let mut h = (seat as u32 ^ (tier as u32) << 8 ^ (salt as u32) << 16).wrapping_mul(0x9E37_79B9);
        h ^= h >> 15;
        h = h.wrapping_mul(0x85EB_CA6B);
        (h ^ (h >> 13)) % 100
    };
    let empty = hash(1) < 12;
    match sub {
        3 => 1,
        _ if empty => [0, 1, 0][sub],
        // The two texels of a seat's head differ a little: a face and hair.
        0 => [4, 5, 3][(hash(2) as usize + px % 2) % 3],
        _ if hash(3) < 48 => 12 + (hash(4) % 3) as u8,
        _ => [2, 6, 7, 8, 9, 10, 11, 3][(hash(5) % 8) as usize],
    }
}

/// A texel of one of the six unique end tiles, or `None` outside them.
fn end_marked_index(px: usize, py: usize, grass: &[u8]) -> Option<u8> {
    let (slot, &(u0, v0)) = END_TILE_ORIGINS.iter().enumerate().find(|(_, &(u0, v0))| {
        (u0..u0 + END_TILE_W).contains(&px) && (v0..v0 + END_TILE_W).contains(&py)
    })?;
    let (lx, lz) = (px - u0, py - v0);
    let (col, row) = (slot % 3 + 1, slot / 3);
    let (x, z) = end_texel_world(col as i32, row as i32, lx as i32, lz as i32);
    Some(if end_chalk(x, z + PITCH_HALF_Z) {
        CHALK_INDEX
    } else {
        grass[lz * GRASS_W + lx]
    })
}

/// Where the floor draws a texel of tile (`col`, `row`): the renderer
/// splits a near tile into 256-uu cells whose corners it pulls onto the
/// chamfered pitch outline and maps the texture across each cell affinely,
/// so interpolate the texel's position between its cell's pulled corners.
fn end_texel_world(col: i32, row: i32, lx: i32, lz: i32) -> (i32, i32) {
    let per_cell = END_TILE_W as i32 * FLOOR_CELL_UU / PITCH_TILE_UU;
    let x0 = -PITCH_HALF_X + col * PITCH_TILE_UU + lx / per_cell * FLOOR_CELL_UU;
    let z0 = -PITCH_HALF_Z + row * PITCH_TILE_UU + lz / per_cell * FLOOR_CELL_UU;
    // Texel centre within the cell, in 1/(2 * per_cell) steps.
    let (fx, fz) = ((lx % per_cell) * 2 + 1, (lz % per_cell) * 2 + 1);
    let den = 2 * per_cell;
    let c = |dx: i32, dz: i32| floor_chamfer(x0 + dx * FLOOR_CELL_UU, z0 + dz * FLOOR_CELL_UU);
    let (a, b, cc, d) = (c(0, 0), c(1, 0), c(0, 1), c(1, 1));
    let lerp = |p: i32, q: i32, t: i32| p + (q - p) * t / den;
    let top = (lerp(a.0, b.0, fx), lerp(a.1, b.1, fx));
    let bottom = (lerp(cc.0, d.0, fx), lerp(cc.1, d.1, fx));
    (lerp(top.0, bottom.0, fz), lerp(top.1, bottom.1, fz))
}

/// draw.rs `Builder::chamfer`, point for point.
fn floor_chamfer(x: i32, z: i32) -> (i32, i32) {
    let foot_x = PITCH_HALF_X - RAMP_R;
    let foot_z = if x.abs() < GOAL_HALF_W {
        PITCH_HALF_Z
    } else {
        PITCH_HALF_Z - RAMP_R
    };
    let (x, z) = (x.clamp(-foot_x, foot_x), z.clamp(-foot_z, foot_z));
    let limit = CORNER - (RAMP_R * 5793 >> 12);
    let sum = x.abs() + z.abs();
    if sum <= limit {
        (x, z)
    } else {
        (x * limit / sum, z * limit / sum)
    }
}

/// Chalk at `x` across the pitch and `d` in from the -Z goal line.
fn end_chalk(x: i32, d: i32) -> bool {
    let ax = x.abs();
    let w = END_LINE_HALF_W;
    let rect = |half_w: i32, depth: i32| {
        let side = (ax - half_w).abs() <= w && (0..=depth + w).contains(&d);
        let front = (d - depth).abs() <= w && ax <= half_w + w;
        side || front
    };
    let r = isqrt(x * x + (d - ARC_CENTRE) * (d - ARC_CENTRE));
    let arc = (r - ARC_R).abs() <= w && d > BIG_BOX_DEPTH + w;
    rect(GOAL_BOX_HALF_W, GOAL_BOX_DEPTH) || rect(BIG_BOX_HALF_W, BIG_BOX_DEPTH) || arc
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
    assert_eq!(texture.clut_entries(), (CLUT_ROWS * CLUT_ENTRIES) as u16);
    let clut = texture.clut_bytes();
    let cover_zero = COVER_CLUT_ROW * CLUT_ENTRIES * 2;
    assert_eq!(
        u16::from_le_bytes([clut[cover_zero], clut[cover_zero + 1]]),
        0,
        "cover and net holes must remain transparent"
    );
    for row in [0usize, 1, MARKED_CLUT_ROW] {
        let offset = row * CLUT_ENTRIES * 2;
        assert_ne!(
            u16::from_le_bytes([clut[offset], clut[offset + 1]]) & 0x7fff,
            0,
            "arena and grass palette zero must retain its opaque color"
        );
    }
    for entry in [1usize, 2] {
        let offset = (COVER_CLUT_ROW * CLUT_ENTRIES + entry) * 2;
        let value = u16::from_le_bytes([clut[offset], clut[offset + 1]]);
        assert_ne!(value & 0x8000, 0, "cover strand must carry STP");
    }
    let spent = (SPENT_CLUT_ROW * CLUT_ENTRIES + 1) * 2;
    assert_eq!(
        u16::from_le_bytes([clut[spent], clut[spent + 1]]) & 0x8000,
        0,
        "a spent pad is an opaque plate"
    );
    for row in [PAD_CLUT_ROW, GLOW_CLUT_ROW] {
        let offset = (row * CLUT_ENTRIES + 15) * 2;
        let value = u16::from_le_bytes([clut[offset], clut[offset + 1]]);
        assert_ne!(value & 0x8000, 0, "glow rings must carry STP");
    }
    let chalk_offset = (MARKED_CLUT_ROW * CLUT_ENTRIES + CHALK_INDEX as usize) * 2;
    let chalk = u16::from_le_bytes([clut[chalk_offset], clut[chalk_offset + 1]]);
    assert_eq!(chalk & 0x8000, 0, "chalk must be opaque");
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

    #[test]
    fn glow_tile_is_a_disc_with_a_hole_around_it() {
        assert_eq!(glow_index(0, 0), 0);
        assert_eq!(glow_index(15, 15), 15);
        assert_eq!(glow_index(0, 15), 1);
    }

    #[test]
    fn marked_pitch_composites_paint_into_grass() {
        let grass = vec![3u8; GRASS_W * GRASS_W];
        // World (8, 8): inside the 80-uu halfway stripe.
        assert_eq!(marked_pitch_index(0, 640, &grass), CHALK_INDEX);
        // World (1144, 8): also inside the centre-circle ring.
        assert_eq!(marked_pitch_index(71, 640, &grass), CHALK_INDEX);
        // A point between the stripe and ring remains ordinary grass.
        assert_eq!(marked_pitch_index(0, 620, &grass), 3);
    }

    #[test]
    fn end_tiles_carry_the_boxes_and_arc() {
        // Every chalk texel an end needs lies in the six unique tiles:
        // columns 1..=3, rows 0..=1, and nothing reaches row 2.
        for x in -PITCH_HALF_X..0 {
            for d in 0..3 * PITCH_TILE_UU {
                if end_chalk(x, d) {
                    let (col, row) = ((x + PITCH_HALF_X) / PITCH_TILE_UU, d / PITCH_TILE_UU);
                    assert!((1..=3).contains(&col) && row <= 1, "chalk at x {x} d {d}");
                }
            }
        }
        let grass = vec![3u8; GRASS_W * GRASS_W];
        // The goal box's front line crosses tile (3, 0) at d = 700, texel 43.
        let (u0, v0) = END_TILE_ORIGINS[2];
        assert_eq!(end_marked_index(u0 + 32, v0 + 43, &grass), Some(CHALK_INDEX));
        assert_eq!(end_marked_index(u0 + 32, v0 + 30, &grass), Some(3));
        assert_eq!(end_marked_index(0, 0, &grass), None);
    }
}
