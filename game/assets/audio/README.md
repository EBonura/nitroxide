# Sound effects

Cooked PS1 ADPCM (`.psau`), copied from PSoXide's vetted pack rather than
re-cooked here, so the bytes are identical to the ones its `psxed audio-pack`
run produced and its `report.json` records the hashes for.

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
