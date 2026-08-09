// SPDX-License-Identifier: GPL-2.0-or-later
//! The demo disc's menu music, borrowed rather than carried.
//!
//! NitroXide's image carries `WORLD.PAK` for startup assets, but has no CD-DA
//! of its own and no reason to grow any: the four menu songs are already
//! pressed onto the demo disc it boots from. CD-DA is addressed by track
//! number rather than by LBA, independently of the pack's `disc_base`
//! relocation.
//!
//! Finding them is by convention. `mkdisc` lays the launcher's menu audio down
//! before any program's, so the menu songs are always the first CD-DA tracks
//! on the disc, and track 1 is data. That makes them 2 through 5, whatever
//! else is on the disc and wherever NitroXide sits in the running order. The
//! `cdda_track_base` the loader hands over does not point at them: it counts
//! every audio track *ahead* of this program, so it lands past the menu in
//! whatever the earlier programs carry.
//!
//! Which is why the base has to be subtracted before asking for a track.
//! `cdrom::try_play_track` shifts every track number by that base on the way
//! out (`disc_base::shift_track`), so handing it a plain 2 asks the drive for
//! track 2 + base. On the Comicon pressing that is 33, the first of Half-Life's
//! tracks; on the public pressing, 6. The console session on 2026-08-07 heard
//! exactly that: KNUCKLE DUST playing Magikarp Pong's song, two titles playing
//! the hardware-test tones, and NIGHT CRAWLER asking for a track past the end
//! of the disc. Pre-subtracting the base cancels the shift, and standalone
//! boot is unaffected because the base is 0 there.
//!
//! The cost of that convention is a coupling: if the disc's menu tracklist
//! ever stops being four songs, this plays the wrong four. The drive is asked
//! how many tracks exist before a note is played, so a disc without them --
//! NitroXide's own, which is one data track and nothing else -- is silent
//! rather than wrong.

use psx_io::cdda::{CddaEndDetector, CddaStarter};
use psx_io::cdrom;

/// First CD-DA track on a mixed-mode disc: track 1 is the data track.
const FIRST_TRACK: u8 = 2;
/// How many menu songs the demo disc carries.
const TRACK_COUNT: u8 = 4;

/// What the demo disc calls its four menu songs, in disc order. Part of the
/// same convention as the track numbers above: the launcher's Makefile presses
/// these titles alongside these tracks, so a disc whose tracklist changes
/// mislabels as well as misplays.
pub const TRACK_NAMES: [&str; TRACK_COUNT as usize] = [
    "KNUCKLE DUST",
    "RUSTED HAMMER",
    "CHAINSAW HEART",
    "NIGHT CRAWLER",
];

/// How long the now-playing banner stays up once a song starts. Four seconds:
/// long enough to read twice, gone before it reads as HUD. Public so the
/// drawing side can time its slide-out against the same clock.
pub const BANNER_TICKS: u32 = 240;
/// The disc must show at least this many tracks for the menu music to be
/// there at all.
const TRACKS_NEEDED: u8 = FIRST_TRACK + TRACK_COUNT - 1;

/// GetTN. Answers with the first and last track numbers, in BCD.
const GET_TN: u8 = 0x13;

/// Per-command spin budget. Emulators answer instantly; silicon does not.
const SPINS: u32 = 0x10_0000;

/// Ticks between drive status polls. A GetStat is a round trip to the drive,
/// and this runs inside a frame that split screen already has at its
/// deadline, so it is paced rather than issued every tick.
const POLL_TICKS: u32 = 30;

/// Consecutive quiet polls that mean the song is over rather than dipping.
const IDLE_POLLS_TO_ADVANCE: u8 = 8;

/// Ticks to wait before asking the drive anything. It has just been used to
/// load this program and the head is still settling; the launcher's own
/// handshake takes the same precaution.
const PROBE_TICK: u32 = 90;

/// BCD byte to binary. Track numbers come off the drive packed.
fn from_bcd(v: u8) -> u8 {
    (v >> 4) * 10 + (v & 0x0F)
}

pub struct Music {
    /// The disc really does carry the menu tracks.
    available: bool,
    /// The player has not turned it off.
    enabled: bool,
    /// Which of the four is playing, 0-based.
    index: u8,
    /// Have we asked the drive what is on it yet?
    probed: bool,
    /// Tick the current song was started on, for the now-playing banner.
    announced_at: Option<u32>,
    starter: CddaStarter,
    end: CddaEndDetector,
    next_poll: u32,
}

impl Music {
    pub fn new() -> Self {
        Music {
            available: false,
            enabled: true,
            index: 0,
            probed: false,
            announced_at: None,
            starter: CddaStarter::new().with_spins(SPINS),
            end: CddaEndDetector::new(IDLE_POLLS_TO_ADVANCE),
            next_poll: 0,
        }
    }

    /// Is there music to talk about? The pause menu hides the row when not,
    /// rather than offering a switch that does nothing.
    pub fn available(&self) -> bool {
        self.available
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// What the current song is called on the disc's own tracklist.
    pub fn track_name(&self) -> &'static str {
        TRACK_NAMES[self.index as usize]
    }

    /// The song to announce and how long it has been up, while a start is
    /// recent enough to announce. The popup follows the drive rather than the
    /// menu: switched off, there is nothing playing to name. The elapsed
    /// ticks are what the popup animates its slide and spin from.
    pub fn now_playing(&self, tick: u32) -> Option<(&'static str, u32)> {
        let at = self.announced_at?;
        if !self.available || !self.enabled {
            return None;
        }
        let elapsed = tick.wrapping_sub(at);
        if elapsed > BANNER_TICKS {
            return None;
        }
        Some((TRACK_NAMES[self.index as usize], elapsed))
    }

    /// Step to the previous or next song. Starts it at once when the music is
    /// on; off, it only moves the pointer the next start will use.
    pub fn cycle_track(&mut self, step: i32, tick: u32) {
        let n = TRACK_COUNT as i32;
        self.index = ((self.index as i32 + step % n + n) % n) as u8;
        if self.available && self.enabled {
            self.begin_track(tick);
        }
    }

    /// Turn the music on or off. Off pauses the drive rather than only
    /// declining to start it again, or the current song would play on under
    /// a menu that claims it is off.
    pub fn set_enabled(&mut self, on: bool, tick: u32) {
        self.enabled = on;
        if !self.available {
            return;
        }
        if on {
            self.begin_track(tick);
        } else {
            cdrom::try_pause(SPINS);
            self.starter = CddaStarter::new().with_spins(SPINS);
        }
    }

    /// Arm the start handshake for the current track.
    fn begin_track(&mut self, tick: u32) {
        self.starter = CddaStarter::new().with_spins(SPINS);
        self.starter.begin(tick);
        self.end.rearm();
        self.next_poll = tick.wrapping_add(POLL_TICKS);
        self.announced_at = Some(tick);
    }

    /// One tick. Drives the start handshake, and once playing, watches for the
    /// song ending so the next one can follow.
    pub fn update(&mut self, tick: u32) {
        if !self.probed {
            if tick < PROBE_TICK {
                return;
            }
            self.probed = true;
            self.available = disc_has_menu_tracks();
            if self.available && self.enabled {
                self.begin_track(tick);
            }
            return;
        }
        if !self.available || !self.enabled {
            return;
        }

        // The handshake first: the drive tolerates command-then-status-drain,
        // and the launcher's console runs showed the reverse order wedging it.
        // The track is absolute, so the loader's base is pre-subtracted here
        // to cancel the shift `try_play_track` applies (see the module note).
        let absolute = FIRST_TRACK + self.index;
        self.starter
            .tick(tick, absolute.wrapping_sub(psx_io::disc_base::cdda_track_base()));

        if self.starter.started() && tick.wrapping_sub(self.next_poll) < u32::MAX / 2 {
            self.next_poll = tick.wrapping_add(POLL_TICKS);
            let status = cdrom::try_get_stat(SPINS).and_then(|r| r.bytes().first().copied());
            if self.end.poll(status) {
                self.index = (self.index + 1) % TRACK_COUNT;
                self.begin_track(tick);
            }
        }
    }

    /// Stop the drive. For leaving the program, not for muting.
    pub fn stop(&mut self) {
        if self.available {
            cdrom::try_pause(SPINS);
        }
    }
}

/// Ask the drive how many tracks the disc has, and decide whether the menu
/// songs can be among them.
fn disc_has_menu_tracks() -> bool {
    // Response is [status, first BCD, last BCD].
    let Some(response) = cdrom::try_command(GET_TN, &[], SPINS) else {
        return false;
    };
    let bytes = response.bytes();
    if bytes.len() < 3 {
        return false;
    }
    from_bcd(bytes[2]) >= TRACKS_NEEDED
}
