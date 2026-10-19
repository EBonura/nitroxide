# Parity ledger: the air game

What `sim/` does with a car in the air, against Rocket League's own numbers, taken
from RocketSim's `RLConst.h` and `Car.cpp` and then checked by running RocketSim
with the same inputs. This covers air control, the dodge and air roll. The rest of
the car (driving, suspension, the hitbox, the ball) is not in this ledger.

Units: one tick is 1/60 s, lengths are uu stored in sub-units (`1 uu = 64`), so
`uu/s -> sub/tick` is `x 16/15`. The car's spin, `Car::w`, is radians a tick scaled
by `2^20` in the world frame.

## What is not the same, and cannot be

| | Rocket League | here |
| --- | --- | --- |
| Physics rate | 120 Hz | 60 Hz |
| Arithmetic | float | i32 fixed point, no 64-bit |
| Solver | Bullet rigid body | one spin vector and a basis, no inertia tensor |

The inertia cancels out of RL's air control (`invInertia.inverse() * torque`), which is
why one spin vector is enough. What the rest costs is in the measurements below.

## Constants

| Quantity | RL | here |
| --- | --- | --- |
| Air control torque (pitch, yaw, roll) | 130, 95, 400 | `AIR_TORQUE_*` 3630, 2653, 11170 (12.46, 9.11, 38.35 rad/s^2) |
| Air control damping | 30, 20, 50 | `AIR_DAMP_*` 3142, 2094, 5236 per 65536 a tick (2.88, 1.92, 4.79 /s) |
| Torque scale | `2pi/65536 * 1000` | folded into the two rows above |
| Pitch and yaw damping | scaled by `1 - |input|` | same, roll's is not |
| Max angular speed | 5.5 rad/s | `AIR_SPIN_MAX` 96119 (on the whole vector) |
| Flip torque (roll, pitch) | 260, 224 rad/s^2, not torque-scaled | `FLIP_TORQUE_*` 75730, 65245 |
| Flip torque time | 0.65 s | 39 ticks, plus half a tick on the press tick |
| Flip pitch lock | 0.65 s + 0.3 s | `FLIP_PITCH_LOCK` 57 ticks |
| Flip kick | 500 uu/s | `DODGE_IMPULSE` 533 |
| Kick scale at 2300 uu/s | fwd 1.0, side 1.9, back 2.5 | 4096, 7782, 10240 (per 4096) |
| Backward flip extra | 16/15 | 4369 per 4096 |
| Kick speed ratio | `|forward speed| / 2300` | `CAR_BOOST_SPEED` 2453 |
| Dodge deadzone | 0.5 (sum of the axes) | `DODGE_DEADZONE` 64 of 128 |
| Flip z damp | 0.35 per 120 Hz step, 0.15 s to 0.65 s | `FLIP_Z_DAMP` 1731 per 4096 a tick |
| Second jump window | 1.25 s | `DODGE_WINDOW` 75 |
| Speed ceiling | 2300 uu/s on the whole velocity | `CAR_BOOST_SPEED` 2453 |

Two things in RocketSim that are easy to read wrong, both measured rather than assumed:

* The kick's speed ratio is against 2300, the boost ceiling (`CAR_MAX_SPEED` in `RLConst.h`),
  not the 1410 throttle top speed. A side flip at 1400 uu/s adds 773.9 uu/s, which is
  `500 * (1 + 0.9 * 1400 / 2300)`; over 1410 it would be 1000.
* A flip's torque is not scaled by `CAR_TORQUE_SCALE`. 224 and 260 are rad/s^2 outright, so
  the spin is at the 5.5 cap after three 120 Hz steps, and the torque keeps adding to it
  before each step's clamp, so a car in a flip turns at 7.4 rad/s, not 5.5. The sim steps
  the spin twice a tick for that reason: each half adds half the tick's torque, carries on at
  that speed, then clamps.

## How it was measured

`oracle/trace.cpp` (kept outside this repository: it is C++ against a RocketSim build) runs
RocketSim's `THE_VOID` mode with the stock Octane and prints the car's basis, velocity and
spin every 1/60 s. The inputs are the ones `air_tests.rs` scripts, and the sim's trace
(`cargo test air_tests::trace -- --ignored --nocapture`, `NX_SCEN`, `NX_SPEED`) prints in the
same format so the two can be diffed. The fixtures in `air_tests.rs` are the oracle's numbers.

## Results

Direction of the nose and roof against RL, worst over the run (a run is 1.5 s):

| Scenario | Worst error |
| --- | --- |
| Held pitch / yaw / roll, full and half stick | 1.4 deg |
| Release and coast on each axis | 1.6 deg |
| All three sticks held (spin capped as a whole) | 5.3 deg |
| Front flip | 6.1 deg |
| Back flip | 6.2 deg |
| Side flip | 7.3 deg (roll) |
| Diagonal flip | 6.7 deg |
| Flip cancelled at once | 1.9 deg |
| Flip cancelled 0.1 s in | 4.7 deg |
| Back flip then forward cancel and roll (half flip) | 3.3 deg |
| Stall | 1.5 deg |
| Side flip, then pitch pulled from 0.75 s (the pitch lock) | 8.0 deg |

Spin rate, rad/s, at 0.25 / 0.5 / 1.0 s of a held stick (RL, then here within 0.1):
pitch 3.12 / 5.50 / 5.50, yaw 2.28 / 4.55 / 5.50, roll 5.50 / 5.50 / 5.50, half pitch
1.32 / 2.23 / 3.31, half yaw 1.02 / 1.82 / 2.94, half roll 2.82 / 3.65 / 3.97.

Kick (forward, sideways) in uu/s from a car doing 0, 700 and 1400 uu/s: every entry in the
table in `air_tests.rs` is within 6 uu/s, most within 2.

Bounds on the error:

* The flip's torque starts on a 60 Hz tick boundary where RL's starts on a 120 Hz step, so a
  flip can be up to half a step (4 ms) early or late, which is up to 3 degrees of attitude.
  The pitch lock ends on a tick too, which is the 8 degrees in the lock row: the car starts
  pitching one tick from when RL's does.
* RL starts the flip's z damping one step after the float comparison says it should, because
  its flip time is a sum of 1/120 s steps held in a float. The sim opens the window at 10 ticks
  where 0.15 s says 9. The first 0.3 s of vertical speed is within 47 uu/s of RL's at the
  worst sample.
* The sim holds the car's equilibrium fall speed under a flip at 18.8 uu/s where RL's is
  15.5, because two damp-then-gravity steps become one.

## Not modelled

* A landing is wheels-down at once. RL lands on whichever surface the car meets first, so a
  car can land on its roof or its nose and be driven from there, and a flip a moment before
  touchdown (the wavedash) carries the car on the ground. The kick of a late flip is kept here
  (it is a horizontal impulse), but not the turning.
* Flip resets. They need the wheels to meet the ball, and this car has no wheel contact.
* `CAR_AUTOFLIP_*` and `CAR_AUTOROLL_*`, the assists that turn a car over on landing.
* RL's per-axis inputs are a float from -1 to 1. Here they are `-128..=128`, so a step of
  stick is 0.8 percent.
