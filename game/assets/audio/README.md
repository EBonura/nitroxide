# Sound effects

The sound effects come from two places.

## Recorded impacts

Four cooked PS1 ADPCM files (`.psau`), copied from PSoXide's vetted pack rather
than re-cooked here, so the bytes are identical to the ones its
`psxed audio-pack` run produced and its `report.json` records the hashes for.

| File | Source clip |
| --- | --- |
| `hit_punch.psau` | `Impact/Retro Impact Punch 07.wav` |
| `pickup_coin.psau` | `PickUp/Retro PickUp Coin 04.wav` |
| `explosion_short.psau` | `Explosion/Retro Explosion Short 01.wav` |
| `swoosh.psau` | `Swoosh/Retro Swooosh 02.wav` |

Source: [Kronbits Free SFX](https://kronbits.itch.io/freesfx), **CC0-1.0**.
CC0 requires no attribution and imposes nothing on this repo's own licence;
the table is here so the provenance is auditable, not because it is owed.

Arcade rather than recorded, which suits the machine: a 1994 console mixing
four voices at 44.1 kHz through ADPCM flatters a bright synthetic impact and
exposes a real recording's noise floor.

## Recordings

Five sounds are cut from recordings that are not in this repository:

- Crowd: Gregor Quendel, "Free Crowd Cheering Sounds" (MP3 pack), **CC BY 4.0**,
  via OpenGameArt. Credit: "Crowd sounds: Gregor Quendel, Free Crowd Cheering
  Sounds, CC BY 4.0".
- Tyre squeal: Tom Haigh, "Car tire squeal skid loop" (24 bit 96 kHz WAV, three
  seconds, already a seamless loop), **CC BY 3.0**,
  <https://opengameart.org/content/car-tire-squeal-skid-loop>. Credit: "Tyre
  squeal loop: Tom Haigh, CC BY 3.0".

`tools/sfx-synth/recordings.manifest` names each source file (relative to one
folder) with its size and FNV-1a hash, the offsets, fades, loop crossfade, rate
and target level of each cut. `make recordings SOURCES_DIR=/path/to/sfx-sources`
decodes them (the `symphonia` MP3 decoder, or the WAV reader), cuts, normalises
to the levels the game plays them at, resamples and encodes ADPCM, and refuses a
file that is not the recorded one. The gain each cut was given is recorded in
`tools/sfx-synth/recordings.applied`.

| File | Source | Cut |
| --- | --- | --- |
| `crowd_loop.psau` | 10 - Ambience | 18 to 26 s, the next 1.5 s crossfaded onto the head, looped |
| `crowd_roar.psau` | 04 - Strong cheering II, short | 4.8 to 8.8 s, a swell with a faded tail |
| `crowd_gasp.psau` | 06 - Soft cheering II | 1.9 to 3.3 s, a rising murmur |
| `crowd_cheer.psau` | 08 - Rhythmic cheering | 6.6 to 9.6 s, a chant: at a kickoff and a second into each goal |
| `skid_loop.psau` | Haigh, tires_squal_loop.wav | the whole file, looped on its own seam |

## Synthesised effects

Every other `.psau` here is rendered by `tools/sfx-synth` from sines, a
xorshift noise source, filters and envelopes, then cooked to SPU ADPCM by the
SDK's shared encoder (`psx-audio-cook`). Nothing in them is sampled from a
recording, so they are this project's own work under its licence and need no
credit. `make sfx` rewrites them; the output is deterministic, so a change to a
sound shows up as a diff. The tool prints each sound's rate, ADPCM size, peak
and RMS, which are the numbers the play volumes in `game/src/audio.rs` were
levelled from (music sits at -15 LUFS).

| Group | Files |
| --- | --- |
| Menus | `ui_move`, `ui_tick`, `ui_select`, `ui_back`, `ui_deny`, `ui_chime`, `ui_saved`, `ui_open`, `ui_close` |
| Match calls | `beep`, `go`, `whistle`, `horn` |
| Car | `engine_loop`, `jump`, `flip`, `land`, `bump`, `sonic`, `pad_big`, `empty` |
| Ball and net | `bounce`, `net` |

`engine_loop` (and the recorded `crowd_loop` and `skid_loop`) are hardware loops: they hold a
whole number of cycles in a whole number of ADPCM blocks, so the loop point is
the waveform continuing, not a seam. The crowd loop is crossfaded and the tyre loop is not; both seams are tested.

SPU RAM: the bank is 342,880 bytes of the 520,176 usable (66 percent), parking
blocks included. The build fails if it outgrows the SPU (`BANK_BYTES` in
`game/src/audio.rs`).

# Music

Six tracks, `jungle1-a`, `jungle1-b`, `jungle2-a`, `jungle2-b`, `dnb-a` and
`dnb-b`, already encoded as XA-ADPCM (`MUSIC1.XA`, `MUSIC2.XA`) and committed in
[`game/assets/music`](../music/README.md), which also holds the credits and the
steps to cook them again from the source WAVs (`make music-check`,
`make music-refresh`). `pack` presses the committed files on the disc, so no WAV
is needed to build. `game/src/music_map.rs` is the table `xa-pack` wrote (file,
channel and length of each track).

Source: Mulula VGM Loops, Ambient Jungle Vol 1, Vol 2 and Ambient Jungle D&B
Vol 3, **CC BY 4.0**, <https://mululavgmloops.itch.io/>. The licence asks for
this attribution, which the README, the changelog and the release notes carry.

## The mix

The SPU adds every voice and the CD (the XA music) into one 16-bit sum and
clips it before the main volume is applied, so the main volume does not make
room. The music is -15 LUFS and peaks within a dB of full scale on its own.
`game/src/audio.rs` therefore mixes the effects and the music at 0.6 (the
mixer's `Master`) and sets the main volume to 0x3FFF to give the level back, so
the sum has 4.4 dB of room, and the goal explosion, the horn and the final
whistle duck the music while they sound (`Sound::with_duck`). Check a change
to a level with a headless dump of the attract demo (`--dump-audio`) on an
emulator that applies the main volume as documented (the demo disc's, dddd2814
or newer): no second of it should have more than a handful of samples at
32,767.
