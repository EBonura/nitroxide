# Changelog

## 0.3.0 (not yet published)

The arena and HUD overhaul, on top of source 2026.09.05.

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
