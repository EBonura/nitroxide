// SPDX-License-Identifier: GPL-2.0-or-later
//! The goal replay as the game runs it: the ring the match is recorded into, the
//! scratch world a replay is drawn from, and the state of one replay in progress.
//! `nitroxide_sim::replay` has the data and the arithmetic.

use nitroxide_sim::replay::{
    apply, direct, pick_style, Clock, Direction, Recent, Recorder, Style, Window,
};
use nitroxide_sim::{Sim, Team};

use crate::{audio, draw};

/// The last four seconds of the match: 15,360 bytes of BSS. Not on the heap,
/// which the game never uses, and not in `NitroXide`, which is copied about.
static mut RING: Recorder = Recorder::new();
/// The world a replay frame is written into and the renderer is handed: a copy
/// of the live sim, so the pads and the scores are right, with the bodies moved.
static mut SCRATCH: Option<Sim> = None;

/// The goal shots of the match's last two replays, newest first, so the director
/// does not use one style three times running.
static mut RECENT: Recent = [None, None];

/// Replays begun, skipped with X and played to the end: for the gate.
pub static mut STARTED: u32 = 0;
pub static mut SKIPPED: u32 = 0;
pub static mut FINISHED: u32 = 0;
/// The goal shot of the replay last begun: 0 none yet, 1 the side angle, 2 from
/// behind the net.
pub static mut LAST_STYLE: u32 = 0;

/// The music's level during the slow motion, Q12.
const DUCKED: u16 = 1400;

/// One replay in progress.
#[derive(Copy, Clone)]
pub struct Replay {
    window: Window,
    clock: Clock,
    /// How the goal is to be shot: the touch to cut on, which angle, which side.
    direction: Direction,
    /// Ticks since the replay began.
    pub age: u16,
    /// True from the cut onto the goal shot.
    pub at_goal: bool,
    /// The team that scored, whose colour the banner is in.
    pub scorer: Team,
    /// The music's current duck, Q12.
    duck: u16,
}

/// A new match: no goals shot yet.
pub fn new_match() {
    // SAFETY: the game is single-threaded.
    unsafe { *core::ptr::addr_of_mut!(RECENT) = [None, None] };
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
        // The goal shot is at random from the match, as the sim has it at this
        // goal: the same match shoots the same goals the same way.
        let recent = unsafe { *core::ptr::addr_of!(RECENT) };
        let style = pick_style(ring, window, live.director_roll(), recent);
        unsafe {
            *core::ptr::addr_of_mut!(RECENT) = [Some(style), recent[0]];
            LAST_STYLE = if style == Style::Side { 1 } else { 2 };
        }
        let mut replay = Self {
            window,
            clock: Clock::new(window),
            direction: Direction::new(ring, window, style),
            age: 0,
            at_goal: false,
            scorer: live.last_scorer,
            duck: 4096,
        };
        replay.show();
        unsafe { STARTED += 1 };
        Some(replay)
    }

    /// Put the current replay frame in the scratch world and the camera on it.
    fn show(&mut self) {
        // SAFETY: the game is single-threaded.
        let (ring, scratch) = unsafe {
            (
                &*core::ptr::addr_of!(RING),
                &mut *core::ptr::addr_of_mut!(SCRATCH),
            )
        };
        let Some(sim) = scratch else { return };
        apply(sim, ring, self.window, self.clock.s);
        let shot = direct(ring, self.window, &self.direction, self.clock.s);
        self.at_goal = (self.clock.s >> 8) >= self.direction.touch;
        draw::set_replay_cam(Some((shot.eye, shot.target)));
        draw::set_replay_plane(Some(shot.plane));
        for (i, car) in [&sim.car, &sim.opponent].into_iter().enumerate() {
            draw::set_flame(i, if car.boosting { 256 } else { 0 });
        }
    }

    /// One display tick. False once the replay is over.
    pub fn tick(&mut self) -> bool {
        self.age += 1;
        let running = self.clock.step();
        self.show();
        // The music drops for the slow part and comes back after it.
        let goal = if self.clock.slow() { DUCKED } else { 4096 };
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
        draw::set_replay_plane(None);
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

/// The scratch world's slot, for the results screen to stage its scene in once
/// the replay is over.
pub fn scratch() -> &'static mut Option<Sim> {
    // SAFETY: the game is single-threaded.
    unsafe { &mut *core::ptr::addr_of_mut!(SCRATCH) }
}

/// The scratch world, for the renderer.
pub fn world() -> Option<&'static Sim> {
    // SAFETY: the game is single-threaded.
    unsafe { (*core::ptr::addr_of!(SCRATCH)).as_ref() }
}
