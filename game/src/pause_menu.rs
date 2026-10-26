// SPDX-License-Identifier: GPL-2.0-or-later
//! The pause menu: a dark slanted panel with a header band, the live score and
//! slanted buttons with the title menu's cyan selection bar.
//!
//! It builds in out of tiles, the sweep of [`crate::sweep`] clipped to the
//! panel's own area, with the match still in view around it and dimmed a little,
//! and breaks up the same way on resume. Quitting and restarting, which are real
//! scene changes, ask first and then use the full-screen sweep.

use nitroxide_sim::WinCondition;
use psx_engine::Ctx;
use psx_font::FontAtlas;
use psx_gpu::Gpu;
use psx_math::fmt::{u32_dec, U32_DEC_MAX};

use crate::draw::{self, seat_signal};
use crate::fx::{self, Rgb, ONE};
use crate::{sweep, NitroXide, PauseRow, PAUSE_HALF, PAUSE_TICKS};

/// The panel's rectangle, on the 16 pixel tile grid so a sweep can cover it
/// exactly: 14 by 13 tiles.
const PX: i32 = 48;
const PY: i32 = 16;
const PW: i32 = 224;
const PH: i32 = 192;
const AREA: sweep::Area = sweep::Area {
    col: PX / 16,
    row: PY / 16,
    cols: PW / 16,
    rows: PH / 16,
};
/// How far the top edge of the panel and its bars lead their bottom edge.
const SLANT: i32 = 8;
/// How much the match is darkened behind the menu, of 255.
const DIM: i32 = 70;
/// The size of the button labels, Q8 of the display face.
const LABEL_Q8: u16 = 200;

const CYAN_HI: Rgb = (92, 200, 242);
const CYAN_LO: Rgb = (46, 138, 190);
const NIB: Rgb = (255, 232, 150);
const LIT: Rgb = (255, 255, 255);
const IDLE: Rgb = (148, 158, 182);

impl NitroXide {
    /// The whole pause screen over the finished frame: the dim, then the menu
    /// or the settings and controls pages that open from it, then the tiles of
    /// the panel's build-in or break-up.
    pub(crate) fn draw_pause(&self, ctx: &mut Ctx, display: &FontAtlas, hud: &FontAtlas) {
        let tick = ctx.sim_tick.as_u32();
        let buffer_y = ctx.fb.draw_origin().1;
        let t = i32::from(self.pause_t);
        let half = i32::from(PAUSE_HALF);
        // The match dims while the tiles come down and lightens as they go.
        let dim = if self.pause_closing {
            ONE - fx::clamp((t - half) * ONE / half, 0, ONE)
        } else {
            fx::clamp(t * ONE / half, 0, ONE)
        };
        dim_frame(Gpu::from_dma_mut(ctx.gpu_dma()), dim);
        if self.settings.is_some() {
            self.draw_settings(display, hud, buffer_y);
            return;
        }
        if let Some(page) = self.controls {
            self.draw_controls(display, hud, buffer_y, page);
            return;
        }
        let building = self.pause_closing || self.pause_t < PAUSE_TICKS;
        // Under the tiles the panel is there while they are all down or rising.
        let shown = if self.pause_closing {
            t < half
        } else {
            t >= half
        };
        if shown {
            self.draw_panel(Gpu::from_dma_mut(ctx.gpu_dma()), display, hud, tick);
        }
        if building {
            sweep::draw_in(
                Gpu::from_dma_mut(ctx.gpu_dma()),
                self.pause_t.into(),
                PAUSE_HALF.into(),
                AREA,
            );
        }
    }

    fn draw_panel(&self, gpu: &mut Gpu, display: &FontAtlas, hud: &FontAtlas, tick: u32) {
        // The plate: a dark border, a lighter body, both leaning like the buttons.
        let w = PW - SLANT;
        fx::slanted(gpu, PX, PY, w, PH, SLANT, (10, 12, 22));
        fx::slanted_gradient(
            gpu,
            (PX + 3, PY + 3, w - 6, PH - 6),
            SLANT,
            (32, 38, 60),
            (16, 19, 31),
        );

        // Header: PAUSED in the display face, the rule in a line on the right.
        let (hx, hy, hh) = (PX + 8, PY + 8, 26);
        fx::slanted_gradient(
            gpu,
            (hx, hy, w - 22, hh),
            SLANT / 2,
            (52, 62, 96),
            (26, 30, 48),
        );
        fx::slanted(gpu, hx, hy + hh - 2, w - 22, 2, SLANT / 2, CYAN_HI);
        // Centred on the band, whose middle leans with its slant.
        let mid = hx + (w - 22) / 2 + SLANT / 4;
        Self::centred(
            display,
            mid as i16,
            (hy + (hh - 10) / 2 - 1) as i16,
            "PAUSED",
            LIT,
        );
        self.draw_score(gpu, hud);

        // The buttons, or the question that replaces them.
        let top = PY + 72;
        let rows = self.pause_rows();
        let step = if rows.len() > 5 { 19 } else { 21 };
        let elapsed = tick.wrapping_sub(self.pause_at);
        if let Some(ask) = self.pause_confirm {
            let (line, sub) = match ask {
                PauseRow::Restart => ("RESTART THE MATCH?", "THE SCORE AND CLOCK START OVER"),
                _ => ("QUIT TO MENU?", "THE MATCH WILL BE LOST"),
            };
            Self::centred(display, (PX + PW / 2) as i16, (top + 2) as i16, line, LIT);
            Self::centred(hud, (PX + PW / 2) as i16, (top + 20) as i16, sub, IDLE);
            for (i, label) in ["YES", "NO"].into_iter().enumerate() {
                let lit = (i == 0) == self.confirm_yes;
                button(
                    gpu,
                    display,
                    top + 38 + i as i32 * step,
                    step - 3,
                    label,
                    lit,
                    elapsed,
                );
            }
        } else {
            for (i, row) in rows.iter().enumerate() {
                let label = match row {
                    PauseRow::Resume => "RESUME",
                    PauseRow::Swap if self.swap_seats => "SIDES: P2 P1",
                    PauseRow::Swap => "SIDES: P1 P2",
                    PauseRow::Settings => "SETTINGS",
                    PauseRow::Controls => "CONTROLS",
                    PauseRow::Restart => "RESTART MATCH",
                    PauseRow::Quit => "QUIT TO MENU",
                };
                let lit = i == self.pause_row;
                button(
                    gpu,
                    display,
                    top + i as i32 * step,
                    step - 3,
                    label,
                    lit,
                    elapsed,
                );
            }
        }
    }

    /// The live score: the two teams' colours at the ends, the clock or the
    /// target between them.
    fn draw_score(&self, gpu: &mut Gpu, hud: &FontAtlas) {
        let (y, h) = (PY + 40, 24);
        let (x, block) = (PX + 22, 46);
        let w = PW - 44 - SLANT;
        let plate = (13, 20, 48);
        fx::slanted(gpu, x, y, w, h, SLANT / 2, plate);
        for (seat, bx) in [(0usize, x), (1, x + w - block)] {
            let c = seat_signal(seat);
            fx::slanted_gradient(
                gpu,
                (bx, y, block, h),
                SLANT / 2,
                fx::mix(c, (0, 0, 0), 90),
                c,
            );
        }
        let mut dec = [0u8; U32_DEC_MAX];
        let white = (255, 255, 255);
        for (seat, score) in [self.sim.score_blue, self.sim.score_orange]
            .into_iter()
            .enumerate()
        {
            let text = u32_dec(&mut dec, score as u32);
            let tw = hud.text_width(text) as i32 * 2;
            let bx = if seat == 0 { x } else { x + w - block };
            let tx = bx + (block + SLANT / 2 - tw) / 2;
            Self::shadowed_big(hud, tx as i16, (y + 4) as i16, text, white);
        }
        let mut clock = [b'0'; 4];
        let centre = match self.sim.win_condition {
            WinCondition::TimeLimit(_) => {
                let secs = self.sim.clock / 60;
                clock[0] = b'0' + (secs / 60) as u8;
                clock[1] = b':';
                clock[2] = b'0' + ((secs % 60) / 10) as u8;
                clock[3] = b'0' + ((secs % 60) % 10) as u8;
                // SAFETY: four ASCII bytes.
                unsafe { core::str::from_utf8_unchecked(&clock) }
            }
            WinCondition::GoalLimit(_) => "FIRST TO",
        };
        let mid = x + block + (w - 2 * block) / 2 + SLANT / 4;
        let cw = hud.text_width(centre) as i32;
        Self::shadowed(
            hud,
            (mid - cw / 2) as i16,
            (y + 8) as i16,
            centre,
            (226, 232, 244),
        );
    }
}

/// One slanted button: the dark plate, and when it is the selected one the cyan
/// bar sweeping in from the left and the gold nib.
fn button(
    gpu: &mut Gpu,
    display: &FontAtlas,
    y: i32,
    h: i32,
    label: &str,
    lit: bool,
    elapsed: u32,
) {
    let x = PX + 18;
    let w = PW - 18 - 14 - SLANT;
    fx::slanted_gradient(gpu, (x, y, w, h), SLANT, (40, 46, 66), (24, 28, 44));
    if lit {
        let sweep = (w * draw::menu_sweep(elapsed)) >> 12;
        fx::slanted_gradient(gpu, (x, y, sweep, h), SLANT, CYAN_HI, CYAN_LO);
        fx::slanted(gpu, x, y, 5, h, SLANT, NIB);
    }
    let ink = if lit { LIT } else { IDLE };
    let (lx, ly) = ((x + 20) as i16, (y + (h - 8) / 2) as i16);
    // The display face at three quarters, so the longest label fits its bar.
    let tint = (ink.0.div_ceil(2), ink.1.div_ceil(2), ink.2.div_ceil(2));
    display.draw_text_scaled_q8(lx + 1, ly + 1, label, LABEL_Q8, LABEL_Q8, (0, 0, 0));
    display.draw_text_scaled_q8(lx, ly, label, LABEL_Q8, LABEL_Q8, tint);
}

/// Darken the frame by `amount` of the menu's full dim, Q12.
fn dim_frame(gpu: &mut Gpu, amount: i32) {
    let v = (DIM * amount >> 12) as u8;
    if v == 0 {
        return;
    }
    fx::subtractive(gpu);
    fx::rect_blended(
        gpu,
        0,
        0,
        i32::from(draw::SCREEN_W),
        i32::from(draw::SCREEN_H),
        (v, v, v),
    );
    fx::opaque(gpu);
}
