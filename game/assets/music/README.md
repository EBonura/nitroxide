# NitroXide music

The soundtrack is six tracks of XA-ADPCM (37.8 kHz stereo, single speed), stored
here already encoded as the files the disc carries. A fresh clone and CI build
the disc straight from them; no WAV is needed.

| File | Songs (channel) | Size |
| --- | --- | --- |
| `MUSIC1.XA` | jungle1-a (0), jungle2-a (1), dnb-a (2), dnb-b (3) | 48,252,416 bytes |
| `MUSIC2.XA` | jungle1-b (0), jungle2-b (1) | 33,741,184 bytes |

`music.json` is the pack manifest `psx-audio-cook xa-pack` wrote, `SHA256SUMS`
the digests of the three files (`make music-verify`, run by `make pack` and CI),
and `game/src/music_map.rs` the generated table the game compiles in.

## Credits and licence

Music: Mulula VGM Loops, Ambient Jungle Vol 1, Ambient Jungle Vol 2 and Ambient
Jungle D&B Vol 3, licensed CC BY 4.0
(<https://creativecommons.org/licenses/by/4.0/>), <https://mululavgmloops.itch.io/>.

Changes made, as CC BY 4.0 asks us to say: loops were chained into six pieces
with 4 s crossfades, resampled to 37.8 kHz, loudness-matched to -15 LUFS
integrated, dithered to 16 bits, and compressed to XA-ADPCM. The pieces are:

| Song | Loops, in order |
| --- | --- |
| jungle1-a | Air, Samanbaia, Frog, Return, Jungle (Vol 1) |
| jungle1-b | Jungle, Bubbles, Manana, Board, Tomate (Vol 1) |
| jungle2-a | Crassula, Monstera, Brunfelsia, Rhipsalis, Dracaena (Vol 2) |
| jungle2-b | Dracaena, Dypsis, Echeveria, Zamioculca (Vol 2) |
| dnb-a | Pera, Semente, Jumbo, Jaca, Lona (D&B Vol 3) |
| dnb-b | Lona, Corda, Sapo, Mamba (D&B Vol 3) |

The Read-ME shipped with the packs that carry one says "CC BY 4.0" and names the
author's pages; the credit also appears on the boot splash, in the README, the
CHANGELOG and the release notes. Keep those in step.

## Refreshing the files

Only needed when the songs change. The six source WAVs (the pieces above, 37.8
kHz 16-bit stereo) are not in the repository; `sources.sha256` lists the digests
the build accepts. With them in a folder:

```sh
make music-check   MUSIC_DIR=/path/to/wavs   # cook; fail unless byte-identical to the committed files
make music-refresh MUSIC_DIR=/path/to/wavs   # cook and replace the committed files and game/src/music_map.rs
```

Both cook with the `psx-audio-cook` of the SDK revision pinned in
`components.lock.json` (through `tools/audio-cook`). The encoder is
deterministic: `music-check` reproduces these bytes exactly.
