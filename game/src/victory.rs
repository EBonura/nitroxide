// SPDX-License-Identifier: GPL-2.0-or-later
//! The results screen: the winning car on the centre spot, turning, under an
//! orbiting camera with the trophy floating up above it and confetti coming
//! down, and beside it slanted panels in the menus' style that slide in one
//! after another: the verdict, a stats table for both sides and REMATCH or QUIT
//! TO MENU on a sliding selection bar. A loss or a draw gets the same panels
//! over a calmer scene, no cup.
//!
//! The sim's `victory` module has the arithmetic (camera, trophy, confetti);
//! this file stages it and draws it.

use nitroxide_sim::victory::{self, Confetti};
use nitroxide_sim::{Sim, V3};
use psx_engine::{button, Ctx};
use psx_font::FontAtlas;
use psx_gpu::Gpu;
use psx_math::fmt::{u32_dec, U32_DEC_MAX};

use crate::draw::{self, seat_signal, Trophy, VictoryBit};
use crate::fx::{self, Rgb, ONE};
use crate::{audio, replay, sweep, NitroXide};

/// Ticks before the buttons answer: the match's last presses are still coming.
const LOCK: u32 = 100;
/// The panels, on the right: left edge, width, and how far they lean.
const PX: i32 = 160;
const PW: i32 = 152;
const SLANT: i32 = 8;
/// When each panel starts to slide in and how long it takes.
const SLIDE_AT: [u32; 3] = [22, 34, 46];
const SLIDE_TICKS: u32 = 16;

const CYAN_HI: Rgb = (92, 200, 242);
const CYAN_LO: Rgb = (46, 138, 190);
const NIB: Rgb = (255, 232, 150);
const LIT: Rgb = (255, 255, 255);
const IDLE: Rgb = (148, 158, 182);
const GOLD: Rgb = (255, 214, 80);
const DEFEAT: Rgb = (240, 130, 130);
const DRAW: Rgb = (230, 230, 150);

#[cfg(feature = "boot-results")]
static mut VARIANT: u32 = 0;

/// How the match came out.
#[derive(Copy, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The winner's seat.
    Win(usize),
    Draw,
}

/// The state of the results screen.
pub struct Victory {
    pub t: u32,
    /// The selected button, 0 REMATCH and 1 QUIT TO MENU, and the one before it.
    pub row: usize,
    pub prev: usize,
    pub moved_at: u32,
    pub verdict: Verdict,
    /// The single player lost: no cup, a quieter scene, DEFEAT.
    pub lost: bool,
    confetti: Confetti,
    /// A button has been chosen and the sweep is on its way.
    pub chosen: bool,
}

impl Victory {
    pub fn new(sim: &Sim, two_player: bool) -> Self {
        let verdict = if sim.score_blue > sim.score_orange {
            Verdict::Win(0)
        } else if sim.score_blue < sim.score_orange {
            Verdict::Win(1)
        } else {
            Verdict::Draw
        };
        let lost = !two_player && verdict == Verdict::Win(1);
        let mut world = *sim;
        // Nothing is moving, scored or counting down.
        world.goal_freeze = 0;
        world.kickoff_hold = 0;
        for car in [&mut world.car, &mut world.opponent] {
            car.v = V3::ZERO;
            car.w = V3::ZERO;
            car.up = V3::new(0, 4096, 0);
            car.grounded = true;
            car.boosting = false;
            car.supersonic = false;
            car.demo_timer = 0;
            car.steer = 0;
            car.slide = 0;
            car.suspension = [0; 2];
            car.suspension_velocity = [0; 2];
        }
        // The ball is high over the pitch, out of every view.
        world.ball.p = V3::new(0, nitroxide_sim::uu(6000), 0);
        world.ball.v = V3::ZERO;
        let rest = nitroxide_sim::uu(nitroxide_sim::CAR_REST_Y);
        match verdict {
            // A draw: both cars, side by side.
            Verdict::Draw => {
                world.car.p = V3::new(-nitroxide_sim::uu(105), rest, 0);
                world.opponent.p = V3::new(nitroxide_sim::uu(105), rest, 0);
            }
            // A win shows the winner alone; a loss shows the player's own car.
            Verdict::Win(seat) => {
                let shown = if lost { 0 } else { seat };
                let (me, other) = if shown == 0 {
                    (&mut world.car, &mut world.opponent)
                } else {
                    (&mut world.opponent, &mut world.car)
                };
                me.p = V3::new(0, rest, 0);
                other.p = V3::new(0, rest, 0);
                other.demo_timer = 1; // not on the pitch
            }
        }
        let this = Self {
            t: 0,
            row: 0,
            prev: 0,
            moved_at: 0,
            verdict,
            lost,
            confetti: Confetti::new(),
            chosen: false,
        };
        // The world the renderer is handed: the car on the spot and nothing else,
        // staged in the replay's scratch world, which is free by now.
        *replay::scratch() = Some(world);
        this
    }

    /// Whether the cup comes out.
    pub fn trophy(&self) -> bool {
        matches!(self.verdict, Verdict::Win(_)) && !self.lost
    }

    fn winner(&self) -> Option<usize> {
        match self.verdict {
            Verdict::Win(seat) => Some(seat),
            Verdict::Draw => None,
        }
    }
}

fn mix(a: Rgb, b: Rgb, k: i32) -> Rgb {
    fx::mix(a, b, k)
}

impl NitroXide {
    /// The match has just ended: stage the scene.
    pub(crate) fn begin_victory(&mut self) {
        self.victory = Some(Victory::new(&self.sim, self.two_player));
        draw::load_trophy();
        draw::set_lite(true);
        draw::set_replay_plane(Some(victory::orbit(0, true).plane));
        self.publish_victory();
    }

    /// Take the scene down: the camera, the cup and the confetti.
    pub(crate) fn end_victory(&mut self) {
        if self.victory.take().is_none() {
            return;
        }
        draw::set_replay_cam(None);
        draw::set_replay_plane(None);
        draw::set_trophy(None);
        draw::victory_clear();
        draw::unload_trophy();
        draw::set_lite(false);
    }

    /// Dev (`boot-results`): stage variant `n` of the results, to look at each
    /// header: bit 0 two players, bit 1 overtime, and the rest who won (0 blue,
    /// 1 orange, 2 a draw). The screen steps to the next every 170 ticks.
    #[cfg(feature = "boot-results")]
    pub(crate) fn stage_variant(&mut self, n: u32) {
        unsafe { VARIANT = n };
        self.two_player = n & 1 != 0;
        self.sim.overtime = n & 2 != 0;
        let (b, o) = match n >> 2 {
            0 => (3, 1),
            1 => (1, 3),
            _ => (2, 2),
        };
        self.sim.score_blue = b;
        self.sim.score_orange = o;
        self.sim.stats.team[0].shots = 9;
        self.sim.stats.team[1].shots = 4;
        self.sim.stats.team[0].top_speed = 2100;
        self.end_victory();
        self.begin_victory();
    }

    /// One tick of the results screen.
    pub(crate) fn update_victory(&mut self, ctx: &mut Ctx) {
        #[cfg(feature = "boot-results")]
        if self.victory.as_ref().is_some_and(|v| v.t >= 170) {
            self.stage_variant((unsafe { VARIANT } + 1) % 12);
        }
        let two = self.two_player;
        if two {
            ctx.refresh_second_pad();
        }
        let pressed = |b: u16| ctx.just_pressed(b) || (two && ctx.just_pressed_for(1, b));
        let Some(v) = self.victory.as_mut() else {
            return;
        };
        v.t += 1;
        audio::victory_sting(v.t, !v.lost && v.winner().is_some());
        if v.t >= LOCK && !v.chosen {
            let step = i32::from(pressed(button::DOWN)) - i32::from(pressed(button::UP));
            if step != 0 {
                v.prev = v.row;
                v.row = (v.row as i32 + step).rem_euclid(2) as usize;
                v.moved_at = v.t;
                audio::ui(audio::Ui::Move);
            }
            if pressed(button::CROSS) || pressed(button::START) {
                v.chosen = true;
                audio::ui(audio::Ui::Select);
                let change = if v.row == 0 {
                    sweep::Change::Restart
                } else {
                    sweep::Change::Quit
                };
                self.sweep = Some(sweep::Sweep::new(change));
            }
        }
        self.publish_victory();
    }

    /// Hand the renderer this tick's scene: the world, the camera, the cup, the
    /// confetti and the lights.
    fn publish_victory(&mut self) {
        let Some(v) = self.victory.as_mut() else {
            return;
        };
        let t = v.t;
        let shot = victory::orbit(t, true);
        draw::set_replay_cam(Some((shot.eye, shot.target)));
        draw::set_replay_plane(Some(shot.plane));
        // The cars turn slowly.
        let yaw = victory::car_yaw(t);
        if let Some(world) = replay::scratch() {
            match v.verdict {
                Verdict::Draw => {
                    world.car.yaw = yaw;
                    world.opponent.yaw = yaw.wrapping_neg();
                }
                Verdict::Win(0) => world.car.yaw = yaw,
                Verdict::Win(_) => {
                    if v.lost {
                        world.car.yaw = yaw;
                    } else {
                        world.opponent.yaw = yaw;
                    }
                }
            }
        }
        draw::victory_clear();
        let signal = match v.winner() {
            Some(seat) => seat_signal(seat),
            None => (180, 190, 215),
        };
        if v.trophy() {
            if let Some((y, tyaw)) = victory::trophy(t) {
                draw::set_trophy(Some(Trophy {
                    pos: (0, y, 0),
                    yaw: tyaw,
                    fade: victory::trophy_fade(t),
                }));
                let age = t as i32 - victory::APPEAR as i32;
                draw::victory_burst((0, y + 60, 0), age, signal);
                v.confetti.step((0, y + 80, 0));
            } else {
                draw::set_trophy(None);
            }
            let palette = [
                signal,
                (255, 255, 255),
                mix(signal, (255, 255, 255), 128),
                GOLD,
            ];
            for p in v.confetti.live() {
                let head = (p.p[0], p.p[1], p.p[2]);
                let tail = (
                    p.p[0] - p.v[0] * 3 / 16,
                    p.p[1] - p.v[1] * 3 / 16,
                    p.p[2] - p.v[2] * 3 / 16,
                );
                draw::victory_add(VictoryBit {
                    head,
                    tail,
                    tint: palette[p.tint as usize & 3],
                    radius: 0,
                });
            }
        } else {
            draw::set_trophy(None);
        }
        if v.winner().is_some() && !v.lost {
            // Flashes in the stands, on and off like cameras.
            for f in 0..4u32 {
                let period = 31 + 7 * f;
                let phase = (t + f * 13) % period;
                if phase < 3 {
                    let n = (t + f * 13) / period;
                    let side = if (n + f) & 1 == 0 { 1 } else { -1 };
                    let z = fx::scatter(f, n) as i32 * 2800 / 255 - 1400;
                    let y = 250 + fx::scatter(n, f + 5) as i32 * 400 / 255;
                    let head = (side * 3750, y, z);
                    draw::victory_add(VictoryBit {
                        head,
                        tail: head,
                        tint: (255, 250, 225),
                        radius: 110,
                    });
                }
            }
            // The corner lights breathe in the winner's colour.
            let pulse = 2048 + (psx_math::sincos::sin_q12((t * 56) as u16) as i32) / 2;
            for (cx, cz) in [(-1, 1), (1, 1)] {
                draw::victory_add(VictoryBit {
                    head: (cx * 3300, 700, cz * 4300),
                    tail: (0, 0, 0),
                    tint: fx::dim(signal, pulse * 255 / ONE),
                    radius: 420,
                });
            }
        }
    }

    /// The world the renderer draws while the results are up.
    pub(crate) fn victory_world(&self) -> Option<&Sim> {
        replay::world()
    }

    /// The panels over the scene.
    pub(crate) fn draw_results(&self, ctx: &mut Ctx, display: &FontAtlas, hud: &FontAtlas) {
        let Some(v) = self.victory.as_ref() else {
            return;
        };
        let gpu = Gpu::from_dma_mut(ctx.gpu_dma());
        let s = &self.sim;
        let (verdict, colour) = match v.verdict {
            Verdict::Win(seat) if v.lost => {
                let _ = seat;
                ("DEFEAT", DEFEAT)
            }
            Verdict::Win(seat) if self.two_player => (
                if seat == 0 { "P1 WINS" } else { "P2 WINS" },
                seat_signal(seat),
            ),
            Verdict::Win(seat) => ("WINNER", seat_signal(seat)),
            Verdict::Draw => ("DRAW", DRAW),
        };

        // Header: the verdict and the score.
        let dx = self.slide(0, v.t);
        if dx < 330 {
            let (x, y, h) = (PX + dx, 12, 40);
            panel(gpu, x, y, PW, h);
            fx::slanted_gradient(
                gpu,
                (x + 8, y + 7, PW - SLANT - 16, 4),
                SLANT / 2,
                colour,
                mix(colour, (0, 0, 0), 120),
            );
            // The plate leans: a row `r` down it starts `SLANT * (h - r) / h` to the
            // right of its foot, so each line is centred on the middle of its own row.
            let mid_at = |r: i32| x + (PW - SLANT) / 2 + SLANT * (h - r) / h;
            let mid = mid_at(20);
            let vx = (mid - display.text_width(verdict) as i32 * 300 / 256 / 2) as i16;
            display.draw_text_scaled_q8(vx + 1, (y + 15) as i16, verdict, 300, 300, (0, 0, 0));
            display.draw_text_scaled_q8(vx, (y + 14) as i16, verdict, 300, 300, Self::ink(colour));
            let mut line = [0u8; 12];
            let n = score_line(&mut line, s.score_blue as u32, s.score_orange as u32);
            // SAFETY: ASCII.
            let text = unsafe { core::str::from_utf8_unchecked(&line[..n]) };
            // The score, and after a gap OVERTIME in its colour when the match went
            // there: one line, centred as a whole, so it stays inside the plate.
            let wa = hud.text_width(text) as i32;
            let wb = if s.overtime {
                hud.text_width("OVERTIME") as i32 + 8
            } else {
                0
            };
            let mid2 = mid_at(35);
            let start = mid2 - (wa + wb) / 2;
            hud.draw_text(
                start as i16,
                (y + 29) as i16,
                text,
                Self::ink((226, 232, 244)),
            );
            if s.overtime {
                hud.draw_text(
                    (start + wa + 8) as i16,
                    (y + 29) as i16,
                    "OVERTIME",
                    Self::ink(crate::OVERTIME_INK),
                );
            }
        }

        // The stats.
        let dx = self.slide(1, v.t);
        if dx < 330 {
            self.draw_stats(gpu, hud, PX + dx, 58, v);
        }

        // The buttons.
        let dx = self.slide(2, v.t);
        if dx < 330 {
            let (x, y) = (PX + dx, 184);
            panel(gpu, x, y, PW, 48);
            let elapsed = v.t.wrapping_sub(v.moved_at);
            let labels = ["REMATCH", "QUIT TO MENU"];
            let step = 21;
            let top = y + 5;
            for i in 0..2 {
                plate(gpu, x, top + i * step, step - 3);
            }
            let k = fx::ease_out(draw::menu_sweep(elapsed));
            let (y0, y1) = (top + v.prev as i32 * step, top + v.row as i32 * step);
            // Dim until the buttons answer.
            let live = v.t >= LOCK;
            if live {
                bar(
                    gpu,
                    x,
                    y0 + ((y1 - y0) * k >> 12),
                    step - 3,
                    draw::menu_sweep(elapsed),
                );
            }
            for (i, label) in labels.iter().enumerate() {
                text(
                    display,
                    x,
                    top + i as i32 * step,
                    label,
                    step - 3,
                    live && i == v.row,
                );
            }
        }
    }

    /// How far right of its place panel `i` is at tick `t`: all the way off the
    /// screen before its time, then easing in.
    fn slide(&self, i: usize, t: u32) -> i32 {
        let t = t as i32 - SLIDE_AT[i] as i32;
        if t < 0 {
            return 400;
        }
        let k = fx::ease_out(t * ONE / SLIDE_TICKS as i32);
        (ONE - k) * 170 >> 12
    }

    fn draw_stats(&self, gpu: &mut Gpu, hud: &FontAtlas, x: i32, y: i32, v: &Victory) {
        let rows = 6;
        let h = 18 + rows * 14 + 6;
        panel(gpu, x, y, PW, h);
        let [a, b] = [self.sim.stats.team[0], self.sim.stats.team[1]];
        let (l, r) = (x + 28, x + PW - SLANT - 22);
        // The two sides' tags over their columns.
        let names = ["P1", if self.two_player { "P2" } else { "CPU" }];
        for (seat, cx) in [(0usize, l), (1, r)] {
            let c = seat_signal(seat);
            let w = 34;
            fx::slanted_gradient(
                gpu,
                (cx - w / 2, y + 6, w, 11),
                SLANT / 2,
                c,
                mix(c, (0, 0, 0), 110),
            );
            let tx = cx - hud.text_width(names[seat]) as i32 / 2 + 1;
            hud.draw_text(tx as i16, (y + 7) as i16, names[seat], Self::ink(LIT));
        }
        let mid = x + PW / 2 + 2;
        let stats: [(&str, u32, u32, bool, u8); 6] = [
            (
                "GOALS",
                u32::from(self.sim.score_blue),
                u32::from(self.sim.score_orange),
                false,
                0,
            ),
            ("SHOTS", u32::from(a.shots), u32::from(b.shots), false, 0),
            ("SAVES", u32::from(a.saves), u32::from(b.saves), false, 0),
            ("BOOST USED", a.boost_pips(), b.boost_pips(), false, 0),
            ("TOP SPEED", a.top_kph(), b.top_kph(), false, 1),
            ("AIR TIME", a.air_secs(), b.air_secs(), true, 2),
        ];
        for (i, &(label, va, vb, tenths, suffix)) in stats.iter().enumerate() {
            let ry = y + 22 + i as i32 * 14;
            if i % 2 == 0 {
                fx::slanted(
                    gpu,
                    x + 8,
                    ry - 2,
                    PW - SLANT - 16,
                    12,
                    SLANT / 4,
                    (22, 27, 46),
                );
            }
            hud.draw_text(
                (mid - hud.text_width(label) as i32 / 2) as i16,
                ry as i16,
                label,
                Self::ink(IDLE),
            );
            let (mut da, mut db) = ([0u8; 12], [0u8; 12]);
            let na = value(&mut da, va, tenths, suffix);
            let nb = value(&mut db, vb, tenths, suffix);
            // SAFETY: ASCII.
            let (ta, tb) = unsafe {
                (
                    core::str::from_utf8_unchecked(&da[..na]),
                    core::str::from_utf8_unchecked(&db[..nb]),
                )
            };
            // The better value is lit, the other is dim.
            let (ca, cb) = if va > vb {
                (GOLD, (110, 120, 148))
            } else if vb > va {
                ((110, 120, 148), GOLD)
            } else {
                (LIT, LIT)
            };
            Self::shadowed(
                hud,
                (l - hud.text_width(ta) as i32 / 2) as i16,
                ry as i16,
                ta,
                ca,
            );
            Self::shadowed(
                hud,
                (r - hud.text_width(tb) as i32 / 2) as i16,
                ry as i16,
                tb,
                cb,
            );
        }
        let _ = v;
    }
}

/// "3 : 1" into `out`.
fn score_line(out: &mut [u8; 12], a: u32, b: u32) -> usize {
    let mut n = 0;
    let mut put = |text: &str| {
        for &c in text.as_bytes() {
            if n < out.len() {
                out[n] = c;
                n += 1;
            }
        }
    };
    let mut dec = [0u8; U32_DEC_MAX];
    put(u32_dec(&mut dec, a));
    put(" : ");
    put(u32_dec(&mut dec, b));
    n
}

/// A stat as text: whole, or as minutes and seconds.
fn value(out: &mut [u8; 12], v: u32, clock: bool, _suffix: u8) -> usize {
    let mut dec = [0u8; U32_DEC_MAX];
    let mut n = 0;
    let mut push = |c: u8, n: &mut usize| {
        if *n < out.len() {
            out[*n] = c;
            *n += 1;
        }
    };
    if clock {
        for &c in u32_dec(&mut dec, v / 60).as_bytes() {
            push(c, &mut n);
        }
        push(b':', &mut n);
        push(b'0' + (v % 60 / 10) as u8, &mut n);
        push(b'0' + (v % 10) as u8, &mut n);
    } else {
        for &c in u32_dec(&mut dec, v).as_bytes() {
            push(c, &mut n);
        }
    }
    n
}

/// The panel plate: a gradient body leaning like the menus' buttons, with a
/// pale line along its top. The menus' plate has a dark border quad under the
/// body as well; here it costs a whole frame (the results are drawn over a full
/// arena), so the body carries its own edge.
fn panel(gpu: &mut Gpu, x: i32, y: i32, w: i32, h: i32) {
    let w = w - SLANT;
    fx::slanted_gradient(gpu, (x, y, w, h), SLANT, (36, 44, 72), (16, 19, 31));
    fx::slanted(gpu, x, y, w, 2, SLANT, (92, 200, 242));
}

fn plate(gpu: &mut Gpu, px: i32, y: i32, h: i32) {
    let (x, w) = (px + 12, PW - 12 - 12 - SLANT);
    fx::slanted_gradient(gpu, (x, y, w, h), SLANT, (40, 46, 66), (24, 28, 44));
}

fn bar(gpu: &mut Gpu, px: i32, y: i32, h: i32, sweep: i32) {
    let (x, w) = (px + 12, PW - 12 - 12 - SLANT);
    fx::slanted_gradient(gpu, (x, y, w, h), SLANT, CYAN_HI, CYAN_LO);
    let at = x - 20 + ((w + 40) * sweep >> 12);
    let (l, r) = (at.max(x), (at + 18).min(x + w));
    if r > l {
        fx::slanted(gpu, l, y, r - l, h, SLANT, (214, 246, 255));
    }
    fx::slanted(gpu, x, y, 5, h, SLANT, NIB);
}

fn text(display: &FontAtlas, px: i32, y: i32, label: &str, h: i32, lit: bool) {
    let ink = if lit { LIT } else { IDLE };
    let (lx, ly) = ((px + 12 + 20) as i16, (y + (h - 8) / 2) as i16);
    let tint = (ink.0.div_ceil(2), ink.1.div_ceil(2), ink.2.div_ceil(2));
    display.draw_text_scaled_q8(lx + 1, ly + 1, label, 150, 150, (0, 0, 0));
    display.draw_text_scaled_q8(lx, ly, label, 150, 150, tint);
}
