// SPDX-License-Identifier: GPL-2.0-or-later
//! The tile sweep between the match and its menus.
//!
//! Sixteen pixel tiles cover the 320x240 frame from left to right, each one
//! growing from its centre, with a column stagger and a small jitter per tile
//! and a one pixel cyan leading edge while it moves. When the last tile is
//! down the screen underneath is swapped, and the tiles shrink away the same
//! way to show the new one. Flat quads only, drawn over the finished frame:
//! there is no copy of the old frame, because the old screen simply keeps
//! being drawn until the tiles hide it.

use psx_gpu::Gpu;

use crate::fx::{self, Rgb, ONE};

/// Ticks (60 Hz) from the first tile to the last one gone.
pub const TICKS: u32 = 30;
/// The tick on which the tiles are all down and the screen is swapped.
pub const SWAP_TICK: u32 = TICKS / 2;

const TILE: i32 = 16;
const COLS: i32 = 20;
const ROWS: i32 = 15;
/// Tiles alternate between two navies, in a checkerboard.
const NAVY: [Rgb; 2] = [(17, 26, 61), (13, 20, 48)];
const CYAN: Rgb = (70, 184, 232);

/// What changes underneath the tiles on [`SWAP_TICK`].
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Change {
    /// Leave the match for the title menu.
    Quit,
    /// Leave the select screen for the match.
    Kickoff,
    /// Start the match again from the pause menu.
    Restart,
    /// The final whistle: on to the results.
    Results,
}

/// A running sweep.
#[derive(Clone, Copy)]
pub struct Sweep {
    /// Ticks since it started, `0..TICKS`.
    pub t: u32,
    pub change: Change,
}

impl Sweep {
    pub const fn new(change: Change) -> Self {
        Self { t: 0, change }
    }
}

/// A block of the 16 pixel tile grid: the part of the screen a sweep covers.
#[derive(Clone, Copy)]
pub struct Area {
    pub col: i32,
    pub row: i32,
    pub cols: i32,
    pub rows: i32,
}

impl Area {
    /// The whole 320x240 frame.
    pub const SCREEN: Area = Area {
        col: 0,
        row: 0,
        cols: COLS,
        rows: ROWS,
    };
}

/// Draw the tiles for tick `t` of a full-screen sweep.
pub fn draw(gpu: &mut Gpu, t: u32) {
    draw_in(gpu, t, SWAP_TICK, Area::SCREEN);
}

/// Draw the tiles of a sweep that covers `area` and nothing else: `half` ticks
/// to cover it and `half` to uncover it, `t` ticks in. The stagger runs across
/// the area's own columns.
pub fn draw_in(gpu: &mut Gpu, t: u32, half: u32, area: Area) {
    let covering = t < half;
    let half = half as i32;
    let step = if covering { t as i32 } else { t as i32 - half };
    // Progress through this half, Q12: its first tick is 0 and its last is 1,
    // so the cover ends fully down and the reveal ends with nothing left.
    let q = step * ONE / (half - 1);
    for row in 0..area.rows {
        for col in 0..area.cols {
            // When this tile starts: later columns start later, and each is
            // nudged a little so the front is ragged rather than a wipe.
            let start = col * 2540 / (area.cols - 1).max(1)
                + (fx::scatter(col as u32, row as u32) as i32) * 328 / 255;
            // How far through its own growth or shrink the tile is, Q12.
            let l = fx::clamp((q - start) * 10 / 3, 0, ONE);
            let scale = if covering {
                fx::ease_out(l)
            } else {
                ONE - fx::ease_in(l)
            };
            let w = TILE * scale >> 12;
            if w <= 0 {
                continue;
            }
            let x = (area.col + col) * TILE + ((TILE - w) >> 1);
            let y = (area.row + row) * TILE + ((TILE - w) >> 1);
            fx::rect(gpu, x, y, w, w, NAVY[((col + row) & 1) as usize]);
            if l > 0 && l < ONE {
                fx::rect(gpu, x, y, w, 1, CYAN);
            }
        }
    }
}
