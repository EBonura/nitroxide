# Changelog

## Unreleased

New sound effects: every menu action and most of the match now has a sound.

- Menus: cursor moves, value changes (higher or lower by direction), select,
  back, panel open and close, a garage lock-in, Save Settings success and
  refusal, the card format question, and the go at the start of a match. All
  of them are quiet, well under the music, and silent when Sound is off.
- Match: jump, double jump, flip, landing, ball bounce (floor, wall, ceiling,
  goal frame), car bump, supersonic, engine note that follows speed, tyre
  squeal in a powerslide, empty boost tank, a bigger sound for the big boost
  pads, the kickoff 3, 2, 1 and go, a tick for each of the last five seconds,
  the goal horn, the crowd's roar after a goal and its gasp at a near miss, a
  quiet crowd under the match, and the final whistle.
- The crowd (the murmur under a match, the roar after a goal, the gasp at a
  near miss, and a rhythmic cheer at kickoff and a second into each goal
  celebration) and the tyre squeal are cut from real recordings.
  Crowd sounds: Gregor Quendel, Free Crowd Cheering Sounds, CC BY 4.0,
  https://opengameart.org/
  Tyre squeal loop: Tom Haigh, CC BY 3.0,
  https://opengameart.org/content/car-tire-squeal-skid-loop
- Everything else new is synthesised by `tools/sfx-synth` (`make sfx`) from
  oscillators, noise and envelopes, so it carries no licence of its own.
- They add 291 KB to SPU RAM, which now holds 343 KB of 520 KB.

New music: six ambient jungle and drum and bass tracks, played as a shuffled
playlist.

- The soundtrack is six tracks of compressed XA audio inside the disc's single
  data track, replacing the four CD audio tracks. The disc has no audio
  tracks, and NitroXide no longer borrows the demo disc's menu music.
- Music plays over the menus and the match alike and shuffles: every track
  once, then a new order that never opens with the track that just ended. The
  TRACK row steps through the same order.
- Music: Mulula VGM Loops, Ambient Jungle Vol 1, Vol 2 and Ambient Jungle D&B
  Vol 3, CC BY 4.0, https://mululavgmloops.itch.io/

Six new cars: Razor, Fury, Bolt, Brute, Dash and Cinder, nine in all.

- The six come from Pavel 3D's Low Poly City Vehicles Pack (Sports Car 02, 03
  and 01, Muscle Car 01, Coupe 01 and Hot Rod), cooked to the same face budget
  as Comet, Hatch and Sprinter, each with its 60-face split-screen copy.
- Cars: Pavel 3D, Low Poly City Vehicles Pack, CC BY 4.0,
  https://pavel-3d.itch.io/low-poly-city-vehicles-pack. The credit is also on
  the boot splash, in the README and in the release notes.

Car models:

- Fewer see-through lines in the cars. The one-pixel gaps between a car's
  separately cut body, glass and trim parts are closed in the cook, and a dark
  core inside the cabin fills the gaps at the pillars. Counted on the garage
  tour, the pitch showing through a car fell by about two thirds.
- The Sprinter no longer has a grey slab poking up through its bonnet, and its
  front and rear bumpers, lamps and window panes are cut one by one instead of
  as pairs.

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
