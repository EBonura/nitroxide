// SPDX-License-Identifier: GPL-2.0-or-later
//! Sound effects: cooked ADPCM samples for the events, the noise generator for
//! the one thing that is genuinely noise, all of it mixed through the SDK's
//! `psx-sfx` mixer and placed in stereo.
//!
//! The samples are the ones the game shipped with. Four impacts are recorded,
//! CC0, from PSoXide's cooked pack (`assets/audio/README.md`). Everything else
//! is synthesised by `tools/sfx-synth` from oscillators, noise and envelopes,
//! so it is ours and carries no licence of its own.
//!
//! # Who plays what
//!
//! The 24 SPU voices are split into lanes once, at boot (see [`LANES`]), and a
//! lane's policy decides what a new sound does when the lane is busy. Nothing
//! is wired to a voice any more: a sound asks for its lane and the mixer finds
//! it a voice, so a second car's engine, skid and jump fit in the same budget as
//! the first one's, and a flood of hits can never cut the goal horn (a
//! `Protected` sound is never stolen).
//!
//! | lane  | voices | policy       | carries                                   |
//! |-------|--------|--------------|-------------------------------------------|
//! | UI    | 2      | round robin  | menu sounds                               |
//! | LOOP  | 5      | first free   | both engines, both skids, the crowd       |
//! | WORLD | 5      | smart        | ball hits, bounces, pads, bangs, bumps    |
//! | CAR   | 5      | smart        | each car's jump, flip, landing, boost     |
//! | SCENE | 5      | smart        | horn, whistle, countdown, crowd calls     |
//!
//! The two left over are the boost hiss, one per car, on the noise generator:
//! a sample cannot hold a hiss for as long as the tank lasts.
//!
//! # Where they are
//!
//! A sound that comes from something in the arena (a car, the ball, a pad) is
//! placed with `psx_sfx::space`: distance to the listener sets the level
//! ([`CURVE_CAR`] and friends), the bearing sets the pan (equal power, unity at
//! the centre, so a sound straight ahead is exactly as loud as it was in mono).
//! The listener is the chase camera, read back from the renderer's own state
//! ([`draw::listener_pose`]). One listener in a one-player match; two in split
//! screen, and then each sound is heard from the viewport it is loudest to, a
//! loop from the one it is nearest to, and a goal from the middle of both. The
//! announcer's calls, the crowd and the menu are not in the arena and stay
//! centred at the level they always had.
//!
//! Three kinds of caller:
//!
//! * [`update`] reads one simulation tick of a match and makes the noises it
//!   earned. It only reads the sim, so the sim stays deterministic.
//! * [`ui`] is the menus' sound, fired by the input handler that changed
//!   something. It needs no match.
//! * [`tick`] is the clock the mixer's one-shot lifetimes run on. It runs every
//!   update in every phase, because a menu click has to be keyed off in a menu.

use crate::draw;
use nitroxide_sim::{uu, Car, Sim, WinCondition, GOAL_HALF_W, GOAL_H, HALF_Z, PADS};
use psx_sfx::hw::Hw;
use psx_sfx::mixer::{Class, Handle, Lane, Mixer, Play, Policy, Sound, SoundId};
use psx_sfx::space::{
    gains, Center, Curve, Emitter, Gains, Listener, ListenerRule, Listeners, PanLaw, Space,
};
use psx_sfx::{Bank, Sample as SfxSample};
use psx_spu::{self as spu, Adsr, CdVolume, Pitch, SpuAddr, Volume};

static HIT_SFX: &[u8] = include_bytes!("../assets/audio/hit_punch.psau");
static PAD_SFX: &[u8] = include_bytes!("../assets/audio/pickup_coin.psau");
static GOAL_SFX: &[u8] = include_bytes!("../assets/audio/explosion_short.psau");
static BOOST_SFX: &[u8] = include_bytes!("../assets/audio/swoosh.psau");

/// Every sound in the bank, in upload order. The index into [`BANK`].
#[derive(Clone, Copy)]
#[repr(u8)]
enum Snd {
    Hit,
    Pad,
    Goal,
    Boost,
    UiMove,
    UiTick,
    UiSelect,
    UiBack,
    UiDeny,
    UiChime,
    UiSaved,
    UiOpen,
    UiClose,
    Beep,
    Go,
    Whistle,
    Horn,
    Roar,
    Gasp,
    CrowdLoop,
    EngineLoop,
    SkidLoop,
    Jump,
    Flip,
    Land,
    Bounce,
    Net,
    Bump,
    Sonic,
    PadBig,
    Empty,
    Cheer,
}
const SND_COUNT: usize = Snd::Cheer as usize + 1;

/// Each sound's cooked bytes and the rate it was cooked at. Playback rates are
/// this times a pitch ratio.
const BANK: [(&[u8], u32); SND_COUNT] = [
    (HIT_SFX, 44_100),
    (PAD_SFX, 44_100),
    (GOAL_SFX, 44_100),
    (BOOST_SFX, 44_100),
    (include_bytes!("../assets/audio/ui_move.psau"), 22_050),
    (include_bytes!("../assets/audio/ui_tick.psau"), 22_050),
    (include_bytes!("../assets/audio/ui_select.psau"), 22_050),
    (include_bytes!("../assets/audio/ui_back.psau"), 22_050),
    (include_bytes!("../assets/audio/ui_deny.psau"), 22_050),
    (include_bytes!("../assets/audio/ui_chime.psau"), 22_050),
    (include_bytes!("../assets/audio/ui_saved.psau"), 22_050),
    (include_bytes!("../assets/audio/ui_open.psau"), 22_050),
    (include_bytes!("../assets/audio/ui_close.psau"), 22_050),
    (include_bytes!("../assets/audio/beep.psau"), 22_050),
    (include_bytes!("../assets/audio/go.psau"), 22_050),
    (include_bytes!("../assets/audio/whistle.psau"), 22_050),
    (include_bytes!("../assets/audio/horn.psau"), 22_050),
    (include_bytes!("../assets/audio/crowd_roar.psau"), 16_000),
    (include_bytes!("../assets/audio/crowd_gasp.psau"), 16_000),
    (include_bytes!("../assets/audio/crowd_loop.psau"), 16_000),
    (include_bytes!("../assets/audio/engine_loop.psau"), 22_050),
    (include_bytes!("../assets/audio/skid_loop.psau"), 22_050),
    (include_bytes!("../assets/audio/jump.psau"), 22_050),
    (include_bytes!("../assets/audio/flip.psau"), 22_050),
    (include_bytes!("../assets/audio/land.psau"), 22_050),
    (include_bytes!("../assets/audio/bounce.psau"), 22_050),
    (include_bytes!("../assets/audio/net.psau"), 22_050),
    (include_bytes!("../assets/audio/bump.psau"), 22_050),
    (include_bytes!("../assets/audio/sonic.psau"), 22_050),
    (include_bytes!("../assets/audio/pad_big.psau"), 22_050),
    (include_bytes!("../assets/audio/empty.psau"), 22_050),
    (include_bytes!("../assets/audio/crowd_cheer.psau"), 16_000),
];

/// A `.psau` is a 12 byte container header and a 20 byte audio header in
/// front of the ADPCM.
const PSAU_HEADER: usize = 32;
/// psx-sfx parks every sample on a self-looping silent block after it.
const PARKING_BYTES: u32 = 16;
/// First address in SPU RAM that is not the CD/capture area.
const SPU_SAMPLE_BASE: u32 = 0x1010;
/// The SPU's RAM.
const SPU_RAM_BYTES: u32 = 512 * 1024;

/// SPU RAM the whole bank takes, parking blocks included.
pub const BANK_BYTES: u32 = {
    let mut total = 0;
    let mut i = 0;
    while i < SND_COUNT {
        total += (BANK[i].0.len() - PSAU_HEADER) as u32 + PARKING_BYTES;
        i += 1;
    }
    total
};
// The build fails, rather than the console, if the bank outgrows the SPU.
const _: () = assert!(SPU_SAMPLE_BASE + BANK_BYTES <= SPU_RAM_BYTES);

// ---- lanes --------------------------------------------------------------------

const LANE_UI: u8 = 0;
const LANE_LOOP: u8 = 1;
const LANE_WORLD: u8 = 2;
const LANE_CAR: u8 = 3;
const LANE_SCENE: u8 = 4;

/// The 24 voices, split once. Menu sounds take turns on two voices, so a fast
/// run of clicks overlaps rather than restarting one. Loops never steal and are
/// never stolen: a lane of five holds both engines, both skids and the crowd.
/// The rest are `Smart`: a free voice, else one that is nearly done, else the
/// least important sound that is no louder than the new one.
const LANES: [Lane; 5] = [
    Lane::new(0x00_0003, Policy::RoundRobin),
    Lane::new(0x00_007C, Policy::FirstFree),
    Lane::new(0x00_0F80, Policy::Smart),
    Lane::new(0x01_F000, Policy::Smart),
    Lane::new(0x3E_0000, Policy::Smart),
];
/// Each car's boost hiss. Noise, not a sample, so outside the mixer.
const V_BOOST: [u8; 2] = [22, 23];
/// Only the sustained boost runs on noise.
const NOISE_MASK: u32 = (1 << 22) | (1 << 23);

/// How a sound is treated by the mixer, in [`Snd`] order: its lane, how much it
/// matters, the priority inside that class (smaller is more important), and
/// whether it loops until stopped.
const PROPS: [(u8, Class, i16, bool); SND_COUNT] = [
    (LANE_WORLD, Class::High, 1, false),    // Hit
    (LANE_WORLD, Class::Normal, 2, false),  // Pad
    (LANE_WORLD, Class::High, 0, false),    // Goal, and a wreck's bang
    (LANE_CAR, Class::Normal, 1, false),    // Boost swoosh
    (LANE_UI, Class::Normal, 0, false),     // UiMove
    (LANE_UI, Class::Normal, 0, false),     // UiTick
    (LANE_UI, Class::Normal, 0, false),     // UiSelect
    (LANE_UI, Class::Normal, 0, false),     // UiBack
    (LANE_UI, Class::Normal, 0, false),     // UiDeny
    (LANE_UI, Class::Normal, 0, false),     // UiChime
    (LANE_UI, Class::Normal, 0, false),     // UiSaved
    (LANE_UI, Class::Normal, 0, false),     // UiOpen
    (LANE_UI, Class::Normal, 0, false),     // UiClose
    (LANE_SCENE, Class::High, 1, false),    // Beep
    (LANE_SCENE, Class::High, 0, false),    // Go
    (LANE_SCENE, Class::Protected, 0, false), // Whistle
    (LANE_SCENE, Class::Protected, 0, false), // Horn
    (LANE_SCENE, Class::High, 2, false),    // Roar
    (LANE_SCENE, Class::Normal, 3, false),  // Gasp
    (LANE_LOOP, Class::Normal, 0, true),    // CrowdLoop
    (LANE_LOOP, Class::Normal, 0, true),    // EngineLoop
    (LANE_LOOP, Class::Normal, 0, true),    // SkidLoop
    (LANE_CAR, Class::Normal, 1, false),    // Jump
    (LANE_CAR, Class::Normal, 1, false),    // Flip
    (LANE_CAR, Class::Normal, 2, false),    // Land
    (LANE_WORLD, Class::Normal, 2, false),  // Bounce
    (LANE_WORLD, Class::Normal, 3, false),  // Net
    (LANE_WORLD, Class::Normal, 2, false),  // Bump
    (LANE_CAR, Class::High, 0, false),      // Sonic
    (LANE_WORLD, Class::Normal, 2, false),  // PadBig
    (LANE_CAR, Class::Normal, 3, false),    // Empty
    (LANE_SCENE, Class::Normal, 2, false),  // Cheer
];

// ---- the SPU as the mixer's hardware --------------------------------------------

/// The envelope a held voice keys on with: a quick attack onto a full sustain,
/// and an audible release on key off.
const HELD: Adsr = Adsr {
    // Attack shift 0x10 (quick), decay 0x0, sustain level 0xF (hold full).
    lower: (0x10 << 8) | 0xF,
    // Release shift 0x08: quick enough that letting go of boost is audible as
    // it stopping. 0x18 took several seconds to fade, so the hiss outlived
    // the tank by a wide margin.
    upper: 0x08 | (0x7F << 6),
};

/// The SPU, as `psx-sfx` drives it: each method is the register writes it
/// names. A one-shot gets the SDK's default envelope (instant attack, hold,
/// about 100 ms release, which plays the sample out and stops at the END flag
/// on silicon), a loop the held one above. Both point the repeat address at
/// silence first, which `configure_sample` does.
struct SpuHw {
    /// The rate each voice's sample was cooked at, for pitch updates.
    rate: [u32; 24],
}

impl SpuHw {
    fn pitch(&self, voice: u8, pitch_q12: u16) -> Pitch {
        Pitch::for_frequency(self.rate[voice as usize] * pitch_q12 as u32 / 4096, 44_100)
    }
}

impl Hw for SpuHw {
    fn configure(&mut self, voice: u8, sample: &SfxSample, looped: bool, pitch_q12: u16) {
        let v = spu::Voice::new(voice);
        self.rate[voice as usize] = sample.sample_rate_hz();
        let adsr = if looped { HELD } else { Adsr::default_tone() };
        v.configure_sample(sample.addr(), sample.sample_rate_hz(), Volume::SILENCE, adsr);
        v.set_pitch(self.pitch(voice, pitch_q12));
    }

    fn set_volume(&mut self, voice: u8, left: i16, right: i16) {
        spu::Voice::new(voice).set_volume(Volume(left), Volume(right));
    }

    fn set_pitch(&mut self, voice: u8, pitch_q12: u16) {
        spu::Voice::new(voice).set_pitch(self.pitch(voice, pitch_q12));
    }

    fn key_on(&mut self, mask: u32) {
        spu::Voice::start(mask);
    }

    fn key_off(&mut self, mask: u32) {
        spu::Voice::release(mask);
    }
}

static mut HW: SpuHw = SpuHw { rate: [44_100; 24] };
/// The mixer's clock: the frame clock, in `TICKS_HZ`.
const TICKS_HZ: u32 = 60;
static mut MIXER: Mixer<5> = Mixer::new(LANES, TICKS_HZ);
/// Frames elapsed since boot, the clock the mixer runs on.
static mut TICK: u32 = 0;

fn mixer() -> &'static mut Mixer<5> {
    // SAFETY: the game is single threaded and none of this is re-entered.
    unsafe { &mut *(&raw mut MIXER) }
}

fn hw() -> &'static mut SpuHw {
    // SAFETY: as `mixer`.
    unsafe { &mut *(&raw mut HW) }
}

/// A sound with the mixer's treatment for it, filled in at upload.
const UNSET: Sound =
    Sound::one_shot(SoundId(0), SfxSample::resident(SpuAddr::new(SPU_SAMPLE_BASE), 22_050, 0), 0);
static mut SOUNDS: [Sound; SND_COUNT] = [UNSET; SND_COUNT];
/// The menu's own samples are contiguous in the bank (`UiMove` to `UiClose`),
/// so "everything but the menu" is two address ranges: below `UI_LO` and from
/// `UI_HI` up. [`stop_all`] uses them.
static mut UI_LO: u32 = 0;
static mut UI_HI: u32 = 0;

// ---- levels -------------------------------------------------------------------
//
// Every sample is cooked to the same peak, so what a sound adds to the mix is
// its RMS times these. The figures come from `tools/sfx-synth`'s report
// ("RMS dBFS"): each is 16384 * 10^((target - rms) / 20), where the target is
// the sound's RMS at the voice in dBFS, before the master volume.
//
// The yardstick is the music. Alone it mixes to -24.8 dBFS RMS at the SPU
// output (headless dump of the music disc, 2026-10-15): its -15 LUFS less the
// 10.1 dB the master volume 0x2800 takes off, which a menu click measured the
// same way confirms (a sound's in-mix peak is its sample peak, plus
// 20 log10(volume / 16384), minus 10.1). In the terms of the targets below,
// which are before the master, the music is -15.
//
// Menu sounds are 10 dB under that, so a click is heard but never competes.
// The countdown and the go are a few dB under it: they carry information and
// sit under the music's peaks. The engine, at top speed, and the crowd are
// beds, 8 and 14 dB under, there to be felt when the music thins out. The
// horn, the whistle and the roar are over it, because they are the match
// telling you something; the horn and the roar run at the top of the range
// because the explosion they share a goal with is already loud. Ball hits,
// pads and the goal explosion keep the levels the game already shipped with.
//
// These are the level a sound has at its loudest and dead centre. Distance
// takes it down from there and the pan moves it between the ears; a sound at
// the camera's own car is at full level in both.

const L_UI_MOVE: i16 = 0x1275;
const L_UI_TICK: i16 = 0x0C9F;
const L_UI_SELECT: i16 = 0x1275;
const L_UI_BACK: i16 = 0x0F87;
const L_UI_DENY: i16 = 0x08D5;
const L_UI_CHIME: i16 = 0x1781;
const L_UI_SAVED: i16 = 0x15AF;
const L_UI_OPEN: i16 = 0x138D;
const L_UI_CLOSE: i16 = 0x131B;
const L_BEEP: i16 = 0x1B1B;
const L_GO: i16 = 0x1B4D;
const L_WHISTLE: i16 = 0x39B4;
const L_HORN: i16 = 0x3800;
const L_ROAR: i16 = 0x3C00;
const L_GASP: i16 = 0x1FB5;
const L_CROWD: i16 = 0x0E32;
const L_ENGINE: i16 = 0x08F0;
const L_SKID: i16 = 0x09AF;
const L_JUMP: i16 = 0x180D;
const L_FLIP: i16 = 0x19C6;
const L_LAND: i16 = 0x2393;
const L_BOUNCE: i16 = 0x2776;
const L_NET: i16 = 0x28D9;
const L_BUMP: i16 = 0x2EE6;
const L_SONIC: i16 = 0x35D9;
const L_PAD_BIG: i16 = 0x2B44;
const L_EMPTY: i16 = 0x1CEC;
/// The chant after a goal sits 7 dB under the horn; at a kickoff it is softer.
const L_CHEER: i16 = 0x2E5C;
const L_CHEER_KICKOFF: i16 = 0x1D42;

// ---- space --------------------------------------------------------------------

/// A distance falloff that holds the full level out to `start` uu, then falls
/// in a straight line to `floor` at `clip` and stays there. Unity peak: the
/// sound's own level is applied after, so one curve serves every level.
const fn ramp(start: i32, clip: i32, floor: i16) -> Curve {
    let peak = 0x3FFF;
    Curve::Ramp {
        peak,
        start,
        // Level lost per uu in 1/256ths. Rounded up so the fall reaches the
        // floor by `clip`.
        slope: (((peak - floor) as i32 * 256) / (clip - start) + 1) as u16,
        shift: 8,
        floor,
    }
}

/// A car, its loops and what it throws off. The chase camera sits 800 to 1100
/// uu from its own car, so the car it follows is at full level; the other one
/// is heard across the pitch only when it is close, and not at all past 9000.
const CURVE_CAR: Curve = ramp(1500, 9000, 0);
/// The ball. It is the thing both players are watching, so it carries across
/// the whole arena, quietly.
const CURVE_BALL: Curve = ramp(1800, 14_000, 0x0A00);
/// A boost pad being taken: where a car is, so it is a car sound.
const CURVE_PAD: Curve = ramp(1500, 6000, 0);
/// A goal, the biggest sound in the match: always heard, wherever you are.
const CURVE_GOAL: Curve = ramp(4000, 20_000, 0x1800);

/// World units are sub-units, 64 to the uu.
const SPACE: Space =
    Space::new(nitroxide_sim::FP as u8, PanLaw::EqualPower(Center::Unity)).with_cull(0x0300);

/// The ears, rebuilt from the cameras every [`update`]. One listener in a
/// one-player match, two in split screen.
static mut EARS_ONE: Option<Listeners<1>> = None;
static mut EARS_TWO: Option<Listeners<2>> = None;
static mut SPLIT: bool = false;

fn listener(slot: usize, car: &Car) -> Listener {
    match draw::listener_pose(slot) {
        Some((eye, yaw)) => Listener::new([uu(eye.0), uu(eye.1), uu(eye.2)], yaw),
        // No frame drawn yet since the last reset: hear from the car.
        None => Listener::new([car.p.x, car.p.y + uu(330), car.p.z], car.yaw),
    }
}

fn refresh_ears(s: &Sim, split: bool) {
    unsafe {
        SPLIT = split;
        if split {
            EARS_TWO = Some(Listeners::new([listener(0, &s.car), listener(1, &s.opponent)]));
        } else {
            EARS_ONE = Some(Listeners::new([listener(0, &s.car)]));
        }
    }
}

/// Where `pos` (sub-units) is, for the camera or cameras, or `None` when it is
/// too far to be worth a voice.
fn place(pos: [i32; 3], curve: &Curve, rule: ListenerRule) -> Option<Gains> {
    let emitter = Emitter::new(pos);
    unsafe {
        if SPLIT {
            let ears = (*(&raw const EARS_TWO)).as_ref()?;
            gains(&SPACE, ears, rule, &emitter, curve)
        } else {
            let ears = (*(&raw const EARS_ONE)).as_ref()?;
            gains(&SPACE, ears, rule, &emitter, curve)
        }
    }
}

/// A sound's own level through the placement's ears.
fn ears(g: Option<Gains>, vol: i16) -> (i16, i16) {
    match g {
        Some(g) => (
            ((g.left.max(0) as i32 * vol as i32) >> 14) as i16,
            ((g.right.max(0) as i32 * vol as i32) >> 14) as i16,
        ),
        None => (0, 0),
    }
}

fn at_car(c: &Car) -> [i32; 3] {
    [c.p.x, c.p.y, c.p.z]
}

// ---- starting sounds ------------------------------------------------------------

/// `1.0` in the mixer's pitch: as cooked.
const UNITY: u16 = 4096;

/// A pitch ratio of `num/16`: 16 is as cooked, 17 is about a semitone up, 24 is
/// a fifth.
const fn ratio(num: u32) -> u16 {
    (num * 256) as u16
}

/// The pitch that plays `snd` at `rate_hz` instead of the rate it was cooked at.
fn at_rate(snd: Snd, rate_hz: u32) -> u16 {
    (rate_hz * 4096 / BANK[snd as usize].1) as u16
}

/// Start `snd` on its lane at these ear levels. `key` is non-zero to replace
/// the sound already playing with the same key (a flip taking over its jump).
fn start(snd: Snd, left: i16, right: i16, pitch_q12: u16, key: u16) -> Option<Handle> {
    // SAFETY: the table is written once, at boot.
    let sound = unsafe { (*(&raw const SOUNDS))[snd as usize] };
    mixer().play(hw(), &sound, Play::new(left, right).with_pitch(pitch_q12).with_key(key))
}

/// A sound that is not in the arena: both ears the same.
fn flat(snd: Snd, vol: i16, pitch_q12: u16) -> Option<Handle> {
    start(snd, vol, vol, pitch_q12, 0)
}

/// A sound from `pos`, panned and attenuated for the camera or cameras.
fn at(
    snd: Snd,
    vol: i16,
    pitch_q12: u16,
    key: u16,
    pos: [i32; 3],
    curve: &Curve,
    rule: ListenerRule,
) -> Option<Handle> {
    let (l, r) = ears(place(pos, curve, rule), vol);
    start(snd, l, r, pitch_q12, key)
}

/// Bring the SPU up, upload the samples, set the mixer up. Once, at boot.
pub fn init() {
    spu::init();
    spu::set_main_volume(Volume(0x2800), Volume(0x2800));
    // Route the CD's own audio into the mix, for the XA music `music.rs`
    // plays. Without this the drive plays to nothing.
    spu::set_cd_volume(CdVolume::MAX, CdVolume::MAX);
    spu::enable_cd_audio(true);
    // Noise clock for the boost hiss: dark enough to read as a jet rather
    // than as static.
    spu::set_noise_clock(8, 2);
    spu::Voice::set_noise_mask(NOISE_MASK);

    // ADPCM payloads are whole blocks, so advancing by the byte length keeps
    // every following start address aligned. psx-sfx lays the bank down in
    // sequence and parks every one-shot on its own silent block after it, so a
    // finished sound cannot run into the next sample. The mixer cuts a one-shot
    // off by its length (it knows the block count from the upload).
    let mut bank = Bank::new(SpuAddr::new(SPU_SAMPLE_BASE));
    for (i, (bytes, _)) in BANK.iter().enumerate() {
        let sample = bank.upload(bytes);
        let (lane, class, prio, looped) = PROPS[i];
        let id = SoundId(i as u16);
        let sound = if looped {
            Sound::looped(id, sample, lane)
        } else {
            Sound::one_shot(id, sample, lane)
        };
        unsafe { (*(&raw mut SOUNDS))[i] = sound.with_class(class).with_prio(prio) };
        if i == Snd::UiMove as usize {
            unsafe { UI_LO = sample.addr().byte_offset() };
        }
        if i == Snd::Beep as usize {
            unsafe { UI_HI = sample.addr().byte_offset() };
        }
    }

    // The boost is held rather than struck.
    for v in V_BOOST {
        spu::Voice::new(v).set_adsr(HELD);
    }
}

/// What the last tick looked like, for the sounds that are an edge between two
/// ticks rather than a flag the sim raises. Everything here is read from the
/// sim; none of it is written back.
#[derive(Clone, Copy)]
struct CarSeen {
    jumps: u8,
    grounded: bool,
    v: (i32, i32, i32),
    supersonic: bool,
    bump: bool,
    boosting: bool,
    /// Engine and skid are looped sounds: the handle while one runs, and the
    /// pitch step last written, so an unchanged note costs no register writes.
    engine: Option<Handle>,
    note: u16,
    skid: Option<Handle>,
    skid_step: u16,
}
const CAR_AT_REST: CarSeen = CarSeen {
    jumps: 0,
    grounded: true,
    v: (0, 0, 0),
    supersonic: false,
    bump: false,
    boosting: false,
    engine: None,
    note: 0,
    skid: None,
    skid_step: 0,
};

#[derive(Clone, Copy)]
struct Seen {
    cars: [CarSeen; 2],
    ball_v: (i32, i32, i32),
    /// The countdown digit last sounded, and whole seconds left on the clock.
    hold: u16,
    clock: u16,
    crowd: Option<Handle>,
}
const SEEN_AT_REST: Seen = Seen {
    cars: [CAR_AT_REST; 2],
    ball_v: (0, 0, 0),
    hold: 0,
    clock: 0,
    crowd: None,
};
static mut SEEN: Seen = SEEN_AT_REST;

/// Goal freeze seen last tick, to catch the moment one starts.
static mut LAST_FREEZE: u16 = 0;
/// A car has been on the ball since the ball was last compared with itself.
static mut BALL_TOUCHED: bool = false;

/// Set by the pause menu. Muting keys everything off at once rather than
/// letting a held boost hiss on under a silent match.
static mut MUTED: bool = false;

/// Is sound currently off?
pub fn muted() -> bool {
    unsafe { MUTED }
}

/// Turn sound off or back on.
pub fn set_muted(off: bool) {
    unsafe { MUTED = off };
    if off {
        stop_all();
        mixer().stop_all(hw());
    }
}

/// The audio clock, once per update in every phase. The mixer retires what has
/// finished even while muted: a sound started before the mute still has a voice
/// to key off.
pub fn tick() {
    let now = unsafe {
        TICK = TICK.wrapping_add(1);
        TICK
    };
    mixer().tick(hw(), now);
}

// ---- menus --------------------------------------------------------------------

/// A menu event that has a sound.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Ui {
    /// The cursor moved to another row or page.
    Move,
    /// A value changed, in the direction given: -1 down, 1 up.
    Adjust(i8),
    /// A choice was made: a row opened, a match chosen.
    Select,
    /// Backed out one level, or a lock-in was undone.
    Back,
    /// Refused or failed: no card, a save that did not take.
    Deny,
    /// A seat locked its choice in on the garage screen.
    Ready,
    /// Settings written to the card.
    Saved,
    /// A panel opened: settings, pause.
    Open,
    /// A panel closed.
    Close,
    /// The match has started.
    Go,
}

/// Sound a menu event. Silent while sound is off.
pub fn ui(event: Ui) {
    if unsafe { MUTED } {
        return;
    }
    let (snd, vol, pitch) = match event {
        Ui::Move => (Snd::UiMove, L_UI_MOVE, UNITY),
        // Up is a step brighter than as cooked and down a step darker, so
        // the direction of a slider can be heard as well as seen.
        Ui::Adjust(dir) => (Snd::UiTick, L_UI_TICK, ratio(if dir >= 0 { 18 } else { 14 })),
        Ui::Select => (Snd::UiSelect, L_UI_SELECT, UNITY),
        Ui::Back => (Snd::UiBack, L_UI_BACK, UNITY),
        Ui::Deny => (Snd::UiDeny, L_UI_DENY, UNITY),
        Ui::Ready => (Snd::UiChime, L_UI_CHIME, UNITY),
        Ui::Saved => (Snd::UiSaved, L_UI_SAVED, UNITY),
        Ui::Open => (Snd::UiOpen, L_UI_OPEN, UNITY),
        Ui::Close => (Snd::UiClose, L_UI_CLOSE, UNITY),
        // The match start is the same call as a kickoff's go.
        Ui::Go => {
            let _ = flat(Snd::Cheer, L_CHEER_KICKOFF, UNITY);
            (Snd::Go, L_GO, UNITY)
        }
    };
    let _ = flat(snd, vol, pitch);
}

/// The final whistle. Called after [`stop_all`], which would otherwise cut it.
pub fn match_end() {
    if unsafe { MUTED } {
        return;
    }
    let _ = flat(Snd::Whistle, L_WHISTLE, UNITY);
}

// ---- the match ----------------------------------------------------------------

/// Fuel under one tick's burn (`BOOST_DRAIN` in the sim) is an empty tank.
const BOOST_DRY: i32 = 36;

/// Below this the contact is a nudge, not a hit, and sounding it turns
/// dribbling into a rattle.
const HIT_FLOOR: i32 = 260;
/// Impulse that counts as a full-volume hit; anything above just saturates.
const HIT_LOUD: i32 = 2600;
/// The cheer after a goal starts when this many ticks of the 150 tick freeze
/// are left: a second into the celebration, so its three seconds end inside the
/// roar's four and the horn has the front of the goal to itself.
const CHEER_AT: u16 = 90;

/// Speed, in sim sub-units a tick, past which the engine is at its highest
/// note: a little above the supersonic line.
const ENGINE_TOP: i32 = 2500;
/// The engine's note at standstill and at full speed, in 1/16ths of the loop's
/// own rate.
const ENGINE_LOW: u32 = 16;
const ENGINE_HIGH: u32 = 44;
/// Steps the engine's pitch is quantised to, so a car cruising does not
/// rewrite the voice's pitch register sixty times a second.
const ENGINE_STEPS: i32 = 16;
/// Powerslide has to be this far in, and the car this fast, to squeal.
const SKID_SLIDE: i32 = 380;
const SKID_SPEED: i32 = 500;
/// Smallest component a bounce has to turn around, in sim sub-units a tick
/// (about uu a second), to be sounded. Rolling and settling stay under it.
const BOUNCE_FLOOR: i32 = 300;
const BOUNCE_LOUD: i32 = 1600;
/// A car landing has to change velocity by this much, summed over the axes.
const LAND_FLOOR: i32 = 260;
const LAND_LOUD: i32 = 1500;
/// Ball speed (summed over the axes) into the net at which it is soft and at
/// which it is full volume.
const NET_SOFT: i32 = 700;
const NET_HARD: i32 = 2600;
/// The ticks a boost pad's respawn timer is set to when it is taken (small and
/// big, `Pad::respawn` in the sim).
const PAD_SMALL_TICKS: u16 = 240;
const PAD_BIG_TICKS: u16 = 600;

fn car_of(s: &Sim, k: usize) -> &Car {
    if k == 0 {
        &s.car
    } else {
        &s.opponent
    }
}

/// Read one tick of the match and make the noises it earned. `split` is a
/// two-player match: two cameras, so two listeners.
pub fn update(s: &Sim, split: bool) {
    if unsafe { MUTED } {
        return;
    }
    refresh_ears(s, split);
    let ball = [s.ball.p.x, s.ball.p.y, s.ball.p.z];

    // Ball contact. Volume tracks how hard, so a dribble whispers and a
    // clearance cracks, and pitch tracks it too: the same sample played a
    // fifth up reads as a harder strike rather than as a louder one.
    if s.hit > HIT_FLOOR {
        let loud = ((s.hit - HIT_FLOOR).min(HIT_LOUD) * 0x2A00 / HIT_LOUD) as i16;
        let rate = 30_000 + (s.hit.min(HIT_LOUD) as u32) * 6;
        let _ = at(
            Snd::Hit,
            loud,
            at_rate(Snd::Hit, rate),
            0,
            ball,
            &CURVE_BALL,
            ListenerRule::Loudest,
        );
    }
    if s.hit != 0 {
        unsafe { BALL_TOUCHED = true };
    }

    if s.pad_taken {
        // The pad whose timer was just set to its respawn is the one taken. A
        // big one fills the tank and sounds it.
        let taken = s.pad_timers.iter().zip(PADS.iter()).position(|(t, p)| {
            *t == if p.big { PAD_BIG_TICKS } else { PAD_SMALL_TICKS }
        });
        if let Some(i) = taken {
            let p = &PADS[i];
            let pos = [uu(p.x), uu(30), uu(p.z)];
            if p.big {
                let _ = at(Snd::PadBig, L_PAD_BIG, UNITY, 0, pos, &CURVE_PAD, ListenerRule::Loudest);
            } else {
                let _ = at(Snd::Pad, 0x1E00, UNITY, 0, pos, &CURVE_PAD, ListenerRule::Loudest);
            }
        }
    }

    // A demolition already arrives as a one-tick event from the simulation.
    // Use the goal explosion's resident sample at its natural, sharper pitch:
    // it reads as a nearby car-sized blast while the pitched-down goal still
    // owns the large arena-wide boom. It sounds where the wreck is.
    if s.demo {
        let wreck = if s.car.demo_timer >= s.opponent.demo_timer { &s.car } else { &s.opponent };
        let _ = at(
            Snd::Goal,
            0x2C00,
            UNITY,
            0,
            at_car(wreck),
            &CURVE_BALL,
            ListenerRule::Loudest,
        );
    }

    // Boost, per car: keyed on the tick it starts and off the tick it stops,
    // so it holds cleanly instead of restarting sixty times a second. The
    // swoosh fires once on the same edge and is left to ring out on its own.
    let seen = unsafe { &mut *(&raw mut SEEN) };
    for k in 0..2 {
        let car = car_of(s, k);
        if car.boosting != seen.cars[k].boosting {
            let v = spu::Voice::new(V_BOOST[k]);
            let pos = at_car(car);
            if car.boosting {
                let (l, r) = ears(place(pos, &CURVE_CAR, ListenerRule::Nearest), 0x1000);
                v.set_volume(Volume(l), Volume(r));
                v.set_pitch(Pitch::for_frequency(8_000, 44_100));
                spu::Voice::start(1 << V_BOOST[k]);
                let _ = at(Snd::Boost, 0x1C00, UNITY, 0, pos, &CURVE_CAR, ListenerRule::Loudest);
            } else {
                spu::Voice::release(1 << V_BOOST[k]);
                // The tank ran dry under the player's thumb: a sputter, so the
                // boost running out is heard and not only seen on the gauge.
                // Dry is less than one tick of fuel (the sim's BOOST_DRAIN, 36),
                // not zero: the burn stops while a remainder is left in the tank.
                if car.boost < BOOST_DRY {
                    let _ = at(Snd::Empty, L_EMPTY, UNITY, 0, pos, &CURVE_CAR, ListenerRule::Loudest);
                }
            }
            seen.cars[k].boosting = car.boosting;
        }
    }

    // A goal: catch the tick the freeze begins. The explosion is pitched
    // down, because the clip is a short arcade explosion and the goal wants
    // weight rather than brightness; the horn and the crowd go up with it,
    // each as a sound of its own. The bang is at the net, heard from the
    // middle of the cameras, since neither player owns it.
    let freeze = s.goal_freeze;
    if freeze > 0 && unsafe { LAST_FREEZE } == 0 {
        let _ = at(
            Snd::Goal,
            0x3000,
            at_rate(Snd::Goal, 32_000),
            0,
            ball,
            &CURVE_GOAL,
            ListenerRule::Centroid,
        );
        let _ = flat(Snd::Horn, L_HORN, UNITY);
        let _ = flat(Snd::Roar, L_ROAR, UNITY);
        // The ball into the netting. The world stops the instant it crosses
        // the line, so it never reaches the back of the net by itself: this
        // is that thump, as hard as the ball was going.
        let v = s.ball.v;
        let pace = (v.x.abs() + v.y.abs() + v.z.abs()).clamp(NET_SOFT, NET_HARD);
        let level = L_NET as i32 * pace / NET_HARD;
        let _ = at(Snd::Net, level as i16, UNITY, 0, ball, &CURVE_GOAL, ListenerRule::Centroid);
    }
    if freeze == CHEER_AT {
        let _ = flat(Snd::Cheer, L_CHEER, UNITY);
    }
    unsafe { LAST_FREEZE = freeze };

    // Live play: not the goal celebration and not the kickoff countdown. The
    // kickoff puts both cars and the ball back on their spots in one tick, and
    // that is a teleport, not a landing or a bounce.
    let live = s.goal_freeze == 0 && s.kickoff_hold == 0;
    // The sounds that are an edge in the sim's state are sampled every second
    // tick, and the held voices every second as well (their placement moves
    // with the camera, and a pan that steps fifteen times a second is heard as
    // a zipper). An edge in this state lasts many ticks (a jump is airtime, a
    // bump a cooldown, a supersonic spell a second), so the lag is a fiftieth
    // of a second at worst. The one-tick flags above (a hit, a pad, a wreck,
    // a goal) are read every tick.
    let tick = unsafe { TICK };
    if tick & 1 == 0 {
        car_sounds(s, seen, live);
        ball_sounds(s, seen, live);
        clock_sounds(s, seen);
        held_sounds(s, seen, live);
    }
}

/// Jump, flip, landing, supersonic, bump: each car's own edges.
fn car_sounds(s: &Sim, seen: &mut Seen, live: bool) {
    for k in 0..2 {
        let car = car_of(s, k);
        let cs = &mut seen.cars[k];
        let pos = at_car(car);
        // A car's jump and flip share a key, so a flip takes over its jump.
        let key = 1 + k as u16;
        if car.jumps_used != cs.jumps {
            // Only a rise is a jump; the count falls back to zero on landing.
            if car.jumps_used > cs.jumps {
                let (snd, vol, pitch) = if car.jumps_used == 1 {
                    (Snd::Jump, L_JUMP, UNITY)
                } else if car.dodge_timer > 0 {
                    (Snd::Flip, L_FLIP, UNITY)
                } else {
                    // A double jump with no direction: the jump again, higher.
                    (Snd::Jump, L_JUMP, ratio(22))
                };
                let _ = at(snd, vol, pitch, key, pos, &CURVE_CAR, ListenerRule::Loudest);
            }
            cs.jumps = car.jumps_used;
        }

        if live && car.grounded && !cs.grounded {
            // How hard it came down: the velocity the contact took off it, summed
            // over the axes (no square root for a sound).
            let d = (car.v.x - cs.v.0).abs() + (car.v.y - cs.v.1).abs() + (car.v.z - cs.v.2).abs();
            if d > LAND_FLOOR {
                let level = d.min(LAND_LOUD) * L_LAND as i32 / LAND_LOUD;
                let _ = at(
                    Snd::Land,
                    level.max(0x0800) as i16,
                    UNITY,
                    0,
                    pos,
                    &CURVE_CAR,
                    ListenerRule::Loudest,
                );
            }
        }
        cs.grounded = car.grounded;
        // Only the airborne velocity is wanted, for the landing that follows.
        if !car.grounded {
            cs.v = (car.v.x, car.v.y, car.v.z);
        }

        if car.supersonic != cs.supersonic {
            if car.supersonic {
                let _ = at(Snd::Sonic, L_SONIC, UNITY, 0, pos, &CURVE_CAR, ListenerRule::Loudest);
            }
            cs.supersonic = car.supersonic;
        }
    }

    // Either car bumping the other starts that car's cooldown. A demolition
    // starts it too, and has its own, bigger sound: a wrecked car in play
    // means the bang was that, not a bump. The bump is where the two meet.
    let bump = [s.car.bump_cool > 0, s.opponent.bump_cool > 0];
    let wreck = s.car.wrecked() || s.opponent.wrecked();
    if ((bump[0] && !seen.cars[0].bump) || (bump[1] && !seen.cars[1].bump)) && !wreck {
        let (a, b) = (&s.car.p, &s.opponent.p);
        let mid = [(a.x + b.x) / 2, (a.y + b.y) / 2, (a.z + b.z) / 2];
        let _ = at(Snd::Bump, L_BUMP, UNITY, 0, mid, &CURVE_BALL, ListenerRule::Loudest);
    }
    seen.cars[0].bump = bump[0];
    seen.cars[1].bump = bump[1];
}

/// Ball against the floor, a wall, the ceiling, the goal frame and the net.
/// Contact with a car is the hit sound's; this is the ball meeting the world.
fn ball_sounds(s: &Sim, seen: &mut Seen, live: bool) {
    let v = s.ball.v;
    let (px, py, pz) = seen.ball_v;
    seen.ball_v = (v.x, v.y, v.z);
    // A car on the ball since the last look is the hit sound's, and it turns
    // the ball round just as a wall does.
    let touched = unsafe { BALL_TOUCHED };
    unsafe { BALL_TOUCHED = false };
    if touched || !live {
        return;
    }
    // The strongest axis that turned around this tick. A bounce flips the sign
    // of the component it bounced on; gravity alone never does it by 300.
    let turned = |before: i32, after: i32| {
        if (before < 0) != (after < 0) && before.abs() > BOUNCE_FLOOR {
            before.abs()
        } else {
            0
        }
    };
    let (ty, tx, tz) = (turned(py, v.y), turned(px, v.x), turned(pz, v.z));
    let hardest = ty.max(tx).max(tz);
    if hardest == 0 {
        return;
    }
    let level = (hardest.min(BOUNCE_LOUD) * L_BOUNCE as i32 / BOUNCE_LOUD) as i16;
    let p = s.ball.p;
    let pos = [p.x, p.y, p.z];
    let bounce = |pitch: u16| {
        let _ = at(Snd::Bounce, level, pitch, 0, pos, &CURVE_BALL, ListenerRule::Loudest);
    };
    if tz > 0
        && p.z.abs() > uu(HALF_Z - 220)
        && p.x.abs() < uu(GOAL_HALF_W + 120)
        && p.y < uu(GOAL_H + 160)
    {
        // Turned back at the mouth of the goal: a post or the bar. The ball
        // rings off it, and the crowd draws its breath.
        bounce(ratio(28));
        let _ = flat(Snd::Gasp, L_GASP, UNITY);
    } else if ty > 0 {
        // The floor is lower than a wall, the ceiling a touch higher.
        bounce(ratio(if v.y > 0 { 16 } else { 19 }));
    } else {
        bounce(ratio(21));
    }
}

/// The kickoff countdown and the last seconds of a timed match.
fn clock_sounds(s: &Sim, seen: &mut Seen) {
    // The banner's digit: 3 for the first second of the hold, then 2, then 1,
    // and 0 once play is released. Compared as a digit rather than for exact
    // ticks, so a tick skipped by the half-rate sampling cannot lose a beep.
    let digit = (s.kickoff_hold + 59) / 60;
    if digit != seen.hold {
        if digit > 0 {
            let _ = flat(Snd::Beep, L_BEEP, UNITY);
        } else {
            let _ = flat(Snd::Go, L_GO, UNITY);
            let _ = flat(Snd::Cheer, L_CHEER_KICKOFF, UNITY);
        }
        seen.hold = digit;
    }
    // Whole seconds on the clock. Each second that falls below five is a tick
    // of the same beep, higher; the last one is a second before the whistle.
    let secs = (s.clock / 60) as u16;
    if secs < seen.clock {
        if secs < 5 && matches!(s.win_condition, WinCondition::TimeLimit(_)) {
            let _ = flat(Snd::Beep, L_BEEP, ratio(20));
        }
    }
    seen.clock = secs;
}

/// The looped sounds: the crowd, each car's engine and tyre squeal, and the
/// level of each car's boost hiss. Their placement follows the cameras, so the
/// levels are rewritten when the mixer sees them change.
fn held_sounds(s: &Sim, seen: &mut Seen, live: bool) {
    let crowd_alive = seen.crowd.is_some_and(|h| mixer().is_alive(h));
    if live && !crowd_alive {
        seen.crowd = flat(Snd::CrowdLoop, L_CROWD, UNITY);
    } else if !live {
        if let Some(h) = seen.crowd.take() {
            mixer().stop(hw(), h);
        }
    }

    for k in 0..2 {
        let car = car_of(s, k);
        let cs = &mut seen.cars[k];
        let speed = car.v.len();
        let engine_on = live && !car.wrecked();
        let sliding = engine_on && car.grounded && car.slide > SKID_SLIDE && speed > SKID_SPEED;
        if !engine_on && !cs.boosting && cs.engine.is_none() {
            continue;
        }
        // One placement serves the car's engine, skid and hiss: all three come
        // from the same spot. A loop is heard from the listener nearest it, so
        // its pan does not flip as two views' levels cross.
        let g = place(at_car(car), &CURVE_CAR, ListenerRule::Nearest);

        if engine_on {
            let step = (speed.clamp(0, ENGINE_TOP) * ENGINE_STEPS / ENGINE_TOP) as u32;
            // The note climbs with speed and the level follows it, halved in the
            // air where the wheels are not loaded.
            let note =
                ENGINE_LOW * 256 + (ENGINE_HIGH - ENGINE_LOW) * 256 * step / ENGINE_STEPS as u32;
            let mut level = L_ENGINE as i32 * (12 + step as i32) / (12 + ENGINE_STEPS);
            if !car.grounded {
                level /= 2;
            }
            let (l, r) = ears(g, level as i16);
            // The note is in 1/16ths of the loop's own rate times 256, which is
            // the mixer's pitch ratio as it stands.
            let pitch = note as u16;
            match cs.engine.filter(|h| mixer().is_alive(*h)) {
                Some(h) => {
                    mixer().update(hw(), h, l, r);
                    if cs.note != pitch {
                        mixer().set_pitch(hw(), h, pitch);
                        cs.note = pitch;
                    }
                }
                None => {
                    cs.engine = start(Snd::EngineLoop, l, r, pitch, 0);
                    cs.note = pitch;
                }
            }
        } else if let Some(h) = cs.engine.take() {
            mixer().stop(hw(), h);
        }

        // Tyre squeal: only while the powerslide is properly in and the car is
        // moving, louder the deeper it goes.
        if sliding {
            let depth =
                (car.slide - SKID_SLIDE).clamp(0, 1024 - SKID_SLIDE) * 3 / (1024 - SKID_SLIDE);
            let level = L_SKID as i32 / 4 * (depth + 1);
            let step = (speed.clamp(0, ENGINE_TOP) * 8 / ENGINE_TOP) as u16;
            let (l, r) = ears(g, level as i16);
            let pitch = (12 + step) * 256;
            match cs.skid.filter(|h| mixer().is_alive(*h)) {
                Some(h) => {
                    mixer().update(hw(), h, l, r);
                    if cs.skid_step != step {
                        mixer().set_pitch(hw(), h, pitch);
                        cs.skid_step = step;
                    }
                }
                None => {
                    cs.skid = start(Snd::SkidLoop, l, r, pitch, 0);
                    cs.skid_step = step;
                }
            }
        } else if let Some(h) = cs.skid.take() {
            mixer().stop(hw(), h);
        }

        if cs.boosting {
            let (l, r) = ears(g, 0x1000);
            spu::Voice::new(V_BOOST[k]).set_volume(Volume(l), Volume(r));
        }
    }
}

/// Silence the match: every voice but the menu's. Used on pause, on leaving
/// the match and when sound is turned off, so a held boost does not hiss over
/// the results screen and an engine does not idle under the pause menu.
///
/// What the last tick looked like is kept, so resuming mid-flight does not
/// replay the jump or the goal horn the pause interrupted; the held sounds are
/// stopped, so their flags are cleared and they start again on resume.
pub fn stop_all() {
    // The menu's samples sit in one block of the bank; everything else is the
    // match's.
    let (lo, hi) = unsafe { (UI_LO, UI_HI) };
    mixer().stop_range(hw(), 0, lo);
    mixer().stop_range(hw(), hi, u32::MAX);
    spu::Voice::release(NOISE_MASK);
    unsafe {
        let seen = &mut *(&raw mut SEEN);
        seen.crowd = None;
        for c in seen.cars.iter_mut() {
            c.boosting = false;
            c.engine = None;
            c.skid = None;
        }
    }
}

/// A new match is about to start: silence the old one and forget it, so the
/// first tick of the next is compared with a match at rest rather than with
/// the last tick of the last one.
pub fn new_match() {
    stop_all();
    unsafe {
        LAST_FREEZE = 0;
        SEEN = SEEN_AT_REST;
    }
}
