// SPDX-License-Identifier: GPL-2.0-or-later
//! Angle of a vector to within two units of a Q12 turn.
//!
//! The SDK's `atan2_q12` is linear in the slope inside each octant, which puts
//! its worst error at about four degrees mid-octant. That is fine for steering
//! logic, but a camera that rebuilds a heading from a vector turns the error
//! into a visible wobble: as a car holds a turn its heading sweeps through the
//! octants and the error rises and falls with them, so the view swings about
//! the car by several degrees every few hundred ticks of yaw. This one folds
//! to the first octant the same way and reads `atan(slope)` from a 64-entry
//! table with linear interpolation, which keeps the error to a unit or two.

/// `atan(i / 64)` for `i` in `0..=64`, in Q12 turns scaled by 16, so the
/// table carries four fractional bits and 45 degrees is exactly 8192.
const ATAN_X16: [u16; 65] = [
    0, 163, 326, 489, 651, 813, 975, 1136, 1297, 1457, 1617, 1775, 1933, 2090, 2246, 2401, 2555,
    2708, 2860, 3010, 3159, 3307, 3453, 3599, 3742, 3884, 4025, 4164, 4302, 4438, 4572, 4705, 4836,
    4966, 5094, 5220, 5344, 5467, 5589, 5708, 5826, 5943, 6058, 6171, 6282, 6392, 6500, 6607, 6712,
    6815, 6917, 7018, 7117, 7214, 7310, 7405, 7498, 7589, 7679, 7768, 7856, 7942, 8026, 8110, 8192,
];

/// Q12 angle of the vector `(x, y)`, measured the way
/// `psx_math::sincos::atan2_q12` measures it (`atan2_q12(sin a, cos a) == a`):
/// 0 along +x, increasing toward +y. `(0, 0)` returns 0.
pub fn atan2_q12_fine(y: i32, x: i32) -> u16 {
    if x == 0 && y == 0 {
        return 0;
    }
    let (mut ax, mut ay) = (x.unsigned_abs(), y.unsigned_abs());
    // `small << 12` has to fit a u32. Scaling both magnitudes together keeps
    // the ratio, which is all that is used below.
    while ax >= (1 << 19) || ay >= (1 << 19) {
        ax >>= 8;
        ay >>= 8;
    }
    let (small, large) = if ax >= ay { (ay, ax) } else { (ax, ay) };
    // Slope in Q12, 0..=4096: six bits pick the table entry, six interpolate.
    let slope = (small << 12) / large;
    let (i, frac) = ((slope >> 6) as usize, slope & 63);
    let a = ATAN_X16[i] as u32;
    let b = ATAN_X16[i + (i < 64) as usize] as u32;
    let octant = ((a * 64 + (b - a) * frac + 512) >> 10) as i32;
    let q = if ax >= ay { octant } else { 1024 - octant };
    let angle = match (x >= 0, y >= 0) {
        (true, true) => q,
        (false, true) => 2048 - q,
        (false, false) => 2048 + q,
        (true, false) => 4096 - q,
    };
    (angle & 0xFFF) as u16
}

#[cfg(test)]
mod tests {
    use super::*;
    use psx_math::sincos::atan2_q12;

    /// Turn difference, folded to `-2048..2048`.
    fn gap(a: u16, b: u16) -> i32 {
        ((a as i32 - b as i32 + 2048).rem_euclid(4096)) - 2048
    }

    #[test]
    fn exact_on_the_axes_and_diagonals() {
        for (y, x, want) in [
            (0, 5, 0),
            (5, 5, 512),
            (5, 0, 1024),
            (5, -5, 1536),
            (0, -5, 2048),
            (-5, -5, 2560),
            (-5, 0, 3072),
            (-5, 5, 3584),
        ] {
            assert_eq!(atan2_q12_fine(y, x), want, "y {y} x {x}");
        }
        assert_eq!(atan2_q12_fine(0, 0), 0);
    }

    #[test]
    fn within_two_units_of_the_true_angle_at_every_angle() {
        // Float is fine on the host: it is only the reference. The vectors are
        // rounded to integers, which alone is worth a unit at the small radius.
        for radius in [800, 4096, 300_000, 40_000_000] {
            for turn in 0..4096 {
                let t = turn as f64 * core::f64::consts::TAU / 4096.0;
                let (y, x) = (
                    (t.sin() * radius as f64).round() as i32,
                    (t.cos() * radius as f64).round() as i32,
                );
                let got = atan2_q12_fine(y, x);
                assert!(gap(got, turn as u16).abs() <= 2, "radius {radius} turn {turn} got {got}");
            }
        }
    }

    #[test]
    fn round_trips_the_sdk_sine_and_cosine() {
        use psx_math::sincos::{cos_q12, sin_q12};
        let mut worst = 0;
        for turn in 0..4096u16 {
            let got = atan2_q12_fine(sin_q12(turn), cos_q12(turn));
            worst = worst.max(gap(got, turn).abs());
        }
        assert!(worst <= 2, "worst {worst}");
    }

    #[test]
    fn the_sdk_approximation_is_what_wobbled_the_camera() {
        // The reason this module exists, kept as a measurement: the octant
        // linear version is off by dozens of units mid-octant, this one is not.
        let (mut sdk, mut fine) = (0, 0);
        for turn in 0..4096u16 {
            let (s, c) = (
                psx_math::sincos::sin_q12(turn),
                psx_math::sincos::cos_q12(turn),
            );
            sdk = sdk.max(gap(atan2_q12(s, c), turn).abs());
            fine = fine.max(gap(atan2_q12_fine(s, c), turn).abs());
        }
        assert!(sdk > 40, "sdk {sdk}");
        assert!(fine <= 2, "fine {fine}");
    }

    #[test]
    fn stays_monotonic_through_a_whole_turn() {
        let mut last = 0i32;
        for k in 0..=4096 {
            let t = k as f64 * core::f64::consts::TAU / 4096.0;
            let a = atan2_q12_fine((t.sin() * 5000.0) as i32, (t.cos() * 5000.0) as i32) as i32;
            let step = (a - last + 2048).rem_euclid(4096) - 2048;
            assert!((0..=2).contains(&step) || k == 0, "k {k} step {step}");
            last = a;
        }
    }
}
