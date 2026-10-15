# Authored feature journeys

Build the ordinary gate disc and symbol list with `make gate-disc`, then run:

```sh
psoxide-gate run tests/completion-chain.toml --strict
psoxide-gate run tests/completion-two-player-reassignment.toml --strict
psoxide-gate run tests/completion-cars-arenas.toml --strict
psoxide-gate run tests/completion-paints.toml --strict
psoxide-gate run tests/completion-music-brightness.toml --strict
psoxide-gate run tests/completion-movement-inputs.toml --strict
```

The continuous solo chain navigates all six match rules, returns normally from each result, plays a bounded practice match and watches the complete four-show attract cycle with automatic menu return. Separate journeys offer shorter rule-specific replays and a two-pad timed draw. There are three authored selectable cars and eight paints. These journeys use ordinary pad input and read-only shipping observables, with no gameplay-state injection.

Port 2 is a separate boot fixture because the current runner fixes its connection at startup. Swapping sides changes screen assignment: port 1 still drives blue and port 2 still drives orange. The two-player journey checks each port after the swap as well as pause, result and menu return.

Practice preserves the selected time/goal rule and parks the opponent. The authored pause menu has Resume, Settings and Quit, plus Swap for two players. A fresh-match reset is reached through Quit and normal selection. These tests do not assume an unimplemented Restart row or an unlimited practice mode.

Music, Sound, Brightness and Screen X/Y have saved profile values. Arena and Track are session settings. Sound's exact card-write/cold-reboot journey remains `save-repin.toml`. The additional Music/Brightness route proves real card writes and exact loaded setting-row equality.

Screen offsets currently need a CPU-only diagnostic:

```sh
psoxide-gate run tests/completion-supported-settings.toml --no-hw
```

Strict hardware capture reports that screen-offset displays defer to the CPU frame. This is an outstanding runner coverage limitation. Do not call the CPU-only result full renderer acceptance. Likewise, the current runner's CPU3 cell enlarges its CPU1 image; native CPU3 execution remains pending.

`completion-settled-results.toml` preserves both the first results transition and the settled winner screen. The first transition can retain a queued goal banner; twelve normal ticks later the winner heading is clear. Preserve both captures when diagnosing that presentation transition.

Timed and practice kickoff witnesses wait eight neutral ticks before capture and require a non-dark scene. The immediate selection-to-match transition can show only the HUD while the first match ordering table warms up. Historical immediate contacts remain transition evidence; the settled witnesses establish the rendered arena.

NitroXide is an original PS1 game. Its authored rule/control design and fixed-point simulation cases define behavior acceptance. Comparisons to its inspiration remain a separate scope. These journeys establish the fields and contacts they actually check, and do not claim every feature, native CPU3 execution or a completed full-game certificate.

## Sound effect journeys

`sfx-menus`, `sfx-save-ok`, `sfx-save-no-card`, `sfx-garage-two-player`, `sfx-match`, `sfx-goal`, `sfx-final`, `sfx-timed-end` and `sfx-demo` prove that each sound effect is keyed on its event. They use the `spu_kon` assert, which counts SPU key-ons by voice (`voice`, optionally `voice_hi`) and by the sample's SPU address (`addr`), over the whole run or the last `window` ticks, against `min_count` and `max_count`. The runner at the time of writing has no such assert: `tests/psoxide-gate-spu-kon.patch` adds it to `tools/psoxide-gate` (apply it with `git apply` in a PSoXide-emulator checkout at aad98fb). With `PSOXIDE_GATE_KON_LOG=<file>` set the patched runner also writes every key-on as `tick,voice,start,volume,pitch`.

The addresses are where the bank puts each sample: the bank is laid down from 0x1010 in the order of `BANK` in `game/src/audio.rs`, each sample followed by a 16 byte parking block. If a sample is added or resized, the addresses after it move, and the journeys have to follow. Today:

```
hit_punch        0x1010
pickup_coin      0x2c60
explosion_short  0x4870
swoosh           0xa400
ui_move          0xdba0
ui_tick          0xdeb0
ui_select        0xe100
ui_back          0xe8f0
ui_deny          0xef70
ui_chime         0xfae0
ui_saved         0x10fa0
ui_open          0x12070
ui_close         0x128e0
beep             0x13060
go               0x13950
whistle          0x15010
horn             0x19610
crowd_roar       0x1d8a0
crowd_gasp       0x23200
crowd_loop       0x255d0
engine_loop      0x28f10
skid_loop        0x293a0
jump             0x2c000
flip             0x2c7f0
land             0x2d6d0
bounce           0x2e0c0
net              0x2eca0
bump             0x30160
sonic            0x30b50
pad_big          0x33050
empty            0x34900
```

```sh
psoxide-gate run tests/sfx-menus.toml --no-hw
psoxide-gate run tests/sfx-match.toml --no-hw
```

They use ordinary pad input and read-only shipping observables, like the rest, and need the no-music gate disc from `make gate-disc`.
