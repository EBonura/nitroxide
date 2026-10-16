// SPDX-License-Identifier: GPL-2.0-or-later
//! Dev: what the audio calls cost, in CPU cycles, read off root counter 2.
//!
//! Counter 2 free-runs at the system clock over eight, so one count is eight
//! cycles. [`measure`] times a closure and keeps a count, a sum and a worst
//! case per slot, and [`report`] prints them on the TTY every thousand frames.
//! Only built with `diag-audio-cost`; a shipping disc carries none of it.

use core::ptr::{read_volatile, write_volatile};
use psx_rt::tty;

const T2_COUNTER: *const u32 = 0x1F80_1120 as *const u32;
const T2_MODE: *mut u32 = 0x1F80_1124 as *mut u32;
/// Clock source: system clock over eight.
const T2_SYSCLK_DIV8: u32 = 1 << 9;

/// The two timed calls.
pub const TICK: usize = 0;
pub const UPDATE: usize = 1;

static mut COUNT: [u32; 2] = [0; 2];
static mut SUM: [u32; 2] = [0; 2];
static mut MAX: [u32; 2] = [0; 2];
static mut FRAMES: u32 = 0;
/// An update this many cycles over is logged with the frame it happened on.
const SPIKE: u32 = 8000;

pub fn init() {
    // SAFETY: root counter 2's mode register; nothing else here uses the counter.
    unsafe { write_volatile(T2_MODE, T2_SYSCLK_DIV8) };
}

fn now() -> u32 {
    // SAFETY: a read of root counter 2's current value.
    unsafe { read_volatile(T2_COUNTER) & 0xFFFF }
}

/// Run `f`, charge its cycles to `slot`.
pub fn measure<R>(slot: usize, f: impl FnOnce() -> R) -> R {
    let start = now();
    let out = f();
    let cycles = (now().wrapping_sub(start) & 0xFFFF) * 8;
    unsafe {
        COUNT[slot] += 1;
        SUM[slot] = SUM[slot].wrapping_add(cycles);
        if cycles > MAX[slot] {
            MAX[slot] = cycles;
        }
        if slot == UPDATE && cycles > SPIKE {
            tty::print("audio-cost spike frame=");
            tty::print_hex_u32(FRAMES);
            tty::print(" cycles=");
            tty::print_hex_u32(cycles);
            tty::print("\n");
        }
    }
    out
}

fn line(label: &str, slot: usize) {
    tty::print(label);
    tty::print(" n=");
    tty::print_hex_u32(unsafe { COUNT[slot] });
    tty::print(" sum=");
    tty::print_hex_u32(unsafe { SUM[slot] });
    tty::print(" max=");
    tty::print_hex_u32(unsafe { MAX[slot] });
    tty::print("\n");
}

/// Once a frame: print the totals every thousandth.
pub fn report() {
    let n = unsafe {
        FRAMES += 1;
        FRAMES
    };
    if n % 1000 == 0 {
        line("audio-cost tick", TICK);
        line("audio-cost update", UPDATE);
    }
}
