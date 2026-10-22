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
        clock: sim.clock,
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
