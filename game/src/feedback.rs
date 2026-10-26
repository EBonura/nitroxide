// SPDX-License-Identifier: GPL-2.0-or-later
//! In-match feedback drawn over the finished frame: an arrow to the ball when it
//! is off screen and a drop line under it in the air, event tags (SHOT ON GOAL,
//! SAVE, EPIC SAVE, DEMOLITION, AERIAL GOAL), a boost trail in the car's colour,
//! supersonic speed streaks, sparks on a wall, dust on a landing, a flash where a
//! pad was taken, the respawn count of a demolished car and the one minute
//! banner.
//!
//! Nothing here touches the sim: the events are read off its state after each
//! tick (`update`), and the particles live in world space in a small ring that
//! is projected through the same cameras the frame was drawn with (`draw`).

use nitroxide_sim::{Car, Sim, Team, DEMO_RESPAWN, FP, GOAL_H, GOAL_HALF_W, HALF_Z};
use psx_font::FontAtlas;
use psx_gpu::Gpu;

use crate::draw::{self, seat_signal, Viewport};
use crate::fx::{self, Rgb, ONE};
use crate::NitroXide;

/// Particles alive at once, over both views.
const PARTS: usize = 40;
/// Event tags on screen at once.
const TAGS: usize = 3;
/// Ticks a tag stays up, and how long it takes to slide in and out.
const TAG_LIFE: u8 = 110;
const TAG_SLIDE: u8 = 8;
/// Ticks the one minute banner stays up.
const BANNER_TICKS: u8 = 130;
/// Ticks of clock at which the banner fires: sixty seconds.
const MINUTE_TICKS: u32 = 60 * 60;
/// The ball must be this fast off a touch, in uu per second, to be a shot.
const SHOT_SPEED: i32 = 950;
/// Ball height, uu, above which the drop line is drawn.
const DROP_HEIGHT: i32 = 260;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// The boost trail, in the car's own colour.
    Trail,
    Spark,
    Dust,
    /// A bright ring segment where a pad was taken.
    Ring,
    /// The pad's flash, one big soft square.
    Flash,
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

#[derive(Clone, Copy)]
struct Tag {
    text: &'static str,
    /// The team the tag is for, which is the colour it wears.
    team: u8,
    age: u8,
}

/// A shot the ball is on, for a save to answer.
#[derive(Clone, Copy)]
struct Shot {
    /// The seat that struck it.
    by: u8,
    age: u16,
}

/// What the last tick left of each car, to see what changed.
#[derive(Clone, Copy)]
struct Memo {
    grounded: bool,
    wrecked: bool,
    vy: i32,
}

/// Everything the feedback layer remembers.
#[derive(Clone, Copy)]
pub struct Feedback {
    parts: [Particle; PARTS],
    next: usize,
    tags: [Option<Tag>; TAGS],
    shot: Option<Shot>,
    shot_cool: u16,
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
            tags: [None; TAGS],
            shot: None,
            shot_cool: 0,
            memo: [Memo {
                grounded: true,
                wrecked: false,
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

    fn push_tag(&mut self, text: &'static str, team: u8) {
        // A newer tag takes the first free place, or the oldest one's.
        let slot = self
            .tags
            .iter()
            .position(Option::is_none)
            .unwrap_or_else(|| {
                (0..TAGS)
                    .max_by_key(|&i| self.tags[i].map_or(0, |t| t.age))
                    .unwrap_or(0)
            });
        self.tags[slot] = Some(Tag { text, team, age: 0 });
    }

    /// Read the sim after a tick: events, and the particles those and the cars'
    /// own state call for.
    pub fn update(&mut self, sim: &Sim, two_player: bool) {
        self.tick = self.tick.wrapping_add(1);
        self.shot_cool = self.shot_cool.saturating_sub(1);
        for tag in self.tags.iter_mut().flatten() {
            tag.age = tag.age.saturating_add(1);
        }
        for slot in self.tags.iter_mut() {
            if slot.is_some_and(|t| t.age > TAG_LIFE) {
                *slot = None;
            }
        }
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

        self.events(sim);
        for seat in 0..2 {
            let car = car_of(sim, seat);
            self.car_effects(seat, car);
        }
        if sim.pad_taken {
            self.pad_flash(sim);
        }
        self.advance();
    }

    fn events(&mut self, sim: &Sim) {
        // A car that has just been wrecked: the other team did it.
        for seat in 0..2 {
            let wrecked = car_of(sim, seat).wrecked();
            if wrecked && !self.memo[seat].wrecked {
                self.push_tag("DEMOLITION", 1 - seat as u8);
            }
            self.memo[seat].wrecked = wrecked;
        }
        // A goal: was it scored out of the air?
        if sim.goal_freeze == nitroxide_sim::GOAL_FREEZE_TICKS {
            let scorer = match sim.last_scorer {
                Team::Blue => 0,
                Team::Orange => 1,
            };
            if !car_of(sim, scorer).grounded && sim.ball.p.y >> FP > 280 {
                self.push_tag("AERIAL GOAL", scorer as u8);
            }
            self.shot = None;
        }
        if let Some(shot) = &mut self.shot {
            shot.age += 1;
            if shot.age > 240 {
                self.shot = None;
            }
        }
        // A touch: who made it, and where the ball is going now.
        if sim.hit != 0 && sim.goal_freeze == 0 {
            let toucher = if dist2(sim.car.p, sim.ball.p) <= dist2(sim.opponent.p, sim.ball.p) {
                0
            } else {
                1
            };
            if let Some(shot) = self.shot {
                if usize::from(shot.by) != toucher {
                    let near = (HALF_Z - (sim.ball.p.z >> FP).abs()).abs() < 1000;
                    let fast = ball_speed(sim) > 1700;
                    self.push_tag(
                        if near || fast { "EPIC SAVE" } else { "SAVE" },
                        toucher as u8,
                    );
                    self.shot = None;
                    self.shot_cool = 90;
                    return;
                }
            }
            if self.shot_cool == 0 && heading_in(sim, toucher) {
                self.shot = Some(Shot {
                    by: toucher as u8,
                    age: 0,
                });
                self.shot_cool = 120;
                self.push_tag("SHOT ON GOAL", toucher as u8);
            } else if self.shot.is_some_and(|s| usize::from(s.by) == toucher) {
                // The same side touching it again: that shot is over.
                self.shot = None;
            }
        }
    }

    fn car_effects(&mut self, seat: usize, car: &Car) {
        let memo = self.memo[seat];
        let pos = (car.p.x >> FP, car.p.y >> FP, car.p.z >> FP);
        let (_, up, fwd) = car.basis();
        let speed = car.v.len() * 60 >> FP;
        if !car.wrecked() {
            if car.boosting {
                // From the tail, a little behind it.
                let back = |a: i32, f: i32| a - (f * CAR_TAIL >> 12);
                let p = (back(pos.0, fwd.x), back(pos.1, fwd.y), back(pos.2, fwd.z));
                self.spawn(Particle {
                    p,
                    v: (
                        (-(fwd.x >> 9)) as i16,
                        (-(fwd.y >> 9)) as i16,
                        (-(fwd.z >> 9)) as i16,
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

    fn pad_flash(&mut self, sim: &Sim) {
        // The pad that was just taken holds its full respawn time.
        let taken = nitroxide_sim::PADS
            .iter()
            .zip(sim.pad_timers.iter())
            .position(|(pad, t)| *t == if pad.big { 600 } else { 240 });
        let Some(i) = taken else { return };
        let pad = &nitroxide_sim::PADS[i];
        self.spawn(Particle {
            p: (pad.x, 40, pad.z),
            v: (0, 0, 0),
            life: 10,
            life0: 10,
            kind: Kind::Flash,
            seat: 0,
        });
        for k in 0..6 {
            let a = (k * 683) as u16;
            self.spawn(Particle {
                p: (pad.x, 30, pad.z),
                v: (
                    (psx_math::sincos::sin_q12(a) >> 7) as i16,
                    4,
                    (psx_math::sincos::cos_q12(a) >> 7) as i16,
                ),
                life: 12,
                life0: 12,
                kind: Kind::Ring,
                seat: 0,
            });
        }
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
    }

    /// Forget everything: a new match.
    pub fn reset(&mut self) {
        *self = Self::new();
    }
}

/// How far behind the car's centre the boost trail starts, uu.
const CAR_TAIL: i32 = 90;

fn car_of(sim: &Sim, seat: usize) -> &Car {
    if seat == 0 {
        &sim.car
    } else {
        &sim.opponent
    }
}

fn dist2(a: nitroxide_sim::V3, b: nitroxide_sim::V3) -> i32 {
    let (dx, dy, dz) = ((a.x - b.x) >> 4, (a.y - b.y) >> 4, (a.z - b.z) >> 4);
    dx.saturating_mul(dx)
        .saturating_add(dy.saturating_mul(dy))
        .saturating_add(dz.saturating_mul(dz))
}

/// The ball's speed, uu per second.
fn ball_speed(sim: &Sim) -> i32 {
    sim.ball.v.len() * 60 >> FP
}

/// Is the ball, as it leaves a touch by `seat`, bound for that seat's
/// opponent's goal, fast enough and on target?
fn heading_in(sim: &Sim, seat: usize) -> bool {
    let b = &sim.ball;
    // Seat 0 attacks +Z, seat 1 -Z.
    let goal_z = if seat == 0 { HALF_Z } else { -HALF_Z };
    let (bx, by, bz) = (b.p.x >> FP, b.p.y >> FP, b.p.z >> FP);
    let dz = goal_z - bz;
    if dz.signum() != b.v.z.signum() || b.v.z == 0 {
        return false;
    }
    // Per tick in 1/64 uu, so a thousand uu a second is about 1070.
    if (b.v.z.abs() * 60 >> FP) < SHOT_SPEED {
        return false;
    }
    let ticks = ((dz << FP) / b.v.z).clamp(1, 240);
    let x = bx + ((b.v.x * ticks) >> FP);
    // Gravity pulls it down by half g t squared on the way.
    let drop = (ticks * ticks * 104 / 9 / 2) >> FP;
    let y = by + ((b.v.y * ticks) >> FP) - drop;
    x.abs() < GOAL_HALF_W + 90 && y > 0 && y < GOAL_H + 140
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
        fb.draw_tags(gpu, hud, seats.len() > 1);
        fb.draw_banner(gpu, display, seats.len() > 1);
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
        if self.parts.iter().all(|p| p.life == 0) {
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
                Kind::Trail => (
                    14 + (256 - a) / 20,
                    fx::mix(seat_signal(usize::from(p.seat)), (255, 255, 255), 50),
                ),
                Kind::Spark => (7, (255, 170, 70)),
                Kind::Dust => (28 + (256 - a) / 6, (62, 56, 44)),
                Kind::Ring => (14, (255, 214, 110)),
                Kind::Flash => (110 + (256 - a) / 2, (150, 130, 80)),
            };
            let px = (size_uu * plane / s.depth).clamp(1, 28);
            let a = if p.kind == Kind::Trail { a * 3 / 5 } else { a };
            fx::rect_blended(gpu, s.x - px / 2, s.y - px / 2, px, px, fx::dim(colour, a));
        }
        fx::opaque(gpu);
    }

    fn draw_tags(&self, gpu: &mut Gpu, hud: &FontAtlas, split: bool) {
        let mut row = 0;
        for tag in self.tags.iter().flatten() {
            let tw = i32::from(hud.text_width(tag.text));
            let (w, h, slant) = (tw + 30, 14, 5);
            // In from the right, out the same way.
            let age = i32::from(tag.age);
            let life = i32::from(TAG_LIFE);
            let slide = i32::from(TAG_SLIDE);
            let k = if age < slide {
                fx::ease_out(age * ONE / slide)
            } else if age > life - slide {
                ONE - fx::ease_in((age - (life - slide)) * ONE / slide)
            } else {
                ONE
            };
            let target = 160 - w / 2;
            let x = target + ((320 - target) * (ONE - k) >> 12);
            let y = if split { 100 + row * 17 } else { 84 + row * 17 };
            row += 1;
            let team = seat_signal(usize::from(tag.team));
            fx::slanted(gpu, x, y, w, h, slant, (10, 12, 22));
            fx::slanted_gradient(
                gpu,
                (x + 1, y + 1, w - 2, h - 2),
                slant,
                team,
                fx::mix(team, (0, 0, 0), 110),
            );
            NitroXide::shadowed(
                hud,
                (x + 14) as i16,
                (y + 3) as i16,
                tag.text,
                (255, 255, 255),
            );
        }
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
