# NitroXide -- rocket-car soccer for the PlayStation 1, built on the PSoXide
# Rust stack (hydrated into .psoxide/ from components.lock.json). A plain
# `cargo build --release` in game/ already produces a PSX-EXE (see
# game/.cargo/config.toml + game/build.rs); these targets add disc packing and
# the headless capture loop used to check the render without a console.

ROOT      := $(CURDIR)
GAME      := $(ROOT)/game
SIM       := $(ROOT)/sim
COOK      := $(ROOT)/tools/cook-models
ARENA_COOK := $(ROOT)/tools/cook-arena
PSOXIDE   := $(ROOT)/.psoxide
MKISOPSX  := $(PSOXIDE)/tools/mkisopsx
FRONTEND  ?= frontend
TARGET    := mipsel-sony-psx
EXE       := $(if $(CARGO_TARGET_DIR),$(CARGO_TARGET_DIR),$(GAME)/target)/$(TARGET)/release/nitroxide.exe
ARENA_SOURCE   := $(ROOT)/assets-src/grass.bmp
ARENA_DISC_DIR := $(GAME)/assets/disc
ARENA_PSXT     := $(ARENA_DISC_DIR)/chunk_1.psxt
SHOT_DISC_BIN  := $(ROOT)/build/shot/NitroXide.bin
SHOT_DISC_CUE  := $(ROOT)/build/shot/NitroXide.cue

# Every disc lands straight in the PS1 library, never in a build-local folder:
# that is where the console-side tooling and the emulator both look for it, so
# a finished build is always one that can be launched or burned.
GAMES_DIR ?= $(HOME)/Downloads/ps1 games
GAME_NAME ?= NitroXide
OUT       := $(GAMES_DIR)/$(GAME_NAME)

# Source glTF for the car models. The cooked .psxm blobs are committed, so a
# normal build never needs these; re-run `make assets` only when the source
# models or the cook settings change.
MODELS_DIR ?= $(HOME)/Downloads/export-2/glb
PREPARED      := $(ROOT)/build/prepared-cars
# Blender's decimate target per car, before the cooker splits vertices at
# material boundaries. That split roughly doubles the vertex count, so the
# budget has to be spent here rather than discovered later.
#
# Measured, two cars on screen, against the 1,128,960-cycle visual budget:
#
#   target   faces/car   worst frame   deadline misses
#      600     589-648     2,471,737       108 / 159
#      320     314-376     1,690,283        58 / 210
#      180     168-254     1,233,556         9 / 259
#      150     136-228     1,151,448         1 / 268
#
# A face costs about 324 cycles and a vertex about 100, fitted across those
# runs, so faces are what the target is really buying. 150 lands level with
# the old low-poly cars on cost while keeping the shape fix.
CAR_FACE_TARGET ?= 150
# The split-screen distance LOD (draw.rs CAR_LOD_DISTANCE). Below 150 the prep
# script scales every role cap by target/150 and uses four-sided wheels.
CAR_LOD_FACE_TARGET ?= 60
PREPARED_LOD  := $(ROOT)/build/prepared-cars-lod
LOD_OUT       := $(ROOT)/build/lod-out

# Headless capture: how many instructions to run, and what to hold on pad 1.
# 0x0200 = R2 (accelerate). See sdk/crates/psx-pad for the mask table.
STEPS  ?= 120000000
PULSES ?= 0x0200@30+4000
SHOT   ?= /tmp/nitroxide.ppm

.PHONY: help test assets assets-pavel textures bake compile build pack disc gate-disc gate-symbols run shot clean psoxide sfx recordings \
	pgo-collect pgo-choose pgo-order demo-check

help:
	@echo "NitroXide targets:"
	@echo "  make test      - run the physics + cook tests on the host"
	@echo "  make sfx       - re-render the synthesised sound effects"
	@echo "  make recordings SOURCES_DIR=x - cut the crowd and tyre sounds from the source recordings"
	@echo "  make assets    - bake + cook the car models from $(MODELS_DIR)"
	@echo "  make assets-pavel PAVEL_DIR=x - cook the six Pavel 3D cars (CC BY 4.0) from the unzipped pack"
	@echo "  make textures  - cook the arena atlas to shared .psxt"
	@echo "  make build     - build the PSX-EXE (PGO_VARIANT=off for no PGO; alias: compile)"
	@echo "  make disc      - build + pack the disc into '$(OUT)'"
	@echo "  make run       - disc + boot it in the PSoXide frontend"
	@echo "  make shot      - headless capture to $(SHOT) (no window)"
	@echo "  make demo-check FRONTEND=x  - check the attract demo for pitch holes and big LOD cars"
	@echo "  make pgo-collect FRONTEND=x - regenerate the committed PGO profile"
	@echo "  make pgo-order FRONTEND=x   - regenerate the committed I-cache layout profile"
	@echo "  make pgo-choose  FRONTEND=x - build and gate every PGO variant"
	@echo "  make clean     - cargo clean both crates"

# The physics is a plain host crate: it does not need a PlayStation, so this is
# the fast loop for anything about how the game feels.
test:
	cd $(SIM) && cargo test
	cd $(COOK) && cargo test
	cd $(ARENA_COOK) && cargo test
	cd $(ROOT)/tools/sfx-synth && cargo test --release

# Two LODs: the cooker writes both team variants of the small gameplay cars
# plus component/wheel sidecars, then a detailed blue-only copy for the front
# end. Output is committed, so this is a content step rather than a build step.
#
# Blender applies mirrored transforms correctly and decimates each object
# independently before the Rust cooker sees it. The old importer-wide vertex
# cluster welded overlapping wheels and bodywork together.
BLENDER ?= /Applications/Blender.app/Contents/MacOS/Blender
BAKED   := $(ROOT)/build/baked
CARS    := sedan hatchback hatchback2 truck truck_2

assets:
	@mkdir -p "$(PREPARED)"
	@for car in $(CARS); do \
		log="$(PREPARED)/$$car.log"; \
		if ! "$(BLENDER)" --background --python "$(ROOT)/tools/prepare_car_models.py" -- \
			"$(MODELS_DIR)/$$car.glb" "$(PREPARED)/$$car.glb" "$(CAR_FACE_TARGET)" \
			>"$$log" 2>&1; then \
			tail -40 "$$log"; \
			exit 1; \
		fi; \
		grep '^PREPARED' "$$log"; \
	done
	cd $(COOK) && cargo run --release -- "$(PREPARED)" "$(GAME)/assets" --components
	@# The split-screen distance LOD: the same three sources at 60 faces,
	@# cooked beside the gameplay set as <car>_lod.psxm / .psxw.
	@mkdir -p "$(PREPARED_LOD)" "$(LOD_OUT)"
	@for car in sedan hatchback hatchback2; do \
		log="$(PREPARED_LOD)/$$car.log"; \
		if ! "$(BLENDER)" --background --python "$(ROOT)/tools/prepare_car_models.py" -- \
			"$(MODELS_DIR)/$$car.glb" "$(PREPARED_LOD)/$$car.glb" "$(CAR_LOD_FACE_TARGET)" \
			>"$$log" 2>&1; then \
			tail -40 "$$log"; \
			exit 1; \
		fi; \
		grep '^PREPARED' "$$log"; \
	done
	cd $(COOK) && cargo run --release -- "$(PREPARED_LOD)" "$(LOD_OUT)" --components
	@for car in sedan hatchback hatchback2; do \
		cp "$(LOD_OUT)/$$car.psxm" "$(GAME)/assets/$${car}_lod.psxm"; \
		cp "$(LOD_OUT)/$$car.psxw" "$(GAME)/assets/$${car}_lod.psxw"; \
	done

# The six cars from Pavel 3D's "Low Poly City Vehicles Pack" (CC BY 4.0,
# https://pavel-3d.itch.io/low-poly-city-vehicles-pack, v1.2). The pack is not
# in this repository (only the cooked blobs are, as for every other car): unzip
# it anywhere and point PAVEL_DIR at the folder that holds "Individual Assets
# FBX" and "Material". tools/pavel-cars.txt lists which model becomes which car.
# tools/convert_pavel_car.py reads each FBX and the palette PNG in Blender
# (headless, nothing in the file is executed) and writes a role-named source GLB;
# the rest is the same prepare and cook as `assets`.
PAVEL_DIR   ?= $(HOME)/Downloads/low-poly-city-vehicles-pack/v1.2
PAVEL_LIST  := $(ROOT)/tools/pavel-cars.txt
PAVEL_BUILD := $(ROOT)/build/pavel
assets-pavel:
	@rm -rf "$(PAVEL_BUILD)" && mkdir -p "$(PAVEL_BUILD)/src" "$(PAVEL_BUILD)/p150" "$(PAVEL_BUILD)/p60" "$(PAVEL_BUILD)/gameplay" "$(PAVEL_BUILD)/lod"
	@while IFS='|' read -r car fbx; do \
		[ -n "$$car" ] || continue; \
		"$(BLENDER)" --background --factory-startup --python "$(ROOT)/tools/convert_pavel_car.py" -- \
			"$(PAVEL_DIR)/Individual Assets FBX/$$fbx" "$(PAVEL_DIR)/Material/city_vehicles_pallete.png" \
			"$(PAVEL_BUILD)/src/$$car.glb" >"$(PAVEL_BUILD)/$$car.log" 2>&1 || { tail -30 "$(PAVEL_BUILD)/$$car.log"; exit 1; }; \
		"$(BLENDER)" --background --factory-startup --python "$(ROOT)/tools/prepare_car_models.py" -- \
			"$(PAVEL_BUILD)/src/$$car.glb" "$(PAVEL_BUILD)/p150/$$car.glb" "$(CAR_FACE_TARGET)" >>"$(PAVEL_BUILD)/$$car.log" 2>&1 || { tail -30 "$(PAVEL_BUILD)/$$car.log"; exit 1; }; \
		"$(BLENDER)" --background --factory-startup --python "$(ROOT)/tools/prepare_car_models.py" -- \
			"$(PAVEL_BUILD)/src/$$car.glb" "$(PAVEL_BUILD)/p60/$$car.glb" "$(CAR_LOD_FACE_TARGET)" >>"$(PAVEL_BUILD)/$$car.log" 2>&1 || { tail -30 "$(PAVEL_BUILD)/$$car.log"; exit 1; }; \
		grep '^PREPARED' "$(PAVEL_BUILD)/$$car.log" | sed 's|$(PAVEL_BUILD)/||g'; \
	done < "$(PAVEL_LIST)"
	cd $(COOK) && cargo run --release -- "$(PAVEL_BUILD)/p150" "$(PAVEL_BUILD)/gameplay" --components
	cd $(COOK) && cargo run --release -- "$(PAVEL_BUILD)/p60" "$(PAVEL_BUILD)/lod" --components
	@while IFS='|' read -r car fbx; do \
		[ -n "$$car" ] || continue; \
		cp "$(PAVEL_BUILD)/gameplay/$$car.psxm" "$(PAVEL_BUILD)/gameplay/$$car.psxw" "$(PAVEL_BUILD)/gameplay/$$car.psxc" "$(GAME)/assets/"; \
		cp "$(PAVEL_BUILD)/lod/$$car.psxm" "$(GAME)/assets/$${car}_lod.psxm"; \
		cp "$(PAVEL_BUILD)/lod/$$car.psxw" "$(GAME)/assets/$${car}_lod.psxw"; \
	done < "$(PAVEL_LIST)"

# The arena atlas is the first shared-format runtime asset: source imagery and
# procedural patterns are cooked on the host, packed into WORLD.PAK, loaded at
# startup, and released from RAM after their one VRAM upload.
textures: psoxide $(ARENA_PSXT)

$(ARENA_PSXT): $(ARENA_SOURCE) $(ARENA_COOK)/Cargo.toml $(ARENA_COOK)/src/main.rs | psoxide
	@mkdir -p "$(ARENA_DISC_DIR)"
	cargo run --release --manifest-path "$(ARENA_COOK)/Cargo.toml" -- \
		"$(ARENA_SOURCE)" "$(ARENA_PSXT)"

bake:
	@mkdir -p "$(BAKED)"
	@for car in $(CARS); do \
		"$(BLENDER)" --background --python $(ROOT)/tools/bake_car_atlas.py -- \
			"$(MODELS_DIR)/$$car.glb" "$(BAKED)/$$car.png" \
			"$(BAKED)/$${car}_baked.glb" 128 300 2>&1 | grep '^BAKED'; \
	done

# Exact SDK, engine/cookers and emulator-library sources are locked separately.
# psoxide-components (the SDK's tools/psoxide-link) brings .psoxide to the lock;
# it is installed once per SDK revision, from the revision the lock pins, under
# target/. PSOXIDE_FROM remains an explicit demo-disc override.
SDK_REV    := $(shell sed -n '/"sdk": *{/,/"revision"/s/.*"revision": *"\([0-9a-f]*\)".*/\1/p' "$(ROOT)/components.lock.json")
# The hydrator itself is installed from LINK_REV, not from the pinned SDK: the
# psoxide-components of 5219afbe resolves the compound workspace offline and
# fails on a runner with a cold cargo registry (CI); the one at 7e97c246 primes
# the registry first. Both write the same .psoxide for the same lock.
LINK_REV   ?= 7e97c246f2afb2cf79eba4687be68aab78039054
COMPONENTS := $(ROOT)/target/psoxide-components/$(LINK_REV)
PSOXIDE_FROM ?=
psoxide:
	@if [ -n "$(PSOXIDE_FROM)" ]; then \
		cargo run -q --manifest-path "$(PSOXIDE_FROM)/tools/psoxide-link/Cargo.toml" -- \
			--from "$(PSOXIDE_FROM)" --into "$(PSOXIDE)"; \
	else \
		[ -x "$(COMPONENTS)/bin/psoxide-components" ] || cargo install -q --locked \
			--git https://github.com/EBonura/PSoXide --rev $(LINK_REV) --root "$(COMPONENTS)" psoxide-link; \
		"$(COMPONENTS)/bin/psoxide-components" --root "$(PSOXIDE)" --lock "$(ROOT)/components.lock.json"; \
	fi

# LLVM's MIPS delay-slot filler searches backwards only by default, which left
# a nop in 18.8M of the 22.0M delay slots a kickoff-and-drive replay executed.
# The SDK owns the switches that also search the successor block and past
# calls (PSX_DELAY_SLOT_FLAGS in the hydrated tools/sdk-examples.mk), so every
# guest builds with one set; they are read from there rather than copied.
# Every search can leave a load in a slot whose consumer runs inside the load
# delay, so the link is always followed by the SDK's hazard-patch, which reroutes
# those branches through psx-rt's HAZARD_TRAMPOLINES and rescans (46 of the
# default 96 words in the shipping PGO build; psx-rt's hazard-trampolines-256
# feature is the room to grow into). `--config` appends to game/.cargo/config.toml; an exported
# RUSTFLAGS would replace it.
comma := ,
PSX_DELAY_SLOT_FLAGS = $(filter -C%,$(subst ", ,$(subst $(comma), ,$(shell sed -n 's/^PSX_DELAY_SLOT_FLAGS :*= *//p' "$(PSOXIDE)/tools/sdk-examples.mk"))))
DELAY_SLOT_CONFIG = $(if $(PSX_DELAY_SLOT_FLAGS),--config 'target.$(TARGET).rustflags=[$(foreach f,$(PSX_DELAY_SLOT_FLAGS),"$(f)",)]',$(error PSX_DELAY_SLOT_FLAGS not found in $(PSOXIDE)/tools/sdk-examples.mk))

# Profile-guided optimisation through the SDK's shared driver
# ($(PSOXIDE)/tools/psoxide-pgo/README.md). pgo/nitroxide.prof is committed and
# portable (its names carry no checkout-path or feature hashes), so every build
# applies it with no emulator: `make build`, `make disc`, CI and the demo disc.
# PGO_VARIANT is the winner of `make pgo-choose`; PGO_VARIANT=off builds the
# plain image. Either way the driver runs the hazard patcher, the scanner and
# the stack guard (psoxide-hazard's stack-guard, which proves every scratchpad stack
# call tree in draw.rs fits its region) with the link map, stops on a failure,
# and the exe lands at $(EXE) as before.
#
# The host tools (psoxide-pgo, mkisopsx) build in .psoxide's Cargo workspace
# against the Cargo.lock imported from the editor pin. --locked keeps a host
# build from rewriting that imported file, which the next `make psoxide` would
# refuse as an edit.
FEATURES    ?=
GAME_CARGO   = build --release$(if $(strip $(FEATURES)), --features "$(FEATURES)") $(DELAY_SLOT_CONFIG)
PGO          = cargo run -q --release --locked --manifest-path "$(PSOXIDE)/tools/psoxide-pgo/Cargo.toml" --
PGO_PROFILE  = $(ROOT)/pgo/nitroxide.prof
# The I-cache layout profile `+order` places functions from (see the
# psoxide-pgo README, "order"): per-word counts and direct calls over the
# train tape's gameplay polls. It binds by portable name and code hash, and
# any change to the code the gameplay runs makes it stale (`apply` stops
# below 98% bound): regenerate it with
# `make pgo-order FRONTEND=x` and commit it with the change.
PGO_LAYOUT   = $(ROOT)/pgo/nitroxide.layout
# `off` since 60 fps: hot=500+profi wins on average work per frame (the
# number `pgo-choose` ranks by) but loses on the heavy frames with both cars
# on screen, which are the ones that miss a vblank. Judge a variant by the
# share of frames at 60, not by the average. `+order` because this game is
# at the mercy of its link order: the commit that added the stands measured
# 84.8% of the train tape's frames at 60 linked plain and 95.3% placed.
# Dev feature builds link plain: a boot shortcut such as boot-goal changes
# code the gameplay runs, the layout then binds only part of itself and
# `apply` refuses it (boot-play binds 100%, boot-goal 75%).
PGO_VARIANT ?= $(if $(strip $(FEATURES)),off,off+order)

compile: psoxide
	PSOXIDE="$(PSOXIDE)" $(PGO) apply --crate "$(GAME)" --profile "$(PGO_PROFILE)" \
		--layout "$(PGO_LAYOUT)" --variant "$(PGO_VARIANT)" -- $(GAME_CARGO)
	@echo "EXE -> $(EXE)"

build: compile

# The game plays six tracks of XA-ADPCM music (game/src/music.rs). The cooked
# files are committed in game/assets/music/ (MUSIC1.XA, MUSIC2.XA, the pack
# manifest music.json and their digests in SHA256SUMS), so a fresh clone and CI
# press the music straight onto the disc; no WAV is needed to build. The tracks
# are Mulula VGM Loops' Ambient Jungle Vol 1, Vol 2 and Ambient Jungle D&B Vol 3
# (CC BY 4.0, https://mululavgmloops.itch.io/; credits in
# game/assets/music/README.md), chained into six loudness-matched 37.8 kHz
# stereo WAVs that are NOT in this repository.
#
# Only a maintainer who changes the songs needs the WAVs. MUSIC_DIR is the
# folder holding them; sources.sha256 refuses any other file:
#   make music-check   MUSIC_DIR=/path/to/music   cook them with the SDK's
#                      `psx-audio-cook xa-pack` into build/music and fail unless
#                      the result is byte-for-byte the committed files
#   make music-refresh MUSIC_DIR=/path/to/music   same, then replace the
#                      committed files (and game/src/music_map.rs, the table
#                      the game compiles in) with the new cook
# `make pack MUSIC=off` presses a silent disc (the attract-demo frame check).
MUSIC       ?= on
MUSIC_DIR   ?=
SHA256      := $(shell command -v sha256sum >/dev/null 2>&1 && echo sha256sum || echo "shasum -a 256")
MUSIC_SONGS := jungle1-a jungle1-b jungle2-a jungle2-b dnb-a dnb-b
MUSIC_WAVS   = $(foreach s,$(MUSIC_SONGS),$(MUSIC_DIR)/$(s).wav)
MUSIC_OUT   := $(ROOT)/build/music
MUSIC_SRC   := $(GAME)/assets/music
MUSIC_XA    := $(MUSIC_SRC)/MUSIC1.XA $(MUSIC_SRC)/MUSIC2.XA
MUSIC_MAP   := $(GAME)/src/music_map.rs
AUDIO_COOK  := $(ROOT)/tools/audio-cook
XA_ARGS      = $(if $(filter off,$(MUSIC)),,$(foreach f,$(MUSIC_XA),--xa-file "$(f)"))

# The synthesised sound effects (game/assets/audio, everything but the four
# recorded impacts) come from tools/sfx-synth. Output is committed and
# deterministic, so this is a content step rather than a build step; re-run it
# when a sound changes and read the printed levels before touching the play
# volumes in game/src/audio.rs.
SFX_SYNTH := $(ROOT)/tools/sfx-synth
.PHONY: sfx
sfx: | psoxide
	cargo run -q --release --locked --manifest-path "$(SFX_SYNTH)/Cargo.toml" -- "$(GAME)/assets/audio"

# The crowd and the tyre squeal are cut from recordings that are not in this
# repository: Gregor Quendel's "Free Crowd Cheering Sounds" (MP3, CC BY 4.0) and
# Tom Haigh's "Car tire squeal skid loop" (WAV, CC BY 3.0), both via
# OpenGameArt. Point SOURCES_DIR at the folder that holds quendel-crowd/ and
# haigh-tire-loop/; tools/sfx-synth/recordings.manifest says which file, which
# seconds and what level, and refuses a file that is not the recorded one.
# Output is committed, so this is a content step too.
SOURCES_DIR ?=
.PHONY: recordings
recordings: | psoxide
	@test -n "$(SOURCES_DIR)" || { echo "recordings: set SOURCES_DIR to the folder holding quendel-crowd/ and haigh-tire-loop/"; exit 1; }
	cargo run -q --release --locked --manifest-path "$(SFX_SYNTH)/Cargo.toml" -- import \
		"$(SFX_SYNTH)/recordings.manifest" "$(SOURCES_DIR)" "$(GAME)/assets/audio"

.PHONY: music-check music-refresh
music-check music-refresh: | psoxide
	@test -n "$(MUSIC_DIR)" || { echo "$@: set MUSIC_DIR to the folder holding $(addsuffix .wav,$(MUSIC_SONGS))"; exit 1; }
	@(cd "$(MUSIC_DIR)" && $(SHA256) -c "$(MUSIC_SRC)/sources.sha256" >/dev/null) || { \
		echo "$@: the WAVs in $(MUSIC_DIR) are not the ones listed in game/assets/music/sources.sha256"; exit 1; }
	@rm -rf "$(MUSIC_OUT)" && mkdir -p "$(MUSIC_OUT)"
	cargo run -q --release --locked --manifest-path "$(AUDIO_COOK)/Cargo.toml" -- \
		xa-pack "$(MUSIC_OUT)" MUSIC $(foreach w,$(MUSIC_WAVS),"$(w)") \
		--manifest "$(MUSIC_OUT)/music.json" --rust "$(MUSIC_OUT)/music_map.rs"
	@if [ "$@" = music-check ]; then \
		for f in MUSIC1.XA MUSIC2.XA music.json; do \
			cmp "$(MUSIC_OUT)/$$f" "$(MUSIC_SRC)/$$f" || { echo "music-check: $$f differs from the committed file"; exit 1; }; \
		done; \
		cmp "$(MUSIC_OUT)/music_map.rs" "$(MUSIC_MAP)" || { echo "music-check: music_map.rs differs"; exit 1; }; \
		echo "music-check: the cook is byte-identical to the committed music"; \
	else \
		cp "$(MUSIC_OUT)/MUSIC1.XA" "$(MUSIC_OUT)/MUSIC2.XA" "$(MUSIC_OUT)/music.json" "$(MUSIC_SRC)/"; \
		cp "$(MUSIC_OUT)/music_map.rs" "$(MUSIC_MAP)"; \
		(cd "$(MUSIC_SRC)" && $(SHA256) MUSIC1.XA MUSIC2.XA music.json > SHA256SUMS); \
		echo "music-refresh: wrote game/assets/music and game/src/music_map.rs; commit them"; \
	fi

# `make music-verify` checks the committed files against their digests (CI).
.PHONY: music-verify
music-verify:
	cd "$(MUSIC_SRC)" && $(SHA256) -c SHA256SUMS

# `make pack PACK_EXE=x PACK_OUT=y.bin` wraps any exe in the game's disc image.
PACK_EXE ?= $(EXE)
PACK_OUT ?= $(OUT)/$(GAME_NAME).bin
pack: $(ARENA_PSXT) $(if $(filter off,$(MUSIC)),,music-verify)
	@mkdir -p "$$(dirname "$(PACK_OUT)")"
	cd "$(MKISOPSX)" && cargo run -q --release --locked -- \
		--exe "$(PACK_EXE)" \
		--out "$(PACK_OUT)" \
		--volume NITROXIDE \
		--world-pack-extra-dir "$(ARENA_DISC_DIR)" \
		$(XA_ARGS)

disc: build
	@$(MAKE) --no-print-directory pack PACK_EXE="$(EXE)" PACK_OUT="$(OUT)/$(GAME_NAME).bin"
	@echo "DISC -> $(OUT)/$(GAME_NAME).cue"

# The journey reads a named static in the exact image it tests. Locate its
# versioned initializer in that PS-X EXE, rather than trusting a link map from
# another build or fixing a layout-dependent address in the journey.
GATE_DISC_OUT ?= $(ROOT)/build/gate/NitroXide.bin
gate-symbols: build
	@set -eu; mkdir -p "$(ROOT)/build"; \
		rustc --edition=2021 "$(ROOT)/tools/gate-symbols.rs" -o "$(ROOT)/build/gate-symbols-tool"; \
		"$(ROOT)/build/gate-symbols-tool" "$(EXE)" > "$(ROOT)/build/gate-symbols.txt"; \
		echo "GATE SYMBOLS -> $(ROOT)/build/gate-symbols.txt"

gate-disc: gate-symbols
	@$(MAKE) --no-print-directory pack PACK_EXE="$(EXE)" PACK_OUT="$(GATE_DISC_OUT)"
	@echo "GATE DISC -> $(patsubst %.bin,%.cue,$(GATE_DISC_OUT))"

# Regenerating the profile and picking the variant need the emulator:
#   make pgo-collect FRONTEND=/path/to/frontend  (after gameplay code changes or an SDK repin)
#   make pgo-choose  FRONTEND=/path/to/frontend  (then commit the winner as PGO_VARIANT)
# The committed profile and the variant were measured on a disc with the
# songs (the committed game/assets/music), as the itch and demo-disc
# builds ship. pgo/train.pxtape presses through the menus to kickoff; polls 396..1200
# are gameplay. There is no second route yet, so the gate has no holdout tape.
# PGO_LAUNCH_ARGS adds frontend arguments (--launch-arg X per word).
TRAIN_TAPE   = $(ROOT)/pgo/train.pxtape
TRAIN_POLLS  = 396..1200
# The layout is also trained on two longer routes (poll-bound tapes of a played
# match, so they replay the same on any build): driving up the walls and the
# ramps, and the ball-cam views over the whole stands that are the heaviest
# frames. A layout trained on the menu-to-kickoff tape alone put the heavy
# frames a percent over the deadline.
WALL_TAPE    = $(ROOT)/pgo/wall.pxtape
WALL_POLLS   = 1500..3300
HEAVY_TAPE   = $(ROOT)/pgo/heavy.pxtape
HEAVY_POLLS  = 3150..3600
PGO_LAUNCH_ARGS ?=
PGO_PACK = '$(MAKE) --no-print-directory -C "$(ROOT)" pack PACK_EXE="$$PSOXIDE_PGO_EXE" PACK_OUT="$$PSOXIDE_PGO_DISC"'
pgo-collect: psoxide
	PSOXIDE="$(PSOXIDE)" $(PGO) collect --crate "$(GAME)" --frontend "$(FRONTEND)" \
		--tape "$(TRAIN_TAPE)" --polls $(TRAIN_POLLS) \
		--pack $(PGO_PACK) --launch-arg --embedded-playtest $(PGO_LAUNCH_ARGS) \
		--out "$(PGO_PROFILE)" -- $(GAME_CARGO)
	@$(MAKE) --no-print-directory compile

# The layout profile for PGO_VARIANT's `+order`, collected on the variant
# without it.
pgo-order: psoxide
	PSOXIDE="$(PSOXIDE)" $(PGO) order --crate "$(GAME)" --frontend "$(FRONTEND)" \
		--tape "$(TRAIN_TAPE)" --polls $(TRAIN_POLLS) \
		--tape "$(WALL_TAPE)" --polls $(WALL_POLLS) \
		--tape "$(HEAVY_TAPE)" --polls $(HEAVY_POLLS) --pack $(PGO_PACK) \
		--launch-arg --embedded-playtest $(PGO_LAUNCH_ARGS) --profile "$(PGO_PROFILE)" \
		--variant "$(patsubst %+order,%,$(PGO_VARIANT))" --out "$(PGO_LAYOUT)" -- $(GAME_CARGO)

# NitroXide renders every second vblank and waits out the rest, so `choose`
# ranks the variants by the work cycles `measure` counts outside the wait loops.
# The last build is the last variant, so this rebuilds the shipping exe at the end.
PGO_VARIANTS = off default hot=500 hot=500+profi accurate+nopgso+hot=1000 accurate+nopgso+hot=1500
PGO_MEASURE  = "$$PSOXIDE_PGO" measure --frontend "$(FRONTEND)" --image "$$PSOXIDE_PGO_IMAGE" \
	--launch-arg --embedded-playtest $(PGO_LAUNCH_ARGS)
pgo-choose: psoxide
	PSOXIDE="$(PSOXIDE)" $(PGO) choose --crate "$(GAME)" --profile "$(PGO_PROFILE)" \
		$(foreach v,$(PGO_VARIANTS),--variant $(v)) --pack $(PGO_PACK) \
		--gate '$(PGO_MEASURE) --tape "$(TRAIN_TAPE)" --polls $(TRAIN_POLLS) --name train' \
		-- $(GAME_CARGO)
	@$(MAKE) --no-print-directory compile

run: disc
	"$(FRONTEND)" launch \
		--path "$(OUT)/$(GAME_NAME).cue"

# Boot straight into a match, hold accelerate, and dump the final frame. This
# is how render changes get checked without opening the GUI.
# The plain image goes through the driver too: the renderer runs phases on a
# scratchpad stack, and only the driver has the link map the stack guard needs.
shot: psoxide $(ARENA_PSXT)
	@$(MAKE) --no-print-directory compile FEATURES=boot-play PGO_VARIANT=off
	@mkdir -p "$(ROOT)/build/shot"
	cd $(MKISOPSX) && cargo run --release -- \
		--exe $(EXE) \
		--out "$(SHOT_DISC_BIN)" \
		--volume NITROXIDE \
		--world-pack-extra-dir "$(ARENA_DISC_DIR)"
	"$(FRONTEND)" launch \
		--path "$(SHOT_DISC_CUE)" --steps $(STEPS) --pad-pulses "$(PULSES)" --dump-hw $(SHOT)
	@echo "SHOT -> $(SHOT)"

# Headless check of the whole attract demo (both split-screen matches) for the
# two split-screen regressions of 2026-10-03: sky showing through holes in the
# pitch and the walls near the camera, and cars drawn big from the 60-face LOD.
# The diag-keys image paints the sky and LOD cars in key colours and changes
# nothing else; tools/frame-check reads the screenshots and fails on either.
# About 800 screenshots (~190 MB) land in build/demo-check/shots.
DEMO_CHECK     := $(ROOT)/build/demo-check
DEMO_CHECK_STEPS ?= 3000000000
demo-check: psoxide $(ARENA_PSXT)
	@$(MAKE) --no-print-directory compile FEATURES=diag-keys
	@$(MAKE) --no-print-directory pack PACK_EXE="$(EXE)" PACK_OUT="$(DEMO_CHECK)/NitroXide.bin" MUSIC=off
	@rm -rf "$(DEMO_CHECK)/shots"
	"$(FRONTEND)" launch --path "$(DEMO_CHECK)/NitroXide.cue" --steps $(DEMO_CHECK_STEPS) \
		--route-screenshot-dir "$(DEMO_CHECK)/shots" --route-screenshot-interval 10
	cargo run -q --release --manifest-path "$(ROOT)/tools/frame-check/Cargo.toml" -- "$(DEMO_CHECK)/shots"

clean:
	cd $(GAME) && cargo clean
	cd $(SIM) && cargo clean

# Local fallback for the canonical CI publisher in
# .github/workflows/itch-release.yml. The itch build ships with the committed
# music (game/assets/music); the credit in release/README.txt rides along.
V_GAME = $(shell awk -F'"' '/^version/{print $$2; exit}' $(GAME)/Cargo.toml)
.PHONY: itch
itch:
	@command -v butler >/dev/null || { echo "itch: install butler and run 'butler login' first"; exit 1; }
	$(MAKE) disc
	@rm -rf build/itch && mkdir -p build/itch
	cp "$(OUT)/$(GAME_NAME).bin" "$(OUT)/$(GAME_NAME).cue" release/README.txt build/itch/
	butler push --userversion "$(V_GAME)" build/itch bonnie-studios/nitroxide:psx
