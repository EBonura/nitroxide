// SPDX-License-Identifier: GPL-2.0-or-later
//! Check a headless capture of a `diag-keys` build for two split-screen
//! regressions Manny found in the attract demo (2026-10-03):
//!
//! - holes in the pitch: the build paints the sky magenta while the camera
//!   is close to level, and then the bottom band of a view only ever shows
//!   the pitch, a wall or a car, so magenta there is sky showing through
//!   geometry that should have been drawn;
//! - the 60-face LOD drawn big: the build paints LOD cars pure green, and a
//!   green blob covering more pixels than a car at the LOD's switch size is
//!   the garbled low-detail mesh where its missing detail shows.
//!
//! Usage: `frame-check <screenshot-dir> [--ticks A..B] [--max-hole-px N]
//! [--max-lod-split N] [--max-lod-single N]`. The directory holds the
//! emulator's `--route-screenshot-dir` frames (`tick-NNNNNN.ppm`, 320x240).
//! Exit status 1 when any frame fails.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

const W: usize = 320;
const H: usize = 240;

struct Frame {
    rgb: Vec<u8>,
}

impl Frame {
    fn load(path: &Path) -> Result<Self, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        // P6, width, height, maxval, one whitespace byte, then the pixels.
        let mut fields = Vec::new();
        let mut at = 0;
        while fields.len() < 4 {
            while at < data.len() && data[at].is_ascii_whitespace() {
                at += 1;
            }
            let start = at;
            while at < data.len() && !data[at].is_ascii_whitespace() {
                at += 1;
            }
            fields.push(String::from_utf8_lossy(&data[start..at]).into_owned());
        }
        at += 1;
        if fields[0] != "P6" || fields[1] != W.to_string() || fields[2] != H.to_string() {
            return Err(format!("{}: not a {W}x{H} P6 image", path.display()));
        }
        let rgb = data.get(at..at + W * H * 3).ok_or("short image")?.to_vec();
        Ok(Self { rgb })
    }

    fn px(&self, x: usize, y: usize) -> (u8, u8, u8) {
        let i = (y * W + x) * 3;
        (self.rgb[i], self.rgb[i + 1], self.rgb[i + 2])
    }

    fn dark(&self, x: usize, y: usize) -> bool {
        let (r, g, b) = self.px(x, y);
        r.max(g).max(b) <= 40
    }

    /// A demo cut or a loading frame: nothing to check.
    fn blank(&self) -> bool {
        let dark = (0..H)
            .flat_map(|y| (0..W).map(move |x| (x, y)))
            .filter(|&(x, y)| self.dark(x, y))
            .count();
        dark * 10 > W * H * 6
    }

    /// Split screen draws a dark seam across rows 119 and 120.
    fn split(&self) -> bool {
        [119, 120]
            .iter()
            .all(|&y| (0..W).filter(|&x| self.dark(x, y)).count() * 10 >= W * 9)
    }
}

fn sky_key((r, g, b): (u8, u8, u8)) -> bool {
    r >= 160 && b >= 160 && g <= 40
}

fn lod_key((r, g, b): (u8, u8, u8)) -> bool {
    g >= 32 && r <= 16 && b <= 16
}

/// Magenta pixels in the bottom fifth of the rows `y0..y1`.
fn hole_pixels(f: &Frame, y0: usize, y1: usize) -> usize {
    let band = (y1 - y0) / 5;
    (y1 - band..y1)
        .flat_map(|y| (0..W).map(move |x| (x, y)))
        .filter(|&(x, y)| sky_key(f.px(x, y)))
        .count()
}

/// The pixel count of the biggest 4-connected green blob in rows `y0..y1`:
/// how much screen one LOD car covers. Area rather than a side, because a
/// car seen from behind is short and wide and one seen side-on is long and
/// thin, and the LOD is meant to cover few pixels either way.
fn largest_lod(f: &Frame, y0: usize, y1: usize) -> usize {
    let mut seen = vec![false; W * H];
    let mut best = 0;
    let mut stack = Vec::new();
    for y in y0..y1 {
        for x in 0..W {
            if seen[y * W + x] || !lod_key(f.px(x, y)) {
                continue;
            }
            let mut area = 0;
            seen[y * W + x] = true;
            stack.push((x, y));
            while let Some((cx, cy)) = stack.pop() {
                area += 1;
                let mut visit = |nx: usize, ny: usize| {
                    if ny >= y0 && ny < y1 && !seen[ny * W + nx] && lod_key(f.px(nx, ny)) {
                        seen[ny * W + nx] = true;
                        stack.push((nx, ny));
                    }
                };
                if cx > 0 {
                    visit(cx - 1, cy);
                }
                if cx + 1 < W {
                    visit(cx + 1, cy);
                }
                if cy > 0 {
                    visit(cx, cy - 1);
                }
                if cy + 1 < H {
                    visit(cx, cy + 1);
                }
            }
            best = best.max(area);
        }
    }
    best
}

struct Limits {
    ticks: Option<(u32, u32)>,
    hole: usize,
    lod_split: usize,
    lod_single: usize,
}

fn parse_args() -> Result<(PathBuf, Limits), String> {
    let mut args = std::env::args().skip(1);
    let dir = PathBuf::from(
        args.next()
            .ok_or("usage: frame-check <screenshot-dir> [options]")?,
    );
    // Defaults: a few dithered crack pixels are what the pitch's underdraw
    // strips allow. A split view's LOD car is at most ~28 px long, which
    // covered 106 px at worst over the attract demo once fixed, against
    // ~250 px for the old own-car LOD under the chase camera. A full view
    // keeps the opponent's LOD down to 56 px long, so it gets more room.
    let mut limits = Limits {
        ticks: None,
        hole: 16,
        lod_split: 150,
        lod_single: 600,
    };
    while let Some(flag) = args.next() {
        let value = args.next().ok_or(format!("{flag} needs a value"))?;
        let number = || value.parse::<usize>().map_err(|e| format!("{flag}: {e}"));
        match flag.as_str() {
            "--ticks" => {
                let (a, b) = value.split_once("..").ok_or("--ticks takes A..B")?;
                let parse = |s: &str| s.parse::<u32>().map_err(|e| format!("--ticks: {e}"));
                limits.ticks = Some((parse(a)?, parse(b)?));
            }
            "--max-hole-px" => limits.hole = number()?,
            "--max-lod-split" => limits.lod_split = number()?,
            "--max-lod-single" => limits.lod_single = number()?,
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    Ok((dir, limits))
}

fn main() -> ExitCode {
    let (dir, limits) = match parse_args() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    let mut frames: Vec<(u32, PathBuf)> = match std::fs::read_dir(&dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter_map(|p| {
                let name = p.file_name()?.to_str()?;
                let tick = name
                    .strip_prefix("tick-")?
                    .strip_suffix(".ppm")?
                    .parse()
                    .ok()?;
                Some((tick, p))
            })
            .collect(),
        Err(e) => {
            eprintln!("{}: {e}", dir.display());
            return ExitCode::from(2);
        }
    };
    frames.sort();
    let (mut checked, mut split_frames, mut failed) = (0, 0, 0);
    let (mut worst_hole, mut worst_lod_split, mut worst_lod_single) = (0, 0, 0);
    for (tick, path) in &frames {
        if let Some((a, b)) = limits.ticks {
            if *tick < a || *tick > b {
                continue;
            }
        }
        let frame = match Frame::load(path) {
            Ok(f) => f,
            Err(e) => {
                eprintln!("{e}");
                return ExitCode::from(2);
            }
        };
        if frame.blank() {
            continue;
        }
        checked += 1;
        let split = frame.split();
        let views: &[(&str, usize, usize)] = if split {
            &[("top", 0, H / 2), ("bottom", H / 2, H)]
        } else {
            &[("full", 0, H)]
        };
        split_frames += split as usize;
        let lod_limit = if split {
            limits.lod_split
        } else {
            limits.lod_single
        };
        let mut problems = Vec::new();
        for &(name, y0, y1) in views {
            let hole = hole_pixels(&frame, y0, y1);
            let lod = largest_lod(&frame, y0, y1);
            worst_hole = worst_hole.max(hole);
            if split {
                worst_lod_split = worst_lod_split.max(lod);
            } else {
                worst_lod_single = worst_lod_single.max(lod);
            }
            if hole > limits.hole {
                problems.push(format!("{name}: {hole} px of sky under the view"));
            }
            if lod > lod_limit {
                problems.push(format!("{name}: LOD car covering {lod} px"));
            }
        }
        if !problems.is_empty() {
            failed += 1;
            println!("tick {tick}: {}", problems.join("; "));
        }
    }
    println!(
        "{checked} frames checked ({split_frames} split), {failed} failed; worst hole {worst_hole} px \
         (limit {}), worst LOD car cover split {worst_lod_split} px (limit {}), single {worst_lod_single} px (limit {})",
        limits.hole, limits.lod_split, limits.lod_single
    );
    if checked == 0 {
        eprintln!("no frames checked");
        return ExitCode::from(2);
    }
    if failed > 0 {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}
