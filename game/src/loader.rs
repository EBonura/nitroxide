// SPDX-License-Identifier: GPL-2.0-or-later
//! The loading screen: the boost meter's ten slanted cells, large and centred
//! on an empty screen, filling as the load really progresses, a white sweep
//! running along what is lit.
//!
//! The load is blocking, so the bar can only move between its steps. Each
//! [`Loader::report`] draws a frame or two at the progress reached, and a load
//! that is over too fast to see is stretched at its end ([`Loader::finish`]) so
//! the bar always fills smoothly rather than flashing: it never takes less than
//! [`MIN_FRAMES`] vblanks from the first frame to the last.

use psx_engine::Ctx;
use psx_gpu::Gpu;
use psx_rt::interrupts::vblank_count;

use crate::fx::{self, Rgb, ONE};

/// How many cells the bar has.
const CELLS: i32 = 10;
/// The fewest vblanks a load shows for.
const MIN_FRAMES: u32 = 40;
/// Cell geometry, three times the in-match meter's: pitch, width, height, slant.
const PITCH: i32 = 25;
const CELL_W: i32 = 21;
const HEIGHT: i32 = 30;
const SLANT: i32 = 12;
const WIDTH: i32 = (CELLS - 1) * PITCH + CELL_W + SLANT;

const CLEAR: Rgb = (6, 8, 16);
const EMPTY: Rgb = (28, 36, 68);
const AMBER: Rgb = (255, 212, 71);
const HOT: Rgb = (255, 112, 26);
const WHITE: Rgb = (255, 255, 255);

/// A load in progress.
pub struct Loader {
    /// What the bar shows, Q12.
    shown: i32,
    /// How far the load really is, Q12.
    real: i32,
    /// Frames drawn, for the sweep.
    frame: u32,
    /// The vblank the load began on.
    began: u32,
}

impl Loader {
    /// Start a load: wait for the GPU to be quiet, and show an empty bar.
    pub fn begin(ctx: &mut Ctx) -> Self {
        psx_gpu::submit_linked_list_wait();
        let mut loader = Self {
            shown: 0,
            real: 0,
            frame: 0,
            began: vblank_count(),
        };
        loader.draw(ctx, 0);
        loader
    }

    fn elapsed(&self) -> i32 {
        vblank_count().wrapping_sub(self.began).min(10_000) as i32
    }

    /// The load has reached `real`, Q12 of the whole: draw the bar eased on to it.
    /// The bar only ever shows what has really been done; a load too short to
    /// see is stretched by [`Loader::finish`], at its end.
    pub fn report(&mut self, ctx: &mut Ctx, real: i32) {
        self.real = real.clamp(0, ONE);
        let target = self.real.max(self.shown);
        for k in 1..=2 {
            let at = self.shown + (target - self.shown) * k / 2;
            self.draw(ctx, at);
        }
        self.shown = target;
    }

    /// The load is done: fill what is left at an even pace, for at least
    /// [`MIN_FRAMES`] in all, and flash.
    pub fn finish(mut self, ctx: &mut Ctx) {
        self.real = ONE;
        loop {
            let want = (self.elapsed() * ONE / MIN_FRAMES as i32).clamp(self.shown, ONE);
            // At most a sixteenth a frame, so a long load's last stretch is a
            // quick run to the end rather than a jump.
            self.shown = (self.shown + (ONE / 16)).min(want.max(self.shown));
            self.draw(ctx, self.shown);
            next_vblank();
            // Done when full and the minimum time is up: a full bar that is
            // early waits, its sweep still running.
            if self.shown >= ONE && self.elapsed() >= MIN_FRAMES as i32 {
                break;
            }
        }
        // A white flash that fades, then the screen is the game's again.
        for k in 0..6 {
            self.draw_flash(ctx, ONE - ONE * k / 6);
            next_vblank();
        }
    }

    fn draw(&mut self, ctx: &mut Ctx, shown: i32) {
        self.frame += 1;
        let (gpu, fb) = ctx.gpu_and_buffers();
        fb.clear(gpu, CLEAR);
        bar(gpu, shown, self.frame, 0);
        gpu.wait_idle();
        fb.swap(gpu);
    }

    fn draw_flash(&mut self, ctx: &mut Ctx, flash: i32) {
        self.frame += 1;
        let (gpu, fb) = ctx.gpu_and_buffers();
        fb.clear(gpu, CLEAR);
        bar(gpu, ONE, self.frame, flash);
        gpu.wait_idle();
        fb.swap(gpu);
    }
}

/// The bar, `shown` of the way full (Q12), with `flash` (Q12) of white over it.
fn bar(gpu: &mut Gpu, shown: i32, frame: u32, flash: i32) {
    let x0 = (320 - WIDTH) / 2;
    let y = (240 - HEIGHT) / 2;
    let filled = shown * CELLS; // Q12 cells
    for i in 0..CELLS {
        let x = x0 + i * PITCH;
        fx::slanted(gpu, x, y, CELL_W, HEIGHT, SLANT, EMPTY);
        let on = fx::clamp(filled - i * ONE, 0, ONE);
        if on > 0 {
            let colour = fx::mix(AMBER, HOT, i * 256 / CELLS);
            fx::slanted(gpu, x, y, CELL_W * on >> 12, HEIGHT, SLANT, colour);
        }
    }
    fx::additive(gpu);
    // The pickup sweep: a white slant that runs along what is lit, over and over.
    let lit_end = x0 + (filled * PITCH >> 12) + SLANT;
    let run = lit_end - x0;
    if run > 8 {
        let k = (frame % 26) as i32 * ONE / 26;
        let sx = x0 - 14 + ((run + 14) * k >> 12);
        let a = 230 - (k * 130 >> 12);
        let clip = (lit_end - sx).clamp(0, 12);
        fx::slanted_blended(gpu, sx, y - 1, clip, HEIGHT + 2, SLANT, fx::dim(WHITE, a));
    }
    // A glint on the cell being filled.
    if shown > 0 && shown < ONE {
        let edge = x0 + (filled * PITCH >> 12);
        fx::slanted_blended(
            gpu,
            edge - 3,
            y - 1,
            6,
            HEIGHT + 2,
            SLANT,
            fx::dim(WHITE, 160),
        );
    }
    if flash > 0 {
        let a = flash * 255 >> 12;
        fx::slanted_blended(
            gpu,
            x0 - 6,
            y - 4,
            WIDTH + 8,
            HEIGHT + 8,
            SLANT,
            fx::dim(WHITE, a),
        );
    }
    fx::opaque(gpu);
}

/// Wait for the next vblank by watching the counter.
fn next_vblank() {
    let at = vblank_count();
    while vblank_count() == at {}
}
