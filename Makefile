# NitroXide -- rocket-car soccer for the PlayStation 1, built on the PSoXide
# Rust stack (hydrated into .psoxide/ from the pin in psoxide-pin/). A plain
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
FRONTEND  := $(PSOXIDE)/emu
TARGET    := mipsel-sony-psx
EXE       := $(GAME)/target/$(TARGET)/release/nitroxide.exe
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

.PHONY: help test assets textures bake build disc run shot clean psoxide

help:
	@echo "NitroXide targets:"
	@echo "  make test      - run the physics + cook tests on the host"
	@echo "  make assets    - bake + cook the car models from $(MODELS_DIR)"
	@echo "  make textures  - cook the arena atlas to shared .psxt"
	@echo "  make build     - build the PSX-EXE"
	@echo "  make disc      - build + pack the disc into '$(OUT)'"
	@echo "  make run       - disc + boot it in the PSoXide frontend"
	@echo "  make shot      - headless capture to $(SHOT) (no window)"
	@echo "  make clean     - cargo clean both crates"

# The physics is a plain host crate: it does not need a PlayStation, so this is
# the fast loop for anything about how the game feels.
test:
	cd $(SIM) && cargo test
	cd $(COOK) && cargo test
	cd $(ARENA_COOK) && cargo test

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

# Which PSoXide this is built against. Cargo owns the pin (psoxide-pin/), and
# psoxide-link copies the resolved checkout to .psoxide so the game's path
# dependencies and the linker script resolve. `make PSOXIDE_FROM=/path/to/tree`
# overrides it with a working tree, which is how the demo disc puts every
# program on one SDK.
PSOXIDE_REV ?= 3d274b7406ac74c3d382c8a36ec523a92fc4da27
PSOXIDE_FROM ?=
psoxide:
	@if [ -n "$(PSOXIDE_FROM)" ]; then \
		cargo run -q --manifest-path $(PSOXIDE_FROM)/tools/psoxide-link/Cargo.toml -- \
			--from "$(PSOXIDE_FROM)" --into $(PSOXIDE); \
	else \
		cargo run -q --manifest-path $(ROOT)/psoxide-pin/Cargo.toml -- $(PSOXIDE); \
	fi

build: psoxide
	cd $(GAME) && cargo build --release

# The game plays CD-DA tracks 2-5 when the disc carries them (game/src/music.rs)
# and stays silent when it does not. The four songs are the demo disc's menu
# tracks, used with the artist's permission (credit: Just Music - YouTube
# @Just-Music-Beats); the audio lives in the PSoXide-demo-disc repo, not here.
# Point CDDA_DIR at its audio/ to press a disc with music:
#   make disc CDDA_DIR=../psx-demo-disc/audio
# The order matches the demo disc's menu tracklist, which is what music.rs
# names on screen.
CDDA_DIR  ?=
CDDA_ARGS  = $(if $(CDDA_DIR),$(foreach t,knuckle-dust rusted-hammer chainsaw-heart night-crawler,--cdda-track "$(CDDA_DIR)/$(t).cdda"))

disc: build $(ARENA_PSXT)
	@mkdir -p "$(OUT)"
	cd $(MKISOPSX) && cargo run --release -- \
		--exe $(EXE) \
		--out "$(OUT)/$(GAME_NAME).bin" \
		--volume NITROXIDE \
		--world-pack-extra-dir "$(ARENA_DISC_DIR)" \
		$(CDDA_ARGS)
	@echo "DISC -> $(OUT)/$(GAME_NAME).cue"

run: disc
	cd $(FRONTEND) && cargo run -p frontend --release -- launch \
		--path "$(OUT)/$(GAME_NAME).cue"

# Boot straight into a match, hold accelerate, and dump the final frame. This
# is how render changes get checked without opening the GUI.
shot: psoxide $(ARENA_PSXT)
	cd $(GAME) && cargo build --release --features boot-play
	@mkdir -p "$(ROOT)/build/shot"
	cd $(MKISOPSX) && cargo run --release -- \
		--exe $(EXE) \
		--out "$(SHOT_DISC_BIN)" \
		--volume NITROXIDE \
		--world-pack-extra-dir "$(ARENA_DISC_DIR)"
	cd $(FRONTEND) && cargo run -p frontend --release -- launch \
		--path "$(SHOT_DISC_CUE)" --steps $(STEPS) --pad-pulses "$(PULSES)" --dump-hw $(SHOT)
	@echo "SHOT -> $(SHOT)"

clean:
	cd $(GAME) && cargo clean
	cd $(SIM) && cargo clean

# Local fallback for the canonical CI publisher in
# .github/workflows/itch-release.yml. CDDA_DIR must point at the demo-disc
# audio/ so the itch build ships with its songs; the credit in
# release/README.txt rides along. CI fetches the same four files through a
# repository-specific read-only key and verifies release/audio-manifest.sha256.
V_GAME = $(shell awk -F'"' '/^version/{print $$2; exit}' $(GAME)/Cargo.toml)
.PHONY: itch
itch:
	@test -n "$(CDDA_DIR)" || { echo "itch: set CDDA_DIR=/path/to/PSoXide-demo-disc/audio (the itch build ships with music)"; exit 1; }
	@command -v butler >/dev/null || { echo "itch: install butler and run 'butler login' first"; exit 1; }
	$(MAKE) disc CDDA_DIR="$(CDDA_DIR)"
	@rm -rf build/itch && mkdir -p build/itch
	cp "$(OUT)/$(GAME_NAME).bin" "$(OUT)/$(GAME_NAME).cue" release/README.txt build/itch/
	butler push --userversion "$(V_GAME)" build/itch bonnie-studios/nitroxide:psx
