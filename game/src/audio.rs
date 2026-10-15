// SPDX-License-Identifier: GPL-2.0-or-later
//! Sound effects: cooked ADPCM samples for the events, the noise generator for
//! the one thing that is genuinely noise.
//!
//! This was all noise, on the theory that an envelope and a pitch are enough
//! for percussion. They are not. The SPU's noise source has no attack
//! transient and no pitch, so a ball hit, a pad and a goal differed only in
//! how bright and how long the hiss was, and all three read as the same
//! static. The impacts are samples now.
//!
//! Boost keeps its noise voice, because a boost really is broadband hiss held
//! for as long as the tank lasts, and a one-shot sample cannot hold. It gets a
//! sampled swoosh laid over the moment it starts, which is the part noise
//! could never do: the transient.
//!
//! Two sources feed the bank. Four impacts are recorded, CC0, from PSoXide's
//! cooked pack (`assets/audio/README.md`). Everything else is synthesised by
//! `tools/sfx-synth` from oscillators, noise and envelopes, so it is ours and
//! carries no licence of its own.
//!
//! Three kinds of caller:
//!
//! * [`update`] reads one simulation tick of a match and makes the noises it
//!   earned. It only reads the sim, so the sim stays deterministic.
//! * [`ui`] is the menus' sound, fired by the input handler that changed
//!   something. It needs no match.
//! * [`tick`] is the clock the one-shot cutoffs run on. It runs every update
//!   in every phase, because a menu click has to be keyed off in a menu.
//!
//! Voices are not shared between kinds of sound. Twenty-two of the SPU's
//! twenty-four are spoken for, one job each, so a hit never cuts the goal
//! horn and a menu click never cuts anything: the only voice that is ever
//! retriggered mid-sound is one repeating its own sound.

use nitroxide_sim::{uu, Sim, WinCondition, GOAL_HALF_W, GOAL_H, HALF_Z, PADS};
use psx_sfx::{Bank, LoopingSample, OneShot, Sample as SfxSample};
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
/// this times a pitch ratio, so a call states the rate it actually plays at
/// and the cutoff is worked out from that.
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

// ---- voices -----------------------------------------------------------------

/// One voice per job, so one sound never cuts another off.
///
/// The ball hit has its own, and so does the goal horn: neither is shared with
/// anything, which is the whole of "voice stealing never cuts them".
const V_HIT: u8 = 0;
/// The held hiss. Noise, not a sample.
const V_BOOST: u8 = 1;
/// Boost pads, small and big. One at a time is all a pad can be.
const V_PAD: u8 = 2;
const V_GOAL: u8 = 3;
/// The transient at the front of a boost, over the top of `V_BOOST`.
const V_BOOST_ON: u8 = 4;
/// A car demolition. It shares the explosion sample with the goal but has its
/// own voice and pitch, so either event can happen without cutting off the
/// other and the lighter demolition does not sound like a scored goal.
const V_DEMO: u8 = 5;
/// Menu sounds alternate between these two so a fast run of clicks overlaps
/// rather than restarting one voice.
const V_UI0: u8 = 6;
const V_UI1: u8 = 7;
/// Jump and flip. A flip follows a jump by a few frames and should replace it.
const V_JUMP: u8 = 8;
const V_LAND: u8 = 9;
const V_BOUNCE: u8 = 10;
/// Car on car.
const V_BUMP: u8 = 11;
const V_SONIC: u8 = 12;
/// The engine, looped, its pitch following the car's speed.
const V_ENGINE: u8 = 13;
/// Tyre squeal, looped, while a powerslide holds.
const V_SKID: u8 = 14;
/// The crowd's murmur, looped under the whole match.
const V_CROWD: u8 = 15;
/// The goal horn. Nothing else is ever played on it.
const V_HORN: u8 = 16;
/// Countdown beeps, the go, the final whistle.
const V_CALL: u8 = 17;
/// The crowd going up for a goal, and drawing its breath on a near miss.
const V_ROAR: u8 = 18;
const V_NET: u8 = 19;
const V_EMPTY: u8 = 20;
/// The rhythmic cheer: at a kickoff, and a second after a goal while the roar
/// is still going. Its own voice, so it never takes the roar or the horn.
const V_CHEER: u8 = 21;
/// Voices this module drives, V_HIT (0) through V_CHEER (21).
const VOICE_COUNT: usize = 22;

/// Only the sustained boost runs on noise now.
const NOISE_MASK: u32 = 1 << V_BOOST;
/// The menu's two voices.
const UI_MASK: u32 = (1 << V_UI0) | (1 << V_UI1);
const ALL_VOICES: u32 = (1 << VOICE_COUNT) - 1;
/// What a match is sounding: everything but the menu's own voices. Pausing or
/// quitting keys these off, and the click that goes with the pause is left to
/// ring.
const MATCH_MASK: u32 = ALL_VOICES & !UI_MASK;

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

/// Where each sample landed, so a retrigger can re-point its voice.
static mut SAMPLE_ADDR: [u32; SND_COUNT] = [0; SND_COUNT];
/// Decoded length of each sample, recorded at upload.
///
/// Without this a one-shot has no length, so nothing ever keys its voice off
/// and it sits on the self-looping parking block psx-sfx appends after every
/// sample. In the emulator that block decodes to silence and the stuck voice
/// is inaudible; on console the 2026-08-07 recording had a continuous tone
/// under the whole match, at exactly 1575 Hz (44100/28, one ADPCM block per
/// cycle at unity pitch) with a second at 1628 Hz that matches the hit
/// sample's loud pitch. Those are parked voices looping a block that is not
/// silent on silicon. Keying the voice off when the sound is over ends it
/// whatever the block holds.
static mut SAMPLE_LEN: [u32; SND_COUNT] = [0; SND_COUNT];
/// Frame each voice must be silenced on. Indexed by VOICE, not by sample:
/// the goal and the demolition share a sample but play on different voices,
/// so a sample-keyed deadline would let one overwrite the other's and leave a
/// voice running.
static mut VOICE_OFF_AT: [u32; VOICE_COUNT] = [0; VOICE_COUNT];
/// Which voices have a deadline pending, so the per-tick sweep is one load
/// and a compare while nothing is sounding.
static mut OFF_PENDING: u32 = 0;
/// Frames elapsed since boot, the clock the cutoffs are scheduled against.
static mut TICK: u32 = 0;
/// The frame clock this game runs its audio on.
const TICKS_HZ: u32 = 60;

/// Whether the boost voice is currently keyed on, so it is only retriggered on
/// the edges rather than every tick.
static mut BOOSTING: bool = false;
/// Goal freeze seen last tick, to catch the moment one starts.
static mut LAST_FREEZE: u16 = 0;
/// A car has been on the ball since the ball was last compared with itself.
static mut BALL_TOUCHED: bool = false;

/// What the last tick looked like, for the sounds that are an edge between two
/// ticks rather than a flag the sim raises. Everything here is read from the
/// sim; none of it is written back.
#[derive(Clone, Copy)]
struct Seen {
    jumps: u8,
    grounded: bool,
    car_v: (i32, i32, i32),
    ball_v: (i32, i32, i32),
    supersonic: bool,
    bump: [bool; 2],
    /// The countdown digit last sounded, and whole seconds left on the clock.
    hold: u16,
    clock: u16,
    /// Engine and skid are looped voices: the pitch step and level last
    /// written while keyed on, so an unchanged one costs no register writes,
    /// and `None` while keyed off.
    engine: Option<(u16, i16)>,
    skid: Option<(u16, i16)>,
    crowd: bool,
}
const SEEN_AT_REST: Seen = Seen {
    jumps: 0,
    grounded: true,
    car_v: (0, 0, 0),
    ball_v: (0, 0, 0),
    supersonic: false,
    bump: [false; 2],
    hold: 0,
    clock: 0,
    engine: None,
    skid: None,
    crowd: false,
};
static mut SEEN: Seen = SEEN_AT_REST;

/// The envelope a held voice keys on with: a quick attack onto a full sustain,
/// and an audible release on key off. (An older note here rejected
/// `Adsr::default_tone` for its half-second attack; that described the
/// pre-fix preset, which attacks instantly now. This hand-built envelope stays
/// for its deliberate release feel.)
const HELD: Adsr = Adsr {
    // Attack shift 0x10 (quick), decay 0x0, sustain level 0xF (hold full).
    lower: (0x10 << 8) | 0xF,
    // Release shift 0x08: quick enough that letting go of boost is audible as
    // it stopping. 0x18 took several seconds to fade, so the hiss outlived
    // the tank by a wide margin.
    upper: 0x08 | (0x7F << 6),
};

/// Bring the SPU up, upload the samples, configure the voices. Once, at boot.
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
    // every following start address aligned.
    // psx-sfx lays the bank down in sequence and keys each voice on
    // correctly, repeat address included, so a finished one-shot parks on
    // silence rather than running into the next sample.
    //
    // Nothing is configured here: `play` and `start_loop` point a voice at its
    // sample and set its envelope on every key-on, and nothing shares a voice
    // between two sounds, so there is no Player either; the per-voice cutoffs
    // live in `play` and `expire_voices`.
    let mut bank = Bank::new(SpuAddr::new(SPU_SAMPLE_BASE));
    for (i, (bytes, _)) in BANK.iter().enumerate() {
        // OneShot's default envelope (default_tone), not Adsr::sample():
        // PSoXide's SB1 console capture (2026-08-02) showed sample()-enveloped
        // one-shots never die on real hardware -- END+mute enters a release
        // that never finishes while the voice loops from the repeat address at
        // full volume. default_tone (post-fix: INSTANT attack, hold, ~100 ms
        // release) plays the sample out and stops at the END flag.
        let sample = bank.upload(bytes);
        unsafe {
            SAMPLE_ADDR[i] = sample.addr().byte_offset();
            SAMPLE_LEN[i] = sample.len_samples();
        };
    }

    // The boost is held rather than struck.
    spu::Voice::new(V_BOOST).set_adsr(HELD);
}

/// Re-point a sample voice at its data and key it on.
///
/// A voice that has played once sits at the end of its sample, so restarting
/// it means writing the start address again, not just keying on. Volume and
/// pitch are per call because several of these are scaled by how hard the hit
/// was.
///
/// Through psx-sfx rather than by hand: a bare set_start_addr leaves the
/// repeat register holding whatever the last sound put there, and on END
/// silicon jumps to it rather than stopping.
///
/// `rate_hz` is the rate the sample plays at, its cooked rate times any pitch
/// ratio. The `SfxSample::resident` block count stays 0 -- psx-sfx computes its
/// own cutoff from a sample's recorded rate, and these are pitch-shifted per
/// call -- so the cutoff is scheduled here instead, against the rate actually
/// played. Leaving it to the envelope is what left voices parked and audible
/// on console; see [`SAMPLE_LEN`].
fn play(voice: u8, snd: Snd, vol: i16, rate_hz: u32) {
    let slot = snd as usize;
    let sample = SfxSample::resident(SpuAddr::new(unsafe { SAMPLE_ADDR[slot] }), rate_hz, 0);
    OneShot::new(sample, Volume(vol))
        .with_pitch(Pitch::for_frequency(rate_hz, 44_100))
        .play(spu::Voice::new(voice));
    // Schedule the key-off. The length is computed against the rate this call
    // actually plays at, so a pitched-up hit is cut earlier and a pitched-down
    // goal later, and TAIL_MARGIN keeps the cutoff clear of the sound itself.
    let rate = if rate_hz == 0 { 1 } else { rate_hz };
    let ticks = unsafe { SAMPLE_LEN[slot] } * TICKS_HZ / rate + TAIL_MARGIN;
    unsafe {
        VOICE_OFF_AT[voice as usize] = TICK.wrapping_add(ticks);
        OFF_PENDING |= 1 << voice;
    }
}

/// `play` at the sample's own rate times `num/16`: 16 is as cooked, 17 is
/// about a semitone up, 24 is a fifth.
fn play_ratio(voice: u8, snd: Snd, vol: i16, num: u32) {
    play(voice, snd, vol, BANK[snd as usize].1 * num / 16);
}

/// Key a looped sample on and leave it running; the caller keys the voice off.
/// The loop is in the sample's own block flags, so this needs no length.
fn start_loop(voice: u8, snd: Snd, vol: i16, rate_hz: u32) {
    let sample = SfxSample::resident(SpuAddr::new(unsafe { SAMPLE_ADDR[snd as usize] }), rate_hz, 0);
    LoopingSample::new(sample, Volume(vol))
        .with_pitch(Pitch::for_frequency(rate_hz, 44_100))
        .with_adsr(HELD)
        .play(spu::Voice::new(voice));
    unsafe { OFF_PENDING &= !(1 << voice) };
}

/// Ticks of margin past a sample's own length before its voice is silenced,
/// so the cutoff never clips a sound that is still sounding.
const TAIL_MARGIN: u32 = 2;

/// Key off any voice whose sound has run out. Without this a finished
/// one-shot sits on its parking block forever; see [`SAMPLE_LEN`].
///
/// The held voices (boost, engine, skid, crowd) are not in here: they are
/// keyed off on their own edges, and they never get a deadline.
fn expire_voices() {
    let mut pending = unsafe { OFF_PENDING };
    if pending == 0 {
        return;
    }
    let now = unsafe { TICK };
    let mut done = 0u32;
    while pending != 0 {
        let voice = pending.trailing_zeros();
        pending &= pending - 1;
        // Wrap-safe "now has reached deadline".
        if now.wrapping_sub(unsafe { VOICE_OFF_AT[voice as usize] }) < u32::MAX / 2 {
            done |= 1 << voice;
        }
    }
    if done != 0 {
        spu::Voice::key_off(done);
        unsafe { OFF_PENDING &= !done };
    }
}

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
        spu::Voice::key_off(UI_MASK);
        unsafe { OFF_PENDING &= !UI_MASK };
    }
}

/// The audio clock, once per update in every phase. The cutoffs run even while
/// muted: a sound started before the mute still has a voice to key off.
pub fn tick() {
    unsafe { TICK = TICK.wrapping_add(1) };
    expire_voices();
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

/// Which of the two menu voices takes the next click.
static mut UI_NEXT: bool = false;

/// Sound a menu event. Silent while sound is off.
pub fn ui(event: Ui) {
    if unsafe { MUTED } {
        return;
    }
    let voice = if unsafe { UI_NEXT } { V_UI1 } else { V_UI0 };
    unsafe { UI_NEXT = !UI_NEXT };
    match event {
        Ui::Move => play_ratio(voice, Snd::UiMove, L_UI_MOVE, 16),
        // Up is a step brighter than as cooked and down a step darker, so
        // the direction of a slider can be heard as well as seen.
        Ui::Adjust(dir) => {
            play_ratio(voice, Snd::UiTick, L_UI_TICK, if dir >= 0 { 18 } else { 14 })
        }
        Ui::Select => play_ratio(voice, Snd::UiSelect, L_UI_SELECT, 16),
        Ui::Back => play_ratio(voice, Snd::UiBack, L_UI_BACK, 16),
        Ui::Deny => play_ratio(voice, Snd::UiDeny, L_UI_DENY, 16),
        Ui::Ready => play_ratio(voice, Snd::UiChime, L_UI_CHIME, 16),
        Ui::Saved => play_ratio(voice, Snd::UiSaved, L_UI_SAVED, 16),
        Ui::Open => play_ratio(voice, Snd::UiOpen, L_UI_OPEN, 16),
        Ui::Close => play_ratio(voice, Snd::UiClose, L_UI_CLOSE, 16),
        // The match start is the same call as a kickoff's go.
        Ui::Go => {
            play_ratio(V_CALL, Snd::Go, L_GO, 16);
            play_ratio(V_CHEER, Snd::Cheer, L_CHEER_KICKOFF, 16);
        }
    }
}

/// The final whistle. Called after [`stop_all`], which would otherwise cut it.
pub fn match_end() {
    if unsafe { MUTED } {
        return;
    }
    play_ratio(V_CALL, Snd::Whistle, L_WHISTLE, 16);
}

// ---- the match ----------------------------------------------------------------

/// Speed, in sim sub-units a tick, past which the engine is at its highest
/// note: a little above the supersonic line.
const ENGINE_TOP: i32 = 2500;
/// The engine's note at standstill and at full speed, in 1/16ths of the loop's
/// own rate.
const ENGINE_LOW: u32 = 16;
const ENGINE_HIGH: u32 = 44;
/// Steps the engine's pitch and level are quantised to, so a car cruising does
/// not rewrite the voice registers sixty times a second.
const ENGINE_STEPS: i32 = 16;
/// Powerslide has to be this far in, and the car this fast, to squeal.
const SKID_SLIDE: i32 = 380;
const SKID_SPEED: i32 = 500;
/// Smallest component a bounce has to turn around, in sub-units a tick
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

/// Read one tick of the match and make the noises it earned.
pub fn update(s: &Sim) {
    if unsafe { MUTED } {
        return;
    }
    // Ball contact. Volume tracks how hard, so a dribble whispers and a
    // clearance cracks, and pitch tracks it too: the same sample played a
    // fifth up reads as a harder strike rather than as a louder one.
    if s.hit > HIT_FLOOR {
        let loud = ((s.hit - HIT_FLOOR).min(HIT_LOUD) * 0x2A00 / HIT_LOUD) as i16;
        let rate = 30_000 + (s.hit.min(HIT_LOUD) as u32) * 6;
        play(V_HIT, Snd::Hit, loud, rate);
    }
    if s.hit != 0 {
        unsafe { BALL_TOUCHED = true };
    }

    if s.pad_taken {
        // A big pad is the one whose timer was just set to the long respawn
        // (600 ticks, `Pad::respawn`); a small one's was set to 240.
        let big = s
            .pad_timers
            .iter()
            .zip(PADS.iter())
            .any(|(t, p)| p.big && *t == 600);
        if big {
            play_ratio(V_PAD, Snd::PadBig, L_PAD_BIG, 16);
        } else {
            play(V_PAD, Snd::Pad, 0x1E00, 44_100);
        }
    }

    // A demolition already arrives as a one-tick event from the simulation.
    // Use the goal explosion's resident sample at its natural, sharper pitch:
    // it reads as a nearby car-sized blast while the pitched-down goal still
    // owns the large arena-wide boom.
    if s.demo {
        play(V_DEMO, Snd::Goal, 0x2C00, 44_100);
    }

    // Boost: keyed on the tick it starts and off the tick it stops, so it
    // holds cleanly instead of restarting sixty times a second. The swoosh
    // fires once on the same edge and is left to ring out on its own.
    let boosting = s.car.boosting;
    if boosting != unsafe { BOOSTING } {
        let v = spu::Voice::new(V_BOOST);
        if boosting {
            v.set_volume(Volume(0x1000), Volume(0x1000));
            v.set_pitch(Pitch::for_frequency(8_000, 44_100));
            spu::Voice::key_on(1 << V_BOOST);
            play(V_BOOST_ON, Snd::Boost, 0x1C00, 44_100);
        } else {
            spu::Voice::key_off(1 << V_BOOST);
            // The tank ran dry under the player's thumb: a sputter, so the
            // boost running out is heard and not only seen on the gauge.
            // Dry is less than one tick of fuel (the sim's BOOST_DRAIN, 36),
            // not zero: the burn stops while a remainder is left in the tank.
            if s.car.boost < BOOST_DRY {
                play_ratio(V_EMPTY, Snd::Empty, L_EMPTY, 16);
            }
        }
        unsafe { BOOSTING = boosting };
    }

    // A goal: catch the tick the freeze begins. The explosion is pitched
    // down, because the clip is a short arcade explosion and the goal wants
    // weight rather than brightness; the horn and the crowd go up with it,
    // each on a voice of its own.
    let freeze = s.goal_freeze;
    if freeze > 0 && unsafe { LAST_FREEZE } == 0 {
        play(V_GOAL, Snd::Goal, 0x3000, 32_000);
        play_ratio(V_HORN, Snd::Horn, L_HORN, 16);
        play_ratio(V_ROAR, Snd::Roar, L_ROAR, 16);
        // The ball into the netting. The world stops the instant it crosses
        // the line, so it never reaches the back of the net by itself: this
        // is that thump, as hard as the ball was going.
        let v = s.ball.v;
        let pace = (v.x.abs() + v.y.abs() + v.z.abs()).clamp(NET_SOFT, NET_HARD);
        let level = L_NET as i32 * pace / NET_HARD;
        play_ratio(V_NET, Snd::Net, level as i16, 16);
    }
    if freeze == CHEER_AT {
        play_ratio(V_CHEER, Snd::Cheer, L_CHEER, 16);
    }
    unsafe { LAST_FREEZE = freeze };

    // SAFETY: the game is single threaded and `update` is not re-entered.
    let seen = unsafe { &mut *(&raw mut SEEN) };
    // Live play: not the goal celebration and not the kickoff countdown. The
    // kickoff puts both cars and the ball back on their spots in one tick, and
    // that is a teleport, not a landing or a bounce.
    let live = s.goal_freeze == 0 && s.kickoff_hold == 0;
    // The sounds that are an edge in the sim's state are sampled every second
    // tick, and the held voices every fourth. An edge in this state lasts many
    // ticks (a jump is airtime, a bump a cooldown, a supersonic spell a
    // second), and a note that follows speed moves at a walking pace, so the
    // lag is a fiftieth of a second at worst and the work is a half and a
    // quarter. The one-tick flags above (a hit, a pad, a wreck, a goal) are
    // read every tick.
    let tick = unsafe { TICK };
    if tick & 1 == 0 {
        car_sounds(s, seen, live);
        ball_sounds(s, seen, live);
        clock_sounds(s, seen);
    }
    if tick & 3 == 0 {
        held_sounds(s, seen, live);
    }
}

/// Jump, flip, landing, supersonic, bump: the player car's own edges.
fn car_sounds(s: &Sim, seen: &mut Seen, live: bool) {
    let car = &s.car;
    if car.jumps_used != seen.jumps {
        // Only a rise is a jump; the count falls back to zero on landing.
        if car.jumps_used > seen.jumps {
            if car.jumps_used == 1 {
                play_ratio(V_JUMP, Snd::Jump, L_JUMP, 16);
            } else if car.dodge_timer > 0 {
                play_ratio(V_JUMP, Snd::Flip, L_FLIP, 16);
            } else {
                // A double jump with no direction: the jump again, higher.
                play_ratio(V_JUMP, Snd::Jump, L_JUMP, 22);
            }
        }
        seen.jumps = car.jumps_used;
    }

    if live && car.grounded && !seen.grounded {
        // How hard it came down: the velocity the contact took off it, summed
        // over the axes (no square root for a sound).
        let d = (car.v.x - seen.car_v.0).abs()
            + (car.v.y - seen.car_v.1).abs()
            + (car.v.z - seen.car_v.2).abs();
        if d > LAND_FLOOR {
            let level = d.min(LAND_LOUD) * L_LAND as i32 / LAND_LOUD;
            play_ratio(V_LAND, Snd::Land, level.max(0x0800) as i16, 16);
        }
    }
    seen.grounded = car.grounded;
    // Only the airborne velocity is wanted, for the landing that follows.
    if !car.grounded {
        seen.car_v = (car.v.x, car.v.y, car.v.z);
    }

    if car.supersonic != seen.supersonic {
        if car.supersonic {
            play_ratio(V_SONIC, Snd::Sonic, L_SONIC, 16);
        }
        seen.supersonic = car.supersonic;
    }

    // Either car bumping the other starts that car's cooldown. A demolition
    // starts it too, and has its own, bigger sound: a wrecked car in play
    // means the bang was that, not a bump.
    let bump = [car.bump_cool > 0, s.opponent.bump_cool > 0];
    let wreck = car.wrecked() || s.opponent.wrecked();
    if ((bump[0] && !seen.bump[0]) || (bump[1] && !seen.bump[1])) && !wreck {
        play_ratio(V_BUMP, Snd::Bump, L_BUMP, 16);
    }
    seen.bump = bump;
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
    if tz > 0
        && p.z.abs() > uu(HALF_Z - 220)
        && p.x.abs() < uu(GOAL_HALF_W + 120)
        && p.y < uu(GOAL_H + 160)
    {
        // Turned back at the mouth of the goal: a post or the bar. The ball
        // rings off it, and the crowd draws its breath.
        play_ratio(V_BOUNCE, Snd::Bounce, level, 28);
        play_ratio(V_ROAR, Snd::Gasp, L_GASP, 16);
    } else if ty > 0 {
        // The floor is lower than a wall, the ceiling a touch higher.
        play_ratio(V_BOUNCE, Snd::Bounce, level, if v.y > 0 { 16 } else { 19 });
    } else {
        play_ratio(V_BOUNCE, Snd::Bounce, level, 21);
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
            play_ratio(V_CALL, Snd::Beep, L_BEEP, 16);
        } else {
            play_ratio(V_CALL, Snd::Go, L_GO, 16);
            play_ratio(V_CHEER, Snd::Cheer, L_CHEER_KICKOFF, 16);
        }
        seen.hold = digit;
    }
    // Whole seconds on the clock. Each second that falls below five is a tick
    // of the same beep, higher; the last one is a second before the whistle.
    let secs = (s.clock / 60) as u16;
    if secs < seen.clock {
        if secs < 5 && matches!(s.win_condition, WinCondition::TimeLimit(_)) {
            play_ratio(V_CALL, Snd::Beep, L_BEEP, 20);
        }
    }
    seen.clock = secs;
}

/// The looped voices: engine, tyre squeal and the crowd.
fn held_sounds(s: &Sim, seen: &mut Seen, live: bool) {
    if live && !seen.crowd {
        start_loop(V_CROWD, Snd::CrowdLoop, L_CROWD, BANK[Snd::CrowdLoop as usize].1);
        seen.crowd = true;
    } else if !live && seen.crowd {
        spu::Voice::key_off(1 << V_CROWD);
        seen.crowd = false;
    }

    let car = &s.car;
    let speed = car.v.len();
    let engine_on = live && !car.wrecked();
    if engine_on {
        let step = (speed.clamp(0, ENGINE_TOP) * ENGINE_STEPS / ENGINE_TOP) as u32;
        // The note climbs with speed and the level follows it, halved in the
        // air where the wheels are not loaded.
        let note = ENGINE_LOW * 256 + (ENGINE_HIGH - ENGINE_LOW) * 256 * step / ENGINE_STEPS as u32;
        let mut level = L_ENGINE as i32 * (12 + step as i32) / (12 + ENGINE_STEPS);
        if !car.grounded {
            level /= 2;
        }
        let key = (note as u16, level as i16);
        if seen.engine != Some(key) {
            let rate = BANK[Snd::EngineLoop as usize].1 * note / (16 * 256);
            if seen.engine.is_none() {
                start_loop(V_ENGINE, Snd::EngineLoop, level as i16, rate);
            } else {
                let v = spu::Voice::new(V_ENGINE);
                v.set_pitch(Pitch::for_frequency(rate, 44_100));
                v.set_volume(Volume(level as i16), Volume(level as i16));
            }
            seen.engine = Some(key);
        }
    } else if seen.engine.is_some() {
        spu::Voice::key_off(1 << V_ENGINE);
        seen.engine = None;
    }

    // Tyre squeal: only while the powerslide is properly in and the car is
    // moving, louder the deeper it goes.
    let sliding = engine_on && car.grounded && car.slide > SKID_SLIDE && speed > SKID_SPEED;
    if sliding {
        let depth = (car.slide - SKID_SLIDE).clamp(0, 1024 - SKID_SLIDE) * 3 / (1024 - SKID_SLIDE);
        let level = L_SKID as i32 / 4 * (depth + 1);
        let step = (speed.clamp(0, ENGINE_TOP) * 8 / ENGINE_TOP) as u16;
        let key = (step, level as i16);
        if seen.skid != Some(key) {
            let rate = BANK[Snd::SkidLoop as usize].1 * (12 + step as u32) / 16;
            if seen.skid.is_none() {
                start_loop(V_SKID, Snd::SkidLoop, level as i16, rate);
            } else {
                let v = spu::Voice::new(V_SKID);
                v.set_pitch(Pitch::for_frequency(rate, 44_100));
                v.set_volume(Volume(level as i16), Volume(level as i16));
            }
            seen.skid = Some(key);
        }
    } else if seen.skid.is_some() {
        spu::Voice::key_off(1 << V_SKID);
        seen.skid = None;
    }
}

/// Silence the match: every voice but the menu's. Used on pause, on leaving
/// the match and when sound is turned off, so a held boost does not hiss over
/// the results screen and an engine does not idle under the pause menu.
///
/// What the last tick looked like is kept, so resuming mid-flight does not
/// replay the jump or the goal horn the pause interrupted; the held voices
/// are keyed off, so their flags are cleared and they start again on resume.
pub fn stop_all() {
    spu::Voice::key_off(MATCH_MASK);
    unsafe {
        BOOSTING = false;
        OFF_PENDING &= UI_MASK;
        let seen = &mut *(&raw mut SEEN);
        seen.engine = None;
        seen.skid = None;
        seen.crowd = false;
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
