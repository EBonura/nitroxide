// SPDX-License-Identifier: GPL-2.0-or-later
//! The soundtrack: six ambient jungle and drum and bass tracks on the disc as
//! XA-ADPCM, played as a shuffled playlist over the front end and the match
//! alike.
//!
//! `make music` packs the tracks with the SDK's `xa-pack` into `MUSIC1.XA` and
//! `MUSIC2.XA` (four songs interleaved in the first, two in the second) and
//! checks that `music_map.rs`, the table it wrote, is the one compiled in:
//! which file and channel hold which song, and how long each runs. The files
//! are found by name in the disc's root directory when the game boots, so no
//! sector address is baked in and the image can sit anywhere on a multi-
//! program disc (`psx_io::disc_base` shifts it).
//!
//! The drive decodes XA itself, so the CPU never sees the audio. What the
//! drive cannot do is read data and stream a song at once, so every disc read
//! the game makes (`WORLD.PAK` and the directory lookup) happens at boot,
//! before the first song starts, and nothing reads the disc afterwards. A
//! future runtime read has to stop the song first and start one again after.
//!
//! The order is a shuffle (`nitroxide_sim::playlist`): each song once, then a
//! fresh shuffle that does not open with the song that just ended. The TRACK
//! row of the settings menu steps through the same order.

use nitroxide_sim::playlist::Playlist;
use psx_fmv::iso;
use psx_io::cd::xa::{DriveSpeed, Event, File, Player};
use psx_io::periph::Cd;
use psx_io::timers::{counter, Timer};
use psx_pack::cd::{SectorReader, SECTOR_WORDS};

use crate::music_map::{XA_FILE_COUNT, XA_FILE_NAMES, XA_FILE_NUMBERS, XA_SONGS};

/// What the songs are called on screen, in the order `xa-pack` was given them
/// (`music_map::XA_SONGS`). The tracks are chained loops from Mulula VGM
/// Loops' Ambient Jungle Vol 1, Vol 2 and Ambient Jungle D&B Vol 3, CC BY 4.0;
/// the credit is in the README and the release notes.
pub const SONG_NAMES: [&str; XA_SONGS.len()] = [
    "JUNGLE 1A",
    "JUNGLE 1B",
    "JUNGLE 2A",
    "JUNGLE 2B",
    "DNB A",
    "DNB B",
];

/// How long the now-playing banner stays up once a song starts. Four seconds:
/// long enough to read twice, gone before it reads as HUD. Public so the
/// drawing side can time its slide-out against the same clock.
pub const BANNER_TICKS: u32 = 240;

/// The files are interleaved for single speed: the drive reads nothing else
/// while a song plays.
const SPEED: DriveSpeed = DriveSpeed::Single;

/// Ticks to wait before asking the drive for anything. It has just loaded
/// this program's data and the head is still settling; the launcher's own
/// handshake takes the same precaution.
const PROBE_TICK: u32 = 90;
/// Between a refused start and the next attempt.
const RETRY_TICKS: u32 = 60;
/// Starts attempted before giving the drive up until the player asks again.
const MAX_ATTEMPTS: u8 = 3;

/// The drive is asked where its head is this often once a song is near its
/// end. A song file ends in 16 guard sectors (0.2 s) the head reads through,
/// so six ticks (0.1 s) cannot step over the end unseen.
const POLL_TICKS: u32 = 6;
/// How long before a song's computed end the polling starts. The sim ticks at
/// 60 Hz on the console and a percent or so slow on the headless emulator, so
/// the lead covers a few percent of the longest song.
const POLL_LEAD_TICKS: u32 = 60 * 12;
/// Sim ticks per second.
const TICKS_PER_SECOND: u32 = 60;

pub struct Music {
    /// The disc carries the song files.
    available: bool,
    /// The player has not turned it off.
    enabled: bool,
    files: [Option<File>; XA_FILE_COUNT],
    player: Player,
    list: Playlist,
    /// Tick to start the current song on, and the attempts made so far.
    start_at: Option<(u32, u8)>,
    /// A song has been started and has not finished.
    playing: bool,
    /// Tick the current song started on, for the now-playing banner.
    announced_at: Option<u32>,
    /// Ticks from which the head is polled for the song's end, and the next
    /// tick to poll on.
    poll_from: u32,
    next_poll: u32,
    /// How long the current song runs, in milliseconds of the head's time.
    song_ms: u32,
}

impl Music {
    pub fn new() -> Self {
        Music {
            available: false,
            enabled: true,
            files: [None; XA_FILE_COUNT],
            // SAFETY: nothing else drives the CD controller while the game
            // runs. The boot-time `SectorReader`s (`assets.rs`, `locate`) are
            // finished before any song starts, and nothing reads the disc
            // afterwards.
            player: Player::new(unsafe { Cd::steal() }),
            list: Playlist::new(XA_SONGS.len(), 1),
            start_at: None,
            playing: false,
            announced_at: None,
            poll_from: 0,
            next_poll: 0,
            song_ms: 0,
        }
    }

    /// Find the song files in the disc's root directory and seed the shuffle.
    /// Boot only: this reads the disc, which the music must not be doing.
    pub fn locate(&mut self) {
        let saved_irq_mask = psx_io::irq::mask();
        let mut reader = SectorReader::new();
        let mut sector = [0u32; SECTOR_WORDS];
        self.available = false;
        if read_sector(&mut reader, iso::PVD_LBA, &mut sector) {
            if let Some((root_lba, _)) = iso::root_directory(sector_bytes(&sector)) {
                if read_sector(&mut reader, root_lba, &mut sector) {
                    self.available = true;
                    for (i, slot) in self.files.iter_mut().enumerate() {
                        *slot = iso::find_in_directory(sector_bytes(&sector), XA_FILE_NAMES[i])
                            .map(|(lba, size)| {
                                File::from_directory_entry(lba, size, XA_FILE_NUMBERS[i], SPEED)
                            });
                        self.available &= slot.is_some();
                    }
                }
            }
        }
        psx_io::irq::set_mask(saved_irq_mask);
        // How long the disc took to spin up and answer, to the cycle: the
        // one thing that differs between boots of the same program.
        let seed = ((counter(Timer::Timer2) as u32) << 16) ^ counter(Timer::Timer1) as u32;
        self.list = Playlist::new(XA_SONGS.len(), seed);
        self.player.set_volume(0x80, 0x80);
    }

    /// Is there music to talk about? The pause menu hides the row when not,
    /// rather than offering a switch that does nothing.
    pub fn available(&self) -> bool {
        self.available
    }

    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// What the current song is called.
    pub fn track_name(&self) -> &'static str {
        SONG_NAMES[self.list.current()]
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
        Some((self.track_name(), elapsed))
    }

    /// Step to the previous or next song of the shuffle. Starts it at once
    /// when the music is on; off, it only moves the pointer the next start
    /// will use.
    pub fn cycle_track(&mut self, step: i32, tick: u32) {
        if step >= 0 {
            self.list.next();
        } else {
            self.list.prev();
        }
        if self.available && self.enabled {
            self.begin_song(tick, true);
        }
    }

    /// Turn the music on or off. Off stops the drive rather than only
    /// declining to start it again, or the current song would play on under
    /// a menu that claims it is off.
    pub fn set_enabled(&mut self, on: bool, tick: u32) {
        self.enabled = on;
        if !self.available {
            return;
        }
        if on {
            self.begin_song(tick, true);
        } else {
            self.start_at = None;
            self.playing = false;
            self.player.stop();
        }
    }

    /// Arrange for the current song to start on `tick`. `stop` pauses a song
    /// still streaming first; the end of a song has already paused the drive.
    fn begin_song(&mut self, tick: u32, stop: bool) {
        if stop && self.playing {
            self.player.stop();
        }
        self.playing = false;
        self.start_at = Some((tick, 0));
        self.announced_at = Some(tick);
    }

    /// One tick. Starts a song that is due, and once one is playing, watches
    /// for its end so the next can follow.
    pub fn update(&mut self, tick: u32) {
        if !self.available || !self.enabled {
            return;
        }
        if let Some((due, attempts)) = self.start_at {
            // Nothing is asked of the drive before the boot reads have
            // settled, whatever the caller asked for.
            if tick < PROBE_TICK || tick.wrapping_sub(due) >= u32::MAX / 2 {
                return;
            }
            self.start(tick, attempts);
            return;
        }
        if !self.playing || tick.wrapping_sub(self.poll_from) >= u32::MAX / 2 {
            return;
        }
        if tick.wrapping_sub(self.next_poll) >= u32::MAX / 2 {
            return;
        }
        self.next_poll = tick.wrapping_add(POLL_TICKS);
        match self.player.poll() {
            // Finished: the song reached its end and the drive paused. Idle:
            // the drive stopped without our asking, which is also the end.
            Event::Finished | Event::Idle => {
                self.list.next();
                self.begin_song(tick, false);
            }
            // The songs of a file are padded with silence to the longest, and
            // the drive only reports the end of the file, so a shorter song
            // is ended by the head's time reaching its own length.
            Event::Playing | Event::Looped => {
                if self.player.is_streaming() && self.player.elapsed_millis() >= self.song_ms {
                    self.player.stop();
                    self.list.next();
                    self.begin_song(tick, false);
                }
            }
        }
    }

    /// Ask the drive for the current song.
    fn start(&mut self, tick: u32, attempts: u8) {
        let (file, channel, ms) = XA_SONGS[self.list.current()];
        let Some(file) = self.files[file as usize] else {
            self.start_at = None;
            return;
        };
        if self.player.play(file.song(channel), false).is_ok() {
            self.start_at = None;
            self.playing = true;
            self.announced_at = Some(tick);
            self.song_ms = ms;
            self.poll_from = tick
                .wrapping_add(ms / 1000 * TICKS_PER_SECOND)
                .wrapping_sub(POLL_LEAD_TICKS);
            self.next_poll = self.poll_from;
        } else if attempts + 1 < MAX_ATTEMPTS {
            self.start_at = Some((tick.wrapping_add(RETRY_TICKS), attempts + 1));
        } else {
            self.start_at = None;
        }
    }
}

fn read_sector(reader: &mut SectorReader, lba: u32, sector: &mut [u32; SECTOR_WORDS]) -> bool {
    let ok = reader.prepare() && reader.start_read(lba) && reader.read_sector(sector);
    reader.stop();
    ok
}

fn sector_bytes(sector: &[u32; SECTOR_WORDS]) -> &[u8] {
    // SAFETY: `[u32; N]` reinterpreted as its own bytes; alignment shrinks and
    // the length is the array's.
    unsafe { core::slice::from_raw_parts(sector.as_ptr().cast::<u8>(), SECTOR_WORDS * 4) }
}
