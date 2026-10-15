// SPDX-License-Identifier: GPL-2.0-or-later
//! Renders NitroXide's synthesised sound effects to `.psau` files.
//!
//! `cargo run --release -- <out dir> [preview dir]` rewrites every file this
//! tool owns in `<out dir>` (`game/assets/audio`) and prints one table row per
//! sound: rate, decoded samples, ADPCM bytes, peak and RMS of what the SPU will
//! decode. With a preview dir it also writes a 16-bit WAV of each decoded
//! sound, to listen to or to feed an analyser.
//!
//! Everything is plain arithmetic: sines, a xorshift noise source, one-pole and
//! state-variable filters, exponential and linear envelopes. The generators are
//! deterministic, so a re-run is byte-identical and a change to a sound shows
//! up as a diff.
//!
//! Loops (`Looping::Whole`) are built with an integer number of cycles in a
//! whole number of ADPCM blocks (28 samples), so the cook does not stretch
//! them and the seam is a continuation of the waveform, not a click.

use psx_audio_cook::{adpcm, cook, psau, wav, CookOptions, Looping, Wav};
use std::f64::consts::PI;
use std::fs;
use std::path::PathBuf;

mod import;
mod variants;

const BLOCK: usize = 28;

struct Rng(u32);
impl Rng {
    fn new(seed: u32) -> Self {
        Rng(seed.max(1))
    }
    /// White noise in -1..1.
    fn next(&mut self) -> f64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        (x as f64 / u32::MAX as f64) * 2.0 - 1.0
    }
}

/// Seconds to samples.
fn n(rate: u32, secs: f64) -> usize {
    (secs * rate as f64).round() as usize
}

/// Round a length up to whole ADPCM blocks.
fn blocks(len: usize) -> usize {
    len.div_ceil(BLOCK) * BLOCK
}

/// One-pole low-pass state.
#[derive(Clone, Copy, Default)]
struct Lp {
    y: f64,
}
impl Lp {
    fn run(&mut self, x: f64, cutoff: f64, rate: u32) -> f64 {
        let a = 1.0 - (-2.0 * PI * cutoff / rate as f64).exp();
        self.y += a * (x - self.y);
        self.y
    }
}

/// Chamberlin state-variable filter; `band` is the band-pass output.
#[derive(Clone, Copy, Default)]
struct Svf {
    low: f64,
    band: f64,
}
impl Svf {
    fn run(&mut self, x: f64, centre: f64, q: f64, rate: u32) -> f64 {
        let f = 2.0 * (PI * centre / rate as f64).sin();
        self.low += f * self.band;
        let high = x - self.low - self.band / q;
        self.band += f * high;
        self.band
    }
}

/// Attack-then-exponential-decay envelope value at time `t` seconds.
fn pluck(t: f64, attack: f64, tau: f64) -> f64 {
    if t < 0.0 {
        0.0
    } else if t < attack {
        t / attack
    } else {
        (-(t - attack) / tau).exp()
    }
}

/// Trapezoid: linear attack, flat, linear release, over `len` seconds.
fn trap(t: f64, attack: f64, len: f64, release: f64) -> f64 {
    if t < 0.0 || t > len {
        0.0
    } else if t < attack {
        t / attack
    } else if t > len - release {
        (len - t) / release
    } else {
        1.0
    }
}

fn soft(x: f64) -> f64 {
    x.tanh()
}

/// Sum of sines at `freq` times each ratio, with matching amplitudes.
fn partials(phase_t: f64, freq: f64, parts: &[(f64, f64)]) -> f64 {
    parts
        .iter()
        .map(|(r, a)| a * (2.0 * PI * freq * r * phase_t).sin())
        .sum()
}

// ---- the sounds -------------------------------------------------------------

fn ui_move(r: u32) -> Vec<f64> {
    (0..n(r, 0.06))
        .map(|i| {
            let t = i as f64 / r as f64;
            pluck(t, 0.001, 0.012) * partials(t, 1320.0, &[(1.0, 1.0), (2.0, 0.25)])
        })
        .collect()
}

fn ui_tick(r: u32) -> Vec<f64> {
    (0..n(r, 0.045))
        .map(|i| {
            let t = i as f64 / r as f64;
            pluck(t, 0.0006, 0.008) * partials(t, 880.0, &[(1.0, 1.0), (3.0, 0.3), (5.0, 0.12)])
        })
        .collect()
}

fn ui_select(r: u32) -> Vec<f64> {
    (0..n(r, 0.16))
        .map(|i| {
            let t = i as f64 / r as f64;
            let a = pluck(t, 0.002, 0.04) * partials(t, 880.0, &[(1.0, 1.0), (2.0, 0.22)]);
            let t2 = t - 0.055;
            let b = pluck(t2, 0.002, 0.05) * partials(t2.max(0.0), 1320.0, &[(1.0, 1.0), (2.0, 0.22)]);
            0.8 * a + b
        })
        .collect()
}

fn ui_back(r: u32) -> Vec<f64> {
    (0..n(r, 0.13))
        .map(|i| {
            let t = i as f64 / r as f64;
            let a = pluck(t, 0.002, 0.035) * partials(t, 1100.0, &[(1.0, 1.0), (2.0, 0.2)]);
            let t2 = t - 0.05;
            let b = pluck(t2, 0.002, 0.045) * partials(t2.max(0.0), 740.0, &[(1.0, 1.0), (2.0, 0.2)]);
            0.8 * a + b
        })
        .collect()
}

fn ui_deny(r: u32) -> Vec<f64> {
    let mut lp = Lp::default();
    (0..n(r, 0.23))
        .map(|i| {
            let t = i as f64 / r as f64;
            let gate = trap(t, 0.004, 0.085, 0.01) + trap(t - 0.12, 0.004, 0.085, 0.01);
            let f = if t < 0.11 { 170.0 } else { 150.0 };
            let raw = partials(t, f, &[(1.0, 1.0), (3.0, 0.45), (5.0, 0.25), (7.0, 0.12)]);
            gate * lp.run(raw, 1800.0, r)
        })
        .collect()
}

fn bell(t: f64, f: f64, tau: f64) -> f64 {
    if t < 0.0 {
        return 0.0;
    }
    pluck(t, 0.002, tau)
        * partials(t, f, &[(1.0, 1.0), (2.76, 0.18), (5.4, 0.06)])
}

fn ui_chime(r: u32) -> Vec<f64> {
    (0..n(r, 0.42))
        .map(|i| {
            let t = i as f64 / r as f64;
            bell(t, 1046.5, 0.09) + bell(t - 0.06, 1318.5, 0.09) + 1.1 * bell(t - 0.12, 1568.0, 0.13)
        })
        .collect()
}

fn ui_saved(r: u32) -> Vec<f64> {
    (0..n(r, 0.34))
        .map(|i| {
            let t = i as f64 / r as f64;
            bell(t, 1318.5, 0.07) + 1.15 * bell(t - 0.075, 1975.5, 0.12)
        })
        .collect()
}

fn sweep(r: u32, secs: f64, from: f64, to: f64, seed: u32) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let mut svf = Svf::default();
    let mut phase = 0.0f64;
    (0..n(r, secs))
        .map(|i| {
            let t = i as f64 / r as f64;
            let u = t / secs;
            let centre = from * (to / from).powf(u);
            let env = (PI * u).sin().powf(1.4);
            phase += 2.0 * PI * centre * 0.5 / r as f64;
            let noise = svf.run(rng.next(), centre, 3.5, r);
            env * (0.9 * noise + 0.35 * phase.sin())
        })
        .collect()
}

fn ui_open(r: u32) -> Vec<f64> {
    sweep(r, 0.17, 500.0, 2600.0, 11)
}

fn ui_close(r: u32) -> Vec<f64> {
    sweep(r, 0.15, 2600.0, 500.0, 12)
}

fn beep(r: u32) -> Vec<f64> {
    (0..n(r, 0.18))
        .map(|i| {
            let t = i as f64 / r as f64;
            trap(t, 0.004, 0.18, 0.03) * partials(t, 880.0, &[(1.0, 1.0), (3.0, 0.18), (5.0, 0.07)])
        })
        .collect()
}

fn go(r: u32) -> Vec<f64> {
    (0..n(r, 0.46))
        .map(|i| {
            let t = i as f64 / r as f64;
            trap(t, 0.004, 0.46, 0.17) * partials(t, 1760.0, &[(1.0, 1.0), (3.0, 0.16), (5.0, 0.06)])
        })
        .collect()
}

/// Three blasts of a pea whistle.
fn whistle(r: u32) -> Vec<f64> {
    let mut rng = Rng::new(21);
    let mut breath = Svf::default();
    let blasts = [(0.0, 0.2), (0.28, 0.2), (0.56, 0.78)];
    let mut ph = 0.0f64;
    (0..n(r, 1.42))
        .map(|i| {
            let t = i as f64 / r as f64;
            let gate: f64 = blasts
                .iter()
                .map(|(s, l)| trap(t - s, 0.02, *l, 0.04))
                .sum();
            // Slight pitch settle on each onset, and the pea's rattle as a
            // fast amplitude flutter.
            let f = 2850.0 + 90.0 * (2.0 * PI * 6.0 * t).sin();
            ph += 2.0 * PI * f / r as f64;
            let flutter = 0.72 + 0.28 * (2.0 * PI * 38.0 * t).sin();
            let air = breath.run(rng.next(), 3400.0, 6.0, r);
            gate * (flutter * (ph.sin() + 0.18 * (2.0 * ph).sin()) + 0.35 * air)
        })
        .collect()
}

/// An air horn: two stacked bright tones a fourth apart, onset bent up.
fn horn(r: u32) -> Vec<f64> {
    let len = 1.35;
    let notes = [(233.1, 1.0), (311.1, 0.85), (233.1 * 1.004, 0.6)];
    let mut ph = [0.0f64; 3];
    let mut lp = Lp::default();
    (0..n(r, len))
        .map(|i| {
            let t = i as f64 / r as f64;
            let bend = 1.0 - 0.07 * (-t / 0.03).exp();
            let env = trap(t, 0.03, len, 0.34);
            let mut s = 0.0;
            for (k, (f, a)) in notes.iter().enumerate() {
                ph[k] += 2.0 * PI * f * bend / r as f64;
                let mut v = 0.0;
                for h in 1..=9 {
                    v += (ph[k] * h as f64).sin() / h as f64;
                }
                s += a * v;
            }
            env * lp.run(soft(0.7 * s), 4200.0, r)
        })
        .collect()
}

/// Crowd noise: noise through two band-pass filters at voice formants, with a
/// slow flutter standing in for many people not breathing in step.
fn crowd(r: u32, len: usize, seed: u32, shape: impl Fn(f64) -> f64, circular: bool) -> Vec<f64> {
    let mut rng = Rng::new(seed);
    let noise: Vec<f64> = (0..len).map(|_| rng.next()).collect();
    let passes = if circular { 3 } else { 1 };
    let mut f1 = Svf::default();
    let mut f2 = Svf::default();
    let mut lp = Lp::default();
    let mut out = Vec::with_capacity(len * passes);
    for _ in 0..passes {
        for (i, x) in noise.iter().enumerate() {
            let t = i as f64 / r as f64;
            let a = f1.run(*x, 520.0, 1.6, r);
            let b = f2.run(*x, 1250.0, 1.8, r);
            let m = lp.run(a + 0.6 * b, 2600.0, r);
            let secs = len as f64 / r as f64;
            // Integer cycles over the length so the flutter loops.
            let flutter = 1.0
                + 0.18 * (2.0 * PI * (2.0 * secs).round() / secs * t).sin()
                + 0.12 * (2.0 * PI * (5.0 * secs).round() / secs * t + 1.3).sin();
            out.push(m * flutter * shape(t));
        }
    }
    if circular {
        out[len..2 * len].to_vec()
    } else {
        out
    }
}

fn roar(r: u32) -> Vec<f64> {
    let len = n(r, 2.5);
    crowd(r, len, 31, |t| trap(t, 0.55, 2.5, 1.3).powf(1.2), false)
}

fn gasp(r: u32) -> Vec<f64> {
    let len = n(r, 1.0);
    let mut rng = Rng::new(41);
    let mut svf = Svf::default();
    let mut lp = Lp::default();
    let _ = len;
    (0..len)
        .map(|i| {
            let t = i as f64 / r as f64;
            // "Ooh": the formant sinks while the swell rises and falls.
            let centre = 900.0 - 380.0 * (t / 1.0);
            let env = (PI * (t / 1.0).clamp(0.0, 1.0)).sin().powf(1.6);
            let x = svf.run(rng.next(), centre, 2.4, r);
            env * lp.run(x, 1800.0, r)
        })
        .collect()
}

fn crowd_loop(r: u32) -> Vec<f64> {
    let len = blocks(n(r, 1.6));
    crowd(r, len, 51, |_| 1.0, true)
}

/// A four-stroke-ish engine note: a stack of harmonics of `f0` where `f0` has a
/// whole number of cycles in the loop, with a little roughness from a second,
/// slightly detuned stack.
fn engine_loop(r: u32) -> Vec<f64> {
    let len = 2016; // 72 blocks
    let cycles = 10.0; // f0 = 10 cycles / (2016/22050 s) = 109.4 Hz
    (0..len)
        .map(|i| {
            let u = i as f64 / len as f64;
            let mut v = 0.0;
            for h in 1..=14 {
                let amp = 1.0 / (h as f64).powf(1.15);
                v += amp * (2.0 * PI * cycles * h as f64 * u).sin();
            }
            // Cylinder pulse: a periodic dip at twice the fundamental.
            let pulse = 1.0 + 0.35 * (2.0 * PI * cycles * 2.0 * u).cos();
            // Roughness: a second note two cycles per loop away from the first.
            let rough = 0.4 * (2.0 * PI * (cycles * 3.0 + 1.0) * u).sin();
            let _ = r;
            soft(0.9 * (v * pulse + rough))
        })
        .collect()
}

/// Tyre squeal: a narrow band of noise around a wobbling tone, looped.
fn skid_loop(r: u32) -> Vec<f64> {
    let len = blocks(n(r, 0.9));
    let secs = len as f64 / r as f64;
    let mut rng = Rng::new(61);
    let noise: Vec<f64> = (0..len).map(|_| rng.next()).collect();
    let mut svf = Svf::default();
    let mut out = Vec::new();
    for _ in 0..3 {
        for (i, x) in noise.iter().enumerate() {
            let t = i as f64 / r as f64;
            let wob = (2.0 * PI * (4.0 * secs).round() / secs * t).sin();
            let centre = 2300.0 + 260.0 * wob;
            let nz = svf.run(*x, centre, 7.0, r);
            // Integer-cycle tone (1100 Hz rounded to a whole count) with vibrato.
            let tone_hz = (1100.0 * secs).round() / secs;
            let tone = (2.0 * PI * tone_hz * t + 0.5 * wob).sin();
            out.push(0.8 * nz + 0.35 * tone);
        }
    }
    out[len..2 * len].to_vec()
}

fn jump(r: u32) -> Vec<f64> {
    let mut rng = Rng::new(71);
    let mut lp = Lp::default();
    let mut ph = 0.0f64;
    (0..n(r, 0.16))
        .map(|i| {
            let t = i as f64 / r as f64;
            let f = 110.0 + 160.0 * (-t / 0.035).exp();
            ph += 2.0 * PI * f / r as f64;
            let body = pluck(t, 0.002, 0.05) * ph.sin();
            let puff = pluck(t, 0.001, 0.02) * lp.run(rng.next(), 1400.0, r);
            body + 0.5 * puff
        })
        .collect()
}

fn flip(r: u32) -> Vec<f64> {
    let mut v = sweep(r, 0.3, 700.0, 3200.0, 81);
    let mut rng = Rng::new(82);
    let mut lp = Lp::default();
    for (i, x) in v.iter_mut().enumerate() {
        let t = i as f64 / r as f64;
        *x += 0.35 * pluck(t, 0.001, 0.02) * lp.run(rng.next(), 900.0, r);
    }
    v
}

fn land(r: u32) -> Vec<f64> {
    let mut rng = Rng::new(91);
    let mut lp = Lp::default();
    let mut ph = 0.0f64;
    (0..n(r, 0.2))
        .map(|i| {
            let t = i as f64 / r as f64;
            let f = 52.0 + 60.0 * (-t / 0.03).exp();
            ph += 2.0 * PI * f / r as f64;
            let thud = pluck(t, 0.002, 0.05) * ph.sin();
            let crunch = pluck(t, 0.001, 0.012) * lp.run(rng.next(), 1100.0, r);
            // The second bump: the suspension coming back up.
            let again = pluck(t - 0.07, 0.003, 0.04) * (2.0 * PI * 62.0 * (t - 0.07).max(0.0)).sin();
            thud + 0.6 * crunch + 0.35 * again
        })
        .collect()
}

fn bounce(r: u32) -> Vec<f64> {
    let mut rng = Rng::new(101);
    let mut lp = Lp::default();
    let mut ph = 0.0f64;
    (0..n(r, 0.24))
        .map(|i| {
            let t = i as f64 / r as f64;
            let f = 100.0 + 90.0 * (-t / 0.045).exp();
            ph += 2.0 * PI * f / r as f64;
            let body = pluck(t, 0.001, 0.07) * (ph.sin() + 0.3 * (2.0 * ph).sin());
            let slap = pluck(t, 0.0005, 0.008) * lp.run(rng.next(), 2600.0, r);
            body + 0.45 * slap
        })
        .collect()
}

fn net(r: u32) -> Vec<f64> {
    let mut rng = Rng::new(111);
    let mut lp = Lp::default();
    let mut lp2 = Lp::default();
    (0..n(r, 0.42))
        .map(|i| {
            let t = i as f64 / r as f64;
            let cut = 700.0 * (-t / 0.12).exp() + 140.0;
            let nz = lp2.run(lp.run(rng.next(), cut, r), cut, r);
            pluck(t, 0.004, 0.09) * (1.8 * nz + 0.5 * (2.0 * PI * 68.0 * t).sin())
        })
        .collect()
}

fn bump(r: u32) -> Vec<f64> {
    let mut rng = Rng::new(121);
    let mut svf = Svf::default();
    (0..n(r, 0.2))
        .map(|i| {
            let t = i as f64 / r as f64;
            let clank = pluck(t, 0.0007, 0.03) * svf.run(rng.next(), 900.0, 2.5, r);
            let tone = pluck(t, 0.001, 0.05) * partials(t, 140.0, &[(1.0, 1.0), (2.93, 0.35)]);
            0.9 * clank + tone
        })
        .collect()
}

fn sonic(r: u32) -> Vec<f64> {
    let mut rng = Rng::new(131);
    let mut svf = Svf::default();
    let mut ph = 0.0f64;
    (0..n(r, 0.75))
        .map(|i| {
            let t = i as f64 / r as f64;
            let u = t / 0.75;
            let centre = 350.0 * (9.0f64).powf(u.min(1.0));
            let air = svf.run(rng.next(), centre, 1.4, r);
            let f = 48.0 + 40.0 * (-t / 0.1).exp();
            ph += 2.0 * PI * f / r as f64;
            let boom = pluck(t, 0.002, 0.16) * ph.sin();
            // The crack on the front, then the hiss lifting away.
            pluck(t, 0.004, 0.22) * 1.3 * air + 1.2 * boom
        })
        .collect()
}

fn pad_big(r: u32) -> Vec<f64> {
    (0..n(r, 0.5))
        .map(|i| {
            let t = i as f64 / r as f64;
            let sub = pluck(t, 0.004, 0.18) * partials(t, 165.0, &[(1.0, 1.0)]);
            sub * 0.8 + bell(t, 660.0, 0.12) + 0.9 * bell(t - 0.07, 990.0, 0.14) + 0.8 * bell(t - 0.14, 1320.0, 0.2)
        })
        .collect()
}

fn empty(r: u32) -> Vec<f64> {
    let mut rng = Rng::new(141);
    let mut svf = Svf::default();
    (0..n(r, 0.16))
        .map(|i| {
            let t = i as f64 / r as f64;
            let puff = pluck(t, 0.0005, 0.012) * svf.run(rng.next(), 1700.0, 1.2, r)
                + 0.8 * pluck(t - 0.065, 0.0005, 0.016) * svf.run(rng.next(), 1300.0, 1.2, r);
            puff + 0.3 * pluck(t, 0.001, 0.03) * (2.0 * PI * 230.0 * t).sin()
        })
        .collect()
}

/// Sounds cut from recordings by `sfx-synth import` (see `import.rs`) rather than
/// synthesised here.
const IMPORTED: [&str; 4] = ["crowd_loop", "crowd_roar", "crowd_gasp", "skid_loop"];

struct Def {
    name: &'static str,
    rate: u32,
    looped: bool,
    gen: fn(u32) -> Vec<f64>,
    /// A voicing chosen in `variants::PICKS`, which replaces `gen`.
    variant: Option<char>,
}

const fn d(name: &'static str, rate: u32, gen: fn(u32) -> Vec<f64>) -> Def {
    Def { name, rate, looped: false, gen, variant: None }
}
const fn dl(name: &'static str, rate: u32, gen: fn(u32) -> Vec<f64>) -> Def {
    Def { name, rate, looped: true, gen, variant: None }
}

fn defs() -> Vec<Def> {
    let all = vec![
        d("ui_move", 22050, ui_move),
        d("ui_tick", 22050, ui_tick),
        d("ui_select", 22050, ui_select),
        d("ui_back", 22050, ui_back),
        d("ui_deny", 22050, ui_deny),
        d("ui_chime", 22050, ui_chime),
        d("ui_saved", 22050, ui_saved),
        d("ui_open", 22050, ui_open),
        d("ui_close", 22050, ui_close),
        d("beep", 22050, beep),
        d("go", 22050, go),
        d("whistle", 22050, whistle),
        d("horn", 22050, horn),
        d("crowd_roar", 16000, roar),
        d("crowd_gasp", 16000, gasp),
        dl("crowd_loop", 16000, crowd_loop),
        dl("engine_loop", 22050, engine_loop),
        dl("skid_loop", 22050, skid_loop),
        d("jump", 22050, jump),
        d("flip", 22050, flip),
        d("land", 22050, land),
        d("bounce", 22050, bounce),
        d("net", 22050, net),
        d("bump", 22050, bump),
        d("sonic", 22050, sonic),
        d("pad_big", 22050, pad_big),
        d("empty", 22050, empty),
    ];
    all.into_iter()
        .filter(|def| !IMPORTED.contains(&def.name))
        .map(|mut def| {
            def.variant = variants::pick(def.name);
            def
        })
        .collect()
}

fn db(x: f64) -> f64 {
    20.0 * x.max(1e-9).log10()
}

/// One sound, rendered and cooked: its `.psau` bytes and the ADPCM as the SPU
/// will decode it.
struct Rendered {
    psau: Vec<u8>,
    decoded: Vec<i16>,
    adpcm_len: usize,
}

fn render(def: &Def) -> Rendered {
    if let Some(v) = def.variant {
        return render_variant(def.name, v);
    }
    let mut pcm = (def.gen)(def.rate);
    if def.looped {
        assert_eq!(pcm.len() % BLOCK, 0, "{} loop is not whole blocks", def.name);
    }
    // Scale into 16-bit range; the cook normalises to its target peak.
    for v in pcm.iter_mut() {
        *v *= 20_000.0;
    }
    let src = Wav {
        rate: def.rate,
        samples: pcm,
        loop_start: None,
        loop_end: None,
        bits: 16,
    };
    let mut opts = CookOptions::one_shot(def.rate);
    if def.looped {
        opts.looping = Looping::Whole;
    }
    // Aim at 0.85 of full scale and back off until the decoded ADPCM, which can
    // overshoot its source by a step, stays clear of the rail.
    let mut target = 0.85;
    let (c, decoded) = loop {
        opts.normalize_peak = Some(target);
        let c = cook(&src, &opts);
        let decoded = adpcm::decode(&c.adpcm);
        let peak = decoded.iter().map(|v| (*v as i32).abs()).max().unwrap_or(0);
        if peak < 32_700 {
            break (c, decoded);
        }
        target *= 0.94;
    };
    Rendered {
        psau: psau(def.rate, decoded.len(), &c.adpcm),
        adpcm_len: c.adpcm.len(),
        decoded,
    }
}

/// Cook variant `v` of `name`: scaled to the RMS of the sound it replaces, so
/// the play volume in the game need not move, and backed off only as far as
/// the rail demands.
fn render_variant(name: &str, v: char) -> Rendered {
    let (_, rate, looped) = *variants::SOUNDS.iter().find(|s| s.0 == name).expect("a sound with variants");
    let mut pcm = variants::render(name, v, rate).expect("a known variant");
    if looped {
        assert_eq!(pcm.len() % BLOCK, 0, "{name}-{v} loop is not whole blocks");
    }
    let rms = (pcm.iter().map(|x| x * x).sum::<f64>() / pcm.len() as f64).sqrt().max(1e-9);
    let mut gain = 10f64.powf(variants::target_rms_db(name) / 20.0) * 32768.0 / rms;
    let mut opts = CookOptions::one_shot(rate);
    opts.normalize_peak = None;
    if looped {
        opts.looping = Looping::Whole;
    }
    loop {
        let src = Wav {
            rate,
            samples: pcm.iter().map(|x| x * gain).collect(),
            loop_start: None,
            loop_end: None,
            bits: 16,
        };
        let c = cook(&src, &opts);
        let decoded = adpcm::decode(&c.adpcm);
        let peak = decoded.iter().map(|x| (*x as i32).abs()).max().unwrap_or(0);
        if peak < 32_000 {
            pcm.clear();
            return Rendered {
                psau: psau(rate, decoded.len(), &c.adpcm),
                adpcm_len: c.adpcm.len(),
                decoded,
            };
        }
        gain *= 0.94;
    }
}

/// `sfx-synth variants <dir>`: every voicing of the eight sounds as a decoded
/// WAV, `<sound>-a.wav` and so on, with a table of what each costs.
fn variants_cmd(dir: &std::path::Path) {
    fs::create_dir_all(dir).unwrap();
    println!("| sound | variant | rate | samples | ADPCM bytes | peak dBFS | RMS dBFS | target RMS |");
    println!("| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |");
    for (name, rate, _) in variants::SOUNDS {
        for v in ['a', 'b', 'c'] {
            let r = render_variant(name, v);
            let (peak, rms) = levels(&r.decoded);
            println!(
                "| {name} | {v} | {rate} | {} | {} | {peak:.1} | {rms:.1} | {:.1} |",
                r.decoded.len(),
                r.adpcm_len,
                variants::target_rms_db(name)
            );
            fs::write(dir.join(format!("{name}-{v}.wav")), wav::write_mono16(rate, &r.decoded)).unwrap();
        }
    }
}

fn levels(dec: &[i16]) -> (f64, f64) {
    let peak = dec.iter().map(|v| (*v as f64).abs()).fold(0.0, f64::max) / 32768.0;
    let rms = (dec.iter().map(|v| (*v as f64 / 32768.0).powi(2)).sum::<f64>() / dec.len() as f64).sqrt();
    (db(peak), db(rms))
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    match argv.get(1).map(String::as_str) {
        Some("hash") => {
            let b = fs::read(&argv[2]).unwrap();
            println!("{}; {:016x}", b.len(), import::fnv64(&b));
            return;
        }
        Some("import") => {
            let p = std::path::Path::new;
            import::run(p(&argv[2]), p(&argv[3]), p(&argv[4]), argv.get(5).map(|s| p(s)));
            return;
        }
        _ => {}
    }
    if argv.get(1).map(String::as_str) == Some("variants") {
        variants_cmd(std::path::Path::new(argv.get(2).expect("usage: sfx-synth variants <dir>")));
        return;
    }
    let mut args = std::env::args().skip(1);
    let out: PathBuf = args.next().expect("usage: sfx-synth <out dir> [preview dir]").into();
    let preview: Option<PathBuf> = args.next().map(Into::into);
    fs::create_dir_all(&out).unwrap();
    if let Some(p) = &preview {
        fs::create_dir_all(p).unwrap();
    }
    println!("| sound | rate | samples | ADPCM bytes | peak dBFS | RMS dBFS |");
    println!("| --- | ---: | ---: | ---: | ---: | ---: |");
    let mut total = 0usize;
    for def in defs() {
        let r = render(&def);
        let (peak, rms) = levels(&r.decoded);
        println!(
            "| {} | {} | {} | {} | {:.1} | {:.1} |",
            def.name,
            def.rate,
            r.decoded.len(),
            r.adpcm_len,
            peak,
            rms
        );
        total += r.adpcm_len + 16;
        fs::write(out.join(format!("{}.psau", def.name)), &r.psau).unwrap();
        if let Some(p) = &preview {
            fs::write(p.join(format!("{}.wav", def.name)), wav::write_mono16(def.rate, &r.decoded)).unwrap();
        }
    }
    // Context for levelling: the same figures for the recorded samples already in
    // the directory (cooked elsewhere, not owned by this tool).
    for name in ["hit_punch", "pickup_coin", "explosion_short", "swoosh"] {
        let bytes = fs::read(out.join(format!("{name}.psau"))).unwrap();
        let dec = adpcm::decode(&bytes[32..]);
        let (peak, rms) = levels(&dec);
        println!("| (existing) {name} | 44100 | {} | {} | {:.1} | {:.1} |", dec.len(), bytes.len() - 32, peak, rms);
    }
    println!();
    println!("new samples: {total} bytes of SPU RAM including one parking block each");
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The committed files are what this tool makes, byte for byte. A sound
    /// edited here and not re-rendered (or a file edited by hand) fails.
    #[test]
    fn committed_files_are_the_tools_output() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../game/assets/audio");
        for def in defs() {
            let r = render(&def);
            let on_disk = fs::read(dir.join(format!("{}.psau", def.name)))
                .unwrap_or_else(|e| panic!("{}.psau: {e}", def.name));
            assert!(on_disk == r.psau, "{}.psau is stale: run `make sfx`", def.name);
        }
    }

    /// Every sound has signal, none clips, and a loop's ADPCM ends on the
    /// block that sends the SPU back to the loop start.
    #[test]
    fn sounds_are_audible_unclipped_and_loops_loop() {
        for def in defs() {
            let r = render(&def);
            let (peak, rms) = levels(&r.decoded);
            assert!(peak <= -0.05, "{} clips ({peak:.2} dBFS)", def.name);
            assert!(rms > -30.0, "{} is nearly silent ({rms:.1} dBFS RMS)", def.name);
            // ADPCM block flags live in byte 1 of each 16-byte block: bit 0 END,
            // bit 1 REPEAT, bit 2 LOOP START.
            let adpcm = &r.psau[32..];
            let last = adpcm.len() - 16;
            let flags_last = adpcm[last + 1];
            if def.looped {
                assert_eq!(flags_last & 3, 3, "{} loop does not repeat", def.name);
                assert!(
                    adpcm.chunks(16).any(|b| b[1] & 4 != 0),
                    "{} loop has no loop-start block",
                    def.name
                );
            } else {
                assert_eq!(flags_last & 3, 1, "{} one-shot must end without repeating", def.name);
            }
        }
    }

    /// A loop's wrap is no bigger a step than the loop makes anywhere else:
    /// the last sample leads into the first as the waveform would.
    #[test]
    fn variant_loops_close_without_a_click() {
        for (name, _, looped) in variants::SOUNDS {
            if !looped {
                continue;
            }
            for v in ['a', 'b', 'c'] {
                let d = render_variant(name, v).decoded;
                let step = |a: i16, b: i16| (a as f64 - b as f64).abs();
                let seam = step(d[0], d[d.len() - 1]);
                let max_step = d.windows(2).map(|w| step(w[1], w[0])).fold(0.0, f64::max);
                println!("{name}-{v}: seam {seam}, largest step inside {max_step}");
                assert!(seam <= 1.15 * max_step, "{name}-{v}: seam {seam} against largest step {max_step:.0}");
            }
        }
    }

    #[test]
    fn output_is_deterministic() {
        let d = defs();
        let a = render(&d[0]).psau;
        let b = render(&d[0]).psau;
        assert!(a == b);
    }
}
