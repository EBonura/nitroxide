# Changelog

## Unreleased

The boost exhaust is a plume of soft round puffs that swell and cool from the car's
colour to grey smoke as they fade, no longer a column of squares. Taking a boost pad
now bursts in the world, in the boost's amber: a flash on the orb, a ring that runs
out over the pitch and a quicker one facing the camera, and sparks thrown up and out,
sorted with the scene instead of laid over it as squares.

The kickoff fly-over is a high swing above the roof, looking down on the pitch with the
roof cover and the lamps left out until the camera comes down under them, so it no
longer squeezes between the walls and the stands.

The title's orbiting camera no longer leaves holes in the pitch: the front end clips the
pitch tiles that reach behind the lens the way a split-screen view does.

Split screen tessellates the pitch with the same distance bands as a single view. They
were set in August for 160-pixel-wide views and left at (400, 900) uu when the views
became 320 by 120, so the pitch beyond 900 uu was one 1,024 uu quad and its mown stripes
bent into kinks.

The arena atlas is read into the idle textured-quad packet sets instead of a 100 KB
array on the boot stack. That stack reached the end of `.bss` once the image grew by
about two kilobytes, and zeroed the pad handler's stack and the VBlank counter under
it, so the boot waited for a vblank that never came.

## 0.5.0 | 2026-10-26

The loading screen is the slanted-cell boost bar on its own, larger and centred. It
fills with the load's real progress (the lighting bake, the arena texture read and the
sound bank at boot, and the lighting bake again when the arena or its time of day is
changed from a menu), with the pickup sweep running along the lit cells and a white
flash as it completes. A load too quick to see is held for at least forty frames and
filled smoothly.

The countdown's numerals and the GO! are centred by the pixels they draw rather than
by their font cell (the 1 sat to one side), in single player and in each split half;
the left and right margins between the ink and the ends of the navy plate agree within
a pixel. As each number leaves, a faint outline of it swells to about twice its size and
fades out over ten ticks, drawn behind the banner so it never touches the next
number (the 1 echoes through the GO!), on overtime kickoffs too. The GO! is white,
flashes for five ticks and is gone, with no fade.

A timed match that is level when the clock runs out now goes to overtime instead of
ending in a draw: the ball and both cars go back to their kickoff spots, three, two,
one, and the next goal wins, after its celebration. An OVERTIME banner opens out
from the middle of the screen for the kickoff, the clock counts up from +0:00 in
violet while it lasts, and the results say the match was won in overtime. Goal-limit
matches and timed matches that are not level are unchanged. The overtime kickoff has
no camera fly-over. The published gate clock carries the overtime flag (0x40000000)
and the ticks of live overtime play. The GO! now flashes for eight ticks as the cars
launch and is gone, leaving the pitch clear.

The menus move. On the title the logo slides in, the camera circles the staged car
in a slow turn (about fifty seconds), and the selection bar slides from one row to
the next with a band of light crossing it, in the pause menu too. The car-select
screen is a turntable: the cars turn steadily, about every eight seconds, on a ring
of light with dashes that turn with them. The attract demo now starts after a
minute without a press rather than thirty seconds.

The match gives more feedback. An arrow on the rim of the view points at the ball
when it is off screen (below the scoreboard in the top view), and a dotted drop line with a ring runs from the ball to the
pitch when it is in the air. A boosting car leaves a
trail in its own colour, a supersonic one gets speed lines at the rim of the view,
a car on a wall throws sparks, a hard landing raises dust, and a pad taken gives a
flash and a ring. A demolished car's own view counts down its respawn, a timed match
announces its last minute with a banner, the clock pops each second of the last
ten, and in single player the field of view opens up a notch while boosting. The
particles are projected through the frame's own cameras and drawn over it, a pool of
forty shared by both views.

Every kickoff opens with a camera fly-over of the arena and a big countdown. The
match start and each restart after a goal first swing the camera round the arena,
high and looking at the centre spot, and bring it down onto the chase camera in
about a second and a half; any button (but Start) cuts it short. Then three, two,
one punch in on a slanted plate with the two team colours at its ends, each landing
on the tick of its beep, and a GO! bursts out when play is released, with the same
timings as the go and cheer. The sim stands still through the fly-over, and the
match start now has the same three-second countdown a goal restart always had.

The pause menu is rebuilt. It builds in out of tiles, the sweep clipped to its own
area with the match dimmed around it, and breaks up the same way on resume. It has
a header band with the rule, the arena and who is playing, the live score and
clock in the team colours, slanted buttons with the title menu's cyan bar sweeping
in (Resume, Sides in two-player, Settings, Controls, Restart match and Quit to
menu, the last two asking Yes or No first) and a hint bar along the foot. The
tile sweep stays full-screen for real scene changes: kickoff from the select
screen, restart, quit and the final whistle.

A tile sweep now hides every change between the match and its menus. Sixteen pixel
tiles grow from their centres across the screen from left to right, the screen
underneath is swapped on the tick the last one is down, and the tiles shrink away
the same way: half a second in all, drawn as flat quads over the finished frame
with no copy of the old one. It covers quitting to the title, restarting, kicking off from the select screen and
the final whistle into the results.
The world stands still under it and the pads are ignored until it is done.

The boost dial in the corner is now a bar of ten slanted cells, the shape of the
menu buttons, going from amber to orange, with the number on a small slanted tag
above it, inside the television's title-safe area. A pad pickup runs a white sweep
along the bar with three chevrons flashing past its end, the big pad adds an outline
flash, the fill climbs to a new value over a fifth of a second, and while boosting
the last lit cell flickers and short streaks trail off the back. Split screen gets
one per half, at two thirds the size. The old dial is gone from the ordering table.

The credits moved off the boot splash. The splash is the Bonnie Studios logo and
"Built with PSoXide" and nothing else; the cars, music, crowd, tyre squeal, impact
and font credits are on a Credits row at the bottom of the title menu, two pages,
styled like the controls page. The links are in the README and the release notes.

The goal sound no longer clips. The SPU adds every voice and the music into one
16-bit sum and clips that sum before the main volume, and the music alone peaks
within a dB of full scale, so the goal's explosion, horn, net and crowd on top of
it put 6 to 14 percent of the samples on the rail for the two and a half seconds
of the celebration, a crushed, harsh noise (a hard kickoff hit crackled too). The
effects and the music are now mixed 4.4 dB lower and the main volume is raised to
give it back (the same loudness to within half a dB), the goal, the horn and the
final whistle pull the music down while they sound (the SDK's `Master` duck, which
the game had not wired up), and the goal's sounds are levelled so that their peaks
add up to well under full scale. Over the 255 s attract demo the clipped samples
went from 77,273 to 6, and the loudest sample of a goal is 0.85 of full scale at
most.

The air game plays like Rocket League's. A car in the air used to turn at a fixed,
slow rate (yaw 1.4 and roll 3.1 radians a second) and a dodge was a push along the
ground with a drawn spin. The stick is now a torque with Rocket League's own
strength and damping (pitch 130, yaw 95, roll 400 against damping 30, 20 and 50)
and the whole spin is capped at its 5.5 radians a second, so a car turns quickly,
keeps turning when you let go and has to be caught. A flip is a real angular
impulse from the same orientation the car is drawn in: the kick is 500 uu/s
scaled up to 1.9 times sideways and 2.5 times backward by speed, the flip's
torque and the pull-down on its climb are Rocket League's, and so are the flip
cancel, the stall, the half flip and the short pitch lockout after a flip. L1
with the stick is free air roll, and Square and R1 roll left and right. The stick
is proportional in the air. A flip at speed keeps its kick instead of being cut
back to the throttle's top speed, a fast car falling is held to the 2300 uu/s
ceiling, a car lands facing the way its nose pointed, and a car leaving a wall by
a jump leaves in the attitude it had. L1 with the stick to the right used to roll
the car to the left; it rolls right now. The controls are on a page of their own
(Select, from the title or the pause menu). The numbers are checked against
RocketSim in `sim/PARITY.md`. Three of the attract demo's four matches use
different seeds, since flips changed how the old ones played out.

The dotted pale line along the lower edge of a goal post halo, and the short bright
dashes under other glows (the ball landing disc, the goal-line strip, sparks), are
gone. A glow quad samples one row past its tile on the pixels of a sloped edge,
and the ball texture sat in that row; it now starts one row lower, behind an
empty one. The picture is otherwise unchanged and the frame cost is identical.

The arena net is drawn in full in every frame. The packet governor is gone: it
dropped the net's near columns and drew the far pitch lines the old way in any
frame that already held 330 packets, which is most of a busy view. The frame
time it saved is found elsewhere, with no change to the picture beyond that: the
floor culls its tiles by sums that step along the lattice, the ball's facet
normals come from a table built at boot, a packet's ordering-table slot is
worked out before its words are written, the wall sweep and the roof no longer
call `memset` and `memcpy` every row, floor edges snap with a shift, and the
build links in the plain order, which measured as fast as a placed layout once
the governor was gone. On the gate tapes the work per frame is 2 to 3 percent
under the 0.4.1 build and the frames that miss a vblank are the same or fewer.

The music is in the repository now, already encoded as the XA files the disc
carries (`game/assets/music`, 82 MB), so a fresh clone and the release workflow
press a disc with its six tracks and need no WAVs from anywhere else. The
release workflow no longer fetches an audio repository. `make music-check` and
`make music-refresh` cook the files again from the source WAVs for a maintainer,
and check the result is byte for byte the committed music. Music: Mulula VGM
Loops, Ambient Jungle Vol 1, Vol 2 and Ambient Jungle D&B Vol 3, CC BY 4.0,
https://mululavgmloops.itch.io/ (credits and changes in
`game/assets/music/README.md`).

The pads are read from the SDK's interrupt-driven pad engine instead of being
polled in the frame loop, so an empty second port costs the game nothing and
the memory card takes the port as a lease for each save step. Analog mode is
requested through the engine and confirmed a few frames later.

The curved ramp between the pitch and the wall keeps one texture at every
distance: the wall panel is stretched over the whole curve, so a ramp seen
up close and from afar shows the same panel, not a stripe per cut.

Save Settings no longer freezes the game. The card is written a frame at a
time while the menu keeps drawing and the pads keep being read: on a formatted
card the 29 vblanks the screen used to stand still are gone, and agreeing to
format a blank card no longer holds the game for 373 vblanks. The row reads
SAVING until the last frame has landed.

Sound effects are now in stereo. Every sound that comes from something in the
arena (a car's engine, tyre squeal, boost, jump and landing, the ball, a boost
pad, a wreck, the goal) has a place: it gets quieter with distance and pans
toward the side it comes from. The opponent's engine, boost, skid and jumps are
heard too, where before only your own car made a sound. Split screen has two
listeners, one per view: each sound is heard from the view it is louder to, an
engine from the view it is nearest to, and a goal from the middle of both.
Your own car sounds as loud and as centred as before. The announcer's calls,
the crowd and the menus are not in the arena and stay centred.

Under it the game now uses the SDK's shared sound mixer (psx-sfx) rather than
one voice per job: sounds ask for a lane of voices (menu, loops, world, car,
scene) and the mixer picks the voice and, when a lane is full, which sound to
drop. The goal horn and the final whistle are protected and are never cut by
anything. The samples and their levels are unchanged.

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
  the title menu's Credits page, in the README and in the release notes.

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
