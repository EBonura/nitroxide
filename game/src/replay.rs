// SPDX-License-Identifier: GPL-2.0-or-later
//! The goal replay as the game runs it: the ring the match is recorded into, the
//! scratch world a replay is drawn from, and the state of one replay in progress.
//! `nitroxide_sim::replay` has the data and the arithmetic.

use nitroxide_sim::replay::{apply, camera, Clock, Recorder, Window};
use nitroxide_sim::{Sim, Team};

use crate::{audio, draw};

/// The last four seconds of the match: 15,360 bytes of BSS. Not on the heap,
/// which the game never uses, and not in `NitroXide`, which is copied about.
static mut RING: Recorder = Recorder::new();
/// The world a replay frame is written into and the renderer is handed: a copy
/// of the live sim, so the pads and the scores are right, with the bodies moved.
static mut SCRATCH: Option<Sim> = None;

/// Replays begun, skipped with X and played to the end: for the gate.
pub static mut STARTED: u32 = 0;
pub static mut SKIPPED: u32 = 0;
pub static mut FINISHED: u32 = 0;

/// Ticks the GOAL banner holds the first frame of the replay.
pub const BANNER_TICKS: u16 = 54;
/// The music's level during the slow motion, Q12.
const DUCKED: u16 = 1400;

/// One replay in progress.
#[derive(Copy, Clone)]
pub struct Replay {
    window: Window,
    clock: Clock,
    /// Ticks since the replay began; the banner holds the picture until
    /// [`BANNER_TICKS`].
    pub age: u16,
    /// The team that scored, whose colour the banner is in.
    pub scorer: Team,
    /// The music's current duck, Q12.
    duck: u16,
}

/// Forget the recording: a new match, or a replay that has been seen.
pub fn clear() {
    // SAFETY: the game is single-threaded.
    unsafe { (*core::ptr::addr_of_mut!(RING)).clear() };
}

/// Record the tick the sim has just run.
pub fn record(sim: &Sim) {
    if Recorder::wants(sim) {
        // SAFETY: the game is single-threaded.
        unsafe { (*core::ptr::addr_of_mut!(RING)).push(sim) };
    }
}

/// Bytes of RAM the ring takes.
pub const fn ring_bytes() -> usize {
    core::mem::size_of::<Recorder>()
}

impl Replay {
    /// Start a replay of the goal just scored, if the ring has one. `live` is
    /// the real sim as the goal's celebration leaves it.
    pub fn begin(live: &Sim) -> Option<Self> {
        // SAFETY: the game is single-threaded.
        let ring = unsafe { &*core::ptr::addr_of!(RING) };
        let window = ring.window()?;
        unsafe { *core::ptr::addr_of_mut!(SCRATCH) = Some(*live) };
        let mut replay = Self {
            window,
            clock: Clock::new(window),
            age: 0,
            scorer: live.last_scorer,
            duck: 4096,
        };
        replay.show();
        unsafe { STARTED += 1 };
        Some(replay)
    }

    fn seat(&self) -> usize {
        match self.scorer {
            Team::Blue => 0,
            Team::Orange => 1,
        }
    }

    /// Put the current replay frame in the scratch world and the camera on it.
    fn show(&self) {
        // SAFETY: the game is single-threaded.
        let (ring, scratch) = unsafe {
            (
                &*core::ptr::addr_of!(RING),
                &mut *core::ptr::addr_of_mut!(SCRATCH),
            )
        };
        let Some(sim) = scratch else { return };
        apply(sim, ring, self.window, self.clock.s);
        let seat = self.seat();
        draw::set_replay_cam(Some(camera(ring, self.window, self.clock.s, seat)));
        for (i, car) in [&sim.car, &sim.opponent].into_iter().enumerate() {
            draw::set_flame(i, if car.boosting { 256 } else { 0 });
        }
    }

    /// One display tick. False once the replay is over.
    pub fn tick(&mut self) -> bool {
        self.age += 1;
        let running = if self.age <= BANNER_TICKS {
            true
        } else {
            self.clock.step()
        };
        self.show();
        // The music drops for the slow part and comes back after it.
        let goal = if self.age > BANNER_TICKS && self.clock.slow() {
            DUCKED
        } else {
            4096
        };
        self.duck = if self.duck > goal {
            self.duck.saturating_sub(512).max(goal)
        } else {
            self.duck.saturating_add(128).min(goal)
        };
        audio::duck_music(self.duck);
        running
    }

    /// Hand the camera and the music back.
    pub fn finish(self, skipped: bool) {
        unsafe {
            if skipped {
                SKIPPED += 1;
            } else {
                FINISHED += 1;
            }
        }
        draw::set_replay_cam(None);
        audio::duck_music(4096);
        for seat in 0..2 {
            draw::set_flame(seat, 0);
        }
        clear();
    }

    /// Where the replay is, for the gate: ticks of source time before (negative)
    /// or after the ball crossed the line.
    pub fn source_tick(&self) -> i32 {
        self.clock.s >> 8
    }
}

/// The scratch world, for the renderer.
pub fn world() -> Option<&'static Sim> {
    // SAFETY: the game is single-threaded.
    unsafe { (*core::ptr::addr_of!(SCRATCH)).as_ref() }
}
