# NitroXide on the scene core

How NitroXide maps onto the SDK's scene system: a resident core in a fixed window at the top of RAM, a
handoff block above it, and `SCENES.TBL` at member LBA 22. The row format, kinds and link rules are the
core lane's CORE-ABI.md (campaign `scene-system-1123`, day 0, 11-23); everything here is provisional
with it. CORE-ABI is written for the 10,240 B cap (window 0x801FD700); this branch links for the
provisional 8,192 B window, **0x801FDF00..0x801FFF00**, so every SP and bound below uses 0x801FDF00.
Line numbers are at commit dfdaa75 of this repo (the scene-core commit changes one line in place); SDK
lines are at the pin 6bfe478ff0e0 (`components.lock.json`). Sizes are from the dfdaa75 ship link map
unless they say otherwise.

Rule this follows (Manny, 11-23): scene boundaries only where the player already waits today, and every
screen reachable in play stays resident in the playing scene. NitroXide is one scene. It is one engine
`Scene` with a `Phase` enum (Title, Select, Play, Results, Demo, `main.rs:137-150`), every phase draws
the same arena with the same cars, and after boot nothing reads the disc: the XA music owns the drive
for good (`music.rs:14-18`, `main.rs:2573`). The song must never stop, so the core must never touch the
drive after boot either.

## What the player sees

| What the player sees | In the program | Resident in |
|---|---|---|
| Boot splash "Built with PSoXide", 150 frames, skip after 8 | `boot_splash` (`main.rs:2540-2545`) returns the engine's `BootSplash`; the engine draws it (SDK `engine/crates/psx-engine/src/app.rs:521-547`) | scene 0 today; the core's boot scene under `scene-core` (step 3) |
| Loading bar (10 cells, at least 40 vblanks, white flash) | `loader::Loader` in `NitroXide::init` (`main.rs:2547-2603`, `loader.rs`) | scene 0 |
| Title (MATCH, PRACTICE, DEMO, SETTINGS, CREDITS) with the controls, credits and settings panels | `Phase::Title`, `draw::render_menu` (`main.rs:3489-3503`) | scene 0 |
| Select (car, paint, arena look, rule; one or two seats) | `Phase::Select` (`main.rs:3509-3540`) | scene 0 |
| Match: kickoff fly-over, countdown, play, goal banner, goal replay | `Phase::Play`, `draw::render` / `render_split`, `replay::RING` (`main.rs:3544-3570`) | scene 0 |
| Pause menu (Resume, Settings, Quit, Swap) | inside Play (`main.rs:618-625`) | scene 0 |
| Results (victory scene with the trophy) | `Phase::Results`, `begin_victory` / `end_victory` (`victory.rs:150-172`) | scene 0 |
| Attract demo after 60 s idle | `Phase::Demo`, `start_demo` (`main.rs:165`, 888-898) | scene 0 |
| Phase changes | a 30-tick tile sweep, state swapped at tick 15 (`sweep.rs:17-19`, `main.rs:1496-1516`); no load | scene 0 |
| Settings save | `CARD_JOB` pumped one card transaction per update (`main.rs:1212-1300`, 2837) | scene 0 |
| Exit | none: `App::run` returns `!`; Quit goes back to Title (`main.rs:1502-1506`) | |

## Scene table rows

| id | kind | What it is | Fields that matter | return scene |
|---|---|---|---|---|
| 0 | 0 Program (boot scene: header 0x10 = 0) | `PSX.EXE`, the whole game | `t_addr` 0x80010000, `t_size` 1,406,976 (0x157800), file 688 sectors with the header, entry PC `__core_boot` once linked with psoxide-core.ld (`_start` today), SP 0x801FDF00, name `NITROXIDE` | 0xFFFF (none: Manny 11-23) |
| 1 | 2 Data **(provisional kind)** | `WORLD.PAK`, member LBA 1024, 50 sectors, one chunk (id 1, the arena atlas, 98,588 B) | LBA and sectors | n/a |
| 2 | 2 Data **(provisional)** | `MUSIC1.XA`, LBA 1074, 20,656 sectors | LBA and sectors; would let `Music::locate` skip the root read if the core ever hands rows to a program (no call for that in CORE-ABI today) | n/a |
| 3 | 2 Data **(provisional)** | `MUSIC2.XA`, LBA 21730, 14,444 sectors | as row 2 | n/a |

Rows 2 and 3 are optional. On the demo disc NitroXide would be one Program row in the host's table with
its own `lba_offset`; it already chain-loads there through `disc_base` today.

## Shared code versus per scene

One row: renderer (`nitroxide::draw`, 220,408 B of text), sim, audio and bank, menus, replay and victory
are shared by every phase, all static, no heap. The only in-RAM swap is the trophy borrowing a car LOD
slot (`draw.rs:3694-3699`).

## The relink (step 1, done)

`game/build.rs` writes `RAM_SIZE_OVERRIDE = 0x1FE000;` into `$OUT_DIR/ram-size.ld` and passes it as a
`-T` file before psoxide.ld (psoxide.ld:16-21); the map records it (map line 2). One constant in
`game/build.rs`, marked provisional. `make build` (ship, `off+order` with the searched layout) and
`make gate-disc` link through build.rs. NitroXide keeps the default 32 KiB stack reserve.

Measured on the ship build (`make build` then `make pack`, the two commands of `[[build]] ship`)
against the installed e7c9419 build (`pgo-search-1123/keep/e7c9419ca19f-inst/`, map sha256 4ff4b7b0... =
the `[maps] ship` baseline). Free RAM from the shared `psoxide-size ram` (tools-hc-894d16439831):

| | before (e7c9419) | after (dfdaa75) |
|---|---|---|
| header SP (`STACK_INIT`) | 0x801FFF00 | 0x801FDF00 |
| `__heap_end` (`STACK_INIT - STACK_RESERVE`) | 0x801F7F00 | 0x801F5F00 |
| `__bss_end`, highest byte the image owns | 0x801DADC4 | 0x801DADC4 |
| .text / .data / .bss | 594,836 / 811,512 / 473,124 | the same |
| free RAM | 119,100 | 110,908 (-8,192) |
| stack reserve | 0x801F7F00..0x801FFF00 | 0x801F5F00..0x801FDF00 |
| PGO layout `pgo/nitroxide.layout` | bound 135 of 135 | bound 135 of 135, 100.0% of the profile's instructions, 0 changed, 0 missing |
| exe bytes that differ | | 33 of 1,409,024: header SP (1) and the 32-byte gate receipt digest at 0x408. No code byte (no heap, so no code refers to `STACK_INIT`) |

Nothing is linked at or above 0x801FDF00 (no map line with a VMA at or above 0x801F0000). The window
fits with 110,908 B still free.

**The stack still starts above the window when the BIOS boots the disc.** psx-rt's `_start` does not
set `$sp`; it trusts the loader (`psx-rt/src/lib.rs:164-168`), and the BIOS boot path takes `STACK =
801FFFF0` from the `SYSTEM.CNF` the SDK writes for every disc (`psx-iso` `iso9660.rs:692`). Measured on
Quake's ship-boot run (`entry_sp = 0x801ffff0`, quake-psx SCENES.md); not measured here. Until
`__core_boot` sets `$sp = 0x801FDF00` (CORE-ABI 4.3 step 1), the relink moves only the link-time budget.

## Conflicts with CORE-ABI.md

- **Window size.** CORE-ABI says 0x801FD700 (10,240 B); this branch uses 8,192 B. 10,240 B leaves
  108,860 B free.
- **Disc layout.** mkisopsx writes `PSX.EXE` at LBA 22; the table wants 22 and the exe at 23. SDK branch
  lane/disc-scenes-1123 adds `mkisopsx --scenes`; the 6bfe pin does not have it.
- **The core must leave the drive alone after boot.** From tick 90 to power-off the XA player streams
  without a gap (`main.rs:2826-2828`, `music.rs:218-261`). Nothing in CORE-ABI makes the core touch the
  drive on its own, and NitroXide never calls `psx_core_switch`, so this holds; it must stay true (no
  periodic lid check during play, PLAN 3.9's lid rule is "before each read").
- **The shuffle seed.** `Music::locate` seeds the playlist from how long the disc took to spin up and
  answer the root read (`music.rs:138-141`, `boot_seed`). Under a core that has already read the table,
  the drive is spun up and answers fast every boot, so the seed varies less between boots. Not a speed
  issue; a behaviour change to check.
- **Boot order and the settings record.** The card is read in `boot_splash` before the splash so the
  splash shows the saved brightness (`main.rs:2540-2545`). With the splash in the core's boot scene the
  brightness comes from the handoff block's settings record (0x30, provisional) or the splash uses the
  default. The `scene-core` feature keeps the card read in place.
- **IRQ mask.** Both boot reads save and restore the engine's `I_MASK` around `SectorReader` (it leaves
  VBlank-only, `assets.rs:20-24`, `music.rs:123`, 137); CORE-ABI 3.1 says the core puts the mask back
  itself, so those saves become redundant, not wrong.
- **Polled only, no CD IRQ handler.** True today; `psx_engine::cd_drive` / `psx_cdstream` are linked but
  never installed (audit section 4).

## What the core replaces (step 3)

Measured on the dfdaa75 ship map (sizes equal the e7c9419 map: no code moved):

| What | Symbols (file:line) | Bytes |
|---|---|---|
| Boot splash | `psx_splash::show` (inside `App::run_with_flow`) 2,640, `logo::expand` 664, `draw_line` closure 260 (text); `logo::CODED` 2,746, `PALETTE` 32 (data). Asked for by `boot_splash` at `main.rs:2544` | 3,564 text + 2,778 data = **6,342** |
| SDK polled sector reader | `SectorReader::prepare` 1,300, `read_sector` 1,036, `send_command_waiting` 724, `start_read` 540, `ack_unexpected` 448, `stop` 232; `ReaderPause::wait_seek_end` 668, `status` 588, `wait_completion` 464 | **6,000** text |
| psx-io command layer | `try_command_until_complete_inner` 552, `Cd::command_within` 520, `read_response_fifo` 168, `begin_polled_command` 160, `finish_failed_polled_command` 112, `drain_response_limited` 84, `ack_all_and_reset_parameters` 48 | **1,644** text (shared with the XA `Drive` impl, which reaches the drive through `psx_core_cd` under the core) |
| | **Total that leaves once the core owns the drive** | **about 7,640 B plus the 6,342 B splash** |

Also dead in this image and not NitroXide's code: `psx_cdstream` (5,136 text, 532 data) and
`psx_engine::cd_drive`, linked through `GameApp` and never installed. Removing them is the SDK's
gameplay-only `App::run` (the same edit as PSXcel's SCENES.md TODO), not a NitroXide change.

Stays in NitroXide: the XA player (`psx_io::cd::xa::Engine`, `Player` and the `Drive` impl, about
2,600 B text), `Music` with the shuffle and now-playing plate, `assets::load_arena_texture` (1,604, its
read becomes one `psx_core_read`), and the root lookup in `Music::locate` (its two sector reads become
`psx_core_read`s).

`Cd::steal()` sites, all three replaced: `assets.rs:26` (atlas reader), `music.rs:125` (root lookup
reader), `music.rs:109` (`Player::new` in `Music::new`, which runs in `NitroXide::new` before
`App::run`; it keeps its token but reaches the drive through `psx_core_cd`).

**Done now, behind the off-by-default `scene-core` feature (commit 8fd7e14):** `boot_splash` keeps the
settings read and returns `None`, so the engine shows no splash. The edit sits on the existing line, so
no panic location moves. The default `make build` of 8fd7e14 equals dfdaa75's exe byte for byte (`cmp`).
**Finding:** `make build FEATURES=scene-core` is refused by psoxide-pgo: the ship variant `off+order` links
the searched layout, whose recorded placement lists 9 sections this link no longer has (the splash's,
`FontAtlas::draw_text`, a `Debug` impl and others), and the driver stops. So a scene-core ship build needs
a re-searched layout (600 rounds by the install rule) or `PGO_VARIANT=off`. Its size is not measured: the
`PGO_VARIANT=off` measurement was stopped unfinished to deliver this report.

**TODO when `psx-core` exists:**

1. `game/build.rs`: `-T psoxide-core.ld` after the ram-size file; `game/Cargo.toml`: `psx-core`
   (feature `link`) under `scene-core`.
2. `assets.rs:23-47`: `load_chunk`'s reader becomes the pack header sector plus one
   `psx_core_read(1024 + 1, 49, boot_buffer, None)` straight into `TEX_QUADS_SETS` (the chunk is
   contiguous; whole sectors fit the 157,696 B buffer).
3. `music.rs:118-145`: the PVD and root reads through `psx_core_read`; `Player` through `psx_core_cd`.
4. Splash: `scene-core` on; brightness from the handoff block's settings record.
5. No exit row (CORE-ABI return scene 0xFFFF).

## Load speed (Manny 11-23: every load the same speed or faster)

No code or data that is resident today becomes a disc read. After boot NitroXide reads nothing, so every
transition the player sees (title to select to match to results, quit, demo) stays a sweep with no read.

| Transition | Disc read today | Under the core | Measured today | Slower? What keeps it equal |
|---|---|---|---|---|
| Boot to first frame (splash) | BIOS loads `PSX.EXE`, 687 payload sectors | the same plus the core image stored after `.data`, about 4 sectors at 8,192 B **(estimate)**, and `__core_boot`'s copy and word FNV over 8 KB | not measured (no boot-time figure; journeys anchor on game state, `tests/journey.toml:11-13`) | **yes, slightly (estimate):** about 4 sectors, about 27 ms at double speed. With `scene-core` on and a separate boot-scene program showing the splash, add that program's whole load. Keeping the splash in NitroXide while it is the disc's first program keeps this to the 4 sectors |
| Loading bar: arena atlas | pack header (1) and chunk 1 (49 sectors) in one session | one `psx_core_read` of 49 sectors (plus the header) | not measured; the bar is held for at least 40 vblanks regardless (`loader.rs:21`) | no: same sectors, one call |
| Loading bar: music lookup | PVD and root directory | the same via `psx_core_read` | not measured | no |
| Title, Select, Match, Results, Demo, pause, settings | none | none | | no |
| Music from tick 90 on | XA, never interrupted | the same | | no, provided the core never takes the drive (conflicts above) |

## Tests, bars and goldens that move

| Check | Moves? |
|---|---|
| `[[size]] ship` (baseline free 119,100) | yes, fails: free 110,908 (-8,192); text, data, bss unchanged. Re-bless with the window as the reason |
| `[maps] ship` (baseline 4ff4b7b0...) | yes: effcd49a... |
| PGO layout `pgo/nitroxide.layout` (600-round search) and `nitroxide-gate.layout` | not from the relink: the ship layout binds 135 of 135 with 0 changed, so no re-search is needed for this step. A core call ABI that moves code would need the 600-round re-search (the install rule) |
| speed `train`, `wall`, `heavy` (poll windows from boot: 396..1200, 1500..3300, 3150..3600) | not expected from the relink: no code address moves. Not run. A boot scene in front shifts every poll-indexed window; `train-state` (anchored on `CAMERA_TICK`) survives that |
| journeys (41) and goldens (`splash.png` and the rest) | not expected from the relink. Not run. `splash.png` and the boot-timed steps (`journey.toml:11-17`, `credits.toml:9-30`) move when the splash moves to a boot scene |
| `[cross]` smoke (presents within 3,000 frames) | not expected; a boot scene adds its time |
| `[clean]` (bin, cue, exe) | the exe and bin change (exe sha256 5e287a0d..., bin 32a4500d...) |
| install facts | yes: `NitroXide.provenance.json`, `install-stamp.json`, `speed-facts.json`, the library map name |
