// SPDX-License-Identifier: GPL-2.0-or-later
//! Three candidate voicings for each of the eight sounds that were sent back,
//! kept behind `pick` so choosing one is a single entry in `PICKS`.
//!
//! These are drafts for a listening pass (`sfx-synth variants <dir>`); nothing
//! here is in the bank until a letter is written into `PICKS` and `make sfx`
//! re-renders the file. Every generator is deterministic.
//!
//! The crowd, roar and gasp are layers of formant-filtered noise (and, for the
//! cheers, voiced stacks built from harmonics under a formant envelope), each
//! with its own slow random swell, put through a Schroeder reverb for the
//! stadium. The loops are built so the waveform is periodic in the loop length:
//! filters and the reverb run over three copies and the middle one is kept,
//! and every modulator has a whole number of cycles in the loop.

use super::*;

/// The chosen voicing per sound: `("whistle", 'b')`. Empty means every sound
/// keeps its original generator, which is what the committed files are.
pub const PICKS: &[(&str, char)] = &[("jump", 'a')];

/// The voicing picked for `name`, if any.
pub fn pick(name: &str) -> Option<char> {
    PICKS.iter().find(|(n, _)| *n == name).map(|(_, v)| *v)
}

/// The eight sounds and, for each, whether it loops.
pub const SOUNDS: [(&str, u32, bool); 8] = [
    ("whistle", 22_050, false),
    ("crowd_loop", 16_000, true),
    ("crowd_roar", 16_000, false),
    ("crowd_gasp", 16_000, false),
    ("engine_loop", 22_050, true),
    ("skid_loop", 22_050, true),
    ("jump", 22_050, false),
    ("flip", 22_050, false),
];

/// RMS (dBFS, decoded) of the sounds being replaced: the levels in
/// `game/src/audio.rs` were set from these, so a variant is scaled to match
/// where the peak allows and the play volume does not have to move.
pub fn target_rms_db(name: &str) -> f64 {
    match name {
        "whistle" => -13.1,
        "crowd_loop" => -13.0,
        "crowd_roar" => -15.7,
        "crowd_gasp" => -14.9,
        "engine_loop" => -6.0,
        "skid_loop" => -12.6,
        "jump" => -13.5,
        _ => -14.1,
    }
}

/// Render variant `v` of `name` at `rate`, or `None` for an unknown pair.
pub fn render(name: &str, v: char, rate: u32) -> Option<Vec<f64>> {
    Some(match (name, v) {
        ("whistle", 'a') => whistle_a(rate),
        ("whistle", 'b') => whistle_b(rate),
        ("whistle", 'c') => whistle_c(rate),
        ("crowd_loop", 'a') => crowd_loop_a(rate),
        ("crowd_loop", 'b') => crowd_loop_b(rate),
        ("crowd_loop", 'c') => crowd_loop_c(rate),
        ("crowd_roar", 'a') => roar_a(rate),
        ("crowd_roar", 'b') => roar_b(rate),
        ("crowd_roar", 'c') => roar_c(rate),
        ("crowd_gasp", 'a') => gasp_a(rate),
        ("crowd_gasp", 'b') => gasp_b(rate),
        ("crowd_gasp", 'c') => gasp_c(rate),
        ("engine_loop", 'a') => engine_a(rate),
        ("engine_loop", 'b') => engine_b(rate),
        ("engine_loop", 'c') => engine_c(rate),
        ("skid_loop", 'a') => skid_a(rate),
        ("skid_loop", 'b') => skid_b(rate),
        ("skid_loop", 'c') => skid_c(rate),
        ("jump", 'a') => jump_a(rate),
        ("jump", 'b') => jump_b(rate),
        ("jump", 'c') => jump_c(rate),
        ("flip", 'a') => flip_a(rate),
        ("flip", 'b') => flip_b(rate),
        ("flip", 'c') => flip_c(rate),
        _ => return None,
    })
}

// ---- building blocks ----------------------------------------------------------

/// Run `f` over `x` three times and keep the middle pass: for a filter or a
/// reverb that settles, the result is periodic in `x.len()`.
fn periodic(x: &[f64], mut f: impl FnMut(f64) -> f64) -> Vec<f64> {
    let n = x.len();
    let mut out = Vec::with_capacity(n * 3);
    for _ in 0..3 {
        for &v in x {
            out.push(f(v));
        }
    }
    out[n..2 * n].to_vec()
}

fn peak_norm(v: &mut [f64]) {
    let m = v.iter().fold(1e-9f64, |m, x| m.max(x.abs()));
    v.iter_mut().for_each(|x| *x /= m);
}

/// Smooth noise, periodic in `len`, scaled to a peak of one. `cut` is its
/// bandwidth in Hz.
fn lfo(seed: u32, len: usize, rate: u32, cut: f64) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let x: Vec<f64> = (0..len).map(|_| rng.next()).collect();
    let (mut a, mut b) = (Lp::default(), Lp::default());
    let mut y = periodic(&x, |v| b.run(a.run(v, cut, rate), cut, rate));
    peak_norm(&mut y);
    y
}

/// A frequency rounded so it has a whole number of cycles in `secs`.
fn cyc(freq: f64, secs: f64) -> f64 {
    (freq * secs).round().max(1.0) / secs
}

struct Comb {
    buf: Vec<f64>,
    i: usize,
    fb: f64,
    damp: f64,
    lp: f64,
}
impl Comb {
    fn new(d: usize, fb: f64, damp: f64) -> Self {
        Comb { buf: vec![0.0; d.max(2)], i: 0, fb, damp, lp: 0.0 }
    }
    fn run(&mut self, x: f64) -> f64 {
        let y = self.buf[self.i];
        self.lp = y * (1.0 - self.damp) + self.lp * self.damp;
        self.buf[self.i] = x + self.lp * self.fb;
        self.i = (self.i + 1) % self.buf.len();
        y
    }
}
struct Allpass {
    buf: Vec<f64>,
    i: usize,
}
impl Allpass {
    fn run(&mut self, x: f64) -> f64 {
        let b = self.buf[self.i];
        let y = b - x;
        self.buf[self.i] = x + b * 0.5;
        self.i = (self.i + 1) % self.buf.len();
        y
    }
}

/// A Schroeder reverb: the distance of a stadium. `size` stretches the room,
/// `fb` is how long it rings, `wet` the share of reverb in the mix. With
/// `circular` the result is periodic in the input's length.
fn reverb(x: &[f64], rate: u32, size: f64, fb: f64, wet: f64, circular: bool) -> Vec<f64> {
    let d = |s: f64| (s * size * rate as f64) as usize;
    let mut combs = [
        Comb::new(d(0.0297), fb, 0.35),
        Comb::new(d(0.0371), fb, 0.35),
        Comb::new(d(0.0411), fb, 0.35),
        Comb::new(d(0.0437), fb, 0.35),
        Comb::new(d(0.0533), fb, 0.35),
        Comb::new(d(0.0617), fb, 0.35),
    ];
    let mut aps = [
        Allpass { buf: vec![0.0; (0.005 * rate as f64) as usize + 2], i: 0 },
        Allpass { buf: vec![0.0; (0.0017 * rate as f64) as usize + 2], i: 0 },
    ];
    let mut run = |v: f64| {
        let mut s = 0.0;
        for c in combs.iter_mut() {
            s += c.run(v);
        }
        s /= 3.0;
        for a in aps.iter_mut() {
            s = a.run(s);
        }
        v * (1.0 - wet) + s * wet
    };
    if circular {
        periodic(x, run)
    } else {
        x.iter().map(|v| run(*v)).collect()
    }
}

/// Formant weight at `f`: three resonance peaks of (centre, bandwidth).
fn formants(f: f64, form: &[(f64, f64); 3]) -> f64 {
    let g = [1.0, 0.7, 0.3];
    form.iter()
        .zip(g)
        .map(|((c, b), g)| g / (1.0 + ((f - c) / b).powi(2)))
        .sum()
}

/// A crowd of voiced throats: `voices` stacks of harmonics whose fundamentals
/// drift and vibrate, under a formant envelope that may move. `f0` is the
/// centre fundamental at time `t`, `form` the three formants at `t`.
#[allow(clippy::too_many_arguments)]
fn voiced(
    r: u32,
    secs: f64,
    seed: u32,
    voices: usize,
    f0: impl Fn(f64) -> f64,
    form: impl Fn(f64) -> [(f64, f64); 3],
    env: impl Fn(f64) -> f64,
    spread: f64,
) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let det: Vec<f64> = (0..voices).map(|_| 1.0 + spread * rng.next()).collect();
    let vib: Vec<(f64, f64)> = (0..voices)
        .map(|_| (4.5 + 2.0 * rng.next().abs(), PI * (rng.next() + 1.0)))
        .collect();
    let mut ph = vec![0.0f64; voices];
    let len = n(r, secs);
    let mut out = vec![0.0; len];
    for i in 0..len {
        let t = i as f64 / r as f64;
        let fm = form(t);
        let mut s = 0.0;
        for k in 0..voices {
            let f = f0(t) * det[k] * (1.0 + 0.015 * (2.0 * PI * vib[k].0 * t + vib[k].1).sin());
            ph[k] += 2.0 * PI * f / r as f64;
            let mut h = 1.0;
            while h * f < 4200.0 {
                s += formants(h * f, &fm) / h.powf(0.6) * (ph[k] * h).sin();
                h += 1.0;
            }
        }
        out[i] = s * env(t) / voices as f64;
    }
    out
}

/// A sparse crackle: `count` short bursts of band-passed noise at random
/// times over `len` samples, as hands and shouts.
fn bursts(r: u32, len: usize, seed: u32, count: usize, centre: f64, tau: f64, level: f64) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let mut out = vec![0.0; len];
    let mut svf = Svf::default();
    for _ in 0..count {
        let at = ((rng.next() * 0.5 + 0.5) * len as f64) as usize;
        let amp = level * (0.4 + 0.6 * (rng.next() * 0.5 + 0.5));
        let burst = n(r, tau * 8.0);
        for k in 0..burst {
            let t = k as f64 / r as f64;
            let v = svf.run(rng.next(), centre * (0.8 + 0.4 * (rng.next() * 0.5 + 0.5)), 1.6, r);
            out[(at + k) % len] += amp * (-t / tau).exp() * v;
        }
    }
    out
}

/// Layers of formant-filtered noise, each a "voice" with its own vowel and its
/// own slow swell. Periodic in `len` when `circular`.
struct Crowd {
    layers: usize,
    f1: (f64, f64),
    f2: (f64, f64),
    swell_hz: (f64, f64),
    depth: f64,
}

fn crowd_bed(r: u32, len: usize, seed: u32, c: &Crowd, circular: bool) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let mut out = vec![0.0; len];
    for k in 0..c.layers {
        let u = |rng: &mut Rng| rng.next() * 0.5 + 0.5;
        let f1 = c.f1.0 + (c.f1.1 - c.f1.0) * u(&mut rng);
        let f2 = c.f2.0 + (c.f2.1 - c.f2.0) * u(&mut rng);
        let sw = c.swell_hz.0 + (c.swell_hz.1 - c.swell_hz.0) * u(&mut rng);
        let swell = lfo(seed + 100 + k as u32, len, r, sw);
        let noise: Vec<f64> = (0..len).map(|_| rng.next()).collect();
        let (mut a, mut b) = (Svf::default(), Svf::default());
        let filtered = if circular {
            periodic(&noise, |v| a.run(v, f1, 2.2, r) + 0.6 * b.run(v, f2, 2.6, r))
        } else {
            noise.iter().map(|v| a.run(*v, f1, 2.2, r) + 0.6 * b.run(*v, f2, 2.6, r)).collect()
        };
        for i in 0..len {
            out[i] += filtered[i] * (1.0 - c.depth + c.depth * (0.5 + 0.5 * swell[i]));
        }
    }
    out
}

fn rumble(r: u32, len: usize, seed: u32, centre: f64, q: f64) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let noise: Vec<f64> = (0..len).map(|_| rng.next()).collect();
    let mut s = Svf::default();
    periodic(&noise, |v| s.run(v, centre, q, r))
}

fn add(dst: &mut [f64], src: &[f64], g: f64) {
    for (d, s) in dst.iter_mut().zip(src) {
        *d += g * s;
    }
}

// ---- whistle ------------------------------------------------------------------

/// A pea whistle blast: two close partials that beat at the pea's trill, the
/// trill itself as an amplitude flutter, a breathy burst on the onset and a
/// scoop up into the note.
#[allow(clippy::too_many_arguments)]
fn blast(
    r: u32,
    len: usize,
    t0: f64,
    dur: f64,
    f: f64,
    gap_hz: f64,
    flutter: f64,
    depth: f64,
    breath: f64,
    seed: u32,
) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let mut air = Svf::default();
    let mut air2 = Svf::default();
    let (mut p1, mut p2) = (0.0f64, 0.0f64);
    (0..len)
        .map(|i| {
            let t = i as f64 / r as f64 - t0;
            if t < 0.0 || t > dur + 0.06 {
                return 0.0;
            }
            let env = trap(t, 0.012, dur, 0.05);
            let scoop = 1.0 + 0.035 * (-t / 0.03).exp();
            let fall = 1.0 - 0.04 * ((t - (dur - 0.12)) / 0.12).clamp(0.0, 1.0);
            p1 += 2.0 * PI * f * scoop * fall / r as f64;
            p2 += 2.0 * PI * (f + gap_hz) * scoop * fall / r as f64;
            let pea = 1.0 - depth * 0.5 * (1.0 + (2.0 * PI * flutter * t).sin());
            let tone = (p1.sin() + p2.sin()) * 0.5 + 0.16 * ((2.0 * p1).sin() + (2.0 * p2).sin()) * 0.5;
            // The breath: loud for the first few hundredths, then a quiet hiss.
            let hiss = 0.25 + 1.6 * (-t / 0.04).exp();
            let noise = air.run(rng.next(), 3400.0, 5.0, r) + 0.6 * air2.run(rng.next(), 5200.0, 3.0, r);
            env * (pea * tone + breath * hiss * noise)
        })
        .collect()
}

fn whistle_a(r: u32) -> Vec<f64> {
    // Short, short, long: the full-time whistle.
    let len = n(r, 1.5);
    let mut out = vec![0.0; len];
    for (k, (t0, d)) in [(0.0, 0.2), (0.29, 0.2), (0.58, 0.78)].into_iter().enumerate() {
        add(&mut out, &blast(r, len, t0, d, 2860.0, 36.0, 34.0, 0.45, 0.22, 21 + k as u32), 1.0);
    }
    out
}

fn whistle_b(r: u32) -> Vec<f64> {
    // One long, lower blast: the pea rolls slower, the breath is heavier.
    let len = n(r, 1.5);
    blast(r, len, 0.0, 1.25, 2380.0, 31.0, 30.0, 0.55, 0.38, 31)
}

fn whistle_c(r: u32) -> Vec<f64> {
    // A sharp, high double blast: peep, peeeep.
    let len = n(r, 1.3);
    let mut out = vec![0.0; len];
    add(&mut out, &blast(r, len, 0.0, 0.17, 3350.0, 40.0, 40.0, 0.4, 0.3, 41), 1.0);
    add(&mut out, &blast(r, len, 0.27, 0.8, 3350.0, 42.0, 39.0, 0.4, 0.2, 42), 1.0);
    out
}

// ---- crowd loops --------------------------------------------------------------

fn crowd_loop_a(r: u32) -> Vec<f64> {
    // A big bowl: a dozen throats, long swells, a lot of room.
    let len = blocks(n(r, 2.4));
    let c = Crowd { layers: 12, f1: (300.0, 900.0), f2: (800.0, 2400.0), swell_hz: (0.4, 1.4), depth: 0.7 };
    let mut v = crowd_bed(r, len, 301, &c, true);
    add(&mut v, &rumble(r, len, 302, 180.0, 1.0), 1.4);
    let mut lp = Lp::default();
    let v = reverb(&v, r, 1.6, 0.84, 0.5, true);
    periodic(&v, |x| lp.run(x, 2600.0, r))
}

fn crowd_loop_b(r: u32) -> Vec<f64> {
    // Closer and keener: brighter vowels, quicker swells, hands and shouts.
    let len = blocks(n(r, 2.0));
    let c = Crowd { layers: 16, f1: (400.0, 1000.0), f2: (1500.0, 3200.0), swell_hz: (1.5, 4.0), depth: 0.6 };
    let mut v = crowd_bed(r, len, 311, &c, true);
    add(&mut v, &rumble(r, len, 312, 220.0, 1.0), 0.6);
    let claps = bursts(r, len, 313, 16, 1900.0, 0.004, 0.9);
    add(&mut v, &claps, 1.0);
    reverb(&v, r, 0.7, 0.8, 0.3, true)
}

fn crowd_loop_c(r: u32) -> Vec<f64> {
    // Far away: mostly low, a single slow swell per loop, a huge tail.
    let len = blocks(n(r, 3.0));
    let c = Crowd { layers: 10, f1: (250.0, 600.0), f2: (600.0, 1100.0), swell_hz: (0.3, 0.8), depth: 0.5 };
    let mut v = crowd_bed(r, len, 321, &c, true);
    add(&mut v, &rumble(r, len, 322, 120.0, 0.9), 2.2);
    add(&mut v, &rumble(r, len, 323, 80.0, 0.9), 1.5);
    let secs = len as f64 / r as f64;
    for (i, x) in v.iter_mut().enumerate() {
        let t = i as f64 / r as f64;
        *x *= 0.65 + 0.35 * (2.0 * PI * t / secs + 0.7).sin();
    }
    reverb(&v, r, 2.4, 0.88, 0.6, true)
}

// ---- roar and gasp ------------------------------------------------------------

/// Shared envelope for a swell: rises over `up`, holds to `hold`, falls over
/// `down`.
fn swell(t: f64, up: f64, hold: f64, down: f64) -> f64 {
    let rise = (t / up).clamp(0.0, 1.0);
    let rise = rise * rise * (3.0 - 2.0 * rise);
    let fall = ((t - hold) / down).clamp(0.0, 1.0);
    rise * (1.0 - fall * fall * (3.0 - 2.0 * fall))
}

fn gliding_crowd(r: u32, secs: f64, seed: u32, layers: usize, from: ([f64; 2], [f64; 2]), to: ([f64; 2], [f64; 2])) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let len = n(r, secs);
    let mut out = vec![0.0; len];
    for _ in 0..layers {
        let j = rng.next() * 0.5 + 0.5;
        let (mut a, mut b) = (Svf::default(), Svf::default());
        let sw = lfo(rng.0 + 7, len, r, 2.0);
        for i in 0..len {
            let u = (i as f64 / len as f64).clamp(0.0, 1.0);
            let f1 = (from.0[0] + (from.0[1] - from.0[0]) * j) * (1.0 - u) + (to.0[0] + (to.0[1] - to.0[0]) * j) * u;
            let f2 = (from.1[0] + (from.1[1] - from.1[0]) * j) * (1.0 - u) + (to.1[0] + (to.1[1] - to.1[0]) * j) * u;
            let x = rng.next();
            out[i] += (a.run(x, f1, 2.2, r) + 0.6 * b.run(x, f2, 2.6, r)) * (0.6 + 0.4 * (0.5 + 0.5 * sw[i]));
        }
    }
    out
}

fn roar_a(r: u32) -> Vec<f64> {
    // A goal cheer: many throats gliding from "ooh" to "aah" as the swell
    // rises, hands crackling through the top, the whole thing in a big room.
    let secs = 2.9;
    let mut v = gliding_crowd(r, secs, 401, 14, ([300.0, 450.0], [700.0, 900.0]), ([650.0, 850.0], [1100.0, 1500.0]));
    let len = v.len();
    let claps = bursts(r, len, 402, 70, 2000.0, 0.005, 0.7);
    for (i, x) in v.iter_mut().enumerate() {
        let t = i as f64 / r as f64;
        let e = swell(t, 0.85, 1.9, 0.9);
        *x = (*x + claps[i] * (t / secs)) * e;
    }
    reverb(&v, r, 1.4, 0.82, 0.4, false)
}

fn roar_b(r: u32) -> Vec<f64> {
    // An eruption: voiced shouts, a rising "aaah", fast attack.
    let secs = 2.7;
    let mut v = voiced(
        r,
        secs,
        411,
        9,
        |t| 215.0 * (1.0 + 0.12 * (1.0 - (-t / 0.45).exp())),
        |t| {
            let u = (t / 0.8).min(1.0);
            [(450.0 + 250.0 * u, 160.0), (900.0 + 250.0 * u, 220.0), (2500.0, 400.0)]
        },
        |t| swell(t, 0.32, 1.5, 1.1),
        0.22,
    );
    let mut rng = Rng::new(412);
    let (mut a, mut b) = (Svf::default(), Svf::default());
    for (i, x) in v.iter_mut().enumerate() {
        let t = i as f64 / r as f64;
        let w = rng.next();
        *x = *x * 0.9 + 0.55 * swell(t, 0.3, 1.5, 1.1) * (a.run(w, 700.0, 1.5, r) + 0.5 * b.run(w, 1800.0, 1.5, r)) * 0.4;
    }
    reverb(&v, r, 1.1, 0.8, 0.35, false)
}

fn roar_c(r: u32) -> Vec<f64> {
    // Roar and chant: a low rumble of a stand stamping, and one long whoop
    // climbing over it.
    let secs = 3.1;
    let len = n(r, secs);
    let rum = rumble(r, len, 421, 220.0, 0.8);
    let rum2 = rumble(r, len, 422, 520.0, 1.0);
    let whoop = voiced(
        r,
        secs,
        423,
        6,
        |t| 280.0 + 260.0 * ((t - 0.35) / 1.0).clamp(0.0, 1.0),
        |t| {
            let u = ((t - 0.35) / 1.0).clamp(0.0, 1.0);
            [(350.0 + 400.0 * u, 140.0), (800.0 + 400.0 * u, 200.0), (2400.0, 400.0)]
        },
        |t| swell(t - 0.3, 0.7, 1.4, 1.1) * 0.9,
        0.15,
    );
    let mut v = vec![0.0; len];
    for i in 0..len {
        let t = i as f64 / r as f64;
        let stomp = 0.7 + 0.3 * (2.0 * PI * 3.5 * t).sin();
        v[i] = (rum[i] * 1.6 + rum2[i] * 0.6) * swell(t, 1.1, 1.8, 1.1) * stomp;
    }
    add(&mut v, &whoop, 0.9);
    reverb(&v, r, 1.8, 0.85, 0.45, false)
}

fn gasp_a(r: u32) -> Vec<f64> {
    // "Ooh": a held, round swell, breathy with a hint of voice.
    let secs = 1.1;
    let mut v = gliding_crowd(r, secs, 501, 10, ([280.0, 350.0], [620.0, 760.0]), ([300.0, 380.0], [700.0, 850.0]));
    let voice = voiced(r, secs, 502, 4, |t| 230.0 - 25.0 * t, |_| [(320.0, 90.0), (750.0, 140.0), (2300.0, 300.0)], |_| 0.5, 0.2);
    add(&mut v, &voice, 2.0);
    for (i, x) in v.iter_mut().enumerate() {
        *x *= swell(i as f64 / r as f64, 0.38, 0.55, 0.5);
    }
    reverb(&v, r, 1.0, 0.78, 0.35, false)
}

fn gasp_b(r: u32) -> Vec<f64> {
    // A sharp in-breath: all air, the formant sweeping up as it comes in.
    let secs = 0.95;
    let mut v = gliding_crowd(r, secs, 511, 8, ([500.0, 650.0], [1000.0, 1300.0]), ([900.0, 1100.0], [2000.0, 2600.0]));
    for (i, x) in v.iter_mut().enumerate() {
        *x *= swell(i as f64 / r as f64, 0.16, 0.3, 0.55);
    }
    reverb(&v, r, 0.8, 0.75, 0.3, false)
}

fn gasp_c(r: u32) -> Vec<f64> {
    // "Ooo-OOH": two swells, the second bigger and rounder, with a tail.
    let secs = 1.5;
    let mut v = gliding_crowd(r, secs, 521, 12, ([300.0, 400.0], [650.0, 800.0]), ([450.0, 560.0], [800.0, 1000.0]));
    let voice = voiced(r, secs, 522, 5, |t| 210.0 + 40.0 * (t / 1.0).min(1.0), |t| {
        let u = (t / 0.9).min(1.0);
        [(320.0 + 180.0 * u, 100.0), (760.0 + 100.0 * u, 150.0), (2400.0, 300.0)]
    }, |_| 0.5, 0.2);
    add(&mut v, &voice, 1.6);
    for (i, x) in v.iter_mut().enumerate() {
        let t = i as f64 / r as f64;
        *x *= 0.45 * swell(t, 0.25, 0.3, 0.3) + swell(t - 0.35, 0.3, 0.5, 0.5);
    }
    reverb(&v, r, 1.4, 0.82, 0.4, false)
}

// ---- engine loops -------------------------------------------------------------

fn engine_a(_r: u32) -> Vec<f64> {
    // A turbine hum: twelve harmonics of 109 Hz phase-modulated at twice the
    // fundamental, a turbo whine high above, a low rumble underneath.
    let len = 2016;
    let c = 10.0;
    let rum = lfo(601, len, 22_050, 260.0);
    (0..len)
        .map(|i| {
            let u = i as f64 / len as f64;
            let fm = 0.9 * (2.0 * PI * 2.0 * c * u).sin();
            let mut v = 0.0;
            for h in 1..=12 {
                v += (2.0 * PI * c * h as f64 * u + fm / (h as f64).powf(0.3)).sin() / (h as f64).powf(0.9);
            }
            let whine = 0.11 * (2.0 * PI * 31.0 * c * u + 2.0 * (2.0 * PI * 3.0 * c * u).sin()).sin();
            soft(0.9 * (v * 0.9 + whine + 0.3 * rum[i]))
        })
        .collect()
}

fn engine_b(r: u32) -> Vec<f64> {
    // A rocket car: a lower, rawer note, a rotor chop six times a cycle and a
    // band of jet roar.
    let len = 4032;
    let c = 16.0;
    let mut rng = Rng::new(611);
    let noise: Vec<f64> = (0..len).map(|_| rng.next()).collect();
    let mut s = Svf::default();
    let jet = periodic(&noise, |v| s.run(v, 950.0, 0.9, r));
    (0..len)
        .map(|i| {
            let u = i as f64 / len as f64;
            let mut v = 0.0;
            for h in 1..=20 {
                v += (2.0 * PI * c * h as f64 * u).sin() / h as f64;
            }
            let chop = 1.0 + 0.32 * (2.0 * PI * 6.0 * c * u).cos();
            let jetg = 0.45 * (1.0 + 0.4 * (2.0 * PI * 3.0 * c * u).sin());
            soft(0.8 * (v * chop + jet[i] * jetg))
        })
        .collect()
}

fn engine_c(_r: u32) -> Vec<f64> {
    // A sci-fi electric drive: near-pure stacked fifths, detuned pairs that
    // beat once a loop, a ring-modulated shimmer and a thin whine.
    let len = 2016;
    let c = 14.0;
    (0..len)
        .map(|i| {
            let u = i as f64 / len as f64;
            let mut v = 0.0;
            for (h, a) in [(1.0, 1.0), (2.0, 0.6), (3.0, 0.45), (5.0, 0.3)] {
                v += a * (2.0 * PI * c * h * u).sin();
                v += a * 0.8 * (2.0 * PI * (c * h + 1.0) * u).sin();
            }
            v += 0.5 * (2.0 * PI * 1.5 * c * u).sin() + 0.3 * (2.0 * PI * 3.0 * c * u).sin();
            let ring = 1.0 + 0.3 * (2.0 * PI * 7.0 * c * u).sin();
            let whine = 0.18 * (2.0 * PI * 18.0 * c * u).sin() * (0.6 + 0.4 * (2.0 * PI * 2.0 * u).sin());
            soft(0.45 * v * ring + whine)
        })
        .collect()
}

// ---- skid loops ---------------------------------------------------------------

/// A tyre's squeal: noise through a very narrow band whose centre wobbles,
/// periodic in the loop.
#[allow(clippy::too_many_arguments)]
fn squeal(r: u32, len: usize, seed: u32, centre: f64, q: f64, wobble: f64, wob_hz: f64, noise_wob: f64) -> Vec<f64> {
    let secs = len as f64 / r as f64;
    let w = cyc(wob_hz, secs);
    let nw = lfo(seed + 1, len, r, 9.0);
    let mut rng = Rng::new(seed);
    let noise: Vec<f64> = (0..len).map(|_| rng.next()).collect();
    let mut s = Svf::default();
    let mut i = 0usize;
    periodic(&noise, |x| {
        let t = (i % len) as f64 / r as f64;
        let c = centre + wobble * (2.0 * PI * w * t).sin() + noise_wob * nw[i % len];
        i += 1;
        s.run(x, c, q, r)
    })
}

/// Gravel under the tyre: grains thrown at `density` a second through a low
/// pass, with a rumble under them.
fn gravel(r: u32, len: usize, seed: u32, density: f64, cutoff: f64) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let p = density / r as f64;
    let grains: Vec<f64> = (0..len)
        .map(|_| if (rng.next() * 0.5 + 0.5) < p { rng.next() * 3.0 } else { 0.0 })
        .collect();
    let (mut a, mut b) = (Lp::default(), Lp::default());
    periodic(&grains, |x| b.run(a.run(x, cutoff, r), cutoff, r))
}

fn skid_a(r: u32) -> Vec<f64> {
    let len = blocks(n(r, 0.9));
    let mut v = squeal(r, len, 701, 2100.0, 22.0, 150.0, 7.0, 40.0);
    add(&mut v, &squeal(r, len, 702, 3150.0, 26.0, 200.0, 7.0, 60.0), 0.45);
    peak_norm(&mut v);
    add(&mut v, &gravel(r, len, 703, 900.0, 1800.0), 0.35);
    add(&mut v, &rumble(r, len, 704, 110.0, 1.0), 0.25);
    v
}

fn skid_b(r: u32) -> Vec<f64> {
    // A heavy, lower squeal that chatters: stick and slip at 28 Hz.
    let len = blocks(n(r, 0.9));
    let mut v = squeal(r, len, 711, 1350.0, 18.0, 90.0, 5.0, 260.0);
    peak_norm(&mut v);
    let secs = len as f64 / r as f64;
    let chat = cyc(28.0, secs);
    for (i, x) in v.iter_mut().enumerate() {
        *x *= 0.62 + 0.38 * (2.0 * PI * chat * i as f64 / r as f64).sin();
    }
    add(&mut v, &gravel(r, len, 712, 2500.0, 2200.0), 0.5);
    add(&mut v, &rumble(r, len, 713, 90.0, 0.9), 0.4);
    v
}

fn skid_c(r: u32) -> Vec<f64> {
    // A shrill wail with a ghost octave above, light gravel, quick vibrato.
    let len = blocks(n(r, 0.9));
    let mut v = squeal(r, len, 721, 3100.0, 30.0, 120.0, 11.0, 30.0);
    add(&mut v, &squeal(r, len, 722, 6200.0, 40.0, 240.0, 11.0, 60.0), 0.3);
    peak_norm(&mut v);
    add(&mut v, &gravel(r, len, 723, 500.0, 3200.0), 0.2);
    add(&mut v, &rumble(r, len, 724, 130.0, 1.0), 0.12);
    v
}

// ---- jump and flip ------------------------------------------------------------

fn jump_a(r: u32) -> Vec<f64> {
    // A punchy hop: a thump that drops, a blip that climbs, a click on the front.
    let mut rng = Rng::new(801);
    let (mut thump_ph, mut blip_ph) = (0.0f64, 0.0f64);
    (0..n(r, 0.15))
        .map(|i| {
            let t = i as f64 / r as f64;
            thump_ph += 2.0 * PI * (60.0 + 110.0 * (-t / 0.03).exp()) / r as f64;
            blip_ph += 2.0 * PI * (500.0 * (2.9f64).powf((t / 0.07).min(1.0))) / r as f64;
            let click = pluck(t, 0.0003, 0.002) * rng.next();
            pluck(t, 0.001, 0.045) * thump_ph.sin() + 0.7 * pluck(t, 0.0008, 0.03) * blip_ph.sin() + 0.4 * click
        })
        .collect()
}

fn jump_b(r: u32) -> Vec<f64> {
    // A spring: a soft triangle wave leaping upward, a low thud under it.
    let mut ph = 0.0f64;
    (0..n(r, 0.2))
        .map(|i| {
            let t = i as f64 / r as f64;
            ph += 2.0 * PI * 220.0 * (1.0 + 2.2 * (1.0 - (-t / 0.05).exp())) / r as f64;
            let tri: f64 = (1..=7).step_by(2).map(|h| (ph * h as f64).sin() / (h * h) as f64).sum();
            pluck(t, 0.002, 0.09) * tri * 1.6 + 0.6 * pluck(t, 0.002, 0.04) * (2.0 * PI * 90.0 * t).sin()
        })
        .collect()
}

fn jump_c(r: u32) -> Vec<f64> {
    // A rocket-car hop: a puff of air rising through the band, a sub thump, a tick.
    let mut rng = Rng::new(821);
    let mut svf = Svf::default();
    (0..n(r, 0.18))
        .map(|i| {
            let t = i as f64 / r as f64;
            let centre = 600.0 * (4.7f64).powf((t / 0.12).min(1.0));
            let puff = pluck(t, 0.003, 0.05) * svf.run(rng.next(), centre, 1.6, r) * 1.3;
            let sub = pluck(t, 0.002, 0.06) * (2.0 * PI * 55.0 * t).sin();
            let tick = pluck(t, 0.0004, 0.012) * (2.0 * PI * 1200.0 * t).sin();
            puff + 0.9 * sub + 0.4 * tick
        })
        .collect()
}

fn flip_a(r: u32) -> Vec<f64> {
    // A whoosh that sweeps up: the flip going away from you.
    let mut rng = Rng::new(831);
    let mut svf = Svf::default();
    let mut ph = 0.0f64;
    (0..n(r, 0.34))
        .map(|i| {
            let t = i as f64 / r as f64;
            let u = t / 0.34;
            let centre = 500.0 * (7.2f64).powf(u);
            ph += 2.0 * PI * centre * 0.5 / r as f64;
            let env = (PI * u).sin().powf(1.3);
            env * (svf.run(rng.next(), centre, 2.6, r) * 1.1 + 0.25 * ph.sin())
        })
        .collect()
}

fn flip_b(r: u32) -> Vec<f64> {
    // A sweep that falls: it comes at you fast and swings away, with a thin
    // falling tone riding it.
    let mut rng = Rng::new(841);
    let mut svf = Svf::default();
    let mut ph = 0.0f64;
    (0..n(r, 0.38))
        .map(|i| {
            let t = i as f64 / r as f64;
            let u = t / 0.38;
            let centre = 3200.0 * (0.14f64).powf(u);
            ph += 2.0 * PI * (1800.0 * (0.33f64).powf(u)) / r as f64;
            let env = pluck(t, 0.02, 0.13);
            env * (svf.run(rng.next(), centre, 4.0, r) * 1.2 + 0.35 * ph.sin())
        })
        .collect()
}

fn flip_c(r: u32) -> Vec<f64> {
    // A pass-by: the band climbs then falls as the car spins past, with a
    // rotation flutter in it.
    let mut rng = Rng::new(851);
    let mut svf = Svf::default();
    let mut ph = 0.0f64;
    (0..n(r, 0.42))
        .map(|i| {
            let t = i as f64 / r as f64;
            let u = t / 0.42;
            let centre = 600.0 + 2300.0 * (1.0 - (2.0 * u - 1.0).powi(2)) + 400.0 * (1.0 - u);
            ph += 2.0 * PI * centre * 0.4 / r as f64;
            let env = (PI * u).sin().powf(1.2);
            let spin = 0.65 + 0.35 * (2.0 * PI * 22.0 * t).sin();
            env * spin * (svf.run(rng.next(), centre, 3.0, r) * 1.1 + 0.3 * ph.sin())
        })
        .collect()
}
