// SPDX-License-Identifier: GPL-2.0-or-later
//! The kickoff countdown: a big 3, 2, 1 that punches in on a slanted plate in
//! the two team colours, and a GO! burst when play is released.
//!
//! Everything here is read off the sim: the digit and its age come from the
//! ticks left in `kickoff_hold`, the burst from the ticks of live play since,
//! so the animation lands on the same ticks as the countdown's beeps and the
//! go (`audio::clock_sounds`). Drawn over the finished frame, in whole screen
//! or over the seam of a split screen.

use nitroxide_sim::Sim;
use psx_font::FontAtlas;
use psx_gpu::Gpu;
use psx_math::sincos::{cos_q12, sin_q12};

use crate::draw::seat_signal;
use crate::fx::{self, Rgb, ONE};

/// Ticks of live play the GO! burst stays up for.
const GO_TICKS: u32 = 8;
/// The plate behind a number and what the ends of it are made of.
const PLATE: Rgb = (13, 20, 48);
const RULE: Rgb = (226, 232, 244);
const GOLD: Rgb = (255, 222, 110);

/// Draw the countdown or the burst for this tick of the sim, if either is up.
/// `held` means the fly-over is still running and nothing shows.
pub fn draw(gpu: &mut Gpu, font: &FontAtlas, sim: &Sim, held: bool, split: bool) {
    if held || sim.goal_freeze > 0 {
        return;
    }
    // One in the middle of each view: the whole screen's, or each half's.
    if split {
        draw_in(gpu, font, sim, 160, 60, true);
        draw_in(gpu, font, sim, 160, 180, true);
    } else {
        draw_in(gpu, font, sim, 160, 78, false);
    }
}

/// The countdown or the burst centred on (`cx`, `cy`).
fn draw_in(gpu: &mut Gpu, font: &FontAtlas, sim: &Sim, cx: i32, cy: i32, split: bool) {
    let rest = if split { 640 } else { 1024 };
    if sim.kickoff_hold > 0 {
        let hold = i32::from(sim.kickoff_hold);
        let digit = (hold + 59) / 60;
        // Ticks since this number appeared: 0 on the tick the beep sounds.
        let age = (60 - hold % 60) % 60;
        let text = ["1", "2", "3"][(digit.clamp(1, 3) - 1) as usize];
        let punch = punch_in(age);
        let fade = if age > 52 { 256 - (age - 52) * 30 } else { 256 };
        plate(gpu, cx, cy, punch, 0, split, fade);
        big_text(
            font,
            text,
            cx,
            cy,
            rest * punch >> 8,
            fx::dim((255, 232, 130), fade),
        );
    } else if u32::from(sim.kickoff_ticks()) < GO_TICKS {
        go_burst(
            gpu,
            font,
            u32::from(sim.kickoff_ticks()) as i32,
            cx,
            cy,
            rest,
            split,
        );
    }
}

/// How big the number is as it lands, in Q8 of its resting size: it arrives
/// small, overshoots, settles, drifts down and shrinks away at the end.
fn punch_in(age: i32) -> i32 {
    match age {
        ..=7 => 60 + (290 * fx::ease_out(age * ONE / 8) >> 12),
        8..=17 => 350 - (94 * fx::ease_out((age - 8) * ONE / 10) >> 12),
        18..=51 => 256 - (age - 18) * 20 / 34,
        _ => 236 - (116 * fx::ease_in((age - 52) * ONE / 8) >> 12),
    }
}

/// The slanted plate behind a number: navy in the middle, the two teams' colours
/// at the ends, a light rule top and bottom. `punch` scales it with the number.
fn plate(gpu: &mut Gpu, cx: i32, cy: i32, punch: i32, extra: i32, split: bool, fade: i32) {
    let (w, h, end, slant) = if split {
        (150, 38, 34, 8)
    } else {
        (190, 64, 46, 12)
    };
    let (w, h) = (
        (w * punch.clamp(40, 300) >> 8) + extra,
        h * punch.clamp(40, 300) >> 8,
    );
    let (x, y) = (cx - w / 2 - slant / 2, cy - h / 2);
    let team = |seat: usize| fx::dim(seat_signal(seat), fade);
    fx::slanted(gpu, x, y, w, h, slant, fx::dim(PLATE, fade));
    fx::slanted(gpu, x, y, end, h, slant, team(0));
    fx::slanted(gpu, x + w - end, y, end, h, slant, team(1));
    let rule = fx::dim(RULE, fade);
    fx::slanted(gpu, x, y, w, 2, slant, rule);
    fx::slanted(gpu, x, y + h - 2, w, 2, slant, rule);
}

/// Centred outlined text at `scale` (Q8 of the font's own size).
fn big_text(font: &FontAtlas, text: &str, cx: i32, cy: i32, scale: i32, tint: Rgb) {
    let scale = scale.clamp(16, 2000) as u16;
    let w = i32::from(font.text_width(text)) * i32::from(scale) / 256;
    let h = i32::from(font.line_height()) * i32::from(scale) / 256;
    // The advance of the last glyph ends in a gap, which is not ink: shift the
    // word back by half of it (measured at about 5 pixels at four times size).
    let gap = i32::from(scale) * 5 / 2048;
    let (x, y) = ((cx - w / 2 + gap) as i16, (cy - h / 2) as i16);
    let ink = (tint.0.div_ceil(2), tint.1.div_ceil(2), tint.2.div_ceil(2));
    let o = if scale > 600 { 2 } else { 1 };
    for (dx, dy) in [(o, o)] {
        font.draw_text_scaled_q8(x + dx, y + dy, text, scale, scale, (0, 0, 0));
    }
    font.draw_text_scaled_q8(x, y, text, scale, scale, ink);
}

/// GO!: the word pops, a flash sweeps the plate's colours across the screen and
/// rays leave the centre.
fn go_burst(gpu: &mut Gpu, font: &FontAtlas, t: i32, cx: i32, cy: i32, rest: i32, split: bool) {
    // Over in eight ticks: the word punches out as the cars launch and is gone
    // before they have moved, so nothing lingers over the pitch.
    let fade = 256 - t * 256 / GO_TICKS as i32;
    let scale = match t {
        0 => 160,
        1 => 420,
        2 => 440,
        _ => 440 + (t - 2) * 40,
    };
    if t < 3 {
        plate(gpu, cx, cy, scale.min(300) * 3 / 4 + 70, 90, split, fade);
    }
    big_text(
        font,
        "GO!",
        cx,
        cy,
        rest * 3 / 5 * scale >> 8,
        fx::dim(GOLD, fade),
    );
    fx::additive(gpu);
    if t < 2 {
        let a = 230 - t * 90;
        let (w, h) = (if split { 320 } else { 360 }, if split { 44 } else { 70 });
        fx::slanted_blended(
            gpu,
            cx - w / 2,
            cy - h / 2,
            w,
            h,
            12,
            fx::dim((255, 255, 255), a),
        );
    }
    // Rays leave the word and are gone with it.
    {
        let grow = fx::ease_out(t * ONE / GO_TICKS as i32);
        let (r_in, r_len) = if split { (26, 40) } else { (34, 64) };
        for i in 0..6 {
            let a = (i * 683 + 150) as u16;
            let (s, c) = (sin_q12(a), cos_q12(a));
            let near = r_in + (r_len * grow >> 12);
            let far = near + 14;
            let (px, py) = ((-c * 2) >> 12, (s * 2) >> 12);
            let p = |r: i32, k: i32| {
                (
                    cx + ((s * r) >> 12) + px * k,
                    cy + ((c * r * 3 / 4) >> 12) + py * k,
                )
            };
            let colour = if i % 2 == 0 { GOLD } else { (255, 255, 255) };
            fx::quad_blended(
                gpu,
                [p(near, -1), p(near, 1), p(far, -1), p(far, 1)],
                fx::dim(colour, fade),
            );
        }
    }
    fx::opaque(gpu);
}
