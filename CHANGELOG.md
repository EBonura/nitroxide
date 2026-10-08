# Changelog

## 0.4.1 | 2026-10-08

A fix for how the ball rolls, a fresh set of attract demo matches and clearer
tyre tracks, on top of 0.4.0.

- The ball now plays the same in every direction. Before, a ball rolling
  toward one end or side lost speed to drag and stopped, while the same ball
  rolling the other way kept going until it hit a wall. Mirrored shots and
  rolls now match.
- Attract demo: four new matches (one view and split screen) picked for the
  fixed ball, with more touches, saves, flips and wall driving before the
  first goal.
- Tyre tracks are wider, darker and last longer (8 seconds, then a 4 second
  fade), and a wheel starts marking the pitch sooner when you slide.

## 0.4.0 | 2026-10-08

Attract demo, rounder arena, a new ball, explosions and tyre tracks, a steadier
split screen, on top of 0.3.0.

- Attract demo: leave a menu alone for 30 seconds, or pick DEMO on the main
  menu, and four bot-against-bot matches play (one view and split screen,
  day, sunset and night). Any button returns to the main menu.
- Arena: the corners are rounded where the corner planes meet the walls, and
  the end walls have a smaller floor ramp than the sides, to match the
  standard arena. Ball and cars follow the new curves.
- The ball has a panelled design, with plates, dark seams and an amber light
  in each pentagon, instead of flat white and black facets.
- The landing hoop under an airborne ball is a smooth ring that stays round
  at any distance, and it no longer draws over a car standing near it.
- Goals and demolitions blow up in glowing flashes, rings, fireballs, sparks
  and smoke. Sliding rear wheels leave tyre tracks on the pitch.
- Split screen holds 30 fps with both cars close together or ball cam looking
  down the arena.
  Cars in split screen no longer turn into low-detail wedges on wheel slabs,
  and the sky no longer shows through the pitch or the foot of the walls when
  the camera is close.
- The chase camera stays above the pitch when you drive up a wall, instead of
  looking at the grass from underneath.
- The view down the full length of the pitch, roof and lights included, holds
  60 fps.
- The now-playing plate no longer covers the goal banner.
- Saving settings no longer wipes another game's saves: a memory card with no
  header now gets a "format it?" prompt (Cross formats and saves, Circle or
  Start leaves the card alone for this session), and a missing card is still
  skipped silently.
- Pinned newer PSoXide SDK, engine and emulator components.

## 0.3.0 | 2026-09-26

The arena and HUD overhaul, on top of source 2026.09.05. Uploaded to itch.io by
hand on 2026-09-26 from the library disc; the itch workflow did not run for
this version.


- Arena: each half of the pitch, its walls and the roof in its team's colour;
  each goal lit in its team's colour.
- Boost pads drawn as light on the pitch, with the floating orbs restored over
  them.
- Goal boxes and arcs marked at each end of the pitch, drawn as geometry (no
  bending near the camera, at least two pixels thick at distance).
- A crowd in stands behind the enclosure.
- A hoop and a growing landing disc on the pitch under an airborne ball.
- HUD: the scoreboard shrinks to a tab during play, every text colour shows as
  authored (no more saturated tints), and the goal banner and results screen
  text are outlined.
- Split-screen ball cam keeps your car in frame.
- Faster drawing: wheels and car vertices posed on the GTE, arena and ball
  vertices through the SDK's scheduled RTPS, a distant opponent and ball drawn
  from low-detail meshes, near floor tiles split with shifts, and a
  profile-placed I-cache link order. Full-screen matches submit frames
  pipelined at 60 Hz with a per-tick chase camera.
- The music's drive status is read on the tick after asking for it.
- Pinned newer PSoXide SDK, engine and emulator components, with the SDK's
  load-delay filler search and hazard trampolines.

## Source 2026.09.05

This source snapshot is tagged `source-2026.09.05`. Download versions are
listed separately below; source cleanup does not replace an already published disc.

- Pinned SDK and engine sources separately and switched to the standalone emulator.
- Added post-link load-delay protection to guest builds.
- Removed unused simulation fields and profiling leftovers.

## 0.2.4-split.20260905 | 2026-09-05

Standalone disc published on itch.io.

- Published the SDK/engine split build of NitroXide 0.2.4.
