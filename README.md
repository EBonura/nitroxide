# NitroXide

**[Download the latest version on itch.io](https://bonnie-studios.itch.io/nitroxide)**

> **Largely written with agentic coding.** I direct the agents and test their work in two places: PSoXide's emulator, which profiles every cycle, and a real PlayStation, which shows me where the emulator is wrong. Working between them is where the accuracy and the speed come from. [How PSoXide is built](https://ebonura.github.io/PSoXide/how-its-built/)

It's also on the [PSoXide Demo Disc](https://bonnie-studios.itch.io/psoxide-demo-disc) with the other Bonnie Studios PlayStation games, and you can [play it in your browser](https://bonnie-studios.itch.io/psoxide).

Rocket-powered car soccer for the original PlayStation, written in Rust on the
[PSoXide SDK](https://github.com/EBonura/PSoXide) and
[engine](https://github.com/EBonura/PSoXide-editor/tree/main/engine). Drive a car into a
ball, put the ball in the net, burn nitro to get there first.

An original game for PS1 hardware, licensed GPL-2.0-or-later.

## Screenshots

Captured at 1280×960 through PSoXide's hardware renderer. The game itself
runs at the original PlayStation's 320×240 resolution.

| **Sunset title screen** | **Match setup and arena selection** |
| :---: | :---: |
| [![NitroXide sunset title screen](readme-assets/menu-title-sunset.png)](readme-assets/menu-title-sunset.png) | [![NitroXide player, car, colour, and arena selection](readme-assets/menu-select-sunset.png)](readme-assets/menu-select-sunset.png) |
| **Daylight boost run** | **Sunset aerial** |
| [![Blue car boosting across the arena in daylight](readme-assets/gameplay-day-boost.png)](readme-assets/gameplay-day-boost.png) | [![Blue car jumping toward the ball at sunset](readme-assets/gameplay-sunset-aerial.png)](readme-assets/gameplay-sunset-aerial.png) |
| **Night goal celebration** | **Two-player split-screen** |
| [![NitroXide goal celebration under the night lighting](readme-assets/gameplay-night-goal.png)](readme-assets/gameplay-night-goal.png) | [![NitroXide two-player split-screen gameplay](readme-assets/gameplay-sunset-split.png)](readme-assets/gameplay-sunset-split.png) |

## Play it

The disc image is on [itch.io](https://bonnie-studios.itch.io/nitroxide),
music included: the soundtrack rides the disc as compressed XA audio inside
the one data track, so the download is the full experience. NitroXide also ships on the
[PSoXide Demo Disc](https://bonnie-studios.itch.io/psoxide-demo-disc), which
runs [in your browser](https://bonnie-studios.itch.io/psoxide) on the PSoXide
page, no console needed.

## Build

Install Rust through rustup, Make, Python 3, host C/C++ build tools and
`mipsel-none-elf-objdump` for the guest load-delay patcher.

```sh
git clone https://github.com/EBonura/nitroxide.git
cd nitroxide
make psoxide     # import the revisions in components.lock.json
make test        # physics tests, on the host, no console needed
make build       # PSX-EXE at game/target/mipsel-sony-psx/release/nitroxide.exe
make disc        # bootable disc, straight into the PS1 library
make run FRONTEND=/absolute/path/to/frontend # disc + standalone emulator
```

Discs always land in `~/Downloads/ps1 games/NitroXide/`, never in a folder
inside the repo. That is where the emulator's library and the burn tooling both
look, so a finished build is always one that can be launched or burned without
a copy step. Override with `make disc GAMES_DIR=...` or `GAME_NAME=...`.

Rust nightly is date-pinned in `rust-toolchain.toml`. The disc carries the six
music tracks as the XA files committed in [`game/assets/music`](game/assets/music/README.md)
(already encoded, 82 MB), so a fresh clone builds with music and needs no WAVs;
`make pack MUSIC=off` presses a silent disc. `make music-check MUSIC_DIR=...`
and `make music-refresh MUSIC_DIR=...` re-cook the files from the source WAVs,
which are not in the repository. Keep the BIN and CUE together.
Blender and the original car models are needed only for `make assets` and
`make assets-pavel`; normal builds use the committed cooked models.

## Sound effects

Every menu action and most of a match has a sound, quiet under the music and
silent when Sound is off. The four impacts are CC0 recordings, the crowd and the tyre
squeal are cut from recordings (credit below), and everything else is synthesised by
`tools/sfx-synth` (`make sfx`). See [the audio notes](game/assets/audio/README.md).

The sounds are in stereo: what comes from the arena is placed by distance and
bearing from the chase camera (two listeners in split screen), through the
SDK's `psx-sfx` mixer and its voice lanes (`game/src/audio.rs`).

Crowd sounds: Gregor Quendel, Free Crowd Cheering Sounds, CC BY 4.0,
https://opengameart.org/

Tyre squeal loop: Tom Haigh, CC BY 3.0,
https://opengameart.org/content/car-tire-squeal-skid-loop

All the credits (cars, music, crowd, tyre squeal, impacts and fonts) are also on
the title menu's Credits page.

## Music

Music: Mulula VGM Loops, Ambient Jungle Vol 1, Vol 2 and Ambient Jungle D&B
Vol 3, CC BY 4.0, https://mululavgmloops.itch.io/. The loops are chained into
six tracks and play as a shuffled playlist, over the menus and the match alike.

## Cars

Nine cars: Comet, Hatch and Sprinter, and six from Pavel 3D's pack: Razor,
Fury, Bolt, Brute, Dash and Cinder.

Cars: Pavel 3D, Low Poly City Vehicles Pack, CC BY 4.0,
https://pavel-3d.itch.io/low-poly-city-vehicles-pack. The six cars are that
pack's Sports Car 02, 03 and 01, Muscle Car 01, Coupe 01 and Hot Rod, recoloured
by the game and cooked down to the same face budget as the others. Only the
cooked meshes are in this repository, not the pack: see `make assets-pavel`
and [the car notes](#car-models).

## Controls

| Button | Action |
| --- | --- |
| R2 | accelerate |
| L2 | reverse / brake |
| X | jump / select (hold for a higher jump, press again in the air to flip) |
| Circle | boost |
| Triangle | toggle ball cam |
| L1 | powerslide on the ground, free air roll in the air |
| Square / R1 | air roll left / right |
| D-pad or left stick | steer on the ground, pitch and turn in the air |
| Start | begin / restart / pause |
| Select | the controls page, from the title or the pause menu |

The same list is in the game: press Select on the title screen or in the pause
menu. The left stick is analog, and in the air it is proportional: a quarter of
a stick turns the car a quarter as hard.

### In the air

Left stick up and down pitches the car (up is nose down), left and right turns
it. Hold L1 and the left and right of the stick roll the car instead of turning
it, and the up and down still pitch: all three at once. Square and R1 roll left
and right whatever the stick is doing. The car turns the way Rocket League's
does: the stick is a torque and the car keeps turning after you let go until
its own damping stops it, at up to 5.5 radians a second, so you catch a spin
as much as start one.

A second X press within about a second of the jump flips the car, in the
direction the stick is held. Up is a front flip, down a back flip, left or
right a side flip, and a diagonal gives a diagonal one. A flip kicks the car
along the ground in that direction (a sideways or backward flip at speed kicks
hardest) and spins it for about two thirds of a second. With the stick
centred the second jump is a plain double jump.

- Flip cancel: pull the stick against a flip while it turns, and the spin stops.
- Half flip: back flip, cancel it, then roll the car upright facing forward
  (Square or R1, or L1 with the stick).
- Stall: hold a roll one way (Square or R1) and the stick the other way, then
  flip. The car rolls and stays level instead of flipping.
- Pitch is locked out for a moment after a flip, as in Rocket League; yaw and
  roll are not.

## Layout

```
sim/                 integer physics: ball, car, arena, goals, scoring. no_std, host-tested.
game/                the PS1 binary: pad in, sim tick, GTE render, HUD.
game/assets/*.psxm   cooked car meshes, committed.
game/assets/disc/    cooked startup assets packed into WORLD.PAK.
assets-src/          editable texture sources, committed.
tools/cook-arena/    host tool: source imagery + patterns -> shared .psxt.
tools/cook-models/   host tool: glTF -> .psxm, via PSoXide's own importer.
```

The split is the point. `sim/` has no PlayStation in it at all, so `make test`
runs the whole match model on the host in milliseconds, and tuning how the
game *feels* never needs a console or an emulator in the loop. `game/` only
draws what the sim already decided.

Everything is i32 fixed point: no floats, no 64-bit math. The PS1 has no FPU,
and 64-bit division is slow enough to matter. Lengths are Rocket League's own
unreal units, stored in sub-units (`1 uu = 64 sub`), angles are Q0.12 (`4096` =
one turn, the GTE's own convention), and one tick is one 60 Hz frame.

The renderer runs everything through one transform: the camera builds a view
matrix, and each object loads `view * rotation` into the GTE with
`view * (object - camera)` as the translation.

The arena and the ball are procedural quads built here. The car is a cooked
mesh drawn through PSoXide's own `GouraudRenderPass`, which handles projection,
back-face culling, per-vertex GTE lighting and OT insertion. Rewriting that
badly was the alternative.

The arena's shape is one cross-section swept around the perimeter: floor edge,
a quarter turn up into the wall, the wall, a quarter turn over into the
ceiling. Sweeping one profile is what gives the side walls, the corner chamfers
and the end walls a consistent silhouette, and it is the difference between a
Rocket League arena and a crate.

Floor, walls, the honeycomb enclosure, and the square goal net share one
224x132 4bpp atlas. It is cooked to PSoXide's common `.psxt` format on the
host, packed as chunk 1 in `WORLD.PAK`, loaded synchronously at startup, and
discarded from RAM after upload to VRAM. Three CLUT rows give the grass, wall,
and translucent meshes independent palettes while sharing one texture page.
The team-end warmth remains a per-quad tint rather than a second texture. The
car is not textured; see below.

## Car models

Nine low-poly cars, cooked by `tools/cook-models`: three from glTF sources and
six from the Pavel 3D pack (CC BY 4.0, see [Cars](#cars)):

```sh
make assets      # re-cook from MODELS_DIR, only needed when the sources change
make assets-pavel PAVEL_DIR=/path/to/unzipped/pack/v1.2   # the six Pavel 3D cars
make textures    # re-cook game/assets/disc/chunk_1.psxt
```

Blender first applies every node transform, removes unused UV/color seams,
welds coincident corners, rebuilds the smoothing/hard-edge topology, and uses
quadric-collapse decimation on each object independently. Keeping the objects
separate is essential: global vertex clustering welded nearby wheels, fenders,
lamps, and bodywork into the same centroid and produced the stretched,
inside-out cars from the first cook. Leaving the imported face corners split
was just as bad: wheels received one normal per triangle and adjacent panels
were decimated apart, producing the bright raster cracks from the second cook.

There is one committed LOD: roughly 150 prepared faces per car, because two
cars share the arena's frame budget. There used to be a second 500-face cook
for the front end, back when the menu stood one car on a private plane of grass
with almost nothing else in the frame. The front end draws the real arena now,
and an arena plus a 500-face car does not fit two vblanks, so the detailed cook
is gone along with the showroom that justified it. The cook replaces
collapse-prone wheel shells before decimation with six-sided cylinders, and
carries a four-corner wheel map: tyre, rim, and hub vertices keep shared pivots
after the source nodes are merged into PSXM, letting the front pair steer and
all four wheels spin and move on the visual suspension.

There is one cook per car, not one per team. `paint.rs` gives the two team
variants identical geometry and identical colours everywhere except
`Role::Body` and `Role::BodyDark`, and those are exactly the two the select
screen repaints, so the second cook was a copy carrying the bytes that get
overwritten. Both cars on the pitch are now repainted from the one blob.

PSoXide's `psxed-gltf` imports each prepared GLB and bakes its material colours
into the PSXM face-colour table. The final cook splits vertices only where two
materials meet. `psx-asset` reads the result at runtime, zero-copy, straight
out of the binary.

The tool on top exists for two things the importer does not do:

* **Placement.** `psxed-gltf` normalises every mesh to its own centroid at unit
  extent. Correct for previewing a model, wrong for a game object that has to
  stand at a known size on a known spot, and scaling it back down through the
  `ActorTransform` would quantise visibly as the car turns. So the cooked
  vertex table is refitted in place: 163 uu for the cars and 180 for the
  trucks, with the origin on the ground between the wheels. That matches the
  widened 164 x 116 x 52 uu gameplay hitbox rather than leaving a full-size
  visual wrapped around the literal narrow Octane box.
* **Colour.** glTF `baseColorFactor` is linear. Written straight to 8 bits, the
  sedan's paint lands at 12/255 and the car renders as a black cutout. The tool
  assigns a display-referred team palette, then gives each material boundary
  distinct runtime vertices. Without that split, welded bodywork claimed the
  colour slots shared by windows and wheels and the whole model rendered as a
  single strange-looking blob.

The Pavel 3D cars come in as an FBX body plus four wheel objects, one flat swatch
per face picked from a shared palette PNG. `tools/convert_pavel_car.py` (Blender,
headless; the FBX is only parsed, never run) reads the swatch of every face and
splits the body by role (`tools/pavel-cars.txt` says which model becomes which
car), then the usual prepare and cook run. Wing mirrors and number plates are
left out, since the pack models them as slivers that decimate into twigs.

The select screen offers nine cars: the three intact passenger-car bodies Comet, Hatch, and
Sprinter, and the six from the pack. The two heavy-truck source meshes remain in the asset library for
future rework, but are not selectable: both are exposed commercial chassis
rather than complete rocket-car silhouettes. Each selectable car is one asset.

Both seats pick a car and a colour on the select screen, and the colours cannot
collide: stepping the paint skips whatever the other seat is wearing, so there
is nothing to reject at confirm time. The chosen colour follows its seat onto
the scoreboard block, the goal frame and the goal burst. A second pad answering
on port 2 is what makes a match two-player; there is no versus row to pick.

## Checking a render change without a console

```sh
make shot        # boots into a match, holds accelerate, dumps a frame to /tmp
```

`STEPS`, `PULSES`, and `SHOT` are overridable, e.g.
`make shot STEPS=60000000 SHOT=/tmp/early.ppm`.

## Performance

Physics and input run at 60 Hz. Single-player renders every vblank, targeting
60 fps; split screen holds a steady 30 fps, and the `pace-30` feature paces
single-player at 30 too. Each frame's ordering table is kicked to the GPU
without waiting for the walk, and the next frame builds into a second table
set, so the CPU prepares the next frame while the GPU rasterises this one. The
engine's queued scene contract measured worse for this game
(`draw::submit_detached` has the numbers). Text and other
immediate overlays wait until the submitted list has drained, preventing a HUD
write from serialising the whole CPU and GPU workload.

The title and select screens are the match renderer with a parked camera. They
used to be a second renderer: a private plane of grass with its own floor
tessellation, its own lighting falloff, a CPU-projected foreground lattice to
work around the GTE clamping H/Z at 2.0, a reserved 32-slot ordering-table band
for the car, and a detailed car LOD to fill it. All of that is gone. A staged
car is an ordinary sim car parked on the halfway line with its yaw written every
frame, so the front end gets the arena's floor, walls, lighting and goals for
free and there is one place a car can look wrong instead of two.

On the headless PSoXide cycle profiler, 600-tick steady-state runs measured:

| Scene | Visual frames | Misses | Cycles/frame | 1,127,078-cycle budget left |
| --- | ---: | ---: | ---: | ---: |
| Title screen, one car in the arena | 466/466 | 0 | 462,901 | 58.9% |
| Select screen, both cars in the arena | 456/456 | 0 | 591,918 | 47.5% |
| Gameplay, driving with animated wheels | 299/299 | 0 | 719,528 | 36.2% |

These are low-overhead `profile-frame` builds, which retain frame telemetry but
exclude the per-stage marker cost. Every run reports steady two-vblank cadence.

## Not done yet

* **The car is untextured.** The `.psxm` format has no UV or material table,
  and `psxed-gltf` says so: it parses glTF's texture coordinates and then drops
  them. Textured models need the other cooked format, `.psxmdl`, through
  `convert_rigid_model_path`, which carries per-part materials and UVs and is
  drawn by a different engine pass. That is the next real piece of work here,
  not a tweak. These particular models have no texture images anyway, only
  material colours, so it would mean authoring a texture atlas for them first.
* The floor-to-wall and wall-to-ceiling curves are square joins.

## Where the numbers come from

There is no open-source Rocket League, but there are three projects in its
orbit, and they turned out to be useful for completely different reasons.

**[RocketSim](https://github.com/ZealanL/RocketSim)** (MIT, C++) is a
clean-room reimplementation of RL's game logic on Bullet. It is the only one of
the three that is *accurate*, and it is where every constant in `sim/` came
from: the Octane hitbox (120.507 x 86.6994 x 38.6591 uu) and its wheel offsets
and radii, the 1410 uu/s throttle ceiling against 2300 with boost, the
steer-angle-versus-speed curve that makes an RL car turn at a near-constant
2 rad/s, 650 uu/s² of gravity, 0.6 ball restitution, 33.3 boost per second. Its
*code* is useless here (Bullet rigid bodies, doubles, and it wants collision
meshes dumped from a copy of the game you own) but its numbers are the spec.

**[Retro League GX](https://github.com/mholtkamp/retro-league)** (MIT, C++ on
the author's Octave engine) is the closest prior demake: GameCube, Wii and 3DS.
Two things came out of reading it. The useful one is how it splits the arena
into meshes, `Floor`, `Sides`, `BotCurve`, `TopCurve`, `Ceiling`, `GoalsBot`,
`GoalsTop`, which is the confirmation that an RL arena is not a box and that the
curved transitions are a thing you model explicitly. The other is a warning: it
runs full Bullet physics with mesh collision, which a GameCube can afford and a
33 MHz R3000 with 2 MB cannot. Its own arena is also a much longer, narrower
field than RL's, so its dimensions were not worth copying.

**[rocketleague-godot](https://github.com/lufemas/rocketleague-godot)** is a
sandbox: two scripts, a stock Godot `VehicleBody`, and 109 MB of downloaded
assets. Nothing transferable.

So the arena here is Standard Soccar's real footprint: side walls at ±4096,
back walls at ±5120, ceiling 2044, the four corners cut by 45° planes crossing
the axes at 8064, goal mouths 892.755 either side of centre and 642.775 tall.
The corner chamfers are the shape that makes it read as an arena rather than a
crate, and they are collided against, not just drawn. What is deliberately
*not* copied is RL's solver: this runs closed-form sphere-and-plane collisions,
so restitution and grip are feel-tuned rather than derived, and the floor-wall
and wall-ceiling curves are square joins for now.

The car you drive is an imported low-poly model, and the ball meets an oriented
box around it, 164 x 116 x 52 uu, aligned to the model the renderer draws. It
used to meet a radius-84 sphere around the car's centre while the constants and
this file both described a box, which meant a nose, a flank, a bonnet and a
corner were four views of one radial push. They are four different contacts
now.

The box carries no origin-to-hitbox offset: `Car::p` is a centre at a 28 uu
ride height, and RocketSim's Octane offset assumes its own convention, so
mixing the two would double-count it. Adopting the literal Octane box and its
offset together is a later decision about the whole physics/visual convention,
not a prerequisite for the contact being the right shape.

## Licence

GPL-2.0-or-later, the same as [PSoXide](https://github.com/EBonura/PSoXide),
whose SDK and engine this links against. See [LICENSE](LICENSE) for the text,
which is PSoXide's own copy verbatim.

The car models under `game/assets/` are cooked from a third-party low-poly
asset pack and carry whatever terms that pack came with, independently of the
code licence.

## PSoXide components

`make psoxide` imports the exact SDK, editor/engine and emulator-library revisions
in `components.lock.json` into ignored `.psoxide/`. Cortex projects are excluded.
For an existing checkout, preserve or remove its old `.psoxide/` cache once before
bootstrapping; imports refuse to overwrite unowned or edited files.
`make run FRONTEND=/path/to/PSoXide-emulator/target/release/frontend` uses the
standalone emulator. The demo disc may supply a bootstrapped editor checkout
explicitly with `PSOXIDE_FROM`.

## Recent changes

**0.6.0: goal replays, hold to skip, a victory screen.** A goal is replayed full screen in
slow motion, shot like a broadcast: a tracking shot beside the scorer, a cut on the touch
into a low angle at the goal, one of two styles chosen at random (never the same one three
goals running), then the explosion. Skipping a replay or a kickoff fly-over is a hold of X
with a filling ring. The match ends on a victory scene: the winning car turning under an
orbiting camera, a gold trophy, confetti, a sting, a stats table for both sides and REMATCH
or QUIT TO MENU (a calmer scene for a loss or a draw). Full notes are in `CHANGELOG.md`.

**0.5.1: boost flame, pad bursts, a calmer fly-over.** The boost has a layered
flickering flame and soft smoke in the car's colour, a pad taken bursts in the arena, the
view widens with speed and boost (in each split half too), and the kickoff fly-over is a
steady high swing around the ball that starts as the menu tiles part. Split screen draws
the pitch as finely as a single view, the title's orbit has no holes, the select ring goes
behind the car, HUD numbers are centred in their panels, and the arena texture no longer
reads into a stack buffer that left the boot no room to grow. Full notes are in
`CHANGELOG.md`.

**0.5.0: air control, new cars, music, kickoffs, overtime and moving menus.** A car in
the air steers with Rocket League's own torque and damping, and a flip is a real
angular impulse. There are nine cars (six new from Pavel 3D's pack), six tracks of
music inside the disc's data track, and stereo sound effects with a sound for every
menu action. Every kickoff opens with a camera fly-over and a big 3, 2, 1, GO!, a timed
match that ends level goes to golden-goal overtime, and the menus move: an orbiting
title, a sliding selection bar, a car turntable and a rebuilt pause menu. The match
shows an arrow to the ball, boost trails, speed lines, sparks and dust, the boost dial
is a bar of slanted cells, the loading screen is that bar filling, and Save Settings
no longer freezes the game. Full notes are in `CHANGELOG.md`.

**0.4.1: the ball rolls the same both ways.** A ball rolling toward one end
or side no longer stops sooner than the same ball rolling the other way, so
mirrored shots and rolls match. The attract demo has four new matches picked
for the fixed ball, and tyre tracks are wider, darker and last longer.

**0.4.0: attract demo, rounder arena, new ball.** Leave a menu alone for 30
seconds, or pick DEMO on the main menu, and four bot-against-bot matches play,
one view and split screen, until you press a button. The arena corners are
rounded and the end walls have a smaller floor ramp, like the standard arena,
the ball has a panelled design, goals and demolitions blow up in glowing
flashes and fireballs, and sliding wheels leave tyre tracks. Split screen holds
30 fps, the sky no longer shows through the pitch near the camera, and the game
asks before formatting a memory card that has no header.

**0.3.0: the arena and HUD overhaul.** The pitch now looks
like a stadium. Each half, its walls and the roof glow in that team's colour,
the goals are lit the same way, boost pads are pools of light with the orbs
floating over them again, and the goal boxes and arcs are marked on the pitch
as real geometry, so they stay straight up close and don't vanish far away.
There's a crowd behind the cage, and an airborne ball gets a hoop on the pitch
with a disc inside it that fills up as the ball comes down, so you can read a
high ball's landing at a glance. The scoreboard shrinks to a tab while you
play, the goal banner and results text are outlined, and split-screen ball
cam keeps your own car in frame. Under the hood the wheels and car vertices
are posed on the GTE, distant cars and the ball draw from low-detail meshes,
and the game links in a profile-placed I-cache order.

See the [changelog](CHANGELOG.md) for the full list and the published download
versions.

## PSoXide source components

`components.lock.json` pins the SDK, engine/editor and emulator libraries separately.
`make psoxide` verifies and materializes them into the ignored `.psoxide` directory.
The demo disc can pass `PSOXIDE_FROM` with a verified composite editor checkout.
Pass `FRONTEND=/path/to/PSoXide-emulator/target/release/frontend` to player helpers.
