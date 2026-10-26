// SPDX-License-Identifier: GPL-2.0-or-later
//! Small helpers the screen-space effects share: a deterministic scatter, a
//! cubic ease, colour scaling and the additive draw mode.

use psx_gpu::material::{BlendMode, TextureMaterial};
use psx_gpu::prim::QuadFlat;
use psx_gpu::Gpu;

/// A colour as the GPU takes it.
pub type Rgb = (u8, u8, u8);

/// One in Q12.
pub const ONE: i32 = 4096;

/// A repeatable scatter of two small integers into `0..=255`. There is no RNG
/// on this hardware and an effect that must look the same on every run wants
/// none: the same inputs give the same sparks.
pub fn scatter(a: u32, b: u32) -> u32 {
    let h = a
        .wrapping_mul(374_761_393)
        .wrapping_add(b.wrapping_mul(668_265_263));
    let h = (h ^ (h >> 13)).wrapping_mul(1_274_126_177);
    ((h ^ (h >> 16)) >> 8) & 0xff
}

/// `1 - (1 - t)^3` for `t` in Q12: fast at the start, settling at the end.
pub const fn ease_out(t: i32) -> i32 {
    let u = ONE - clamp(t, 0, ONE);
    let u2 = u * u >> 12;
    ONE - (u2 * u >> 12)
}

/// `t^3` for `t` in Q12: slow at the start, fast at the end.
pub const fn ease_in(t: i32) -> i32 {
    let t = clamp(t, 0, ONE);
    let t2 = t * t >> 12;
    t2 * t >> 12
}

/// `v` limited to `lo..=hi`; `i32::clamp` is not `const`.
pub const fn clamp(v: i32, lo: i32, hi: i32) -> i32 {
    if v < lo {
        lo
    } else if v > hi {
        hi
    } else {
        v
    }
}

/// `colour` at `alpha` of full, `alpha` in Q8 (`0..=256`). An additive draw has
/// no alpha channel of its own: dimming the colour is how it fades.
pub fn dim(colour: Rgb, alpha: i32) -> Rgb {
    let a = clamp(alpha, 0, 256);
    (
        (colour.0 as i32 * a >> 8) as u8,
        (colour.1 as i32 * a >> 8) as u8,
        (colour.2 as i32 * a >> 8) as u8,
    )
}

/// Linear blend of two colours, `t` in Q8.
pub fn mix(a: Rgb, b: Rgb, t: i32) -> Rgb {
    let f = |x: u8, y: u8| (x as i32 + ((y as i32 - x as i32) * t >> 8)) as u8;
    (f(a.0, b.0), f(a.1, b.1), f(a.2, b.2))
}

/// Switch the GPU to add what is drawn to what is already there, for glows and
/// light sweeps. Follow it with [`opaque`] when the effect is done.
pub fn additive(gpu: &mut Gpu) {
    gpu.set_draw_mode(TextureMaterial::blended(0, 0, (0, 0, 0), BlendMode::Add));
}

/// Back to plain drawing after [`additive`].
pub fn opaque(gpu: &mut Gpu) {
    gpu.set_draw_mode(TextureMaterial::opaque(0, 0, (0x80, 0x80, 0x80)));
}

/// An opaque axis-aligned rectangle.
pub fn rect(gpu: &mut Gpu, x: i32, y: i32, w: i32, h: i32, colour: Rgb) {
    if w <= 0 || h <= 0 {
        return;
    }
    gpu.draw(&QuadFlat::rect(
        (x as i16, y as i16),
        (w as u16, h as u16),
        colour,
    ));
}

/// A rectangle drawn with whatever blend [`additive`] last set.
pub fn rect_blended(gpu: &mut Gpu, x: i32, y: i32, w: i32, h: i32, colour: Rgb) {
    if w <= 0 || h <= 0 {
        return;
    }
    gpu.draw(&QuadFlat::rect((x as i16, y as i16), (w as u16, h as u16), colour).translucent());
}

/// A parallelogram whose top edge leads its bottom edge by `slant` pixels: the
/// shape of every menu button. `x` is the bottom-left corner.
pub fn slanted(gpu: &mut Gpu, x: i32, y: i32, w: i32, h: i32, slant: i32, colour: Rgb) {
    if w <= 0 || h <= 0 {
        return;
    }
    gpu.draw(&QuadFlat::new(
        slanted_corners(x, y, w, h, slant),
        colour.0,
        colour.1,
        colour.2,
    ));
}

/// [`slanted`] drawn with the blend [`additive`] last set.
pub fn slanted_blended(gpu: &mut Gpu, x: i32, y: i32, w: i32, h: i32, slant: i32, colour: Rgb) {
    if w <= 0 || h <= 0 {
        return;
    }
    gpu.draw(
        &QuadFlat::new(
            slanted_corners(x, y, w, h, slant),
            colour.0,
            colour.1,
            colour.2,
        )
        .translucent(),
    );
}

/// Corners in PS1 order (top left, top right, bottom left, bottom right).
const fn slanted_corners(x: i32, y: i32, w: i32, h: i32, slant: i32) -> [(i16, i16); 4] {
    [
        ((x + slant) as i16, y as i16),
        ((x + slant + w) as i16, y as i16),
        (x as i16, (y + h) as i16),
        ((x + w) as i16, (y + h) as i16),
    ]
}
