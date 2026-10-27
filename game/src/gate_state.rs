//! Read-only match snapshot for headless journeys and external debuggers.
//!
//! `NITRO_GATE_STATE` is present in ordinary shipping images. Version 1 is a
//! fixed 256-byte, little-endian layout. Every field is a 32-bit word. Body
//! order is blue car, orange car, ball; each body is position XYZ followed by
//! velocity XYZ in simulation units. The 34 pad words follow the three bodies
//! in `nitroxide_sim::PADS` order. A zero pad word means the pad is available.
//! The snapshot is written on each game update and is never read by the game.

use nitroxide_sim::{Sim, PADS, V3};

use crate::{CardAsk, NitroXide, Phase};

/// Independent settings receipt. All words are little-endian u32s; this
/// block is never consulted by gameplay or card persistence.
#[repr(C)]
pub struct SettingsState {
    magic: u32,
    version: u32,
    size_bytes: u32,
    /// Zero when closed, otherwise the selected row plus one.
    panel_row: u32,
    row_count: u32,
    dirty: u32,
    /// Idle=0, asking=1, formatting=2, declined=3.
    card_prompt: u32,
    /// None=0, saved=1, failed=2, no card=3, canceled=4.
    save_status: u32,
    sfx_volume: u32,
}

#[used]
#[no_mangle]
pub static mut NITRO_SETTINGS_STATE: SettingsState = SettingsState {
    magic: u32::from_le_bytes(*b"NSET"),
    version: 1,
    size_bytes: 36,
    panel_row: 0,
    row_count: 0,
    dirty: 0,
    card_prompt: 0,
    save_status: 0,
    sfx_volume: 0,
};

const _: () = assert!(core::mem::size_of::<SettingsState>() == 36);

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GateBody {
    pub position: [i32; 3],
    pub velocity: [i32; 3],
}

impl GateBody {
    const ZERO: Self = Self {
        position: [0; 3],
        velocity: [0; 3],
    };

    fn from_parts(position: V3, velocity: V3) -> Self {
        Self {
            position: [position.x, position.y, position.z],
            velocity: [velocity.x, velocity.y, velocity.z],
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct GateState {
    pub magic: u32,
    pub version: u32,
    pub size_bytes: u32,
    pub completed_tick: u32,
    /// Intro=0, title=1, select=2, play=3, results=4, demo=5.
    pub phase: u32,
    pub two_player: u32,
    pub score_blue: u32,
    pub score_orange: u32,
    pub goal_freeze: u32,
    pub kickoff_hold: u32,
    pub clock: u32,
    pub pads_spent: u32,
    pub bodies: [GateBody; 3],
    pub pad_remaining: [u32; PADS.len()],
}

/// Set in the published clock while the match is in overtime, with the ticks of
/// live overtime play in the low bits: a journey can tell that the clock has
/// turned round without the layout growing.
pub const OVERTIME_FLAG: u32 = 0x4000_0000;

const MAGIC: u32 = u32::from_le_bytes(*b"NITR");
const VERSION: u32 = 1;

#[used]
#[no_mangle]
pub static mut NITRO_GATE_STATE: GateState = GateState {
    magic: MAGIC,
    version: VERSION,
    size_bytes: core::mem::size_of::<GateState>() as u32,
    completed_tick: 0,
    phase: 0,
    two_player: 0,
    score_blue: 0,
    score_orange: 0,
    goal_freeze: 0,
    kickoff_hold: 0,
    clock: 0,
    pads_spent: 0,
    bodies: [GateBody::ZERO; 3],
    pad_remaining: [0; PADS.len()],
};

/// The goal replay, for a journey that scores and watches or skips it. Words,
/// little-endian: magic, version, size in bytes, the stage (0 none, 1 the GOAL
/// banner, 2 the replay), ticks since it began, the source tick it is showing
/// relative to the ball crossing the line (signed), then how many replays have
/// begun, been skipped and played to the end.
#[repr(C)]
pub struct ReplayState {
    magic: u32,
    version: u32,
    size_bytes: u32,
    stage: u32,
    age: u32,
    source_tick: i32,
    started: u32,
    skipped: u32,
    finished: u32,
}

const _: () = assert!(core::mem::size_of::<ReplayState>() == 36);

#[used]
#[no_mangle]
pub static mut NITRO_REPLAY_STATE: ReplayState = ReplayState {
    magic: u32::from_le_bytes(*b"NRPL"),
    version: 1,
    size_bytes: 36,
    stage: 0,
    age: 0,
    source_tick: 0,
    started: 0,
    skipped: 0,
    finished: 0,
};

pub fn publish(game: &NitroXide, completed_tick: u32) {
    let settings = SettingsState {
        magic: u32::from_le_bytes(*b"NSET"),
        version: 1,
        size_bytes: 36,
        panel_row: game.settings.map_or(0, |row| row as u32 + 1),
        row_count: game.settings_rows().len() as u32,
        dirty: u32::from(game.settings_dirty),
        card_prompt: match game.card_ask {
            CardAsk::Idle => 0,
            CardAsk::Asking => 1,
            CardAsk::Formatting => 2,
            CardAsk::Declined => 3,
        },
        save_status: match game.settings_save_status {
            "SAVED" => 1,
            "FAILED" => 2,
            "NO CARD" => 3,
            "CANCELED" => 4,
            "SAVING" => 5,
            _ => 0,
        },
        sfx_volume: game.profile.sfx_volume as u32,
    };
    let (stage, age, source_tick) = match game.replay {
        Some(r) => (
            if r.age <= crate::replay::BANNER_TICKS {
                1
            } else {
                2
            },
            u32::from(r.age),
            r.source_tick(),
        ),
        None => (0, 0, 0),
    };
    let replay = ReplayState {
        magic: u32::from_le_bytes(*b"NRPL"),
        version: 1,
        size_bytes: 36,
        stage,
        age,
        source_tick,
        started: unsafe { crate::replay::STARTED },
        skipped: unsafe { crate::replay::SKIPPED },
        finished: unsafe { crate::replay::FINISHED },
    };
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(NITRO_REPLAY_STATE), replay) };
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(NITRO_SETTINGS_STATE), settings) };
    let sim: &Sim = &game.sim;
    let phase = match game.phase {
        Phase::Intro => 0,
        Phase::Title => 1,
        Phase::Select => 2,
        Phase::Play => 3,
        Phase::Results => 4,
        Phase::Demo => 5,
    };
    let mut pads = [0; PADS.len()];
    for (dst, src) in pads.iter_mut().zip(sim.pad_timers) {
        *dst = u32::from(src);
    }
    let snapshot = GateState {
        magic: MAGIC,
        version: VERSION,
        size_bytes: core::mem::size_of::<GateState>() as u32,
        completed_tick,
        phase,
        two_player: u32::from(game.two_player),
        score_blue: u32::from(sim.score_blue),
        score_orange: u32::from(sim.score_orange),
        goal_freeze: u32::from(sim.goal_freeze),
        kickoff_hold: u32::from(sim.kickoff_hold),
        clock: if sim.overtime {
            OVERTIME_FLAG | sim.overtime_ticks
        } else {
            sim.clock
        },
        pads_spent: pads.iter().filter(|&&t| t != 0).count() as u32,
        bodies: [
            GateBody::from_parts(sim.car.p, sim.car.v),
            GateBody::from_parts(sim.opponent.p, sim.opponent.v),
            GateBody::from_parts(sim.ball.p, sim.ball.v),
        ],
        pad_remaining: pads,
    };
    // The game does not use this block. Volatile keeps it observable to an
    // external reader even with link-time optimisation enabled.
    unsafe { core::ptr::write_volatile(core::ptr::addr_of_mut!(NITRO_GATE_STATE), snapshot) };
}

pub const fn size_is_stable() -> bool {
    core::mem::size_of::<GateState>() == 256
}

const _: () = assert!(size_is_stable());
