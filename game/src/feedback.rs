// SPDX-License-Identifier: GPL-2.0-or-later
//! In-match feedback drawn over the finished frame: an arrow to the ball when it
//! is off screen and a drop line under it in the air, a boost trail in the car's
//! colour,
//! supersonic speed streaks, sparks on a wall, dust on a landing, a flash where a
//! pad was taken, the respawn count of a demolished car and the one minute
//! banner.
//!
//! Nothing here touches the sim: the cars' state is read after each tick
//! (`update`), and the particles live in world space in a small ring that
//! is projected through the same cameras the frame was drawn with (`draw`).

use nitroxide_sim::{Car, Sim, DEMO_RESPAWN, FP};
use psx_font::FontAtlas;
use psx_gpu::Gpu;

use crate::draw::{self, seat_signal, Viewport};
use crate::fx::{self, Rgb, ONE};
use crate::NitroXide;

/// Particles alive at once, over both views.
const PARTS: usize = 40;
/// Ticks the one minute banner stays up.
const BANNER_TICKS: u8 = 130;
/// Ticks of clock at which the banner fires: sixty seconds.
const MINUTE_TICKS: u32 = 60 * 60;
/// Ball height, uu, above which the drop line is drawn.
const DROP_HEIGHT: i32 = 260;
/// Lowest line, from the top of the screen, a ball arrow may be centred on in
/// the top view: the open scoreboard (18 lines) plus the arrow's half size.
const SCOREBOARD_CLEAR: i32 = 32;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The boost trail, in the car's own colour.
    Trail,
    Spark,
    Dust,
}

#[derive(Clone, Copy)]
struct Particle {
    /// Position, uu, Y up.
    p: (i32, i32, i32),
    /// Velocity, uu per tick, in 1/8 uu.
    v: (i16, i16, i16),
    life: u8,
    life0: u8,
    kind: Kind,
    seat: u8,
}

impl Particle {
    const DEAD: Particle = Particle {
        p: (0, 0, 0),
        v: (0, 0, 0),
        life: 0,
        life0: 1,
        kind: Kind::Spark,
        seat: 0,
    };
}

/// What the last tick left of each car, to see what changed.
#[derive(Clone, Copy)]
struct Memo {
    grounded: bool,
    vy: i32,
}

/// Everything the feedback layer remembers.
#[derive(Clone, Copy)]
pub struct Feedback {
    parts: [Particle; PARTS],
    next: usize,
    memo: [Memo; 2],
    tick: u32,
    banner: u8,
    warned: bool,
    /// How far the boost has widened the field of view, Q12.
    pub widen: i32,
}

impl Feedback {
    pub const fn new() -> Self {
        Self {
            parts: [Particle::DEAD; PARTS],
            next: 0,
            memo: [Memo {
                grounded: true,
                vy: 0,
            }; 2],
            tick: 0,
            banner: 0,
            warned: false,
            widen: 0,
        }
    }

    fn spawn(&mut self, particle: Particle) {
        self.parts[self.next] = particle;
        self.next = (self.next + 1) % PARTS;
    }

    /// Read the sim after a tick: events, and the particles those and the cars'
    /// own state call for.
    pub fn update(&mut self, sim: &Sim, two_player: bool) {
        self.tick = self.tick.wrapping_add(1);
        if self.banner > 0 {
            self.banner -= 1;
        }
        // The last minute of a timed match.
        let timed = matches!(sim.win_condition, nitroxide_sim::WinCondition::TimeLimit(_));
        if timed && !self.warned && sim.clock > 0 && u32::from(sim.clock) <= MINUTE_TICKS {
            // Only a match that began with more than a minute announces one.
            if let nitroxide_sim::WinCondition::TimeLimit(total) = sim.win_condition {
                if u32::from(total) > MINUTE_TICKS {
                    self.banner = BANNER_TICKS;
                }
            }
            self.warned = true;
        }
        if sim.clock as u32 > MINUTE_TICKS + 60 {
            self.warned = false;
        }

        // The field of view opens up while the single player's car is boosting.
        let target = if !two_player && sim.car.boosting {
            ONE
        } else {
            0
        };
        self.widen += (target - self.widen) / 6;

        for seat in 0..2 {
            let car = car_of(sim, seat);
            self.car_effects(seat, car);
        }
        // A pad taken is drawn by the renderer, in the world (draw.rs
        // `pad_bursts`), from the pad's own timer.
        self.advance();
    }

    fn car_effects(&mut self, seat: usize, car: &Car) {
        let memo = self.memo[seat];
        let pos = (car.p.x >> FP, car.p.y >> FP, car.p.z >> FP);
        let (_, up, fwd) = car.basis();
        let speed = car.v.len() * 60 >> FP;
        if !car.wrecked() {
            if car.boosting {
                // From the exhaust: just behind the rear bumper and low, near
                // the wheel axle, along the car's own axes so it stays at the
                // tail on a wall or in the air.
                let at =
                    |a: i32, f: i32, u: i32| a - (f * CAR_TAIL >> 12) - (u * EXHAUST_DROP >> 12);
                let p = (
                    at(pos.0, fwd.x, up.x),
                    at(pos.1, fwd.y, up.y),
                    at(pos.2, fwd.z, up.z),
                );
                // Each puff drifts back off the exhaust line, spreading
                // sideways into a plume instead of a column.
                let wobble =
                    |n: u32| (fx::scatter(self.tick, n * 7 + seat as u32) as i16 - 128) >> 3;
                self.spawn(Particle {
                    p,
                    v: (
                        (-(fwd.x >> 9)) as i16 + wobble(1),
                        (-(fwd.y >> 9)) as i16 + wobble(2).abs() / 4,
                        (-(fwd.z >> 9)) as i16 + wobble(3),
                    ),
                    life: 16,
                    life0: 16,
                    kind: Kind::Trail,
                    seat: seat as u8,
                });
            }
            // On a wall: sparks off the wheels, a couple of ticks apart.
            if car.grounded && up.y < 2900 && speed > 700 && self.tick % 2 == seat as u32 {
                let k = self.tick as i32;
                let jitter = |n: i32| (((k * 37 + n * 91) & 31) - 16) as i16;
                self.spawn(Particle {
                    p: (pos.0, pos.1 - 20, pos.2),
                    v: (
                        (up.x >> 10) as i16 + jitter(1),
                        (up.y >> 10) as i16 + 8,
                        (up.z >> 10) as i16 + jitter(2),
                    ),
                    life: 12,
                    life0: 12,
                    kind: Kind::Spark,
                    seat: seat as u8,
                });
            }
            // A landing hard enough to raise dust.
            if car.grounded && !memo.grounded && memo.vy < -320 {
                for i in 0..4 {
                    let a = (i * 1024) as u16;
                    self.spawn(Particle {
                        p: (pos.0, pos.1 - 24, pos.2),
                        v: (
                            (psx_math::sincos::sin_q12(a) >> 8) as i16,
                            6,
                            (psx_math::sincos::cos_q12(a) >> 8) as i16,
                        ),
                        life: 20,
                        life0: 20,
                        kind: Kind::Dust,
                        seat: seat as u8,
                    });
                }
            }
        }
        self.memo[seat].grounded = car.grounded;
        self.memo[seat].vy = car.v.y;
    }

    fn advance(&mut self) {
        for p in self.parts.iter_mut().filter(|p| p.life > 0) {
            p.life -= 1;
            p.p.0 += i32::from(p.v.0) / 8;
            p.p.1 += i32::from(p.v.1) / 8;
            p.p.2 += i32::from(p.v.2) / 8;
            match p.kind {
                Kind::Spark => p.v.1 -= 2,
                Kind::Dust => p.v.1 = (p.v.1 - 1).max(1),
                _ => {}
            }
        }
        // The renderer draws the exhaust puffs in the world, in the car's own
        // ordering table, so a puff behind a car sorts behind it.
        draw::clear_puffs();
        for p in self
            .parts
            .iter()
            .filter(|p| p.life > 0 && p.kind == Kind::Trail)
        {
            draw::add_puff(
                (p.p.0, p.p.1.max(6), p.p.2),
                i32::from(p.life) * 256 / i32::from(p.life0),
                usize::from(p.seat),
            );
        }
    }

    /// Forget everything: a new match.
    pub fn reset(&mut self) {
        *self = Self::new();
        draw::clear_puffs();
    }
}

/// How far behind the car's centre the exhaust is: the rear bumper (the
/// hitbox's half length) and a little past it, uu.
const CAR_TAIL: i32 = nitroxide_sim::CAR_HALF_L + 10;
/// How far below the hitbox centre, along the car's up, the exhaust sits, uu:
/// about the wheel axle (the centre is 28 above the ground at rest).
const EXHAUST_DROP: i32 = 12;

fn car_of(sim: &Sim, seat: usize) -> &Car {
    if seat == 0 {
        &sim.car
    } else {
        &sim.opponent
    }
}

// ---- drawing ---------------------------------------------------------------

impl NitroXide {
    /// Everything of the feedback layer that goes over the frame, for each view
    /// in `seats` (the viewport, and the seat looking through it).
    pub(crate) fn draw_feedback(
        &self,
        gpu: &mut Gpu,
        display: &FontAtlas,
        hud: &FontAtlas,
        seats: &[(Viewport, usize)],
    ) {
        let fb = &self.fb;
        let sim = &self.sim;
        for &(vp, seat) in seats {
            self.ball_pointer(gpu, seat);
            if car_of(sim, seat).supersonic && !car_of(sim, seat).wrecked() {
                speed_streaks(gpu, vp, fb.tick);
            }
            fb.draw_particles(gpu, seat);
            let car = car_of(sim, seat);
            if car.wrecked() {
                respawn_count(display, hud, vp, car);
            }
        }
        fb.draw_banner(gpu, display, seats.len() > 1);
        draw_overtime(gpu, display, hud, sim, seats.len() > 1);
    }

    /// The arrow to the ball at the edge of a view that is not looking at it,
    /// and the drop line down to the pitch when it is airborne.
    fn ball_pointer(&self, gpu: &mut Gpu, seat: usize) {
        let b = &self.sim.ball;
        let at = (b.p.x >> FP, b.p.y >> FP, b.p.z >> FP);
        let Some(p) = draw::project_seat(seat, at) else {
            return;
        };
        let vp = p.view;
        let (l, t) = (i32::from(vp.x) + 12, i32::from(vp.y) + 12);
        let (r, bt) = (
            i32::from(vp.x) + i32::from(vp.w) - 12,
            i32::from(vp.y) + i32::from(vp.h) - 12,
        );
        let on = p.depth > 20 && p.x >= l && p.x <= r && p.y >= t && p.y <= bt;
        if on {
            if at.1 > DROP_HEIGHT {
                if let Some(g) = draw::project_seat(seat, (at.0, 0, at.2)) {
                    if g.depth > 20 {
                        drop_line(gpu, (p.x, p.y), (g.x, g.y));
                    }
                }
            }
            return;
        }
        // Off screen: an arrow on the rim of the view, pointing out at it.
        let (cx, cy) = (
            i32::from(vp.x) + i32::from(vp.w) / 2,
            i32::from(vp.y) + i32::from(vp.h) / 2,
        );
        let (mut dx, mut dy) = p.dir;
        if p.depth <= 0 && dx == 0 && dy == 0 {
            dy = 1;
        }
        // Behind the camera the projection mirrors; the direction does not.
        if dx == 0 && dy == 0 {
            dx = 1;
        }
        let (hw, hh) = ((r - l) / 2, (bt - t) / 2);
        // Scale the direction until it touches the rim rectangle.
        let sx = if dx != 0 {
            hw * 1024 / dx.abs()
        } else {
            i32::MAX
        };
        let sy = if dy != 0 {
            hh * 1024 / dy.abs()
        } else {
            i32::MAX
        };
        let k = sx.min(sy);
        let (ex, ey) = (cx + (dx * k >> 10), cy + (dy * k >> 10));
        // The scoreboard hangs from the top of the screen (18 lines when
        // open): in the top view the arrow stays below it, so it never sits
        // on the clock. The lower view of a split has no board.
        let ey = if vp.y == 0 {
            ey.max(SCOREBOARD_CLEAR)
        } else {
            ey
        };
        arrow(gpu, (ex, ey), (dx, dy), self.fb.tick);
    }
}

/// A line of light from the ball down to the spot under it, with a ring there.
fn drop_line(gpu: &mut Gpu, from: (i32, i32), to: (i32, i32)) {
    fx::additive(gpu);
    let c = (120, 190, 230);
    // A dotted line: short dashes, so it reads as a guide not a wall.
    let steps = 6;
    for i in 1..steps {
        if i % 2 == 0 {
            continue;
        }
        let x = from.0 + (to.0 - from.0) * i / steps;
        let y = from.1 + (to.1 - from.1) * i / steps;
        fx::rect_blended(gpu, x, y, 1, 3, c);
    }
    fx::rect_blended(gpu, to.0 - 4, to.1, 9, 1, (150, 210, 245));
    fx::rect_blended(gpu, to.0 - 2, to.1 - 1, 5, 3, (90, 150, 190));
    fx::opaque(gpu);
}

/// A small arrowhead at `at` pointing along `dir`, pulsing gently.
fn arrow(gpu: &mut Gpu, at: (i32, i32), dir: (i32, i32), tick: u32) {
    let len = isqrt(dir.0 * dir.0 + dir.1 * dir.1).max(1);
    let (ux, uy) = (dir.0 * 64 / len, dir.1 * 64 / len);
    let pulse = ((tick >> 2) & 1) as i32;
    let s = 10 + pulse * 2;
    // Tip, and the two back corners, each in 1/64ths of the direction.
    let tip = (at.0 + ux * s / 64, at.1 + uy * s / 64);
    let (px, py) = (-uy, ux);
    let back = (at.0 - ux * s / 64, at.1 - uy * s / 64);
    let (a, b) = (
        (back.0 + px * s / 100, back.1 + py * s / 100),
        (back.0 - px * s / 100, back.1 - py * s / 100),
    );
    // Dark first so it holds on bright grass, then the gold.
    let dark = |q: (i32, i32), o: i32| (q.0 + o, q.1 + o);
    fx::quad(
        gpu,
        [dark(tip, 1), dark(tip, 1), dark(a, 1), dark(b, 1)],
        (10, 12, 22),
    );
    fx::quad(gpu, [tip, tip, a, b], (255, 222, 110));
}

fn isqrt(v: i32) -> i32 {
    psx_math::int32::isqrt_i32(v)
}

/// Speed lines at the rim of a view while the car is supersonic: short bright
/// streaks flowing out from the middle, different every other tick.
fn speed_streaks(gpu: &mut Gpu, vp: Viewport, tick: u32) {
    use psx_math::sincos::{cos_q12, sin_q12};
    let (cx, cy) = (
        i32::from(vp.x) + i32::from(vp.w) / 2,
        i32::from(vp.y) + i32::from(vp.h) / 2,
    );
    let (rx, ry) = (i32::from(vp.w) / 2, i32::from(vp.h) / 2);
    fx::additive(gpu);
    for i in 0..8u32 {
        let a = (fx::scatter(i, tick >> 1) * 16 + i * 512) as u16;
        let (s, c) = (sin_q12(a), cos_q12(a));
        let near = 62 + (fx::scatter(i + 9, tick >> 1) as i32 * 20 >> 8);
        let far = near + 14 + (fx::scatter(i + 3, tick >> 1) as i32 * 16 >> 8);
        let at = |r: i32| (cx + (s * rx * r >> 12) / 100, cy + (c * ry * r >> 12) / 100);
        let (n, f) = (at(near), at(far));
        fx::quad_blended(gpu, [n, (n.0 + 1, n.1), f, (f.0 + 1, f.1)], (120, 120, 140));
    }
    fx::opaque(gpu);
}

/// The countdown a demolished car's own view shows until it is back.
fn respawn_count(display: &FontAtlas, hud: &FontAtlas, vp: Viewport, car: &Car) {
    let secs = u32::from(car.demo_timer)
        .div_ceil(60)
        .min(u32::from(DEMO_RESPAWN) / 60);
    let mut dec = [0u8; psx_math::fmt::U32_DEC_MAX];
    let n = psx_math::fmt::u32_dec(&mut dec, secs);
    let cx = (vp.x + vp.w / 2) as i16;
    let cy = vp.y + vp.h / 2;
    NitroXide::centred(hud, cx, cy - 34, "RESPAWNING IN", (255, 190, 110));
    let q = 640u16;
    let w = (i32::from(display.text_width(n)) * i32::from(q) / 256) as i16;
    let x = cx - w / 2;
    display.draw_text_scaled_q8(x + 2, cy - 18 + 2, n, q, q, (0, 0, 0));
    display.draw_text_scaled_q8(x, cy - 18, n, q, q, (128, 128, 128));
}

impl Feedback {
    fn draw_particles(&self, gpu: &mut Gpu, seat: usize) {
        if self
            .parts
            .iter()
            .all(|p| p.life == 0 || p.kind == Kind::Trail)
        {
            return;
        }
        let plane = draw::plane();
        fx::additive(gpu);
        for p in self.parts.iter().filter(|p| p.life > 0) {
            let Some(s) = draw::project_seat(seat, p.p) else {
                continue;
            };
            if s.depth < 24 || s.depth > 7000 {
                continue;
            }
            let vp = s.view;
            if s.x < i32::from(vp.x) || s.x >= i32::from(vp.x) + i32::from(vp.w) {
                continue;
            }
            if s.y < i32::from(vp.y) || s.y >= i32::from(vp.y) + i32::from(vp.h) {
                continue;
            }
            // Fades out over its life.
            let a = i32::from(p.life) * 256 / i32::from(p.life0);
            let (size_uu, colour): (i32, Rgb) = match p.kind {
                // Drawn in the world by the renderer (`draw::add_puff`).
                Kind::Trail => continue,
                Kind::Spark => (7, (255, 170, 70)),
                Kind::Dust => (28 + (256 - a) / 6, (62, 56, 44)),
            };
            let px = (size_uu * plane / s.depth).clamp(1, 28);
            fx::rect_blended(gpu, s.x - px / 2, s.y - px / 2, px, px, fx::dim(colour, a));
        }
        fx::opaque(gpu);
    }

    fn draw_banner(&self, gpu: &mut Gpu, display: &FontAtlas, split: bool) {
        if self.banner == 0 {
            return;
        }
        let age = i32::from(BANNER_TICKS - self.banner);
        let slide = 10;
        let k = if age < slide {
            fx::ease_out(age * ONE / slide)
        } else if i32::from(self.banner) < slide {
            fx::ease_out(i32::from(self.banner) * ONE / slide)
        } else {
            ONE
        };
        let (w, h) = (150, 22);
        let y = if split { 70 } else { 100 };
        let x = 160 - w / 2;
        // Opens out from the middle.
        let ow = w * k >> 12;
        fx::slanted(gpu, 160 - ow / 2 - 3, y, ow, h, 6, (13, 20, 48));
        fx::slanted(gpu, 160 - ow / 2 - 3, y + h - 2, ow, 2, 6, (92, 200, 242));
        if k > 3000 {
            NitroXide::centred(display, 160, (y + 7) as i16, "1 MINUTE", (255, 232, 130));
        }
        let _ = x;
    }
}

impl NitroXide {
    /// The car-select turntable: a ring of light on the pitch under each staged
    /// car with a few brighter dashes that turn the way the car does.
    pub(crate) fn draw_turntable(&self, gpu: &mut Gpu, tick: u32) {
        use psx_math::sincos::{cos_q12, sin_q12};
        let solo = self.select_solo();
        let seats = if solo { 1 } else { 2 };
        const SEGS: i32 = 24;
        const RADIUS: i32 = 115;
        fx::additive(gpu);
        for seat in 0..seats {
            let cx = if solo {
                0
            } else {
                draw::stage_car_x(seat, true)
            };
            let turn = (tick / 3) as i32;
            let mut prev: Option<(i32, i32)> = None;
            let mut first = (0, 0);
            for i in 0..=SEGS {
                let a = (i % SEGS * 4096 / SEGS) as u16;
                let p = (
                    cx + ((sin_q12(a) * RADIUS) >> 12),
                    0,
                    draw::STAGE_Z + ((cos_q12(a) * RADIUS) >> 12),
                );
                let Some(s) = draw::project_seat(0, p) else {
                    return;
                };
                if s.depth < 20 {
                    prev = None;
                    continue;
                }
                let here = (s.x, s.y);
                if i == 0 {
                    first = here;
                }
                if let Some(from) = prev {
                    // Dashes of two segments in six, turning with the car.
                    let lit = (i + turn).rem_euclid(6) < 2;
                    let c = if lit { (150, 225, 255) } else { (52, 104, 136) };
                    fx::quad_blended(
                        gpu,
                        [from, (from.0 + 2, from.1), here, (here.0 + 2, here.1)],
                        c,
                    );
                    fx::quad_blended(
                        gpu,
                        [
                            (from.0, from.1 + 1),
                            (from.0 + 1, from.1 + 1),
                            (here.0, here.1 + 1),
                            (here.0 + 1, here.1 + 1),
                        ],
                        fx::dim(c, 160),
                    );
                }
                prev = Some(here);
            }
            let _ = first;
        }
        fx::opaque(gpu);
    }
}

/// The OVERTIME banner: a neutral navy slanted plate with a violet rule that
/// opens out from its middle when overtime kicks off, stays through the
/// countdown and a moment of play, and closes. Stateless: its age is read off
/// the sim's own countdown and overtime clock.
fn draw_overtime(gpu: &mut Gpu, display: &FontAtlas, hud: &FontAtlas, sim: &Sim, split: bool) {
    if !sim.overtime {
        return;
    }
    let hold = i32::from(nitroxide_sim::KICKOFF_HOLD_TICKS);
    let age = if sim.kickoff_hold > 0 {
        hold - i32::from(sim.kickoff_hold)
    } else {
        hold + sim.overtime_ticks.min(1000) as i32
    };
    let total = hold + 70;
    if age >= total {
        return;
    }
    let slide = 12;
    let k = if age < slide {
        fx::ease_out(age * ONE / slide)
    } else if total - age < slide {
        fx::ease_out((total - age) * ONE / slide)
    } else {
        ONE
    };
    let (w, h) = (176, 32);
    let y = if split { 94 } else { 134 };
    let ow = w * k >> 12;
    let x = 160 - ow / 2 - 3;
    fx::slanted(gpu, x, y, ow, h, 6, (13, 20, 48));
    fx::slanted(gpu, x, y + h - 2, ow, 2, 6, crate::OVERTIME_INK);
    fx::slanted(gpu, x, y, ow, 2, 6, crate::OVERTIME_INK);
    if k > 3000 {
        NitroXide::centred(display, 160, (y + 6) as i16, "OVERTIME", (255, 255, 255));
        NitroXide::centred(
            hud,
            160,
            (y + 20) as i16,
            "NEXT GOAL WINS",
            crate::OVERTIME_INK,
        );
    }
}
