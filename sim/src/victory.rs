// SPDX-License-Identifier: GPL-2.0-or-later
//! The arithmetic of the victory scene: the orbiting camera, the trophy's rise
//! and the confetti. The game draws what this says, so all of it can be tested
//! on the host and a run of the same scene is the same on every run.

use psx_math::sincos::{cos_q12, sin_q12};

/// Where the winner stands, the middle of the pitch.
pub const SPOT: (i32, i32) = (0, 0);

/// One frame of the camera: the eye, what it looks at (world, Y up) and the
/// lens as a projection plane in pixels.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Shot {
    pub eye: (i32, i32, i32),
    pub target: (i32, i32, i32),
    pub plane: i32,
}

fn ease_out(t: i32, len: i32) -> i32 {
    // Q12 cubic ease-out over `len` ticks.
    let u = (4096 - (t.clamp(0, len) * 4096 / len)).max(0);
    4096 - (u * u >> 12) * u / 4096
}

/// The camera `t` ticks into the scene. It swings round the winner a turn every
/// twenty seconds, always the same way, starting wide and low and pushing in
/// over a second and a half, and is aimed by the exact look-at so the car and
/// what is above it stay put on the screen however the eye moves. The aim is
/// shifted a little along the view's right so the car stands left of centre and
/// leaves the right of the screen for the results panels.
pub fn orbit(t: u32, panels: bool) -> Shot {
    let t = t as i32;
    let k = ease_out(t, 90);
    let radius = 900 + (400 - 900) * k / 4096;
    let height = 90 + (215 - 90) * k / 4096;
    let a = ((t * 4096 / 1200) + 700) as u16 as i32;
    let (s, c) = (sin_q12(a as u16), cos_q12(a as u16));
    let eye = (
        SPOT.0 + (s * radius >> 12),
        height,
        SPOT.1 + (c * radius >> 12),
    );
    // The view's right, on the ground plane, is perpendicular to the line from
    // the eye to the spot.
    let shift = if panels { 120 * k / 4096 } else { 0 };
    let target = (
        SPOT.0 + (c * shift >> 12) * -1,
        140,
        SPOT.1 + (s * shift >> 12),
    );
    Shot {
        eye,
        target,
        plane: 205,
    }
}

/// The trophy's height above the pitch (its foot) and its yaw, `t` ticks in.
/// It appears at tick `APPEAR` and floats up to its place over the next second,
/// then bobs. `None` before it appears.
pub const APPEAR: u32 = 18;

pub fn trophy(t: u32) -> Option<(i32, u16)> {
    let since = t.checked_sub(APPEAR)? as i32;
    let rise = ease_out(since, 60);
    let bob = sin_q12((since * 40) as u16) * 6 >> 12;
    Some((64 + 90 * rise / 4096 + bob, (since * 28) as u16))
}

/// How lit the trophy is, Q12, from nothing at its appearance.
pub fn trophy_fade(t: u32) -> i32 {
    ((t as i32 - APPEAR as i32) * 4096 / 24).clamp(0, 4096)
}

/// The winning car's yaw, a slow turn.
pub fn car_yaw(t: u32) -> u16 {
    (t * 9) as u16
}

/// Confetti pieces alive at once.
pub const PIECES: usize = 40;

/// One piece, in world units (uu) with velocity in sixteenths of a uu a tick.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct Piece {
    pub p: [i32; 3],
    pub v: [i32; 3],
    /// Which of the palette this piece wears, `0..4`.
    pub tint: u8,
}

/// The confetti: a burst from the trophy, then a steady fall over the middle of
/// the pitch. Every piece that lands is thrown again from the top, so there is
/// always rain and nothing grows.
#[derive(Copy, Clone, Debug)]
pub struct Confetti {
    pub pieces: [Piece; PIECES],
    rng: u32,
    ticks: u32,
}

impl Confetti {
    pub const fn new() -> Self {
        Self {
            pieces: [Piece {
                p: [0, -1000, 0],
                v: [0; 3],
                tint: 0,
            }; PIECES],
            rng: 0x1c0f_f1e5,
            ticks: 0,
        }
    }

    fn rand(&mut self, range: i32) -> i32 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.rng = x;
        (x >> 8) as i32 % range.max(1)
    }

    /// Throw piece `i` from `origin` upward and outward.
    fn burst(&mut self, i: usize, origin: (i32, i32, i32)) {
        let a = self.rand(4096) as u16;
        let out = 30 + self.rand(70);
        let up = 130 + self.rand(120);
        let (s, c) = (sin_q12(a), cos_q12(a));
        self.pieces[i] = Piece {
            p: [origin.0, origin.1, origin.2],
            v: [s * out >> 12, up, c * out >> 12],
            tint: self.rand(4) as u8,
        };
    }

    /// Drop piece `i` in from above the middle of the pitch.
    fn rain(&mut self, i: usize) {
        let (x, z) = (self.rand(1500) - 750, self.rand(1500) - 750);
        let y = 520 + self.rand(260);
        self.pieces[i] = Piece {
            p: [x, y, z],
            v: [
                self.rand(24) - 12,
                -(20 + self.rand(30)),
                self.rand(24) - 12,
            ],
            tint: self.rand(4) as u8,
        };
    }

    /// One tick. `origin` is where the trophy is, for the opening burst.
    pub fn step(&mut self, origin: (i32, i32, i32)) {
        self.ticks += 1;
        for i in 0..PIECES {
            let landed = self.pieces[i].p[1] < 0;
            // The burst fires over the trophy's first moments, a few a tick.
            let due = self.ticks >= APPEAR && self.ticks < APPEAR + 24 && {
                let phase = (self.ticks - APPEAR) as usize % 24;
                i % 24 == phase || (i + 12) % 24 == phase
            };
            if due {
                self.burst(i, origin);
                continue;
            }
            if landed {
                // After the burst they fall again from the top, a few at a time
                // so the rain starts light and thickens.
                if self.ticks > APPEAR + 30 && self.rand(40) == 0 {
                    self.rain(i);
                } else {
                    self.pieces[i].p[1] = -1000;
                }
                continue;
            }
            let p = &mut self.pieces[i];
            // Gravity small, drag strong: paper.
            p.v[1] -= 3;
            for axis in 0..3 {
                p.v[axis] -= p.v[axis] / 24;
            }
            for axis in 0..3 {
                p.p[axis] += p.v[axis] / 16;
            }
        }
    }

    /// The pieces in flight.
    pub fn live(&self) -> impl Iterator<Item = &Piece> {
        self.pieces.iter().filter(|p| p.p[1] >= 0)
    }
}

impl Default for Confetti {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_camera_stays_round_the_winner_and_off_the_walls() {
        let mut last = orbit(0, true);
        for t in 1..4000 {
            let s = orbit(t, true);
            let r = isqrt((s.eye.0 - SPOT.0).pow(2) / 16 + (s.eye.2 - SPOT.1).pow(2) / 16) * 4;
            assert!((330..=1000).contains(&r), "radius {r} at {t}");
            assert!(s.eye.0.abs() < crate::HALF_X - 300 && s.eye.2.abs() < crate::HALF_Z - 300);
            assert!((60..=240).contains(&s.eye.1));
            // No jumps: a smooth swing.
            let d = (s.eye.0 - last.eye.0).abs() + (s.eye.2 - last.eye.2).abs();
            assert!(d <= 40, "jump of {d} at {t}");
            last = s;
        }
    }

    fn isqrt(v: i32) -> i32 {
        psx_math::int32::isqrt_i32(v)
    }

    #[test]
    fn the_trophy_rises_after_the_car_and_is_lit_with_it() {
        assert_eq!(trophy(APPEAR - 1), None);
        let (first, _) = trophy(APPEAR).unwrap();
        let (high, _) = trophy(APPEAR + 90).unwrap();
        assert!(high > first + 60, "{first} {high}");
        assert_eq!(trophy_fade(APPEAR), 0);
        assert_eq!(trophy_fade(APPEAR + 40), 4096);
    }

    #[test]
    fn confetti_bursts_then_rains_and_never_leaves_the_arena() {
        let mut c = Confetti::new();
        let mut max_live = 0;
        for t in 0..1500 {
            c.step((0, 250, 0));
            let live = c.live().count();
            max_live = max_live.max(live);
            for p in c.live() {
                assert!(
                    p.p[0].abs() < 2500 && p.p[2].abs() < 2500 && p.p[1] < 1800,
                    "{p:?} at {t}"
                );
            }
        }
        assert!(max_live > 20, "{max_live}");
        // Still raining a long time in.
        assert!(c.live().count() > 3);
        // The same scene is the same every run.
        let mut d = Confetti::new();
        for _ in 0..1500 {
            d.step((0, 250, 0));
        }
        assert_eq!(c.pieces, d.pieces);
    }
}
