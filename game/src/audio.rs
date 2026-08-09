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
//! Samples are CC0, from PSoXide's cooked pack. See `assets/audio/README.md`.

use nitroxide_sim::Sim;
use psx_sfx::{Bank, OneShot, Sample as SfxSample};
use psx_spu::{self as spu, Adsr, CdVolume, Pitch, SpuAddr, Volume};

static HIT_SFX: &[u8] = include_bytes!("../assets/audio/hit_punch.psau");
static PAD_SFX: &[u8] = include_bytes!("../assets/audio/pickup_coin.psau");
static GOAL_SFX: &[u8] = include_bytes!("../assets/audio/explosion_short.psau");
static BOOST_SFX: &[u8] = include_bytes!("../assets/audio/swoosh.psau");

/// One voice per sound, so a ball hit never cuts off the boost.
const V_HIT: u8 = 0;
/// The held hiss. Noise, not a sample.
const V_BOOST: u8 = 1;
const V_PAD: u8 = 2;
const V_GOAL: u8 = 3;
/// The transient at the front of a boost, over the top of `V_BOOST`.
const V_BOOST_ON: u8 = 4;
/// A car demolition. It shares the explosion sample with the goal but has its
/// own voice and pitch, so either event can happen without cutting off the
/// other and the lighter demolition does not sound like a scored goal.
const V_DEMO: u8 = 5;

/// Only the sustained boost runs on noise now.
const NOISE_MASK: u32 = 1 << V_BOOST;
/// Everything this module might be sounding, for a single key-off.
const ALL_MASK: u32 = (1 << V_HIT)
    | (1 << V_BOOST)
    | (1 << V_PAD)
    | (1 << V_GOAL)
    | (1 << V_BOOST_ON)
    | (1 << V_DEMO);

/// First address in SPU RAM that is not the CD/capture area.
const SPU_SAMPLE_BASE: u32 = 0x1010;

/// Below this the contact is a nudge, not a hit, and sounding it turns
/// dribbling into a rattle.
const HIT_FLOOR: i32 = 260;
/// Impulse that counts as a full-volume hit; anything above just saturates.
const HIT_LOUD: i32 = 2600;

/// Where each sample landed, so a retrigger can re-point its voice.
static mut SAMPLE_ADDR: [u32; 4] = [0; 4];
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
static mut SAMPLE_LEN: [u32; 4] = [0; 4];
/// Frame each voice must be silenced on, or `None` when idle. Indexed by
/// VOICE, not by sample slot: the goal and the demolition share slot 2 but
/// play on different voices, so a slot-keyed deadline would let one overwrite
/// the other's and leave a voice running.
static mut VOICE_OFF_AT: [Option<u32>; VOICE_COUNT] = [None; VOICE_COUNT];
/// Voices this module drives, V_HIT (0) through V_DEMO (5).
const VOICE_COUNT: usize = 6;
/// Frames elapsed since boot, the clock the cutoffs are scheduled against.
static mut TICK: u32 = 0;
/// The frame clock this game runs its audio on.
const TICKS_HZ: u32 = 60;

/// Whether the boost voice is currently keyed on, so it is only retriggered on
/// the edges rather than every tick.
static mut BOOSTING: bool = false;
/// Goal freeze seen last tick, to catch the moment one starts.
static mut LAST_FREEZE: u16 = 0;

/// Bring the SPU up, upload the samples, configure the voices. Once, at boot.
pub fn init() {
    spu::init();
    spu::set_main_volume(Volume(0x2800), Volume(0x2800));
    // Route the CD's own audio into the mix, for the disc music `music.rs`
    // borrows. Without this the drive plays to nothing.
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
    // configure rather than play: NitroXide keeps one voice per sound and sets
    // them up once here, then only keys on. Nothing shares voices, so there is
    // no Player; the per-voice cutoffs live in `play` and `expire_voices`.
    let mut bank = Bank::new(SpuAddr::new(SPU_SAMPLE_BASE));
    for (i, (bytes, voice)) in [
        (HIT_SFX, V_HIT),
        (PAD_SFX, V_PAD),
        (GOAL_SFX, V_GOAL),
        (BOOST_SFX, V_BOOST_ON),
    ]
    .into_iter()
    .enumerate()
    {
        // default_tone, not Adsr::sample(): PSoXide's SB1 console capture
        // (2026-08-02) showed sample()-enveloped one-shots never die on
        // real hardware -- END+mute enters a release that never finishes
        // while the voice loops from the repeat address at full volume.
        // default_tone (post-fix: INSTANT attack, hold, ~100 ms release)
        // plays the sample out and stops at the END flag. It is what
        // OneShot uses by default.
        let sample = bank.upload(bytes);
        OneShot::new(sample, Volume(0x2000)).configure(spu::Voice::new(voice));
        unsafe {
            SAMPLE_ADDR[i] = sample.addr().byte_offset();
            SAMPLE_LEN[i] = sample.len_samples();
        };
    }

    // The boost is held rather than struck, so it wants a fast attack onto
    // a full sustain with an audible release on key_off. (An older note
    // here rejected Adsr::default_tone for its half-second attack; that
    // described the pre-fix preset -- it attacks instantly now. This
    // hand-built envelope stays for its deliberate release feel.)
    spu::Voice::new(V_BOOST).set_adsr(Adsr {
        // Attack shift 0x10 (quick), decay 0x0, sustain level 0xF (hold full).
        lower: (0x10 << 8) | 0xF,
        // Release shift 0x08: quick enough that letting go of boost is
        // audible as it stopping. 0x18 took several seconds to fade, so the
        // hiss outlived the tank by a wide margin.
        upper: 0x08 | (0x7F << 6),
    });
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
/// The `SfxSample::resident` block count stays 0 -- psx-sfx computes its own
/// cutoff from a sample's recorded rate, and these are pitch-shifted per call
/// -- so the cutoff is scheduled here instead, against the rate actually
/// played. Leaving it to the envelope is what left voices parked and audible
/// on console; see [`SAMPLE_LEN`].
fn play(voice: u8, slot: usize, vol: i16, rate_hz: u32) {
    let sample = SfxSample::resident(SpuAddr::new(unsafe { SAMPLE_ADDR[slot] }), rate_hz, 0);
    OneShot::new(sample, Volume(vol))
        .with_pitch(Pitch::for_frequency(rate_hz, 44_100))
        .play(spu::Voice::new(voice));
    // Schedule the key-off. The length is computed against the rate this call
    // actually plays at, so a pitched-up hit is cut earlier and a pitched-down
    // goal later, and TAIL_MARGIN keeps the cutoff clear of the sound itself.
    let rate = if rate_hz == 0 { 1 } else { rate_hz };
    let ticks = unsafe { SAMPLE_LEN[slot] } * TICKS_HZ / rate + TAIL_MARGIN;
    unsafe { VOICE_OFF_AT[voice as usize] = Some(TICK.wrapping_add(ticks)) };
}

/// Ticks of margin past a sample's own length before its voice is silenced,
/// so the cutoff never clips a sound that is still sounding.
const TAIL_MARGIN: u32 = 2;

/// Key off any voice whose sound has run out. Without this a finished
/// one-shot sits on its parking block forever; see [`SAMPLE_LEN`].
///
/// The held boost (V_BOOST) is not in here: it is keyed off on its own edge,
/// and it never gets a deadline because it never goes through `play`.
fn expire_voices() {
    let now = unsafe { TICK };
    for voice in 0..VOICE_COUNT {
        let Some(deadline) = (unsafe { VOICE_OFF_AT[voice] }) else {
            continue;
        };
        // Wrap-safe "now has reached deadline".
        if now.wrapping_sub(deadline) < u32::MAX / 2 {
            spu::Voice::key_off(1 << voice);
            unsafe { VOICE_OFF_AT[voice] = None };
        }
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
    }
}

/// Read one tick of the match and make the noises it earned.
pub fn update(s: &Sim) {
    // The clock and the cutoffs run even while muted: a sound started before
    // the mute still has a voice to key off.
    unsafe { TICK = TICK.wrapping_add(1) };
    expire_voices();
    if unsafe { MUTED } {
        return;
    }
    // Ball contact. Volume tracks how hard, so a dribble whispers and a
    // clearance cracks, and pitch tracks it too: the same sample played a
    // fifth up reads as a harder strike rather than as a louder one.
    if s.hit > HIT_FLOOR {
        let loud = ((s.hit - HIT_FLOOR).min(HIT_LOUD) * 0x2A00 / HIT_LOUD) as i16;
        let rate = 30_000 + (s.hit.min(HIT_LOUD) as u32) * 6;
        play(V_HIT, 0, loud, rate);
    }

    if s.pad_taken {
        play(V_PAD, 1, 0x1E00, 44_100);
    }

    // A demolition already arrives as a one-tick event from the simulation.
    // Use the goal explosion's resident sample at its natural, sharper pitch:
    // it reads as a nearby car-sized blast while the pitched-down goal still
    // owns the large arena-wide boom.
    if s.demo {
        play(V_DEMO, 2, 0x2C00, 44_100);
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
            play(V_BOOST_ON, 3, 0x1C00, 44_100);
        } else {
            spu::Voice::key_off(1 << V_BOOST);
        }
        unsafe { BOOSTING = boosting };
    }

    // A goal: catch the tick the freeze begins. Pitched down, because the
    // clip is a short arcade explosion and the goal wants weight rather than
    // brightness.
    let freeze = s.goal_freeze;
    if freeze > 0 && unsafe { LAST_FREEZE } == 0 {
        play(V_GOAL, 2, 0x3000, 32_000);
    }
    unsafe { LAST_FREEZE = freeze };
}

/// Silence everything. Used when leaving the match, so a held boost does not
/// hiss over the results screen.
pub fn stop_all() {
    spu::Voice::key_off(ALL_MASK);
    unsafe {
        BOOSTING = false;
        LAST_FREEZE = 0;
        VOICE_OFF_AT = [None; VOICE_COUNT];
    }
}
