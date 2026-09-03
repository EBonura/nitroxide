// SPDX-License-Identifier: GPL-2.0-or-later
//! 3D renderer: arena, ball, Octane, chase camera.
//!
//! Coordinates are Rocket League's unreal units, straight from the sim, which
//! the GTE takes as `i16`. The sim is Y-up; the GTE draws with +Y down, so the
//! flip happens once, folded into the object transform.
//!
//! Everything goes through one transform path: the camera builds a view matrix
//! `V`, and each object loads `V * R_object` into the GTE's rotation registers
//! with `V * (P_object - P_camera)` as the translation. Meshes are therefore
//! plain constant tables in object space, and the car's wheels can steer and
//! spin for the cost of one 3x3 multiply each.
//!
//! The arena and the ball are procedural quads built here. The car is a cooked
//! `.psxm` mesh drawn through the engine's own projection helpers, which do
//! the parts worth not rewriting: GTE projection, back-face culling, per-vertex
//! lighting off the loaded light rig, packet build, and deterministic OT
//! insertion. Both feed one ordering table, quads first and the mesh appended
//! with `OtFrame::resume`.
//!
//! The arena samples one cooked `.psxt` atlas loaded from `WORLD.PAK` at boot.
//! The PS1 has no Z-buffer, so a closed shape still relies on the depth sort
//! plus its own front faces landing nearer than its back ones.

use nitroxide_sim as sim;
use psx_asset::{Mesh, Texture};
use psx_engine::{ActorTransform, DepthRange, OtFrame, PrimitiveArena, Vec3World};
use psx_gpu::material::{TextureMaterial, TexturedGouraudPacketMaterial};
use psx_gpu::ot::OrderingTable;
use psx_gpu::prim::{QuadGouraud, QuadTexturedGouraud, TriGouraud};
use psx_gte::lighting::{project_lit, project_lit_triangle, Light, LightRig, ProjectedLit};
use psx_gte::math::{Mat3I16, Vec3I16, Vec3I32};
use psx_gte::scene;
use psx_math::int32::isqrt_i32;
use psx_math::sincos::{atan2_q12, cos_q12, sin_q12};
#[cfg(feature = "profile")]
use psx_telemetry as telemetry;
use psx_vram::{upload_bytes, Clut, TexDepth, Tpage, VramRect};
use sim::{Sim, FP};

/// Ordering-table depth. The arena is ~13000 uu corner to corner, so this is
/// about 25 uu per slot: fine enough that the car never fights its own wheels.
pub const OT_DEPTH: usize = 512;

/// Bracket a render sub-stage for the headless profiler. Compiles to nothing
/// without the `profile` feature, so a shipping build carries no markers.
macro_rules! staged {
    ($id:expr, $body:block) => {{
        #[cfg(feature = "profile")]
        telemetry::emit::stage_begin($id);
        let out = $body;
        #[cfg(feature = "profile")]
        telemetry::emit::stage_end($id);
        out
    }};
}

pub const SCREEN_W: i16 = 320;
pub const SCREEN_H: i16 = 240;
/// Projection plane distance: about a 63 degree horizontal field of view.
/// Narrower than Rocket League's ~100, deliberately. RL renders this arena at
/// 1080p, where a ball 4600 uu away is still tens of pixels across; at 320x240
/// the same shot is three pixels. Trading peripheral vision for reach is what
/// keeps the ball legible at kickoff.
const PROJ_H: u16 = 260;

/// One player's slice of the back buffer, stacked top and bottom, the way
/// Rocket League splits a two-player game.
///
/// The projection plane stays at [`PROJ_H`] in a half-height view, so a split
/// player keeps the full horizontal field of view, which is the axis car
/// soccer is played on, and gets half the vertical one. It also pays for the
/// second pass: the cull frustum flattens with the viewport, so the roof and
/// the far floor are rejected before they reach the GTE. The side-by-side
/// layout this replaces cut the horizontal field to 33 degrees a player.
#[derive(Copy, Clone)]
pub struct Viewport {
    /// Left edge in display pixels.
    pub x: i16,
    /// Top edge in display pixels.
    pub y: i16,
    /// Width in display pixels.
    pub w: i16,
    /// Height in display pixels.
    pub h: i16,
}

impl Viewport {
    /// The whole screen: one player.
    pub const FULL: Viewport = Viewport {
        x: 0,
        y: 0,
        w: SCREEN_W,
        h: SCREEN_H,
    };
    /// Player one's half of a top-and-bottom split.
    pub const TOP: Viewport = Viewport {
        x: 0,
        y: 0,
        w: SCREEN_W,
        h: SCREEN_H / 2,
    };
    /// Player two's half.
    pub const BOTTOM: Viewport = Viewport {
        x: 0,
        y: SCREEN_H / 2,
        w: SCREEN_W,
        h: SCREEN_H / 2,
    };
}

/// How far in from a view's right edge the boost dial's centre sits. Exported
/// so the HUD's boost readout lands in the middle of the dial it belongs to.
pub const BOOST_GAUGE_INSET: i16 = 52;

/// The seam between two split views, and its half-width in pixels. Slot 0 is
/// the front of the table and nothing else in a match uses it.
const SPLIT_SEAM_SLOT: usize = 0;
const SPLIT_SEAM_W: i16 = 2;

/// Centre of the boost dial, and its readout, for one view.
pub const fn boost_gauge_x(vp: Viewport) -> i16 {
    vp.x + vp.w - BOOST_GAUGE_INSET
}

/// How far up from a view's bottom edge the boost dial's centre sits.
pub const BOOST_GAUGE_RISE: i16 = 50;

/// Vertical centre of the boost dial for one view.
pub const fn boost_gauge_y(vp: Viewport) -> i16 {
    vp.y + vp.h - BOOST_GAUGE_RISE
}

/// Slack around the viewport that the screen-space rejection tests allow, so a
/// quad straddling an edge is kept rather than popped. The cull frustum uses
/// the same figure, which is what keeps the two tests from disagreeing.
const EDGE_SLACK: i16 = 80;

/// Horizontal accept band for projected vertices, and the cull frustum's half
/// width over the projection plane. Both follow the viewport being drawn.
///
/// Statics rather than parameters because the rejection test sits at the
/// bottom of every emit path in this file; threading a viewport through all of
/// them would touch thirty call sites to say one thing. Set by [`enter_view`],
/// which is the only way to start drawing a view.
static mut VIEW_MIN_X: i16 = -EDGE_SLACK;
static mut VIEW_MAX_X: i16 = SCREEN_W + EDGE_SLACK;
static mut VIEW_HALF_W: i32 = (SCREEN_W / 2 + EDGE_SLACK) as i32;
static mut VIEW_MIN_Y: i16 = -EDGE_SLACK;
static mut VIEW_MAX_Y: i16 = SCREEN_H + EDGE_SLACK;
static mut VIEW_HALF_H: i32 = (SCREEN_H / 2 + EDGE_SLACK) as i32;
/// True while drawing a half-width viewport. Detail follows the viewport: a
/// 160-pixel view cannot show the near tessellation a 320-pixel one can, and
/// it is drawn twice, so the finest band is paid for twice to be seen half as
/// well.
static mut VIEW_SPLIT: bool = false;

/// Is the pass being drawn a half-width one?
#[inline]
fn split_view() -> bool {
    unsafe { VIEW_SPLIT }
}

/// Is a projected vertex close enough to the current viewport to keep?
#[inline]
fn on_view(sx: i16, sy: i16) -> bool {
    let (min_x, max_x, min_y, max_y) = unsafe { (VIEW_MIN_X, VIEW_MAX_X, VIEW_MIN_Y, VIEW_MAX_Y) };
    sx >= min_x && sx < max_x && sy >= min_y && sy < max_y
}

/// Does a projected quad's bounding box overlap the current view?
///
/// Testing only whether one corner is inside is not conservative. A roof
/// patch close to the camera can surround the whole screen while all four of
/// its corners are outside, which made a visible part of the enclosure vanish.
#[inline]
fn quad_overlaps_view(sp: &[(i16, i16); 4]) -> bool {
    let (mut min_x, mut max_x) = (i16::MAX, i16::MIN);
    let (mut min_y, mut max_y) = (i16::MAX, i16::MIN);
    for &(x, y) in sp {
        min_x = min_x.min(x);
        max_x = max_x.max(x);
        min_y = min_y.min(y);
        max_y = max_y.max(y);
    }
    let (view_min_x, view_max_x, view_min_y, view_max_y) =
        unsafe { (VIEW_MIN_X, VIEW_MAX_X, VIEW_MIN_Y, VIEW_MAX_Y) };
    max_x >= view_min_x && min_x < view_max_x && max_y >= view_min_y && min_y < view_max_y
}

/// Point the GTE and the GPU at one viewport, and set the bounds the rejection
/// tests use. `buffer_y` is where the engine's current back buffer starts in
/// VRAM, which is what turns a display-space viewport into a VRAM scissor.
///
/// The projection centre moves with the viewport, so vertices come out in
/// whole-screen coordinates and the framebuffer's own drawing offset still
/// puts them in the right buffer. Only the scissor has to know about VRAM.
fn enter_view(vp: Viewport, buffer_y: u16) {
    unsafe {
        VIEW_MIN_X = vp.x - EDGE_SLACK;
        VIEW_MAX_X = vp.x + vp.w + EDGE_SLACK;
        VIEW_HALF_W = (vp.w / 2 + EDGE_SLACK) as i32;
        VIEW_MIN_Y = vp.y - EDGE_SLACK;
        VIEW_MAX_Y = vp.y + vp.h + EDGE_SLACK;
        VIEW_HALF_H = (vp.h / 2 + EDGE_SLACK) as i32;
        VIEW_SPLIT = vp.w < SCREEN_W || vp.h < SCREEN_H;
    }
    scene::set_screen_offset(
        ((vp.x + vp.w / 2) as i32) << 16,
        ((vp.y + vp.h / 2) as i32) << 16,
    );
    psx_gpu::set_draw_area(
        vp.x as u16,
        buffer_y + vp.y as u16,
        (vp.x + vp.w) as u16 - 1,
        buffer_y + (vp.y + vp.h) as u16 - 1,
    );
}

/// Hand the whole back buffer back, so the overlay pass can write across the
/// seam. The runner draws the HUD after `render` returns and before the swap
/// that would otherwise reset the scissor, so leaving a half-width area
/// behind clips every string on the right of the screen.
fn leave_view(buffer_y: u16) {
    enter_view(Viewport::FULL, buffer_y);
}

// The default chase camera follows the car. Ball cam keeps the ball as its
// primary subject, but shifts far enough toward the car to keep both visible
// in the PS1 renderer's deliberately narrow field of view.
#[cfg(not(feature = "boot-wheels"))]
const CAM_DIST: i32 = 800;
#[cfg(feature = "boot-wheels")]
const CAM_DIST: i32 = 430;
#[cfg(not(feature = "boot-wheels"))]
const CAM_HEIGHT: i32 = 330;
#[cfg(feature = "boot-wheels")]
const CAM_HEIGHT: i32 = 190;
#[cfg(not(feature = "boot-wheels"))]
const CAM_MIN_FLAT_DIST: i32 = 650;
#[cfg(feature = "boot-wheels")]
const CAM_MIN_FLAT_DIST: i32 = 400;
/// Horizontal camera clearance from a vertical driving surface. The normal
/// component grows from `CAM_HEIGHT` on the pitch to this distance on a wall,
/// keeping the eye inside the arena without a discrete left/right relocation.
#[cfg(not(feature = "boot-wheels"))]
const CAM_WALL_BOOM: i32 = 720;
#[cfg(feature = "boot-wheels")]
const CAM_WALL_BOOM: i32 = CAM_HEIGHT;
/// How much of the chase distance transfers into the vertical axis when the
/// car drives straight up a wall. Shorter than the pitch boom so the view is
/// diagonal rather than directly under the rear bumper.
#[cfg(not(feature = "boot-wheels"))]
const CAM_WALL_TRAIL: i32 = 500;
#[cfg(feature = "boot-wheels")]
const CAM_WALL_TRAIL: i32 = 0;
const CAM_MIN_SEP: i32 = 300;
const CAM_PITCH_MIN: i32 = -260; // Q12, negative = looking up at a ball overhead
const CAM_WALL_PITCH_MIN: i32 = -620;
const CAM_PITCH_MAX: i32 = 700;
const CAM_FALLBACK_AIM: i32 = 900;
/// Maximum horizontal angle between the view centre and the car in ball cam,
/// in Q12 turns (~29.0 degrees). At the wall-clamped kickoff the camera has to
/// sit to one side of the car; aiming dead-centre at the ball would otherwise
/// put the car outside the 63-degree horizontal field of view.
const CAM_BALL_CAR_YAW: i32 = 330;
/// Vertical equivalent of `CAM_BALL_CAR_YAW` (~22.9 degrees). The car is much
/// nearer than the ball at kickoff, so its downward sightline is considerably
/// steeper even though both subjects are on the floor.
const CAM_BALL_CAR_PITCH: i32 = 260;
/// Car cam looks just beyond the nose. A far-ahead aim point works only while
/// the full follow distance is available; at kickoff the end wall shortens
/// that distance and the same pitch put the car below the 240-line frame.
const CAM_CAR_AIM: i32 = 120;
/// Maximum per-frame change of the camera boom relative to the car. Ordinary
/// driving stays below this; a discontinuous surface-basis change is spread
/// over a few frames without delaying the car's own world-space movement.
const CAM_OFFSET_STEP: i32 = 96;
const CAM_YAW_STEP: i32 = 96;
const CAM_PITCH_STEP: i32 = 96;

#[derive(Copy, Clone)]
struct CameraState {
    valid: bool,
    offset: (i32, i32, i32),
    yaw: u16,
    pitch: i32,
}

impl CameraState {
    const EMPTY: Self = Self {
        valid: false,
        offset: (0, 0, 0),
        yaw: 0,
        pitch: 0,
    };
}

static mut CHASE_CAMERAS: [CameraState; 2] = [CameraState::EMPTY; 2];
const DEPTH_RANGE: DepthRange = DepthRange::new(120, 14000);
const SKY_SLOT: usize = OT_DEPTH - 1;
/// The scoreboard fascia. In front of the world, behind the boost dial on
/// slot 1, with the team blocks cutting the plate they sit on.
const HUD_PLATE_SLOT: usize = 3;
const HUD_BLOCK_SLOT: usize = 2;
const HUD_RULE_SLOT: usize = 1;

/// Horizontal centre of the scoreboard's dark middle panel, which is what the
/// clock is centred on. Exported so the HUD text and the plate under it cannot
/// drift apart.
pub const HUD_CENTRE_X: i16 = 160;
// ---- depth layering --------------------------------------------------------
//
// The ordering table *prepends* within a slot, so of two packets sharing one
// slot the later insertion is drawn first and ends up underneath. The floor is
// built in phase one and the car in phase two, so every depth tie between them
// put the car under the pitch. Nudging one thing at a time chases that around
// the scene forever; this is the whole scheme in one place instead.
//
// Biases are in camera-space depth units, and `DEPTH_RANGE` spreads 120..14000
// over 512 slots, so roughly 27 units to a slot. Positive is further away.
// Everything that stands on the pitch is nearer than the pitch, by more than
// the depth tie is wide, and by less than the length of a car so nothing ever
// jumps in front of something genuinely closer.

/// The pitch itself, pushed back so nothing standing on it can tie with it.
const FLOOR_BIAS: i32 = 150;
/// Crack-underlay strips along subdivision-band edges: behind the pitch by
/// more than half a tile of depth, because the neighbouring quads sort by
/// their centres, not by the shared edge. They show only through the
/// single-pixel holes the band boundary can still open.
const UNDERDRAW_BIAS: i32 = FLOOR_BIAS + 1000;
/// Shadows sit between the pitch and the thing casting them.
const SHADOW_DEPTH_BIAS: i32 = 60;
/// Boost pads read as objects on the ground rather than paint, so they come
/// forward of the markings.
const PAD_BIAS: i32 = 30;
/// Pulls the boost plume in front of the car's own shadow, which otherwise wins
/// the slot and hides it.
///
/// Well clear of [`SHADOW_DEPTH_BIAS`] rather than just past it. The ordering
/// table quantises depth into slots roughly 27 uu apart at the distance a chase
/// camera sits, so a bias 30 above the shadow's is about one slot of margin and
/// measured as still hidden; this is nearer four.
const FLAME_BIAS: i32 = -300;

type Rgb = (u8, u8, u8);

/// The three authored arena looks. These are discrete presets rather than a
/// clock: a five-minute match should not cross from daylight into darkness,
/// and a player can change the look from the pause menu without changing any
/// simulation state.
#[derive(Clone, Copy, PartialEq)]
pub enum ArenaTime {
    Day,
    Sunset,
    Night,
}

impl ArenaTime {
    pub const fn next(self) -> Self {
        match self {
            Self::Day => Self::Sunset,
            Self::Sunset => Self::Night,
            Self::Night => Self::Day,
        }
    }

    pub const fn prev(self) -> Self {
        match self {
            Self::Day => Self::Night,
            Self::Sunset => Self::Day,
            Self::Night => Self::Sunset,
        }
    }
}

/// Sky and baked-world lighting for one arena preset.
///
/// This follows VoXide's Minecraft sky pass: the zenith and horizon are
/// authored separately, sunset warms the horizon rather than browning the
/// entire dome, and the world receives a related cast instead of remaining
/// cold under an orange sky.
#[derive(Clone, Copy)]
struct ArenaLook {
    zenith: Rgb,
    horizon: Rgb,
    ambient: (i32, i32, i32),
    /// Floodlight contribution, where 256 is the original night rig.
    lamp_scale: i32,
    world_tint: Rgb,
    /// Sixteenths of `world_tint` mixed into the baked result.
    world_mix: i32,
}

const DAY_LOOK: ArenaLook = ArenaLook {
    zenith: (58, 110, 214),
    horizon: (120, 167, 255),
    ambient: (92, 98, 112),
    lamp_scale: 96,
    world_tint: (148, 164, 190),
    world_mix: 2,
};
const SUNSET_LOOK: ArenaLook = ArenaLook {
    zenith: (24, 36, 88),
    horizon: (78, 88, 146),
    ambient: (58, 52, 62),
    lamp_scale: 192,
    world_tint: (190, 114, 78),
    world_mix: 3,
};
const NIGHT_LOOK: ArenaLook = ArenaLook {
    zenith: (4, 6, 28),
    horizon: (12, 18, 58),
    ambient: (44, 45, 54),
    lamp_scale: 256,
    world_tint: (128, 128, 128),
    world_mix: 0,
};
const SUNSET_GLOW: Rgb = (236, 122, 60);

static mut ARENA_TIME: ArenaTime = ArenaTime::Night;

fn arena_look() -> ArenaLook {
    match unsafe { ARENA_TIME } {
        ArenaTime::Day => DAY_LOOK,
        ArenaTime::Sunset => SUNSET_LOOK,
        ArenaTime::Night => NIGHT_LOOK,
    }
}

/// Change the arena look and rebuild the static lighting tables. This is paid
/// once when the setting changes, never in the steady-state frame loop.
pub fn set_arena_time(time: ArenaTime) {
    if unsafe { ARENA_TIME } == time {
        return;
    }
    unsafe { ARENA_TIME = time };
    build_lighting();
    if unsafe { CURB_PAINTED } {
        paint_curb();
    }
}

// The fixed team colours used to live here. They are now the signal colours
// of PAINTS[0] and PAINTS[5], which are what the two seats wear by default, so
// a match nobody redressed looks exactly as it did.
const GRASS_A: Rgb = (30, 62, 40);
const GRASS_B: Rgb = (38, 76, 48);
// Pitch tiling. Finer than a flat surface needs, because a quad with a vertex
// behind the camera is dropped whole: small tiles lose a sliver, one big quad
// loses the entire floor.
const TILES_X: i32 = 8;
const TILES_Z: i32 = 10;
/// Segments per side wall, for the same reason.
const WALL_SEGS: i32 = 8;
/// Radius of the curve where the floor rolls up into the wall, and the wall
/// rolls over into the ceiling. Rocket League's arena is a rounded tray, not a
/// box: these transitions are most of why it reads as an arena. Real ones are
/// about this size; RLBot's field tables ignore them, so this is eyeballed.
const RAMP_R: i32 = sim::WALL_RAMP_R;
const CEIL_R: i32 = sim::CEIL_R;
/// Segments used for each quarter-circle wall transition.
///
/// Three made every chord turn thirty degrees, and split-screen then skipped
/// alternate rings, including the floor curve's tangent point. Eight keeps the
/// silhouette within about five world units of the true 260-uu arc and is still
/// comfortably below a pixel per chord in a half-width view.
const CURVE_SEGS: usize = 8;
/// Points in the swept cross-section: both quarter turns, their joins, and
/// the two rings that bracket the lit rail.
const PROFILE_LEN: usize = 2 * CURVE_SEGS + 4;
/// The two rings the rail runs between, and how high off the pitch it sits.
///
/// Between the top of the floor ramp and the roof curve the wall used to be
/// one single band, so the tallest surface in the game had exactly two rows
/// of vertices and could only ever be a gradient. Splitting it at the rail
/// costs two quads a span and is what lets the wall carry a hard bright line
/// the way a real arena's illuminated hoarding does. Low on purpose: the
/// transparent enclosure should begin below the 643-uu crossbar instead of
/// turning most of the arena wall into an opaque textured ramp.
const RAIL_LO_RING: usize = CURVE_SEGS + 1;
const RAIL_HI_RING: usize = CURVE_SEGS + 2;
const RAIL_LO_Y: i32 = 320;
const RAIL_HI_Y: i32 = 400;

/// Where the side wall stops and the corner chamfer begins, on Z.
const CORNER_Z: i32 = sim::CORNER - sim::HALF_X; // 3968
/// Where the chamfer meets the end wall, on X.
const CORNER_X: i32 = sim::CORNER - sim::HALF_Z; // 2944

// floor(80) + swept walls(24 spans x 7) + roof cover(96)
// + goals(14) + ball(48) + flame(2 x 2) + shadows(2 x 3) + lamps(27)
// + pads(34, up to 4 each now that each stands on a two-ring plate), with slack.
// The pads were never in this tally and the plate pushed them past the old 448.
const MAX_QUADS: usize = 576;

/// GP0 polygon-command bit 25: blend this primitive with what is already in
/// the framebuffer instead of overwriting it.
const SEMI_TRANSPARENT: u32 = 1 << 25;

/// Sides on a shadow disc.
///
/// Eight, drawn as three quads in a strip. A shadow is twenty to forty pixels
/// across, and at that size an octagon is a circle: the flats are under a
/// pixel of chord error each. Twelve sides cost two more quads to remove
/// something already invisible.
const SHADOW_SIDES: usize = 8;

/// How many pieces an explosion throws, how long they last in ticks, how fast
/// they leave in uu a tick, how big they start in uu, and what pulls them back
/// down in uu per tick squared, Q8.
///
/// The gravity is not the arena's. Debris under 650 uu/s^2 barely moves in the
/// half second it is alive; this is the value that makes sixteen pieces read as
/// thrown rather than as drifting.
/// How much bigger a goal explosion is than a demolition. The camera stays
/// behind your car through the celebration, so the blast is most of an arena
/// away and at life size it is a few pixels of confetti.
const GOAL_BURST_SCALE: i32 = 4096 * 7 / 2;

/// The flash: how long it lasts in ticks and how wide it gets in uu.
const FLASH_LIFE: i32 = 8;
const FLASH_R: i32 = 70;
/// Screen half-size ceilings, in pixels, for the two soft layers. A blast goes
/// off under the bumper of the car that caused it, so perspective alone will
/// hand either of them the whole frame if it is allowed to.
const FLASH_MAX_PX: i16 = 26;
const SMOKE_MAX_PX: i16 = 16;
/// Sparks: how many, how long each lives, how fast it leaves in uu a tick,
/// how thick its streak is in uu, and how far back the streak reaches.
const SPARK_COUNT: i32 = 18;
const SPARK_LIFE: i32 = 30;
const SPARK_SPEED: i32 = 26;
const SPARK_W: i32 = 6;
/// The fire half of the spray: brighter, faster, thinner, and over in a fifth
/// of a second.
const FIRE_LIFE: i32 = 13;
const FIRE_SPEED: i32 = 46;
const FIRE_W: i32 = 5;
const STREAK_TICKS: i32 = 3;
/// Screen ceilings for one streak, across and along, in pixels.
const STREAK_MAX_PX: i32 = 5;
const STREAK_MAX_LEN: i32 = 26;
/// Smoke: how many puffs, how fast they drift out and up in uu a tick, and
/// how big they start and grow in uu.
const SMOKE_COUNT: i32 = 9;
const SMOKE_SPEED: i32 = 5;
const SMOKE_RISE: i32 = 26;
const SMOKE_R: i32 = 12;
const SMOKE_GROW: i32 = 13;
const BURST_LIFE: i32 = 40;
const BURST_GRAVITY: i32 = 128;

// ---- the light rig ---------------------------------------------------------
//
// Everything except the cars used to be a colour typed in by hand, so the
// arena had no light in it: no floodlights, no pool on the pitch, nothing
// telling you where the brightness came from. Night is defined by its lamps;
// day and sunset retain the same rig at lower strength under a lifted sky.
//
// The arena never moves and neither do its lamps, so none of this is a
// per-frame cost. The falloff is evaluated once at boot into two tables --
// one over the pitch, one over the swept wall profile -- and the frame loop
// only ever indexes them. That buys per-vertex lighting on the two largest
// surfaces in the game for four array reads a quad.

/// Distance unit the falloff works in. A 13000-uu diagonal squared overflows
/// nothing at 1/64 uu, and the extra precision buys nothing at these radii.
const LAMP_SHIFT: i32 = 6;

/// A floodlight.
struct Lamp {
    /// Render-space position (Y negative is up), in `1 << LAMP_SHIFT` units.
    p: (i32, i32, i32),
    /// Squared reach, same units. Attenuation is `(r2 / (r2 + d2))^2`: no
    /// cutoff, no shadow, and cheap enough to bake thousands of samples of at
    /// boot. Squared because the plain form has a 1/d^2 tail, and nine of
    /// those summed over an arena this size add up to a flat wash -- the exact
    /// thing being fixed. Squaring makes each lamp a pool with a dark edge.
    r2: i32,
    /// Peak contribution in tint units, where 128 is the GPU's 1.0.
    c: (i32, i32, i32),
}

/// How high the lamps hang: just under the roof line, where a real arena
/// puts them.
const LAMP_Y: i32 = -((sim::CEIL - 240) >> LAMP_SHIFT);

/// Nine sources: one over each corner chamfer, one halfway along each side
/// and end wall, and a rig over the centre spot.
///
/// The rig is what puts a pool of light on the middle of the pitch. Without
/// it the eight wall banks light the touchlines and leave the centre -- the
/// one part of the pitch you are always looking at -- as the darkest thing
/// on screen, which is exactly backwards.
/// Peak contribution of each kind of bank. The wall banks hang a couple of
/// hundred units off the surface they light, so they nearly saturate it and
/// need no headroom; the roof rig is two thousand units off the pitch, so its
/// peak is scaled for the attenuation it arrives with.
const CORNER_C: (i32, i32, i32) = (216, 220, 236);
const SIDE_C: (i32, i32, i32) = (200, 200, 212);
const END_C: (i32, i32, i32) = (196, 188, 176);
const RIG_C: (i32, i32, i32) = (600, 610, 615);

const LAMPS: [Lamp; 9] = [
    // Corner pylons, hung just off the 45-degree chamfer they light.
    Lamp {
        p: (-3379 >> LAMP_SHIFT, LAMP_Y, -(4403 >> LAMP_SHIFT)),
        r2: 27 * 27,
        c: CORNER_C,
    },
    Lamp {
        p: (3379 >> LAMP_SHIFT, LAMP_Y, -(4403 >> LAMP_SHIFT)),
        r2: 27 * 27,
        c: CORNER_C,
    },
    Lamp {
        p: (-3379 >> LAMP_SHIFT, LAMP_Y, 4403 >> LAMP_SHIFT),
        r2: 27 * 27,
        c: CORNER_C,
    },
    Lamp {
        p: (3379 >> LAMP_SHIFT, LAMP_Y, 4403 >> LAMP_SHIFT),
        r2: 27 * 27,
        c: CORNER_C,
    },
    // Side-wall banks, level with the halfway line.
    Lamp {
        p: (-(3900 >> LAMP_SHIFT), LAMP_Y, 0),
        r2: 26 * 26,
        c: SIDE_C,
    },
    Lamp {
        p: (3900 >> LAMP_SHIFT, LAMP_Y, 0),
        r2: 26 * 26,
        c: SIDE_C,
    },
    // Behind each goal. Warmer, because they are what the shot you are
    // lining up is lit by.
    Lamp {
        p: (0, LAMP_Y, -(4900 >> LAMP_SHIFT)),
        r2: 24 * 24,
        c: END_C,
    },
    Lamp {
        p: (0, LAMP_Y, 4900 >> LAMP_SHIFT),
        r2: 24 * 24,
        c: END_C,
    },
    // The centre rig, hung off the roof. This is the one that puts a pool on
    // the middle of the pitch, which is the part you are always looking at.
    Lamp {
        p: (0, -(sim::CEIL >> LAMP_SHIFT), 0),
        r2: 30 * 30,
        c: RIG_C,
    },
];

/// How much of the pitch's own light bounces back onto the wall standing on
/// it, and in what colour. Green, because that is what it is bouncing off.
///
/// This exists for one view: jammed into a corner, where the camera is close
/// enough to the chamfer that the rail and the lamp are both off the top of
/// the screen and the only wall you can see is the bottom two feet of it.
/// Every lamp in the rig is 1700 uu over that, so with distance falloff alone
/// it stays black no matter what the lamps do. The pitch, on the other hand,
/// is right there and lit.
const BOUNCE: (i32, i32, i32) = (72, 118, 52);
/// Height at which the bounce has fallen to half. Roughly a wall's worth.
const BOUNCE_FALL: i32 = 560;

/// Tint at `p` for a surface facing `n` (Q12, render space). Pass
/// `(0, 0, 0)` for a surface with no meaningful facing.
///
/// Boot-time only, so the square root per lamp is free. The facing term is
/// half-Lambert -- a surface turned away keeps half its light rather than
/// going to nothing -- because a hard cosine on an arena made of six flat
/// walls turns every seam into a hard edge.
fn lamp_light(p: (i32, i32, i32), n: (i32, i32, i32)) -> Rgb {
    let q = (p.0 >> LAMP_SHIFT, p.1 >> LAMP_SHIFT, p.2 >> LAMP_SHIFT);
    let look = arena_look();
    let (mut r, mut g, mut b) = look.ambient;
    let faces = n != (0, 0, 0);
    for l in LAMPS.iter() {
        let d = (l.p.0 - q.0, l.p.1 - q.1, l.p.2 - q.2);
        let d2 = d.0 * d.0 + d.1 * d.1 + d.2 * d.2;
        let mut f = (l.r2 << 12) / (l.r2 + d2);
        f = (f * f) >> 12;
        if faces {
            let dist = isqrt_i32(d2).max(1);
            let cos = (n.0 * d.0 + n.1 * d.1 + n.2 * d.2) / dist;
            f = (f * (2048 + cos / 2).clamp(0, 4096)) >> 12;
        }
        r += ((l.c.0 * f) >> 12) * look.lamp_scale >> 8;
        g += ((l.c.1 * f) >> 12) * look.lamp_scale >> 8;
        b += ((l.c.2 * f) >> 12) * look.lamp_scale >> 8;
    }
    let lit = (
        r.clamp(0, 255) as u8,
        g.clamp(0, 255) as u8,
        b.clamp(0, 255) as u8,
    );
    mix(lit, look.world_tint, look.world_mix)
}

/// Finest the floor ever splits a tile, and so the resolution the pitch's
/// light is baked at.
const FLOOR_SPLIT_MAX: i32 = 4;
const FLOOR_GX: usize = (TILES_X * FLOOR_SPLIT_MAX) as usize + 1;
const FLOOR_GZ: usize = (TILES_Z * FLOOR_SPLIT_MAX) as usize + 1;

/// Per-vertex pitch tint, on the sub-tile grid, in two copies.
///
/// Two, because the mown stripes are a per-tile brightness step and a vertex
/// shared between two tiles can only carry one colour. Giving each stripe its
/// own table keeps the step hard where the tiles meet, which is what a mown
/// stripe looks like, and costs 8 KB.
static mut FLOOR_LIGHT: [[[Rgb; FLOOR_GZ]; FLOOR_GX]; 2] =
    [[[(128, 128, 128); FLOOR_GZ]; FLOOR_GX]; 2];

/// A perimeter run of the swept wall profile.
#[derive(Copy, Clone)]
struct Span {
    a: (i32, i32),
    b: (i32, i32),
    /// Inward unit normal, Q12.
    n: (i32, i32),
}

const SPAN_COUNT: usize = WALL_SEGS as usize * 2 + 8;
static mut SPANS: [Span; SPAN_COUNT] = [Span {
    a: (0, 0),
    b: (0, 0),
    n: (0, 0),
}; SPAN_COUNT];

/// Positions along a span the splitter can land a vertex on, as twelfths:
/// 0, 1/3, 1/2, 2/3, 1. Splits are 1, 2 or 3, so those five cover every
/// vertex the wall can emit, and the light for each is baked once.
const SLOT_TWELFTHS: [i32; 5] = [0, 4, 6, 8, 12];
/// Slot index of vertex `k` of a span split `splits` ways.
const SLOT_OF: [[usize; 4]; 4] = [[0, 0, 0, 0], [0, 4, 0, 0], [0, 2, 4, 0], [0, 1, 3, 4]];

static mut WALL_LIGHT: [[[Rgb; 5]; PROFILE_LEN]; SPAN_COUNT] =
    [[[(128, 128, 128); 5]; PROFILE_LEN]; SPAN_COUNT];

/// The untinted light for the rings the barrier occupies, kept so the team
/// colours can be laid over them again whenever a match changes them.
///
/// Doing that per vertex per frame instead cost a mix and a multiply on every
/// corner of every lower-wall quad, which measured as eleven dropped frames in
/// nine hundred. The barrier's colour changes twice a match at the very most.
static mut CURB_BASE: [[[Rgb; 5]; RAIL_LO_RING + 1]; SPAN_COUNT] =
    [[[(128, 128, 128); 5]; RAIL_LO_RING + 1]; SPAN_COUNT];
/// Where each span's split positions sit in Z, for the barrier's blend.
static mut CURB_Z: [[i32; 5]; SPAN_COUNT] = [[0; 5]; SPAN_COUNT];

/// The four corners of the roof, lit like everything else.
static mut CEIL_LIGHT: [Rgb; 4] = [(128, 128, 128); 4];
/// Roof patch layout, shared by the light bake and the draw so the corner
/// light table below indexes exactly the corners `ceiling` projects.
/// Smaller than the atlas itself because PS1 texture mapping is affine.
/// A full 128x84 patch viewed from directly underneath shears its near
/// cells into long rectangles. These dimensions remain exact lattice
/// periods (8 texels across, 14 down), so subdivision adds no seam.
const ROOF_PATCH_U: i32 = 64;
const ROOF_PATCH_V: i32 = 28;
const ROOF_STEP_X: i32 = ROOF_PATCH_U * COVER_UU_PER_TEXEL;
const ROOF_STEP_Z: i32 = ROOF_PATCH_V * COVER_UU_PER_TEXEL;
const ROOF_HALF_X: i32 = sim::HALF_X - CEIL_R;
const ROOF_HALF_Z: i32 = sim::HALF_Z - CEIL_R;
const ROOF_COLS: usize = ((2 * ROOF_HALF_X + ROOF_STEP_X - 1) / ROOF_STEP_X) as usize;
const ROOF_ROWS: usize = ((2 * ROOF_HALF_Z + ROOF_STEP_Z - 1) / ROOF_STEP_Z) as usize;
/// Light at every roof patch corner, baked once with `CEIL_LIGHT`. The draw
/// used to re-blend the four roof corners for all four corners of every patch
/// every frame (twelve mixes a patch, about a hundred patches a view), which
/// was the single largest cost of a split frame; the roof never moves.
static mut ROOF_CORNER_LIGHT: [[Rgb; ROOF_ROWS + 1]; ROOF_COLS + 1] =
    [[(128, 128, 128); ROOF_ROWS + 1]; ROOF_COLS + 1];
/// World X of roof corner column `ix` (the last column is clipped to the
/// roof edge, as the patch walk always did).
fn roof_corner_x(ix: usize) -> i32 {
    (-ROOF_HALF_X + ix as i32 * ROOF_STEP_X).min(ROOF_HALF_X)
}
fn roof_corner_z(iz: usize) -> i32 {
    (-ROOF_HALF_Z + iz as i32 * ROOF_STEP_Z).min(ROOF_HALF_Z)
}

/// Multiply a surface colour by a tint the way the GPU does for a texture,
/// so the untextured pieces of the arena sit in the same light as the
/// textured ones instead of floating at a brightness of their own.
fn tinted(base: Rgb, tint: Rgb) -> Rgb {
    (
        ((base.0 as i32 * tint.0 as i32) >> 7).min(255) as u8,
        ((base.1 as i32 * tint.1 as i32) >> 7).min(255) as u8,
        ((base.2 as i32 * tint.2 as i32) >> 7).min(255) as u8,
    )
}

/// Half-length of a floodlight bar, and of the centre rig.
const LAMP_HALF_W: i32 = 680;
/// Half-height of the glowing face.
const LAMP_HALF_H: i32 = 84;
const RIG_HALF: i32 = 760;
/// The lamp face, its cooler lower half, and the housing over it. The face
/// is the only thing in the game allowed to be this bright.
const LAMP_HOT: Rgb = (255, 252, 236);
const LAMP_WARM: Rgb = (214, 206, 170);
const LAMP_HOUSING: Rgb = (46, 50, 62);

/// What the rail glows at, before the wall panel underneath it. It is an
/// emitter, not a lit surface, so the falloff does not apply: at 238 the wall
/// palette's brightest texel comes out near white-blue, which is the only
/// thing besides the lamp faces occupying the top of the range.
const RAIL_TINT: Rgb = (238, 242, 255);
/// A weaker lift at the wall top, so the roofline reads as an edge rather
/// than fading into the ceiling.
const RAIL_LIFT: i32 = 40;
/// And at the line where the floor curve meets the wall. Not a rail, a
/// gradient: it fades over the short straight run into the real rail. From a
/// camera a foot off the ground this is often the only part of the wall on
/// screen, so it is what gives a corner an edge to read against.
const BASE_LIFT: i32 = 132;
/// Index of the profile ring at the top of the straight wall.
const WALL_TOP_RING: usize = RAIL_HI_RING + 1;

/// Pitch tint at an arbitrary world point, off the baked grid.
///
/// The markings and the shadows are painted on the pitch, so they have to
/// follow its light or they read as decals dropped on top of it.
fn floor_tint(x: i32, z: i32) -> Rgb {
    let gx = ((x + sim::HALF_X) * (FLOOR_GX as i32 - 1) / (sim::HALF_X * 2))
        .clamp(0, FLOOR_GX as i32 - 1) as usize;
    let gz = ((z + sim::HALF_Z) * (FLOOR_GZ as i32 - 1) / (sim::HALF_Z * 2))
        .clamp(0, FLOOR_GZ as i32 - 1) as usize;
    unsafe { FLOOR_LIGHT[0][gx][gz] }
}

/// Bake every static light table. Runs once, at boot.
fn build_lighting() {
    // Pitch. Up in render space is -Y.
    let step_x = sim::HALF_X * 2 / TILES_X;
    let step_z = sim::HALF_Z * 2 / TILES_Z;
    for stripe in 0..2 {
        // The mown stripes, as a brightness step rather than a hue one: the
        // grass photo has no blue in it at all, so a tint can only move the
        // red/green balance, and a plain step is what reads as mowing.
        let k = if stripe == 0 { 256 } else { 274 };
        for gx in 0..FLOOR_GX {
            for gz in 0..FLOOR_GZ {
                let x = -sim::HALF_X + gx as i32 * step_x / FLOOR_SPLIT_MAX;
                let z = -sim::HALF_Z + gz as i32 * step_z / FLOOR_SPLIT_MAX;
                // The corners of the pitch are pulled in to meet the chamfer,
                // so that is where the vertex actually is.
                let (cx, cz) = Builder::chamfer(x, z);
                let c = lamp_light((cx, 0, cz), (0, -4096, 0));
                // Warm each end toward its team, smoothly. The old version
                // stepped it per tile, which drew two bands across the pitch.
                let w = (((cz.abs() - 2600).max(0) * 16) / 2520).min(5);
                let team = if cz < 0 {
                    (86, 132, 210)
                } else {
                    (226, 152, 74)
                };
                let c = mix(c, team, w);
                unsafe {
                    FLOOR_LIGHT[stripe][gx][gz] = (
                        ((c.0 as i32 * k) >> 8).clamp(0, 255) as u8,
                        ((c.1 as i32 * k) >> 8).clamp(0, 255) as u8,
                        ((c.2 as i32 * k) >> 8).clamp(0, 255) as u8,
                    );
                }
            }
        }
    }

    // Walls. One tint per (span, ring, split position).
    let profile = Builder::profile();
    for si in 0..SPAN_COUNT {
        let s = unsafe { SPANS[si] };
        for (ri, &(inset, height)) in profile.iter().enumerate() {
            for slot in 0..5 {
                let t = SLOT_TWELFTHS[slot];
                let at = (
                    s.a.0 + (s.b.0 - s.a.0) * t / 12,
                    s.a.1 + (s.b.1 - s.a.1) * t / 12,
                );
                let p = (
                    at.0 + ((s.n.0 * inset) >> 12),
                    -height,
                    at.1 + ((s.n.1 * inset) >> 12),
                );
                let c = if ri == RAIL_LO_RING || ri == RAIL_HI_RING {
                    RAIL_TINT
                } else {
                    let c = lamp_light(p, (s.n.0, 0, s.n.1));
                    let lift = match ri {
                        WALL_TOP_RING => RAIL_LIFT,
                        CURVE_SEGS => BASE_LIFT,
                        _ => 0,
                    };
                    // Light off the pitch below, falling away with height.
                    let below = floor_tint(p.0, p.2);
                    let k = (BOUNCE_FALL << 12) / (BOUNCE_FALL + height.max(0));
                    let up = |v: u8, w: i32| (((v as i32 * k) >> 12) * w) >> 8;
                    (
                        (c.0 as i32 + lift + up(below.0, BOUNCE.0)).clamp(0, 255) as u8,
                        (c.1 as i32 + lift + up(below.1, BOUNCE.1)).clamp(0, 255) as u8,
                        (c.2 as i32 + lift + up(below.2, BOUNCE.2)).clamp(0, 255) as u8,
                    )
                };
                unsafe {
                    WALL_LIGHT[si][ri][slot] = c;
                    if ri <= RAIL_LO_RING {
                        CURB_BASE[si][ri][slot] = c;
                        CURB_Z[si][slot] = at.1;
                    }
                }
            }
        }
    }

    // Roof, seen edge-on from a ground camera but lit all the same.
    let (cx, cz) = (sim::HALF_X - CEIL_R, sim::HALF_Z - CEIL_R);
    for (i, &(sx, sz)) in [(-1, -1), (1, -1), (-1, 1), (1, 1)].iter().enumerate() {
        unsafe {
            CEIL_LIGHT[i] = lamp_light((sx * cx, -sim::CEIL, sz * cz), (0, 4096, 0));
        }
    }
    // The same bilinear blend `ceiling` used to evaluate per patch corner per
    // frame, evaluated once per distinct corner instead.
    let l = unsafe { CEIL_LIGHT };
    let (x, z) = (ROOF_HALF_X, ROOF_HALF_Z);
    for ix in 0..=ROOF_COLS {
        for iz in 0..=ROOF_ROWS {
            let (px, pz) = (roof_corner_x(ix), roof_corner_z(iz));
            let tx = ((px + x) * 16 / (2 * x)).clamp(0, 16);
            let tz = ((pz + z) * 16 / (2 * z)).clamp(0, 16);
            unsafe {
                ROOF_CORNER_LIGHT[ix][iz] = mix(mix(l[0], l[1], tx), mix(l[2], l[3], tx), tz);
            }
        }
    }
}

/// Lay out the wall perimeter. Same order the old `walls` loop drew it in;
/// pulling it into a table is what lets the light bake index a span.
fn build_spans() {
    let mut i = 0;
    let mut put = |a: (i32, i32), b: (i32, i32), n: (i32, i32)| {
        unsafe { SPANS[i] = Span { a, b, n } };
        i += 1;
    };
    let step = CORNER_Z * 2 / WALL_SEGS;
    for k in 0..WALL_SEGS {
        let z0 = -CORNER_Z + k * step;
        let z1 = z0 + step;
        put((-sim::HALF_X, z0), (-sim::HALF_X, z1), (4096, 0));
        put((sim::HALF_X, z0), (sim::HALF_X, z1), (-4096, 0));
    }
    const D: i32 = 2896; // 4096 / sqrt(2)
    for &sx in &[-1i32, 1] {
        for &sz in &[-1i32, 1] {
            put(
                (sx * sim::HALF_X, sz * CORNER_Z),
                (sx * CORNER_X, sz * sim::HALF_Z),
                (-sx * D, -sz * D),
            );
        }
    }
    for &sz in &[-1i32, 1] {
        let z = sz * sim::HALF_Z;
        let gw = sim::GOAL_HALF_W;
        put((-CORNER_X, z), (-gw, z), (0, -sz * 4096));
        put((gw, z), (CORNER_X, z), (0, -sz * 4096));
    }
}

// ---- arena texture ---------------------------------------------------------
// One 4bpp page holds the 64x64 pitch tile, the 32x32 wall panel, the 128x84
// honeycomb enclosure, a separate 96x48 square goal net, and two more pages of
// full-resolution grass with the pitch markings composited into it. Separate
// CLUTs let the solid surfaces and two open meshes share the asset without
// trying to share a sixteen-colour palette.

const TEX_TPAGE: Tpage = Tpage::new(384, 0, TexDepth::Bit4);
const MARKED_LEFT_TPAGE: Tpage = Tpage::new(448, 0, TexDepth::Bit4);
const MARKED_RIGHT_TPAGE: Tpage = Tpage::new(576, 0, TexDepth::Bit4);
/// One palette each. Sharing sixteen colours between grass and wall left six
/// for the pitch, which is the largest surface in the game; 4bpp lets every
/// quad name its own CLUT, so they get sixteen apiece for nothing.
const TEX_CLUT: Clut = Clut::new(384, 257);
const GRASS_CLUT: Clut = Clut::new(384, 258);
/// Third palette, for the translucent arena cover and goal nets.
///
/// Its own CLUT because entry 0 has to be `0x0000`, and a textured polygon
/// skips a texel that resolves to that: it is how the PS1 masks sprites, and it
/// is what makes the holes in a net holes rather than black paint. Grass and
/// wall both use entry 0 for real colour, so they cannot share this.
const COVER_CLUT: Clut = Clut::new(384, 259);
/// Fifteen grass colours plus chalk for the two marked-pitch pages.
const MARKED_CLUT: Clut = Clut::new(384, 260);
/// Grass occupies a 64x64 square at the origin, the wall a 32x32 tile beside
/// it, the goal net sits directly below both, and the honeycomb fills the
/// upper-right. Two following source pages are four by four 64-pixel marked
/// grass tiles each. They upload into the free VRAM columns on either side of
/// the HUD page rather than sitting contiguously at runtime.
const TEX_W: usize = 256;
const TEX_H: usize = 256 * 3;
const GRASS_TILE_W: i32 = 64;
const MARKED_TILE_W: i32 = 64;
const MARKED_U0: i32 = 0;
const MARKED_V0: i32 = 0;
const MARKED_FIRST_Z: i32 = 3;
const MARKED_ROWS: i32 = 4;
const MARKED_COLS_PER_PAGE: i32 = 4;
/// 4bpp packs four texels per halfword.
const TEX_HALFWORDS_PER_ROW: usize = TEX_W / 4;

/// One texture coordinate, packed the way the packet wants it.
const fn uvw(u: u8, v: u8) -> u16 {
    (u as u16) | ((v as u16) << 8)
}

/// The arena's texture state, with dithering on.
///
/// The framebuffer is 15-bit, so a channel has 32 levels and every gradient
/// in the game -- sky, wall, pitch falloff, the ball -- steps in bands you
/// can count. The GPU's ordered dither trades those bands for noise at no
/// per-polygon cost, and it is the single cheapest thing that can be done to
/// this image. It has to be asked for twice: a textured polygon carries its
/// own tpage word, which owns the dither bit, while an untextured Gouraud one
/// reads whatever GP0(E1) was last set to, which is what
/// [`apply_arena_draw_mode`] is for.
const ARENA_MATERIAL: TextureMaterial =
    TextureMaterial::new(0, TEX_TPAGE.uv_tpage_word(0)).with_dither(true);

/// Prepacked packet words, one per palette. The floor and the walls are the
/// only textured geometry in the game and neither ever changes material, so
/// the CLUT / tpage / command words are resolved at compile time and every
/// quad only fills in positions, UVs and four colours.
const GRASS_PACKET: TexturedGouraudPacketMaterial =
    TextureMaterial::new(GRASS_CLUT.uv_clut_word(), TEX_TPAGE.uv_tpage_word(0))
        .with_dither(true)
        .textured_gouraud_packet_material();
const WALL_PACKET: TexturedGouraudPacketMaterial =
    TextureMaterial::new(TEX_CLUT.uv_clut_word(), TEX_TPAGE.uv_tpage_word(0))
        .with_dither(true)
        .textured_gouraud_packet_material();
const COVER_PACKET: TexturedGouraudPacketMaterial =
    TextureMaterial::new(COVER_CLUT.uv_clut_word(), TEX_TPAGE.uv_tpage_word(0))
        .with_dither(true)
        .textured_gouraud_packet_material();
const MARKED_LEFT_PACKET: TexturedGouraudPacketMaterial =
    TextureMaterial::new(
        MARKED_CLUT.uv_clut_word(),
        MARKED_LEFT_TPAGE.uv_tpage_word(0),
    )
    .with_dither(true)
    .textured_gouraud_packet_material();
const MARKED_RIGHT_PACKET: TexturedGouraudPacketMaterial =
    TextureMaterial::new(
        MARKED_CLUT.uv_clut_word(),
        MARKED_RIGHT_TPAGE.uv_tpage_word(0),
    )
        .with_dither(true)
        .textured_gouraud_packet_material();

/// One seamless honeycomb sheet in the page's spare width. The upper walls and
/// roof sample this same material, so the enclosure cannot change cell shape
/// at a join.
///
/// The 128-wide sheet fills the unused right of the 4bpp page. Eighty-four rows
/// are six complete two-row honeycomb periods, enough to carry the lower net
/// boundary continuously around the roof curve without exhausting V, while
/// both axes still tile without a doubled strand at a patch boundary.
const COVER_U0: u8 = 128;
const COVER_V0: u8 = 0;
const COVER_W: i32 = 128;
const COVER_H: i32 = 84;
const HEX_W: i32 = 8;
/// A distinct square-string sheet for the bag inside each goal. It lives below
/// the honeycomb so the goal can keep the same transparent palette and packet
/// without ever sampling the arena enclosure's hexagons.
const NET_U0: u8 = 0;
const NET_V0: u8 = GRASS_TILE_W as u8;
const NET_W: i32 = 96;
const NET_H: i32 = 48;
/// One cover texel represents this many world units on every face. The old
/// wall path mapped a full 32-pixel tile onto every band regardless of whether
/// that band was 804 uu of straight wall or 87 uu of roof curve; this shared
/// scale is what keeps every cell regular through the bend and over the roof.
const COVER_UU_PER_TEXEL: i32 = 22;

const _: () = assert!(
    COVER_U0 as i32 + COVER_W <= TEX_W as i32,
    "cover mesh runs off the texture page"
);
const _: () = assert!(
    NET_V0 as i32 + NET_H <= 256,
    "goal net runs off the base texture page"
);
const _: () = assert!(
    MARKED_U0 + MARKED_COLS_PER_PAGE * MARKED_TILE_W <= TEX_W as i32
        && MARKED_V0 + MARKED_ROWS * MARKED_TILE_W <= 256,
    "marked pitch tiles run off the texture page"
);

/// Texels of cover for `span` world units, shared by walls and roof.
const fn cover_texels(span: i32) -> u8 {
    let t = (span + COVER_UU_PER_TEXEL - 1) / COVER_UU_PER_TEXEL;
    if t < 1 {
        1
    } else if t > COVER_W {
        COVER_W as u8
    } else {
        t as u8
    }
}

/// Texels of square net for `span` world units. It uses the enclosure's world
/// scale so the goal cells stay square on the back, sides, and ceiling.
const fn net_texels(span: i32) -> u8 {
    let t = (span + COVER_UU_PER_TEXEL - 1) / COVER_UU_PER_TEXEL;
    if t < 1 {
        1
    } else if t > NET_W {
        NET_W as u8
    } else {
        t as u8
    }
}

// The square sheet has to hold every goal face without tiling: the back is the
// widest, while the side and roof depths are the largest V span. Keep these as
// compile-time checks so a goal-size change cannot silently cross atlas blocks.
const _: () = assert!(
    net_texels(2 * sim::GOAL_HALF_W) as i32 <= NET_W,
    "square net too narrow for the back of the goal"
);
const _: () = assert!(
    net_texels(sim::GOAL_H) as i32 <= NET_H,
    "square net too short for the back of the goal"
);
const _: () = assert!(
    net_texels(sim::GOAL_DEPTH) as i32 <= NET_H,
    "square net too short for the roof of the goal"
);
const _: () = assert!(
    NET_U0 as i32 + NET_W <= TEX_W as i32 && NET_V0 as i32 + NET_H <= TEX_H as i32,
    "square net runs off the texture page"
);

/// Tint for a cover strand. The texel is near-white and the GPU modulates by
/// this, so 128 a channel is unchanged; this is a touch brighter than neutral.
const COVER_STRAND: Rgb = (152, 156, 168);
/// Tint the barrier at the foot of the wall by which half of the pitch it is
/// on: a seat's own colour at its end, blending through the middle.
///
/// A multiplier on the baked light rather than a colour of its own, because
/// the barrier is textured and the GPU modulates the panel by whatever the
/// vertex carries. Held near 128 a channel so this shifts the hue without
/// making one end of the arena darker than the other.
fn curb(light: Rgb, z: i32) -> Rgb {
    const BLEND: i32 = 1400;
    let t = ((z + BLEND) * 16 / (2 * BLEND)).clamp(0, 16);
    let (a, b) = unsafe { (SEAT_HUE[0], SEAT_HUE[1]) };
    tinted(light, mix(a, b, t))
}

/// Lay the seats' colours back over the barrier's baked light. Runs when a
/// match sets its paints, not when it draws a frame.
fn paint_curb() {
    for si in 0..SPAN_COUNT {
        for ri in 0..=RAIL_LO_RING {
            for slot in 0..5 {
                unsafe {
                    WALL_LIGHT[si][ri][slot] = curb(CURB_BASE[si][ri][slot], CURB_Z[si][slot]);
                }
            }
        }
    }
}

/// The dark inside the goal box, at the floor and up at the crossbar.
const GOAL_VOID: Rgb = (18, 20, 30);
const GOAL_VOID_HI: Rgb = (34, 38, 52);

/// Turn dithering on for the untextured primitives in this frame's ordering
/// table. Immediate GP0 state, so it has to be re-applied every frame: the
/// HUD's own font draws leave the draw mode pointing at their atlas.
fn apply_arena_draw_mode() {
    ARENA_MATERIAL.apply_draw_mode();
}

/// Validate the cooked arena atlas and upload its three source pages and four
/// palettes. The marked pages straddle the HUD's VRAM column, so their source
/// rows are contiguous in the asset but their upload destinations are not.
/// The caller owns the backing buffer only until this returns; VRAM owns the
/// useful copy afterwards.
pub fn upload_arena_texture(blob: &[u8]) -> bool {
    let Ok(texture) = Texture::from_bytes(blob) else {
        return false;
    };
    if texture.width() as usize != TEX_W
        || texture.height() as usize != TEX_H
        || texture.halfwords_per_row() as usize != TEX_HALFWORDS_PER_ROW
        || texture.pixel_bytes().len() != TEX_HALFWORDS_PER_ROW * TEX_H * 2
        || texture.clut_entries() != 16 * 4
        || texture.clut_bytes().len() != 16 * 4 * 2
    {
        return false;
    }

    const PAGE_H: usize = 256;
    const PAGE_BYTES: usize = TEX_HALFWORDS_PER_ROW * PAGE_H * 2;
    for (page, tpage) in [TEX_TPAGE, MARKED_LEFT_TPAGE, MARKED_RIGHT_TPAGE]
        .iter()
        .copied()
        .enumerate()
    {
        let start = page * PAGE_BYTES;
        upload_bytes(
            VramRect::new(
                tpage.x(),
                tpage.y(),
                TEX_HALFWORDS_PER_ROW as u16,
                PAGE_H as u16,
            ),
            &texture.pixel_bytes()[start..start + PAGE_BYTES],
        );
    }
    for (row, clut) in [TEX_CLUT, GRASS_CLUT, COVER_CLUT, MARKED_CLUT]
        .iter()
        .copied()
        .enumerate()
    {
        let start = row * 16 * 2;
        upload_bytes(
            VramRect::new(clut.x(), clut.y(), 16, 1),
            &texture.clut_bytes()[start..start + 16 * 2],
        );
    }
    true
}

static mut OT: OrderingTable<OT_DEPTH> = OrderingTable::new();
static mut QUADS: [QuadGouraud; MAX_QUADS] =
    [const { QuadGouraud::new([(0, 0); 4], [(0, 0, 0); 4]) }; MAX_QUADS];
/// Textured quads live in their own pool: a different packet size, and the
/// arena's floor and walls are the only things that use them.
///
/// Sized for the worst frame rather than the observed one, because a full
/// arena drops quads silently: the near tiles split sixteen ways and the wall
/// sweep now carries nine rings a span, so a wide view of the pitch and a
/// dozen spans is about five hundred. RAM only, no cycles.
// The roof cover adds at most 96 regular patches to the former worst case.
// Keep another thirty-two packets of headroom for a near-plane split rather
// than allowing a high aerial to lose random cells from the enclosure.
const MAX_TEX_QUADS: usize = 704;
static mut TEX_QUADS: [QuadTexturedGouraud; MAX_TEX_QUADS] =
    [const { QuadTexturedGouraud::EMPTY }; MAX_TEX_QUADS];

/// Sim sub-units -> uu.
#[inline]
fn r(v: i32) -> i32 {
    v >> FP
}
/// Sim height -> render Y (negated: the GTE's +Y is down).
#[inline]
fn ry(v: i32) -> i32 {
    -(v >> FP)
}

fn shade(c: Rgb, num: i32, den: i32) -> Rgb {
    (
        (c.0 as i32 * num / den).clamp(0, 255) as u8,
        (c.1 as i32 * num / den).clamp(0, 255) as u8,
        (c.2 as i32 * num / den).clamp(0, 255) as u8,
    )
}

fn mix(a: Rgb, b: Rgb, weight_b: i32) -> Rgb {
    let w = weight_b.clamp(0, 16);
    (
        ((a.0 as i32 * (16 - w) + b.0 as i32 * w) / 16) as u8,
        ((a.1 as i32 * (16 - w) + b.1 as i32 * w) / 16) as u8,
        ((a.2 as i32 * (16 - w) + b.2 as i32 * w) / 16) as u8,
    )
}

// ---- transforms ------------------------------------------------------------

/// Yaw about Y from a Q0.12 angle, mapping local +Z onto `(sin a, 0, cos a)`.
///
/// `Mat3I16::rotate_y` exists but takes 256-per-revolution angles off an
/// uninterpolated table, which is 1.4 degrees a step: a car yawing at walking
/// pace visibly clicks between orientations. `psx-math`'s Q0.12 sin/cos
/// interpolates to 4096 steps, so the matrices are built here and composed
/// with the engine's own `mul`.
fn rot_y_q12(a: u16) -> Mat3I16 {
    let (s, c) = (sin_q12(a) as i16, cos_q12(a) as i16);
    Mat3I16 {
        m: [[c, 0, s], [0, 4096, 0], [-s, 0, c]],
    }
}

/// Roll about X, which for a rolling ball is the axis it turns on.
fn rot_x_q12(a: u16) -> Mat3I16 {
    let (s, c) = (sin_q12(a) as i16, cos_q12(a) as i16);
    Mat3I16 {
        m: [[4096, 0, 0], [0, c, -s], [0, s, c]],
    }
}

const IDENTITY: Mat3I16 = Mat3I16 {
    m: [[4096, 0, 0], [0, 4096, 0], [0, 0, 4096]],
};
/// Maps object Y-up onto render Y-down. Commutes with a yaw, so it can be
/// folded into either side of one.
const FLIP_Y: Mat3I16 = Mat3I16 {
    m: [[4096, 0, 0], [0, -4096, 0], [0, 0, 4096]],
};

/// `m * v`, Q12, on i32 inputs. `Mat3I16::transform` wants a `Vec3I16`, and
/// camera-relative offsets here are i32 by habit even though they fit.
fn apply(m: &Mat3I16, v: (i32, i32, i32)) -> (i32, i32, i32) {
    let row = |r: [i16; 3]| ((r[0] as i32) * v.0 + (r[1] as i32) * v.1 + (r[2] as i32) * v.2) >> 12;
    (row(m.m[0]), row(m.m[1]), row(m.m[2]))
}

/// The camera for one frame: a view matrix plus where it is standing.
#[derive(Copy, Clone)]
struct View {
    v: Mat3I16,
    pos: (i32, i32, i32),
}

impl View {
    /// Where an object sits in camera space. This is the translation the GTE
    /// wants, whether it is loaded by hand or through an `ActorTransform`.
    fn camera_space(&self, pos: (i32, i32, i32)) -> (i32, i32, i32) {
        apply(
            &self.v,
            (pos.0 - self.pos.0, pos.1 - self.pos.1, pos.2 - self.pos.2),
        )
    }

    /// Point the GTE at an object. `rot` takes object space to world space
    /// (render handedness); `pos` is the object's origin in render coordinates.
    fn set_object(&self, rot: &Mat3I16, pos: (i32, i32, i32)) {
        scene::load_rotation(&self.v.mul(rot));
        let t = self.camera_space(pos);
        scene::load_translation(Vec3I32::new(t.0, t.1, t.2));
    }

    /// Point the GTE at the world itself: no rotation, no offset.
    fn set_world(&self) {
        self.set_object(&IDENTITY, (0, 0, 0));
    }

    fn cull(&self) -> Cull {
        Cull {
            pos: self.pos,
            right: self.v.m[0],
            vertical: self.v.m[1],
            fwd: self.v.m[2],
        }
    }
}

/// Backdrop colours for this camera. Sunset keeps a cool zenith and warms the
/// horizon most strongly toward a fixed +Z sun, mirroring VoXide's directional
/// Minecraft sunset rather than washing all four corners orange.
fn arena_sky(view: &View) -> [Rgb; 4] {
    let look = arena_look();
    if unsafe { ARENA_TIME } != ArenaTime::Sunset {
        return [look.zenith, look.zenith, look.horizon, look.horizon];
    }
    let forward_z = view.v.m[2][2] as i32;
    let right_z = view.v.m[0][2] as i32;
    let warm = |facing: i32| {
        // A little warmth remains around the whole horizon, with most of it
        // confined to the half of the dome that faces the low sun.
        let w = 3 + facing.max(0) * 10 / 4096;
        mix(look.horizon, SUNSET_GLOW, w)
    };
    let left = warm(forward_z - right_z * 3 / 5);
    let right = warm(forward_z + right_z * 3 / 5);
    [look.zenith, look.zenith, left, right]
}

/// A whole-object rejection test, applied before any of the object's quads
/// reach the GTE.
///
/// Every quad in the arena used to be projected -- four `RTPS` plus the
/// register shuffle around them -- and only then checked against the screen.
/// About 55% of them failed that check, which is most of an arena's worth of
/// projection spent on geometry that is behind the camera. A bounding sphere
/// against the near plane and the two side planes costs two dot products and
/// throws a floor tile, a wall span or a boost pad away whole.
///
/// The bound is a world-axis-aligned box rather than a sphere, because the
/// things being tested are long and low or tall and thin: a wall span is two
/// thousand units high and a few hundred deep, and a sphere around it is wide
/// enough to fail almost nothing. Projecting the half-extents onto each plane
/// normal costs three more multiplies and rejects a third again as much.
///
/// Conservative on purpose: the extent is added to the depth on both tests, so
/// an object straddling a plane is kept. Nothing pops.
#[derive(Copy, Clone)]
struct Cull {
    pos: (i32, i32, i32),
    right: [i16; 3],
    vertical: [i16; 3],
    fwd: [i16; 3],
}

/// Screen half-width the side planes are drawn at, over the projection plane
/// distance, plus the slack `quad_biased` allows, so this never rejects a quad
/// that test keeps. Follows the viewport: halving the width in a split game
/// halves the frustum, which is where the second pass is paid for.
#[inline]
fn cull_half_w() -> i32 {
    unsafe { VIEW_HALF_W }
}

impl Cull {
    #[inline]
    fn dot(n: [i16; 3], d: (i32, i32, i32)) -> i32 {
        ((n[0] as i32) * d.0 + (n[1] as i32) * d.1 + (n[2] as i32) * d.2) >> 12
    }

    /// How far the box's half-extents reach along `n`: the support function of
    /// a world-axis-aligned box in a rotated direction.
    #[inline]
    fn extent(n: [i16; 3], h: (i32, i32, i32)) -> i32 {
        ((n[0] as i32).abs() * h.0 + (n[1] as i32).abs() * h.1 + (n[2] as i32).abs() * h.2) >> 12
    }

    /// Is any part of the box at `c` with half-extents `h` worth projecting?
    fn visible(&self, c: (i32, i32, i32), h: (i32, i32, i32)) -> bool {
        let d = (c.0 - self.pos.0, c.1 - self.pos.1, c.2 - self.pos.2);
        let z = Self::dot(self.fwd, d) + Self::extent(self.fwd, h);
        if z <= 0 {
            return false;
        }
        let x = Self::dot(self.right, d);
        x.abs() - Self::extent(self.right, h) <= z * cull_half_w() / PROJ_H as i32
    }

    /// Vertical half of the same conservative box/frustum test. Kept
    /// separate because most arena objects are already cheap to reject after
    /// the horizontal test; the roof uses this to replace its old pitch gate.
    fn visible_vertically(&self, c: (i32, i32, i32), h: (i32, i32, i32)) -> bool {
        let d = (c.0 - self.pos.0, c.1 - self.pos.1, c.2 - self.pos.2);
        let z = Self::dot(self.fwd, d) + Self::extent(self.fwd, h);
        if z <= 0 {
            return false;
        }
        let y = Self::dot(self.vertical, d);
        // Follows the viewport the way the side planes do: a half-height view
        // has half the vertical frustum, and the roof and far floor go with it.
        let half_h = unsafe { VIEW_HALF_H };
        y.abs() - Self::extent(self.vertical, h) <= z * half_h / PROJ_H as i32
    }

    /// Chebyshev distance from the camera on the ground plane, which is what
    /// the tessellation bands key off.
    fn flat_distance(&self, x: i32, z: i32) -> i32 {
        (x - self.pos.0).abs().max((z - self.pos.2).abs())
    }
}

/// Screen offset and projection plane. Call once at boot.
pub fn setup() {
    scene::set_screen_offset((SCREEN_W as i32 / 2) << 16, (SCREEN_H as i32 / 2) << 16);
    scene::set_projection_plane(PROJ_H);
    build_meshes();
    build_spans();
    build_lighting();
    build_car_materials();
}

/// Margin the camera keeps from the walls, so it never clips through one.
// At the camera's ordinary 330-uu height the quarter pipe is already almost
// vertical, so this clears it without crushing the follow distance as badly
// as a full ramp-radius inset would.
const CAM_WALL_MARGIN: i32 = 180;

/// Pull a point inside the arena footprint: side walls, end walls, and the four
/// corner chamfers. Same shape the sim confines the ball with, minus the goals.
fn keep_inside(mut x: i32, mut z: i32) -> (i32, i32) {
    x = x.clamp(
        -(sim::HALF_X - CAM_WALL_MARGIN),
        sim::HALF_X - CAM_WALL_MARGIN,
    );
    z = z.clamp(
        -(sim::HALF_Z - CAM_WALL_MARGIN),
        sim::HALF_Z - CAM_WALL_MARGIN,
    );
    let limit = sim::CORNER - CAM_WALL_MARGIN * 3 / 2;
    let over = x.abs() + z.abs() - limit;
    if over > 0 {
        x -= x.signum() * over / 2;
        z -= z.signum() * over / 2;
    }
    (x, z)
}

/// The chase camera for one player. `subject` is the car this view follows,
/// which is what makes a split game two calls instead of one.
/// `hold_car` is what the ordinary ball cam does: it gives up some of its aim
/// on the ball to keep your own car inside the frame, because a driving camera
/// that loses the car is useless. A celebration wants the opposite -- the car
/// is parked and the thing worth looking at is the ball -- so it asks for the
/// undiluted aim.
fn camera(
    s: &Sim,
    subject: &sim::Car,
    ball_cam: bool,
    hold_car: bool,
    camera_slot: usize,
) -> View {
    let camera_slot = camera_slot.min(1);
    let previous = unsafe { CHASE_CAMERAS[camera_slot] };
    let (_, car_up, car_fwd) = subject.basis();
    let dx = if ball_cam {
        r(s.ball.p.x - subject.p.x)
    } else {
        car_fwd.x
    };
    let dz = if ball_cam {
        r(s.ball.p.z - subject.p.z)
    } else {
        car_fwd.z
    };
    // Close in on the ball and the car-ball line goes wild, so hold the car's
    // own heading until they separate again.
    let close = dx * dx + dz * dz <= CAM_MIN_SEP * CAM_MIN_SEP;
    let follow_yaw = if !ball_cam || close {
        atan2_q12(car_fwd.x, car_fwd.z)
    } else {
        atan2_q12(dx, dz)
    };
    #[cfg(feature = "boot-wheels")]
    // Three-quarter inspection view: exposes the front steer angle and the
    // different front/rear suspension travel in one still.
    let follow_yaw = follow_yaw.wrapping_add(512);
    let (follow_s, follow_c) = (sin_q12(follow_yaw), cos_q12(follow_yaw));
    // A floor car has a full-length horizontal nose vector. On a wall that
    // vector shrinks toward zero, and normalising it to `follow_yaw` must not
    // turn numerical crumbs into a full 800-uu camera relocation. Ball cam is
    // deliberately positioned from the horizontal car-to-ball line instead.
    let follow_flat = if ball_cam {
        4096
    } else {
        isqrt_i32(car_fwd.x * car_fwd.x + car_fwd.z * car_fwd.z).min(4096)
    };
    let flat_trail = (CAM_DIST * follow_flat) >> 12;

    // Behind the car, then dragged back inside the arena. Without this the
    // camera ends up through the back wall at kickoff (the spawn is 4608 out
    // of 5120) and inside the net every time you score.
    // Height follows the surface normal. As that normal rolls away from world
    // up, grow its horizontal contribution to a full camera boom. This moves
    // the eye smoothly into the arena on a wall and preserves its distance
    // from the car without choosing between two lateral camera positions.
    let wall_amount = 4096 - car_up.y.clamp(0, 4096);
    let surface_boom = CAM_HEIGHT + (((CAM_WALL_BOOM - CAM_HEIGHT) * wall_amount) >> 12);
    let desired_x =
        r(subject.p.x) - ((follow_s * flat_trail) >> 12) + ((car_up.x * surface_boom) >> 12);
    let desired_z =
        r(subject.p.z) - ((follow_c * flat_trail) >> 12) + ((car_up.z * surface_boom) >> 12);
    let (mut cx, mut cz) = keep_inside(desired_x, desired_z);
    let (car_x, car_z) = (r(subject.p.x), r(subject.p.z));
    let current_flat = isqrt_i32((car_x - cx) * (car_x - cx) + (car_z - cz) * (car_z - cz));
    if current_flat < CAM_MIN_FLAT_DIST {
        // At the back-middle kickoff there is not enough room directly behind
        // the car for an 800-uu boom. Slide the eye along the wall instead of
        // collapsing almost onto the bumper; the final look-at yaw below
        // keeps the car centred from that offset position. Wall driving gets
        // its distance from the continuous surface-normal boom above, so this
        // branch remains a floor/kickoff fallback instead of firing mid-climb.
        let side = isqrt_i32(CAM_MIN_FLAT_DIST * CAM_MIN_FLAT_DIST - current_flat * current_flat);
        let candidate = |sign: i32| {
            keep_inside(
                cx + sign * ((follow_c * side) >> 12),
                cz - sign * ((follow_s * side) >> 12),
            )
        };
        let a = candidate(1);
        let b = candidate(-1);
        let distance_sq =
            |p: (i32, i32)| (car_x - p.0) * (car_x - p.0) + (car_z - p.1) * (car_z - p.1);
        (cx, cz) = if distance_sq(a) >= distance_sq(b) {
            a
        } else {
            b
        };
    }
    // The part of the trail lost from X/Z becomes vertical while climbing.
    // Render Y is inverted, so a positive world-space nose puts the eye lower
    // on screen-space Y, behind the car rather than above it.
    let vertical_trail = (((car_fwd.y * CAM_WALL_TRAIL) >> 12) * wall_amount) >> 12;
    let car_y = ry(subject.p.y);
    let desired_cyy = car_y + vertical_trail - ((car_up.y * CAM_HEIGHT) >> 12);
    let desired_offset = (cx - car_x, desired_cyy - car_y, cz - car_z);
    let offset = if !previous.valid {
        desired_offset
    } else {
        (
            previous.offset.0
                + (desired_offset.0 - previous.offset.0).clamp(-CAM_OFFSET_STEP, CAM_OFFSET_STEP),
            previous.offset.1
                + (desired_offset.1 - previous.offset.1).clamp(-CAM_OFFSET_STEP, CAM_OFFSET_STEP),
            previous.offset.2
                + (desired_offset.2 - previous.offset.2).clamp(-CAM_OFFSET_STEP, CAM_OFFSET_STEP),
        )
    };
    (cx, cz) = keep_inside(car_x + offset.0, car_z + offset.2);
    let cyy = car_y + offset.1;
    let current_flat = isqrt_i32((car_x - cx) * (car_x - cx) + (car_z - cz) * (car_z - cz));

    // Ball cam primarily aims at the ball. Its vertical aim is softened so a
    // high ball cannot push the car below the short 240-line frame.
    let mut aim_x = if ball_cam {
        r(s.ball.p.x)
    } else {
        r(subject.p.x) + ((car_fwd.x * CAM_CAR_AIM) >> 12)
    };
    let mut aim_z = if ball_cam {
        r(s.ball.p.z)
    } else {
        r(subject.p.z) + ((car_fwd.z * CAM_CAR_AIM) >> 12)
    };
    let mut ay = if ball_cam {
        // Half the ball's rise keeps the car safely inside a 240-line frame.
        ry(subject.p.y) + (ry(s.ball.p.y) - ry(subject.p.y)) / 2
    } else {
        ry(subject.p.y)
            - ((car_up.y * sim::CAR_HALF_H) >> 12)
            - (((car_fwd.y * CAM_CAR_AIM) >> 12) * wall_amount >> 12)
    };
    let mut flat = isqrt_i32((aim_x - cx) * (aim_x - cx) + (aim_z - cz) * (aim_z - cz));
    if ball_cam && flat < CAM_FALLBACK_AIM / 2 {
        // Ball practically on the lens: aim down the car's nose instead.
        aim_x = r(subject.p.x) + ((follow_s * CAM_FALLBACK_AIM) >> 12);
        aim_z = r(subject.p.z) + ((follow_c * CAM_FALLBACK_AIM) >> 12);
        ay = ry(subject.p.y);
        flat = isqrt_i32((aim_x - cx) * (aim_x - cx) + (aim_z - cz) * (aim_z - cz));
    }
    let flat = flat.max(1);
    // atan2 hands back an unsigned turn; fold the top half to a signed tilt so
    // the camera can look up at a ball that is over its head.
    let raw = atan2_q12(ay - cyy, flat) as i32;
    let mut signed = if raw > 2048 { raw - 4096 } else { raw };
    if ball_cam && hold_car {
        let car_flat = current_flat.max(1);
        let car_raw = atan2_q12(ry(subject.p.y) - cyy, car_flat) as i32;
        let car_pitch = if car_raw > 2048 {
            car_raw - 4096
        } else {
            car_raw
        };
        let delta = car_pitch - signed;
        let shift = (delta.abs() - CAM_BALL_CAR_PITCH).max(0);
        signed += delta.signum() * shift;
    }
    let pitch_min =
        CAM_PITCH_MIN + (((CAM_WALL_PITCH_MIN - CAM_PITCH_MIN) * wall_amount) >> 12);
    let desired_pitch = signed.clamp(pitch_min, CAM_PITCH_MAX);
    let mut view_yaw = atan2_q12(aim_x - cx, aim_z - cz);
    if ball_cam && hold_car {
        // The end-wall clamp slides the camera sideways at kickoff to retain a
        // useful boom length. On a 63-degree FOV, the resulting car-to-ball
        // subject angle is wider than either subject's safe screen margin.
        // Bias the view away from the ball only as much as needed to retain
        // the car; once the boom is no longer wall-limited this becomes zero.
        let car_yaw = atan2_q12(car_x - cx, car_z - cz);
        let delta = ((car_yaw as i32 - view_yaw as i32 + 2048).rem_euclid(4096)) - 2048;
        let shift = (delta.abs() - CAM_BALL_CAR_YAW).max(0);
        view_yaw = (view_yaw as i32 + delta.signum() * shift).rem_euclid(4096) as u16;
    }
    let (view_yaw, pitch) = if previous.valid {
        let yaw_delta = ((view_yaw as i32 - previous.yaw as i32 + 2048).rem_euclid(4096)) - 2048;
        (
            (previous.yaw as i32 + yaw_delta.clamp(-CAM_YAW_STEP, CAM_YAW_STEP))
                .rem_euclid(4096) as u16,
            previous.pitch
                + (desired_pitch - previous.pitch).clamp(-CAM_PITCH_STEP, CAM_PITCH_STEP),
        )
    } else {
        (view_yaw, desired_pitch)
    };
    unsafe {
        CHASE_CAMERAS[camera_slot] = CameraState {
            valid: true,
            offset: (cx - car_x, cyy - car_y, cz - car_z),
            yaw: view_yaw,
            pitch,
        };
    }
    look_from(
        (cx, cyy, cz),
        view_yaw,
        pitch.rem_euclid(4096) as u16,
    )
}

/// A camera at `pos` looking along `yaw` with `pitch` below the horizontal.
///
/// Render space is Y down, so a positive pitch tips the view toward the floor.
fn look_from(pos: (i32, i32, i32), yaw: u16, pitch: u16) -> View {
    let (sp, cp) = (sin_q12(pitch), cos_q12(pitch));
    let (sy, cy) = (sin_q12(yaw), cos_q12(yaw));
    // View basis in render space (Y down): forward, right, and their cross.
    let f = [
        ((sy * cp) >> 12) as i16,
        sp as i16,
        ((cy * cp) >> 12) as i16,
    ];
    let rt = [cy as i16, 0, -sy as i16];
    let up = [
        ((-sy * sp) >> 12) as i16,
        cp as i16,
        ((-cy * sp) >> 12) as i16,
    ];
    View {
        v: Mat3I16 { m: [rt, up, f] },
        pos,
    }
}

// ---- meshes ----------------------------------------------------------------

/// The player car, cooked from `assets/*.psxm` by `tools/cook-models`. Fitted
/// there to the widened gameplay hitbox, with its origin on the ground between
/// the wheels.
/// One blob per model. There used to be a blue and an orange cook of each,
/// but the cooker gives the two variants identical geometry and identical
/// colours everywhere except `Role::Body` and `Role::BodyDark` -- which are
/// exactly the two roles the select screen repaints. The orange cook was a
/// second copy of the same car carrying the two bytes that get overwritten.
static CAR_BLOBS: [&[u8]; CAR_COUNT] = [
    include_bytes!("../assets/sedan.psxm"),
    include_bytes!("../assets/hatchback.psxm"),
    include_bytes!("../assets/hatchback2.psxm"),
];

/// The blue body colours the cooker writes, which are the keys the garage
/// repaints. They must match `paint.rs`'s `Role::Body` and `Role::BodyDark`
/// for `Team::Blue` exactly: the remap is a colour match, and a change there
/// that is not mirrored here would silently stop repainting anything.
const BODY_KEY: Rgb = (32, 80, 168);
const BODY_DARK_KEY: Rgb = (16, 34, 80);

/// Paints the select screen offers: name, main body colour, the darker
/// secondary bodywork that goes with it, and the signal colour.
///
/// Body and dark are authored rather than one being a multiply of the other.
/// The lighting clips the strongest channel first, and a dark shade derived by
/// scaling loses the hue on exactly the panels that catch the light.
///
/// The signal colour is the same hue with the ceiling taken off. Body colours
/// are held under about 168 in their strongest channel so a lit panel keeps
/// its hue instead of going white, but the scoreboard block, the goal frame
/// and the goal burst are all drawn flat and want the paint at full strength.
/// The first and sixth entries are the blue and orange this game shipped with,
/// so the default match looks exactly as it did.
pub const PAINTS: [(&str, Rgb, Rgb, Rgb); 8] = [
    ("COBALT", (32, 80, 168), (16, 34, 80), (54, 118, 240)),
    ("SKY", (72, 148, 208), (28, 62, 104), (104, 196, 255)),
    ("TEAL", (24, 132, 124), (10, 56, 54), (32, 196, 184)),
    ("LIME", (108, 168, 40), (44, 72, 16), (150, 232, 56)),
    ("GOLD", (208, 160, 32), (92, 66, 12), (255, 206, 48)),
    ("EMBER", (208, 96, 24), (88, 38, 10), (250, 138, 34)),
    ("CRIMSON", (176, 40, 52), (74, 16, 22), (240, 58, 74)),
    ("VIOLET", (124, 68, 176), (52, 26, 78), (172, 96, 244)),
];

/// Which paint each seat is wearing this match. Set once when the match
/// starts; the HUD, the goal frames and the goal burst all read it, so nothing
/// has to thread a colour through the drawing calls.
static mut SEAT_PAINT: [usize; SEATS] = [0, 5];

/// Each seat's signal colour normalised to a constant total, so it can be used
/// as a tint without changing how bright the surface it tints ends up.
///
/// Cached rather than derived per vertex: the barrier at the foot of the wall
/// is tinted by this at every corner of every quad, and working it out there
/// cost six integer divides a vertex and thirty-eight dropped frames.
static mut SEAT_HUE: [Rgb; SEATS] = [(128, 128, 128); SEATS];
/// Whether the barrier has had a colour laid over it yet. The defaults in
/// `SEAT_PAINT` are a real selection, so without this the first call matching
/// them would decide there was nothing to do and leave the barrier grey.
static mut CURB_PAINTED: bool = false;

/// Tell the renderer which paints the two seats picked.
pub fn set_seat_paints(paints: [usize; SEATS]) {
    let want = [
        paints[0].min(PAINT_COUNT - 1),
        paints[1].min(PAINT_COUNT - 1),
    ];
    // Idempotent, so the front end can hand this its current selection every
    // frame and only pay for it on the frame the selection moved.
    if unsafe { SEAT_PAINT } == want && unsafe { CURB_PAINTED } {
        return;
    }
    unsafe {
        SEAT_PAINT = want;
        CURB_PAINTED = true;
    }
    for seat in 0..SEATS {
        let c = seat_signal(seat);
        let sum = (c.0 as i32 + c.1 as i32 + c.2 as i32).max(1);
        // Pulled back toward neutral. At full strength the normalised hue
        // halves the red channel on the blue half, which reads as one end of
        // the arena being in shadow rather than being blue.
        let full = (
            (c.0 as i32 * 384 / sum).min(255) as u8,
            (c.1 as i32 * 384 / sum).min(255) as u8,
            (c.2 as i32 * 384 / sum).min(255) as u8,
        );
        unsafe { SEAT_HUE[seat] = mix((128, 128, 128), full, 10) };
    }
    paint_curb();
}

/// The flat colour that stands for a seat away from its car: scoreboard block,
/// goal frame, goal burst.
pub fn seat_signal(seat: usize) -> Rgb {
    PAINTS[unsafe { SEAT_PAINT[seat.min(SEATS - 1)] }].3
}

/// How many paints the garage cycles through.
pub const PAINT_COUNT: usize = PAINTS.len();

/// Working per-vertex colours for the car the player drives, one table per
/// level of detail. The base tables stay untouched, so a repaint is a scan of
/// one table rather than a rebuild, and nothing is baked per colour: eight
/// paints across three cars and two LODs would be ninety-odd KiB of tables to
/// say what one scan says.
/// One per seat, because both cars are now repainted from the same blob.
static mut PAINTED_GAME: [[Rgb; CAR_MAX_VERTS]; SEATS] = [[(128, 128, 128); CAR_MAX_VERTS]; SEATS];
/// Which (car, paint) each seat's working tables currently hold, so a frame
/// that changes nothing does no work.
static mut PAINTED_FOR: [Option<(usize, usize)>; SEATS] = [None; SEATS];

/// Repaint the working tables for `car` in `paint`, if they are not already.
///
/// Only the two body roles move. Glass, tyres, rims, lamps, bumper, grille and
/// chassis keep the colours the cooker gave them, which is what stops a car
/// turning into one flat silhouette.
pub fn set_appearance(seat: usize, car: usize, paint: usize) {
    let seat = seat.min(SEATS - 1);
    if unsafe { PAINTED_FOR[seat] } == Some((car, paint)) {
        return;
    }
    let which = car.min(CAR_COUNT - 1);
    let (_, body, dark, _) = PAINTS[paint.min(PAINT_COUNT - 1)];
    let repaint = |src: &[Rgb; CAR_MAX_VERTS], dst: &mut [Rgb; CAR_MAX_VERTS]| {
        for (out, &base) in dst.iter_mut().zip(src.iter()) {
            *out = if base == BODY_KEY {
                body
            } else if base == BODY_DARK_KEY {
                dark
            } else {
                base
            };
        }
    };
    unsafe {
        repaint(&CAR_MATERIALS[which], &mut PAINTED_GAME[seat]);
        PAINTED_FOR[seat] = Some((car, paint));
    }
}

/// Per-vertex wheel-corner assignments for the gameplay LODs.
///
/// Slots are rear-left, rear-right, front-left, front-right; `255` is rigid
/// bodywork. Geometry is identical between team variants, so the blue maps
/// serve both cars.
static CAR_WHEELS: [&[u8]; CAR_COUNT] = [
    include_bytes!("../assets/sedan.psxw"),
    include_bytes!("../assets/hatchback.psxw"),
    include_bytes!("../assets/hatchback2.psxw"),
];
const WHEEL_NONE: u8 = u8::MAX;

/// Seats, in the order the sim keeps them. Seat 0 defends -Z and is player
/// one; seat 1 defends +Z and is either player two or the AI.
pub const SEATS: usize = 2;

/// How many cars the select screen cycles through.
pub const CAR_COUNT: usize = 3;

/// Names, in the same order, for the select screen to label them with.
pub const CAR_NAMES: [&str; CAR_COUNT] = ["COMET", "HATCH", "SPRINTER"];

/// Stadium light rig, in **render** space, so its Y points down like the rest
/// of the renderer.
///
/// Directional again. The cars used to arrive with ambient occlusion baked
/// into their face colours, which meant this rig had to stay nearly flat or it
/// would light an already-lit model. That bake cost 6.8x on render time, so it
/// is gone, and the shading is the GTE's per-vertex job again. Declaring it Y-up and pushing it through the view matrix
/// lights the underside of everything and leaves the roofs black.
///
/// Directions point FROM the surface TOWARD the lamp. Rotated into camera
/// space once a frame, then into each object's local frame by the engine.
const LIGHTS: LightRig = LightRig::new(
    [
        // Key: high, and a little toward the blue end.
        Light {
            direction: Vec3I16::new(0x0400, -0x0E00, -0x0500),
            colour: (0x1200, 0x1100, 0x0F00),
        },
        // Fill: cool, from the opposite side, keeps unlit flanks readable.
        Light {
            direction: Vec3I16::new(-0x0A00, -0x0600, 0x0400),
            colour: (0x0700, 0x0800, 0x0B00),
        },
        Light::OFF,
    ],
    // Ambient, lifted hard. These models are authored for a modern lit
    // renderer with exposure; the PS1 has none, and the sedan's body bakes to
    // a linear near-black. Over-bright light values are legal here, the GTE
    // clamps at the MAC stage.
    (0x0700, 0x0700, 0x0900),
);

/// The key light's direction in render space (Y down), for the ball, which is
/// procedural and shades itself rather than going through the GTE rig. Mirrors
/// `LIGHTS`' first entry with Y flipped, so the two agree on where the sun is.
const BALL_LIGHT: (i32, i32, i32) = (0x0400, -0x0E00, -0x0500);

/// Triangle budget for both cars together. The garage always pairs the chosen
/// model with the one two slots ahead; the heaviest prepared pair is 1189.
const CAR_TRI_CAP: usize = 1248;
/// Faces held per decoded gameplay model. Matches the triangle arena, which is
/// already sized for the worst car in the library.
const CAR_FACE_CAP: usize = CAR_TRI_CAP;
/// Projected-vertex scratch. Undersize this and `project_car` quietly truncates
/// the car, so the cook tests check every committed blob against it.
const CAR_VERT_CAP: usize = 1344;

const EMPTY_LIT: ProjectedLit = ProjectedLit {
    sx: 0,
    sy: 0,
    sz: 0,
    r: 0,
    g: 0,
    b: 0,
};

/// Bounding half-extent for the whole-car frustum test, on every axis.
///
/// Isotropic because the car rolls: the bound has to hold at any orientation,
/// so it is the hitbox's half-diagonal, `sqrt(82^2 + 58^2 + 26^2)` = 104,
/// rounded up for the bodywork that overhangs the collision box.
const CAR_BOUND_R: i32 = 128;

static mut CAR_TRIS: [TriGouraud; CAR_TRI_CAP] =
    [const { TriGouraud::new([(0, 0); 3], [(0, 0, 0); 3]) }; CAR_TRI_CAP];
static mut CAR_PROJ: [ProjectedLit; CAR_VERT_CAP] = [EMPTY_LIT; CAR_VERT_CAP];
/// Four authored wheel pivots per selectable gameplay car, derived once from
/// the `.psxw` vertex groups while the loading screen is up.
static mut CAR_WHEEL_CENTRES: [[Vec3I16; 4]; CAR_COUNT] = [[Vec3I16::ZERO; 4]; CAR_COUNT];

/// Largest vertex table across the prepared gameplay and menu asset library,
/// with slack.
///
/// The cooker now splits welded vertices at material boundaries, so glass,
/// tyres, and lights retain their own colours instead of inheriting body paint.
const CAR_MAX_VERTS: usize = 1344;
/// More than the largest selectable menu car.

/// Each car model's per-vertex material colour, resolved once at boot.
///
/// `GouraudRenderPass::submit_lit_mesh` resolves this itself, and the way it
/// does it is the single most expensive thing in the renderer: for every
/// vertex it scans the face table from the start looking for a face that uses
/// it, which is O(verts x faces). On these meshes that is about twenty
/// thousand index decodes per frame per pair of cars, and it measured as 1.5M
/// of a 2.2M-cycle frame.
///
/// The mapping is a property of the cooked blob and never changes, so it is
/// built once here and `project_cars` feeds `submit_projected_mesh` instead.
/// Walking faces in order and keeping the first colour to claim each vertex
/// reproduces the engine's forward scan exactly, so the pixels are identical.
static mut CAR_MATERIALS: [[(u8, u8, u8); CAR_MAX_VERTS]; CAR_COUNT] =
    [[(128, 128, 128); CAR_MAX_VERTS]; CAR_COUNT];

/// Resolve one blob's per-vertex colours into `out`.
fn car_materials_for(blob: &[u8], out: &mut [(u8, u8, u8); CAR_MAX_VERTS]) {
    let Ok(mesh) = Mesh::from_bytes(blob) else {
        return;
    };
    let verts = (mesh.vert_count() as usize).min(CAR_MAX_VERTS);
    let mut claimed = [false; CAR_MAX_VERTS];
    for f in 0..mesh.face_count() {
        let Some(colour) = mesh.face_color(f) else {
            break;
        };
        let (a, b, c) = mesh.face(f);
        for v in [a, b, c] {
            let v = v as usize;
            if v < verts && !claimed[v] {
                claimed[v] = true;
                out[v] = colour;
            }
        }
    }
}

/// Find one pivot per wheel corner from the final cooked gameplay vertices.
///
/// Using the bounding-box centre rather than an average makes the pivot
/// independent of tessellation density: a rim with six vertices and a tyre
/// with twelve still rotate about the same axle.
fn build_car_wheel_centres() {
    for which in 0..CAR_COUNT {
        let Ok(mesh) = Mesh::from_bytes(CAR_BLOBS[which]) else {
            continue;
        };
        let slots = CAR_WHEELS[which];
        let mut minimum = [[i32::MAX; 3]; 4];
        let mut maximum = [[i32::MIN; 3]; 4];
        let mut found = [false; 4];
        let count = (mesh.vert_count() as usize).min(slots.len());
        for vertex in 0..count {
            let slot = slots[vertex];
            if slot == WHEEL_NONE || slot >= 4 {
                continue;
            }
            let slot = slot as usize;
            let p = mesh.vertex(vertex as u16);
            let values = [p.x as i32, p.y as i32, p.z as i32];
            for axis in 0..3 {
                minimum[slot][axis] = minimum[slot][axis].min(values[axis]);
                maximum[slot][axis] = maximum[slot][axis].max(values[axis]);
            }
            found[slot] = true;
        }
        for slot in 0..4 {
            if !found[slot] {
                continue;
            }
            unsafe {
                CAR_WHEEL_CENTRES[which][slot] = Vec3I16::new(
                    ((minimum[slot][0] + maximum[slot][0]) / 2) as i16,
                    ((minimum[slot][1] + maximum[slot][1]) / 2) as i16,
                    ((minimum[slot][2] + maximum[slot][2]) / 2) as i16,
                );
            }
        }
    }
}

/// Gameplay car geometry, decoded once at boot.
///
/// `Mesh`'s accessors rebuild every component out of unaligned bytes: six
/// `lbu` and their shifts per vertex, the same again per normal, each behind
/// its own bounds check, and the normal wrapped in an `Option`. That decode
/// ran per vertex per car per view, which a split frame does four times, and
/// it measured as most of the 202 cycles a vertex the projection stage was
/// spending. The blobs are static for the life of the program, so it is paid
/// once here and the hot loop reads aligned arrays.
///
/// Cost is 95 KiB of `.bss` against a 54 KiB starting footprint and most of
/// two megabytes free, which is the trade this hardware wants: the RAM is
/// sitting there and the cycles are not.
static mut CAR_VERTS: [[Vec3I16; CAR_VERT_CAP]; CAR_COUNT] =
    [[Vec3I16::ZERO; CAR_VERT_CAP]; CAR_COUNT];
static mut CAR_NORMALS: [[Vec3I16; CAR_VERT_CAP]; CAR_COUNT] =
    [[Vec3I16::ZERO; CAR_VERT_CAP]; CAR_COUNT];
/// How many of those entries each model actually filled.
static mut CAR_VERT_COUNT: [u16; CAR_COUNT] = [0; CAR_COUNT];
/// Triangle indices, decoded the same way and for the same reason: `Mesh::face`
/// rebuilds three `u16` from six `lbu` behind a stride branch, once per face
/// per car per view.
static mut CAR_FACES: [[[u16; 3]; CAR_FACE_CAP]; CAR_COUNT] = [[[0; 3]; CAR_FACE_CAP]; CAR_COUNT];
/// Depth-sorted face keys for one car draw (`submit_car_faces`).
static mut CAR_SORT_KEYS: [u32; CAR_FACE_CAP] = [0; CAR_FACE_CAP];
static mut CAR_FACE_COUNT: [u16; CAR_COUNT] = [0; CAR_COUNT];

/// Decode one gameplay car's vertices and normals into the aligned tables.
fn decode_car_geometry(blob: &[u8], which: usize) {
    let Ok(mesh) = Mesh::from_bytes(blob) else {
        return;
    };
    let count = (mesh.vert_count() as usize).min(CAR_VERT_CAP);
    for i in 0..count {
        unsafe {
            CAR_VERTS[which][i] = mesh.vertex(i as u16);
            CAR_NORMALS[which][i] = mesh.vertex_normal(i as u16).unwrap_or(Vec3I16::ZERO);
        }
    }
    unsafe { CAR_VERT_COUNT[which] = count as u16 };

    // Faces are dropped here if any index reaches past the vertices we kept,
    // so the hot loop needs no bounds check of its own.
    let mut kept = 0usize;
    for f in 0..(mesh.face_count() as usize).min(CAR_FACE_CAP) {
        let (ia, ib, ic) = mesh.face(f as u16);
        if ia as usize >= count || ib as usize >= count || ic as usize >= count {
            continue;
        }
        unsafe { CAR_FACES[which][kept] = [ia, ib, ic] };
        kept += 1;
    }
    unsafe { CAR_FACE_COUNT[which] = kept as u16 };
}

fn build_car_materials() {
    for (ci, blob) in CAR_BLOBS.iter().enumerate() {
        car_materials_for(blob, unsafe { &mut CAR_MATERIALS[ci] });
        decode_car_geometry(blob, ci);
    }
    build_car_wheel_centres();
}

/// Local transforms shared by every vertex in a front or rear wheel group.
struct WheelPose {
    front: Mat3I16,
    rear: Mat3I16,
    /// Front/rear travel in whole object-space uu.
    travel: [i16; 2],
}

/// Decode one mesh vertex and apply its wheel's local steering, roll, and
/// suspension transform. Rigid body vertices take the fast identity branch.
fn animated_car_vertex(
    position: Vec3I16,
    normal: Vec3I16,
    slot: u8,
    centres: &[Vec3I16; 4],
    pose: &WheelPose,
) -> (Vec3I16, Vec3I16) {
    if slot == WHEEL_NONE || slot >= 4 {
        return (position, normal);
    }

    let slot = slot as usize;
    let axle = if slot >= 2 { 0 } else { 1 };
    let rotation = if axle == 0 { &pose.front } else { &pose.rear };
    let centre = centres[slot];
    let local = (
        position.x as i32 - centre.x as i32,
        position.y as i32 - centre.y as i32,
        position.z as i32 - centre.z as i32,
    );
    let turned = apply(rotation, local);
    let lit_normal = apply(
        rotation,
        (normal.x as i32, normal.y as i32, normal.z as i32),
    );
    let narrow = |value: i32| value.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    (
        Vec3I16::new(
            narrow(centre.x as i32 + turned.0),
            narrow(centre.y as i32 + turned.1 + pose.travel[axle] as i32),
            narrow(centre.z as i32 + turned.2),
        ),
        Vec3I16::new(
            narrow(lit_normal.0),
            narrow(lit_normal.1),
            narrow(lit_normal.2),
        ),
    )
}

/// Is a projected triangle wound clockwise on screen, and so facing away?
///
/// The engine's own test is private, and this is one cross product.
#[inline]
fn car_back_facing(a: &ProjectedLit, b: &ProjectedLit, c: &ProjectedLit) -> bool {
    let abx = b.sx as i32 - a.sx as i32;
    let aby = b.sy as i32 - a.sy as i32;
    let acx = c.sx as i32 - a.sx as i32;
    let acy = c.sy as i32 - a.sy as i32;
    abx * acy - aby * acx <= 0
}

/// Build one car's triangles and hang them off the ordering table.
///
/// Replaces the engine's `submit_projected_mesh`, which keeps a parallel
/// command list and insertion-sorts every triangle into its slot by exact
/// depth. That sort is quadratic in the triangles sharing a slot, and a
/// gameplay car is small enough that its whole depth spread lands in a
/// handful of them, so the cost rose as the car got smaller and further away
/// -- exactly backwards. Prepending into the slot, the way every arena quad
/// in this file already does, is one write. The ordering it gives up is
/// between triangles that were already inside one slot of each other on a
/// model thirty pixels tall.
/// Distance past which a half-width view stops paying for a car's full mesh.
/// At 2,800 uu on the 260-plane projection a car is a dozen pixels wide, and
/// the mesh pass costs the same ~83k cycles it does filling the screen. The
/// split-screen kickoff draws two of them, one per view, and that is most of
/// why kickoff frames blew the two-vblank budget.
const FAR_CAR_DISTANCE: i32 = 2200;

/// A distant car in a half-width view: two screen-space slabs, a dark
/// running-gear band under a body-paint block, sorted at the car's own depth.
/// Eight projected corners and four flat tris against ~350 lit GTE vertices
/// and ~200 faces for the real mesh.
#[inline(never)]
fn draw_far_car(
    seat: usize,
    body: &sim::Car,
    view: &View,
    tris: &mut PrimitiveArena<'_, TriGouraud>,
    ot: &mut OtFrame<'_, OT_DEPTH>,
) {
    let t = view.camera_space(car_ground(body));
    if t.2 < DEPTH_RANGE.near() as i32 {
        return;
    }
    let cx = unsafe { (VIEW_MIN_X + VIEW_MAX_X) as i32 / 2 };
    let sx = cx + t.0 * PROJ_H as i32 / t.2;
    let sy = (SCREEN_H as i32 / 2) + t.1 * PROJ_H as i32 / t.2;
    // ponytail: one isotropic half-extent between the car's width and length;
    // at a dozen pixels the yaw-correct footprint is a two-pixel refinement.
    let hw = ((sim::CAR_HALF_W + sim::CAR_HALF_L) / 2 * PROJ_H as i32 / t.2).max(1);
    let h = |uu: i32| (uu * PROJ_H as i32 / t.2).max(1);
    let paint = PAINTS[unsafe { SEAT_PAINT }[seat].min(PAINT_COUNT - 1)];
    let (body_col, dark_col) = (paint.1, paint.2);
    let mut slab = |y0: i32, y1: i32, top: Rgb, bot: Rgb| {
        let (x0, x1) = ((sx - hw) as i16, (sx + hw) as i16);
        let (y0, y1) = (y0 as i16, y1 as i16);
        for prim in [
            TriGouraud::new([(x0, y0), (x1, y0), (x0, y1)], [top, top, bot]),
            TriGouraud::new([(x1, y0), (x1, y1), (x0, y1)], [top, bot, bot]),
        ] {
            if let Some(p) = tris.push(prim) {
                ot.add_packet_depth(DEPTH_RANGE, t.2, p);
            }
        }
    };
    // Wheels and shadowed underside, then the painted body above them.
    slab(sy - h(22), sy, dark_col, (12, 12, 16));
    slab(sy - h(52), sy - h(22), body_col, dark_col);
}

fn submit_car_faces(
    faces: &[[u16; 3]],
    projected: &[ProjectedLit],
    tris: &mut PrimitiveArena<'_, TriGouraud>,
    ot: &mut OtFrame<'_, OT_DEPTH>,
) {
    // The ordering table has 512 slots over the arena's 14,000 uu of depth,
    // about 27 uu a slot, and a car is 120 uu long: its faces land in four
    // or five slots, and within a slot they drew in mesh order, which is why
    // windows came through the roof and wheels through the sills. Sort the
    // front faces by depth first and insert them nearest first: the table
    // prepends, so within one slot the last insert draws first and the far
    // face goes down before the near one. Exact painter's order inside the
    // car, and the slot spread still orders it against the world.
    // Off the stack: the cap is sized for the detailed front-end car.
    let keys = unsafe { &mut CAR_SORT_KEYS };
    let mut n = 0usize;
    for (i, face) in faces.iter().enumerate().take(CAR_FACE_CAP) {
        let a = &projected[face[0] as usize];
        let b = &projected[face[1] as usize];
        let c = &projected[face[2] as usize];
        if car_back_facing(a, b, c) {
            continue;
        }
        let depth = ((a.sz as i32 + b.sz as i32 + c.sz as i32) / 3).clamp(0, 0xffff) as u32;
        keys[n] = (depth << 16) | i as u32;
        n += 1;
    }
    keys[..n].sort_unstable();
    for &key in &keys[..n] {
        let face = &faces[(key & 0xffff) as usize];
        let a = &projected[face[0] as usize];
        let b = &projected[face[1] as usize];
        let c = &projected[face[2] as usize];
        let depth = (key >> 16) as i32;
        let prim = TriGouraud::new(
            [(a.sx, a.sy), (b.sx, b.sy), (c.sx, c.sy)],
            [(a.r, a.g, a.b), (b.r, b.g, b.b), (c.r, c.g, c.b)],
        );
        if let Some(t) = tris.push(prim) {
            ot.add_packet_depth(DEPTH_RANGE, depth, t);
        } else {
            return;
        }
    }
}

/// Project and light one car mesh into `CAR_PROJ`, returning how many vertices
/// landed there. The batched GTE path (RTPT + NCCT for a run of three) is the
/// same one the engine uses, so this is the engine's `submit_lit_mesh` with
/// the material scan replaced by a table lookup, and the authored wheel groups
/// posed on the way through.
fn project_car_animated(
    which: usize,
    materials: &[(u8, u8, u8); CAR_MAX_VERTS],
    slots: &[u8],
    centres: &[Vec3I16; 4],
    pose: &WheelPose,
) -> usize {
    // Aligned tables decoded at boot by `decode_car_geometry`, not the mesh
    // blob: the byte-at-a-time decode was most of this stage's cost.
    let verts = unsafe { &CAR_VERTS[which] };
    let normals = unsafe { &CAR_NORMALS[which] };
    let count = (unsafe { CAR_VERT_COUNT[which] } as usize)
        .min(CAR_VERT_CAP)
        .min(CAR_MAX_VERTS)
        .min(slots.len());
    let proj = unsafe { &mut CAR_PROJ };
    let mut vi = 0;
    while vi + 2 < count {
        let a = animated_car_vertex(verts[vi], normals[vi], slots[vi], centres, pose);
        let b = animated_car_vertex(verts[vi + 1], normals[vi + 1], slots[vi + 1], centres, pose);
        let c = animated_car_vertex(verts[vi + 2], normals[vi + 2], slots[vi + 2], centres, pose);
        let out = project_lit_triangle(
            [a.0, b.0, c.0],
            [a.1, b.1, c.1],
            [materials[vi], materials[vi + 1], materials[vi + 2]],
        );
        proj[vi] = out[0];
        proj[vi + 1] = out[1];
        proj[vi + 2] = out[2];
        vi += 3;
    }
    while vi < count {
        let (position, normal) =
            animated_car_vertex(verts[vi], normals[vi], slots[vi], centres, pose);
        proj[vi] = project_lit(position, normal, materials[vi]);
        vi += 1;
    }
    count
}

/// Ball tessellation. 16 by 5 rather than 12 by 4: at 12 you can count the
/// flats around the silhouette, which is the first thing that reads as cheap
/// on a shape everyone knows is round.
///
/// A sphere is convex, so any facet whose outward normal points away from the
/// camera is behind the ones that do not, guaranteed, with no sorting needed
/// to prove it. Culling those pays for part of the extra detail but not all
/// of it: the stage went 32.5k cycles a visual frame to 46.4k. The cull's own
/// share of that is about a tenth, because the cost here is dominated by
/// projecting the shared vertices, which happens whichever facets survive.
/// 16 by 6 also fits, but tipped 2 frames in 270 past the deadline.
const BALL_LON: usize = 16;
const BALL_LAT: usize = 5;

/// Ball sphere, built at boot so the sin/cos pairs are paid once.
static mut BALL_MESH: [[(i32, i32, i32); BALL_LON]; BALL_LAT + 1] =
    [[(0, 0, 0); BALL_LON]; BALL_LAT + 1];
/// The swept cross-section, built once. It used to be rebuilt inside every
/// span, paying for every quarter-circle sample twenty-four times a frame.
static mut WALL_PROFILE: [(i32, i32); PROFILE_LEN] = [(0, 0); PROFILE_LEN];
/// V coordinate at each wall-profile ring, measured along the surface from
/// the upper rail. This preserves one world-space scale through the straight
/// wall and the roof curve instead of restarting a texture at every band.
static mut COVER_PROFILE_V: [u8; PROFILE_LEN] = [0; PROFILE_LEN];

fn build_meshes() {
    for j in 0..=BALL_LAT {
        let lat = -1024 + (2048 * j as i32) / BALL_LAT as i32;
        let y = (sin_q12(lat as u16) * sim::BALL_R) >> 12;
        let ring = (cos_q12(lat as u16) * sim::BALL_R) >> 12;
        for i in 0..BALL_LON {
            let lon = (4096 * i as i32) / BALL_LON as i32;
            unsafe {
                BALL_MESH[j][i] = (
                    (sin_q12(lon as u16) * ring) >> 12,
                    y,
                    (cos_q12(lon as u16) * ring) >> 12,
                );
            }
        }
    }
    let profile = Builder::profile();
    let mut cover_v = [0u8; PROFILE_LEN];
    let mut distance = 0;
    for i in RAIL_HI_RING + 1..PROFILE_LEN {
        let (a, b) = (profile[i - 1], profile[i]);
        let (dx, dy) = (b.0 - a.0, b.1 - a.1);
        distance += isqrt_i32(dx * dx + dy * dy);
        cover_v[i] = cover_texels(distance).min(COVER_H as u8);
    }
    unsafe {
        WALL_PROFILE = profile;
        COVER_PROFILE_V = cover_v;
    }
}

// ---- builder ---------------------------------------------------------------

/// Quads offered to the builder and quads that survived to a packet, so a
/// profile can say how much of the arena's cost is spent on geometry that is
/// then thrown away. Counted only under `profile`.
#[cfg(feature = "profile")]
static mut QUADS_OFFERED: u32 = 0;
#[cfg(feature = "profile")]
static mut QUADS_KEPT: u32 = 0;

macro_rules! count_offered {
    () => {
        #[cfg(feature = "profile")]
        unsafe {
            QUADS_OFFERED += 1
        };
    };
}
/// Primitives an arena refused because it was full. Silent until now: a full
/// arena drops the rest of the frame's geometry and looks exactly like a
/// culling bug, which is the wrong thing to go looking for.
#[cfg(feature = "profile")]
static mut QUADS_OVERFLOW: u32 = 0;
macro_rules! count_overflow {
    () => {
        #[cfg(feature = "profile")]
        unsafe {
            QUADS_OVERFLOW += 1
        };
    };
}
macro_rules! count_kept {
    () => {
        #[cfg(feature = "profile")]
        unsafe {
            QUADS_KEPT += 1
        };
    };
}

struct Builder<'a> {
    ot: OtFrame<'a, OT_DEPTH>,
    arena: PrimitiveArena<'a, QuadGouraud>,
    textured: PrimitiveArena<'a, QuadTexturedGouraud>,
}

impl Builder<'_> {
    /// Project four object-space corners (PS1 Z-order) and file the quad at its
    /// average depth. Drops quads with a vertex behind the camera and quads
    /// entirely off screen.
    fn quad(&mut self, corners: [(i32, i32, i32); 4], colors: [Rgb; 4]) {
        self.quad_biased(corners, colors, 0);
    }

    fn quad_flat(&mut self, corners: [(i32, i32, i32); 4], color: Rgb) {
        self.quad_biased(corners, [color; 4], 0);
    }

    /// As [`Builder::quad`], but files the quad `bias` deeper. Used by the
    /// shadows, which are nearly coplanar with what casts them.
    fn quad_biased(&mut self, corners: [(i32, i32, i32); 4], colors: [Rgb; 4], bias: i32) {
        count_offered!();
        let mut sp = [(0i16, 0i16); 4];
        let mut z_sum = 0i32;
        for (k, &(x, y, z)) in corners.iter().enumerate() {
            let p = scene::project_vertex(Vec3I16::new(x as i16, y as i16, z as i16));
            if p.sz == 0 {
                return;
            }
            sp[k] = (p.sx, p.sy);
            z_sum += p.sz as i32;
        }
        if !quad_overlaps_view(&sp) {
            return;
        }
        count_kept!();
        self.emit(sp, z_sum / 4 + bias, colors);
    }

    /// As [`Builder::quad_biased`], but semi-transparent: the GPU averages the
    /// quad with what is behind it. What the boost plume is made of.
    fn quad_blended(&mut self, corners: [(i32, i32, i32); 4], colors: [Rgb; 4], bias: i32) {
        let mut sp = [(0i16, 0i16); 4];
        let mut z_sum = 0i32;
        for (k, &(x, y, z)) in corners.iter().enumerate() {
            let p = scene::project_vertex(Vec3I16::new(x as i16, y as i16, z as i16));
            if p.sz == 0 {
                return;
            }
            sp[k] = (p.sx, p.sy);
            z_sum += p.sz as i32;
        }
        if !quad_overlaps_view(&sp) {
            return;
        }
        self.emit_blended(sp, z_sum / 4 + bias, colors);
    }

    /// A textured quad, projected the same way as [`Builder::quad`], with a
    /// tint per corner. The GPU multiplies the texture by the interpolated
    /// tint and treats 128 as 1.0, so this is where the arena's lighting
    /// lands: four table lookups a quad and the gradient is the hardware's
    /// problem. A flat tint per quad would have cost four fewer words, and
    /// would have drawn the pitch falloff as a staircase of tile-sized steps.
    #[allow(clippy::too_many_arguments)]
    fn quad_tex(
        &mut self,
        corners: [(i32, i32, i32); 4],
        uvs: [u16; 4],
        tints: [Rgb; 4],
        bias: i32,
        packet: TexturedGouraudPacketMaterial,
        blended: bool,
    ) {
        count_offered!();
        let mut sp = [(0i16, 0i16); 4];
        let mut z_sum = 0i32;
        for (k, &(x, y, z)) in corners.iter().enumerate() {
            let p = scene::project_vertex(Vec3I16::new(x as i16, y as i16, z as i16));
            if p.sz == 0 {
                return;
            }
            sp[k] = (p.sx, p.sy);
            z_sum += p.sz as i32;
        }
        if !quad_overlaps_view(&sp) {
            return;
        }
        count_kept!();
        self.quad_tex_projected(sp, z_sum, uvs, tints, bias, packet, blended);
    }

    /// The emit half of [`Builder::quad_tex`], for callers that projected the
    /// corners themselves. The floor grid projects each shared corner once
    /// instead of once per quad, which is most of what the pitch used to cost.
    #[allow(clippy::too_many_arguments)]
    fn quad_tex_projected(
        &mut self,
        sp: [(i16, i16); 4],
        z_sum: i32,
        uvs: [u16; 4],
        tints: [Rgb; 4],
        bias: i32,
        packet: TexturedGouraudPacketMaterial,
        blended: bool,
    ) {
        let mut prim =
            QuadTexturedGouraud::with_packet_material_packed_uv_words(sp, uvs, tints, packet);
        if blended {
            prim.color0_cmd |= SEMI_TRANSPARENT;
        }
        if let Some(q) = self.textured.push(prim) {
            self.ot.add_packet_depth(DEPTH_RANGE, z_sum / 4 + bias, q);
        } else {
            count_overflow!();
        }
    }

    fn emit(&mut self, sp: [(i16, i16); 4], depth: i32, colors: [Rgb; 4]) {
        if let Some(q) = self.arena.push(QuadGouraud::new(sp, colors)) {
            self.ot.add_packet_depth(DEPTH_RANGE, depth, q);
        } else {
            count_overflow!();
        }
    }

    /// [`Self::emit`] with the semi-transparent command bit set.
    ///
    /// The arena's draw mode already selects the GPU's average blend, since
    /// `BlendMode::Opaque` and `BlendMode::Average` share tpage bits 0. So the
    /// bit alone is the whole difference: the primitive becomes
    /// `(pitch + shadow) / 2` and nothing about the draw mode has to change,
    /// which matters because changing it mid-table would need a second
    /// material command in the ordering table.
    fn emit_blended(&mut self, sp: [(i16, i16); 4], depth: i32, colors: [Rgb; 4]) {
        count_offered!();
        let mut quad = QuadGouraud::new(sp, colors);
        quad.color0_cmd |= SEMI_TRANSPARENT;
        if let Some(q) = self.arena.push(quad) {
            count_kept!();
            self.ot.add_packet_depth(DEPTH_RANGE, depth, q);
        }
    }

    fn screen_quad(&mut self, slot: usize, rect: [(i16, i16); 4], colors: [Rgb; 4]) {
        if let Some(q) = self.arena.push(QuadGouraud::new(rect, colors)) {
            self.ot.add_packet(slot, q);
        }
    }

    // ---- arena ---------------------------------------------------------

    /// Pull a floor point inside the corner chamfer, so the pitch ends exactly
    /// where the angled wall starts instead of poking through it.
    fn chamfer(x: i32, z: i32) -> (i32, i32) {
        // Stop the flat pitch where the swept wall picks it up.
        //
        // The sweep's first profile point is `(RAMP_R, 0)`: it leaves the
        // floor a ramp radius in from the wall and curves up from there. The
        // pitch used to be drawn out to the wall line regardless, so its outer
        // ramp-radius of tiles lay underneath the curve, and with no z-buffer
        // the two fought for the same pixels a slot at a time. That is the
        // sawtooth along the floor-to-wall join.
        //
        // In front of a goal mouth there is no wall to sweep up into, so the
        // pitch has to run all the way to the line or a strip of nothing
        // appears where the ball goes in.
        let foot_x = sim::HALF_X - RAMP_R;
        let foot_z = if x.abs() < sim::GOAL_HALF_W {
            sim::HALF_Z
        } else {
            sim::HALF_Z - RAMP_R
        };
        let (x, z) = (x.clamp(-foot_x, foot_x), z.clamp(-foot_z, foot_z));

        // The corner planes take the same radius off, measured along their own
        // normal, which is what keeps the join continuous round the chamfer
        // instead of stepping at the two places it meets the straight walls.
        let limit = sim::CORNER - (RAMP_R * 5793 >> 12); // RAMP_R * sqrt(2)
        let sum = x.abs() + z.abs();
        if sum <= limit {
            (x, z)
        } else {
            (x * limit / sum, z * limit / sum)
        }
    }

    /// How many ways to split a floor tile at this distance from the camera.
    ///
    /// The PS1 interpolates texture coordinates affinely, with no perspective
    /// divide per pixel, so a big textured quad seen at a glancing angle bends
    /// its texture along the diagonal. There is no hardware fix; the fix is
    /// more vertices, because the error is bounded by how far a single polygon
    /// spans in screen space.
    ///
    /// wipEout does this by authoring rather than at runtime: its track is a
    /// ribbon of many small quads, each mapping one whole texture tile, drawn
    /// per section with distance culling (`track_draw_section` in
    /// phoboslab's reimplementation). A fixed pitch is the right call for a
    /// track you always see from the same height and angle. This arena is a
    /// single open floor seen from a camera that roams it, so the same idea
    /// applies per tile instead: dense where you are, coarse where you are not.
    ///
    /// PSoXide's own world pass does the screen-space version of this for
    /// models, splitting any textured triangle whose projected edge exceeds
    /// `WorldSurfaceOptions::textured_split_max_edge`. These floor quads are
    /// built by hand and never go through it, hence doing it here.
    fn floor_split(distance: i32) -> i32 {
        // The near band is where the camera sits and where the near-plane
        // clipper does most of its work. Keep a four-way core in a half-width
        // view too: a two-way cell directly under the eye exceeds the PS1
        // rasteriser's safe screen extent and disappears even though its
        // wireframe edges still reach the screen. Past that core the narrower
        // viewport can safely use two-way cells and retain its performance win.
        //
        // Full-view bands tuned 2026-08-08 against the deterministic drive
        // route: widening near to 2200 and mid to 4000 straightens the last
        // visible stripe kinks for +39k cycles at the p90 frame (685k -> 724k
        // of the 1,127k budget, zero deadline misses). Split screen keeps the
        // old bands: it was already missing deadlines before this change, so
        // it has no headroom to spend on quality.
        let near = 4;
        // Split's mid band pulled from 2600 to 1800 on 2026-08-09: the
        // split kickoff, with both views down the long axis, was the one
        // scene missing its deadline, and a 160-pixel-wide view cannot show
        // the two-way tessellation past 1800 that it is paying twice for.
        let (near_distance, mid_distance) = if split_view() {
            (400, 1200)
        } else {
            (2200, 4000)
        };
        match distance {
            d if d < near_distance => near,
            d if d < mid_distance => 2.min(near),
            _ => 1,
        }
    }

    /// Snap the edge points a coarser neighbour does not sample onto the
    /// straight screen segment between the points it does, so both tiles
    /// rasterise the same edge. Only boundary points move, and only against a
    /// coarser band; interior points keep their true camera-space projection,
    /// which is the property the subdivision exists for.
    ///
    /// Returns a bit per conformed edge (`1 << k` for the k-th direction in
    /// `EDGE_STEPS`). Snapping alone is one pixel short of exact: the integer
    /// midpoint can sit a pixel off the GPU's own line for the same segment,
    /// which reads as a dotted dark arc tracing the band boundary. The caller
    /// lays an underdraw strip behind each flagged edge to catch those.
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn conform_tile(
        g: &mut [[Option<(i16, i16, i32)>; FLOOR_SPLIT_MAX as usize + 1];
             FLOOR_SPLIT_MAX as usize + 1],
        cull: &Cull,
        ix: i32,
        iz: i32,
        mx: i32,
        mz: i32,
        step_x: i32,
        step_z: i32,
        n: i32,
    ) -> u8 {
        let mut conformed = 0u8;
        let nu = n as usize;
        for (k, (dx, dz)) in Self::EDGE_STEPS.into_iter().enumerate() {
            let (jx, jz) = (ix + dx, iz + dz);
            if jx < 0 || jx >= TILES_X || jz < 0 || jz >= TILES_Z {
                continue;
            }
            let nn = Self::floor_split(cull.flat_distance(mx + dx * step_x, mz + dz * step_z));
            if nn >= n {
                continue;
            }
            conformed |= 1 << k;
            // The shared edge: constant sx against an x-step neighbour,
            // constant sz against a z-step one.
            let at = if dx + dz > 0 { nu } else { 0 };
            let cs = (n / nn) as usize;
            for i in 1..nu {
                if i % cs == 0 {
                    continue;
                }
                let (a, b) = (i - i % cs, i - i % cs + cs);
                let (pa, pb) = if dx != 0 {
                    (g[at][a], g[at][b])
                } else {
                    (g[a][at], g[b][at])
                };
                let (Some(pa), Some(pb)) = (pa, pb) else {
                    continue;
                };
                let t = (i - a) as i32;
                let lerp = |u: i32, v: i32| u + (v - u) * t / cs as i32;
                let snapped = Some((
                    lerp(pa.0 as i32, pb.0 as i32) as i16,
                    lerp(pa.1 as i32, pb.1 as i32) as i16,
                    lerp(pa.2, pb.2),
                ));
                if dx != 0 {
                    g[at][i] = snapped;
                } else {
                    g[i][at] = snapped;
                }
            }
        }
        conformed
    }

    /// Neighbour offsets for [`Self::conform_tile`]'s edge mask, in mask-bit
    /// order: -x, +x, -z, +z.
    const EDGE_STEPS: [(i32, i32); 4] = [(-1, 0), (1, 0), (0, -1), (0, 1)];

    /// Lay a thin screen-space strip behind one conformed tile edge, in the
    /// pitch's own tints. Snapped edges still round a pixel off the coarse
    /// side's rasterised line here and there; the strip is what shows through
    /// those single-pixel holes instead of the vista. Quake's renderer ships
    /// the same trick as crack underdraw, a few ordering-table slots behind
    /// the surface.
    #[inline(never)]
    fn underdraw_edge(
        &mut self,
        g: &[[Option<(i16, i16, i32)>; FLOOR_SPLIT_MAX as usize + 1];
             FLOOR_SPLIT_MAX as usize + 1],
        nu: usize,
        k: usize,
        ta: Rgb,
        tb: Rgb,
    ) {
        let (dx, dz) = Self::EDGE_STEPS[k];
        let at = if dx + dz > 0 { nu } else { 0 };
        let (pa, pb) = if dx != 0 {
            (g[at][0], g[at][nu])
        } else {
            (g[0][at], g[nu][at])
        };
        let (Some(a), Some(b)) = (pa, pb) else {
            return;
        };
        let sp = [
            (a.0 - 1, a.1 - 1),
            (b.0 + 1, b.1 - 1),
            (a.0 - 1, a.1 + 1),
            (b.0 + 1, b.1 + 1),
        ];
        if !sp.iter().any(|&(x, y)| on_view(x, y)) {
            return;
        }
        // Behind BOTH tiles that share the edge. Measured from the edge, a
        // neighbouring quad's own sorting depth is its centre, up to half a
        // tile deeper, so a small slot bias left the strip in front of the
        // far tile and drew the boundary as a line. Nothing else lives
        // between the pitch and the vista, so deep is safe.
        let depth = (a.2 + b.2) / 2 + UNDERDRAW_BIAS;
        let (ca, cb) = (tinted(GRASS_A, ta), tinted(GRASS_A, tb));
        if let Some(q) = self.arena.push(QuadGouraud::new(sp, [ca, cb, ca, cb])) {
            self.ot.add_packet_depth(DEPTH_RANGE, depth, q);
        }
    }

    /// One far floor tile as a single quad: four chamfered corners, four
    /// projections, one emit. The general grid path costs several times this
    /// in fixed machinery, and at n == 1 buys nothing for it. Out of line for
    /// the same i-cache reason as [`Self::conform_tile`].
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn floor_tile_far(
        &mut self,
        x0: i32,
        z0: i32,
        x1: i32,
        z1: i32,
        light: &[[Rgb; FLOOR_GZ]; FLOOR_GX],
        gx: usize,
        gz: usize,
        tex_u0: i32,
        tex_v0: i32,
        tex_w: i32,
        packet: TexturedGouraudPacketMaterial,
    ) {
        count_offered!();
        let mut sp = [(0i16, 0i16); 4];
        let mut z_sum = 0;
        for (k, (wx, wz)) in [(x0, z0), (x1, z0), (x0, z1), (x1, z1)]
            .into_iter()
            .enumerate()
        {
            let (cx, cz) = Self::chamfer(wx, wz);
            let p = scene::project_vertex(Vec3I16::new(cx as i16, 0, cz as i16));
            if p.sz == 0 {
                return;
            }
            sp[k] = (p.sx, p.sy);
            z_sum += p.sz as i32;
        }
        if !sp.iter().any(|&(x, y)| on_view(x, y)) {
            return;
        }
        count_kept!();
        let stride = FLOOR_SPLIT_MAX as usize;
        let (r0, r1) = unsafe { (light.get_unchecked(gx), light.get_unchecked(gx + stride)) };
        let tints = unsafe {
            [
                *r0.get_unchecked(gz),
                *r1.get_unchecked(gz),
                *r0.get_unchecked(gz + stride),
                *r1.get_unchecked(gz + stride),
            ]
        };
        let (u0, v0, last) = (tex_u0 as u8, tex_v0 as u8, (tex_w - 1) as u8);
        let (u1, v1) = (u0 + last, v0 + last);
        let uvs = [uvw(u0, v0), uvw(u1, v0), uvw(u0, v1), uvw(u1, v1)];
        self.quad_tex_projected(
            sp,
            z_sum,
            uvs,
            tints,
            FLOOR_BIAS,
            packet,
            false,
        );
    }

    fn floor(&mut self, cull: &Cull) {
        let step_x = sim::HALF_X * 2 / TILES_X;
        let step_z = sim::HALF_Z * 2 / TILES_Z;
        // The pitch is flat, so a tile's box is its footprint with no height.
        // The chamfer only ever pulls corners inward, so this stays generous.
        let tile_h = (step_x / 2, 0, step_z / 2);
        for ix in 0..TILES_X {
            for iz in 0..TILES_Z {
                let x0 = -sim::HALF_X + ix * step_x;
                let z0 = -sim::HALF_Z + iz * step_z;
                let (x1, z1) = (x0 + step_x, z0 + step_z);
                let (mx, mz) = ((x0 + x1) / 2, (z0 + z1) / 2);
                // The vertical test matters in a half-height view: the tiles
                // under and just ahead of the camera, the subdivided ones,
                // fall below its bottom edge.
                if !cull.visible((mx, 0, mz), tile_h)
                    || !cull.visible_vertically((mx, 0, mz), tile_h)
                {
                    continue;
                }
                // Mown stripes down the pitch and the whole floodlight
                // falloff, both baked into one table per stripe at boot. The
                // tile only has to pick its stripe; every vertex colour after
                // that is an array read.
                let light = unsafe { &FLOOR_LIGHT[(ix & 1) as usize] };

                // Chebyshev distance from the camera to the tile centre: one
                // compare cheaper than a hypotenuse and the bands are coarse.
                let n = Self::floor_split(cull.flat_distance(mx, mz));
                // Grid columns one sub-tile is worth, so a coarse tile steps
                // the same table a fine one does.
                let stride = (FLOOR_SPLIT_MAX / n) as usize;
                let (gx, gz) = (
                    (ix * FLOOR_SPLIT_MAX) as usize,
                    (iz * FLOOR_SPLIT_MAX) as usize,
                );
                let marked = (MARKED_FIRST_Z..MARKED_FIRST_Z + MARKED_ROWS).contains(&iz);
                let (tex_u0, tex_v0, tex_w, packet) = if marked {
                    let packet = if ix < MARKED_COLS_PER_PAGE {
                        MARKED_LEFT_PACKET
                    } else {
                        MARKED_RIGHT_PACKET
                    };
                    (
                        MARKED_U0 + (ix % MARKED_COLS_PER_PAGE) * MARKED_TILE_W,
                        MARKED_V0 + (iz - MARKED_FIRST_Z) * MARKED_TILE_W,
                        MARKED_TILE_W,
                        packet,
                    )
                } else {
                    (0, 0, GRASS_TILE_W, GRASS_PACKET)
                };

                // Most of the pitch is one-quad tiles, and the general path
                // below charges each of them the full grid machinery (a 5x5
                // Option array, closures, the conform check) to emit a single
                // quad. Take them straight through: four chamfered corners,
                // four projections, one emit. Pixel-identical, and an n == 1
                // tile never conforms, so nothing else changes.
                if n == 1 {
                    self.floor_tile_far(
                        x0, z0, x1, z1, light, gx, gz, tex_u0, tex_v0, tex_w, packet,
                    );
                    continue;
                }

                // Sub-tiles carry a slice of the same UV rectangle, so the
                // texture keeps its scale and only the vertex count goes up.
                let px = |i: i32| x0 + (x1 - x0) * i / n;
                let pz = |i: i32| z0 + (z1 - z0) * i / n;
                let u = |base: i32, i: i32| {
                    (base + (tex_w * i / n).min(tex_w - 1)) as u8
                };

                // Project the tile's corner grid once. Every interior corner
                // is shared by four sub-quads, so projecting per quad ran the
                // GTE four times per corner; this runs it once. `None` is a
                // corner behind the near plane, and any quad touching one is
                // skipped exactly as `quad_tex`'s own per-corner check did.
                let nu = n as usize;
                let mut corners = [[None; FLOOR_SPLIT_MAX as usize + 1];
                    FLOOR_SPLIT_MAX as usize + 1];
                for (sx, column) in corners.iter_mut().enumerate().take(nu + 1) {
                    for (sz, corner) in column.iter_mut().enumerate().take(nu + 1) {
                        let (cx, cz) = Self::chamfer(px(sx as i32), pz(sz as i32));
                        let p = scene::project_vertex(Vec3I16::new(cx as i16, 0, cz as i16));
                        if p.sz != 0 {
                            *corner = Some((p.sx, p.sy, p.sz as i32));
                        }
                    }
                }

                // A tile bordering a coarser band draws its shared edge as a
                // polyline through more projected points than the neighbour's
                // one straight screen segment, and where the polyline rounds
                // to the far side of that segment the gap shows the vista: a
                // dotted dark arc tracing each band boundary across the pitch.
                // Conforming snaps those extra edge points onto the segment.
                // Out of line, and entered only when this tile could have a
                // coarser neighbour at all: inlining this into the tile loop
                // measured about +100k cycles a frame across the whole render
                // pass, which is this code eating the 4 KB i-cache, not the
                // few adds it actually runs.
                if n > 1 {
                    let mask =
                        Self::conform_tile(&mut corners, cull, ix, iz, mx, mz, step_x, step_z, n);
                    if mask != 0 {
                        // Edge tints from the corners of the tile's light
                        // rows, ordered to match each edge's (a, b) corner
                        // pair in `underdraw_edge`.
                        let m = FLOOR_SPLIT_MAX as usize;
                        let t = |a: usize, b: usize| unsafe {
                            *light.get_unchecked(gx + a).get_unchecked(gz + b)
                        };
                        let tints = [
                            (t(0, 0), t(0, m)),
                            (t(m, 0), t(m, m)),
                            (t(0, 0), t(m, 0)),
                            (t(0, m), t(m, m)),
                        ];
                        for (k, &(ta, tb)) in tints.iter().enumerate() {
                            if mask & (1 << k) != 0 {
                                self.underdraw_edge(&corners, nu, k, ta, tb);
                            }
                        }
                    }
                }

                for sx in 0..nu {
                    let (ua, ub) = (u(tex_u0, sx as i32), u(tex_u0, sx as i32 + 1));
                    // Rows of the light table, resolved once per column of
                    // sub-tiles. Unchecked because the grid is sized from the
                    // same constants the loop bounds are, and a bounds check
                    // on every corner of every floor quad measured as most of
                    // the cost of lighting the pitch at all.
                    let (r0, r1) = unsafe {
                        (
                            light.get_unchecked(gx + sx * stride),
                            light.get_unchecked(gx + (sx + 1) * stride),
                        )
                    };
                    for sz in 0..nu {
                        count_offered!();
                        let (Some(a), Some(b), Some(c), Some(d)) = (
                            corners[sx][sz],
                            corners[sx + 1][sz],
                            corners[sx][sz + 1],
                            corners[sx + 1][sz + 1],
                        ) else {
                            continue;
                        };
                        let sp = [(a.0, a.1), (b.0, b.1), (c.0, c.1), (d.0, d.1)];
                        if !sp.iter().any(|&(x, y)| on_view(x, y)) {
                            continue;
                        }
                        count_kept!();
                        let (va, vb) = (u(tex_v0, sz as i32), u(tex_v0, sz as i32 + 1));
                        let uvs = [uvw(ua, va), uvw(ub, va), uvw(ua, vb), uvw(ub, vb)];
                        let (j0, j1) = (gz + sz * stride, gz + (sz + 1) * stride);
                        let tints = unsafe {
                            [
                                *r0.get_unchecked(j0),
                                *r1.get_unchecked(j0),
                                *r0.get_unchecked(j1),
                                *r1.get_unchecked(j1),
                            ]
                        };
                        self.quad_tex_projected(
                            sp,
                            a.2 + b.2 + c.2 + d.2,
                            uvs,
                            tints,
                            FLOOR_BIAS,
                            packet,
                            false,
                        );
                    }
                }
            }
        }
    }

    /// Boost pads, as orbs floating over the pitch.
    ///
    /// Flat diamonds painted on the grass were invisible in play: at this
    /// camera height the pitch is nearly edge-on, so anything lying on it is
    /// a few pixels of a slightly different green. Rocket League floats a lit
    /// orb instead, and that is why you can see them. Two crossed vertical
    /// quads give one from any angle for the price of two polygons.
    fn pads(&mut self, s: &Sim, cull: &Cull) {
        for (i, pad) in sim::PADS.iter().enumerate() {
            let r = if pad.big { 62 } else { 42 };
            let lift = if pad.big { 78 } else { 58 };
            // Orb and pool together: the orb tops out at `lift + r` and the
            // pool lies on the pitch, so the box runs the whole way down.
            let top = lift + r;
            if !cull.visible((pad.x, -top / 2, pad.z), (r, top / 2, r))
                || !cull.visible_vertically((pad.x, -top / 2, pad.z), (r, top / 2, r))
            {
                continue;
            }
            // Past 3000 in a half-width view the orb is a pixel or two;
            // nothing a player steers by survives at that size.
            if split_view() && cull.flat_distance(pad.x, pad.z) > 3000 {
                continue;
            }
            let live = s.pad_timers[i] == 0;
            let (bright, dim) = ((255, 214, 84), (196, 132, 30));
            let (px, pz) = (pad.x, pad.z);
            // The plate stays whether the pad is up or not: it is the thing that
            // says a pad belongs here, so the layout is learnable and a spent one
            // reads as spent rather than as absent. Two rings, the outer a dark
            // kerb and the inner lit only while there is something to collect.
            let g = r * 3 / 4;
            let plate = |b: &mut Self, reach: i32, y: i32, colour: Rgb, bias: i32| {
                b.quad_biased(
                    [
                        (px - reach, y, pz),
                        (px, y, pz - reach),
                        (px, y, pz + reach),
                        (px + reach, y, pz),
                    ],
                    [colour; 4],
                    bias,
                );
            };
            // A half-width view keeps only the orb once a pad is distant: the
            // plate rings are a couple of pixels there, and the split kickoff
            // sees every pad on the pitch from both views at once.
            if !(split_view() && cull.flat_distance(px, pz) > 2000) {
                plate(self, g, -4, (52, 56, 70), PAD_BIAS + 20);
                plate(
                    self,
                    g / 2,
                    -6,
                    if live { dim } else { (34, 38, 50) },
                    PAD_BIAS + 10,
                );
            }

            // The orb only exists while the pad does. It used to linger as a
            // ghost, which made a taken pad look collectable from any distance
            // where the colour was hard to judge.
            if !live {
                continue;
            }
            let top = -(lift + r);
            let mid = -lift;
            let bot = -(lift - r);
            // Two diamonds in perpendicular vertical planes.
            for axis in 0..2 {
                let (ax, az) = if axis == 0 { (r, 0) } else { (0, r) };
                self.quad_biased(
                    [
                        (px, top, pz),
                        (px - ax, mid, pz - az),
                        (px + ax, mid, pz + az),
                        (px, bot, pz),
                    ],
                    [bright, dim, dim, bright],
                    PAD_BIAS,
                );
            }
        }
    }

    /// The boost gauge: an arc that fills as the tank does, with the number in
    /// the middle. A flat row of pips reads as a debug bar; a dial reads as an
    /// instrument, and it is what the original uses.
    /// The scoreboard fascia: a sheared dark plate with a team block at each
    /// end, for the score digits to sit on.
    ///
    /// The old HUD was bare text on the sky. It survived only because the top
    /// of the screen happens to be dark, and put the ball over the crossbar and
    /// it sat on grey wall instead. Nothing shipped on this hardware with a HUD
    /// that thin: Gran Turismo 2, Colin McRae 2 and Crash Team Racing all give
    /// their readouts a plate to live on, and the plate is what makes type read
    /// at any brightness behind it.
    ///
    /// The shear matches the front end's panels, so the two look like one game.
    /// The team blocks do the job the BLU and ORG labels used to: colour tells
    /// you whose score is whose faster than three letters can.
    pub fn scoreboard(&mut self) {
        const TOP: i16 = 0;
        const BOT: i16 = 26;
        const L: i16 = 82;
        const R: i16 = 238;
        const SHEAR: i16 = 7;
        /// Where the team block ends and the clock's darker centre begins.
        const BLOCK: i16 = 44;

        let plate = (18, 22, 34);
        let plate_lo = (28, 34, 50);
        // Centre panel, carrying the clock.
        self.screen_quad(
            HUD_PLATE_SLOT,
            [(L + SHEAR, TOP), (R - SHEAR, TOP), (L, BOT), (R, BOT)],
            [plate, plate, plate_lo, plate_lo],
        );
        // Team blocks. Drawn in front of the plate so their edges cut it.
        // The far edge is derived rather than authored: five eighths lands
        // between the two darker halves the fixed blue and orange used, and a
        // second authored colour per paint is one more thing to keep in step.
        for block in [true, false] {
            let near = seat_signal(if block { 0 } else { 1 });
            let far = shade(near, 5, 8);
            let (x0, x1) = if block {
                (L, L + BLOCK)
            } else {
                (R - BLOCK, R)
            };
            let t0 = if block {
                L + SHEAR
            } else {
                R - BLOCK - SHEAR + SHEAR
            };
            let _ = t0;
            // Only the outer edge is sheared; the inner one stays vertical so
            // the two blocks and the centre read as one bar.
            let (tx0, tx1) = if block {
                (x0 + SHEAR, x1)
            } else {
                (x0, x1 - SHEAR)
            };
            self.screen_quad(
                HUD_BLOCK_SLOT,
                [(tx0, TOP), (tx1, TOP), (x0, BOT), (x1, BOT)],
                [far, far, near, near],
            );
        }
        // A bright rule along the bottom, so the fascia has an edge rather
        // than fading into whatever is behind it. In front of the blocks: the
        // table prepends within a slot, so drawing it after them at the same
        // depth put it behind, and it only showed across the dark centre.
        self.screen_quad(
            HUD_RULE_SLOT,
            [(L, BOT - 2), (R, BOT - 2), (L - 1, BOT), (R + 1, BOT)],
            [(150, 178, 230); 4],
        );
    }

    /// The boost dial, centred `cx` across. One player's screen puts it near
    /// the right edge; a split game gives each half its own.
    fn boost_gauge(&mut self, cx: i16, cy: i16, boost_pips: i32) {
        const SEGS: i32 = 18;
        // Two thirds the size in a half-height view, or the dial is a
        // quarter of the picture.
        let (r_out, r_in) = if split_view() { (20, 15) } else { (30, 22) };
        // Sweep three quarters of a turn, opening at the bottom right so the
        // gap faces away from the pitch.
        const START: i32 = 1300;
        const SWEEP: i32 = 3000;

        let filled = boost_pips * SEGS / sim::BOOST_MAX_PIPS;
        for i in 0..SEGS {
            let lit = i < filled;
            let c = if lit {
                // Warms toward the top of the tank, so a full one reads at a
                // glance without having to count.
                let heat = (i * 255 / SEGS) as u8;
                (255, 150 + heat / 3, 40)
            } else {
                (44, 48, 62)
            };
            let a0 = (START + SWEEP * i / SEGS) as u16;
            let a1 = (START + SWEEP * (i + 1) / SEGS - 40) as u16;
            let p = |a: u16, rad: i32| {
                (
                    cx + ((sin_q12(a) * rad) >> 12) as i16,
                    cy - ((cos_q12(a) * rad) >> 12) as i16,
                )
            };
            self.screen_quad(
                1,
                [p(a0, r_in), p(a0, r_out), p(a1, r_in), p(a1, r_out)],
                [c; 4],
            );
        }
    }

    /// The goal celebration: a shockwave ring off the ball and a wash of the
    /// scoring team's colour over the whole screen, both fading out.
    ///
    /// Drawn only while the world is frozen after a goal, so it costs nothing
    /// during play. The ring is built from the same wedge trick the turntable
    /// uses, expanded by how long ago the goal went in.
    /// One piece of debris, as a screen-space streak from where it was to
    /// where it is.
    ///
    /// A square is the wrong shape for something moving fast: sixteen of them
    /// read as confetti, whichever way they are flying. Stretching each piece
    /// along its own travel is what makes it debris, and it costs a second
    /// projection rather than any state.
    fn streak(
        &mut self,
        head: (i32, i32, i32),
        tail: (i32, i32, i32),
        w: i32,
        hot: Rgb,
        cold: Rgb,
    ) {
        let h = scene::project_vertex(Vec3I16::new(head.0 as i16, head.1 as i16, head.2 as i16));
        let t = scene::project_vertex(Vec3I16::new(tail.0 as i16, tail.1 as i16, tail.2 as i16));
        if h.sz == 0 || t.sz == 0 || !on_view(h.sx, h.sy) {
            return;
        }
        // Width in pixels, from a width in uu at the head's depth, never less
        // than the pixel it would otherwise round away to.
        let half = ((w * PROJ_H as i32 / h.sz.max(1) as i32).max(1)).min(STREAK_MAX_PX) as i16;
        let (mut dx, mut dy) = (h.sx as i32 - t.sx as i32, h.sy as i32 - t.sy as i32);
        // And a ceiling on the length as well as the width. A piece thrown
        // past the lens covers most of the screen in three ticks, and a streak
        // that long stops reading as speed and starts reading as a plank.
        let mut len = isqrt_i32(dx * dx + dy * dy).max(1);
        if len > STREAK_MAX_LEN {
            dx = dx * STREAK_MAX_LEN / len;
            dy = dy * STREAK_MAX_LEN / len;
            len = STREAK_MAX_LEN;
        }
        let (tx, ty) = ((h.sx as i32 - dx) as i16, (h.sy as i32 - dy) as i16);
        // Across the streak, so the quad has thickness whichever way it points.
        let (px, py) = (
            (-dy * half as i32 / len) as i16,
            (dx * half as i32 / len) as i16,
        );
        // A degenerate streak still wants to be visible, so a piece that has
        // barely moved falls back to a square.
        let (px, py) = if px == 0 && py == 0 {
            (half, 0)
        } else {
            (px, py)
        };
        self.emit(
            [
                (h.sx + px, h.sy + py),
                (h.sx - px, h.sy - py),
                (tx + px, ty + py),
                (tx - px, ty - py),
            ],
            h.sz as i32,
            [hot, hot, cold, cold],
        );
    }

    /// A burst of debris thrown out of `origin`, its whole flight a function of
    /// `age` alone.
    ///
    /// Stateless on purpose. There is no particle pool and nothing to update:
    /// each piece's position is computed from its index and the tick count, so
    /// a split frame draws the same explosion twice from two cameras without
    /// either pass advancing it, and a dropped frame does not lose a piece.
    /// The cost of that is that the pieces cannot collide or be disturbed,
    /// which nothing here wants them to do.
    ///
    /// Three layers, because one is not an explosion. A flash that is over
    /// almost before you see it, sparks that fly and fall, and smoke that
    /// outlives both and drifts up. Everything is camera-facing: a flat quad
    /// in the world disappears when you see it edge on, and at this size that
    /// is most of the time.
    /// `scale` is Q12 and multiplies every distance: a demolition happens
    /// under your bumper and wants life size, a goal happens at the far end of
    /// the arena where life size is four pixels.
    fn burst(&mut self, origin: (i32, i32, i32), age: i32, colour: Rgb, scale: i32) {
        if !(0..BURST_LIFE).contains(&age) {
            return;
        }
        // Halved in a split pass, the same trade the floor and the walls make:
        // the effect keeps its shape with half the pieces, and a split frame
        // draws it twice.
        let half_detail = split_view();

        // The flash. Two quads, a wide dim one under a small white one, both
        // growing fast and gone in a fifth of a second. This is what reads as
        // the bang; everything after it is the aftermath.
        if age < FLASH_LIFE {
            let grow = (age + 2) * 4096 / FLASH_LIFE;
            let fade = (FLASH_LIFE - age) * 4096 / FLASH_LIFE;
            for (r, tint) in [
                (
                    (FLASH_R * scale >> 12) * grow >> 12,
                    mix(colour, (255, 236, 170), 10),
                ),
                ((FLASH_R * scale >> 12) * grow >> 13, (255, 250, 226)),
            ] {
                let v = scene::project_vertex(Vec3I16::new(
                    origin.0 as i16,
                    origin.1 as i16,
                    origin.2 as i16,
                ));
                if v.sz == 0 || !on_view(v.sx, v.sy) {
                    continue;
                }
                let h = ((r * PROJ_H as i32 / v.sz.max(1) as i32).max(1) as i16).min(FLASH_MAX_PX);
                let c = shade(tint, fade, 4096);
                // Blended, not opaque. A flat quad this size laid over the
                // pitch is a hole in the picture; averaged with what is behind
                // it, it is light.
                self.emit_blended(
                    [
                        (v.sx - h, v.sy - h),
                        (v.sx + h, v.sy - h),
                        (v.sx - h, v.sy + h),
                        (v.sx + h, v.sy + h),
                    ],
                    v.sz as i32,
                    [c; 4],
                );
            }
        }

        // Sparks. Fast, thrown out on a spread of headings, pulled down hard,
        // and burning from white through the team colour as they go.
        let sparks = if half_detail {
            SPARK_COUNT / 2
        } else {
            SPARK_COUNT
        };
        for i in 0..sparks {
            // Two kinds in one loop. Every other piece is fire: bright, quick,
            // and gone before the smoke arrives. The rest is debris, which
            // leaves slower, stays the colour of the car it came off, and is
            // still falling when the fire has burnt out. One layer doing both
            // jobs is what made this read as tumbling wood.
            let fire = i & 1 == 0;
            let life = if fire { FIRE_LIFE } else { SPARK_LIFE };
            let born = (i * 3) % 5;
            if age < born || age - born >= life {
                continue;
            }
            let t = age - born;
            let left = life - t;
            // Spread from the index rather than a table: a ring of headings
            // with an elevation that walks around it, which at this many
            // pieces is indistinguishable from a scattered one and costs no
            // bytes.
            let yaw = ((4096 * i / sparks) + ((i * 1013) & 255)) as u16;
            let elev = (((i * 577) & 1023) + 128) as u16;
            let (ce, se) = (cos_q12(elev), sin_q12(elev));
            let dir = ((sin_q12(yaw) * ce) >> 12, se, (cos_q12(yaw) * ce) >> 12);
            // Slower pieces near the middle, so the cloud has a front and a
            // tail instead of being one expanding shell.
            let speed =
                ((if fire { FIRE_SPEED } else { SPARK_SPEED } - ((i * 7) & 15)) * scale) >> 12;
            let at = |t: i32| {
                // Eased out rather than linear: a blast throws everything at
                // once and the air takes it back, so the spread is nearly all
                // in the first few ticks. Travelling at a constant rate for a
                // third of a second is what a firework does, not an explosion.
                let travel = speed * t * ((2 * life) - t) / (2 * life);
                let drop = (t * t * BURST_GRAVITY) >> 8;
                (
                    origin.0 + ((dir.0 * travel) >> 12),
                    // Render space has +Y down, so rising is negative and the
                    // gravity term is what brings a piece back.
                    origin.1 - ((dir.1 * travel) >> 12) + drop,
                    origin.2 + ((dir.2 * travel) >> 12),
                )
            };
            // Held bright for most of the flight and dropped only over the
            // last third. Fading linearly from the first tick turned every
            // spark brown before it had gone anywhere.
            let fade = (left * 3).min(life);
            let hot = if fire {
                // Fire runs white to the team colour over its short life, so
                // the first frames of a blast are light rather than paint.
                shade(mix(colour, (255, 248, 226), 14 * left / life), fade, life)
            } else {
                shade(mix(colour, (40, 36, 36), 8), fade, life)
            };
            self.streak(
                at(t),
                at((t - STREAK_TICKS).max(0)),
                ((if fire { FIRE_W } else { SPARK_W }) * scale) >> 12,
                hot,
                shade(hot, 1, 3),
            );
        }

        // Smoke. Slow, rising, growing rather than shrinking, and outliving
        // the sparks so the explosion has something left after the light has
        // gone out of it.
        let puffs = if half_detail {
            SMOKE_COUNT / 2
        } else {
            SMOKE_COUNT
        };
        for i in 0..puffs {
            let born = i * 2;
            if age < born {
                continue;
            }
            let t = age - born;
            let left = (BURST_LIFE - born - t).max(0);
            if left == 0 {
                continue;
            }
            let yaw = ((4096 * i / puffs) + 400) as u16;
            let travel = ((SMOKE_SPEED * scale) >> 12) * t;
            let p = (
                origin.0 + ((sin_q12(yaw) * travel) >> 12),
                origin.1 - ((SMOKE_RISE * scale) >> 12) * t / 8,
                origin.2 + ((cos_q12(yaw) * travel) >> 12),
            );
            let v = scene::project_vertex(Vec3I16::new(p.0 as i16, p.1 as i16, p.2 as i16));
            if v.sz == 0 || !on_view(v.sx, v.sy) {
                continue;
            }
            let r = ((SMOKE_R + SMOKE_GROW * t / 8) * scale) >> 12;
            let h = ((r * PROJ_H as i32 / v.sz.max(1) as i32).max(1) as i16).min(SMOKE_MAX_PX);
            // Dark and getting darker, so it settles into the pitch instead of
            // ending as a grey square that suddenly is not there. Blended for
            // the same reason as the flash: opaque smoke is a grey box.
            let c = shade((132, 128, 132), left * left / (BURST_LIFE / 2), BURST_LIFE);
            self.emit_blended(
                [
                    (v.sx - h, v.sy - h),
                    (v.sx + h, v.sy - h),
                    (v.sx - h, v.sy + h),
                    (v.sx + h, v.sy + h),
                ],
                v.sz as i32,
                [c; 4],
            );
        }
    }

    /// The explosion a demolished car leaves behind, at the point of contact.
    ///
    /// The sim keeps a wreck where it was hit until its timer runs out, so the
    /// car's own position is the right place for this and nothing has to be
    /// remembered across ticks.
    fn demo_burst(&mut self, s: &Sim) {
        for (seat, car) in [&s.car, &s.opponent].into_iter().enumerate() {
            if !car.wrecked() {
                continue;
            }
            let age = (sim::DEMO_RESPAWN - car.demo_timer) as i32;
            self.burst(
                (r(car.p.x), ry(car.p.y), r(car.p.z)),
                age,
                seat_signal(seat),
                4096,
            );
        }
    }

    fn goal_burst(&mut self, s: &Sim) {
        if s.goal_freeze == 0 {
            return;
        }
        const SEGS: usize = 10;
        // Ticks since the goal, counting up from zero.
        let age = (sim::GOAL_FREEZE_TICKS - s.goal_freeze) as i32;
        let team = match s.last_scorer {
            sim::Team::Blue => seat_signal(0),
            sim::Team::Orange => seat_signal(1),
        };

        // No full-screen wash. A quad at the front of the ordering table is
        // opaque on this hardware, so it does not tint the frame, it replaces
        // it: the first version blacked the screen out for forty-five ticks.
        // Doing it properly needs a semi-transparent primitive, and the
        // shockwave alone reads well enough that it can wait.

        // Shockwave: a ring on the floor under the ball, expanding and fading.
        if age < 70 {
            let radius = 120 + age * 26;
            let fade = (70 - age) * 4096 / 70;
            let c = shade(team, fade, 4096);
            let (bx, bz) = (r(s.ball.p.x), r(s.ball.p.z));
            for i in 0..SEGS {
                let a0 = (4096 * i as i32 / SEGS as i32) as u16;
                let a1 = (4096 * (i as i32 + 1) / SEGS as i32) as u16;
                let p = |a: u16, rad: i32| {
                    (
                        bx + ((sin_q12(a) * rad) >> 12),
                        -5,
                        bz + ((cos_q12(a) * rad) >> 12),
                    )
                };
                self.quad_biased(
                    [
                        p(a0, radius - 40),
                        p(a0, radius),
                        p(a1, radius - 40),
                        p(a1, radius),
                    ],
                    [c; 4],
                    -80,
                );
            }
        }
        // And the debris, between the posts, where the original detonates it
        // rather than at whoever scored. On the goal line, not at the ball:
        // the ball carries on into the back of the net, and an explosion in
        // there happens behind the mesh where none of it can be seen.
        self.burst(
            (
                r(s.ball.p.x),
                ry(s.ball.p.y),
                r(s.ball.p.z).clamp(-sim::HALF_Z, sim::HALF_Z),
            ),
            age,
            team,
            GOAL_BURST_SCALE,
        );
    }

    /// The arena's cross-section, from the floor edge up and over to the
    /// ceiling edge, as `(inset from the wall, height)` pairs in uu.
    ///
    /// The wall is not a flat plane: the floor curves up into it and it curves
    /// over into the ceiling. Sweeping one profile around the perimeter gets
    /// both curves, the wall, and a consistent silhouette in the corners, which
    /// is the shape a box was missing.
    fn profile() -> [(i32, i32); PROFILE_LEN] {
        // Every slot must be written. An unwritten one is (0, 0), which is a
        // point on the floor at the wall line, and the band reaching it from
        // the ceiling draws an inside-out skirt down the whole arena.
        let mut out = [(0, 0); PROFILE_LEN];
        // Floor-to-wall quarter turn: inset shrinks to zero as height climbs.
        for i in 0..=CURVE_SEGS {
            let a = (1024 * i as i32) / CURVE_SEGS as i32; // 0..90 degrees, Q12
            let c = cos_q12(a as u16);
            let sn = sin_q12(a as u16);
            out[i] = (
                RAMP_R - ((sn * RAMP_R) >> 12),
                (RAMP_R - ((c * RAMP_R) >> 12)),
            );
        }
        // The lit rail, flush with the wall, splitting it into a lower and an
        // upper half so each can carry its own light.
        out[RAIL_LO_RING] = (0, RAIL_LO_Y);
        out[RAIL_HI_RING] = (0, RAIL_HI_Y);
        // Wall-to-ceiling quarter turn. Runs upward and inward, from the top
        // of the straight wall to the ceiling edge: `i == 0` is the wall top,
        // so the band between it and the rail is the upper wall.
        for i in 0..=CURVE_SEGS {
            let a = (1024 * i as i32) / CURVE_SEGS as i32;
            let c = cos_q12(a as u16);
            let sn = sin_q12(a as u16);
            out[RAIL_HI_RING + 1 + i] = (
                CEIL_R - ((c * CEIL_R) >> 12),
                sim::CEIL - CEIL_R + ((sn * CEIL_R) >> 12),
            );
        }
        out
    }

    /// Sweep the profile along one perimeter span, taking its vertex colours
    /// from the light baked for it at boot.
    fn wall_span(&mut self, si: usize, cull: &Cull) {
        let Span { a, b, n } = unsafe { SPANS[si] };
        let end_wall = a.1 == b.1 && a.1.abs() == sim::HALF_Z;
        if end_wall
            && cull.pos.2 * a.1.signum() > sim::HALF_Z
            && cull.pos.0.abs() < sim::GOAL_HALF_W
        {
            // From inside the goal the two wall runs beside the opening are
            // closer than the PS1 near plane. They are genuinely peripheral,
            // but projection saturation stretches them across the viewport.
            return;
        }
        let mid = ((a.0 + b.0) / 2, (a.1 + b.1) / 2);
        // The span's footprint, grown by the deepest inward reach of the sweep
        // on both horizontal axes, and the full arena height on the vertical.
        let h = (
            (b.0 - a.0).abs() / 2 + RAMP_R,
            sim::CEIL / 2,
            (b.1 - a.1).abs() / 2 + RAMP_R,
        );
        if !cull.visible((mid.0, -sim::CEIL / 2, mid.1), h) {
            return;
        }
        let profile = unsafe { &WALL_PROFILE };
        // Same distance bands as the floor. A wall is the surface you see at
        // the most glancing angle of anything in here, because you drive along
        // it, so it warps for exactly the same reason and takes the same fix.
        let distance = cull.flat_distance(mid.0, mid.1);
        // A half-width view caps a span at two columns: the third exists to
        // keep panel texture slices from stretching at full width, and at 160
        // pixels the slices it saves are two pixels wide. Kickoff shows every
        // span from both views, which is where the saving is spent.
        let splits = Self::floor_split(distance).min(if split_view() { 2 } else { 3 });
        // Eight samples make a nearby quarter-pipe read as a curve. Once a
        // whole wall span is several thousand units away, alternate samples
        // project into the same pixel in a 160-wide view; collapse those pairs
        // without ever crossing an arc endpoint or a rail/material boundary.
        let ahead = Cull::dot(cull.fwd, (mid.0 - cull.pos.0, 0, mid.1 - cull.pos.2)) > 0;
        let curve_stride = match (ahead, distance) {
            (true, d) if d < 1400 => 1,
            // A half-width view hands the two-sample curve back to four-sample
            // a thousand units sooner; past that, one band per curve: the
            // stride clamps at the arc endpoints, which keeps the rail and
            // material boundaries exactly where they were.
            (true, d) if d < if split_view() { 2400 } else { 3600 } => 2,
            (_, d) if d > 3600 && split_view() => 8,
            _ => 4,
        };
        let light = unsafe { &WALL_LIGHT[si] };
        let slots = &SLOT_OF[splits as usize];
        // Where each split lands along the span, and the slice of the panel's
        // U range it carries. Once per span: this used to be recomputed for
        // every quad of every ring, which on a three-way split is a hundred
        // integer divides for four distinct answers.
        let span_len = isqrt_i32((b.0 - a.0) * (b.0 - a.0) + (b.1 - a.1) * (b.1 - a.1));
        let cover_u = cover_texels(span_len) as i32;
        let (mut sx, mut sz, mut panel_u, mut cover_us) =
            ([0i32; 4], [0i32; 4], [0u8; 4], [0u8; 4]);
        for i in 0..=splits as usize {
            sx[i] = a.0 + (b.0 - a.0) * i as i32 / splits;
            sz[i] = a.1 + (b.1 - a.1) * i as i32 / splits;
            panel_u[i] = (64 + 32 * i as i32 / splits).min(95) as u8;
            cover_us[i] = (COVER_U0 as i32 + cover_u * i as i32 / splits) as u8;
        }
        // Rings of the sweep. Keep every ring in split-screen as well as full
        // screen: skipping alternate samples did not merely reduce detail. It
        // jumped over the floor curve's vertical tangent and joined a point on
        // the arc directly to the rail, turning the quarter-circle into two
        // enormous diagonal bands.
        //
        // The rings this span will actually visit, resolved up front so the
        // ring-by-split vertex grid can be projected once. Every interior
        // vertex is shared by four quads, so projecting per quad ran the GTE
        // four times per vertex.
        let mut rings = [0usize; PROFILE_LEN];
        let mut ring_count = 1;
        {
            let mut ri = 0;
            while ri + 1 < profile.len() {
                let top = if ri < CURVE_SEGS {
                    (ri + curve_stride).min(CURVE_SEGS)
                } else if ri >= WALL_TOP_RING {
                    (ri + curve_stride).min(PROFILE_LEN - 1)
                } else {
                    ri + 1
                };
                rings[ring_count] = top;
                ring_count += 1;
                ri = top;
            }
        }
        let ring = |p: (i32, i32), at: (i32, i32)| {
            (at.0 + ((n.0 * p.0) >> 12), -p.1, at.1 + ((n.1 * p.0) >> 12))
        };
        let mut grid = [[None; 4]; PROFILE_LEN];
        for (row, &r) in rings.iter().enumerate().take(ring_count) {
            for k in 0..=splits as usize {
                let (wx, wy, wz) = ring(profile[r], (sx[k], sz[k]));
                let p = scene::project_vertex(Vec3I16::new(wx as i16, wy as i16, wz as i16));
                if p.sz != 0 {
                    grid[row][k] = Some((p.sx, p.sy, p.sz as i32));
                }
            }
        }

        for row in 0..ring_count - 1 {
            let (ri, top) = (rings[row], rings[row + 1]);
            // Unchecked for the same reason the floor is: the ring index
            // walks a window over a table sized from `PROFILE_LEN`.
            let (llo, lhi) = unsafe { (light.get_unchecked(ri), light.get_unchecked(top)) };
            for k in 0..splits as usize {
                count_offered!();
                let (Some(a), Some(b), Some(c), Some(d)) = (
                    grid[row][k],
                    grid[row][k + 1],
                    grid[row + 1][k],
                    grid[row + 1][k + 1],
                ) else {
                    continue;
                };
                let sp = [(a.0, a.1), (b.0, b.1), (c.0, c.1), (d.0, d.1)];
                // A nearby roof-curve band can cross the whole view while all
                // four projected corners sit beyond its edges. Corner-only
                // acceptance made that top section disappear during a wall
                // climb even though the polygon covered visible pixels.
                if !quad_overlaps_view(&sp) {
                    continue;
                }
                count_kept!();
                // The wall tile starts at texel 64 now that grass owns the
                // first 64 columns. Slicing from 32 sampled grass and painted
                // the arena walls with pitch.
                let covered = ri >= RAIL_HI_RING;
                // Everything above the lit rail samples the shared cover in
                // world units, including distance travelled around the roof
                // curve. The solid barrier below retains its panel tile.
                let (u0, u1, v0, v1) = if covered {
                    let v = unsafe { &COVER_PROFILE_V };
                    (cover_us[k], cover_us[k + 1], v[ri], v[top])
                } else {
                    (panel_u[k], panel_u[k + 1], 0, 31)
                };
                let uvs = [uvw(u0, v0), uvw(u1, v0), uvw(u0, v1), uvw(u1, v1)];
                let (s0, s1) = (slots[k], slots[k + 1]);
                // The barrier's rings already carry the colour of the half of
                // the pitch they stand on, laid over their light by
                // `paint_curb` when the match set its paints.
                let tints = unsafe {
                    [
                        *llo.get_unchecked(s0),
                        *llo.get_unchecked(s1),
                        *lhi.get_unchecked(s0),
                        *lhi.get_unchecked(s1),
                    ]
                };
                self.quad_tex_projected(
                    sp,
                    a.2 + b.2 + c.2 + d.2,
                    uvs,
                    tints,
                    0,
                    if covered { COVER_PACKET } else { WALL_PACKET },
                    covered,
                );
            }
        }
    }

    /// The floodlights themselves.
    ///
    /// A lighting term with no fixture to point at is just a gradient. Each
    /// bank is a bright bar hung on the wall with a housing over it, sitting
    /// where [`LAMPS`] says the light comes from, so the pools on the pitch
    /// and the wall have a visible cause. The bar is the brightest thing in
    /// the game by a wide margin, which is the point: nothing else in the
    /// frame occupies the top of the range.
    fn lamps(&mut self, cull: &Cull) {
        for l in LAMPS.iter().take(8) {
            let p = (
                l.p.0 << LAMP_SHIFT,
                l.p.1 << LAMP_SHIFT,
                l.p.2 << LAMP_SHIFT,
            );
            if !cull.visible(p, (LAMP_HALF_W, 200, LAMP_HALF_W)) {
                continue;
            }
            // A bar across the wall it hangs on: the tangent is whichever
            // horizontal axis the fixture is not facing along.
            let (tx, tz) = if p.0.abs() > 3000 && p.2.abs() > 3000 {
                // Corner chamfer: run along the 45-degree face.
                (if p.0 < 0 { 1 } else { -1 }, if p.2 < 0 { -1 } else { 1 })
            } else if p.0.abs() > p.2.abs() {
                (0, 1)
            } else {
                (1, 0)
            };
            let (ax, az) = (tx * LAMP_HALF_W, tz * LAMP_HALF_W);
            let (top, bot) = (p.1 - LAMP_HALF_H, p.1 + LAMP_HALF_H);
            self.quad(
                [
                    (p.0 - ax, top, p.2 - az),
                    (p.0 + ax, top, p.2 + az),
                    (p.0 - ax, bot, p.2 - az),
                    (p.0 + ax, bot, p.2 + az),
                ],
                [LAMP_HOT, LAMP_HOT, LAMP_WARM, LAMP_WARM],
            );
            // Housing above it, so the bar reads as fitted rather than
            // floating, and the roofline gets a silhouette.
            self.quad_flat(
                [
                    (p.0 - ax, top - 110, p.2 - az),
                    (p.0 + ax, top - 110, p.2 + az),
                    (p.0 - ax, top, p.2 - az),
                    (p.0 + ax, top, p.2 + az),
                ],
                LAMP_HOUSING,
            );
        }
        // The rig over the centre spot, seen from underneath.
        let rig = &LAMPS[8];
        let (rx, ry_, rz) = (
            rig.p.0 << LAMP_SHIFT,
            rig.p.1 << LAMP_SHIFT,
            rig.p.2 << LAMP_SHIFT,
        );
        if cull.visible((rx, ry_, rz), (RIG_HALF, 40, RIG_HALF)) {
            for &(ox, oz) in &[(-1i32, -1i32), (1, -1), (-1, 1), (1, 1)] {
                let (cx, cz) = (rx + ox * RIG_HALF / 2, rz + oz * RIG_HALF / 2);
                let h = RIG_HALF / 2 - 40;
                self.quad_flat(
                    [
                        (cx - h, ry_ + 30, cz - h),
                        (cx + h, ry_ + 30, cz - h),
                        (cx - h, ry_ + 30, cz + h),
                        (cx + h, ry_ + 30, cz + h),
                    ],
                    LAMP_HOT,
                );
            }
        }
    }

    fn walls(&mut self, cull: &Cull) {
        for si in 0..SPAN_COUNT {
            self.wall_span(si, cull);
        }
        // Continue the translucent enclosure over each goal mouth. The old
        // lintel was one opaque quad at the goal line, so it formed a dark
        // rectangular seam and ignored the wall-to-roof curve on either side.
        for (i, &sz) in [-1i32, 1].iter().enumerate() {
            let z = sz * sim::HALF_Z;
            if cull.pos.2 * sz > sim::HALF_Z && cull.pos.0.abs() < sim::GOAL_HALF_W {
                continue;
            }
            let gw = sim::GOAL_HALF_W;
            let profile = unsafe { &WALL_PROFILE };
            let profile_v = unsafe { &COVER_PROFILE_V };
            // `build_spans` appends the two end-wall runs for -Z, then the
            // matching pair for +Z. Borrow their goalpost vertices verbatim:
            // matching colours at the shared edge removes the last vertical
            // lighting seam without another per-frame light calculation.
            let left_light = unsafe { &WALL_LIGHT[SPAN_COUNT - 4 + i * 2] };
            let right_light = unsafe { &WALL_LIGHT[SPAN_COUNT - 3 + i * 2] };
            let ring_light = |ri: usize| [left_light[ri][4], right_light[ri][0]];
            let across = cover_texels(2 * gw);
            // Phase the honeycomb from the left end-wall span. Its eight-texel
            // horizontal period then reaches the right span without a doubled
            // strand at either goalpost.
            let phase = cover_texels(CORNER_X - gw) as i32 % HEX_W;
            let u0 = COVER_U0 + phase as u8;
            let u1 = u0 + across;
            let wall_top = profile[WALL_TOP_RING];
            // The adjacent wall maps one straight quad from the upper rail to
            // `wall_top`. Interpolate inside that exact mapping instead of
            // independently rounding world units at the crossbar.
            let bottom_num = sim::GOAL_H - RAIL_HI_Y;
            let bottom_den = wall_top.1 - RAIL_HI_Y;
            let bottom_v = (profile_v[WALL_TOP_RING] as i32 * bottom_num / bottom_den) as u8;
            let interpolate = |a: Rgb, b: Rgb| {
                let channel = |x: u8, y: u8| {
                    (x as i32 + (y as i32 - x as i32) * bottom_num / bottom_den).clamp(0, 255) as u8
                };
                (channel(a.0, b.0), channel(a.1, b.1), channel(a.2, b.2))
            };
            let rail_light = ring_light(RAIL_HI_RING);
            let wall_top_light = ring_light(WALL_TOP_RING);
            let bottom_light = [
                interpolate(rail_light[0], wall_top_light[0]),
                interpolate(rail_light[1], wall_top_light[1]),
            ];
            let emit_band = |this: &mut Self,
                             lo: (i32, i32),
                             hi: (i32, i32),
                             v0: u8,
                             v1: u8,
                             lo_light: [Rgb; 2],
                             hi_light: [Rgb; 2]| {
                let ring = |p: (i32, i32)| (0, -p.1, z - sz * p.0);
                let a = ring(lo);
                let b = ring(hi);
                this.quad_tex(
                    [
                        (-gw, a.1, a.2),
                        (gw, a.1, a.2),
                        (-gw, b.1, b.2),
                        (gw, b.1, b.2),
                    ],
                    [uvw(u0, v0), uvw(u1, v0), uvw(u0, v1), uvw(u1, v1)],
                    [lo_light[0], lo_light[1], hi_light[0], hi_light[1]],
                    0,
                    COVER_PACKET,
                    true,
                );
            };

            // Straight net from the crossbar to the first point of the roof
            // curve, then every high-density arc band through the ceiling.
            emit_band(
                self,
                (0, sim::GOAL_H),
                wall_top,
                bottom_v,
                profile_v[WALL_TOP_RING],
                bottom_light,
                wall_top_light,
            );
            for ri in WALL_TOP_RING..PROFILE_LEN - 1 {
                emit_band(
                    self,
                    profile[ri],
                    profile[ri + 1],
                    profile_v[ri],
                    profile_v[ri + 1],
                    ring_light(ri),
                    ring_light(ri + 1),
                );
            }
        }
    }

    /// The translucent cover over the roof.
    fn ceiling(&mut self, cull: &Cull) {
        let (x, z) = (ROOF_HALF_X, ROOF_HALF_Z);
        // The old pitch threshold omitted the roof whenever a wall-climbing
        // camera looked level, even with the ceiling plainly inside the right
        // side of the frame. Test the whole roof against both view axes
        // instead, retaining the all-patches skip when it is truly offscreen.
        let roof_box = ((0, -sim::CEIL, 0), (x, 0, z));
        if !cull.visible(roof_box.0, roof_box.1)
            || !cull.visible_vertically(roof_box.0, roof_box.1)
        {
            return;
        }
        // One quad stretched a 32-pixel wall tile over the whole 7,672 by
        // 9,720-uu roof. Patch it at exact texture-repeat distances instead:
        // every roof cell now has the same dimensions as one on the wall, and
        // the 128x84 atlas periods meet without a doubled strand. Corner
        // light comes from the boot-time table; each patch is culled on its
        // own box, the way floor tiles are, so a camera looking along the
        // pitch projects the dozen patches in front of it and not the roof.
        let y = -sim::CEIL;
        let lights = unsafe { &ROOF_CORNER_LIGHT };
        let (half_x, half_z) = (ROOF_STEP_X / 2, ROOF_STEP_Z / 2);
        for iz in 0..ROOF_ROWS {
            let (z0, z1) = (roof_corner_z(iz), roof_corner_z(iz + 1));
            for ix in 0..ROOF_COLS {
                let (x0, x1) = (roof_corner_x(ix), roof_corner_x(ix + 1));
                if !cull.visible(((x0 + x1) / 2, y, (z0 + z1) / 2), (half_x, 0, half_z)) {
                    continue;
                }
                let w = cover_texels(x1 - x0);
                let h = ((z1 - z0 + COVER_UU_PER_TEXEL - 1) / COVER_UU_PER_TEXEL)
                    .clamp(1, ROOF_PATCH_V) as u8;
                self.quad_tex(
                    [(x0, y, z0), (x1, y, z0), (x0, y, z1), (x1, y, z1)],
                    [
                        uvw(COVER_U0, COVER_V0),
                        uvw(COVER_U0 + w, COVER_V0),
                        uvw(COVER_U0, COVER_V0 + h),
                        uvw(COVER_U0 + w, COVER_V0 + h),
                    ],
                    [
                        lights[ix][iz],
                        lights[ix + 1][iz],
                        lights[ix][iz + 1],
                        lights[ix + 1][iz + 1],
                    ],
                    0,
                    COVER_PACKET,
                    true,
                );
            }
        }
    }

    fn goals(&mut self, view: &View) {
        // The far goal (+Z) is the one you shoot at, so it wears the
        // opponent's colour; your own net behind you is blue.
        for (z_line, color) in [
            (sim::HALF_Z, seat_signal(1)),
            (-sim::HALF_Z, seat_signal(0)),
        ] {
            let back = z_line + sim::GOAL_DEPTH * z_line.signum();
            // There used to be a guard here that skipped this whole box when
            // the camera was inside the goal, on the grounds that an unclipped
            // quad straddling the eye becomes a screen-wide slab. It was
            // unreachable: `keep_inside` holds the camera a wall margin inside
            // the pitch, so its |z| never exceeds HALF_Z - CAM_WALL_MARGIN and
            // the test needed |z| past HALF_Z. Removed rather than left to
            // send the next reader after the same red herring -- and the slab
            // it feared cannot happen now anyway, since `quad_biased` clips
            // the near plane.
            let (gw, gh) = (sim::GOAL_HALF_W, -sim::GOAL_H);
            // The box behind the net, dark rather than team-bright. It used to
            // be the net: a flat coloured slab with the team colour on it, which
            // read as a painted wall at the end of a tunnel because that is what
            // it was. Now it is only what shows through the mesh, so it wants to
            // be the inside of a goal, which is nearly black.
            self.quad(
                [
                    (-gw, 0, back),
                    (gw, 0, back),
                    (-gw, gh, back),
                    (gw, gh, back),
                ],
                // Neutral, not a fraction of the team colour: a sixth of orange
                // is brown, and the inside of a goal behind white netting wants
                // to look like shadow with a wash of the team in it.
                // Neutral, and deliberately not `tinted` by the team: a
                // fraction of orange is brown, and multiplying a dark grey by
                // orange is the same brown again. The team colour lives on the
                // posts and the frame, where it can be bright.
                [GOAL_VOID, GOAL_VOID, GOAL_VOID_HI, GOAL_VOID_HI],
            );
            for &sx in &[-1i32, 1] {
                let x = sx * gw;
                self.quad_flat(
                    [(x, 0, z_line), (x, 0, back), (x, gh, z_line), (x, gh, back)],
                    shade(color, 1, 5),
                );
            }
            self.quad_flat(
                [
                    (-gw, gh, z_line),
                    (gw, gh, z_line),
                    (-gw, gh, back),
                    (gw, gh, back),
                ],
                shade(color, 1, 5),
            );
            // Floor of the box. Dark for the same reason as the back panel: it
            // is most of what shows under the net from a chase camera, and in
            // team colour it read as a brown carpet.
            self.quad_flat(
                [
                    (-gw, 0, z_line),
                    (gw, 0, z_line),
                    (-gw, 0, back),
                    (gw, 0, back),
                ],
                GOAL_VOID,
            );
            // The netting itself: back wall, both sides and the roof, hung well
            // inside the box so the dark panels read as depth behind it rather
            // than as the net's own colour.
            //
            // One quad a face. The holes cost nothing: the GPU discards a texel
            // that resolves to 0x0000 through `COVER_CLUT`, so this is netting
            // without a strand of geometry per thread, and because the mesh block
            // is big enough for the widest face there is nothing to tile and no
            // seam to line up.
            //
            // Hung 60 uu clear of the box, not snug against it. The ordering
            // table quantises depth into slots about 27 uu apart out here and
            // prepends within a slot, so a net inset by less than a slot lands in
            // the same bucket as the panel behind it and draws first, which is to
            // say underneath. That showed netting across the top only, where
            // perspective happened to separate the two.
            let hang = 60;
            let inset = hang * z_line.signum();
            let (nw, nh) = (gw - hang, gh + hang);
            let far = back - inset;

            // Each face samples the mesh in proportion to its own size, so the
            // holes are square and a strand is the same distance from its
            // neighbour whichever face it is on. Half-open: a span of n texels is
            // `u0 .. u0 + n`, not `u0 .. u0 + n - 1`, which samples one fewer and
            // stretches them over the full width.
            let across = net_texels(2 * gw);
            let tall = net_texels(sim::GOAL_H);
            let deep = net_texels(sim::GOAL_DEPTH);
            let patch = |w: u8, h: u8| {
                [
                    uvw(NET_U0, NET_V0),
                    uvw(NET_U0 + w, NET_V0),
                    uvw(NET_U0, NET_V0 + h),
                    uvw(NET_U0 + w, NET_V0 + h),
                ]
            };

            // Shaded over the whole face rather than per patch, and white: a
            // near-white strand modulated by the team colour made yellow string
            // in one goal and blue in the other.
            let net_hi = shade(COVER_STRAND, 3400, 4096);
            let net_lo = shade(COVER_STRAND, 2400, 4096);

            // Back.
            self.quad_tex(
                [(-nw, 0, far), (nw, 0, far), (-nw, nh, far), (nw, nh, far)],
                patch(across, tall),
                [net_lo, net_lo, net_hi, net_hi],
                0,
                COVER_PACKET,
                true,
            );
            // Sides.
            for &sx in &[-1i32, 1] {
                let x = sx * nw;
                self.quad_tex(
                    [(x, 0, z_line), (x, 0, far), (x, nh, z_line), (x, nh, far)],
                    patch(deep, tall),
                    [net_lo, net_lo, net_hi, net_hi],
                    0,
                    COVER_PACKET,
                    true,
                );
            }
            // Roof.
            self.quad_tex(
                [
                    (-nw, nh, z_line),
                    (nw, nh, z_line),
                    (-nw, nh, far),
                    (nw, nh, far),
                ],
                patch(across, deep),
                [net_hi; 4],
                0,
                COVER_PACKET,
                true,
            );

            // Bright posts and crossbar, so the mouth pops out of the wall.
            let post = 34;
            for &sx in &[-1i32, 1] {
                let x = sx * gw;
                self.quad_flat(
                    [
                        (x - post, 0, z_line),
                        (x + post, 0, z_line),
                        (x - post, gh, z_line),
                        (x + post, gh, z_line),
                    ],
                    shade(color, 3, 2),
                );
            }
            self.quad_flat(
                [
                    (-gw, gh + post, z_line),
                    (gw, gh + post, z_line),
                    (-gw, gh - post, z_line),
                    (gw, gh - post, z_line),
                ],
                shade(color, 3, 2),
            );
        }
    }

    // ---- actors --------------------------------------------------------

    /// A patch on the floor under something airborne. Cheap, and without it you
    /// cannot tell a high ball from a near one. Takes separate half-extents
    /// because a car is two and a half times longer than it is wide, and a
    /// square shadow under it reads as a hole in the pitch.
    fn shadow(&mut self, x: i32, height: i32, z: i32, rx: i32, rz: i32, yaw: u16) {
        let h = height.max(0);
        let k = (4096 - (h * 2048 / (sim::CEIL / 3)).min(3000)).max(900);
        let (ex, ez) = ((rx * k) >> 12, (rz * k) >> 12);
        // Darken the grass rather than fading to black: a shadow is less light
        // on the same pitch, and at this size a black patch is louder than the
        // thing casting it. Sampling the pitch's own light keeps a shadow at
        // the lit centre darker than the pitch and one out at the touchline
        // from being brighter than what it lies on.
        let dim = 2048 + ((4096 - k) >> 1);
        // Halved, because the quads are drawn semi-transparent and the GPU
        // averages them with the pitch. Without this the blend lands halfway
        // back to the unshaded grass and the shadow all but disappears.
        let c = tinted(shade(GRASS_A, dim >> 1, 4096), floor_tint(x, z));

        // Project the rim. One ellipse, so a ball gets a circle and a car
        // gets the oblong its footprint actually is.
        let mut sp = [(0i16, 0i16); SHADOW_SIDES];
        let mut z_sum = 0i32;
        let mut on_screen = false;
        // Turned with its caster. The ellipse is built on the CPU anyway, so
        // yawing it is two multiplies a corner rather than the second transform
        // load an axis-aligned patch was avoiding. It has to turn now that it
        // is the size of the car: at a quarter of the footprint the body hid
        // the whole thing, which is why it looked like there was no shadow at
        // all, and the moment it reaches past the bodywork a patch pointing the
        // wrong way is the first thing you see.
        let (ys, yc) = (sin_q12(yaw) as i32, cos_q12(yaw) as i32);
        for (side, p) in sp.iter_mut().enumerate() {
            let a = ((4096 * side) / SHADOW_SIDES) as u16;
            let (ox, oz) = (
                (ex * cos_q12(a) as i32) >> 12,
                (ez * sin_q12(a) as i32) >> 12,
            );
            let px = x + ((ox * yc + oz * ys) >> 12);
            let pz = z + ((oz * yc - ox * ys) >> 12);
            let v = scene::project_vertex(Vec3I16::new(px as i16, -2, pz as i16));
            if v.sz == 0 {
                return;
            }
            *p = (v.sx, v.sy);
            z_sum += v.sz as i32;
            if on_view(v.sx, v.sy) {
                on_screen = true;
            }
        }
        if !on_screen {
            return;
        }
        let depth = z_sum / SHADOW_SIDES as i32 + SHADOW_DEPTH_BIAS;

        // Fan the octagon as a quad strip worked inwards from both ends, which
        // is how a convex polygon tiles with the GPU's own quad-to-triangle
        // split: (v0,v1,v2) then (v1,v2,v3). Three quads, six triangles, no
        // centre vertex and no degenerate slivers.
        let mut lo = 0usize;
        let mut hi = SHADOW_SIDES - 1;
        while hi - lo >= 3 {
            self.emit_blended([sp[hi], sp[lo], sp[hi - 1], sp[lo + 1]], depth, [c; 4]);
            lo += 1;
            hi -= 1;
        }
    }

    /// The ball, in its own object space so it can roll.
    fn ball(&mut self, s: &Sim, view: &View) {
        // Sim uses a right-handed Y-up world, where +X angular velocity is
        // the forward roll for travel toward +Z. The renderer then reflects Y
        // through `FLIP_Y`; a reflection reverses rotation handedness. Feeding
        // the physical angle through unchanged therefore made the visible
        // panels roll backward even though the contact physics was correct.
        let visible_roll = 0u16.wrapping_sub(s.ball.roll);
        let spin = rot_y_q12(s.ball.roll_dir).mul(&rot_x_q12(visible_roll));
        let world = spin.mul(&FLIP_Y);
        view.set_object(&world, (r(s.ball.p.x), ry(s.ball.p.y), r(s.ball.p.z)));

        // Ball to camera, in the same space the face normals come out in.
        // Computed once: the ball is small against the distance to the eye, so
        // one direction for the whole sphere is close enough, and erring this
        // way keeps a thin band of silhouette facets that a per-face eye
        // vector would drop. Keeping a facet costs a quad; dropping a visible
        // one puts a hole in the ball.
        let ball_pos = (r(s.ball.p.x), ry(s.ball.p.y), r(s.ball.p.z));
        let to_cam = (
            view.pos.0 - ball_pos.0,
            view.pos.1 - ball_pos.1,
            view.pos.2 - ball_pos.2,
        );

        // Sixteen columns around the ball is what stops the silhouette
        // reading as a polygon at one player's scale. At half the width, drawn
        // twice, eight is past the point where anybody counts them.
        let lon_step = if split_view() { 2 } else { 1 };
        let mesh = unsafe { &BALL_MESH };
        let mut sp = [[(0i16, 0i16); BALL_LON]; BALL_LAT + 1];
        let mut sz = [[0i32; BALL_LON]; BALL_LAT + 1];
        for j in 0..=BALL_LAT {
            // Project only the columns the quad loop below reads: a split
            // view was projecting all sixteen and then drawing every other
            // one, throwing half the GTE work away.
            for i in (0..BALL_LON).step_by(lon_step) {
                let v = mesh[j][i];
                let p = scene::project_vertex(Vec3I16::new(v.0 as i16, v.1 as i16, v.2 as i16));
                sp[j][i] = (p.sx, p.sy);
                sz[j][i] = p.sz as i32;
            }
        }
        for j in 0..BALL_LAT {
            for i in (0..BALL_LON).step_by(lon_step) {
                let i2 = (i + lon_step) % BALL_LON;
                if sz[j][i] == 0 || sz[j][i2] == 0 || sz[j + 1][i] == 0 || sz[j + 1][i2] == 0 {
                    continue;
                }
                // A quad's four corners already sit on the sphere, so their sum
                // points straight out of it: that is the normal, for free.
                let (a, b) = (mesh[j][i], mesh[j][i2]);
                let (c, d) = (mesh[j + 1][i], mesh[j + 1][i2]);
                let k = 1024 / sim::BALL_R.max(1);
                let n = apply(
                    &world,
                    (
                        (a.0 + b.0 + c.0 + d.0) * k,
                        (a.1 + b.1 + c.1 + d.1) * k,
                        (a.2 + b.2 + c.2 + d.2) * k,
                    ),
                );
                // Facing away: behind the front of the ball, always. Tested
                // before the lighting and the packet, so a culled facet costs
                // one dot product and nothing else.
                if n.0 * to_cam.0 + n.1 * to_cam.1 + n.2 * to_cam.2 <= 0 {
                    continue;
                }
                let dot = (n.0 * BALL_LIGHT.0 + n.1 * BALL_LIGHT.1 + n.2 * BALL_LIGHT.2) >> 12;
                let lit = (2500 + dot / 2).clamp(1100, 4096);
                // Panels: two facets wide, staggered a third of the way round
                // each band. The old pattern was six lone dark quads in an
                // irregular scatter, which read as blotches rather than as a
                // ball. Caps stay light, because a pattern that runs to a pole
                // turns into a pinwheel the moment the ball rolls one at you.
                let cap = j == 0 || j == BALL_LAT - 1;
                let dark = !cap && (i / 2 + j) % 3 == 0;
                let base: Rgb = if dark { (34, 34, 46) } else { (242, 242, 248) };
                let col = shade(base, lit, 4096);
                self.emit(
                    [sp[j][i], sp[j][i2], sp[j + 1][i], sp[j + 1][i2]],
                    (sz[j][i] + sz[j][i2] + sz[j + 1][i] + sz[j + 1][i2]) / 4,
                    [col; 4],
                );
            }
        }
    }

    /// The boost flame. The only thing drawn on the car by hand: brake lights
    /// and everything else come baked into the model's own materials.
    fn car_flame(&mut self, c: &sim::Car, view: &View) {
        if !c.boosting {
            return;
        }
        // Nothing doing if the car is behind the eye or almost on it. Quads are
        // not clipped against the camera plane on this hardware, so a plume with
        // one corner behind it projects to a screen-wide smear, and since the
        // opponent boosts constantly that smear was the only flame usually
        // visible. The mesh pass has a frustum test for the same reason.
        let depth = view.camera_space(car_ground(c)).2;
        // The +40 keeps the clamped tail below from ever needing a negative
        // length: at this depth the shortest tail still fits.
        if depth < DEPTH_RANGE.near() as i32 + sim::CAR_HALF_L + 40 {
            return;
        }
        // A car past the imposter distance in a half-width view is two flat
        // slabs; a plume on a car that is not there reads as a firefly.
        if split_view() && depth > FAR_CAR_DISTANCE {
            return;
        }
        // Same origin and orientation the mesh uses.
        view.set_object(&car_world(c), car_ground(c));
        let flick = if (c.wheel_spin >> 6) & 1 == 0 { 0 } else { 28 };

        // Off the back of the car, not out of the middle of it. This started at
        // the tail of the hitbox, which is 82 back, and 60 put the root a good
        // 20 uu inside the bodywork.
        let root = -sim::CAR_HALF_L;
        // An exhaust tail rather than a stub: three tapering, semi-transparent
        // segments running hot to ember, after WipEout's plumes. The taper is
        // the fade; the GPU's average blend cannot fade a quad to nothing, so
        // the shape thins to a point instead.
        //
        // Clamped so no corner reaches the camera plane: quads are not clipped
        // against it on this hardware, and a chase camera sits square in the
        // tail's path. The near guard above only covers the car itself.
        let reach = depth - DEPTH_RANGE.near() as i32 - sim::CAR_HALF_L - 16;
        let len = (170 + flick).min(reach).max(24);

        // Crossed sheets, one lying flat and one standing up, because a single
        // sheet trailing backwards is edge-on from exactly the angle the game is
        // played at. Low, so the tail clears the bodywork and its own shadow.
        let y = 20;
        let half_w = [14, 9, 5, 1];
        let half_h = [13, 8, 4, 1];
        let cols: [Rgb; 4] = [
            (255, 226, 130),
            (255, 140, 40),
            (255, 80, 24),
            (160, 40, 16),
        ];
        let z_at = |i: usize| root - len * i as i32 / 3;
        for i in 0..3 {
            let (z0, z1) = (z_at(i), z_at(i + 1));
            let shade = [cols[i], cols[i], cols[i + 1], cols[i + 1]];
            let (w0, w1) = (half_w[i], half_w[i + 1]);
            self.quad_blended(
                [(-w0, y, z0), (w0, y, z0), (-w1, y, z1), (w1, y, z1)],
                shade,
                FLAME_BIAS,
            );
            let (h0, h1) = (half_h[i], half_h[i + 1]);
            self.quad_blended(
                [
                    (0, y - h0, z0),
                    (0, y + h0, z0),
                    (0, y - h1, z1),
                    (0, y + h1, z1),
                ],
                shade,
                FLAME_BIAS,
            );
        }
    }
}

/// The car's orientation as an object-to-render matrix.
///
/// Built from the sim's own basis rather than from yaw, so a car on a wall
/// leans onto it instead of standing bolt upright with its wheels in the air.
/// The columns are the world images of the object's X, Y and Z axes, with Y
/// negated on the way out because the GTE draws with +Y down.
fn car_world(c: &sim::Car) -> Mat3I16 {
    let (right, up, fwd) = c.basis();
    let e = |v: i32| v.clamp(-32767, 32767) as i16;
    Mat3I16 {
        m: [
            [e(right.x), e(up.x), e(fwd.x)],
            [e(-right.y), e(-up.y), e(-fwd.y)],
            [e(right.z), e(up.z), e(fwd.z)],
        ],
    }
}

/// Where the car's mesh origin sits in render space: on the ground under the
/// hitbox centre, because that is where `tools/cook-models` puts it.
fn car_ground(c: &sim::Car) -> (i32, i32, i32) {
    (r(c.p.x), ry(c.p.y) + sim::CAR_REST_Y, r(c.p.z))
}

/// Draw the player car: a cooked mesh through the engine's Gouraud pass.
///
/// This is the part worth not hand-rolling. `submit_lit_mesh` projects every
/// vertex through the loaded transform, runs the GTE's lighting on it, culls
/// clockwise screen triangles, builds the packets, and inserts them in a
/// deterministic order. The alternative was another per-face normal-dot loop.
/// Draw both cars through one pass.
///
/// One pass and one packet arena for the pair, not one each: a second
/// `PrimitiveArena` over the same static rewinds it and overwrites the first
/// car's packets while the ordering table is still pointing at them, which
/// renders as a black screen rather than as a missing car.
fn draw_cars(
    s: &Sim,
    cars: [usize; SEATS],
    view: &View,
    ot: &mut OtFrame<'_, OT_DEPTH>,
    lights: &LightRig,
) {
    let mut tris = unsafe { PrimitiveArena::new(&mut CAR_TRIS) };
    let cull = view.cull();

    // One entry a seat, in the sim's own order: seat 0 defends -Z, seat 1
    // defends +Z. Both wear whatever the select screen gave them.
    for (seat, (body, which)) in [(&s.car, cars[0]), (&s.opponent, cars[1])]
        .into_iter()
        .enumerate()
    {
        // A car off screen still costs its whole mesh: every vertex projected
        // and lit, every face culled one at a time. One box test skips it.
        // Isotropic, because the car rolls: the bound has to hold at any
        // orientation, so it is the hitbox's half-diagonal on every axis.
        // A wreck is not on the pitch. The sim leaves it where it was hit so
        // the explosion has somewhere to happen, which means the renderer is
        // what has to take the car away.
        if body.wrecked() {
            continue;
        }
        let ground = car_ground(body);
        if !cull.visible(
            (ground.0, ground.1 - CAR_BOUND_R, ground.2),
            (CAR_BOUND_R, CAR_BOUND_R, CAR_BOUND_R),
        ) {
            continue;
        }
        // A half-width view swaps a distant car for the two-slab stand-in:
        // the mesh pass costs the same whether the car is 12 pixels or 300,
        // and at the split kickoff both views hold the opponent at ~4,600 uu.
        if split_view() && cull.flat_distance(ground.0, ground.2) > FAR_CAR_DISTANCE {
            draw_far_car(seat, body, view, &mut tris, ot);
            continue;
        }
        // Geometry comes from the tables `decode_car_geometry` filled at boot,
        // so the blob is never parsed again here: that was a header walk per
        // car per view, four of them in a split frame.
        let which = which.min(CAR_COUNT - 1);
        // Mid-flip, spin the car about the axis across its dodge direction:
        // yaw into the dodge frame, tumble about X, yaw back out. A forward
        // dodge front-flips, a sideways one barrel-rolls, which is the whole
        // reason the move looks like anything.
        let mut world = car_world(body);
        if body.dodge_timer > 0 {
            let done = (sim::DODGE_TICKS - body.dodge_timer) as i32;
            let spin = (done * 4096 / sim::DODGE_TICKS as i32) as u16;
            world = world
                .mul(&rot_y_q12(body.dodge_dir))
                .mul(&rot_x_q12(spin))
                .mul(&rot_y_q12(body.dodge_dir.wrapping_neg()));
        }
        let view_rot = view.v.mul(&world);
        let t = view.camera_space(car_ground(body));
        ActorTransform::at(Vec3World::from_raw(t.0, t.1, t.2))
            .with_rotation(view_rot)
            .load_gte();
        lights.for_object(&view_rot).load();
        let materials = unsafe { &PAINTED_GAME[seat] };
        // `Car::steer` stores tan(angle), because that is what the bicycle
        // model consumes. Convert it back to a signed turn for the authored
        // front-wheel groups. Wheel roll is about their local axle before the
        // steering yaw, exactly like a real front hub.
        let steer = atan2_q12(body.steer, 4096);
        let roll = rot_x_q12(body.wheel_spin);
        let pose = WheelPose {
            front: rot_y_q12(steer).mul(&roll),
            rear: roll,
            travel: [
                (body.suspension[0] as i32 >> sim::SUSPENSION_VISUAL_FP) as i16,
                (body.suspension[1] as i32 >> sim::SUSPENSION_VISUAL_FP) as i16,
            ],
        };
        let centres = unsafe { &CAR_WHEEL_CENTRES[which] };
        let n = staged!(S_CAR_PROJECT, {
            project_car_animated(which, materials, CAR_WHEELS[which], centres, &pose)
        });
        let projected = unsafe { &mut CAR_PROJ[..n] };
        let faces = unsafe { &CAR_FACES[which][..CAR_FACE_COUNT[which] as usize] };
        staged!(S_CAR_FACES, {
            submit_car_faces(faces, projected, &mut tris, ot)
        });
    }
}

/// How many rows the front end has, and where they sit on screen.
pub const MENU_ROWS: usize = 3;
const MENU_X: i16 = 10;
const MENU_W: i16 = 164;
const MENU_TOP: i16 = 156;
const MENU_STEP: i16 = 25;

/// Ordering-table slots the front end reserves for itself. The pitch and the
/// car both map through `DEPTH_RANGE` into the low slots at this camera
/// distance, so without reserving these the floor tiles interleave with the
/// panels and eat half of each one.
const UI_NIB_SLOT: usize = 0;
/// The sweeping highlight, in front of the unlit plate it fills.
const UI_FILL_SLOT: usize = 1;
const UI_PANEL_SLOT: usize = 2;
/// How tall a menu row's panel is.
pub const MENU_ROW_H: i16 = 21;

/// Screen Y of the top edge of a menu row's panel. The caller centres its own
/// type in `MENU_ROW_H`, because only it knows how tall its face is.
pub fn menu_row_y(row: usize) -> i16 {
    MENU_TOP + row as i16 * MENU_STEP
}

/// How far the top edge of a panel leads its bottom edge. The slant is the
/// whole look: square panels read as a debug list, sheared ones as a fascia.
/// Public so the now-playing popup can wear the same shear as the menu rows.
pub const MENU_SLANT: i16 = 9;

/// How many sim ticks the highlight takes to cross a row.
///
/// Ten, which is a sixth of a second and five rendered frames at the front
/// end's 30 Hz. Fast enough that holding a direction still feels like a list
/// rather than an animation waiting to finish, slow enough to be a sweep and
/// not a jump cut.
pub const MENU_SWEEP_TICKS: u32 = 10;

/// How far the highlight has crossed its row, Q12, `elapsed` ticks after the
/// selection last moved.
pub fn menu_sweep(elapsed: u32) -> i32 {
    ((elapsed.min(MENU_SWEEP_TICKS) * 4096) / MENU_SWEEP_TICKS) as i32
}

/// What the front-end row list should look like this frame.
pub struct MenuRows {
    pub selected: usize,
    pub rows: usize,
    /// How far the highlight has swept across the selected row, Q12.
    pub fill: i32,
}

/// State needed to put each select-screen option on the same angled fascia as
/// one title-menu option. Text is still drawn by `main` after the ordering
/// table; these are the separate car/paint bars, their compact paint swatches,
/// and the shared arena/win-condition bars.
pub struct SelectPanels {
    /// Screen Y of the P1/P2 heading. Each bar derives its Y from this.
    pub top: i16,
    /// Vertical distance between the car and paint option bars.
    pub row_step: i16,
    /// Screen Y of the one match-wide arena option beneath both player lists.
    pub arena_y: i16,
    /// Screen Y of the match-wide win-condition option below the arena.
    pub rule_y: i16,
    /// Which seat is presently controlled. In a two-pad game both are live.
    pub live: [bool; SEATS],
    /// A locked seat loses its row highlight and receives a green plate.
    pub ready: [bool; SEATS],
    /// Car, paint, arena, or win-condition row selected by each seat.
    pub selected: [usize; SEATS],
    /// Both seats staged, or seat 0 alone (solo practice, which stands its
    /// one car where the title does and drops the CPU panel).
    pub pair: bool,
}

/// Which screen-space fascia the front-end renderer should append over its
/// live arena. Keeping this explicit avoids using `None` to mean two different
/// things (match HUD versus select screen with custom panels).
pub enum FrontPanels {
    Title(MenuRows),
    Select(SelectPanels),
}

/// Screen-space panels for the front-end row list, appended to a frame that
/// has already drawn the arena behind them.
fn menu_panels(b: &mut Builder<'_>, menu: &MenuRows) {
    for row in 0..menu.rows.min(MENU_ROWS) {
        let y = menu_row_y(row);
        // Sheared: the top edge leads the bottom, so the column reads as
        // angled fascia rather than a stack of buttons. Sized close to the
        // type it holds, since a panel with a lot of air in it looks like
        // a placeholder.
        let h = MENU_ROW_H;
        // Every row gets the unlit plate, the selected one included: the
        // highlight is drawn over it and has not reached the right-hand end
        // yet, so the plate is what the rest of the row is made of.
        //
        // Shallow gradients. A steep one makes a 26-pixel panel read as a
        // 12-pixel band with its lower half lost in the pitch behind it.
        b.screen_quad(
            UI_PANEL_SLOT,
            [
                (MENU_X + MENU_SLANT, y),
                (MENU_X + MENU_W + MENU_SLANT, y),
                (MENU_X, y + h),
                (MENU_X + MENU_W, y + h),
            ],
            [(40, 46, 66), (40, 46, 66), (24, 28, 44), (24, 28, 44)],
        );
        if row != menu.selected {
            continue;
        }
        // The highlight fills in from the left. Both edges advance together,
        // so the sweep's leading edge keeps the panel's own slant instead of
        // running up it.
        let w = (MENU_W as i32 * menu.fill.clamp(0, 4096) / 4096) as i16;
        let (a, c) = ((92, 200, 242), (46, 138, 190));
        b.screen_quad(
            UI_FILL_SLOT,
            [
                (MENU_X + MENU_SLANT, y),
                (MENU_X + w + MENU_SLANT, y),
                (MENU_X, y + h),
                (MENU_X + w, y + h),
            ],
            [a, a, c, c],
        );
        b.screen_quad(
            UI_NIB_SLOT,
            [
                (MENU_X + MENU_SLANT, y),
                (MENU_X + MENU_SLANT + 5, y),
                (MENU_X, y + h),
                (MENU_X + 5, y + h),
            ],
            [(255, 232, 150); 4],
        );
    }
}

/// Two separate option bars per player, using the title menu's shear, gradient,
/// cyan fill and gold nib. P1's bars lean with the title rows; P2/CPU mirrors
/// each bar from the other side.
fn select_panels(b: &mut Builder<'_>, panels: &SelectPanels) {
    const W: i16 = 128;
    const H: i16 = 18;
    const SLANT: i16 = MENU_SLANT;

    let sheared = |x: i16, y: i16, w: i16, h: i16, lead: i16| {
        [(x + lead, y), (x + w + lead, y), (x, y + h), (x + w, y + h)]
    };

    for seat in 0..if panels.pair { SEATS } else { 1 } {
        // Solo, the one staged car stands at screen centre and the card
        // follows it there.
        let cx = if panels.pair {
            stage_car_screen_x(seat, true)
        } else {
            SCREEN_W / 2
        };
        let x = cx - W / 2;
        let lead = if seat == 0 { SLANT } else { -SLANT };
        let live = panels.live[seat];
        let ready = panels.ready[seat];
        let (top, bottom) = if ready {
            ((34, 72, 58), (20, 44, 36))
        } else if live {
            ((40, 46, 66), (24, 28, 44))
        } else {
            ((28, 32, 46), (16, 19, 31))
        };
        for row in 0..2 {
            let row_y = panels.top + 14 + row as i16 * panels.row_step;
            b.screen_quad(
                UI_PANEL_SLOT,
                sheared(x, row_y, W, H, lead),
                [top, top, bottom, bottom],
            );
            if live && !ready && panels.selected[seat] == row {
                // Start with the title menu's cyan and lean it toward the
                // seat's signal colour. P1 stays cool; P2 picks up its warmer
                // identity.
                let signal = seat_signal(seat);
                let hi = mix((92, 200, 242), signal, 5);
                let lo = mix((46, 138, 190), signal, 5);
                b.screen_quad(
                    UI_FILL_SLOT,
                    sheared(x, row_y, W, H, lead),
                    [hi, hi, lo, lo],
                );

                // Same gold five-pixel nib as the selected title row,
                // mirrored to the outer edge of P2's selected option.
                let nib_x = if seat == 0 { x } else { x + W - 5 };
                b.screen_quad(
                    UI_NIB_SLOT,
                    sheared(nib_x, row_y, 5, H, lead),
                    [(255, 232, 150); 4],
                );
            }

            if row == 1 {
                // The old 48x16 swatch consumed the only row available below
                // both player lists. Keep the same two authored body colours
                // as a compact underline inside the Paint option instead.
                let paint = PAINTS[unsafe { SEAT_PAINT[seat] }];
                let (sx, sy) = (cx - 16, row_y + H - 3);
                b.screen_quad(
                    UI_NIB_SLOT,
                    [(sx, sy), (sx + 16, sy), (sx, sy + 3), (sx + 16, sy + 3)],
                    [paint.1; 4],
                );
                b.screen_quad(
                    UI_NIB_SLOT,
                    [
                        (sx + 16, sy),
                        (sx + 32, sy),
                        (sx + 16, sy + 3),
                        (sx + 32, sy + 3),
                    ],
                    [paint.2; 4],
                );
            }
        }
    }

    // Shared match settings. Each symmetric trapezoid carries both player-card
    // shears at once, so it belongs to neither seat. Same width as the player
    // cards, so every bar on the screen is one size.
    const ARENA_X0: i16 = (SCREEN_W - W) / 2;
    const ARENA_X1: i16 = (SCREEN_W + W) / 2;
    const ARENA_INSET: i16 = MENU_SLANT;
    for &(y, row) in &[(panels.arena_y, 2usize), (panels.rule_y, 3usize)] {
        let rect = [
            (ARENA_X0 + ARENA_INSET, y),
            (ARENA_X1 - ARENA_INSET, y),
            (ARENA_X0, y + H),
            (ARENA_X1, y + H),
        ];
        b.screen_quad(
            UI_PANEL_SLOT,
            rect,
            [(40, 46, 66), (40, 46, 66), (24, 28, 44), (24, 28, 44)],
        );
        let focus = [
            panels.live[0] && !panels.ready[0] && panels.selected[0] == row,
            panels.live[1] && !panels.ready[1] && panels.selected[1] == row,
        ];
        if focus[0] || focus[1] {
            b.screen_quad(
                UI_FILL_SLOT,
                rect,
                [
                    (92, 200, 242),
                    (92, 200, 242),
                    (46, 138, 190),
                    (46, 138, 190),
                ],
            );
        }
        // A left or right nib records which pad currently owns the shared
        // row. If both players focus it, both edges light without duplicating
        // the option.
        if focus[0] {
            b.screen_quad(
                UI_NIB_SLOT,
                [
                    (ARENA_X0 + ARENA_INSET, y),
                    (ARENA_X0 + ARENA_INSET + 5, y),
                    (ARENA_X0, y + H),
                    (ARENA_X0 + 5, y + H),
                ],
                [(255, 232, 150); 4],
            );
        }
        if focus[1] {
            b.screen_quad(
                UI_NIB_SLOT,
                [
                    (ARENA_X1 - ARENA_INSET - 5, y),
                    (ARENA_X1 - ARENA_INSET, y),
                    (ARENA_X1 - 5, y + H),
                    (ARENA_X1, y + H),
                ],
                [(255, 232, 150); 4],
            );
        }
    }
}

/// Where the front-end camera stands, and what it looks at.
///
/// The front end used to stage the car on a private plane of grass with its
/// own floor tessellation, its own lighting falloff and its own detailed mesh.
/// It is the arena now, drawn by the match renderer from a parked camera. That
/// deleted a second floor renderer, a second car LOD and five embedded assets,
/// and it costs nothing extra: the arena was already inside the frame budget
/// and the plane was not much cheaper than it.
const STAGE_CAM_BACK: i32 = 520;
/// How far down the pitch the stage stands.
///
/// The halfway line. Parking it in front of a net is about a fifth cheaper,
/// because the far half of the arena falls behind the lens, but at that range
/// the goal fills the top of the frame and collides with the title. Neither
/// position misses a deadline, so this is the one that frames better.
pub const STAGE_Z: i32 = 0;
const STAGE_CAM_UP: i32 = 150;
const STAGE_CAM_PITCH: u16 = 168;
/// How far each seat's car stands from the middle on the select screen. Wide
/// enough that the two never overlap as they turn, close enough that both stay
/// large on a 320-line frame.
pub const STAGE_PAIR_X: i32 = 150;
/// The one car on the title screen stands right of centre, so the row list has
/// the left of the frame to itself.
pub const STAGE_SOLO_X: i32 = 150;

/// Screen X a staged car projects to, for the overlay to line its panel up
/// with. One place decides where a car stands and where its label goes.
pub fn stage_car_screen_x(seat: usize, pair: bool) -> i16 {
    let x = stage_car_x(seat, pair);
    (SCREEN_W as i32 / 2 + PROJ_H as i32 * x / STAGE_CAM_BACK) as i16
}

/// Where the front end parks a seat's car, in world uu. `main` writes these
/// into the sim so the ordinary match renderer draws them.
pub fn stage_car_x(seat: usize, pair: bool) -> i32 {
    if !pair {
        return STAGE_SOLO_X;
    }
    if seat == 0 {
        -STAGE_PAIR_X
    } else {
        STAGE_PAIR_X
    }
}

/// The front end: the arena, with the staged cars and the requested fascia
/// over the top.
///
/// Laid out after Rocket League's own main menu, which puts the list on one
/// side and the car on the other rather than centring either. Title rows use
/// the left-hand list; the select screen uses a mirrored card beneath each
/// staged car.
pub fn render_menu(s: &Sim, cars: [usize; SEATS], panels: FrontPanels, pair: bool, buffer_y: u16) {
    // The camera always looks straight down the pitch from the middle. On the
    // title that puts the one staged car in the right-hand third, clear of the
    // row list, without swinging the lens off it and foreshortening it into a
    // wedge.
    let _ = pair;
    let view = look_from(
        (0, -STAGE_CAM_UP, STAGE_Z - STAGE_CAM_BACK),
        0,
        STAGE_CAM_PITCH,
    );
    enter_view(Viewport::FULL, buffer_y);
    build_view(s, cars, view, Viewport::FULL, Some(panels), 0);
    submit_prepared();
}

// Borrowed slots from the engine's stage table. Nothing in this game uses
// rooms or props, so their ids are free to mean something else here.
#[cfg(feature = "profile")]
const S_FLOOR: u16 = telemetry::stage::ROOM;
#[cfg(feature = "profile")]
const S_WALLS: u16 = telemetry::stage::ROOM_SURFACE_DRAW;
#[cfg(feature = "profile")]
const S_TRIM: u16 = telemetry::stage::SKY;
#[cfg(feature = "profile")]
const S_PADS: u16 = telemetry::stage::BOX_PROPS;
#[cfg(feature = "profile")]
const S_BALL: u16 = telemetry::stage::IMAGE_PROPS;
#[cfg(feature = "profile")]
const S_CARS: u16 = telemetry::stage::MODEL_DRAW;
#[cfg(feature = "profile")]
const S_SETUP: u16 = telemetry::stage::CAMERA;
#[cfg(feature = "profile")]
const S_SUBMIT: u16 = telemetry::stage::OT_SUBMIT;
// Inside the car draw, which the split-screen budget made the stage worth
// taking apart. These four ids are the engine's textured-model slots; nothing
// in this game draws a textured model, so they are free to mean this.
#[cfg(feature = "profile")]
const S_CAR_PROJECT: u16 = telemetry::stage::TEXTURED_MODEL_PROJECT;
#[cfg(feature = "profile")]
const S_CAR_LAYER: u16 = telemetry::stage::MODEL_BOUNDS;
#[cfg(feature = "profile")]
const S_CAR_FACES: u16 = telemetry::stage::TEXTURED_MODEL_FACES;
#[cfg(feature = "profile")]
const S_CAR_FLUSH: u16 = telemetry::stage::TEXTURED_MODEL_JOINTS;
#[cfg(not(feature = "profile"))]
const S_FLOOR: u16 = 0;
#[cfg(not(feature = "profile"))]
const S_WALLS: u16 = 0;
#[cfg(not(feature = "profile"))]
const S_TRIM: u16 = 0;
#[cfg(not(feature = "profile"))]
const S_PADS: u16 = 0;
#[cfg(not(feature = "profile"))]
const S_BALL: u16 = 0;
#[cfg(not(feature = "profile"))]
const S_CARS: u16 = 0;
#[cfg(not(feature = "profile"))]
const S_SETUP: u16 = 0;
#[cfg(not(feature = "profile"))]
const S_SUBMIT: u16 = 0;
#[cfg(not(feature = "profile"))]
const S_CAR_PROJECT: u16 = 0;
#[cfg(not(feature = "profile"))]
const S_CAR_LAYER: u16 = 0;
#[cfg(not(feature = "profile"))]
const S_CAR_FACES: u16 = 0;
#[cfg(not(feature = "profile"))]
const S_CAR_FLUSH: u16 = 0;

/// Draw one frame of the match for one player, on the whole screen.
pub fn render(s: &Sim, cars: [usize; SEATS], ball_cam: bool, buffer_y: u16) {
    enter_view(Viewport::FULL, buffer_y);
    render_view(s, cars, ball_cam, &s.car, Viewport::FULL, 0);
    submit_prepared();
}

/// Draw one frame of a two-player match: player one on the left half of the
/// screen, player two on the right.
///
/// Two full passes, each with its own camera, ordering table and submission.
/// They cannot share one table: the scissor has to change between them, and
/// the drawing area is GPU state rather than something a packet carries.
///
/// The second pass is not a second frame's worth of work. Halving the viewport
/// halves the cull frustum, so each pass rejects roughly half the arena before
/// it reaches the GTE, and the rasteriser fills half as many pixels.
/// `swapped` puts player two on the left. Which side of a shared TV a player
/// sits on is a property of the room, not of the game, so it is something the
/// pause menu can change rather than something the seating has to match.
pub fn render_split(
    s: &Sim,
    cars: [usize; SEATS],
    ball_cam: [bool; 2],
    buffer_y: u16,
    swapped: bool,
) {
    let (near, far) = if swapped {
        (&s.opponent, &s.car)
    } else {
        (&s.car, &s.opponent)
    };
    let (near_cam, far_cam, near_slot, far_slot) = if swapped {
        (ball_cam[1], ball_cam[0], 1, 0)
    } else {
        (ball_cam[0], ball_cam[1], 0, 1)
    };
    for (vp, subject, cam, camera_slot) in [
        (Viewport::TOP, near, near_cam, near_slot),
        (Viewport::BOTTOM, far, far_cam, far_slot),
    ] {
        enter_view(vp, buffer_y);
        render_view(s, cars, cam, subject, vp, camera_slot);
        submit_prepared();
    }
    leave_view(buffer_y);
}

/// Build the ordering table for one view. The caller has already pointed the
/// GTE and the scissor at `vp` and is responsible for submitting.
fn render_view(
    s: &Sim,
    cars: [usize; SEATS],
    ball_cam: bool,
    subject: &sim::Car,
    vp: Viewport,
    camera_slot: usize,
) {
    // A goal takes the camera off your car and puts it on the ball, whichever
    // camera you were driving with. The original detonates its explosion
    // between the posts and looks at it; there is no reason to be watching a
    // stopped car at the other end of the pitch while that happens.
    // A goal puts every camera on the ball, whichever one you were driving
    // with. Not a cut to a new position: the same ball cam the triangle button
    // gives you, which keeps the shot behind your car and swings the aim onto
    // the ball rather than teleporting the lens into the net.
    let celebrating = s.goal_freeze > 0;
    let view = staged!(S_SETUP, {
        camera(
            s,
            subject,
            ball_cam || celebrating,
            !celebrating,
            camera_slot,
        )
    });
    build_view(s, cars, view, vp, None, subject.boost / sim::BOOST_SCALE);
}

/// Build the ordering table for one view from an already-chosen camera.
///
/// `front` selects title or player-choice fascia on the front end. Either
/// suppresses the match furniture -- scoreboard, clock and boost dial -- and
/// puts its own screen-space panels in their place. Everything else is the
/// arena exactly as a match draws it, which is the point: the menu stands in
/// the same building.
fn build_view(
    s: &Sim,
    cars: [usize; SEATS],
    view: View,
    vp: Viewport,
    front: Option<FrontPanels>,
    boost: i32,
) {
    // World-space rig into camera space once; each object then rotates it
    // into its own local frame.
    let lights = staged!(S_SETUP, { LIGHTS.rotated(&view.v) });

    // Phase 1: the procedural quads. `begin` clears the table.
    {
        let mut b = staged!(S_SETUP, {
            let mut b = Builder {
                ot: unsafe { OtFrame::begin(&mut OT) },
                arena: unsafe { PrimitiveArena::new(&mut QUADS) },
                textured: unsafe { PrimitiveArena::new(&mut TEX_QUADS) },
            };

            // Sky behind everything: screen-space, no geometry. Sized to the
            // viewport rather than the screen, so a split pass does not hand
            // the rasteriser a full-width quad to throw half of away.
            let (x0, x1) = (vp.x, vp.x + vp.w);
            let (y0, y1) = (vp.y, vp.y + vp.h);
            b.screen_quad(
                SKY_SLOT,
                [(x0, y0), (x1, y0), (x0, y1), (x1, y1)],
                arena_sky(&view),
            );
            // Two views butted together read as one broken picture without a
            // seam between them. Emitted per pass and clipped by the scissor,
            // so each half draws its own side of the line.
            if vp.h < SCREEN_H {
                let inner = if vp.y == 0 { vp.h } else { vp.y };
                b.screen_quad(
                    SPLIT_SEAM_SLOT,
                    [
                        (0, inner - SPLIT_SEAM_W),
                        (SCREEN_W, inner - SPLIT_SEAM_W),
                        (0, inner + SPLIT_SEAM_W),
                        (SCREEN_W, inner + SPLIT_SEAM_W),
                    ],
                    [(10, 12, 20); 4],
                );
            }
            view.set_world();
            b
        });
        let cull = view.cull();
        staged!(S_FLOOR, { b.floor(&cull) });
        staged!(S_PADS, { b.pads(s, &cull) });
        staged!(S_WALLS, {
            b.walls(&cull);
            b.lamps(&cull);
        });
        staged!(S_TRIM, {
            // The dial belongs to whoever is looking through this view, and
            // sits the same distance in from that view's right edge.
            if let Some(panels) = &front {
                match panels {
                    FrontPanels::Title(menu) => menu_panels(&mut b, menu),
                    FrontPanels::Select(select) => select_panels(&mut b, select),
                }
            } else {
                b.boost_gauge(boost_gauge_x(vp), boost_gauge_y(vp), boost);
                // Score and clock stay whole-screen and straddle the seam, the
                // way a split-screen game shares one scoreline. Each pass draws
                // the part of it the scissor lets through.
                b.scoreboard();
                b.goal_burst(s);
                b.demo_burst(s);
            }
            b.ceiling(&cull);
            b.goals(&view);
            b.shadow(
                r(s.ball.p.x),
                r(s.ball.p.y) - sim::BALL_R,
                r(s.ball.p.z),
                sim::BALL_R,
                sim::BALL_R,
                0,
            );
            // The car's own footprint, not half of it. It used to be half,
            // which from any camera above the bumper line put the entire
            // shadow underneath the car that cast it: the only way to see one
            // was to tint it, and the car read as floating on the grass.
            for body in [&s.car, &s.opponent] {
                if body.wrecked() {
                    continue;
                }
                b.shadow(
                    r(body.p.x),
                    r(body.p.y) - sim::CAR_REST_Y,
                    r(body.p.z),
                    sim::CAR_HALF_W,
                    sim::CAR_HALF_L,
                    body.yaw,
                );
            }
        });

        staged!(S_BALL, { b.ball(s, &view) });
        for body in [&s.car, &s.opponent] {
            if !body.wrecked() {
                b.car_flame(body, &view);
            }
        }
    }

    // Phase 2: the car mesh, appended into the same frame.
    let mut ot = unsafe { OtFrame::resume(&mut OT) };
    staged!(S_CARS, { draw_cars(s, cars, &view, &mut ot, &lights) });

    #[cfg(feature = "profile")]
    unsafe {
        telemetry::emit::counter(telemetry::counter::MODEL_OVERFLOW_FLAGS, QUADS_OVERFLOW);
        QUADS_OVERFLOW = 0;
        telemetry::emit::counter(telemetry::counter::TRI_PRIMITIVES, QUADS_OFFERED);
        telemetry::emit::counter(telemetry::counter::WORLD_COMMANDS, QUADS_KEPT);
        QUADS_OFFERED = 0;
        QUADS_KEPT = 0;
    }
}

/// Kick the ordering table prepared by [`render`] or [`render_menu`].
///
/// The scene uses the engine's queued contract: while the CPU prepares frame
/// N+1, the GPU rasterises frame N. Submission therefore has to remain
/// separate from packet construction, and immediate text waits until the
/// runner's overlay hook after the linked list has drained.
pub fn submit_prepared() {
    staged!(S_SUBMIT, {
        apply_arena_draw_mode();
        // Synchronous: kick the linked-list DMA and wait for the walk.
        // The GPU keeps rasterising afterwards; the engine's draw_sync
        // before the flip covers that tail, same as voxide's frame shape.
        unsafe { OtFrame::resume(&mut OT) }.submit();
    });
}
