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

.PHONY: help test assets textures bake compile build pack disc run shot clean psoxide \
	pgo-collect pgo-choose pgo-order

help:
	@echo "NitroXide targets:"
	@echo "  make test      - run the physics + cook tests on the host"
	@echo "  make assets    - bake + cook the car models from $(MODELS_DIR)"
	@echo "  make textures  - cook the arena atlas to shared .psxt"
	@echo "  make build     - build the PSX-EXE (PGO_VARIANT=off for no PGO; alias: compile)"
	@echo "  make disc      - build + pack the disc into '$(OUT)'"
	@echo "  make run       - disc + boot it in the PSoXide frontend"
	@echo "  make shot      - headless capture to $(SHOT) (no window)"
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

# Exact SDK, engine/cookers and emulator-library sources are locked separately.
# PSOXIDE_FROM remains an explicit demo-disc override.
PSOXIDE_FROM ?=
psoxide:
	@if [ -n "$(PSOXIDE_FROM)" ]; then \
		cargo run -q --manifest-path "$(PSOXIDE_FROM)/tools/psoxide-link/Cargo.toml" -- \
			--from "$(PSOXIDE_FROM)" --into "$(PSOXIDE)"; \
	else \
		python3 "$(ROOT)/tools/bootstrap-components.py" --root "$(PSOXIDE)" --lock "$(ROOT)/components.lock.json"; \
	fi

# LLVM's MIPS delay-slot filler searches backwards only by default, which left
# a nop in 18.8M of the 22.0M delay slots a kickoff-and-drive replay executed.
# The SDK owns the switches that also search the successor block and past
# calls (PSX_DELAY_SLOT_FLAGS in the hydrated tools/sdk-examples.mk), so every
# guest builds with one set; they are read from there rather than copied.
# Every search can leave a load in a slot whose consumer runs inside the load
# delay, so the link is always followed by hazard_patch.py, which reroutes
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
# the stack guard (tools/stack_guard.py, which proves every scratchpad stack
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
# train tape's gameplay polls. It binds by portable name and code hash, so
# a feature build binds it as well, but any change to the code the gameplay
# runs makes it stale (`apply` stops below 98% bound): regenerate it with
# `make pgo-order FRONTEND=x CDDA_DIR=...` and commit it with the change.
PGO_LAYOUT   = $(ROOT)/pgo/nitroxide.layout
# `off` since 60 fps: hot=500+profi wins on average work per frame (the
# number `pgo-choose` ranks by) but loses on the heavy frames with both cars
# on screen, which are the ones that miss a vblank. Judge a variant by the
# share of frames at 60, not by the average. `+order` because this game is
# at the mercy of its link order: the commit that added the stands measured
# 84.8% of the train tape's frames at 60 linked plain and 95.3% placed.
PGO_VARIANT ?= off+order

compile: psoxide
	PSOXIDE="$(PSOXIDE)" $(PGO) apply --crate "$(GAME)" --profile "$(PGO_PROFILE)" \
		--layout "$(PGO_LAYOUT)" --variant "$(PGO_VARIANT)" -- $(GAME_CARGO)
	@echo "EXE -> $(EXE)"

build: compile

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

# `make pack PACK_EXE=x PACK_OUT=y.bin` wraps any exe in the game's disc image.
PACK_EXE ?= $(EXE)
PACK_OUT ?= $(OUT)/$(GAME_NAME).bin
pack: $(ARENA_PSXT)
	@mkdir -p "$$(dirname "$(PACK_OUT)")"
	cd "$(MKISOPSX)" && cargo run -q --release --locked -- \
		--exe "$(PACK_EXE)" \
		--out "$(PACK_OUT)" \
		--volume NITROXIDE \
		--world-pack-extra-dir "$(ARENA_DISC_DIR)" \
		$(CDDA_ARGS)

disc: build
	@$(MAKE) --no-print-directory pack PACK_EXE="$(EXE)" PACK_OUT="$(OUT)/$(GAME_NAME).bin"
	@echo "DISC -> $(OUT)/$(GAME_NAME).cue"

# Regenerating the profile and picking the variant need the emulator:
#   make pgo-collect FRONTEND=/path/to/frontend CDDA_DIR=...  (after gameplay code changes or an SDK repin)
#   make pgo-choose  FRONTEND=/path/to/frontend CDDA_DIR=...  (then commit the winner as PGO_VARIANT)
# The committed profile and the variant were measured on a disc with the four
# songs (CDDA_DIR = the demo disc's audio/), as the itch and demo-disc builds
# ship. pgo/train.pxtape presses through the menus to kickoff; polls 396..1200
# are gameplay. There is no second route yet, so the gate has no holdout tape.
# PGO_LAUNCH_ARGS adds frontend arguments (--launch-arg X per word).
TRAIN_TAPE   = $(ROOT)/pgo/train.pxtape
TRAIN_POLLS  = 396..1200
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
		--tape "$(TRAIN_TAPE)" --polls $(TRAIN_POLLS) --pack $(PGO_PACK) \
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
