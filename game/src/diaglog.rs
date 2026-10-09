// SPDX-License-Identifier: GPL-2.0-or-later
//! Dev only (`diag-log`): per-frame state rings (the last N rows, wrapping) kept in RAM, read back from a
//! headless run's `--dump-ram`. Writing a row is a few stores, so a tape replay
//! keeps the cadence it has without the log. Each ring starts with a magic
//! word pair so the dump can be searched for it. A caller takes the next row
//! and fills it in place: the renderer runs on the 1 KB scratchpad stack,
//! which has no room for a row-sized temporary.

pub const CAM_W: usize = 24;
pub const TRK_W: usize = 8;
pub const DRAW_W: usize = 8;
const CAM_N: usize = 600;
const TRK_N: usize = 1400;
const DRAW_N: usize = 600;

#[repr(C)]
struct Ring<const W: usize, const N: usize> {
    magic: [u32; 2],
    width: u32,
    len: u32,
    rows: [[i32; W]; N],
    spare: [i32; W],
}

static mut CAM: Ring<CAM_W, CAM_N> = Ring {
    magic: [0x4e58_4344, 0x4d41_4331],
    width: CAM_W as u32,
    len: 0,
    rows: [[0; CAM_W]; CAM_N],
    spare: [0; CAM_W],
};
static mut TRK: Ring<TRK_W, TRK_N> = Ring {
    magic: [0x4e58_4344, 0x4b52_5431],
    width: TRK_W as u32,
    len: 0,
    rows: [[0; TRK_W]; TRK_N],
    spare: [0; TRK_W],
};
static mut DRAW: Ring<DRAW_W, DRAW_N> = Ring {
    magic: [0x4e58_4344, 0x4452_5731],
    width: DRAW_W as u32,
    len: 0,
    rows: [[0; DRAW_W]; DRAW_N],
    spare: [0; DRAW_W],
};

fn next<const W: usize, const N: usize>(ring: &'static mut Ring<W, N>) -> &'static mut [i32; W] {
    let n = ring.len as usize % N;
    ring.len += 1;
    &mut ring.rows[n]
}

pub fn cam() -> &'static mut [i32; CAM_W] {
    unsafe { next(&mut *core::ptr::addr_of_mut!(CAM)) }
}

pub fn trk() -> &'static mut [i32; TRK_W] {
    unsafe { next(&mut *core::ptr::addr_of_mut!(TRK)) }
}

pub fn draw() -> &'static mut [i32; DRAW_W] {
    unsafe { next(&mut *core::ptr::addr_of_mut!(DRAW)) }
}

/// `fill!(cam; a, b, c)`: take the ring's next row and store the values in it.
macro_rules! fill {
    ($ring:ident; $($v:expr),* $(,)?) => {{
        let row = $crate::diaglog::$ring();
        let mut i = 0;
        $( row[i] = $v; i += 1; )*
        let _ = i;
    }};
}
pub(crate) use fill;
