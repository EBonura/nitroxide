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
| Crowd | `crowd_roar`, `crowd_gasp`, `crowd_loop` |
| Car | `engine_loop`, `skid_loop`, `jump`, `flip`, `land`, `bump`, `sonic`, `pad_big`, `empty` |
| Ball and net | `bounce`, `net` |

`engine_loop`, `skid_loop` and `crowd_loop` are hardware loops: they hold a
whole number of cycles in a whole number of ADPCM blocks, so the loop point is
the waveform continuing, not a seam.

SPU RAM: the bank is 213,216 bytes of the 520,176 usable (41 percent), parking
blocks included. The build fails if it outgrows the SPU (`BANK_BYTES` in
`game/src/audio.rs`).

# Music

Six tracks, external to this repository: `jungle1-a`, `jungle1-b`,
`jungle2-a`, `jungle2-b`, `dnb-a` and `dnb-b`, 16-bit 37.8 kHz stereo WAVs
(chained, loudness-matched loops). `make music MUSIC_DIR=...` packs them with
the SDK's `xa-pack` into `MUSIC1.XA` and `MUSIC2.XA`, and `pack` presses those
on the disc. `game/src/music_map.rs` is the table `xa-pack` wrote (file,
channel and length of each track); `make music` fails if it differs from the
pack, and the fix is copying `build/music/music_map.rs` over it.

Source: Mulula VGM Loops, Ambient Jungle Vol 1, Vol 2 and Ambient Jungle D&B
Vol 3, **CC BY 4.0**, <https://mululavgmloops.itch.io/>. The licence asks for
this attribution, which the README, the changelog and the release notes carry.
