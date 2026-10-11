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
