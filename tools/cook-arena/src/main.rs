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
const BALL_CLUT_ROW: usize = 4;
const SPENT_CLUT_ROW: usize = 5;
const GLOW_CLUT_ROW: usize = 6;
const RING_CLUT_ROW: usize = 7;
const CLUT_ROWS: usize = 8;
/// A 32x32 radial glow below the goal net: index 15 at the centre falling to
/// 1 at the rim, 0 (a hole) outside it. The goal halos, the goal-line strip
/// and the ball's landing disc sample it through the glow palette, and the
/// ball's ground hoop samples its centre texel.
const GLOW_U0: usize = 0;
const GLOW_V0: usize = NET_V0 + NET_H;
const GLOW_W: usize = 32;
const CHALK_INDEX: u8 = 15;
/// The ball's texture (draw.rs `Builder::ball`), in the base page's free
/// space below the glow tile, where the unused end-of-pitch marking tiles
/// were. Laid out the way the ball's 16 x 5 facets sample it: eight texels a
/// column of facets, and the latitude rows at `BALL_ROW_V`. Drawn through the
/// ball palette, CLUT row 4.
const BALL_U0: usize = 0;
const BALL_V0: usize = 144;
const BALL_TEX_W: usize = 128;
const BALL_TEX_H: usize = 64;
const BALL_LON: usize = 16;
const BALL_LAT: usize = 5;
/// V at each latitude row of the ball mesh, pole to pole (draw.rs keeps the
/// same table).
const BALL_ROW_V: [f64; BALL_LAT + 1] = [0.0, 13.0, 26.0, 38.0, 51.0, 64.0];
/// The crowd behind the enclosure (draw.rs `Builder::stands`): tiers of
/// fans below the honeycomb's rows, drawn through one of two per-frame
/// palettes whose entries 12..=14 are the team's colour and 15 the lit fascia
/// along the front tier. Four texel rows a tier: heads, two of shirts, the
/// step behind them. No entry is black, so nothing in it is a hole.
const CROWD_U0: usize = 128;
const CROWD_V0: usize = 88;
const CROWD_W: usize = 128;
const CROWD_H: usize = 40;
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

/// The ball's sixteen colours: the palette of the approved generated design
/// (work/nitro-review-2026-10-01/ballgen, reduced to 16 colours), with its
/// one unused entry turned into the deepest seam. Mid-grey on purpose: the
/// vertex tint runs past 128 on the lit side. Opaque, and no entry is black,
/// so nothing in the ball is a hole.
const BALL_PALETTE: [[u8; 3]; CLUT_ENTRIES] = [
    [50, 50, 53],
    [63, 62, 64],
    [73, 76, 78],
    [130, 70, 30],
    [81, 83, 87],
    [89, 92, 96],
    [98, 102, 106],
    [210, 101, 8],
    [108, 112, 115],
    [30, 30, 33],
    [110, 115, 117],
    [121, 125, 130],
    [131, 136, 141],
    [143, 147, 153],
    [247, 204, 8],
    [155, 159, 166],
];

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

    let ball_map = ball_texture();
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
            } else if (BALL_U0..BALL_U0 + BALL_TEX_W).contains(&x)
                && (BALL_V0..BALL_V0 + BALL_TEX_H).contains(&y)
            {
                ball_map[(y - BALL_V0) * BALL_TEX_W + (x - BALL_U0)]
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
        BALL_PALETTE.to_vec(),
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

/// The ball texture, baked through the ball's own mesh so its panels come
/// out evenly sized on screen.
///
/// The approved design was drawn as a latitude/longitude map, and a
/// lat/long map squeezes panels together toward the poles and spreads them
/// at the equator; worse, the ball's polar facets are triangles that sample
/// only half of their map cell. So the layout is not painted in map space at
/// all. Each texel asks which ball facet draws it and where, maps that point
/// onto the sphere, and takes the colour of the panel there. The panels are
/// a football's: a truncated icosahedron's twelve pentagons and twenty
/// hexagons, projected onto the sphere. The look (plate greys, bevels, dark
/// seams, an amber light in every pentagon) and the palette are the design's.
fn ball_texture() -> Vec<u8> {
    let faces = ball_faces();
    let mut out = vec![0u8; BALL_TEX_W * BALL_TEX_H];
    for py in 0..BALL_TEX_H {
        for px in 0..BALL_TEX_W {
            let d = ball_texel_direction(px as f64 + 0.5, py as f64 + 0.5);
            out[py * BALL_TEX_W + px] = ball_style(&faces, d);
        }
    }
    out
}

/// Ball mesh vertex (`i` round, `j` from the -Y pole) on the unit sphere,
/// the same formula as draw.rs `build_meshes`.
fn ball_vertex(i: usize, j: usize) -> [f64; 3] {
    use std::f64::consts::PI;
    let lat = -PI / 2.0 + PI * j as f64 / BALL_LAT as f64;
    let lon = 2.0 * PI * i as f64 / BALL_LON as f64;
    [lon.sin() * lat.cos(), lat.sin(), lon.cos() * lat.cos()]
}

fn unit(p: [f64; 3]) -> [f64; 3] {
    let l = (p[0] * p[0] + p[1] * p[1] + p[2] * p[2]).sqrt();
    [p[0] / l, p[1] / l, p[2] / l]
}

fn dot3(a: [f64; 3], b: [f64; 3]) -> f64 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// Barycentric weights of `q` in the UV triangle `p0 p1 p2`.
fn bary(p0: [f64; 2], p1: [f64; 2], p2: [f64; 2], q: [f64; 2]) -> [f64; 3] {
    let den = (p1[1] - p2[1]) * (p0[0] - p2[0]) + (p2[0] - p1[0]) * (p0[1] - p2[1]);
    let w0 = ((p1[1] - p2[1]) * (q[0] - p2[0]) + (p2[0] - p1[0]) * (q[1] - p2[1])) / den;
    let w1 = ((p2[1] - p0[1]) * (q[0] - p2[0]) + (p0[0] - p2[0]) * (q[1] - p2[1])) / den;
    [w0, w1, 1.0 - w0 - w1]
}

/// The point on the ball where its facets draw map texel (`u`, `v`).
///
/// Facet column `i` covers U 8i..8i+8 and latitude band `j` covers V
/// `BALL_ROW_V[j]..BALL_ROW_V[j + 1]`. Its corners are a b c d = (i, j)
/// (i+1, j) (i, j+1) (i+1, j+1), and a pole's two corners take the column's
/// middle U. The GPU draws the quad as triangles (a, b, c) and (b, c, d) with
/// texture coordinates interpolated affinely across each, which is what this
/// inverts; at a pole one of the two has no area. A texel the facet never
/// reaches (half of each polar cell) extrapolates from the nearest triangle.
fn ball_texel_direction(u: f64, v: f64) -> [f64; 3] {
    let cell = (BALL_TEX_W / BALL_LON) as f64;
    let i = ((u / cell) as usize).min(BALL_LON - 1);
    let j = (0..BALL_LAT).rev().find(|&j| v >= BALL_ROW_V[j]).unwrap_or(0);
    let (u0, u1) = (i as f64 * cell, (i + 1) as f64 * cell);
    let corner = |di: usize, dj: usize| {
        let row = j + dj;
        let pole = row == 0 || row == BALL_LAT;
        let cu = if pole { (u0 + u1) / 2.0 } else if di == 0 { u0 } else { u1 };
        ([cu, BALL_ROW_V[row]], ball_vertex((i + di) % BALL_LON, row))
    };
    let (a, b, c, d) = (corner(0, 0), corner(1, 0), corner(0, 1), corner(1, 1));
    let tri = if j == 0 {
        (b, c, d)
    } else if j == BALL_LAT - 1 {
        (a, b, c)
    } else if bary(a.0, b.0, c.0, [u, v]).iter().all(|&w| w >= -1e-9) {
        (a, b, c)
    } else {
        (b, c, d)
    };
    let w = bary(tri.0 .0, tri.1 .0, tri.2 .0, [u, v]);
    unit([0, 1, 2].map(|k| w[0] * tri.0 .1[k] + w[1] * tri.1 .1[k] + w[2] * tri.2 .1[k]))
}

/// One panel of the ball: its outward normal, its distance from the centre
/// on a truncated icosahedron of unit edge, and its tone.
struct BallFace {
    n: [f64; 3],
    h: f64,
    pentagon: bool,
    tone: u8,
}

/// The truncated icosahedron's 32 faces: pentagons on the icosahedron's
/// vertices, hexagons on its faces. The hexagons take three plate greys,
/// coloured greedily so that no two neighbours match.
fn ball_faces() -> Vec<BallFace> {
    let phi = (1.0 + 5f64.sqrt()) / 2.0;
    let mut v = Vec::new();
    for a in [-1.0, 1.0] {
        for b in [-phi, phi] {
            v.push([0.0, a, b]);
            v.push([a, b, 0.0]);
            v.push([b, 0.0, a]);
        }
    }
    // Inradius of each face of a truncated icosahedron with unit edges.
    const H_PENT: f64 = 2.327_438_436;
    const H_HEX: f64 = 2.267_283_942;
    let mut faces: Vec<BallFace> = v
        .iter()
        .map(|&p| BallFace { n: unit(p), h: H_PENT, pentagon: true, tone: 1 })
        .collect();
    let edge = |p: &[f64; 3], q: &[f64; 3]| {
        let d = (p[0] - q[0]).powi(2) + (p[1] - q[1]).powi(2) + (p[2] - q[2]).powi(2);
        (d - 4.0).abs() < 1e-6
    };
    for i in 0..12 {
        for j in i + 1..12 {
            for k in j + 1..12 {
                if edge(&v[i], &v[j]) && edge(&v[j], &v[k]) && edge(&v[i], &v[k]) {
                    let n = unit([0, 1, 2].map(|c| v[i][c] + v[j][c] + v[k][c]));
                    faces.push(BallFace { n, h: H_HEX, pentagon: false, tone: 0 });
                }
            }
        }
    }
    assert_eq!(faces.len(), 32, "a truncated icosahedron has 32 faces");
    // Neighbouring hexagons' normals are 41.8 degrees apart; the next
    // nearest are 70.5.
    const TONES: [u8; 3] = [12, 10, 2];
    for f in 12..32 {
        let used: Vec<u8> = (12..f)
            .filter(|&g| dot3(faces[f].n, faces[g].n) > 0.6)
            .map(|g| faces[g].tone)
            .collect();
        faces[f].tone = *TONES.iter().find(|t| !used.contains(t)).unwrap_or(&TONES[0]);
    }
    faces
}

/// The palette index for the ball at direction `d`.
fn ball_style(faces: &[BallFace], d: [f64; 3]) -> u8 {
    // The face a ray from the centre meets first is the one with the
    // largest `d . n / h`.
    let mut best = (f64::MIN, 0usize);
    let mut second = (f64::MIN, 0usize);
    for (k, f) in faces.iter().enumerate() {
        let s = dot3(d, f.n) / f.h;
        if s > best.0 {
            second = best;
            best = (s, k);
        } else if s > second.0 {
            second = (s, k);
        }
    }
    let (f1, f2) = (&faces[best.1], &faces[second.1]);
    // Degrees to the seam: the score gap over its rate of change across the
    // boundary.
    let g = [0, 1, 2].map(|c| f1.n[c] / f1.h - f2.n[c] / f2.h);
    let rate = dot3(g, g).sqrt();
    let seam = ((best.0 - second.0) / rate).to_degrees();
    let from_centre = dot3(d, f1.n).clamp(-1.0, 1.0).acos().to_degrees();
    if seam < 0.9 {
        9
    } else if seam < 2.0 {
        0
    } else if f1.pentagon {
        if from_centre < 3.2 {
            14
        } else if from_centre < 5.4 {
            7
        } else if from_centre < 6.6 {
            3
        } else if seam < 3.6 {
            6
        } else {
            1
        }
    } else if seam < 3.6 {
        15
    } else {
        f1.tone
    }
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
    let offset = (GLOW_CLUT_ROW * CLUT_ENTRIES + 15) * 2;
    let value = u16::from_le_bytes([clut[offset], clut[offset + 1]]);
    assert_ne!(value & 0x8000, 0, "glow rings must carry STP");
    for entry in 0..CLUT_ENTRIES {
        let offset = (BALL_CLUT_ROW * CLUT_ENTRIES + entry) * 2;
        let value = u16::from_le_bytes([clut[offset], clut[offset + 1]]);
        assert_eq!(value & 0x8000, 0, "the ball is opaque");
        assert_ne!(value, 0, "no ball colour may be a hole");
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

    /// Whether a facet draws map point (`u`, `v`): everywhere but the half
    /// of each polar cell outside its one triangle.
    fn ball_texel_drawn(u: f64, v: f64) -> bool {
        let cell = (BALL_TEX_W / BALL_LON) as f64;
        let (u0, um) = ((u / cell).floor() * cell, (u / cell).floor() * cell + cell / 2.0);
        let polar = if v < BALL_ROW_V[1] {
            Some((BALL_ROW_V[0], BALL_ROW_V[1]))
        } else if v >= BALL_ROW_V[BALL_LAT - 1] {
            Some((BALL_ROW_V[BALL_LAT], BALL_ROW_V[BALL_LAT - 1]))
        } else {
            None
        };
        match polar {
            None => true,
            Some((pole_v, row_v)) => {
                let w = bary([um, pole_v], [u0, row_v], [u0 + cell, row_v], [u, v]);
                w.iter().all(|&x| x >= 0.0)
            }
        }
    }

    #[test]
    fn ball_texels_land_on_the_facet_that_draws_them() {
        // A non-polar facet corner maps to its own mesh vertex.
        let d = ball_texel_direction(8.0 * 3.0 + 1e-6, BALL_ROW_V[2] + 1e-6);
        let v = ball_vertex(3, 2);
        assert!(dot3(d, v) > 0.9999, "{d:?} vs {v:?}");
        // The middle of a polar cell's top edge is the pole itself.
        let pole = ball_texel_direction(8.0 * 5.0 + 4.0, 1e-6);
        assert!(pole[1] < -0.9999, "{pole:?}");
    }

    #[test]
    fn ball_panels_are_evenly_sized_on_the_sphere() {
        // Every panel is drawn, and each hexagon covers about the same share
        // of the sphere: count texels by panel, weighted by the solid angle
        // each texel covers on the ball.
        let faces = ball_faces();
        let mut area = vec![0.0f64; faces.len()];
        let step = 0.25;
        let mut u = step / 2.0;
        while u < BALL_TEX_W as f64 {
            let mut v = step / 2.0;
            while v < BALL_TEX_H as f64 {
                let (c, du, dv) = (
                    ball_texel_direction(u, v),
                    ball_texel_direction(u + 0.01, v),
                    ball_texel_direction(u, v + 0.01),
                );
                let e1 = [0, 1, 2].map(|k| (du[k] - c[k]) / 0.01);
                let e2 = [0, 1, 2].map(|k| (dv[k] - c[k]) / 0.01);
                let cross = [
                    e1[1] * e2[2] - e1[2] * e2[1],
                    e1[2] * e2[0] - e1[0] * e2[2],
                    e1[0] * e2[1] - e1[1] * e2[0],
                ];
                // Only texels a facet actually draws.
                let inside = ball_texel_drawn(u, v);
                if inside {
                    let mut best = (f64::MIN, 0);
                    for (k, f) in faces.iter().enumerate() {
                        let s = dot3(c, f.n) / f.h;
                        if s > best.0 {
                            best = (s, k);
                        }
                    }
                    area[best.1] += dot3(cross, cross).sqrt() * step * step;
                }
                v += step;
            }
            u += step;
        }
        let total: f64 = area.iter().sum();
        assert!((total - 4.0 * std::f64::consts::PI).abs() < 0.5, "sphere area {total}");
        let hex: Vec<f64> = area[12..].to_vec();
        let (lo, hi) = hex.iter().fold((f64::MAX, 0f64), |(a, b), &x| (a.min(x), b.max(x)));
        assert!(hi / lo < 1.35, "hexagon areas range {lo}..{hi}");
        let map = ball_texture();
        assert!(map.contains(&14) && map.contains(&9) && map.contains(&12));
    }
}
