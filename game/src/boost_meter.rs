// SPDX-License-Identifier: GPL-2.0-or-later
//! The boost meter: ten slanted cells, shaped like the menu buttons, filling
//! from amber to orange, with the number on a small slanted tag above the
//! right-hand end. Drawn over the finished frame, one per view.
//!
//! A pad pickup sends a white sweep along the bar and three chevrons flashing
//! past its end, the big pad adds a thin outline flash, and the shown fill
//! climbs to the new value over [`CLIMB_TICKS`]. While boosting, the last lit
//! cell flickers and short orange streaks trail off the back of the bar.

use nitroxide_sim::{Car, Sim, BOOST_MAX_PIPS, BOOST_SCALE, PADS};
use psx_font::FontAtlas;
use psx_gpu::Gpu;
use psx_math::fmt::{u32_dec, U32_DEC_MAX};

use crate::draw::{Viewport, SCREEN_H};
use crate::fx::{self, Rgb, ONE};

/// How many cells the bar has.
const CELLS: i32 = 10;
/// Ticks the shown fill takes to climb to a freshly picked up value.
pub const CLIMB_TICKS: u32 = 12;
/// How long the pickup sweep and chevrons run.
const SWEEP_TICKS: u32 = 16;
/// How long the big pad's outline flash runs.
const FLASH_TICKS: u32 = 20;
/// A `since` that means no pickup lately.
const LONG_AGO: u32 = 999;
/// A big pad's respawn timer, which is what it holds on the tick it was taken.
const BIG_PAD_TIMER: u16 = 600;

const EMPTY: Rgb = (13, 20, 48);
const AMBER: Rgb = (255, 212, 71);
const HOT: Rgb = (255, 112, 26);
const WHITE: Rgb = (255, 255, 255);
const CHEVRON: Rgb = (255, 190, 80);
const OUTLINE: Rgb = (255, 210, 120);
const STREAK: Rgb = (255, 170, 70);

/// What one seat's meter remembers between ticks.
#[derive(Clone, Copy)]
pub struct Meter {
    /// The car's boost last tick, in storage units, to see a rise.
    last: i32,
    /// The fill the climb started from, in pips.
    from: i32,
    /// The car's boost now, in pips.
    value: i32,
    /// Ticks since the last pickup.
    since: u32,
    /// The last pickup was a big pad.
    big: bool,
    /// The car is burning boost.
    burning: bool,
}

impl Meter {
    pub const fn new() -> Self {
        Self {
            last: 0,
            from: 0,
            value: 0,
            since: LONG_AGO,
            big: false,
            burning: false,
        }
    }

    /// The fill to draw, in pips: the value, or on its way there after a pickup.
    fn shown(&self) -> i32 {
        if self.since >= CLIMB_TICKS {
            return self.value;
        }
        let t = self.since as i32 * ONE / CLIMB_TICKS as i32;
        self.from + ((self.value - self.from) * fx::ease_out(t) >> 12)
    }

    /// Follow one seat's car after a sim tick. `taken` is `Some(big)` when a pad
    /// was driven over this tick, whoever took it: the car's own rise says
    /// whether it was this one.
    fn track(&mut self, car: &Car, taken: Option<bool>) {
        let shown = self.shown();
        match taken {
            Some(big) if car.boost > self.last => {
                self.from = shown;
                self.since = 0;
                self.big = big;
            }
            _ => self.since = (self.since + 1).min(LONG_AGO),
        }
        self.value = (car.boost / BOOST_SCALE).clamp(0, BOOST_MAX_PIPS);
        self.last = car.boost;
        self.burning = car.boosting;
    }
}

/// Both seats' meters.
#[derive(Clone, Copy)]
pub struct Meters {
    seats: [Meter; 2],
}

impl Meters {
    pub const fn new() -> Self {
        Self {
            seats: [Meter::new(), Meter::new()],
        }
    }

    /// Follow the match after one sim tick.
    pub fn track(&mut self, sim: &Sim) {
        // The pad taken this tick is the one whose timer was just set to its
        // respawn; of those only the big pads' is told apart by it.
        let taken = sim.pad_taken.then(|| {
            PADS.iter()
                .zip(sim.pad_timers.iter())
                .any(|(pad, timer)| pad.big && *timer == BIG_PAD_TIMER)
        });
        self.seats[0].track(&sim.car, taken);
        self.seats[1].track(&sim.opponent, taken);
    }

    pub fn seat(&self, seat: usize) -> &Meter {
        &self.seats[seat]
    }
}

/// Where the bar and its tag go in a view, and how big everything is. A
/// half-height view gets two thirds of everything.
struct Layout {
    x: i32,
    y: i32,
    pitch: i32,
    cell_w: i32,
    h: i32,
    slant: i32,
    tag_w: i32,
    tag_h: i32,
    tag_gap: i32,
    /// Scale for effect distances, as a fraction.
    num: i32,
    den: i32,
}

impl Layout {
    fn new(vp: Viewport) -> Self {
        // The bar ends 14 pixels in from the right and its foot sits 18 above
        // the bottom: inside the title-safe area a television crops to.
        let (pitch, cell_w, h, slant, tag_w, tag_h, tag_gap, num) = if vp.h < SCREEN_H {
            (5, 4, 5, 3, 20, 9, 2, 2)
        } else {
            (7, 6, 8, 4, 30, 11, 4, 3)
        };
        let width = (CELLS - 1) * pitch + cell_w + slant;
        Self {
            x: i32::from(vp.x) + i32::from(vp.w) - 14 - width,
            y: i32::from(vp.y) + i32::from(vp.h) - 18 - h,
            pitch,
            cell_w,
            h,
            slant,
            tag_w,
            tag_h,
            tag_gap,
            num,
            den: 3,
        }
    }

    fn width(&self) -> i32 {
        (CELLS - 1) * self.pitch + self.cell_w + self.slant
    }

    /// A length scaled for this view.
    fn s(&self, v: i32) -> i32 {
        (v * self.num + self.den / 2) / self.den
    }
}

/// Draw one seat's meter in `vp`. `tick` is the frame clock the flicker and the
/// streaks run on.
pub fn draw(gpu: &mut Gpu, font: &FontAtlas, vp: Viewport, meter: &Meter, tick: u32) {
    let l = Layout::new(vp);
    let shown = meter.shown();
    // Whole cells are lit up to the shown fill, the next one to its fraction.
    let filled_q = shown * 256 / (BOOST_MAX_PIPS / CELLS);
    let lit = (filled_q + 255) >> 8;

    // The tag with the number, over the bar's right-hand end.
    let tag_x = l.x + l.width() - l.slant - l.tag_w;
    let tag_y = l.y - l.tag_gap - l.tag_h;
    fx::slanted(gpu, tag_x, tag_y, l.tag_w, l.tag_h, l.slant, EMPTY);

    for i in 0..CELLS {
        let x = l.x + i * l.pitch;
        fx::slanted(gpu, x, l.y, l.cell_w, l.h, l.slant, EMPTY);
        let on = fx::clamp(filled_q - i * 256, 0, 256);
        if on == 0 {
            continue;
        }
        let mut colour = fx::mix(AMBER, HOT, i * 256 / CELLS);
        if meter.burning && i == lit - 1 && tick % 4 < 2 {
            colour = WHITE;
        }
        fx::slanted(gpu, x, l.y, l.cell_w * on >> 8, l.h, l.slant, colour);
    }

    let mut dec = [0u8; U32_DEC_MAX];
    let text = u32_dec(&mut dec, meter.value as u32);
    // The middle of the digits' ink on the tag's midline (the slant puts the
    // middle half a slant right of the bottom-left corner) and at half its height;
    // the shadow lies outside the measure, in the lower margin.
    let (tx, ty) = fx::ink_origin(
        font,
        text,
        2 * tag_x + l.tag_w + l.slant,
        2 * tag_y + l.tag_h,
        256,
    );
    font.draw_text(tx + 1, ty + 1, text, (0, 0, 0));
    font.draw_text(tx, ty, text, (128, 128, 128));

    if meter.since >= SWEEP_TICKS.max(FLASH_TICKS) && !meter.burning {
        return;
    }
    fx::additive(gpu);
    let since = meter.since as i32;
    if meter.since < SWEEP_TICKS {
        // A white slant runs along the bar...
        let k = since * ONE / SWEEP_TICKS as i32;
        let sx = l.x + (k * (l.width() + l.s(10)) >> 12) - l.s(6);
        let fade = 230 - since * 8;
        fx::slanted_blended(
            gpu,
            sx,
            l.y - 1,
            l.s(5),
            l.h + 2,
            l.slant * 2,
            fx::dim(WHITE, fade),
        );
        // ...and three chevrons flash one after another past its end.
        for j in 0..3 {
            let a = 256 - (since * 48 - (j * 256 + 128)).abs();
            if a > 0 {
                chevron(gpu, &l, l.x + l.width() + l.s(6) + j * l.s(5), a);
            }
        }
    }
    if meter.big && meter.since < FLASH_TICKS {
        let a = 256 - since * 256 / FLASH_TICKS as i32;
        let c = fx::dim(OUTLINE, a);
        let (x, y, w, h) = (l.x - 2, l.y - 2, l.width() + 4, l.h + 4);
        fx::rect_blended(gpu, x, y, w, 1, c);
        fx::rect_blended(gpu, x, y + h - 1, w, 1, c);
        fx::rect_blended(gpu, x, y + 1, 1, h - 2, c);
        fx::rect_blended(gpu, x + w - 1, y + 1, 1, h - 2, c);
    }
    if meter.burning {
        // Short streaks stream off the back of the bar.
        for i in 0..6u32 {
            let yy = l.y + 1 + (fx::scatter(i, tick >> 2) as i32 * (l.h - 2) >> 8);
            let len = l.s(6 + (fx::scatter(i, (tick >> 1) + 9) as i32 * 16 >> 8));
            let x = l.x - l.s(3) - ((tick * 3 + i * 7) % 24) as i32 * l.num / l.den;
            fx::rect_blended(gpu, x - len, yy, len, 1, fx::dim(STREAK, 140));
        }
    }
    fx::opaque(gpu);
}

/// A right-pointing chevron `>` centred on the bar's height at `x`, at `alpha`
/// (Q8), drawn as two slanted arms.
fn chevron(gpu: &mut Gpu, l: &Layout, x: i32, alpha: i32) {
    let c = fx::dim(CHEVRON, alpha);
    let half = l.s(3);
    let mid = l.y + l.h / 2;
    let thick = l.s(2).max(1);
    // Upper arm falls to the point, lower arm returns.
    fx::slanted_blended(gpu, x, mid - half, thick, half, -half, c);
    fx::slanted_blended(gpu, x - half, mid, thick, half, half, c);
}
