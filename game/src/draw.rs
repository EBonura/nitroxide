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
use psx_engine::{ActorTransform, DepthRange, GpuPacket, OtFrame, Vec3World};
use psx_gpu::frame::PrimitiveArena;
use psx_gpu::material::{BlendMode, TextureMaterial, TexturedGouraudPacketMaterial};
use psx_gpu::ot::OrderingTable;
use psx_font::FontAtlas;
use psx_gpu::prim::{QuadFlat, QuadGouraud, QuadTexturedGouraud, QuadTexturedMaterial, TriGouraud};
use psx_gte::lighting::{Light, LightRig};
use psx_gte::math::{Mat3I16, Vec3I16, Vec3I32};
use psx_gte::scene::{self, project_vertex_scheduled as project};
use psx_gte::{mfc2, mtc2};
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

/// Project a quad's four corners: three through one RTPT and the fourth
/// through RTPS, the same per-vertex projection as four RTPS with half the
/// GTE round trips. `None` when any corner is behind the near plane;
/// otherwise the screen corners and the sum of their depths.
#[inline(always)]
fn project_quad(c: &[(i32, i32, i32); 4]) -> Option<([(i16, i16); 4], i32)> {
    let v = |k: usize| Vec3I16::new(c[k].0 as i16, c[k].1 as i16, c[k].2 as i16);
    let t = scene::project_triangle_scheduled(v(0), v(1), v(2));
    let d = project(v(3));
    if t[0].sz == 0 || t[1].sz == 0 || t[2].sz == 0 || d.sz == 0 {
        return None;
    }
    Some((
        [(t[0].sx, t[0].sy), (t[1].sx, t[1].sy), (t[2].sx, t[2].sy), (d.sx, d.sy)],
        t[0].sz as i32 + t[1].sz as i32 + t[2].sz as i32 + d.sz as i32,
    ))
}

/// Does a projected quad's bounding box overlap the current view?
///
/// Testing only whether one corner is inside is not conservative. A roof
/// patch close to the camera can surround the whole screen while all four of
/// its corners are outside, which made a visible part of the enclosure vanish.
#[inline]
fn quad_overlaps_view(sp: &[(i16, i16); 4]) -> bool {
    let (vx0, vx1, vy0, vy1) = unsafe { (VIEW_MIN_X, VIEW_MAX_X, VIEW_MIN_Y, VIEW_MAX_Y) };
    corners_overlap(
        sp.map(|c| c.0 as i32),
        sp.map(|c| c.1 as i32),
        (vx0 as i32, vx1 as i32, vy0 as i32, vy1 as i32),
    )
}

/// Does the box around four corners reach the view `(min x, max x, min y,
/// max y)`: the box's max at or past the min edge, its min short of the max
/// edge, on both axes?
///
/// The R3000 has no min or max instruction, and a box found corner by corner
/// costs a compare and a branch per corner per bound. The sign bits answer the
/// same question: every corner left of (above) the view leaves the AND of
/// their offsets from its near edge negative, every corner at or past the far
/// edge leaves the OR of their offsets from it non-negative, and the box
/// reaches the view when neither holds on either axis. Offsets of 16-bit
/// coordinates from a view edge cannot overflow.
#[inline(always)]
fn corners_overlap(x: [i32; 4], y: [i32; 4], (vx0, vx1, vy0, vy1): (i32, i32, i32, i32)) -> bool {
    let before_x = (x[0] - vx0) & (x[1] - vx0) & (x[2] - vx0) & (x[3] - vx0);
    let past_x = (x[0] - vx1) | (x[1] - vx1) | (x[2] - vx1) | (x[3] - vx1);
    let before_y = (y[0] - vy0) & (y[1] - vy0) & (y[2] - vy0) & (y[3] - vy0);
    let past_y = (y[0] - vy1) | (y[1] - vy1) | (y[2] - vy1) | (y[3] - vy1);
    (before_x | !past_x | before_y | !past_y) >= 0
}

/// Did the GTE clamp this screen coordinate? It stores -1024..=1023, so a
/// vertex at either end is short of its true place.
#[inline(always)]
const fn screen_saturated((x, y): (i16, i16)) -> bool {
    x <= -1024 || x >= 1023 || y <= -1024 || y >= 1023
}

/// Will the rasteriser draw both of this projected quad's triangles? It
/// drops a triangle two of whose vertices are 1024 or more pixels apart
/// across or 512 or more down.
#[inline]
fn gpu_draws_whole(sp: &[(i16, i16); 4]) -> bool {
    let fits = |t: [(i16, i16); 3]| {
        let (x0, x1) = (
            t[0].0.min(t[1].0).min(t[2].0),
            t[0].0.max(t[1].0).max(t[2].0),
        );
        let (y0, y1) = (
            t[0].1.min(t[1].1).min(t[2].1),
            t[0].1.max(t[1].1).max(t[2].1),
        );
        (x1 as i32 - x0 as i32) < 1024 && (y1 as i32 - y0 as i32) < 512
    };
    fits([sp[0], sp[1], sp[2]]) && fits([sp[1], sp[2], sp[3]])
}

/// The part of the screen segment `a`..`b` between the vertical lines `x = lo`
/// and `x = hi`, or `None` when none of it is. Ends keep their order.
fn clip_x(a: (i16, i16), b: (i16, i16), lo: i32, hi: i32) -> Option<((i16, i16), (i16, i16))> {
    let (ax, ay, bx, by) = (a.0 as i32, a.1 as i32, b.0 as i32, b.1 as i32);
    if (ax < lo && bx < lo) || (ax > hi && bx > hi) {
        return None;
    }
    let at = |x: i32, from: (i32, i32), to: (i32, i32)| {
        // Where the segment from `from` to `to` crosses `x`; it spans more than a
        // pixel across here, since one end is on each side.
        let (dx, dy) = (to.0 - from.0, to.1 - from.1);
        (x as i16, (from.1 + dy * (x - from.0) / dx) as i16)
    };
    let (p, q) = ((ax, ay), (bx, by));
    let a2 = if ax < lo {
        at(lo, p, q)
    } else if ax > hi {
        at(hi, p, q)
    } else {
        a
    };
    let b2 = if bx < lo {
        at(lo, q, p)
    } else if bx > hi {
        at(hi, q, p)
    } else {
        b
    };
    Some((a2, b2))
}

/// The nearest depth (SZ) at which the GTE still projects a vertex where it
/// belongs. RTPS saturates its divide once the depth is half the projection
/// plane distance or less, and a vertex that close lands short of its true
/// place without any sign of it: a pitch quad with one there met its
/// neighbour at an angle and left a wedge of sky between them.
const GTE_TRUE_SZ: i32 = PROJ_H as i32 / 2;

/// The depth a projected vertex must be past to count as drawn where it
/// belongs: [`GTE_TRUE_SZ`] in split screen, and in a full view only in
/// front at all, its old test. The pitch and wall passes that use it are
/// built twice, `SPLIT` or not, so a full view runs its old code with none
/// of the near-camera fix compiled in: inline, the fix's extra code cost a
/// full view 28k cycles a frame where both cars are close (train tape polls
/// 795..865, 505k to 533k, 2026-10-03).
#[inline]
const fn near_sz<const SPLIT: bool>() -> i32 {
    if SPLIT {
        GTE_TRUE_SZ
    } else {
        0
    }
}

/// What [`Builder::clip_piece`] draws a clipped quad as.
#[derive(Copy, Clone)]
enum Pieces {
    Floor,
    /// A flat-lit Gouraud quad of the goal box.
    Flat,
    /// The goal's floor: a Gouraud quad filed with the pitch, behind
    /// everything depth-sorted.
    GoalFloor,
    /// The goal's netting: a blended wall quad filed at its average depth,
    /// since the net hangs in front of whatever stands in the goal.
    Net {
        packet: TexturedGouraudPacketMaterial,
    },
    Wall {
        packet: TexturedGouraudPacketMaterial,
        blended: bool,
    },
}

/// One quad queued for [`Builder::clip_piece`].
#[derive(Copy, Clone)]
struct PieceJob {
    world: [(i32, i32, i32); 4],
    uvs: [(u8, u8); 4],
    tints: [u32; 4],
    kind: Pieces,
}
/// Quads a phase may queue for [`Builder::clip_piece`]: the few beside or
/// under the camera, with room to spare.
const MAX_PIECE_JOBS: usize = 128;
static mut PIECE_JOBS: [PieceJob; MAX_PIECE_JOBS] = [PieceJob {
    world: [(0, 0, 0); 4],
    uvs: [(0, 0); 4],
    tints: [0; 4],
    kind: Pieces::Floor,
}; MAX_PIECE_JOBS];
static mut PIECE_JOB_COUNT: usize = 0;

#[derive(Copy, Clone, PartialEq, Eq)]
enum EyeReach {
    Clear,
    Cuts,
    Behind,
}

/// Where a goal's whole box is against the eye (see `Builder::goals`).
#[derive(Copy, Clone, PartialEq, Eq)]
enum GoalReach {
    /// Every corner is clear of the near depth: plain projection.
    Clear,
    /// Some of it may reach the eye: test and clip quad by quad.
    Near,
    /// None of it can be seen (behind the eye, or outside the view): nothing
    /// to draw.
    Behind,
}

/// Goal-box quads the camera is too near to project (see [`Builder::goals`]):
/// nine to a goal, both goals.
const MAX_GOAL_JOBS: usize = 20;
static mut GOAL_JOBS: [PieceJob; MAX_GOAL_JOBS] = [PieceJob {
    world: [(0, 0, 0); 4],
    uvs: [(0, 0); 4],
    tints: [0; 4],
    kind: Pieces::Floor,
}; MAX_GOAL_JOBS];
static mut GOAL_JOB_COUNT: usize = 0;
/// The two corner buffers [`Builder::clip_piece`] cuts back and forth between.
static mut CLIP_BUFS: [[[i32; 11]; 9]; 2] = [[[0; 11]; 9]; 2];

/// One wall quad queued by index (span, column level, column, lower and
/// upper ring), with its UVs and tints. See [`Builder::queue_wall_quad`].
#[derive(Copy, Clone)]
struct WallJob {
    at: [u8; 5],
    uv: [u8; 4],
    tints: [u32; 4],
    covered: bool,
}
static mut WALL_JOBS: [WallJob; MAX_PIECE_JOBS] = [WallJob {
    at: [0; 5],
    uv: [0; 4],
    tints: [0; 4],
    covered: false,
}; MAX_PIECE_JOBS];
static mut WALL_JOB_COUNT: usize = 0;

/// Point the GTE and the GPU at one viewport, and set the bounds the rejection
/// tests use. `buffer_y` is where the engine's current back buffer starts in
/// VRAM, which is what turns a display-space viewport into a VRAM scissor.
///
/// The projection centre moves with the viewport, so vertices come out in
/// whole-screen coordinates and the framebuffer's own drawing offset still
/// puts them in the right buffer. Only the scissor has to know about VRAM.
fn enter_view(vp: Viewport, buffer_y: u16) {
    enter_view_cpu(vp);
    psx_gpu::set_draw_area(
        vp.x as u16,
        buffer_y + vp.y as u16,
        (vp.x + vp.w) as u16 - 1,
        buffer_y + (vp.y + vp.h) as u16 - 1,
    );
}

/// The CPU half of [`enter_view`]: the rejection bounds and the GTE's
/// projection centre, with no GP0 write, for a view whose scissor travels in
/// its own table ([`AreaPacket`]) while another table may still be walking.
fn enter_view_cpu(vp: Viewport) {
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
/// How long after a kickoff the camera stays directly behind the car even where
/// the end wall leaves less than `CAM_MIN_FLAT_DIST` of boom (the back-middle
/// spot has 330 uu), shortening the boom instead of sliding the eye sideways
/// along the wall: the first thing a player sees should be the car from behind.
/// A tick is a sixtieth of a second; the byte saturates at 255.
const CAM_KICKOFF_BEHIND_TICKS: u8 = 240;
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
/// The same limit for a half-height split view. The split keeps [`PROJ_H`],
/// so its vertical half-field is 60 lines, about 13 degrees against the full
/// screen's 25, and the full-screen limit left the car below the view.
/// `atan2_q12` is linear in the slope inside an octant, so this is not a
/// clean angle: it was set from headless captures (boot-split-play and
/// boot-split-wall, both seats) to keep the whole car inside the bottom of
/// the view with the ball still below the scoreboard.
const CAM_BALL_CAR_PITCH_SPLIT: i32 = 68;
/// Ball cam's eye height in a split view. From the full `CAM_HEIGHT` the car
/// sits about 22 degrees below a far ball, nearly the whole 26-degree split
/// field, so holding the car pushed the ball up under the scoreboard and the
/// view read as looking down from above. Only the ball cam that holds the
/// car uses it; the chase camera and the goal celebration keep `CAM_HEIGHT`.
const CAM_SPLIT_BALL_HEIGHT: i32 = 200;
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
    /// Sim tick this state was computed on.
    tick: u32,
    /// The sim's `kickoff_ticks` when it was computed. That counter only ever
    /// counts up until a kickoff resets it, so a smaller one now means the
    /// match was kicked off again (a goal, or a new match) since this state.
    kickoff: u8,
}

impl CameraState {
    const EMPTY: Self = Self {
        valid: false,
        offset: (0, 0, 0),
        yaw: 0,
        pitch: 0,
        tick: 0,
        kickoff: 0,
    };
}

static mut CHASE_CAMERAS: [CameraState; 2] = [CameraState::EMPTY; 2];
/// EXPERIMENT: the sim tick being rendered. The chase camera's step limits
/// were tuned per 30 Hz frame (two ticks); scaling them by the ticks since
/// the last update keeps the camera's catch-up speed the same at any rate.
static mut CAMERA_TICK: u32 = 0;
pub fn set_camera_tick(tick: u32) {
    unsafe { CAMERA_TICK = tick };
}
/// Drop the ordering table `render` built last frame without drawing it. A
/// frame that draws nothing (the attract demo's cuts) must call this, or the
/// next `render` submits a table from before the cut, whose packets a split
/// frame may since have overwritten.
pub fn drop_pending() {
    unsafe {
        PENDING = false;
        SPLIT_PENDING = false;
    }
}

/// Forget both chase cameras' history, so the next frame frames its car
/// from scratch instead of easing in from wherever the last match left it.
/// The attract demo calls it at the start of each match so every run of it
/// draws the same frames.
pub fn reset_cameras() {
    unsafe { CHASE_CAMERAS = [CameraState::EMPTY; 2] };
}
const DEPTH_RANGE: DepthRange = DepthRange::new(120, 14000);
const SKY_SLOT: usize = OT_DEPTH - 1;
/// Horizontal centre of the scoreboard's dark middle panel, which is what the
/// clock is centred on. Exported so the HUD text and the plate under it cannot
/// drift apart.
pub const HUD_CENTRE_X: i16 = 160;

/// Ticks the scoreboard takes to open from the in-play tab to the full
/// scoreboard, and to close again.
pub const SCOREBOARD_STEPS: u8 = 8;

/// Where the scoreboard's text goes at one point of its open/close animation.
/// Returned by [`scoreboard`], which drew the plates it has to sit on.
pub struct ScoreText {
    /// Horizontal centres of the blue and orange scores.
    pub score_x: [i16; 2],
    /// Top of the score digits.
    pub score_y: i16,
    /// Scores at 2x. They change size just past half way through the animation.
    pub big: bool,
    /// Top of the clock, which is centred on [`HUD_CENTRE_X`].
    pub clock_y: i16,
}

/// The scoreboard fascia: a dark plate for the clock with a team block either
/// side for the scores, drawn straight into the back buffer.
///
/// `open` runs from 0 to [`SCOREBOARD_STEPS`]. Closed is a 10-line tab, 56
/// pixels wide, with every digit at 1x: what a player driving needs, and
/// almost nothing of the picture. Open is the Rocket League pill: 18-line team
/// blocks with the scores at 2x and a 13-line clock plate between them, for
/// the moments the score is the news (kickoff, a goal, the final minute, the
/// pause menu). Every edge moves in a straight line between the two, so the
/// bottom slides down and the blocks spread out as it opens.
///
/// Drawn from the overlay rather than an ordering table. The tables of a split
/// game are built per view, so a fascia there was built twice and half of it
/// scissored away; and a single-player table goes to the GPU a frame after it
/// is built, which would leave the plates of a moving scoreboard a frame
/// behind the text on them. Each quad goes as the two triangles the GPU would
/// have split it into: the SDK has no immediate Gouraud quad.
///
/// The old HUD was bare text on the sky. It survived only because the top
/// of the screen happens to be dark, and put the ball over the crossbar and
/// it sat on grey wall instead. Nothing shipped on this hardware with a HUD
/// that thin: Gran Turismo 2, Colin McRae 2 and Crash Team Racing all give
/// their readouts a plate to live on, and the plate is what makes type read
/// at any brightness behind it.
///
/// The shear matches the front end's panels, so the two look like one game,
/// and the team blocks do the job the BLU and ORG labels used to: colour tells
/// you whose score is whose faster than three letters can.
pub fn scoreboard(open: u8) -> ScoreText {
    const PLATE: Rgb = (18, 22, 34);
    const PLATE_LO: Rgb = (28, 34, 50);
    const RULE: Rgb = (150, 178, 230);
    let k = open.min(SCOREBOARD_STEPS) as i32;
    let at = |tab: i16, pill: i16| {
        (tab as i32 + (pill as i32 - tab as i32) * k / SCOREBOARD_STEPS as i32) as i16
    };
    // Past half way, so a 2x digit never sits on a block still tab-sized.
    let big = 2 * k > SCOREBOARD_STEPS as i32;
    let (plate_l, plate_r, plate_h) = (at(146, 140), at(174, 180), at(10, 13));
    let (block_l, block_r, block_h) = (at(132, 114), at(188, 206), at(10, 18));
    let shear = at(2, 4);

    // The rasteriser's dither, which the arena draws with. Without it the
    // plate's gradient steps in visible bands.
    ARENA_MATERIAL.apply_draw_mode();
    hud_quad(
        [
            (plate_l, 0),
            (plate_r, 0),
            (plate_l, plate_h),
            (plate_r, plate_h),
        ],
        [PLATE, PLATE, PLATE_LO, PLATE_LO],
    );
    // The far colour is derived rather than authored: five eighths lands
    // between the two darker halves the fixed blue and orange used, and a
    // second authored colour per paint is one more thing to keep in step.
    // Only the outer edge is sheared; the inner one stays vertical so the two
    // blocks and the centre read as one bar.
    let blue = seat_signal(0);
    let blue_far = shade(blue, 5, 8);
    hud_quad(
        [
            (block_l + shear, 0),
            (plate_l, 0),
            (block_l, block_h),
            (plate_l, block_h),
        ],
        [blue_far, blue_far, blue, blue],
    );
    let orange = seat_signal(1);
    let orange_far = shade(orange, 5, 8);
    hud_quad(
        [
            (plate_r, 0),
            (block_r - shear, 0),
            (plate_r, block_h),
            (block_r, block_h),
        ],
        [orange_far, orange_far, orange, orange],
    );
    // A bright rule along the foot of the clock plate, so the fascia has an
    // edge rather than fading into whatever is behind it. It arrives with the
    // big scores; the tab is too small to carry one.
    if big {
        hud_quad(
            [
                (plate_l, plate_h - 1),
                (plate_r, plate_h - 1),
                (plate_l, plate_h),
                (plate_r, plate_h),
            ],
            [RULE; 4],
        );
    }
    ScoreText {
        score_x: [at(140, 128), at(181, 193)],
        score_y: if big { 1 } else { 0 },
        big,
        clock_y: at(0, 3),
    }
}

/// One screen-space Gouraud quad in PS1 vertex order (top left, top right,
/// bottom left, bottom right), drawn now.
fn hud_quad(v: [(i16, i16); 4], c: [Rgb; 4]) {
    psx_gpu::draw_tri_gouraud([v[0], v[1], v[2]], [c[0], c[1], c[2]]);
    psx_gpu::draw_tri_gouraud([v[1], v[2], v[3]], [c[1], c[2], c[3]]);
}
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

/// The pitch is not depth-sorted at all: it has its own slots at the back of
/// the table, behind every depth-sorted slot, and draws before anything else
/// in the arena. With the camera above it, a plane cannot cover anything that
/// stands on it, so there is nothing to sort, and the markings lie on it in
/// the next slot forward without a bias to tune. Sorted by depth, a marking
/// near the far edge of a big pitch quad would sort behind that quad's
/// centre and vanish under the grass.
///
/// Depth-sorted packets reach these slots only past about 13,900 uu, further
/// than the arena's diagonal.
///
/// Crack-underlay strips along subdivision-band edges come first. They show
/// only through the single-pixel holes the band boundary can still open.
const UNDERDRAW_SLOT: usize = STAND_SLOT - 1;
const FLOOR_SLOT: usize = UNDERDRAW_SLOT - 1;
const LINE_SLOT: usize = FLOOR_SLOT - 1;
/// Shadows sit between the pitch and the thing casting them.
const SHADOW_DEPTH_BIAS: i32 = 60;
/// Boost pads read as objects on the ground rather than paint, so they come
/// forward of the markings.
const PAD_BIAS: i32 = 30;
/// A distant pad orb's one colour: between its lit tips and dim sides.
const PAD_ORB_FAR: Rgb = (225, 173, 57);

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
// Pitch tiling. Finer than a flat surface needs, because a quad with a vertex
// behind the camera is dropped whole: small tiles lose a sliver, one big quad
// loses the entire floor.
const TILES_X: i32 = 8;
const TILES_Z: i32 = 10;
/// Segments per side wall, for the same reason. Six since the rounded
/// corner joints took their spans' cost: the straight run is shorter, and
/// near the camera each span still splits into three columns.
const WALL_SEGS: i32 = 6;
/// Radius of the curve where the floor rolls up into the wall, and the wall
/// rolls over into the ceiling. Rocket League's arena is a rounded tray, not a
/// box: these transitions are most of why it reads as an arena. Real ones are
/// about this size; RLBot's field tables ignore them, so this is eyeballed.
const RAMP_R: i32 = sim::WALL_RAMP_R;
/// The floor-to-wall curve's radius where a wall column stands at `x`, over
/// the side walls' [`RAMP_R`], in Q12: the swept profile's curve points are
/// scaled by it. Exactly 4096 on the side walls, so they sweep as before.
fn ramp_scale(x: i32) -> i32 {
    (sim::ramp_radius(x.abs()) << 12) / RAMP_R
}
/// A curve point of the swept profile at ramp scale `s` (see [`ramp_scale`]).
#[inline(always)]
fn ramp_point(p: (i32, i32), s: i32) -> (i32, i32) {
    ((p.0 * s) >> 12, (p.1 * s) >> 12)
}
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
// The pitch markings add the quads they shade (see LINE_FLAT_SPREAD).
// + the stands' aprons (up to three a piece, a dozen pieces in view), with slack.
const MAX_QUADS: usize = 576 + 48 + 32;

/// GP0 polygon-command bit 25: blend this primitive with what is already in
/// the framebuffer instead of overwriting it.
const SEMI_TRANSPARENT: u32 = 1 << 25;

/// The ball indicator: shown once the ball's underside is this high, its
/// hoop radius, the height at which its inner disc is smallest, and tints.
const BALL_RING_MIN_H: i32 = 150;
/// No bias. The pitch and its markings have their own slots behind every
/// sorted one (see [`FLOOR_SLOT`]), and shadows and pads sort with positive
/// biases, so the hoop already lands in front of all of them. The -200 it
/// used to carry dated from a depth-sorted pitch, and it also put the hoop
/// over any car standing near its far edge: an additive white band across
/// the car's rear (train tape, route tick 700).
const BALL_RING_BIAS: i32 = 0;
const BALL_RING_R: i32 = 190;
/// The hoop band: dark at its inner and outer radius, brightest halfway.
/// The span the ring palette's lit entries covered on the old textured hoop.
const BALL_HOOP_IN: i32 = 136;
const BALL_HOOP_OUT: i32 = 188;
const BALL_RING_FULL_H: i32 = 1400;
const BALL_RING_TINT: Rgb = (132, 140, 132);
const BALL_DISC_TINT: Rgb = (84, 94, 76);

/// Sides on a shadow disc.
///
/// Eight, drawn as three quads in a strip. A shadow is twenty to forty pixels
/// across, and at that size an octagon is a circle: the flats are under a
/// pixel of chord error each. Twelve sides cost two more quads to remove
/// something already invisible.
const SHADOW_SIDES: usize = 8;

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
/// stripe looks like, and costs 11 KB.
///
/// Held as GPU colour words (`pack_color`, high byte clear): a floor quad's
/// four tints are four aligned loads straight into the packet, where three
/// byte loads and the packing each cost a main-RAM stall and a handful of
/// shifts per corner.
static mut FLOOR_LIGHT: [[[u32; FLOOR_GZ]; FLOOR_GX]; 2] =
    [[[rgbc((128, 128, 128)); FLOOR_GZ]; FLOOR_GX]; 2];
/// Every pitch grid point pulled onto the floor's outline (`Builder::chamfer`),
/// at boot. The floor reads these instead of clamping each vertex a frame:
/// the rounded corner joints made that clamp four planes deep, and the
/// floor projects several hundred grid points a view.
static mut FLOOR_POS: [[(i16, i16); FLOOR_GZ]; FLOOR_GX] = [[(0, 0); FLOOR_GZ]; FLOOR_GX];

/// The pitch light before the goal mouths' team pools go over it.
static mut FLOOR_BASE: [[[u32; FLOOR_GZ]; FLOOR_GX]; 2] =
    [[[rgbc((128, 128, 128)); FLOOR_GZ]; FLOOR_GX]; 2];

/// A colour word back to its channels.
#[inline(always)]
const fn rgb_of(w: u32) -> Rgb {
    (w as u8, (w >> 8) as u8, (w >> 16) as u8)
}

/// A perimeter run of the swept wall profile.
#[derive(Copy, Clone)]
struct Span {
    a: (i32, i32),
    b: (i32, i32),
}

/// Where the straight side walls end on Z, and the straight end walls on X:
/// at the rounded corner joints' tangent points.
const SIDE_WALL_Z: i32 = sim::CORNER_JOINT_PTS[0].1;
const END_WALL_X: i32 = sim::CORNER_JOINT_PTS[5].0;
/// Side wall runs, one span per corner (round both joints and along the
/// corner plane), and the four end wall runs.
const SPAN_COUNT: usize = WALL_SEGS as usize * 2 + 4 + 4;
/// Light slots per span: a corner span has six columns at its finest.
const SLOTS: usize = 6;

/// The stands: a tiered slope outside the enclosure, seen through the
/// honeycomb. Its front edge stands just behind the wall above the rail, its
/// back edge high and far out, and it runs all the way round the octagon,
/// over both goals, as one ring of pieces: eight along each side, one per
/// corner and three along each end (the middle one over the goal).
const STAND_IN: i32 = 600;
const STAND_OUT: i32 = 2300;
const STAND_Y_IN: i32 = 560;
const STAND_Y_OUT: i32 = 1850;
/// Crowd texels per uu along the front edge: 16 uu a texel, the pitch's.
const STAND_UU_PER_TEXEL: i32 = 16;
const STAND_COUNT: usize = 2 * 8 + 4 + 2 * 3;
/// The deepest slot but the sky's. The stands are the inside of a convex
/// bowl seen from within it, so no piece hides another and they need no
/// sorting; everything in the arena is in front of them.
const STAND_SLOT: usize = SKY_SLOT - 1;
/// The apron under the stands, from the pitch's level up to where the crowd
/// starts: dark, and darker where it meets the ground. Without it a gap of sky
/// showed between the barrier's top and the crowd, a thin band all round the
/// arena (and cyan in the day look), and through the goal mouth.
const APRON_TOP: Rgb = (34, 34, 46);
const APRON_BOTTOM: Rgb = (10, 10, 18);
const STAND_TINT_IN: Rgb = (118, 118, 126);
const STAND_TINT_OUT: Rgb = (72, 72, 88);

#[derive(Copy, Clone)]
struct Stand {
    /// The piece's vertex grid at its finest: three rows from the front edge
    /// (at `STAND_IN`, low) to the back edge (`STAND_OUT`, high), four
    /// columns along it. A far piece draws the grid's corners only.
    grid: [[Vec3I16; 4]; 3],
    /// Box centre and half extents for the cull.
    centre: (i32, i32, i32),
    half: (i32, i32, i32),
    /// U at each grid column, and which end's palette the piece uses.
    u: [u8; 4],
    team: u8,
    /// Which octagon edge the piece stands on (see [`ApronEdge`]).
    edge: u8,
}

/// One octagon edge's apron, as a single quad when the edge can be drawn whole.
#[derive(Copy, Clone)]
struct ApronEdge {
    /// The front edge's two ends at the crowd's foot, then at the ground.
    top: [Vec3I16; 2],
    foot: [Vec3I16; 2],
    centre: (i32, i32, i32),
    half: (i32, i32, i32),
}
static mut APRON_EDGES: [ApronEdge; 8] = [ApronEdge {
    top: [Vec3I16::ZERO; 2],
    foot: [Vec3I16::ZERO; 2],
    centre: (0, 0, 0),
    half: (0, 0, 0),
}; 8];
static mut STANDS: [Stand; STAND_COUNT] = [Stand {
    grid: [[Vec3I16::ZERO; 4]; 3],
    centre: (0, 0, 0),
    half: (0, 0, 0),
    u: [0; 4],
    team: 0,
    edge: 0,
}; STAND_COUNT];

/// Lay the stand ring out once at boot. Each octagon vertex moves out along
/// its mitre, so neighbouring pieces meet with no gap at the corners.
fn build_stands() {
    const D: i32 = 2896; // 4096 / sqrt(2)
    let (hx, hz, cx, cz) = (sim::HALF_X, sim::HALF_Z, CORNER_X, CORNER_Z);
    // Octagon vertices, and the outward normal of the edge leaving each.
    let verts = [
        (-hx, -cz),
        (-hx, cz),
        (-cx, hz),
        (cx, hz),
        (hx, cz),
        (hx, -cz),
        (cx, -hz),
        (-cx, -hz),
    ];
    let normals = [
        (-4096, 0),
        (-D, D),
        (0, 4096),
        (D, D),
        (4096, 0),
        (D, -D),
        (0, -4096),
        (-D, -D),
    ];
    let mitre = |i: usize, o: i32| {
        let (n1, n2) = (normals[(i + 7) % 8], normals[i]);
        let dot = (n1.0 * n2.0 + n1.1 * n2.1) >> 12;
        let den = 4096 + dot;
        let v = verts[i];
        (
            v.0 + (n1.0 + n2.0) * o / den,
            v.1 + (n1.1 + n2.1) * o / den,
        )
    };
    let lerp = |a: (i32, i32), b: (i32, i32), t: i32, den: i32| {
        (a.0 + (b.0 - a.0) * t / den, a.1 + (b.1 - a.1) * t / den)
    };
    let mut k = 0;
    for e in 0..8 {
        let (f0, f1) = (mitre(e, STAND_IN), mitre((e + 1) % 8, STAND_IN));
        let (b0, b1) = (mitre(e, STAND_OUT), mitre((e + 1) % 8, STAND_OUT));
        {
            let at = |p: (i32, i32), y: i32| Vec3I16::new(p.0 as i16, y as i16, p.1 as i16);
            let (dx, dz) = ((f1.0 - f0.0).abs(), (f1.1 - f0.1).abs());
            unsafe {
                APRON_EDGES[e] = ApronEdge {
                    top: [at(f0, -STAND_Y_IN), at(f1, -STAND_Y_IN)],
                    foot: [at(f0, 0), at(f1, 0)],
                    centre: ((f0.0 + f1.0) / 2, -STAND_Y_IN / 2, (f0.1 + f1.1) / 2),
                    half: (dx / 2 + 1, STAND_Y_IN / 2 + 1, dz / 2 + 1),
                };
            }
        }
        // Split points along the edge, as fractions of 4096.
        let side = [0, 512, 1024, 1536, 2048, 2560, 3072, 3584, 4096];
        let goal_t = (cx - sim::GOAL_HALF_W) * 4096 / (2 * cx);
        let end = [0, goal_t, 4096 - goal_t, 4096];
        let cuts: &[i32] = match e {
            0 | 4 => &side,
            2 | 6 => &end,
            _ => &[0, 4096],
        };
        for w in cuts.windows(2) {
            let front = [lerp(f0, f1, w[0], 4096), lerp(f0, f1, w[1], 4096)];
            let back = [lerp(b0, b1, w[0], 4096), lerp(b0, b1, w[1], 4096)];
            let pts = [front[0], front[1], back[0], back[1]];
            let (mut x0, mut x1, mut z0, mut z1) = (i32::MAX, i32::MIN, i32::MAX, i32::MIN);
            for p in pts {
                x0 = x0.min(p.0);
                x1 = x1.max(p.0);
                z0 = z0.min(p.1);
                z1 = z1.max(p.1);
            }
            let (dx, dz) = (front[1].0 - front[0].0, front[1].1 - front[0].1);
            let len = isqrt_i32(dx * dx + dz * dz);
            let texels = (len / STAND_UU_PER_TEXEL).clamp(1, 127);
            let mut grid = [[Vec3I16::ZERO; 4]; 3];
            for (j, row) in grid.iter_mut().enumerate() {
                let j = j as i32;
                let a = lerp(front[0], back[0], j, 2);
                let b = lerp(front[1], back[1], j, 2);
                let y = -(STAND_Y_IN + (STAND_Y_OUT - STAND_Y_IN) * j / 2);
                for (i, p) in row.iter_mut().enumerate() {
                    let (x, z) = lerp(a, b, i as i32, 3);
                    *p = Vec3I16::new(x as i16, y as i16, z as i16);
                }
            }
            let u = core::array::from_fn(|i| (CROWD_U0 + texels * i as i32 / 3) as u8);
            unsafe {
                STANDS[k] = Stand {
                    grid,
                    centre: ((x0 + x1) / 2, -(STAND_Y_IN + STAND_Y_OUT) / 2, (z0 + z1) / 2),
                    half: (
                        (x1 - x0) / 2,
                        (STAND_Y_OUT - STAND_Y_IN) / 2,
                        (z1 - z0) / 2,
                    ),
                    u,
                    team: ((z0 + z1) > 0) as u8,
                    edge: e as u8,
                };
            }
            k += 1;
        }
    }
}
static mut SPANS: [Span; SPAN_COUNT] = [Span {
    a: (0, 0),
    b: (0, 0),
}; SPAN_COUNT];

/// Positions along a straight span the splitter can land a vertex on, as
/// twelfths: 0, 1/3, 1/2, 2/3, 1. Splits are 1, 2 or 3, so those five cover
/// every vertex a straight wall can emit, and the light for each is baked
/// once. A corner span's eight slots are its columns (`corner_slots`).
const SLOT_TWELFTHS: [i32; 5] = [0, 4, 6, 8, 12];

/// Baked wall light per span, ring and slot, as GPU colour words for the
/// same reason as [`FLOOR_LIGHT`].
static mut WALL_LIGHT: [[[u32; SLOTS]; PROFILE_LEN]; SPAN_COUNT] =
    [[[rgbc((128, 128, 128)); SLOTS]; PROFILE_LEN]; SPAN_COUNT];

/// The untinted wall light for every ring, kept so the team colours can be
/// laid over it again whenever a match changes them: the barrier below the
/// rail and the enclosure strands above it both take the colour of the half
/// they stand on.
///
/// Doing that per vertex per frame instead cost a mix and a multiply on every
/// corner of every lower-wall quad, which measured as eleven dropped frames in
/// nine hundred. The colours change twice a match at the very most.
static mut WALL_BASE: [[[Rgb; SLOTS]; PROFILE_LEN]; SPAN_COUNT] =
    [[[(128, 128, 128); SLOTS]; PROFILE_LEN]; SPAN_COUNT];
/// Where each span's slots sit in Z, for the barrier's blend.
static mut CURB_Z: [[i32; SLOTS]; SPAN_COUNT] = [[0; SLOTS]; SPAN_COUNT];

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
/// Light at every roof patch corner, baked once with `CEIL_LIGHT`, as the
/// GPU colour words the patches carry. The draw used to re-blend the four
/// roof corners for all four corners of every patch every frame (twelve
/// mixes a patch, about a hundred patches a view), which was the single
/// largest cost of a split frame; the roof never moves. Words rather than
/// RGB triples: a byte at a time was twelve main-RAM loads a patch.
static mut ROOF_CORNER_LIGHT: [[u32; ROOF_ROWS + 1]; ROOF_COLS + 1] =
    [[rgbc((128, 128, 128)); ROOF_ROWS + 1]; ROOF_COLS + 1];
/// The same corners before the team colours go over them.
static mut ROOF_BASE: [[Rgb; ROOF_ROWS + 1]; ROOF_COLS + 1] =
    [[(128, 128, 128); ROOF_ROWS + 1]; ROOF_COLS + 1];
/// World X of roof corner column `ix` (the last column is clipped to the
/// roof edge, as the patch walk always did).
const fn roof_corner_x(ix: usize) -> i32 {
    let x = -ROOF_HALF_X + ix as i32 * ROOF_STEP_X;
    if x < ROOF_HALF_X { x } else { ROOF_HALF_X }
}
/// Cover texels across each roof column, for its patches' U range.
const ROOF_PATCH_W: [u8; ROOF_COLS] = {
    let mut w = [0u8; ROOF_COLS];
    let mut ix = 0;
    while ix < ROOF_COLS {
        w[ix] = cover_texels(roof_corner_x(ix + 1) - roof_corner_x(ix));
        ix += 1;
    }
    w
};
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
    rgb_of(unsafe { FLOOR_LIGHT[0][gx][gz] })
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
                unsafe { FLOOR_POS[gx][gz] = (cx as i16, cz as i16) };
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
                let w = rgbc((
                    ((c.0 as i32 * k) >> 8).clamp(0, 255) as u8,
                    ((c.1 as i32 * k) >> 8).clamp(0, 255) as u8,
                    ((c.2 as i32 * k) >> 8).clamp(0, 255) as u8,
                ));
                unsafe {
                    FLOOR_LIGHT[stripe][gx][gz] = w;
                    FLOOR_BASE[stripe][gx][gz] = w;
                }
            }
        }
    }
    paint_lines();

    // Walls. One tint per (span, ring, split position).
    let profile = Builder::profile();
    for si in 0..SPAN_COUNT {
        let slots = unsafe { SPAN_SLOT[si] };
        for (ri, &point) in profile.iter().enumerate() {
            for (slot, &(sx, sz, nx, nz)) in slots.iter().enumerate() {
                // Lit where the column's own floor curve puts the point.
                let (inset, height) = if ri <= CURVE_SEGS {
                    ramp_point(point, ramp_scale(sx))
                } else {
                    point
                };
                let at = (sx, sz);
                let n = (nx, nz);
                let p = (
                    at.0 + ((n.0 * inset) >> 12),
                    -height,
                    at.1 + ((n.1 * inset) >> 12),
                );
                let c = if ri == RAIL_LO_RING || ri == RAIL_HI_RING {
                    RAIL_TINT
                } else {
                    let c = lamp_light(p, (n.0, 0, n.1));
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
                    WALL_LIGHT[si][ri][slot] = rgbc(c);
                    WALL_BASE[si][ri][slot] = c;
                    CURB_Z[si][slot] = at.1;
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
            let c = mix(mix(l[0], l[1], tx), mix(l[2], l[3], tx), tz);
            unsafe {
                ROOF_CORNER_LIGHT[ix][iz] = rgbc(c);
                ROOF_BASE[ix][iz] = c;
            }
        }
    }
}

/// The columns a span is drawn with at one level of detail: where each
/// stands on the floor, the inward normal its profile is swept along, which
/// baked light slot it reads, and the panel and cover U it carries.
#[derive(Copy, Clone)]
struct SpanCols {
    count: usize,
    x: [i32; SLOTS],
    z: [i32; SLOTS],
    nx: [i32; SLOTS],
    nz: [i32; SLOTS],
    slot: [u8; SLOTS],
    panel_u: [u8; SLOTS],
    cover_u: [u8; SLOTS],
    /// The floor-to-wall curve's radius at each column over [`RAMP_R`], Q12:
    /// 4096 along the side walls, less along the end walls (see
    /// `sim::ramp_radius`).
    ramp: [u16; SLOTS],
}
const EMPTY_COLS: SpanCols = SpanCols {
    count: 0,
    x: [0; SLOTS],
    z: [0; SLOTS],
    nx: [0; SLOTS],
    nz: [0; SLOTS],
    slot: [0; SLOTS],
    panel_u: [0; SLOTS],
    cover_u: [0; SLOTS],
    ramp: [0; SLOTS],
};
/// Per span, the columns at one, two and three splits' worth of detail.
static mut SPAN_COLS: [[SpanCols; 3]; SPAN_COUNT] = [[EMPTY_COLS; 3]; SPAN_COUNT];
/// Per span and light slot: floor position and inward normal (x, z, nx, nz).
static mut SPAN_SLOT: [[(i32, i32, i32, i32); SLOTS]; SPAN_COUNT] =
    [[(0, 0, 0, 0); SLOTS]; SPAN_COUNT];

/// Which slots a straight span's columns use at each level of detail:
/// twelfths 0..12, 0..6..12, 0..4..8..12 (see [`SLOT_TWELFTHS`]).
const STRAIGHT_LODS: [&[usize]; 3] = [&[0, 4], &[0, 2, 4], &[0, 1, 3, 4]];
/// A corner span's slots are sim::CORNER_JOINT_PTS: the side wall's
/// tangent point, the side joint's middle, the corner plane's two ends, the
/// end joint's middle and the end wall's tangent point. Near the camera each
/// joint is drawn with both its chords, the same two the simulation drives
/// on; further away with one.
const CORNER_LODS: [&[usize]; 3] = [&[0, 2, 3, 5], &[0, 2, 3, 5], &[0, 1, 2, 3, 4, 5]];

/// Lay out the wall perimeter. Same order the old `walls` loop drew it in;
/// pulling it into a table is what lets the light bake index a span.
///
/// The straight walls stop at the rounded corner joints' tangent points
/// (sim::CORNER_JOINT_PTS), and each corner is one span that
/// runs round the side joint, along the corner plane and round the end
/// joint, with a normal per column. One span rather than one per chord: the
/// chords share their columns' projections. A span per chord cost the train
/// tape a sixth of its frames at 60.
fn build_spans() {
    let mut i = 0;
    let mut put = |slots: [(i32, i32, i32, i32); SLOTS], lods: [&[usize]; 3]| {
        let (first, last) = (slots[0], slots[SLOTS - 1]);
        unsafe {
            SPANS[i] = Span {
                a: (first.0, first.1),
                b: (last.0, last.1),
            };
            SPAN_SLOT[i] = slots;
        }
        // Distance along the span to each slot, for the U ranges.
        let mut along = [0i32; SLOTS];
        for s in 1..SLOTS {
            let (dx, dz) = (slots[s].0 - slots[s - 1].0, slots[s].1 - slots[s - 1].1);
            along[s] = along[s - 1] + isqrt_i32(dx * dx + dz * dz);
        }
        let total = along[SLOTS - 1].max(1);
        let cover = cover_texels(total) as i32;
        for (level, cols) in lods.iter().enumerate() {
            let mut out = EMPTY_COLS;
            out.count = cols.len();
            for (k, &s) in cols.iter().enumerate() {
                out.x[k] = slots[s].0;
                out.z[k] = slots[s].1;
                out.nx[k] = slots[s].2;
                out.nz[k] = slots[s].3;
                out.slot[k] = s as u8;
                out.panel_u[k] = (64 + 32 * along[s] / total).min(95) as u8;
                out.cover_u[k] = (COVER_U0 as i32 + cover * along[s] / total) as u8;
                out.ramp[k] = ramp_scale(slots[s].0) as u16;
            }
            unsafe { SPAN_COLS[i][level] = out };
        }
        i += 1;
    };
    // A straight run: five slots at SLOT_TWELFTHS, the rest repeating the
    // last, so a slot table can always be read whole.
    let straight = |a: (i32, i32), b: (i32, i32), n: (i32, i32)| {
        let mut s = [(b.0, b.1, n.0, n.1); SLOTS];
        for (k, &t) in SLOT_TWELFTHS.iter().enumerate() {
            s[k] = (a.0 + (b.0 - a.0) * t / 12, a.1 + (b.1 - a.1) * t / 12, n.0, n.1);
        }
        s
    };
    for k in 0..WALL_SEGS {
        let z0 = -SIDE_WALL_Z + 2 * SIDE_WALL_Z * k / WALL_SEGS;
        let z1 = -SIDE_WALL_Z + 2 * SIDE_WALL_Z * (k + 1) / WALL_SEGS;
        put(straight((-sim::HALF_X, z0), (-sim::HALF_X, z1), (4096, 0)), STRAIGHT_LODS);
        put(straight((sim::HALF_X, z0), (sim::HALF_X, z1), (-4096, 0)), STRAIGHT_LODS);
    }
    for &sx in &[-1i32, 1] {
        for &sz in &[-1i32, 1] {
            put(corner_slots(sx, sz), CORNER_LODS);
        }
    }
    // Last, so the goal lintel in `walls` finds them at SPAN_COUNT - 4.
    for &sz in &[-1i32, 1] {
        let z = sz * sim::HALF_Z;
        let gw = sim::GOAL_HALF_W;
        put(straight((-END_WALL_X, z), (-gw, z), (0, -sz * 4096)), STRAIGHT_LODS);
        put(straight((gw, z), (END_WALL_X, z), (0, -sz * 4096)), STRAIGHT_LODS);
    }
}

/// The six slots of the corner span in quadrant (`sx`, `sz`): the joint
/// vertices the simulation uses, each with the normal its profile is swept
/// along. The two tangent points take their straight wall's normal, so the
/// sweep meets the wall's own without a step; the others take the bisector
/// of the two faces that meet there.
fn corner_slots(sx: i32, sz: i32) -> [(i32, i32, i32, i32); SLOTS] {
    let planes = sim::CORNER_JOINT_PLANES;
    const D: i32 = 2896; // the corner plane, 4096 / sqrt(2)
    let faces = [
        (4096, 0),
        (planes[0].0, planes[0].1),
        (planes[1].0, planes[1].1),
        (D, D),
        (planes[2].0, planes[2].1),
        (planes[3].0, planes[3].1),
        (0, 4096),
    ];
    let mut out = [(0, 0, 0, 0); SLOTS];
    for (k, &(px, pz)) in sim::CORNER_JOINT_PTS.iter().enumerate() {
        let (nx, nz) = match k {
            0 => faces[0],
            5 => faces[6],
            _ => {
                // Faces either side of vertex k: chords before and after,
                // with the corner plane between vertices 2 and 3.
                let (f0, f1) = match k {
                    1 => (faces[1], faces[2]),
                    2 => (faces[2], faces[3]),
                    3 => (faces[3], faces[4]),
                    _ => (faces[4], faces[5]),
                };
                let (bx, bz) = (f0.0 + f1.0, f0.1 + f1.1);
                let len = isqrt_i32(bx * bx + bz * bz).max(1);
                (bx * 4096 / len, bz * 4096 / len)
            }
        };
        out[k] = (sx * px, sz * pz, -sx * nx, -sz * nz);
    }
    out
}

// ---- arena texture ---------------------------------------------------------
// One 4bpp page holds the 64x64 pitch tile, the 32x32 wall panel, the 128x84
// honeycomb enclosure, a separate 96x48 square goal net, and two more pages of
// full-resolution grass with the pitch markings composited into it. The pitch
// no longer samples those two pages or the end tiles: the markings are
// geometry (see [`LINE_SECTIONS`]). Separate CLUTs let the solid surfaces and
// two open meshes share the asset without trying to share a sixteen-colour
// palette.

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
/// The ball's sixteen colours (tools/cook-arena `BALL_PALETTE`), in the row
/// the boost pads' palette had before they went back to untextured orbs.
const BALL_CLUT: Clut = Clut::new(384, 261);
/// A spent pad's old palette: unsampled, uploaded because the atlas carries
/// it.
const SPENT_CLUT: Clut = Clut::new(384, 262);
/// The ball's texture (tools/cook-arena `ball_texture`): below the glow tile,
/// eight texels a column of facets, the latitude rows at `BALL_ROW_V`.
const BALL_U0: u8 = 0;
const BALL_V0: u8 = 144;
const BALL_ROW_V: [u8; BALL_LAT + 1] = [0, 13, 26, 38, 51, 64];
const BALL_PACKET: TexturedGouraudPacketMaterial =
    TextureMaterial::new(BALL_CLUT.uv_clut_word(), TEX_TPAGE.uv_tpage_word(0))
        .with_dither(true)
        .textured_gouraud_packet_material();
/// A plain radial falloff for every other light sprite.
const GLOW_CLUT: Clut = Clut::new(384, 263);
/// The glow tile's outer rings only. Unsampled since the hoop became a ring
/// of quads; uploaded because the atlas carries it.
const RING_CLUT: Clut = Clut::new(384, 264);
/// The 32x32 radial glow tile, below the goal net in the base page.
const GLOW_U0: u8 = 0;
const GLOW_V0: u8 = 112;
const GLOW_W: u8 = 32;
/// Grass occupies a 64x64 square at the origin, the wall a 32x32 tile beside
/// it, the goal net sits directly below both, and the honeycomb fills the
/// upper-right. Two following source pages are four by four 64-pixel marked
/// grass tiles each. They upload into the free VRAM columns on either side of
/// the HUD page rather than sitting contiguously at runtime.
const TEX_W: usize = 256;
const TEX_H: usize = 256 * 3;
const GRASS_TILE_W: i32 = 64;
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
/// The crowd tile (tools/cook-arena `CROWD_*`): tiers of fans under the
/// honeycomb's rows in the base page, the front tier's fascia at the bottom.
const CROWD_U0: i32 = 128;
const CROWD_V0: u8 = 88;
const CROWD_H: u8 = 40;
const CROWD_CLUTS: [Clut; 2] = [Clut::new(384, 265), Clut::new(384, 266)];
const CROWD_PACKETS: [TexturedGouraudPacketMaterial; 2] = [
    TextureMaterial::new(CROWD_CLUTS[0].uv_clut_word(), TEX_TPAGE.uv_tpage_word(0))
        .with_dither(true)
        .textured_gouraud_packet_material(),
    TextureMaterial::new(CROWD_CLUTS[1].uv_clut_word(), TEX_TPAGE.uv_tpage_word(0))
        .with_dither(true)
        .textured_gouraud_packet_material(),
];
/// Additive (B + F): light added to the frame rather than averaged into it. A
/// lit object can only ever brighten what it covers. The blend lives in the
/// material, which owns the tpage word's blend bits.
const GLOW_PACKET: TexturedGouraudPacketMaterial =
    TextureMaterial::new(GLOW_CLUT.uv_clut_word(), TEX_TPAGE.uv_tpage_word(0))
        .with_blend_mode(BlendMode::Add)
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

/// Tint the enclosure above the rail, and the roof, by the half it covers.
///
/// Stronger than the barrier's: the strand texel is near white and drawn
/// half-transparent, so a hue pulled toward neutral the way the barrier's is
/// reads as grey from any distance. At full strength each end of the arena
/// reads as its team from every camera, which is what Rocket League's halves
/// do, for no primitives and no VRAM: the colour rides on vertex tints the
/// quads already carry.
///
/// The hue is carried at a brightest channel of 128, the GPU's 1.0, and the
/// baked light only sets how bright it is, never its colour. A tint much past
/// 128 clips the near-white strand's strongest channels first, and an orange
/// tint clipped that way comes out yellow; the light's own colour carries
/// the pitch's green bounce, which turned the orange end olive.
fn glow(light: Rgb, z: i32) -> Rgb {
    const BLEND: i32 = 1400;
    let t = ((z + BLEND) * 16 / (2 * BLEND)).clamp(0, 16);
    let (a, b) = unsafe { (SEAT_GLOW[0], SEAT_GLOW[1]) };
    let hue = mix(a, b, t);
    let lum = (light.0.max(light.1).max(light.2) as i32).clamp(64, 176);
    let k = |c: u8| (c as i32 * lum / 128).min(255) as u8;
    (k(hue.0), k(hue.1), k(hue.2))
}

/// Lay the seats' colours back over the barrier's, the enclosure's and the
/// roof's baked light. Runs when a match sets its paints, not when it draws a
/// frame.
fn paint_curb() {
    for si in 0..SPAN_COUNT {
        for ri in 0..PROFILE_LEN {
            for slot in 0..SLOTS {
                unsafe {
                    let (base, z) = (WALL_BASE[si][ri][slot], CURB_Z[si][slot]);
                    WALL_LIGHT[si][ri][slot] = rgbc(if ri <= RAIL_LO_RING {
                        curb(base, z)
                    } else if ri >= RAIL_HI_RING {
                        glow(base, z)
                    } else {
                        base
                    });
                }
            }
        }
    }
    for ix in 0..=ROOF_COLS {
        for iz in 0..=ROOF_ROWS {
            unsafe { ROOF_CORNER_LIGHT[ix][iz] = rgbc(glow(ROOF_BASE[ix][iz], roof_corner_z(iz))) };
        }
    }
    paint_goal_pools();
}

/// Throw each goal's colour onto the pitch in front of its mouth: the lit
/// goal box is a light, and a light with no pool under it is a sticker.
/// Baked into the pitch light, so it costs nothing a frame.
fn paint_goal_pools() {
    let step_x = sim::HALF_X * 2 / TILES_X;
    let step_z = sim::HALF_Z * 2 / TILES_Z;
    let (blue, orange) = unsafe { (SEAT_GLOW[0], SEAT_GLOW[1]) };
    for stripe in 0..2 {
        for gx in 0..FLOOR_GX {
            for gz in 0..FLOOR_GZ {
                let x = -sim::HALF_X + gx as i32 * step_x / FLOOR_SPLIT_MAX;
                let z = -sim::HALF_Z + gz as i32 * step_z / FLOOR_SPLIT_MAX;
                let (cx, cz) = Builder::chamfer(x, z);
                let base = rgb_of(unsafe { FLOOR_BASE[stripe][gx][gz] });
                // Elliptical falloff, Q8: wide along the goal line, short
                // out into the pitch.
                let d = sim::HALF_Z - cz.abs();
                let e = (cx * cx / (GOAL_POOL_X * GOAL_POOL_X / 256))
                    + (d * d / (GOAL_POOL_Z * GOAL_POOL_Z / 256));
                let lit = if e < 256 {
                    let f = (256 - e) * (256 - e) >> 8; // 0..256
                    let hue = if cz < 0 { blue } else { orange };
                    let add = |b: u8, h: u8| (b as i32 + h as i32 * f * GOAL_POOL_GAIN / (256 * 16)).min(255) as u8;
                    (add(base.0, hue.0), add(base.1, hue.1), add(base.2, hue.2))
                } else {
                    base
                };
                unsafe { FLOOR_LIGHT[stripe][gx][gz] = rgbc(lit) };
            }
        }
    }
    paint_lines();
}

// ---- pitch markings --------------------------------------------------------
// The lines are geometry, not texture. Painted into the pitch texture, chalk
// was mapped affinely across quads up to a whole 1,024-uu tile across, so a
// straight line bent where a quad's two triangles met and an arc wobbled from
// quad to quad; its edges were 16-uu texel steps, several pixels tall near
// the camera. A polygon's own edges stay straight under any projection, so
// each marking is a strip of flat quads lying on the pitch, drawn in the slot
// in front of it (see [`FLOOR_SLOT`]): exact edges and nothing to warp.

/// Chalk before the pitch light multiplies it, as the texture's chalk was.
const CHALK: Rgb = (205, 220, 210);
/// Half-widths: the straight lines and the centre circle's band.
const LINE_HALF_W: i32 = 40;
const CIRCLE_HALF_W: i32 = 30;
/// The halfway line stops this short of each side wall's foot.
const HALFWAY_INSET: i32 = 300;
const CIRCLE_R: i32 = 1122;
/// Each end, measured in from its goal line: a goal box, a bigger box, and
/// the arc on the bigger box's front, centred `ARC_CENTRE` in.
const GOAL_BOX_HALF_W: i32 = 1300;
const GOAL_BOX_DEPTH: i32 = 700;
const BIG_BOX_HALF_W: i32 = 2300;
const BIG_BOX_DEPTH: i32 = 1650;
const ARC_CENTRE: i32 = 1100;
const ARC_R: i32 = 860;
/// Straight strips are cut every 256 uu, the near pitch cells' size, so a
/// strip loses no more to the near plane than the grass under it does, and
/// no quad outgrows what the GPU will draw. Away from the camera the draw
/// steps over cuts the way the pitch's own bands coarsen.
const LINE_STEP: i32 = 256;
/// Curves are cut finely enough to read round: 226 uu a segment on the
/// centre circle.
const CIRCLE_SEGS: i32 = 32;
/// The most a colour channel may change along a marking quad and still be
/// drawn flat: under one 15-bit step, which is eight 8-bit ones.
const LINE_FLAT_SPREAD: i32 = 6;
const ARC_SEGS: i32 = 8;
/// Sections (cross-cuts, two points each) and strips, with room.
const MAX_LINE_SECTIONS: usize = 200;
const MAX_LINE_STRIPS: usize = 16;

/// The floor light grid is sampled at 256-uu steps, and [`chalk_at`] reads
/// it with shifts.
const _: () = assert!(
    sim::HALF_X * 2 / TILES_X / FLOOR_SPLIT_MAX == 256
        && sim::HALF_Z * 2 / TILES_Z / FLOOR_SPLIT_MAX == 256,
    "chalk_at assumes a 256-uu floor light grid"
);

#[derive(Copy, Clone)]
struct LineSection {
    a: (i16, i16),
    b: (i16, i16),
    /// The chalk's light at the cut's centre.
    c: Rgb,
}

#[derive(Copy, Clone)]
struct LineStrip {
    first: u8,
    last: u8,
    /// Curves never skip a section: a far circle cut to an eight-gon shows
    /// its corners.
    curved: bool,
    centre: (i32, i32),
    half: (i32, i32),
}

static mut LINE_SECTIONS: [LineSection; MAX_LINE_SECTIONS] = [LineSection {
    a: (0, 0),
    b: (0, 0),
    c: (0, 0, 0),
}; MAX_LINE_SECTIONS];
static mut LINE_STRIPS: [LineStrip; MAX_LINE_STRIPS] = [LineStrip {
    first: 0,
    last: 0,
    curved: false,
    centre: (0, 0),
    half: (0, 0),
}; MAX_LINE_STRIPS];
static mut LINE_STRIP_COUNT: usize = 0;

/// Lay out every marking's sections once at boot. Colours come later, from
/// [`paint_lines`], because the pitch light they follow changes with the
/// time of day and the teams' paints.
fn build_lines() {
    let mut sections = 0usize;
    let mut strips = 0usize;
    // One strip from a run of cross-cuts, each given as its two points.
    let mut strip = |cuts: &mut dyn Iterator<Item = ((i32, i32), (i32, i32))>, curved: bool| {
        let first = sections;
        let (mut lo, mut hi) = ((i32::MAX, i32::MAX), (i32::MIN, i32::MIN));
        for (a, b) in cuts {
            for p in [a, b] {
                lo = (lo.0.min(p.0), lo.1.min(p.1));
                hi = (hi.0.max(p.0), hi.1.max(p.1));
            }
            unsafe {
                LINE_SECTIONS[sections].a = (a.0 as i16, a.1 as i16);
                LINE_SECTIONS[sections].b = (b.0 as i16, b.1 as i16);
            }
            sections += 1;
        }
        unsafe {
            LINE_STRIPS[strips] = LineStrip {
                first: first as u8,
                last: (sections - 1) as u8,
                curved,
                centre: ((lo.0 + hi.0) / 2, (lo.1 + hi.1) / 2),
                half: ((hi.0 - lo.0) / 2, (hi.1 - lo.1) / 2),
            };
        }
        strips += 1;
    };
    // An axis-aligned straight line between two points on its centre line,
    // cut every LINE_STEP and at its far end.
    let straight = |from: (i32, i32), to: (i32, i32)| {
        let along_x = from.1 == to.1;
        let (s0, s1) = if along_x { (from.0, to.0) } else { (from.1, to.1) };
        let dir = if s1 >= s0 { 1 } else { -1 };
        let len = (s1 - s0).abs();
        let cuts = (len + LINE_STEP - 1) / LINE_STEP;
        (0..=cuts).map(move |k| {
            let t = s0 + dir * (k * LINE_STEP).min(len);
            if along_x {
                ((t, from.1 - LINE_HALF_W), (t, from.1 + LINE_HALF_W))
            } else {
                ((from.0 - LINE_HALF_W, t), (from.0 + LINE_HALF_W, t))
            }
        })
    };
    // Cuts across a ring band from angle `a0` over `segs` steps of `da`
    // (Q12 of a turn), about (cx, cz), with +Z of the angle along `dz`.
    let ring = |cx: i32, cz: i32, dz: i32, r: i32, w: i32, a0: i32, da: i32, segs: i32| {
        (0..=segs).map(move |k| {
            let t = (a0 + da * k) as u16;
            let (sn, cs) = (sin_q12(t), cos_q12(t) * dz);
            let at = |rr: i32| (cx + (rr * sn >> 12), cz + (rr * cs >> 12));
            (at(r - w), at(r + w))
        })
    };

    let x_end = sim::HALF_X - RAMP_R - HALFWAY_INSET;
    strip(&mut straight((-x_end, 0), (x_end, 0)), false);
    strip(
        &mut ring(0, 0, 1, CIRCLE_R, CIRCLE_HALF_W, 0, 4096 / CIRCLE_SEGS, CIRCLE_SEGS),
        true,
    );
    // The D's centre line meets the big box's front line's centre line where
    // cos(a) = (BIG_BOX_DEPTH - ARC_CENTRE) / ARC_R; its ends overlap into
    // that line, which hides the join.
    let mut arc_half = 0;
    while cos_q12(arc_half as u16) * ARC_R > (BIG_BOX_DEPTH - ARC_CENTRE) * 4096 {
        arc_half += 1;
    }
    for end in [-1, 1] {
        // `end` is the goal's side; `d` runs in from its goal line.
        let z = |d: i32| end * (sim::HALF_Z - d);
        // The side lines start where the flat pitch does, an end-wall ramp
        // radius in.
        for (half_w, depth) in [(GOAL_BOX_HALF_W, GOAL_BOX_DEPTH), (BIG_BOX_HALF_W, BIG_BOX_DEPTH)] {
            for side in [-half_w, half_w] {
                strip(&mut straight((side, z(sim::END_RAMP_R)), (side, z(depth + LINE_HALF_W))), false);
            }
            let reach = half_w + LINE_HALF_W;
            strip(&mut straight((-reach, z(depth)), (reach, z(depth))), false);
        }
        strip(
            &mut ring(
                0,
                z(ARC_CENTRE),
                -end,
                ARC_R,
                LINE_HALF_W,
                -arc_half,
                2 * arc_half / ARC_SEGS,
                ARC_SEGS,
            ),
            true,
        );
    }
    unsafe { LINE_STRIP_COUNT = strips };
}

/// Chalk under the pitch light at a point: both mown stripes' light,
/// averaged (chalk is not mown), bilinear off the 256-uu grid.
fn chalk_at(x: i32, z: i32) -> Rgb {
    let fx = (x + sim::HALF_X).clamp(0, 2 * sim::HALF_X - 1);
    let fz = (z + sim::HALF_Z).clamp(0, 2 * sim::HALF_Z - 1);
    let (gx, tx) = ((fx >> 8) as usize, fx & 255);
    let (gz, tz) = ((fz >> 8) as usize, fz & 255);
    let mut sum = [0i32; 3];
    for stripe in 0..2 {
        for (dx, wx) in [(0, 256 - tx), (1, tx)] {
            for (dz, wz) in [(0, 256 - tz), (1, tz)] {
                let c = rgb_of(unsafe { FLOOR_LIGHT[stripe][gx + dx][gz + dz] });
                let w = wx * wz;
                sum[0] += c.0 as i32 * w;
                sum[1] += c.1 as i32 * w;
                sum[2] += c.2 as i32 * w;
            }
        }
    }
    // Weights total 2 * 256 * 256.
    tinted(
        CHALK,
        ((sum[0] >> 17) as u8, (sum[1] >> 17) as u8, (sum[2] >> 17) as u8),
    )
}

/// Light every marking's sections from the current pitch light. Run after
/// anything rewrites [`FLOOR_LIGHT`].
fn paint_lines() {
    let n = unsafe { LINE_STRIPS[LINE_STRIP_COUNT.max(1) - 1].last as usize + 1 };
    for i in 0..n.min(MAX_LINE_SECTIONS) {
        unsafe {
            let s = &mut LINE_SECTIONS[i];
            s.c = chalk_at(
                (s.a.0 as i32 + s.b.0 as i32) / 2,
                (s.a.1 as i32 + s.b.1 as i32) / 2,
            );
        }
    }
}

/// The goal pools: half-extents across the mouth and out into the pitch, and
/// strength in sixteenths of the team hue (whose brightest channel is 128).
const GOAL_POOL_X: i32 = 1700;
const GOAL_POOL_Z: i32 = 1300;
const GOAL_POOL_GAIN: i32 = 18;
/// The lit goal box, as sixteenths of the team's signal colour: brightest at
/// the back wall's floor, falling toward the roof and the mouth, so the box
/// reads as lit from inside rather than painted.
const GOAL_BACK_LO: i32 = 13;
const GOAL_BACK_HI: i32 = 7;
const GOAL_SIDE: i32 = 8;
const GOAL_FLOOR: i32 = 10;
/// The white-hot core the posts and bar take on, and how much of it.
const GOAL_FRAME_HOT: Rgb = (255, 248, 232);
const GOAL_FRAME_MIX: i32 = 7;
/// Halo sizes: how far past the post and bar the glow reaches, and the
/// goal-line strip's half-depth.
const GOAL_POST_HALO: i32 = 190;
const GOAL_BAR_HALO: i32 = 170;
const GOAL_LINE_HALO: i32 = 110;

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
        || texture.clut_entries() != 16 * 8
        || texture.clut_bytes().len() != 16 * 8 * 2
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
    for (row, clut) in [
        TEX_CLUT,
        GRASS_CLUT,
        COVER_CLUT,
        MARKED_CLUT,
        BALL_CLUT,
        SPENT_CLUT,
        GLOW_CLUT,
        RING_CLUT,
    ]
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

/// Packet sets: the frame being built and the frame in flight, for one view
/// (sets 0 and 1) and, in split screen, for the second view (2 and 3).
const SET_COUNT: usize = 4;
static mut OT_SETS: [OrderingTable<OT_DEPTH>; SET_COUNT] =
    [const { OrderingTable::new() }; SET_COUNT];
/// EXPERIMENT: which packet set the frame being built uses; the other may be in flight.
static mut SET: usize = 0;
static mut PENDING: bool = false;
/// Split screen's tables from the last frame wait to be drawn: both views'
/// sets `SET` and `SET + 2` (see [`render_split`]).
static mut SPLIT_PENDING: bool = false;
/// The vblank the last split frame flips on (see [`note_split_flip`]).
static mut SPLIT_FLIP_VBLANK: u32 = 0;

/// Note when a split frame flips: call last thing before every flip (the HUD
/// overlay's end); it does nothing unless the frame was a split one. The
/// flip lands on the vblank after this returns. See [`hold_split_cadence`].
pub fn note_split_flip() {
    unsafe {
        if SPLIT_PENDING {
            SPLIT_FLIP_VBLANK = psx_rt::interrupts::vblank_count().wrapping_add(1);
        }
    }
}

/// Hold split screen at a steady 30, last thing in [`render_split`].
///
/// Pipelined, a quiet split frame fits in one vblank, and a picture that
/// alternates between one and two vblanks judders. A frame built within the
/// vblank of the last flip waits here for the next one, so it flips two
/// vblanks after the last, as it always did when every split frame ran long.
/// Waiting here rather than at the flip lets the runner spend the rest of
/// that second vblank on the fixed update it falls due for, instead of
/// owing it after the flip. A frame that is late anyway does not wait.
fn hold_split_cadence() {
    use psx_rt::interrupts::{vblank_count, wait_vblank};
    unsafe {
        while (vblank_count().wrapping_sub(SPLIT_FLIP_VBLANK) as i32) < 1 {
            wait_vblank();
        }
    }
}

/// GP0(E3h) and GP0(E4h) as an ordering-table packet: a view's scissor set
/// from inside its own table, so the GPU switches between the two halves
/// in command order while the CPU builds the next view. Filled in when the
/// table is kicked, because the back buffer it lands in is only known then.
#[repr(C, align(4))]
struct AreaPacket {
    tag: u32,
    top_left: u32,
    bottom_right: u32,
}
const AREA_WORDS: u8 = 2;
impl AreaPacket {
    const EMPTY: Self = Self {
        tag: 0,
        top_left: 0xE300_0000,
        bottom_right: 0xE400_0000,
    };
    fn set(&mut self, vp: Viewport, buffer_y: u16) {
        let (x0, y0) = (vp.x as u32, buffer_y as u32 + vp.y as u32);
        let (x1, y1) = ((vp.x + vp.w) as u32 - 1, buffer_y as u32 + (vp.y + vp.h) as u32 - 1);
        self.top_left = 0xE300_0000 | (x0 & 0x3FF) | ((y0 & 0x1FF) << 10);
        self.bottom_right = 0xE400_0000 | (x1 & 0x3FF) | ((y1 & 0x1FF) << 10);
    }
}
/// Per set: the scissor its view is drawn with, first in its table, and for
/// the second split view the whole screen again, last in its table, so the
/// HUD overlay after it is not clipped to the bottom half.
static mut AREA_HEAD: [AreaPacket; SET_COUNT] = [const { AreaPacket::EMPTY }; SET_COUNT];
static mut AREA_TAIL: [AreaPacket; SET_COUNT] = [const { AreaPacket::EMPTY }; SET_COUNT];
/// `build_view` links `AREA_TAIL[SET]` as the last packet of the table.
static mut VIEW_TAIL: bool = false;
static mut QUADS_SETS: [[QuadGouraud; MAX_QUADS]; SET_COUNT] = [QUADS_INIT; SET_COUNT];
/// Single-colour quads: the pitch markings (one per pair of cuts at worst)
/// and, per pad, the two plates and the far orb's one diamond. A flat quad
/// costs the GPU a quarter of a Gouraud one's setup and half its fill, so
/// nothing that is one colour anyway pays for shading.
/// The tyre tracks add one flat quad per joined pair of points, at most.
const MAX_FLAT_QUADS: usize = MAX_LINE_SECTIONS + 3 * sim::PADS.len() + TRACK_WHEELS * TRACK_LEN;
static mut FLAT_QUADS_SETS: [[QuadFlat; MAX_FLAT_QUADS]; SET_COUNT] = [FLAT_QUADS_INIT; SET_COUNT];
const FLAT_QUADS_INIT: [QuadFlat; MAX_FLAT_QUADS] =
    [const { QuadFlat::new([(0, 0); 4], 0, 0, 0) }; MAX_FLAT_QUADS];
const QUADS_INIT: [QuadGouraud; MAX_QUADS] =
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
static mut TEX_QUADS_SETS: [[QuadTexturedGouraud; MAX_TEX_QUADS]; SET_COUNT] = [TEX_INIT; SET_COUNT];
const TEX_INIT: [QuadTexturedGouraud; MAX_TEX_QUADS] =
    [const { QuadTexturedGouraud::EMPTY }; MAX_TEX_QUADS];

/// An additive light packet: the textured quad, then a GP0(E1) that puts the
/// draw mode back to the arena's average blend.
///
/// The restore is not optional. A textured polygon's tpage word also becomes
/// the GPU's current draw mode, blend bits included, and the untextured
/// semi-transparent pieces (shadows, flame, smoke) blend with whatever mode is
/// current. Without the trailing word, a shadow the ordering table happened to
/// put after a pad would be added to the pitch instead of averaged into it,
/// and turn into a bright hole.
#[repr(C, align(4))]
struct GlowQuad {
    quad: QuadTexturedGouraud,
    restore: u32,
}
/// Data words after the tag: the quad's thirteen and the restore.
const GLOW_WORDS: u8 = 14;
// SAFETY: `#[repr(C, align(4))]` with the quad first, so the quad's tag is the
// packet's tag, followed by its thirteen payload words and the restore word:
// fourteen plain words after the tag, one DMA node.
unsafe impl GpuPacket for GlowQuad {
    const WORDS: u8 = GLOW_WORDS;
}
const GLOW_RESTORE: u32 = ARENA_MATERIAL.draw_mode_word();
/// Goal halos (four a goal, eight with both goals in view), the ball's disc,
/// and its hoop at up to sixteen segments of two quads: 41 at most. An
/// explosion is up to 34 more, and a goal and a demolition can overlap.
const MAX_GLOWS: usize = 112;
static mut GLOW_SETS: [[GlowQuad; MAX_GLOWS]; SET_COUNT] = [GLOW_INIT; SET_COUNT];
const GLOW_INIT: [GlowQuad; MAX_GLOWS] = [const {
    GlowQuad {
        quad: QuadTexturedGouraud::EMPTY,
        restore: 0,
    }
}; MAX_GLOWS];

/// The goal banner, as the packets `draw_text_scaled_q8` would have written to
/// the GPU one word at a time after the scene: every glyph of every outline
/// pass, in the same order. Written by hand over the ports, a banner of fifty
/// glyph quads waited on the GPU between the end of the scene and the flip and
/// took a sixth of a frame; as packets at the back of the ordering table the
/// GPU draws them during the next build instead.
///
/// A glyph is one quad of eleven words (its texture window travels with it).
/// Ten glyphs, five outline passes and the ink: sixty.
const BANNER_MAX: usize = 64;
static mut BANNER_QUADS: [[QuadTexturedMaterial; BANNER_MAX]; SET_COUNT] = [BANNER_INIT; SET_COUNT];
const BANNER_INIT: [QuadTexturedMaterial; BANNER_MAX] = [const {
    QuadTexturedMaterial::with_material(
        [(0, 0); 4],
        [(0, 0); 4],
        TextureMaterial::opaque(0, 0, (0, 0, 0)),
    )
}; BANNER_MAX];

/// What the goal banner says and where, for the next table built. `parts` are
/// drawn in order, each with its black outline: x, text, ink.
#[derive(Copy, Clone)]
pub struct Banner {
    pub font: FontAtlas,
    pub y: i16,
    pub q8: u16,
    pub parts: [(i16, &'static str, (u8, u8, u8)); 2],
}
static mut BANNER: Option<Banner> = None;
/// The draw mode the banner's glyphs are drawn in: the font's own, which has
/// no dither. Written to the GPU ahead of them so the grain the arena's
/// textured packets draw with does not land on the lettering.
static mut BANNER_MODE: [ModePacket; SET_COUNT] = [const { ModePacket { tag: 0, word: 0 } }; SET_COUNT];
/// What each set's glyph packets were last built from, and how many there
/// are: a banner that has finished growing is the same sixty quads every frame.
type BannerKey = (i16, u16, [(i16, &'static str, (u8, u8, u8)); 2]);
static mut BANNER_BUILT: [Option<(BannerKey, usize)>; SET_COUNT] = [None; SET_COUNT];

/// Ask for a goal banner in the next single-view table, or for none.
pub fn set_banner(banner: Option<Banner>) {
    unsafe { BANNER = banner };
}

/// The black offsets the banner's outline is drawn at before its ink.
const BANNER_OUTLINE: [(i16, i16); 5] = [(-1, 0), (1, 0), (0, -1), (0, 1), (1, 1)];

/// One sixteen-entry CLUT row uploaded from inside the ordering table:
/// GP0(A0) with its data inline, then GP0(01) so no cached copy of the old
/// row outlives it. Twelve data words, inside the silicon's sixteen.
#[repr(C, align(4))]
struct ClutLoad {
    tag: u32,
    cmd: u32,
    xy: u32,
    wh: u32,
    data: [u32; 8],
    flush: u32,
}
const CLUT_LOAD_WORDS: u8 = 12;
impl ClutLoad {
    const EMPTY: Self = Self {
        tag: 0,
        cmd: 0xA000_0000,
        xy: 0,
        wh: (1 << 16) | 16,
        data: [0; 8],
        flush: 0x0100_0000,
    };
}
/// The crowd's two palettes, one per end, loaded every frame from inside the
/// table (one per packet set: the other set's table may still be in flight):
/// they carry the teams' colours and a shimmer through the fans.
static mut CROWD_CLUT_LOAD: [[ClutLoad; 2]; SET_COUNT] =
    [const { [ClutLoad::EMPTY, ClutLoad::EMPTY] }; SET_COUNT];
/// What each of those packets' palettes was last worked out for: the team
/// colour and the shimmer's phase, which move rarely, so the sixteen entries
/// are not rebuilt every frame.
static mut CROWD_CLUT_KEY: [[Option<(Rgb, u8)>; 2]; SET_COUNT] = [[None; 2]; SET_COUNT];

/// Crowd palette entries that are not team colour (tools/cook-arena's crowd
/// tile indexes them): seat shadow, tier step, clothes and faces.
const CROWD_BASE: [Rgb; 12] = [
    (18, 18, 26),
    (30, 30, 40),
    (52, 46, 44),
    (90, 70, 60),
    (150, 120, 100),
    (200, 170, 140),
    (60, 60, 80),
    (110, 110, 130),
    (180, 180, 190),
    (140, 40, 40),
    (40, 90, 60),
    (210, 200, 80),
];

/// One end's crowd palette this frame: entries 12..14 are fans in the team's
/// colour at three strengths, rotated every few frames so the stand
/// shimmers, and 15 is the lit fascia along the front tier.
fn crowd_clut(seat: usize, tick: u32) -> [u32; 8] {
    let c = seat_signal(seat);
    let half = |c: Rgb| -> u16 {
        let (r, g, b) = ((c.0 as u16 >> 3).max(1), c.1 as u16 >> 3, c.2 as u16 >> 3);
        r | (g << 5) | (b << 10)
    };
    let scale = |k: i32| {
        (
            (c.0 as i32 * k >> 4) as u8,
            (c.1 as i32 * k >> 4) as u8,
            (c.2 as i32 * k >> 4) as u8,
        )
    };
    let phase = (tick / 6 % 3) as usize;
    const SHADES: [i32; 3] = [16, 11, 7];
    let mut words = [0u32; 8];
    for (k, w) in words.iter_mut().enumerate() {
        let entry = |i: usize| -> u16 {
            match i {
                0..=11 => half(CROWD_BASE[i]),
                12..=14 => half(scale(SHADES[(i - 12 + phase) % 3])),
                _ => half(mix(c, (255, 255, 255), 5)),
            }
        };
        *w = entry(2 * k) as u32 | ((entry(2 * k + 1) as u32) << 16);
    }
    words
}

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

use psx_math::color::scale_rgb as shade;

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
/// interpolates to 4096 steps; the shared Q12 constructors preserve that
/// precision before composition with `mul`.
fn rot_y_q12(a: u16) -> Mat3I16 {
    Mat3I16::rotate_y_q12(a)
}

/// Roll about X, which for a rolling ball is the axis it turns on.
fn rot_x_q12(a: u16) -> Mat3I16 {
    Mat3I16::rotate_x_q12(a)
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
    let [x, y, z] = m.transform_i32([v.0, v.1, v.2]);
    (x, y, z)
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

/// The `diag-keys` sky, which `tools/frame-check` finds as a hole in the pitch.
const DIAG_SKY_KEY: Rgb = (255, 0, 255);

/// Backdrop colours for this camera. Sunset keeps a cool zenith and warms the
/// horizon most strongly toward a fixed +Z sun, mirroring VoXide's directional
/// Minecraft sunset rather than washing all four corners orange.
fn arena_sky(view: &View) -> [Rgb; 4] {
    // Keyed only while the camera is low, level (vertical axis within about
    // 30 degrees of world up) and inside the pitch's length: then the bottom
    // of a view only ever shows the pitch or the foot of a wall. A camera
    // tipped up at a car on a wall, high on a wall or inside a goal sees the
    // sky through the net there, legitimately.
    if cfg!(feature = "diag-keys")
        && (view.v.m[1][1] as i32).abs() > 3550
        && -view.pos.1 < 600
        && view.pos.2.abs() < sim::HALF_Z
    {
        return [DIAG_SKY_KEY; 4];
    }
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

    /// How far a box with half-extents `h` reaches along the forward, right
    /// and vertical view axes: [`Self::extent`] on each, for a box size that
    /// is tested many times a frame.
    fn extents(&self, h: (i32, i32, i32)) -> [i32; 3] {
        [
            Self::extent(self.fwd, h),
            Self::extent(self.right, h),
            Self::extent(self.vertical, h),
        ]
    }

    /// `visible(c, h) && visible_vertically(c, h)`, given `extents(h)`: the
    /// same tests on the same integers, with the forward distance and the
    /// extents computed once rather than once per test.
    fn visible_box(&self, c: (i32, i32, i32), e: [i32; 3]) -> bool {
        let d = (c.0 - self.pos.0, c.1 - self.pos.1, c.2 - self.pos.2);
        let z = Self::dot(self.fwd, d) + e[0];
        if z <= 0 {
            return false;
        }
        if Self::dot(self.right, d).abs() - e[1] > z * cull_half_w() / PROJ_H as i32 {
            return false;
        }
        let half_h = unsafe { VIEW_HALF_H };
        Self::dot(self.vertical, d).abs() - e[2] <= z * half_h / PROJ_H as i32
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
    build_stands();
    build_lines();
    build_lighting();
    build_car_materials();
    build_burst();
}

/// Margin the camera keeps from the walls, so it never clips through one.
// At the camera's ordinary 330-uu height the quarter pipe is already almost
// vertical, so this clears it without crushing the follow distance as badly
// as a full ramp-radius inset would.
const CAM_WALL_MARGIN: i32 = 180;
/// Height the camera keeps above the pitch and below the roof. Climbing a
/// wall, the boom trails the car downward, and from the foot of the ramp
/// that put the eye under the pitch, looking up through it at the arena
/// (Manny's 2026-09-24 tape, polls 3290..3328). Within `CAM_WALL_MARGIN` of a
/// wall the ramp is at most about 13 uu high, so this clears it too.
const CAM_SURFACE_CLEAR: i32 = 100;

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
    // The rounded corner joints cut further in than the planes do.
    for &(nx, nz, off) in &sim::CORNER_JOINT_PLANES {
        let over = ((x.abs() * nx + z.abs() * nz) >> 12) - ((off >> 2) - CAM_WALL_MARGIN);
        if over > 0 {
            x -= x.signum() * (nx * over >> 12);
            z -= z.signum() * (nz * over >> 12);
        }
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
    split: bool,
    camera_slot: usize,
) -> View {
    // Headings are rebuilt from vectors here, and the SDK's octant-linear
    // `atan2_q12` is off by up to four degrees mid-octant: through a held turn
    // that error rises and falls with the car's yaw and swings the view about
    // the car. The pitch angles below keep the SDK's, which the pitch limits
    // were tuned against and which depends on a slope that does not sweep.
    use sim::angle::atan2_q12_fine as atan2_fine;
    let camera_slot = camera_slot.min(1);
    let previous = unsafe { CHASE_CAMERAS[camera_slot] };
    // A kickoff is a cut: the car is somewhere new, and easing the camera in
    // from where the last match, or the last goal, left it is a flight across
    // the arena. Frame the car from scratch, which is the pose the kickoff
    // camera settles into.
    let previous = if previous.kickoff > s.kickoff_ticks() {
        CameraState::EMPTY
    } else {
        previous
    };
    let now = unsafe { CAMERA_TICK };
    let ticks = (now.wrapping_sub(previous.tick).clamp(1, 8)) as i32;
    let offset_step = CAM_OFFSET_STEP * ticks / 2;
    let yaw_step = CAM_YAW_STEP * ticks / 2;
    let pitch_step = CAM_PITCH_STEP * ticks / 2;
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
        atan2_fine(car_fwd.x, car_fwd.z)
    } else {
        atan2_fine(dx, dz)
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
    if current_flat < CAM_MIN_FLAT_DIST && s.kickoff_ticks() >= CAM_KICKOFF_BEHIND_TICKS {
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
    let height = if ball_cam && hold_car && split {
        CAM_SPLIT_BALL_HEIGHT
    } else {
        CAM_HEIGHT
    };
    let desired_cyy = car_y + vertical_trail - ((car_up.y * height) >> 12);
    let desired_offset = (cx - car_x, desired_cyy - car_y, cz - car_z);
    let offset = if !previous.valid {
        desired_offset
    } else {
        (
            previous.offset.0
                + (desired_offset.0 - previous.offset.0).clamp(-offset_step, offset_step),
            previous.offset.1
                + (desired_offset.1 - previous.offset.1).clamp(-offset_step, offset_step),
            previous.offset.2
                + (desired_offset.2 - previous.offset.2).clamp(-offset_step, offset_step),
        )
    };
    (cx, cz) = keep_inside(car_x + offset.0, car_z + offset.2);
    // Render Y is down: the pitch is at 0 and the roof at -CEIL.
    let cyy = (car_y + offset.1).clamp(-(sim::CEIL - CAM_SURFACE_CLEAR), -CAM_SURFACE_CLEAR);
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
        let limit = if split {
            CAM_BALL_CAR_PITCH_SPLIT
        } else {
            CAM_BALL_CAR_PITCH
        };
        let shift = (delta.abs() - limit).max(0);
        signed += delta.signum() * shift;
    }
    let pitch_min = CAM_PITCH_MIN + (((CAM_WALL_PITCH_MIN - CAM_PITCH_MIN) * wall_amount) >> 12);
    let desired_pitch = signed.clamp(pitch_min, CAM_PITCH_MAX);
    let mut view_yaw = atan2_fine(aim_x - cx, aim_z - cz);
    if ball_cam && hold_car {
        // The end-wall clamp slides the camera sideways at kickoff to retain a
        // useful boom length. On a 63-degree FOV, the resulting car-to-ball
        // subject angle is wider than either subject's safe screen margin.
        // Bias the view away from the ball only as much as needed to retain
        // the car; once the boom is no longer wall-limited this becomes zero.
        let car_yaw = atan2_fine(car_x - cx, car_z - cz);
        let delta = ((car_yaw as i32 - view_yaw as i32 + 2048).rem_euclid(4096)) - 2048;
        let shift = (delta.abs() - CAM_BALL_CAR_YAW).max(0);
        view_yaw = (view_yaw as i32 + delta.signum() * shift).rem_euclid(4096) as u16;
    }
    #[cfg(feature = "diag-log")]
    let desired_yaw = view_yaw;
    let (view_yaw, pitch) = if previous.valid {
        let yaw_delta = ((view_yaw as i32 - previous.yaw as i32 + 2048).rem_euclid(4096)) - 2048;
        (
            (previous.yaw as i32 + yaw_delta.clamp(-yaw_step, yaw_step)).rem_euclid(4096)
                as u16,
            previous.pitch
                + (desired_pitch - previous.pitch).clamp(-pitch_step, pitch_step),
        )
    } else {
        (view_yaw, desired_pitch)
    };
    #[cfg(feature = "diag-log")]
    crate::diaglog::fill!(cam;
        now as i32,
        camera_slot as i32,
        ball_cam as i32
            | (hold_car as i32) << 1
            | (split as i32) << 2
            | (previous.valid as i32) << 3,
        cx,
        cyy,
        cz,
        view_yaw as i32,
        pitch,
        desired_yaw as i32,
        desired_pitch,
        subject.yaw as i32,
        subject.p.x,
        subject.p.z,
        subject.v.x,
        subject.v.z,
        subject.steer,
        subject.slide,
        subject.grounded as i32 | (subject.up.y << 1),
        s.kickoff_ticks() as i32,
        follow_yaw as i32,
        ticks,
        s.ball.p.x,
        s.ball.p.z,
        offset.0,
    );
    unsafe {
        CHASE_CAMERAS[camera_slot] = CameraState {
            valid: true,
            offset: (cx - car_x, cyy - car_y, cz - car_z),
            yaw: view_yaw,
            pitch,
            tick: now,
            kickoff: s.kickoff_ticks(),
        };
    }
    look_from((cx, cyy, cz), view_yaw, pitch.rem_euclid(4096) as u16)
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
static CAR_BLOBS: [&[u8]; CAR_SLOTS] = [
    include_bytes!("../assets/sedan.psxm"),
    include_bytes!("../assets/hatchback.psxm"),
    include_bytes!("../assets/hatchback2.psxm"),
    // Distance LOD, cooked from the same sources at 60 faces
    // (`make assets CAR_FACE_TARGET=60`, see the Makefile), in the same
    // order so slot `which + CAR_COUNT` is the far copy of `which`.
    include_bytes!("../assets/sedan_lod.psxm"),
    include_bytes!("../assets/hatchback_lod.psxm"),
    include_bytes!("../assets/hatchback2_lod.psxm"),
];

/// Every cooked car mesh: the three select-screen cars, then their LODs.
const CAR_SLOTS: usize = CAR_COUNT * 2;

/// A full-screen view swaps a car to its 60-face LOD once it is this many
/// pixels long or less on screen, and back to the full mesh above
/// [`CAR_LOD_EXIT_PX`]. Screen size rather than distance, because size is what
/// decides whether the extra faces can be seen; the gap between the two is
/// hysteresis, so a car hovering at the threshold does not flicker between
/// meshes every frame.
///
/// Per seat. A full-screen view is always seat 0's, so seat 0 is the car the
/// chase camera trails, which the player studies and which keeps the full
/// mesh down to 24 pixels. Seat 1 is the opponent: 60 faces hold up to about
/// 56 pixels, and the heavy frames are the ones with both cars close, where
/// the opponent's full mesh was most of what tipped them past a vblank.
const CAR_LOD_ENTER_PX: [i32; SEATS] = [24, 56];
const CAR_LOD_EXIT_PX: [i32; SEATS] = [28, 64];
/// Camera-space depths those sizes fall at: car length on screen is
/// `2 * CAR_HALF_L * PROJ_H / depth`.
const CAR_LOD_ENTER_DEPTH: [i32; SEATS] = [
    2 * sim::CAR_HALF_L * PROJ_H as i32 / CAR_LOD_ENTER_PX[0],
    2 * sim::CAR_HALF_L * PROJ_H as i32 / CAR_LOD_ENTER_PX[1],
];
const CAR_LOD_EXIT_DEPTH: [i32; SEATS] = [
    2 * sim::CAR_HALF_L * PROJ_H as i32 / CAR_LOD_EXIT_PX[0],
    2 * sim::CAR_HALF_L * PROJ_H as i32 / CAR_LOD_EXIT_PX[1],
];
/// Which seats a full-screen view is currently drawing from the LOD.
static mut CAR_FAR_LOD: [bool; SEATS] = [false; SEATS];
/// The same per split-screen half (top, bottom), each with its own camera.
/// Both cars in a split match are a player's car, studied from a chase
/// camera, so both take seat 0's sizes: the 60-face LOD is garbled at the
/// sixty pixels the chase camera shows a car at (thin wheel slabs under a
/// wedge of body), which is what drawing every split car past 500 uu from
/// the LOD looked like (Manny's attract demo, 2026-10-03).
static mut SPLIT_CAR_FAR_LOD: [[bool; SEATS]; 2] = [[false; SEATS]; 2];
/// Which split half `build_view` is drawing: the index into
/// [`SPLIT_CAR_FAR_LOD`]. Set by [`render_split`].
static mut SPLIT_HALF: usize = 0;

/// The same idea for the ball: a full-screen view drops to eight columns (the
/// split view's mesh) once the ball is this many pixels across or less, and
/// goes back to sixteen above [`BALL_LOD_EXIT_PX`].
const BALL_LOD_ENTER_PX: i32 = 20;
const BALL_LOD_EXIT_PX: i32 = 24;
const BALL_LOD_ENTER_DEPTH: i32 = 2 * sim::BALL_R * PROJ_H as i32 / BALL_LOD_ENTER_PX;
const BALL_LOD_EXIT_DEPTH: i32 = 2 * sim::BALL_R * PROJ_H as i32 / BALL_LOD_EXIT_PX;
static mut BALL_FAR_LOD: bool = false;

/// Hysteresis between two depths: `far` turns on past `enter` and off
/// nearer than `exit`.
fn lod_far(far: &mut bool, depth: i32, enter: i32, exit: i32) -> bool {
    if depth > enter {
        *far = true;
    } else if depth < exit {
        *far = false;
    }
    *far
}

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
/// The seat's signal hue with its brightest channel at 128, for the
/// enclosure and the roof (see [`glow`]).
static mut SEAT_GLOW: [Rgb; SEATS] = [(128, 128, 128); SEATS];
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
        let top = (c.0.max(c.1).max(c.2) as i32).max(1);
        let glow = (
            (c.0 as i32 * 128 / top) as u8,
            (c.1 as i32 * 128 / top) as u8,
            (c.2 as i32 * 128 / top) as u8,
        );
        unsafe {
            SEAT_HUE[seat] = mix((128, 128, 128), full, 10);
            SEAT_GLOW[seat] = glow;
        }
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
/// Held as the GTE's RGBC word (`0x00BBGGRR`), which is what the projection
/// loads: a colour is one aligned read there instead of three byte loads.
static mut PAINTED_GAME: [[u32; CAR_MAX_VERTS]; SEATS] = [[RGBC_GREY; CAR_MAX_VERTS]; SEATS];
/// The same seat paints applied to the LOD copy of each seat's car.
static mut PAINTED_LOD: [[u32; CAR_MAX_VERTS]; SEATS] = [[RGBC_GREY; CAR_MAX_VERTS]; SEATS];
const RGBC_GREY: u32 = rgbc((128, 128, 128));
/// Every vertex of a `diag-keys` LOD car: pure green, which lighting only
/// darkens, so `tools/frame-check` can measure how big the LOD is drawn.
static DIAG_LOD_KEY: [u32; CAR_MAX_VERTS] = [rgbc((0, 255, 0)); CAR_MAX_VERTS];

/// A colour as the GTE's RGBC data register takes it, code byte zero.
const fn rgbc(c: Rgb) -> u32 {
    (c.0 as u32) | ((c.1 as u32) << 8) | ((c.2 as u32) << 16)
}
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
    let repaint = |src: &[Rgb; CAR_MAX_VERTS], dst: &mut [u32; CAR_MAX_VERTS]| {
        for (out, &base) in dst.iter_mut().zip(src.iter()) {
            *out = rgbc(if base == BODY_KEY {
                body
            } else if base == BODY_DARK_KEY {
                dark
            } else {
                base
            });
        }
    };
    unsafe {
        repaint(&CAR_MATERIALS[which], &mut PAINTED_GAME[seat]);
        repaint(&CAR_MATERIALS[which + CAR_COUNT], &mut PAINTED_LOD[seat]);
        PAINTED_FOR[seat] = Some((car, paint));
    }
}

/// Per-vertex wheel-corner assignments for the gameplay LODs.
///
/// Slots are rear-left, rear-right, front-left, front-right; `255` is rigid
/// bodywork. Geometry is identical between team variants, so the blue maps
/// serve both cars.
static CAR_WHEELS: [&[u8]; CAR_SLOTS] = [
    include_bytes!("../assets/sedan.psxw"),
    include_bytes!("../assets/hatchback.psxw"),
    include_bytes!("../assets/hatchback2.psxw"),
    include_bytes!("../assets/sedan_lod.psxw"),
    include_bytes!("../assets/hatchback_lod.psxw"),
    include_bytes!("../assets/hatchback2_lod.psxw"),
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
/// The ball's vertex tint where the light is full on it: 1.56 times the
/// texture's own colour (128 is 1.0), so the mid-grey plates come up bright
/// on the lit side.
const BALL_TINT_LIT: i32 = 200;

/// Triangle budget for both cars together. The garage always pairs the chosen
/// model with the one two slots ahead; the heaviest prepared pair is 1189.
const CAR_TRI_CAP: usize = 1248;
/// Faces held per decoded gameplay model. Matches the triangle arena, which is
/// already sized for the worst car in the library.
const CAR_FACE_CAP: usize = CAR_TRI_CAP;
/// Projected-vertex scratch. Undersize this and `project_car` quietly truncates
/// the car, so the cook tests check every committed blob against it.
const CAR_VERT_CAP: usize = 1344;

/// One projected, lit car vertex, kept in the words the GTE hands back and
/// the GPU packet takes: `SXY` (x low, y high), the lit `RGB` (code byte
/// zero) and `SZ`. The SDK's `ProjectedLit` spreads the same values over ten
/// bytes, so every face re-assembled them with unaligned and byte loads, each
/// paying a main-RAM stall; here a packet corner is two aligned reads.
#[derive(Copy, Clone)]
#[repr(C)]
struct CarLit {
    xy: u32,
    rgb: u32,
    sz: u32,
}

const EMPTY_LIT: CarLit = CarLit {
    xy: 0,
    rgb: 0,
    sz: 0,
};

impl CarLit {
    #[inline(always)]
    fn sx(&self) -> i32 {
        self.xy as i16 as i32
    }
    #[inline(always)]
    fn sy(&self) -> i32 {
        (self.xy >> 16) as i16 as i32
    }
}

/// `TriGouraud`'s command byte, taken from the SDK constructor itself.
const TRI_GOURAUD_CMD: u32 = TriGouraud::new([(0, 0); 3], [(0, 0, 0); 3]).color0_cmd;

/// Bounding half-extent for the whole-car frustum test, on every axis.
///
/// Isotropic because the car rolls: the bound has to hold at any orientation,
/// so it is the hitbox's half-diagonal, `sqrt(82^2 + 58^2 + 26^2)` = 104,
/// rounded up for the bodywork that overhangs the collision box.
const CAR_BOUND_R: i32 = 128;

static mut CAR_TRIS_SETS: [[TriGouraud; CAR_TRI_CAP]; SET_COUNT] = [TRIS_INIT; SET_COUNT];
const TRIS_INIT: [TriGouraud; CAR_TRI_CAP] =
    [const { TriGouraud::new([(0, 0); 3], [(0, 0, 0); 3]) }; CAR_TRI_CAP];
static mut CAR_PROJ: [CarLit; CAR_VERT_CAP] = [EMPTY_LIT; CAR_VERT_CAP];
/// Four authored wheel pivots per selectable gameplay car, derived once from
/// the `.psxw` vertex groups while the loading screen is up.
static mut CAR_WHEEL_CENTRES: [[Vec3I16; 4]; CAR_SLOTS] = [[Vec3I16::ZERO; 4]; CAR_SLOTS];

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
static mut CAR_MATERIALS: [[(u8, u8, u8); CAR_MAX_VERTS]; CAR_SLOTS] =
    [[(128, 128, 128); CAR_MAX_VERTS]; CAR_SLOTS];

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
    for which in 0..CAR_SLOTS {
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
/// Positions and normals as the GTE loads them: the packed `x | y << 16`
/// word, and `z`, which a halfword load sign-extends for free. Same six bytes
/// a vertex as `Vec3I16`, but one aligned word read instead of two halves and
/// a shift-or per register.
static mut CAR_VERT_XY: [[u32; CAR_VERT_CAP]; CAR_SLOTS] = [[0; CAR_VERT_CAP]; CAR_SLOTS];
static mut CAR_VERT_Z: [[i16; CAR_VERT_CAP]; CAR_SLOTS] = [[0; CAR_VERT_CAP]; CAR_SLOTS];
static mut CAR_NORMAL_XY: [[u32; CAR_VERT_CAP]; CAR_SLOTS] = [[0; CAR_VERT_CAP]; CAR_SLOTS];
static mut CAR_NORMAL_Z: [[i16; CAR_VERT_CAP]; CAR_SLOTS] = [[0; CAR_VERT_CAP]; CAR_SLOTS];
/// How many of those entries each model actually filled.
static mut CAR_VERT_COUNT: [u16; CAR_SLOTS] = [0; CAR_SLOTS];
/// Triangle indices, decoded the same way and for the same reason: `Mesh::face`
/// rebuilds three `u16` from six `lbu` behind a stride branch, once per face
/// per car per view.
static mut CAR_FACES: [[[u16; 3]; CAR_FACE_CAP]; CAR_SLOTS] = [[[0; 3]; CAR_FACE_CAP]; CAR_SLOTS];
/// Depth-sorted face keys for one car draw (`submit_car_faces`).
static mut CAR_SORT_KEYS: [u32; CAR_FACE_CAP] = [0; CAR_FACE_CAP];
static mut CAR_SORT_SPARE: [u32; CAR_FACE_CAP] = [0; CAR_FACE_CAP];
static mut CAR_FACE_COUNT: [u16; CAR_SLOTS] = [0; CAR_SLOTS];

/// Decode one gameplay car's vertices and normals into the aligned tables.
fn decode_car_geometry(blob: &[u8], which: usize) {
    let Ok(mesh) = Mesh::from_bytes(blob) else {
        return;
    };
    let count = (mesh.vert_count() as usize).min(CAR_VERT_CAP);
    for i in 0..count {
        unsafe {
            let v = mesh.vertex(i as u16);
            let n = mesh.vertex_normal(i as u16).unwrap_or(Vec3I16::ZERO);
            CAR_VERT_XY[which][i] = v.xy_packed();
            CAR_VERT_Z[which][i] = v.z;
            CAR_NORMAL_XY[which][i] = n.xy_packed();
            CAR_NORMAL_Z[which][i] = n.z;
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
    build_wheel_lists();
}

/// Local transforms shared by every vertex in a front or rear wheel group.
struct WheelPose {
    front: Mat3I16,
    rear: Mat3I16,
    /// Front/rear travel in whole object-space uu.
    travel: [i16; 2],
}

/// Wheel vertex indices per car, grouped by wheel in [`WHEEL_ORDER`];
/// `WHEEL_BOUNDS` is where each group starts, with the total last. Built at
/// boot so the per-frame pass never scans the bodywork.
static mut WHEEL_LIST: [[u16; WHEEL_CAP]; CAR_SLOTS] = [[0; WHEEL_CAP]; CAR_SLOTS];
/// Wheel vertices a car may have. The committed cars have 72 to 96; a mesh
/// with more poses only the first `WHEEL_CAP`.
const WHEEL_CAP: usize = 128;
static mut WHEEL_BOUNDS: [[u16; 5]; CAR_SLOTS] = [[0; 5]; CAR_SLOTS];
/// Each listed wheel vertex's posing input, three words in list order: its
/// offset from its wheel's pivot (x and y packed), that offset's z with the
/// normal's z above it, and the normal's x and y packed. Main-RAM loads are
/// the posing pass's cost, and these are three where the tables were nine.
static mut WHEEL_IN: [[[u32; 3]; WHEEL_CAP]; CAR_SLOTS] = [[[0; 3]; WHEEL_CAP]; CAR_SLOTS];
/// Wheel slots in posing order: the front axle (front-left, front-right),
/// then the rear (rear-left, rear-right), so each axle's turn and each
/// wheel's pivot are loaded once.
const WHEEL_ORDER: [u8; 4] = [2, 3, 0, 1];
/// This frame's posed wheel vertices as GTE words (position xy, z, normal
/// xy, z), indexed by vertex; only wheel entries are ever read.
static mut POSED_WHEELS: [[u32; 4]; CAR_VERT_CAP] = [[0; 4]; CAR_VERT_CAP];

/// Vertices `project_car_animated` projects for `which`.
fn car_projected_count(which: usize) -> usize {
    (unsafe { CAR_VERT_COUNT[which] } as usize)
        .min(CAR_VERT_CAP)
        .min(CAR_MAX_VERTS)
        .min(CAR_WHEELS[which].len())
}

fn build_wheel_lists() {
    for which in 0..CAR_SLOTS {
        let slots = CAR_WHEELS[which];
        let count = car_projected_count(which);
        let (vxy, vz) = unsafe { (&CAR_VERT_XY[which], &CAR_VERT_Z[which]) };
        let (nxy, nz) = unsafe { (&CAR_NORMAL_XY[which], &CAR_NORMAL_Z[which]) };
        let mut n = 0;
        for (w, &wheel) in WHEEL_ORDER.iter().enumerate() {
            unsafe { WHEEL_BOUNDS[which][w] = n as u16 };
            let c = unsafe { CAR_WHEEL_CENTRES[which][wheel as usize] };
            for (i, &slot) in slots.iter().enumerate().take(count) {
                if slot != wheel || n == WHEEL_CAP {
                    continue;
                }
                let local = Vec3I16::new(
                    (vxy[i] as i16).wrapping_sub(c.x),
                    ((vxy[i] >> 16) as i16).wrapping_sub(c.y),
                    vz[i].wrapping_sub(c.z),
                );
                unsafe {
                    WHEEL_LIST[which][n] = i as u16;
                    WHEEL_IN[which][n] = [
                        local.xy_packed(),
                        (local.z as u16 as u32) | ((nz[i] as u16 as u32) << 16),
                        nxy[i],
                    ];
                }
                n += 1;
            }
        }
        unsafe { WHEEL_BOUNDS[which][4] = n as u16 };
    }
}

/// Pose every wheel vertex of `which` into `POSED_WHEELS`: the steer and roll
/// about the wheel's pivot, then the suspension travel.
///
/// The CPU version applied `Mat3I16::transform_i32` twice per vertex (pivot
/// offset and normal); this moves both 3x3 products onto the GTE. MVMVA with a zero translation computes `(RT . V) >> 12` on the full
/// 44-bit sum, which is the CPU's `transform_i32` exactly while that sum fits
/// in 32 bits, as it does for a pivot-relative offset or a unit normal. Read
/// back from MAC1-3, not the saturating IR registers, and narrowed the same
/// way, so the words match the CPU path bit for bit. Eighteen CPU multiplies
/// a vertex were most of the car projection's cost.
fn pose_wheels(which: usize, centres: &[Vec3I16; 4], pose: &WheelPose) {
    let list = unsafe { &WHEEL_LIST[which] };
    let input = unsafe { &WHEEL_IN[which] };
    let bounds = unsafe { WHEEL_BOUNDS[which] };
    let posed = unsafe { &mut POSED_WHEELS };
    let narrow = |value: i32| value.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    scene::load_translation(Vec3I32::new(0, 0, 0));
    for (axle, rotation) in [&pose.front, &pose.rear].into_iter().enumerate() {
        scene::load_rotation(rotation);
        let travel = pose.travel[axle] as i32;
        for w in axle * 2..axle * 2 + 2 {
            let centre = centres[WHEEL_ORDER[w] as usize];
            let (cx, cy, cz) = (centre.x as i32, centre.y as i32 + travel, centre.z as i32);
            let range = bounds[w] as usize..bounds[w + 1] as usize;
            for (&i, &[local_xy, zs, n_xy]) in list[range.clone()].iter().zip(&input[range]) {
                // The SDK's padded schedule: two NOPs between the V0 writes
                // and MVMVA, the console-confirmed fix for the HWB-010/011
                // commit slip that has MVMVA read the previous V0.x.
                let turned = scene::transform_vertex_scheduled(Vec3I16::new(
                    local_xy as i16,
                    (local_xy >> 16) as i16,
                    zs as i16,
                ));
                let lit = scene::transform_vertex_scheduled(Vec3I16::new(
                    n_xy as i16,
                    (n_xy >> 16) as i16,
                    (zs >> 16) as i16,
                ));
                let p = Vec3I16::new(
                    narrow(cx + turned.x),
                    narrow(cy + turned.y),
                    narrow(cz + turned.z),
                );
                let n = Vec3I16::new(narrow(lit.x), narrow(lit.y), narrow(lit.z));
                posed[i as usize] = [p.xy_packed(), p.z_packed(), n.xy_packed(), n.z_packed()];
            }
        }
    }
}

/// Is a projected triangle wound clockwise on screen, and so facing away?
///
/// The engine's own test is private, and this is one cross product.
#[inline]
fn car_back_facing(a: &CarLit, b: &CarLit, c: &CarLit) -> bool {
    let abx = b.sx() - a.sx();
    let aby = b.sy() - a.sy();
    let acx = c.sx() - a.sx();
    let acy = c.sy() - a.sy();
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
fn draw_far_car<'a>(
    seat: usize,
    body: &sim::Car,
    view: &View,
    tris: &mut PrimitiveArena<'a, TriGouraud>,
    ot: &mut OtFrame<'a, OT_DEPTH>,
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

/// Sort `keys` ascending by their high 16 bits with two 256-bucket counting
/// passes over the depth bytes, ties kept in index order. `CAR_SORT_SPARE`
/// is the scratch the second pass writes through.
fn radix_sort_u32_high16(keys: &mut [u32]) {
    let n = keys.len();
    let spare = unsafe { &mut CAR_SORT_SPARE[..n] };
    let mut src: &mut [u32] = keys;
    let mut dst: &mut [u32] = spare;
    for shift in [16u32, 24] {
        let mut counts = [0u16; 256];
        for &k in src.iter() {
            counts[((k >> shift) & 0xff) as usize] += 1;
        }
        let mut start = 0u16;
        for c in counts.iter_mut() {
            let n = *c;
            *c = start;
            start += n;
        }
        for &k in src.iter() {
            let b = ((k >> shift) & 0xff) as usize;
            dst[counts[b] as usize] = k;
            counts[b] += 1;
        }
        core::mem::swap(&mut src, &mut dst);
    }
    // Two passes leave the result back in `keys`.
}

/// Sort `keys` ascending by their high 16 bits, ties kept in index order, and
/// return the sorted run, which is in `keys` or in `CAR_SORT_SPARE`. A car's
/// faces span a few hundred depth units, so when the run's depths `lo..=hi`
/// fit one 256-bucket pass, a single counting pass over just that range puts
/// the faces in the order the two byte passes did, for about half the cost.
fn sort_by_depth(keys: &mut [u32], lo: u32, hi: u32) -> &[u32] {
    if keys.is_empty() || hi - lo >= 256 {
        radix_sort_u32_high16(keys);
        return keys;
    }
    let spare = unsafe { &mut CAR_SORT_SPARE[..keys.len()] };
    // Only the buckets the depths span are cleared and read: a car is a
    // hundred-odd uu deep, and clearing all 256 was most of the sort's own
    // time. Every key's bucket is below `hi - lo + 1 <= 256`, and the
    // running starts stay below `keys.len()`, so the indexing is unchecked.
    let range = (hi - lo) as usize + 1;
    let mut counts = core::mem::MaybeUninit::<[u16; 256]>::uninit();
    let counts = unsafe {
        let p = counts.as_mut_ptr() as *mut u16;
        core::ptr::write_bytes(p, 0, range);
        core::slice::from_raw_parts_mut(p, range)
    };
    for &k in keys.iter() {
        unsafe { *counts.get_unchecked_mut(((k >> 16) - lo) as usize) += 1 };
    }
    let mut start = 0u16;
    for c in counts.iter_mut() {
        let n = *c;
        *c = start;
        start += n;
    }
    for &k in keys.iter() {
        let b = ((k >> 16) - lo) as usize;
        unsafe {
            let at = counts.get_unchecked_mut(b);
            *spare.get_unchecked_mut(*at as usize) = k;
            *at += 1;
        }
    }
    spare
}

fn submit_car_faces<'a>(
    faces: &[[u16; 3]],
    projected: &[CarLit],
    tris: &mut PrimitiveArena<'a, TriGouraud>,
    ot: &mut OtFrame<'a, OT_DEPTH>,
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
    // Each front face's packet is written as soon as the face passes the
    // back-face test, while its corners are still in registers, and the sort
    // key carries the packet's place in the arena rather than the face's.
    // Re-reading the face and its three corners from main RAM after the sort
    // was nine stalled loads per drawn face. Same packets, linked in the same
    // order (the radix sort is stable and front faces keep mesh order), so
    // the same pixels; only where each packet sits in the arena moves.
    let mut n = 0usize;
    let mut first: *mut TriGouraud = core::ptr::null_mut();
    let (mut lo, mut hi) = (u32::MAX, 0u32);
    // Every face index is below the mesh's vertex count, which `projected`
    // covers: `decode_car_geometry` drops any face that is not, once at
    // boot, and `draw_cars` passes no faces if fewer vertices were
    // projected. Checking all three again on every face of every frame was
    // nine instructions of the loop that runs most.
    debug_assert!(faces
        .iter()
        .take(CAR_FACE_CAP)
        .all(|f| f.iter().all(|&i| (i as usize) < projected.len())));
    for face in faces.iter().take(CAR_FACE_CAP) {
        let (a, b, c) = unsafe {
            (
                projected.get_unchecked(face[0] as usize),
                projected.get_unchecked(face[1] as usize),
                projected.get_unchecked(face[2] as usize),
            )
        };
        if car_back_facing(a, b, c) {
            continue;
        }
        let depth = ((a.sz + b.sz + c.sz) as i32 / 3).clamp(0, 0xffff) as u32;
        // Word for word what `TriGouraud::new` builds from the unpacked
        // corners: the GTE's SXY is the GPU's vertex word and its RGB the
        // colour word, both with nothing in the bits the packet leaves clear.
        let prim = TriGouraud {
            tag: 0,
            color0_cmd: TRI_GOURAUD_CMD | a.rgb,
            v0: a.xy,
            color1: b.rgb,
            v1: b.xy,
            color2: c.rgb,
            v2: c.xy,
        };
        let Some(t) = tris.push(prim) else {
            break;
        };
        if n == 0 {
            first = t;
        }
        keys[n] = (depth << 16) | n as u32;
        n += 1;
        lo = lo.min(depth);
        hi = hi.max(depth);
    }
    // A counting sort on the 16-bit depth (the index rides in the low half),
    // or the two-pass radix when the depths spread too far for one pass:
    // either is a fraction of quicksort's cost at this size, and the order
    // is total, so painter's order inside a slot is exact either way.
    for &key in sort_by_depth(&mut keys[..n], lo, hi) {
        // SAFETY: the arena hands out consecutive slots, so the n packets
        // pushed above start at `first`, and the index came from that count.
        let t = unsafe { &mut *first.add((key & 0xffff) as usize) };
        ot.add_packet_depth(DEPTH_RANGE, (key >> 16) as i32, t);
    }
}

/// RTPT then NCCT for three vertices that share a material, as one asm
/// block: positions `p` and normals `n` as packed (XY, Z) register words,
/// `rgbc` the material. Returns (SXY0-2, SZ1-3, RGB0-2).
///
/// The GTE sees exactly what the separate `mtc2!`/`mfc2!` sequence gave it:
/// the same register writes in the same order, RTPT straight after the last
/// vertex write and NCCT straight after RGBC, as before. What goes is the
/// glue around each transfer, a `move` into `$8` before every write and a
/// NOP plus a `move` after every read: consecutive reads fill one another's
/// load delay, and one NOP closes the last. About thirty instructions a
/// triple, on the pass that projects every car vertex every frame.
#[cfg(target_arch = "mips")]
#[inline(always)]
fn rtpt_ncct(p: [u32; 6], n: [u32; 6], rgbc: u32) -> ([u32; 3], [u32; 3], [u32; 3]) {
    let (s0, s1, s2, z1, z2, z3): (u32, u32, u32, u32, u32, u32);
    let (c0, c1, c2): (u32, u32, u32);
    unsafe {
        core::arch::asm!(
            // MTC2 $8..$13 into VXY0, VZ0, VXY1, VZ1, VXY2, VZ2.
            ".word 0x48880000",
            ".word 0x48890800",
            ".word 0x488a1000",
            ".word 0x488b1800",
            ".word 0x488c2000",
            ".word 0x488d2800",
            // RTPT.
            ".word 0x4a080030",
            // MFC2 SXY0-2 and SZ1-3 into $8..$13.
            ".word 0x48086000",
            ".word 0x48096800",
            ".word 0x480a7000",
            ".word 0x480b8800",
            ".word 0x480c9000",
            ".word 0x480d9800",
            // MTC2 the normals ($14, $15, $24, $25, $2, $3) into V0-V2 and
            // the material ($4) into RGBC.
            ".word 0x488e0000",
            ".word 0x488f0800",
            ".word 0x48981000",
            ".word 0x48991800",
            ".word 0x48822000",
            ".word 0x48832800",
            ".word 0x48843000",
            // NCCT.
            ".word 0x4a08003f",
            // MFC2 RGB0-2 into $14, $15, $24, then the last read's delay.
            ".word 0x480ea000",
            ".word 0x480fa800",
            ".word 0x4818b000",
            ".word 0",
            inlateout("$8") p[0] => s0,
            inlateout("$9") p[1] => s1,
            inlateout("$10") p[2] => s2,
            inlateout("$11") p[3] => z1,
            inlateout("$12") p[4] => z2,
            inlateout("$13") p[5] => z3,
            inlateout("$14") n[0] => c0,
            inlateout("$15") n[1] => c1,
            inlateout("$24") n[2] => c2,
            in("$25") n[3],
            in("$2") n[4],
            in("$3") n[5],
            in("$4") rgbc,
            options(nostack, nomem, preserves_flags),
        );
    }
    ([s0, s1, s2], [z1 & 0xffff, z2 & 0xffff, z3 & 0xffff], [c0, c1, c2])
}

/// Project and light one car mesh into `CAR_PROJ`, returning how many vertices
/// landed there. The batched GTE path (RTPT + NCCT for a run of three) is the
/// same one the engine uses, so this is the engine's `submit_lit_mesh` with
/// the material scan replaced by a table lookup, and the authored wheel groups
/// posed on the way through.
fn project_car_animated(which: usize, materials: &[u32; CAR_MAX_VERTS], slots: &[u8]) -> usize {
    // Aligned tables decoded at boot by `decode_car_geometry`, not the mesh
    // blob: the byte-at-a-time decode was most of this stage's cost.
    let (vxy, vz) = unsafe { (&CAR_VERT_XY[which], &CAR_VERT_Z[which]) };
    let (nxy, nz) = unsafe { (&CAR_NORMAL_XY[which], &CAR_NORMAL_Z[which]) };
    let count = (unsafe { CAR_VERT_COUNT[which] } as usize)
        .min(CAR_VERT_CAP)
        .min(CAR_MAX_VERTS)
        .min(slots.len());
    let proj = unsafe { &mut CAR_PROJ };
    // Position and normal registers for one vertex: bodywork straight from
    // the tables, a wheel from what `pose_wheels` left for this frame.
    let posed = unsafe { &POSED_WHEELS };
    let vertex = |i: usize| -> (u32, u32, u32, u32) {
        let slot = slots[i];
        if slot == WHEEL_NONE || slot >= 4 {
            (vxy[i], vz[i] as i32 as u32, nxy[i], nz[i] as i32 as u32)
        } else {
            let w = posed[i];
            (w[0], w[1], w[2], w[3])
        }
    };
    // RTPT for three positions, then NCCT for their normals when the three
    // share a material (NCCS each otherwise): `psx_gte::lighting`'s
    // `project_lit_triangle`, reading and writing the packed words directly.
    let mut vi = 0;
    while vi + 2 < count {
        let a = vertex(vi);
        let b = vertex(vi + 1);
        let c = vertex(vi + 2);
        let (ma, mb, mc) = (materials[vi], materials[vi + 1], materials[vi + 2]);
        #[cfg(target_arch = "mips")]
        if ma == mb && mb == mc {
            let (sxy, sz, rgb) =
                rtpt_ncct([a.0, a.1, b.0, b.1, c.0, c.1], [a.2, a.3, b.2, b.3, c.2, c.3], ma);
            for k in 0..3 {
                proj[vi + k] = CarLit {
                    xy: sxy[k],
                    rgb: rgb[k],
                    sz: sz[k],
                };
            }
            vi += 3;
            continue;
        }
        mtc2!(0, a.0);
        mtc2!(1, a.1);
        mtc2!(2, b.0);
        mtc2!(3, b.1);
        mtc2!(4, c.0);
        mtc2!(5, c.1);
        // SAFETY: V0-V2 loaded; the car's rotation, translation and the
        // projection were loaded by `draw_cars`.
        unsafe { psx_gte::ops::rtpt() };
        let sxy = [mfc2!(12), mfc2!(13), mfc2!(14)];
        let sz = [mfc2!(17) & 0xffff, mfc2!(18) & 0xffff, mfc2!(19) & 0xffff];
        let rgb = if ma == mb && mb == mc {
            mtc2!(0, a.2);
            mtc2!(1, a.3);
            mtc2!(2, b.2);
            mtc2!(3, b.3);
            mtc2!(4, c.2);
            mtc2!(5, c.3);
            mtc2!(6, ma);
            // SAFETY: V0-V2 hold the normals, RGBC the shared material, and
            // the light rig was loaded by `draw_cars`.
            unsafe { psx_gte::ops::ncct() };
            [mfc2!(20), mfc2!(21), mfc2!(22)]
        } else {
            let mut rgb = [0u32; 3];
            for (k, (n, m)) in [((a.2, a.3), ma), ((b.2, b.3), mb), ((c.2, c.3), mc)]
                .into_iter()
                .enumerate()
            {
                mtc2!(0, n.0);
                mtc2!(1, n.1);
                mtc2!(6, m);
                // SAFETY: as above, one normal at a time.
                unsafe { psx_gte::ops::nccs() };
                rgb[k] = mfc2!(22);
            }
            rgb
        };
        for k in 0..3 {
            proj[vi + k] = CarLit {
                xy: sxy[k],
                rgb: rgb[k],
                sz: sz[k],
            };
        }
        vi += 3;
    }
    while vi < count {
        let a = vertex(vi);
        mtc2!(0, a.0);
        mtc2!(1, a.1);
        // SAFETY: V0 loaded; scene state as above.
        unsafe { psx_gte::ops::rtps() };
        let (xy, sz) = (mfc2!(14), mfc2!(19) & 0xffff);
        mtc2!(0, a.2);
        mtc2!(1, a.3);
        mtc2!(6, materials[vi]);
        // SAFETY: V0 holds the normal, RGBC the material.
        unsafe { psx_gte::ops::nccs() };
        proj[vi] = CarLit {
            xy,
            rgb: mfc2!(22),
            sz,
        };
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
/// How far into the pitch the translucent cover over a goal mouth reaches and
/// how high it climbs: (lowest and highest inward offset, highest point), over
/// the crossbar and the rings from the wall top round the roof curve.
static mut GOAL_COVER_REACH: (i32, i32, i32) = (0, 0, 0);

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
    let (mut lo, mut hi, mut top) = (0, 0, sim::GOAL_H);
    for p in &profile[WALL_TOP_RING..] {
        lo = lo.min(p.0);
        hi = hi.max(p.0);
        top = top.max(p.1);
    }
    unsafe {
        WALL_PROFILE = profile;
        COVER_PROFILE_V = cover_v;
        GOAL_COVER_REACH = (lo, hi, top);
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
    flats: PrimitiveArena<'a, QuadFlat>,
    glow: PrimitiveArena<'a, GlowQuad>,
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
        let Some((sp, z_sum)) = project_quad(&corners) else {
            return;
        };
        if !quad_overlaps_view(&sp) {
            return;
        }
        count_kept!();
        self.emit(sp, z_sum / 4 + bias, colors);
    }

    /// Emit a projected quad of one colour. See [`MAX_FLAT_QUADS`].
    fn emit_flat(&mut self, sp: [(i16, i16); 4], depth: i32, color: Rgb) {
        if let Some(q) = self.flats.push(QuadFlat::new(sp, color.0, color.1, color.2)) {
            self.ot.add_packet_depth(DEPTH_RANGE, depth, q);
        } else {
            count_overflow!();
        }
    }

    /// As [`Builder::quad_biased`], but semi-transparent: the GPU averages the
    /// quad with what is behind it. What the boost plume is made of.
    fn quad_blended(&mut self, corners: [(i32, i32, i32); 4], colors: [Rgb; 4], bias: i32) {
        let Some((sp, z_sum)) = project_quad(&corners) else {
            return;
        };
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
        let Some((sp, z_sum)) = project_quad(&corners) else {
            return;
        };
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

    /// [`Self::quad_tex_projected`] for tints already in GPU colour words:
    /// the same packet, with the words dropped in unpacked.
    #[allow(clippy::too_many_arguments)]
    #[inline(always)]
    fn quad_tex_words(
        &mut self,
        sp: [(i16, i16); 4],
        z_sum: i32,
        uvs: [u16; 4],
        tints: [u32; 4],
        bias: i32,
        packet: TexturedGouraudPacketMaterial,
        blended: bool,
    ) {
        let mut prim = QuadTexturedGouraud::with_packet_material_packed_uv_words(
            sp,
            uvs,
            [(0, 0, 0); 4],
            packet,
        );
        prim.color0_cmd |= tints[0];
        prim.color1 = tints[1];
        prim.color2 = tints[2];
        prim.color3 = tints[3];
        if blended {
            prim.color0_cmd |= SEMI_TRANSPARENT;
        }
        if let Some(q) = self.textured.push(prim) {
            self.ot.add_packet_depth(DEPTH_RANGE, z_sum / 4 + bias, q);
        } else {
            count_overflow!();
        }
    }

    /// Emit an additive light quad from projected corners. See [`GlowQuad`].
    fn emit_glow(
        &mut self,
        sp: [(i16, i16); 4],
        depth: i32,
        uvs: [u16; 4],
        tints: [Rgb; 4],
        packet: TexturedGouraudPacketMaterial,
    ) {
        let mut quad =
            QuadTexturedGouraud::with_packet_material_packed_uv_words(sp, uvs, tints, packet);
        quad.color0_cmd |= SEMI_TRANSPARENT;
        if let Some(g) = self.glow.push(GlowQuad {
            quad,
            restore: GLOW_RESTORE,
        }) {
            self.ot.add_packet_depth(DEPTH_RANGE, depth, g);
        } else {
            count_overflow!();
        }
    }

    /// The whole glow tile, as it is sampled by every light.
    const GLOW_UVS: [u16; 4] = [
        uvw(GLOW_U0, GLOW_V0),
        uvw(GLOW_U0 + GLOW_W, GLOW_V0),
        uvw(GLOW_U0, GLOW_V0 + GLOW_W),
        uvw(GLOW_U0 + GLOW_W, GLOW_V0 + GLOW_W),
    ];

    /// A glow lying in the world: four corners, projected like any quad.
    fn glow_quad(
        &mut self,
        corners: [(i32, i32, i32); 4],
        tint: Rgb,
        bias: i32,
        packet: TexturedGouraudPacketMaterial,
    ) {
        count_offered!();
        let Some((sp, z_sum)) = project_quad(&corners) else {
            return;
        };
        if !quad_overlaps_view(&sp) {
            return;
        }
        count_kept!();
        self.emit_glow(sp, z_sum / 4 + bias, Self::GLOW_UVS, [tint; 4], packet);
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
        // How far the pitch may run under the ramp where its outline cannot
        // follow the ramp's foot exactly (see below).
        const FLOOR_TUCK: i32 = 48;
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
        // The end walls' curve is smaller (`sim::END_RAMP_R`), so the pitch
        // runs closer to them.
        //
        // The grid's first point past a post is up to a grid step beyond
        // the mouth, and the pitch edge from the mouth's last point to it
        // ran diagonally in front of the post, leaving a sliver of sky at the
        // post's foot. That point runs out to the line too: the pitch is
        // drawn before every wall, so the ramp covers what lies under it.
        const GRID: i32 = sim::HALF_X * 2 / TILES_X / FLOOR_SPLIT_MAX;
        let foot_x = sim::HALF_X - RAMP_R;
        let foot_z = if x.abs() < sim::GOAL_HALF_W + GRID {
            sim::HALF_Z
        } else {
            sim::HALF_Z - sim::END_RAMP_R
        };
        let (x, z) = (x.clamp(-foot_x, foot_x), z.clamp(-foot_z, foot_z));

        // The corner planes take the same radius off, measured along their own
        // normal, which is what keeps the join continuous round the chamfer
        // instead of stepping at the two places it meets the straight walls.
        //
        // Tucked under the ramp by FLOOR_TUCK, like the joints below: the end
        // joints' smaller curve puts their foot outside the corner plane's
        // foot line near the plane, and a pitch cut on that line showed a
        // sliver of sky along the end joint.
        let limit = sim::CORNER - ((RAMP_R - FLOOR_TUCK) * 5793 >> 12); // sqrt(2)
        let sum = x.abs() + z.abs();
        let (x, z) = if sum <= limit {
            (x, z)
        } else {
            (x * limit / sum, z * limit / sum)
        };

        // And cut the rounded joints either side of each corner plane: the
        // foot of the ramp is each joint chord a ramp radius further in.
        // The pitch is sampled on a 256-uu grid, and between two samples on
        // either side of a joint's bend its edge cuts inside the ramp's foot:
        // a sliver of sky showed through. The pitch is drawn before every
        // wall, so it can run under the ramp with nothing to fight: tuck it
        // FLOOR_TUCK further out than the foot along the joints. Only the
        // corner boxes the joints stand in can reach them, so test the box.
        let (mut x, mut z) = (x, z);
        // Each chord takes the smaller of the ramp radii at its two ends: the
        // end joints' curve shrinks towards the end wall, and a pitch edge
        // that stops short of the ramp's foot shows sky, where one that runs
        // a little under the ramp is drawn over.
        const CHORD_ENDS: [(usize, usize); 4] = [(0, 1), (1, 2), (3, 4), (4, 5)];
        if x.abs() > sim::CORNER_JOINT_PTS[5].0 - RAMP_R && z.abs() > sim::CORNER_JOINT_PTS[0].1 - RAMP_R {
            for (&(nx, nz, off), &(a, b)) in sim::CORNER_JOINT_PLANES.iter().zip(&CHORD_ENDS) {
                let pts = sim::CORNER_JOINT_PTS;
                let foot = sim::ramp_radius(pts[a].0).min(sim::ramp_radius(pts[b].0));
                let over = ((x.abs() * nx + z.abs() * nz) >> 12) - ((off >> 2) - foot + FLOOR_TUCK);
                if over > 0 {
                    x -= x.signum() * (nx * over >> 12);
                    z -= z.signum() * (nz * over >> 12);
                }
            }
        }
        (x, z)
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
        // A half-height view shows a tile 900 uu out at a dozen pixels tall,
        // where the two-way cells stop being visible; the top-and-bottom
        // kickoff, both views down the long axis, is the scene that needs it.
        let (near_distance, mid_distance) = if split_view() {
            (400, 900)
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
            // The shared edge: constant sx against an x-step neighbour,
            // constant sz against a z-step one.
            let at = if dx + dz > 0 { nu } else { 0 };
            // A coarse neighbour whose end of this edge is saturated (the
            // GTE stores a screen coordinate no further than -1024 or 1023)
            // is drawn as clipped pieces, which follow the true edge. The
            // straight screen segment to the saturated end points somewhere
            // else, and snapping this tile's edge onto it left a wedge of
            // sky between the two.
            let ends = if dx != 0 {
                (g[at][0], g[at][nu])
            } else {
                (g[0][at], g[nu][at])
            };
            let saturated =
                |p: Option<(i16, i16, i32)>| p.is_some_and(|p| screen_saturated((p.0, p.1)));
            if saturated(ends.0) || saturated(ends.1) {
                continue;
            }
            conformed |= 1 << k;
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
        g: &[[Option<(i16, i16, i32)>; FLOOR_SPLIT_MAX as usize + 1]; FLOOR_SPLIT_MAX as usize + 1],
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
        // Behind BOTH tiles that share the edge: its own slot, drawn just
        // before the pitch's.
        let (ca, cb) = (tinted(GRASS_A, ta), tinted(GRASS_A, tb));
        if let Some(q) = self.arena.push(QuadGouraud::new(sp, [ca, cb, ca, cb])) {
            self.ot.add_packet(UNDERDRAW_SLOT, q);
        }
    }

    /// One far floor tile as a single quad: four chamfered corners, four
    /// projections, one emit. The general grid path costs several times this
    /// in fixed machinery, and at n == 1 buys nothing for it. Out of line for
    /// the same i-cache reason as [`Self::conform_tile`].
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    fn floor_tile_far<const SPLIT: bool>(
        &mut self,
        light: &[[u32; FLOOR_GZ]; FLOOR_GX],
        gx: usize,
        gz: usize,
    ) {
        count_offered!();
        let mut sp = [(0i16, 0i16); 4];
        let mut behind = 0;
        let (split, near) = (SPLIT, near_sz::<SPLIT>());
        let step = FLOOR_SPLIT_MAX as usize;
        let at = [
            (gx, gz),
            (gx + step, gz),
            (gx, gz + step),
            (gx + step, gz + step),
        ];
        // Three corners through one RTPT and the fourth through RTPS: the
        // same per-vertex projection, fewer GTE round trips.
        let corner = |k: usize| {
            let (cx, cz) = unsafe { *FLOOR_POS.get_unchecked(at[k].0).get_unchecked(at[k].1) };
            Vec3I16::new(cx, 0, cz)
        };
        let t = scene::project_triangle_scheduled(corner(0), corner(1), corner(2));
        let projected = [t[0], t[1], t[2], project(corner(3))];
        for (k, p) in projected.iter().enumerate() {
            if p.sz as i32 <= near {
                // A full view drops the tile, as it always did.
                if !split {
                    return;
                }
                behind += 1;
            }
            sp[k] = (p.sx, p.sy);
        }
        // A one-quad tile is a whole 1024-uu tile, and the distance band
        // that picks it is measured to the tile's centre, so its near edge
        // can still pass right by the camera: see `queue_pieces`.
        let whole = behind == 0 && (!split || gpu_draws_whole(&sp));
        if whole {
            if !sp.iter().any(|&(x, y)| on_view(x, y)) {
                return;
            }
        } else if behind == 4 || (behind == 0 && !quad_overlaps_view(&sp)) {
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
        let last = (GRASS_TILE_W - 1) as u8;
        if !whole {
            let world = at.map(|(ix, iz)| {
                let (x, z) = unsafe { *FLOOR_POS.get_unchecked(ix).get_unchecked(iz) };
                (x as i32, 0, z as i32)
            });
            let uvs = [(0, 0), (last, 0), (0, last), (last, last)];
            Self::queue_pieces(world, uvs, tints, Pieces::Floor);
            return;
        }
        let uvs = [uvw(0, 0), uvw(last, 0), uvw(0, last), uvw(last, last)];
        self.floor_quad(sp, uvs, tints);
    }

    /// One pitch quad into the pitch's own slot. See [`FLOOR_SLOT`].
    #[inline(always)]
    fn floor_quad(&mut self, sp: [(i16, i16); 4], uvs: [u16; 4], tints: [u32; 4]) {
        let mut prim = QuadTexturedGouraud::with_packet_material_packed_uv_words(
            sp,
            uvs,
            [(0, 0, 0); 4],
            GRASS_PACKET,
        );
        prim.color0_cmd |= tints[0];
        prim.color1 = tints[1];
        prim.color2 = tints[2];
        prim.color3 = tints[3];
        if let Some(q) = self.textured.push(prim) {
            self.ot.add_packet(FLOOR_SLOT, q);
        } else {
            count_overflow!();
        }
    }

    /// Note a pitch or wall quad the GPU cannot draw whole, for
    /// [`Self::draw_pieces`] to clip once the phase is off the scratchpad
    /// stack (the pitch and wall phases have no room left on it).
    ///
    /// Near the camera a quad can have a corner behind the near plane or too
    /// near for the GTE to project right (see [`GTE_TRUE_SZ`]), or a corner
    /// so close that its projection lands a thousand pixels off screen, and
    /// the rasteriser drops any triangle with an edge 1024 or more pixels
    /// wide or 512 tall. Either way the whole quad went and the sky showed
    /// through the pitch and the foot of the walls, in split screen most of
    /// all, where pitch tiles and wall spans are cut coarser (Manny's attract
    /// demo, 2026-10-03). `world`, `uvs` and `tints` are in the packets'
    /// corner order: (0, 0), (1, 0), (0, 1), (1, 1). Kept inline and cheap:
    /// the pitch and wall loops reach it on the scratchpad stack, which has
    /// no room for more. A quad past the queue's end is dropped, as every
    /// such quad used to be.
    #[inline(always)]
    fn queue_pieces(
        world: [(i32, i32, i32); 4],
        uvs: [(u8, u8); 4],
        tints: [u32; 4],
        kind: Pieces,
    ) {
        unsafe {
            let n = PIECE_JOB_COUNT;
            if n < MAX_PIECE_JOBS {
                // Unchecked: `n` was just checked, and a panic path here
                // costs the scratchpad stack bytes it does not have.
                *PIECE_JOBS.get_unchecked_mut(n) = PieceJob {
                    world,
                    uvs,
                    tints,
                    kind,
                };
                PIECE_JOB_COUNT = n + 1;
            }
        }
    }

    /// [`Self::queue_pieces`] for one pitch sub-quad, by its first grid corner
    /// in `FLOOR_POS`. Out of line and cold so the pitch loop, which runs on
    /// the scratchpad stack, does not carry its frame.
    #[inline(never)]
    #[cold]
    fn queue_floor_quad(corner: (usize, usize), step: usize, uv: [u8; 4], tints: [u32; 4]) {
        let at = |di: usize, dj: usize| {
            let (x, z) = unsafe {
                *FLOOR_POS
                    .get_unchecked(corner.0 + di * step)
                    .get_unchecked(corner.1 + dj * step)
            };
            (x as i32, 0, z as i32)
        };
        let [ua, ub, va, vb] = uv;
        Self::queue_pieces(
            [at(0, 0), at(1, 0), at(0, 1), at(1, 1)],
            [(ua, va), (ub, va), (ua, vb), (ub, vb)],
            tints,
            Pieces::Floor,
        );
    }

    /// Note one wall quad the GPU cannot draw whole by its span, column level,
    /// column and rings (`at`), for [`Self::draw_pieces`] to sweep from
    /// `SPAN_COLS` and `WALL_PROFILE` and clip. A handful of byte stores, so
    /// the wall loop stays small in the I-cache and on the scratchpad stack.
    #[inline(always)]
    fn queue_wall_quad(at: [u8; 5], uv: [u8; 4], tints: [u32; 4], covered: bool) {
        unsafe {
            let n = WALL_JOB_COUNT;
            if n < MAX_PIECE_JOBS {
                // Unchecked for the same reason as `queue_pieces`.
                *WALL_JOBS.get_unchecked_mut(n) = WallJob {
                    at,
                    uv,
                    tints,
                    covered,
                };
                WALL_JOB_COUNT = n + 1;
            }
        }
    }

    /// The queued wall quads as world corners, swept exactly as `wall_span`
    /// sweeps its rings, then into [`Self::queue_pieces`].
    #[inline(never)]
    #[cold]
    fn sweep_wall_jobs() {
        let profile = unsafe { &WALL_PROFILE };
        for k in 0..unsafe { WALL_JOB_COUNT } {
            let job = unsafe { WALL_JOBS[k] };
            let [si, level, col, lo, hi] = job.at.map(|v| v as usize);
            let cols = unsafe { &SPAN_COLS[si][level] };
            let point = |c: usize, ring: usize| {
                let p = profile[ring];
                let pk = if ring <= CURVE_SEGS {
                    ramp_point(p, cols.ramp[c] as i32)
                } else {
                    p
                };
                (
                    cols.x[c] + ((cols.nx[c] * pk.0) >> 12),
                    -pk.1,
                    cols.z[c] + ((cols.nz[c] * pk.0) >> 12),
                )
            };
            let [u0, u1, v0, v1] = job.uv;
            let packet = if job.covered {
                COVER_PACKET
            } else {
                WALL_PACKET
            };
            Self::queue_pieces(
                [
                    point(col, lo),
                    point(col + 1, lo),
                    point(col, hi),
                    point(col + 1, hi),
                ],
                [(u0, v0), (u1, v0), (u0, v1), (u1, v1)],
                job.tints,
                Pieces::Wall {
                    packet,
                    blended: job.covered,
                },
            );
        }
        unsafe { WALL_JOB_COUNT = 0 };
    }

    /// Draw the quads the last phase queued with [`Self::queue_pieces`]. The
    /// GTE still holds the world view the phase projected with. Only a quad
    /// whose box is in the view is clipped: most of them straddle the camera
    /// beside or behind it, out of sight, and clipping every one cost single
    /// player its 60 fps. A full view clips none at all and so draws what
    /// it always did: the quads it queues are the ones the GPU dropped.
    #[inline(never)]
    fn draw_pieces(&mut self, cull: &Cull) {
        Self::sweep_wall_jobs();
        let count = if split_view() {
            unsafe { PIECE_JOB_COUNT }
        } else {
            0
        };
        for k in 0..count {
            let job = unsafe { PIECE_JOBS[k] };
            let w = job.world;
            let (mut lo, mut hi) = (w[0], w[0]);
            for p in &w[1..] {
                lo = (lo.0.min(p.0), lo.1.min(p.1), lo.2.min(p.2));
                hi = (hi.0.max(p.0), hi.1.max(p.1), hi.2.max(p.2));
            }
            let centre = ((lo.0 + hi.0) / 2, (lo.1 + hi.1) / 2, (lo.2 + hi.2) / 2);
            let half = (
                (hi.0 - lo.0) / 2 + 1,
                (hi.1 - lo.1) / 2 + 1,
                (hi.2 - lo.2) / 2 + 1,
            );
            if !cull.visible_box(centre, cull.extents(half)) {
                continue;
            }
            self.clip_piece(w, job.uvs, job.tints, job.kind, cull);
        }
        unsafe { PIECE_JOB_COUNT = 0 };
    }

    /// Clip a queued quad in camera space to the depth the GTE still
    /// projects right (see [`GTE_TRUE_SZ`]) and to a guard band around the
    /// view (480 pixels either side of its centre and 240 above and below),
    /// so every corner left projects close enough to the others for the GPU,
    /// and draw the clipped polygon as a fan of triangles (quads with a
    /// repeated corner), UVs and tints carried along the cut edges. Clipped
    /// corners are placed back in the world and projected by the GTE like
    /// any other, so they land on the screen line of the edge they were cut
    /// from. One clip per quad: cutting the quads into grids of pieces first
    /// cost single player its 60 fps (2026-10-03, train tape).
    #[inline(never)]
    #[cold]
    fn clip_piece(
        &mut self,
        world: [(i32, i32, i32); 4],
        uvs: [(u8, u8); 4],
        tints: [u32; 4],
        kind: Pieces,
        cull: &Cull,
    ) {
        // A polygon corner: world position, camera-space position, then u,
        // v, r, g, b, all carried along a cut edge by the same fraction.
        type Corner = [i32; 11];
        // Four corners, and one more for each of the five planes at most.
        const MAX: usize = 9;
        const H: i32 = PROJ_H as i32;
        const GUARD_X: i32 = 480;
        const GUARD_Y: i32 = 240;
        const NEAR: i32 = GTE_TRUE_SZ + 8;
        let corner = |k: usize| -> Corner {
            let (p, c) = (world[k], rgb_of(tints[k]));
            let d = (p.0 - cull.pos.0, p.1 - cull.pos.1, p.2 - cull.pos.2);
            [
                p.0,
                p.1,
                p.2,
                Cull::dot(cull.right, d),
                Cull::dot(cull.vertical, d),
                Cull::dot(cull.fwd, d),
                uvs[k].0 as i32,
                uvs[k].1 as i32,
                c.0 as i32,
                c.1 as i32,
                c.2 as i32,
            ]
        };
        // Two corner buffers the planes cut back and forth between: copying a
        // whole polygon per plane was a measurable share of the clip.
        // SAFETY: nothing else touches this scratch; `clip_piece` does not nest.
        // Kept off the frame so each call does not zero 792 bytes of it.
        let bufs = unsafe { &mut *core::ptr::addr_of_mut!(CLIP_BUFS) };
        // The packets' corner order is a zigzag; walk the quad's outline.
        (bufs[0][0], bufs[0][1], bufs[0][2], bufs[0][3]) =
            (corner(0), corner(1), corner(3), corner(2));
        // Most queued quads lie beside or behind the camera, out of the view
        // itself: if all four corners are past one of its planes, nothing of
        // the quad shows, and the clip below is the expensive part.
        let (half_w, half_h) = unsafe { (VIEW_HALF_W, VIEW_HALF_H) };
        let quad = &bufs[0][..4];
        let past =
            |test: fn(&Corner, i32, i32) -> bool| quad.iter().all(|c| test(c, half_w, half_h));
        if past(|c, _, _| c[5] <= NEAR)
            || past(|c, w, _| c[3] * H > w * c[5])
            || past(|c, w, _| -c[3] * H > w * c[5])
            || past(|c, _, h| c[4] * H > h * c[5])
            || past(|c, _, h| -c[4] * H > h * c[5])
        {
            return;
        }
        let (mut src, mut n) = (0, 4);
        for plane in 0..5 {
            // Signed distance outside the plane, in camera space, scaled by
            // the projection plane for the four guard-band sides.
            let outside = |c: &Corner| {
                let (x, y, z) = (c[3], c[4], c[5]);
                match plane {
                    0 => NEAR - z,
                    1 => x * H - GUARD_X * z,
                    2 => -x * H - GUARD_X * z,
                    3 => y * H - GUARD_Y * z,
                    _ => -y * H - GUARD_Y * z,
                }
            };
            let mut d = [0i32; MAX];
            let mut cut = false;
            for i in 0..n {
                d[i] = outside(&bufs[src][i]);
                cut |= d[i] > 0;
            }
            if !cut {
                continue;
            }
            let dst = src ^ 1;
            let mut m = 0;
            for i in 0..n {
                let j = if i + 1 == n { 0 } else { i + 1 };
                let (da, db) = (d[i], d[j]);
                if da <= 0 && m < MAX {
                    bufs[dst][m] = bufs[src][i];
                    m += 1;
                }
                if (da <= 0) != (db <= 0) && m < MAX {
                    // Where the edge crosses the plane, in Q12, with both
                    // terms shifted down together so the product fits.
                    let (mut num, mut den) = (da.abs(), (da - db).abs());
                    while den > 1 << 18 {
                        num >>= 1;
                        den >>= 1;
                    }
                    let t = (num << 12) / den.max(1);
                    let (a, b) = (bufs[src][i], bufs[src][j]);
                    for k in 0..11 {
                        bufs[dst][m][k] = a[k] + (((b[k] - a[k]) * t) >> 12);
                    }
                    m += 1;
                }
            }
            src = dst;
            n = m;
            if n < 3 {
                return;
            }
        }
        let poly = &bufs[src];
        let mut screen = [(0i16, 0i16, 0i32); MAX];
        for (s, c) in screen.iter_mut().zip(&poly[..n]) {
            let v = project(Vec3I16::new(c[0] as i16, c[1] as i16, c[2] as i16));
            if v.sz as i32 <= GTE_TRUE_SZ {
                return;
            }
            *s = (v.sx, v.sy, v.sz as i32);
        }
        let ch = |v: i32| v.clamp(0, 255) as u8;
        let uv = |c: &Corner| uvw(ch(c[6]), ch(c[7]));
        let tint = |c: &Corner| rgbc((ch(c[8]), ch(c[9]), ch(c[10])));
        for i in 1..n - 1 {
            let tri = [0, i, i + 1, i + 1];
            let sp = tri.map(|k| (screen[k].0, screen[k].1));
            if !gpu_draws_whole(&sp) || !quad_overlaps_view(&sp) {
                continue;
            }
            let uvs = tri.map(|k| uv(&poly[k]));
            let tints = tri.map(|k| tint(&poly[k]));
            // A wall piece is a backdrop and is filed at its far edge, like
            // the whole wall quads (see `wall_span`).
            let z_sum = if matches!(kind, Pieces::Wall { .. }) {
                4 * tri.iter().map(|&k| screen[k].2).max().unwrap_or(0)
            } else {
                tri.iter().map(|&k| screen[k].2).sum()
            };
            self.emit_piece(kind, sp, z_sum, uvs, tints);
        }
    }

    /// One triangle from [`Self::clip_piece`], as the
    /// packet the whole quad would have been.
    fn emit_piece(
        &mut self,
        kind: Pieces,
        sp: [(i16, i16); 4],
        z_sum: i32,
        uvs: [u16; 4],
        tints: [u32; 4],
    ) {
        match kind {
            Pieces::Floor => self.floor_quad(sp, uvs, tints),
            Pieces::Flat => self.emit(sp, z_sum / 4, tints.map(rgb_of)),
            Pieces::GoalFloor => self.emit_goal_floor(sp, tints.map(rgb_of)),
            Pieces::Net { packet } => self.quad_tex_words(sp, z_sum, uvs, tints, 0, packet, true),
            Pieces::Wall { packet, blended } => {
                self.quad_tex_words(sp, z_sum, uvs, tints, 0, packet, blended)
            }
        }
    }

    fn floor<const SPLIT: bool>(&mut self, cull: &Cull) {
        let step_x = sim::HALF_X * 2 / TILES_X;
        let step_z = sim::HALF_Z * 2 / TILES_Z;
        // Split screen draws the near-camera quads the GPU would drop; a full
        // view draws what it always did (see `near_sz`).
        let near = near_sz::<SPLIT>();
        // The pitch is flat, so a tile's box is its footprint with no height.
        // The chamfer only ever pulls corners inward, so this stays generous.
        let tile_e = cull.extents((step_x / 2, 0, step_z / 2));
        for ix in 0..TILES_X {
            for iz in 0..TILES_Z {
                let x0 = -sim::HALF_X + ix * step_x;
                let z0 = -sim::HALF_Z + iz * step_z;
                let (x1, z1) = (x0 + step_x, z0 + step_z);
                let (mx, mz) = ((x0 + x1) / 2, (z0 + z1) / 2);
                // The vertical test matters in a half-height view: the tiles
                // under and just ahead of the camera, the subdivided ones,
                // fall below its bottom edge.
                if !cull.visible_box((mx, 0, mz), tile_e) {
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
                // Most of the pitch is one-quad tiles, and the general path
                // below charges each of them the full grid machinery (a 5x5
                // Option array, closures, the conform check) to emit a single
                // quad. Take them straight through: four chamfered corners,
                // four projections, one emit. Pixel-identical, and an n == 1
                // tile never conforms, so nothing else changes.
                if n == 1 {
                    self.floor_tile_far::<SPLIT>(light, gx, gz);
                    continue;
                }

                // Sub-tiles carry a slice of the same UV rectangle, so the
                // texture keeps its scale and only the vertex count goes up.
                // `n` is a power of two (`floor_split`), and every operand
                // here is non-negative, so a shift is the same division
                // without the R3000's 36-cycle DIV on every grid point.
                let shift = n.trailing_zeros();
                let u = |i: i32| (GRASS_TILE_W * i >> shift).min(GRASS_TILE_W - 1) as u8;
                // Grid points between sub-tile corners, for `FLOOR_POS`.
                let grid_step = FLOOR_SPLIT_MAX as usize >> shift;

                // Project the tile's corner grid once. Every interior corner
                // is shared by four sub-quads, so projecting per quad ran the
                // GTE four times per corner; this runs it once. `None` is a
                // corner behind the near plane, and any quad touching one is
                // skipped exactly as `quad_tex`'s own per-corner check did.
                let nu = n as usize;
                let mut corners =
                    [[None; FLOOR_SPLIT_MAX as usize + 1]; FLOOR_SPLIT_MAX as usize + 1];
                for (sx, column) in corners.iter_mut().enumerate().take(nu + 1) {
                    for (sz, corner) in column.iter_mut().enumerate().take(nu + 1) {
                        let (cx, cz) = unsafe {
                            *FLOOR_POS
                                .get_unchecked(gx + sx * grid_step)
                                .get_unchecked(gz + sz * grid_step)
                        };
                        let p = project(Vec3I16::new(cx, 0, cz));
                        if p.sz as i32 > near {
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
                            rgb_of(*light.get_unchecked(gx + a).get_unchecked(gz + b))
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
                    let (ua, ub) = (u(sx as i32), u(sx as i32 + 1));
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
                        let quad = (
                            corners[sx][sz],
                            corners[sx + 1][sz],
                            corners[sx][sz + 1],
                            corners[sx + 1][sz + 1],
                        );
                        let whole = match quad {
                            (Some(a), Some(b), Some(c), Some(d)) => {
                                let sp = [(a.0, a.1), (b.0, b.1), (c.0, c.1), (d.0, d.1)];
                                if near == 0 || gpu_draws_whole(&sp) {
                                    if !sp.iter().any(|&(x, y)| on_view(x, y)) {
                                        continue;
                                    }
                                    Some(sp)
                                } else if quad_overlaps_view(&sp) {
                                    None
                                } else {
                                    continue;
                                }
                            }
                            (None, None, None, None) => continue,
                            _ if near != 0 => None,
                            _ => continue,
                        };
                        count_kept!();
                        let (va, vb) = (u(sz as i32), u(sz as i32 + 1));
                        let (j0, j1) = (gz + sz * stride, gz + (sz + 1) * stride);
                        let tints = unsafe {
                            [
                                *r0.get_unchecked(j0),
                                *r1.get_unchecked(j0),
                                *r0.get_unchecked(j1),
                                *r1.get_unchecked(j1),
                            ]
                        };
                        let Some(sp) = whole else {
                            // Too near the camera to draw whole: the same
                            // quad from its world corners, clipped.
                            let corner = (gx + sx * grid_step, gz + sz * grid_step);
                            Self::queue_floor_quad(corner, grid_step, [ua, ub, va, vb], tints);
                            continue;
                        };
                        let uvs = [uvw(ua, va), uvw(ub, va), uvw(ua, vb), uvw(ub, vb)];
                        self.floor_quad(sp, uvs, tints);
                    }
                }
            }
        }
    }

    /// The pitch markings, as strips of flat quads (see [`LINE_SECTIONS`]).
    /// Out of line, for the same i-cache reason as [`Self::conform_tile`],
    /// and kept lean: it runs once per cut.
    #[inline(never)]
    fn lines(&mut self, cull: &Cull) {
        // Cuts a strip steps over by distance: every cut near the camera,
        // then every second, fourth and eighth. Each quad still ends on a
        // cut, so a straight strip keeps one straight edge. Curves keep
        // every cut out to a couple of thousand uu, where a sixteen-gon
        // circle stops showing its corners, and never pass two: an
        // eight-gon shows them at any range.
        let (near, mid) = if split_view() { (400, 900) } else { (500, 1800) };
        let curve_near = if split_view() { 900 } else { 2200 };
        let view = unsafe {
            (VIEW_MIN_X as i32, VIEW_MAX_X as i32, VIEW_MIN_Y as i32, VIEW_MAX_Y as i32)
        };
        let sections = unsafe { &*core::ptr::addr_of!(LINE_SECTIONS) };
        // Far away a marking seen edge-on is under a pixel thick, and the
        // rasteriser then skips most of its columns: a solid line breaks
        // into dashes. So a cut is never closer than two pixels across.
        let cut = |s: &LineSection| {
            // Both ends through one RTPT (the third slot repeats the second):
            // the same per-vertex projection as two RTPS, one GTE wait.
            let vb = Vec3I16::new(s.b.0, 0, s.b.1);
            let [a, mut b, _] =
                scene::project_triangle_scheduled(Vec3I16::new(s.a.0, 0, s.a.1), vb, vb);
            let dy = b.sy - a.sy;
            if (b.sx - a.sx).abs() < 2 && dy.abs() < 2 {
                b.sy = a.sy + if dy < 0 { -2 } else { 2 };
            }
            (a, b)
        };
        for k in 0..unsafe { LINE_STRIP_COUNT } {
            let strip = unsafe { LINE_STRIPS[k] };
            let c = (strip.centre.0, 0, strip.centre.1);
            let h = (strip.half.0, 0, strip.half.1);
            if !cull.visible_box(c, cull.extents(h)) {
                continue;
            }
            let (mut i, last) = (strip.first as usize, strip.last as usize);
            let far_step = if strip.curved { 2 } else { 4 };
            let mut prev = cut(unsafe { sections.get_unchecked(i) });
            while i < last {
                let s = unsafe { sections.get_unchecked(i) };
                let d = cull.flat_distance(s.a.0 as i32, s.a.1 as i32);
                let step = if d < near || (strip.curved && d < curve_near) {
                    1
                } else if d < mid {
                    2
                } else if d < 3500 || strip.curved {
                    far_step
                } else {
                    8
                };
                let mut j = (i + step).min(last);
                let mut next = cut(unsafe { sections.get_unchecked(j) });
                // A long step that reaches behind the camera would drop the
                // whole quad, visible part and all: take one cut instead.
                if j > i + 1 && (next.0.sz == 0 || next.1.sz == 0) {
                    j = i + 1;
                    next = cut(unsafe { sections.get_unchecked(j) });
                }
                count_offered!();
                let (a, b, cc, dd) = (prev.0, prev.1, next.0, next.1);
                prev = next;
                i = j;
                if a.sz == 0 || b.sz == 0 || cc.sz == 0 || dd.sz == 0 {
                    continue;
                }
                if !corners_overlap(
                    [a.sx as i32, b.sx as i32, cc.sx as i32, dd.sx as i32],
                    [a.sy as i32, b.sy as i32, cc.sy as i32, dd.sy as i32],
                    view,
                ) {
                    continue;
                }
                count_kept!();
                // Flat where the light barely moves along the quad: a
                // Gouraud quad costs the GPU four times the setup and twice
                // the fill. Shaded where it does move, in the goal pools and
                // down a long far step, or the chalk steps visibly from quad
                // to quad.
                let sp = [(a.sx, a.sy), (b.sx, b.sy), (cc.sx, cc.sy), (dd.sx, dd.sy)];
                let (c0, c1) = (s.c, unsafe { sections.get_unchecked(j) }.c);
                let dif = |u: u8, v: u8| (u as i32 - v as i32).abs();
                if dif(c0.0, c1.0).max(dif(c0.1, c1.1)).max(dif(c0.2, c1.2)) > LINE_FLAT_SPREAD {
                    if let Some(quad) = self.arena.push(QuadGouraud::new(sp, [c0, c0, c1, c1])) {
                        self.ot.add_packet(LINE_SLOT, quad);
                    } else {
                        count_overflow!();
                    }
                } else if let Some(quad) = self.flats.push(QuadFlat::new(sp, c0.0, c0.1, c0.2)) {
                    self.ot.add_packet(LINE_SLOT, quad);
                } else {
                    count_overflow!();
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
        // Every corner is projected once and shared: the inner plate's
        // corners are halfway from the centre to the outer plate's on screen
        // (a 46-uu plate is too small for perspective to tell), and the two
        // orb diamonds share their tips.
        // The two pad sizes' cull boxes, (r, top / 2, r) below.
        let pad_e = [cull.extents((42, 50, 42)), cull.extents((62, 70, 62))];
        for (i, pad) in sim::PADS.iter().enumerate() {
            let r = if pad.big { 62 } else { 42 };
            let lift = if pad.big { 78 } else { 58 };
            // Orb and pool together: the orb tops out at `lift + r` and the
            // pool lies on the pitch, so the box runs the whole way down.
            let top = lift + r;
            if !cull.visible_box((pad.x, -top / 2, pad.z), pad_e[pad.big as usize]) {
                continue;
            }
            let far = cull.flat_distance(pad.x, pad.z);
            // Past 3000 in a half-width view the orb is a pixel or two;
            // nothing a player steers by survives at that size.
            if split_view() && far > 3000 {
                continue;
            }
            let live = s.pad_timers[i] == 0;
            let (bright, dim) = ((255, 214, 84), (196, 132, 30));
            let (px, pz) = (pad.x, pad.z);
            // The plate stays whether the pad is up or not: it is the thing that
            // says a pad belongs here, so the layout is learnable and a spent one
            // reads as spent rather than as absent. Two rings, the outer a dark
            // kerb and the inner lit only while there is something to collect.
            //
            // A half-width view keeps only the orb once a pad is distant: the
            // plate rings are a couple of pixels there, and the split kickoff
            // sees every pad on the pitch from both views at once.
            // Past 3500 in a full view the plates are a sliver a pixel tall.
            if far <= if split_view() { 2000 } else { 3500 } {
                let g = r * 3 / 4;
                // Five corners through two RTPTs (the second repeats its
                // last): the same per-vertex projection, fewer GTE round trips.
                let v = |x: i32, z: i32| Vec3I16::new(x as i16, -4, z as i16);
                let [c, o0, o1] = scene::project_triangle_scheduled(
                    v(px, pz),
                    v(px - g, pz),
                    v(px, pz - g),
                );
                let [o2, o3, _] = scene::project_triangle_scheduled(
                    v(px, pz + g),
                    v(px + g, pz),
                    v(px + g, pz),
                );
                let o = [o0, o1, o2, o3];
                if c.sz != 0 && o.iter().all(|p| p.sz != 0) {
                    let sp = [(o[0].sx, o[0].sy), (o[1].sx, o[1].sy), (o[2].sx, o[2].sy), (o[3].sx, o[3].sy)];
                    if quad_overlaps_view(&sp) {
                        let depth = o.iter().map(|p| p.sz as i32).sum::<i32>() / 4;
                        self.emit_flat(sp, depth + PAD_BIAS + 20, (52, 56, 70));
                        let half = |p: (i16, i16)| ((c.sx + p.0) >> 1, (c.sy + p.1) >> 1);
                        let inner = [half(sp[0]), half(sp[1]), half(sp[2]), half(sp[3])];
                        let lit = if live { dim } else { (34, 38, 50) };
                        self.emit_flat(inner, depth + PAD_BIAS + 10, lit);
                    }
                }
            }

            // The orb only exists while the pad does. It used to linger as a
            // ghost, which made a taken pad look collectable from any distance
            // where the colour was hard to judge.
            if !live {
                continue;
            }
            let mid = -lift;
            // Far away the orb is a few pixels of diamond whichever way it
            // is built, so it is one flat diamond facing the camera, in the
            // colour between the lit tips and the dim sides: a sixth of the
            // GPU time of two shaded ones.
            let sides: [(i32, i32); 2] = if far > if split_view() { 1200 } else { 2500 } {
                [(r * cull.right[0] as i32 >> 12, r * cull.right[2] as i32 >> 12), (0, 0)]
            } else {
                // Two diamonds in perpendicular vertical planes.
                [(r, 0), (0, r)]
            };
            let flat = sides[1] == (0, 0);
            // Both tips and the first diamond's left corner through one RTPT,
            // the remaining corners through a second (or RTPS for the flat
            // orb's one): the same per-vertex projection as one at a time.
            let w = |x: i32, y: i32, z: i32| Vec3I16::new(x as i16, y as i16, z as i16);
            let (a0, a1) = (sides[0], sides[1]);
            let [t, b, l0] = scene::project_triangle_scheduled(
                w(px, -(lift + r), pz),
                w(px, -(lift - r), pz),
                w(px - a0.0, mid, pz - a0.1),
            );
            if t.sz == 0 || b.sz == 0 {
                continue;
            }
            let (r0, l1, r1) = if flat {
                (project(w(px + a0.0, mid, pz + a0.1)), l0, l0)
            } else {
                let [r0, l1, r1] = scene::project_triangle_scheduled(
                    w(px + a0.0, mid, pz + a0.1),
                    w(px - a1.0, mid, pz - a1.1),
                    w(px + a1.0, mid, pz + a1.1),
                );
                (r0, l1, r1)
            };
            for (l, rr) in [(l0, r0), (l1, r1)].into_iter().take(if flat { 1 } else { 2 }) {
                if l.sz == 0 || rr.sz == 0 {
                    continue;
                }
                let sp = [(t.sx, t.sy), (l.sx, l.sy), (rr.sx, rr.sy), (b.sx, b.sy)];
                count_offered!();
                if !quad_overlaps_view(&sp) {
                    continue;
                }
                count_kept!();
                let depth = (t.sz as i32 + l.sz as i32 + rr.sz as i32 + b.sz as i32) / 4 + PAD_BIAS;
                if flat {
                    self.emit_flat(sp, depth, PAD_ORB_FAR);
                } else {
                    self.emit(sp, depth, [bright, dim, dim, bright]);
                }
            }
        }
    }

    /// The boost gauge: an arc that fills as the tank does, with the number in
    /// the middle. A flat row of pips reads as a debug bar; a dial reads as an
    /// instrument, and it is what the original uses.
    ///
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

    /// The goal celebration: the explosion between the posts, team coloured,
    /// for as long as the world is frozen after a goal.
    fn goal_burst(&mut self, s: &Sim) {
        if s.goal_freeze == 0 {
            return;
        }
        // Ticks since the goal, counting up from zero.
        let age = (sim::GOAL_FREEZE_TICKS - s.goal_freeze) as i32;
        let team = match s.last_scorer {
            sim::Team::Blue => seat_signal(0),
            sim::Team::Orange => seat_signal(1),
        };
        // On the goal line, not at the ball: the ball carries on into the
        // back of the net, and an explosion in there happens behind the mesh
        // where none of it can be seen.
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
    fn wall_span<const SPLIT: bool>(&mut self, si: usize, cull: &Cull) {
        let Span { a, b, .. } = unsafe { SPANS[si] };
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
        // See `near_sz`: split screen alone draws the near quads the GPU drops.
        let (split, near_z) = (SPLIT, near_sz::<SPLIT>());
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
            (true, d) if d < if split_view() { 1000 } else { 1400 } => 1,
            // A half-width view hands the two-sample curve back to four-sample
            // a thousand units sooner; past that, one band per curve: the
            // stride clamps at the arc endpoints, which keeps the rail and
            // material boundaries exactly where they were.
            (true, d) if d < if split_view() { 1800 } else { 3600 } => 2,
            (_, d) if d > 3600 && split_view() => 8,
            _ => 4,
        };
        let light = unsafe { &WALL_LIGHT[si] };
        // Where each column stands, the normal its profile sweeps along, its
        // light slot, and the panel and cover U it carries. Once per span at
        // boot (`build_spans`): per frame, the divides and the square root
        // were 25 DIVs a visible span.
        // Copied out whole: `build_view` runs on the scratchpad stack, so the
        // per-vertex reads below then cost a cycle, not a main-RAM load stall
        // each, which with a normal per column was most of a corner's cost.
        let SpanCols {
            count: columns,
            x: sx,
            z: sz,
            nx: cnx,
            nz: cnz,
            slot: slots,
            panel_u,
            cover_u: cover_us,
            ramp,
        } = unsafe { SPAN_COLS[si][splits as usize - 1] };
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
        // Two rings of the grid at a time: the lower edge of the band being
        // drawn and its upper edge, projected just before the band needs it.
        // Keeping every ring made the frame too big for the scratchpad stack
        // `build_view` runs this on. One place projects a ring, the top of
        // each pass (the first pass only primes `upper`), so the projection
        // stays inline and the GTE's latency stays hidden behind the loop.
        // Two ring buffers swapped by row parity: copying the upper ring
        // into the lower one every row was a measurable memcpy per span.
        let mut ring_buf: [[Option<(i16, i16, i32)>; SLOTS]; 2] = [[None; SLOTS]; 2];
        // A straight span sweeps every column along the same normal and the
        // same floor curve, so its inward offset is one pair of multiplies a
        // ring, not a pair a vertex. A corner span's columns turn, and round
        // the end joints their floor curve shrinks to the end walls'.
        let uniform = (1..columns)
            .all(|k| cnx[k] == cnx[0] && cnz[k] == cnz[0] && ramp[k] == ramp[0]);
        for row in 0..ring_count {
            let top = rings[row];
            let (lower, upper) = {
                let (a, b) = ring_buf.split_at_mut(1);
                if row & 1 == 0 { (&a[0], &mut b[0]) } else { (&b[0], &mut a[0]) }
            };
            let curve = top <= CURVE_SEGS;
            let p = profile[top];
            let p0 = if curve { ramp_point(p, ramp[0] as i32) } else { p };
            let shared = ((cnx[0] * p0.0) >> 12, (cnz[0] * p0.0) >> 12, p0.1);
            for k in 0..columns {
                // Each column sweeps the profile along its own normal: one
                // normal for a straight span, turning round a corner span's
                // rounded joints.
                let (x0, z0) = unsafe { (*sx.get_unchecked(k), *sz.get_unchecked(k)) };
                let (ox, oz, height) = if uniform {
                    shared
                } else {
                    let (nx, nz) = unsafe { (*cnx.get_unchecked(k), *cnz.get_unchecked(k)) };
                    let pk = if curve {
                        ramp_point(p, unsafe { *ramp.get_unchecked(k) } as i32)
                    } else {
                        p
                    };
                    ((nx * pk.0) >> 12, (nz * pk.0) >> 12, pk.1)
                };
                let v = project(Vec3I16::new((x0 + ox) as i16, -height as i16, (z0 + oz) as i16));
                upper[k] = if v.sz as i32 > near_z {
                    Some((v.sx, v.sy, v.sz as i32))
                } else {
                    None
                };
            }
            if row == 0 {
                continue;
            }
            let ri = rings[row - 1];
            // Unchecked for the same reason the floor is: the ring index
            // walks a window over a table sized from `PROFILE_LEN`.
            let (llo, lhi) = unsafe { (light.get_unchecked(ri), light.get_unchecked(top)) };
            for k in 0..columns - 1 {
                count_offered!();
                let whole = match (lower[k], lower[k + 1], upper[k], upper[k + 1]) {
                    (Some(a), Some(b), Some(c), Some(d)) => {
                        let sp = [(a.0, a.1), (b.0, b.1), (c.0, c.1), (d.0, d.1)];
                        // A nearby roof-curve band can cross the whole view
                        // while all four projected corners sit beyond its
                        // edges. Corner-only acceptance made that top section
                        // disappear during a wall climb even though the
                        // polygon covered visible pixels.
                        if !quad_overlaps_view(&sp) {
                            continue;
                        }
                        // Filed at its far edge, not its average depth: a wall is
                        // a backdrop, and a quad whose middle is nearer than a
                        // car on it drew over the car's far half (the car on a
                        // side ramp flickered in and out of existence).
                        (!split || gpu_draws_whole(&sp))
                            .then_some((sp, 4 * a.2.max(b.2).max(c.2).max(d.2)))
                    }
                    (None, None, None, None) => continue,
                    _ if split => None,
                    _ => continue,
                };
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
                let (s0, s1) = (slots[k] as usize, slots[k + 1] as usize);
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
                let packet = if covered { COVER_PACKET } else { WALL_PACKET };
                let Some((sp, z_sum)) = whole else {
                    // Too near the camera to draw whole: noted by span,
                    // columns and rings, for `draw_pieces` to sweep and clip.
                    Self::queue_wall_quad(
                        [si as u8, splits as u8 - 1, k as u8, ri as u8, top as u8],
                        [u0, u1, v0, v1],
                        tints,
                        covered,
                    );
                    continue;
                };
                self.quad_tex_words(sp, z_sum, uvs, tints, 0, packet, covered);
            }
        }
    }

    /// The apron between a crowd's foot line (`t0`..`t1`) and the same line at
    /// the ground (`f0`..`f1`), cut to the width of the view, which is all that
    /// shows of it and keeps the quad inside what the rasteriser takes whole.
    /// A straight line in the world is a straight line on the screen, so each
    /// of the two lines is cut where it crosses the view's sides.
    fn apron_span(&mut self, t0: (i16, i16), t1: (i16, i16), f0: (i16, i16), f1: (i16, i16)) {
        let (vx0, vx1) = unsafe { (VIEW_MIN_X as i32 - 64, VIEW_MAX_X as i32 + 64) };
        let inside = |x: i16| (vx0..=vx1).contains(&(x as i32));
        let (t0, t1, f0, f1) = if inside(t0.0) && inside(t1.0) && inside(f0.0) && inside(f1.0) {
            // Nothing to cut: all four ends are already within the view's width.
            (t0, t1, f0, f1)
        } else {
            let (Some((t0, t1)), Some((f0, f1))) =
                (clip_x(t0, t1, vx0, vx1), clip_x(f0, f1, vx0, vx1))
            else {
                return;
            };
            (t0, t1, f0, f1)
        };
        let sp = [t0, t1, f0, f1];
        if quad_overlaps_view(&sp) && gpu_draws_whole(&sp) {
            self.apron_quad(sp);
        }
    }

    fn apron_quad(&mut self, sp: [(i16, i16); 4]) {
        if let Some(q) = self
            .arena
            .push(QuadGouraud::new(sp, [APRON_TOP, APRON_TOP, APRON_BOTTOM, APRON_BOTTOM]))
        {
            self.ot.add_packet(STAND_SLOT, q);
        } else {
            count_overflow!();
        }
    }

    /// The stands behind the enclosure. A near piece is drawn from its full
    /// three-by-two grid, the way near wall spans are split, so a piece
    /// beside the camera keeps the part in front of the near plane and the
    /// crowd does not swim; a far one is a single quad on the grid corners.
    fn stands(&mut self, cull: &Cull) {
        // Front row on the tile's last row, the fascia; back on its first.
        const V: [u8; 3] = [CROWD_V0 + CROWD_H - 1, CROWD_V0 + (CROWD_H - 1) / 2, CROWD_V0];
        const TINT: [Rgb; 3] = [
            STAND_TINT_IN,
            (
                ((STAND_TINT_IN.0 as u16 + STAND_TINT_OUT.0 as u16) / 2) as u8,
                ((STAND_TINT_IN.1 as u16 + STAND_TINT_OUT.1 as u16) / 2) as u8,
                ((STAND_TINT_IN.2 as u16 + STAND_TINT_OUT.2 as u16) / 2) as u8,
            ),
            STAND_TINT_OUT,
        ];
        // The apron first, a whole edge at a time: the crowd's foot dropped to
        // the ground, so no sky shows under it. An edge that runs behind the
        // lens is cut where it crosses the depth the GTE projects true, so one
        // quad still covers what is in front.
        const NEAR: i32 = GTE_TRUE_SZ + 8;
        for edge in unsafe { APRON_EDGES.iter() } {
            if !cull.visible_box(edge.centre, cull.extents(edge.half)) {
                continue;
            }
            let mut pts = [edge.top[0], edge.top[1], edge.foot[0], edge.foot[1]];
            let depth = |p: Vec3I16| {
                Cull::dot(
                    cull.fwd,
                    (p.x as i32 - cull.pos.0, p.y as i32 - cull.pos.1, p.z as i32 - cull.pos.2),
                )
            };
            let d = [depth(pts[0]), depth(pts[1]), depth(pts[2]), depth(pts[3])];
            let (end0, end1) = (d[0].min(d[2]) <= NEAR, d[1].min(d[3]) <= NEAR);
            if end0 && end1 {
                continue;
            }
            // How far along a line, from its end `a` (depth `da`) to its end `b`
            // (depth `db`), the depth reaches NEAR; Q12.
            let cut = |da: i32, db: i32| ((NEAR - da) * 4096 / (db - da).max(1)).clamp(0, 4096);
            let lerp = |a: Vec3I16, b: Vec3I16, t: i32| {
                Vec3I16::new(
                    (a.x as i32 + (((b.x as i32 - a.x as i32) * t) >> 12)) as i16,
                    (a.y as i32 + (((b.y as i32 - a.y as i32) * t) >> 12)) as i16,
                    (a.z as i32 + (((b.z as i32 - a.z as i32) * t) >> 12)) as i16,
                )
            };
            if end0 {
                let t = cut(d[0], d[1]).max(cut(d[2], d[3]));
                (pts[0], pts[2]) = (lerp(pts[0], pts[1], t), lerp(pts[2], pts[3], t));
            } else if end1 {
                let t = cut(d[1], d[0]).max(cut(d[3], d[2]));
                (pts[1], pts[3]) = (lerp(pts[1], pts[0], t), lerp(pts[3], pts[2], t));
            }
            let t = scene::project_triangle_scheduled(pts[0], pts[1], pts[1]);
            let f = scene::project_triangle_scheduled(pts[2], pts[3], pts[3]);
            if t[0].sz == 0 || t[1].sz == 0 || f[0].sz == 0 || f[1].sz == 0 {
                continue;
            }
            self.apron_span(
                (t[0].sx, t[0].sy),
                (t[1].sx, t[1].sy),
                (f[0].sx, f[0].sy),
                (f[1].sx, f[1].sy),
            );
        }
        for st in unsafe { STANDS.iter() } {
            if !cull.visible_box(st.centre, cull.extents(st.half)) {
                continue;
            }
            let near = Self::floor_split(cull.flat_distance(st.centre.0, st.centre.2)) > 2;
            let mut apron_whole = false;
            let (cols, rows): (&[usize], &[usize]) =
                if near { (&[0, 1, 2, 3], &[0, 1, 2]) } else { (&[0, 3], &[0, 2]) };
            let packet = CROWD_PACKETS[st.team as usize];
            let mut g = [[None; 4]; 3];
            // A row's first three columns through one RTPT, any fourth through
            // RTPS: the same per-vertex projection, fewer GTE round trips.
            for &j in rows {
                let at = |n: usize| st.grid[j][cols[n.min(cols.len() - 1)]];
                let t = scene::project_triangle_scheduled(at(0), at(1), at(2));
                for (n, v) in t.iter().enumerate().take(cols.len()) {
                    if v.sz != 0 {
                        g[j][cols[n]] = Some((v.sx, v.sy));
                    }
                }
                if cols.len() > 3 {
                    let v = project(at(3));
                    if v.sz != 0 {
                        g[j][cols[3]] = Some((v.sx, v.sy));
                    }
                }
            }
            // The apron: the front edge dropped to the ground. The edge is a
            // straight line, so one quad covers the piece unless it is too wide
            // for the rasteriser to take whole (a piece beside the camera), and
            // then it goes in the crowd's own columns.
            let last = cols[cols.len() - 1];
            let spans: &[(usize, usize)] = if cols.len() > 2 {
                &[(0, last), (0, 1), (1, 2), (2, 3)]
            } else {
                &[(0, last)]
            };
            for (n, &(i0, i1)) in spans.iter().enumerate() {
                if n == 1 && apron_whole {
                    break;
                }
                let (Some(t0), Some(t1)) = (g[0][i0], g[0][i1]) else {
                    continue;
                };
                let (a, b) = (st.grid[0][i0], st.grid[0][i1]);
                let (b0, b1) = (
                    project(Vec3I16::new(a.x, 0, a.z)),
                    project(Vec3I16::new(b.x, 0, b.z)),
                );
                if b0.sz == 0 || b1.sz == 0 {
                    continue;
                }
                let sp = [t0, t1, (b0.sx, b0.sy), (b1.sx, b1.sy)];
                if !quad_overlaps_view(&sp) {
                    continue;
                }
                if n == 0 && cols.len() > 2 {
                    apron_whole = gpu_draws_whole(&sp);
                    if !apron_whole {
                        continue;
                    }
                } else if !gpu_draws_whole(&sp) {
                    continue;
                }
                if let Some(q) = self
                    .arena
                    .push(QuadGouraud::new(sp, [APRON_TOP, APRON_TOP, APRON_BOTTOM, APRON_BOTTOM]))
                {
                    self.ot.add_packet(STAND_SLOT, q);
                } else {
                    count_overflow!();
                }
            }
            for r in rows.windows(2) {
                let (j0, j1) = (r[0], r[1]);
                for c in cols.windows(2) {
                    let (i0, i1) = (c[0], c[1]);
                    let (Some(p0), Some(p1), Some(p2), Some(p3)) =
                        (g[j0][i0], g[j0][i1], g[j1][i0], g[j1][i1])
                    else {
                        continue;
                    };
                    let sp = [p0, p1, p2, p3];
                    if !quad_overlaps_view(&sp) {
                        continue;
                    }
                    let (u0, u1) = (st.u[i0], st.u[i1]);
                    let prim = QuadTexturedGouraud::with_packet_material_packed_uv_words(
                        sp,
                        [uvw(u0, V[j0]), uvw(u1, V[j0]), uvw(u0, V[j1]), uvw(u1, V[j1])],
                        [TINT[j0], TINT[j0], TINT[j1], TINT[j1]],
                        packet,
                    );
                    if let Some(q) = self.textured.push(prim) {
                        self.ot.add_packet(STAND_SLOT, q);
                    } else {
                        count_overflow!();
                    }
                }
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
            if split_view() {
                self.wall_span::<true>(si, cull);
            } else {
                self.wall_span::<false>(si, cull);
            }
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
            // Behind the view or off to a side of it, the cover's every band
            // would be thrown away after its corners were projected.
            let (reach_lo, reach_hi, reach_top) = unsafe { GOAL_COVER_REACH };
            let cover_box =
                cull.extents((gw, (reach_top - sim::GOAL_H) / 2 + 1, (reach_hi - reach_lo) / 2 + 1));
            if !cull.visible_box(
                (0, -(sim::GOAL_H + reach_top) / 2, z - sz * (reach_lo + reach_hi) / 2),
                cover_box,
            ) {
                continue;
            }
            let profile = unsafe { &WALL_PROFILE };
            let profile_v = unsafe { &COVER_PROFILE_V };
            // `build_spans` appends the two end-wall runs for -Z, then the
            // matching pair for +Z. Borrow their goalpost vertices verbatim:
            // matching colours at the shared edge removes the last vertical
            // lighting seam without another per-frame light calculation.
            let left_light = unsafe { &WALL_LIGHT[SPAN_COUNT - 4 + i * 2] };
            let right_light = unsafe { &WALL_LIGHT[SPAN_COUNT - 3 + i * 2] };
            let ring_light = |ri: usize| [rgb_of(left_light[ri][4]), rgb_of(right_light[ri][0])];
            let across = cover_texels(2 * gw);
            // Phase the honeycomb from the left end-wall span. Its eight-texel
            // horizontal period then reaches the right span without a doubled
            // strand at either goalpost.
            let phase = cover_texels(END_WALL_X - gw) as i32 % HEX_W;
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
        if !cull.visible(roof_box.0, roof_box.1) || !cull.visible_vertically(roof_box.0, roof_box.1)
        {
            return;
        }
        // One quad stretched a 32-pixel wall tile over the whole 7,672 by
        // 9,720-uu roof. Patch it at exact texture-repeat distances instead:
        // every roof cell now has the same dimensions as one on the wall, and
        // the 128x84 atlas periods meet without a doubled strand. Corner
        // light comes from the boot-time table; each row of patches is culled
        // on its own box, so a camera looking along the pitch projects the
        // few rows in front of it and not the roof.
        let y = -sim::CEIL;
        let lights = unsafe { &ROOF_CORNER_LIGHT };
        // Each patch corner is shared by up to four patches. Seen from below
        // the whole roof is in view, ninety-six patches, and projecting their
        // corners per patch ran the GTE 384 times for 119 distinct points;
        // the full-length roof view was the costliest view on Manny's tape.
        // The box test per patch was the other half of that bill, so the
        // roof is now culled a row at a time: a visible row projects its
        // corner line once, the line above carried over to the next row, and
        // each patch then only needs its projected corners on screen. A patch
        // the old per-patch box test rejected lies past a side of the view,
        // so its corners land past the same edge and the screen test rejects
        // it too: the same patches, projections and sums, so the same pixels.
        let corner_line = |z: i32| {
            let mut line = [None; ROOF_COLS + 1];
            for (ix, corner) in line.iter_mut().enumerate() {
                let p = project(Vec3I16::new(roof_corner_x(ix) as i16, y as i16, z as i16));
                if p.sz != 0 {
                    *corner = Some((p.sx, p.sy, p.sz as i32));
                }
            }
            line
        };
        let mut lo: Option<[Option<(i16, i16, i32)>; ROOF_COLS + 1]> = None;
        for iz in 0..ROOF_ROWS {
            let (z0, z1) = (roof_corner_z(iz), roof_corner_z(iz + 1));
            if !cull.visible((0, y, (z0 + z1) / 2), (ROOF_HALF_X, 0, (z1 - z0) / 2)) {
                lo = None;
                continue;
            }
            let below = match lo {
                Some(line) => line,
                None => corner_line(z0),
            };
            let above = corner_line(z1);
            let h = ((z1 - z0 + COVER_UU_PER_TEXEL - 1) / COVER_UU_PER_TEXEL).clamp(1, ROOF_PATCH_V)
                as u8;
            for ix in 0..ROOF_COLS {
                count_offered!();
                let (Some((ax, ay, az)), Some((bx, by, bz)), Some((cx, cy, cz)), Some((dx, dy, dz))) =
                    (below[ix], below[ix + 1], above[ix], above[ix + 1])
                else {
                    continue;
                };
                let sp = [(ax, ay), (bx, by), (cx, cy), (dx, dy)];
                if !quad_overlaps_view(&sp) {
                    continue;
                }
                count_kept!();
                let w = ROOF_PATCH_W[ix];
                self.quad_tex_words(
                    sp,
                    az + bz + cz + dz,
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
            lo = Some(above);
        }
    }

    /// Where a quad's corners are against the depth the GTE stops projecting
    /// true (see [`GTE_TRUE_SZ`]): every corner past it, none, or some. A
    /// quad with some is clipped rather than projected: [`project_quad`]
    /// drops a quad with a corner behind the eye, and one with a corner that
    /// close lands off its true place. One wholly behind it shows nothing.
    #[inline]
    fn eye_reach(cull: &Cull, corners: &[(i32, i32, i32); 4]) -> EyeReach {
        let mut inside = 0;
        for c in corners {
            let d = (c.0 - cull.pos.0, c.1 - cull.pos.1, c.2 - cull.pos.2);
            inside += (Cull::dot(cull.fwd, d) <= GTE_TRUE_SZ + 8) as u8;
        }
        match inside {
            0 => EyeReach::Clear,
            4 => EyeReach::Behind,
            _ => EyeReach::Cuts,
        }
    }

    /// One quad's [`EyeReach`] given its goal's: only a goal that may reach the
    /// eye is tested corner by corner, and one wholly behind it is skipped.
    #[inline]
    fn quad_reach(cull: &Cull, reach: GoalReach, corners: &[(i32, i32, i32); 4]) -> EyeReach {
        match reach {
            GoalReach::Clear => EyeReach::Clear,
            GoalReach::Near => Self::eye_reach(cull, corners),
            GoalReach::Behind => EyeReach::Behind,
        }
    }

    /// Queue a goal-box quad for [`Builder::flush_goal_jobs`], which clips it
    /// once the phase is off the scratchpad stack (the clip needs more frame
    /// than the stack has). A quad past the queue's end is dropped, as every
    /// such quad used to be.
    #[inline(never)]
    #[cold]
    fn queue_goal_job(
        world: [(i32, i32, i32); 4],
        uvs: [(u8, u8); 4],
        tints: [u32; 4],
        kind: Pieces,
    ) {
        unsafe {
            let n = GOAL_JOB_COUNT;
            if n < MAX_GOAL_JOBS {
                GOAL_JOBS[n] = PieceJob {
                    world,
                    uvs,
                    tints,
                    kind,
                };
                GOAL_JOB_COUNT = n + 1;
            }
        }
    }

    /// Clip and draw what [`Builder::queue_goal_job`] collected.
    #[inline(never)]
    fn flush_goal_jobs(&mut self, cull: &Cull) {
        for k in 0..unsafe { GOAL_JOB_COUNT } {
            let job = unsafe { GOAL_JOBS[k] };
            self.clip_piece(job.world, job.uvs, job.tints, job.kind, cull);
        }
        unsafe { GOAL_JOB_COUNT = 0 };
    }

    /// [`Builder::quad`] for the goal box, whose faces are big enough for the
    /// camera to stand inside: a driver in the mouth of the goal has the near
    /// corners of the floor and the sides behind the lens, and the whole
    /// face used to go with them (the pitch under the car vanished). `near`
    /// is whether any of this goal's box can reach the eye (see `goals`), so a
    /// goal across the pitch pays for no test at all.
    fn goal_quad(
        &mut self,
        cull: &Cull,
        reach: GoalReach,
        corners: [(i32, i32, i32); 4],
        colors: [Rgb; 4],
    ) {
        match Self::quad_reach(cull, reach, &corners) {
            EyeReach::Clear => self.quad(corners, colors),
            EyeReach::Cuts => {
                Self::queue_goal_job(corners, [(0, 0); 4], colors.map(rgbc), Pieces::Flat)
            }
            EyeReach::Behind => {}
        }
    }

    fn goal_quad_flat(
        &mut self,
        cull: &Cull,
        reach: GoalReach,
        corners: [(i32, i32, i32); 4],
        color: Rgb,
    ) {
        self.goal_quad(cull, reach, corners, [color; 4]);
    }

    /// The goal's floor, like the pitch's, is not depth-sorted: it lies under
    /// everything that stands in the goal, and a floor sorted by its average
    /// depth drew over the lower half of a car parked on it.
    fn goal_floor(
        &mut self,
        cull: &Cull,
        reach: GoalReach,
        corners: [(i32, i32, i32); 4],
        colors: [Rgb; 4],
    ) {
        match Self::quad_reach(cull, reach, &corners) {
            EyeReach::Clear => {
                if let Some((sp, _)) = project_quad(&corners) {
                    if quad_overlaps_view(&sp) {
                        self.emit_goal_floor(sp, colors);
                    }
                }
            }
            EyeReach::Cuts => {
                Self::queue_goal_job(corners, [(0, 0); 4], colors.map(rgbc), Pieces::GoalFloor)
            }
            EyeReach::Behind => {}
        }
    }

    fn emit_goal_floor(&mut self, sp: [(i16, i16); 4], colors: [Rgb; 4]) {
        if let Some(q) = self.arena.push(QuadGouraud::new(sp, colors)) {
            self.ot.add_packet(FLOOR_SLOT, q);
        } else {
            count_overflow!();
        }
    }

    /// [`Builder::quad_tex`] for the goal's netting, for the same reason.
    #[allow(clippy::too_many_arguments)]
    fn goal_quad_tex(
        &mut self,
        cull: &Cull,
        reach: GoalReach,
        corners: [(i32, i32, i32); 4],
        uvs: [u16; 4],
        tints: [Rgb; 4],
        bias: i32,
        packet: TexturedGouraudPacketMaterial,
        blended: bool,
    ) {
        match Self::quad_reach(cull, reach, &corners) {
            EyeReach::Clear => self.quad_tex(corners, uvs, tints, bias, packet, blended),
            EyeReach::Cuts => Self::queue_goal_job(
                corners,
                uvs.map(|w| (w as u8, (w >> 8) as u8)),
                tints.map(rgbc),
                Pieces::Net { packet },
            ),
            EyeReach::Behind => {}
        }
    }

    /// Where one goal's whole box is against the eye and the view.
    #[inline(never)]
    fn goal_reach(cull: &Cull, z_line: i32, back: i32) -> GoalReach {
        let c = (0, -sim::GOAL_H / 2, (z_line + back) / 2);
        let e = cull.extents((sim::GOAL_HALF_W, sim::GOAL_H / 2, sim::GOAL_DEPTH / 2));
        // Wholly behind the eye, or off to a side or above or below the view:
        // nothing of the box can show.
        if !cull.visible_box(c, e) {
            return GoalReach::Behind;
        }
        let d = (c.0 - cull.pos.0, c.1 - cull.pos.1, c.2 - cull.pos.2);
        if Cull::dot(cull.fwd, d) - e[0] <= GTE_TRUE_SZ + 8 {
            GoalReach::Near
        } else {
            GoalReach::Clear
        }
    }

    fn goals(&mut self, view: &View) {
        let cull = view.cull();
        // The far goal (+Z) is the one you shoot at, so it wears the
        // opponent's colour; your own net behind you is blue.
        for (z_line, color) in [
            (sim::HALF_Z, seat_signal(1)),
            (-sim::HALF_Z, seat_signal(0)),
        ] {
            let back = z_line + sim::GOAL_DEPTH * z_line.signum();
            // Can any of this goal's box reach the eye? Its nearest point is no
            // nearer than the box's centre less its reach along the view.
            let reach = Self::goal_reach(&cull, z_line, back);
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
            // A box wholly behind the eye shows nothing: not even its colours are
            // worked out.
            'boxed: {
                if reach == GoalReach::Behind {
                    break 'boxed;
                }
                // The box behind the net, lit in the team's colour from inside.
                // It used to be near black: the team colour lived only on the
                // frame, and at the far end of the pitch the goal was the
                // darkest thing on screen. Rocket League's goal is the brightest
                // thing at its end of the arena. A fraction of orange is brown,
                // so the box is never less than a third of the signal colour and
                // the white net and the hot frame are in front of it.
                let glow = |n: i32| shade(color, n, 16);
                self.goal_quad(
                    &cull,
                    reach,
                    [
                        (-gw, 0, back),
                        (gw, 0, back),
                        (-gw, gh, back),
                        (gw, gh, back),
                    ],
                    [
                        glow(GOAL_BACK_LO),
                        glow(GOAL_BACK_LO),
                        glow(GOAL_BACK_HI),
                        glow(GOAL_BACK_HI),
                    ],
                );
                for &sx in &[-1i32, 1] {
                    let x = sx * gw;
                    self.goal_quad(
                        &cull,
                        reach,
                        [(x, 0, z_line), (x, 0, back), (x, gh, z_line), (x, gh, back)],
                        [
                            glow(GOAL_SIDE - 2),
                            glow(GOAL_SIDE),
                            glow(GOAL_BACK_HI - 2),
                            glow(GOAL_BACK_HI),
                        ],
                    );
                }
                self.goal_quad_flat(
                    &cull,
                    reach,
                    [
                        (-gw, gh, z_line),
                        (gw, gh, z_line),
                        (-gw, gh, back),
                        (gw, gh, back),
                    ],
                    glow(GOAL_BACK_HI - 1),
                );
                self.goal_floor(
                    &cull,
                    reach,
                    [
                        (-gw, 0, z_line),
                        (gw, 0, z_line),
                        (-gw, 0, back),
                        (gw, 0, back),
                    ],
                    [
                        glow(GOAL_FLOOR - 2),
                        glow(GOAL_FLOOR - 2),
                        glow(GOAL_FLOOR),
                        glow(GOAL_FLOOR),
                    ],
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
                self.goal_quad_tex(
                    &cull,
                    reach,
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
                    self.goal_quad_tex(
                        &cull,
                        reach,
                        [(x, 0, z_line), (x, 0, far), (x, nh, z_line), (x, nh, far)],
                        patch(deep, tall),
                        [net_lo, net_lo, net_hi, net_hi],
                        0,
                        COVER_PACKET,
                        true,
                    );
                }
                // Roof.
                self.goal_quad_tex(
                    &cull,
                    reach,
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

            }

            // Hot posts and crossbar, each inside a halo of the team's
            // light, and a glowing strip along the goal line.
            let frame = mix(color, GOAL_FRAME_HOT, GOAL_FRAME_MIX);
            // Half again the team hue: past 128 the strongest channel starts
            // to clip, which on a black sky reads as the light burning hot.
            let halo = {
                let g = unsafe { SEAT_GLOW[if z_line > 0 { 1 } else { 0 }] };
                let k = |c: u8| (c as i32 * 3 / 2).min(255) as u8;
                (k(g.0), k(g.1), k(g.2))
            };
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
                    frame,
                );
                let h = GOAL_POST_HALO;
                self.glow_quad(
                    [
                        (x - h, gh - h, z_line),
                        (x + h, gh - h, z_line),
                        (x - h, h / 2, z_line),
                        (x + h, h / 2, z_line),
                    ],
                    halo,
                    -40,
                    GLOW_PACKET,
                );
            }
            self.quad_flat(
                [
                    (-gw, gh + post, z_line),
                    (gw, gh + post, z_line),
                    (-gw, gh - post, z_line),
                    (gw, gh - post, z_line),
                ],
                frame,
            );
            let h = GOAL_BAR_HALO;
            self.glow_quad(
                [
                    (-gw - h, gh - h, z_line),
                    (gw + h, gh - h, z_line),
                    (-gw - h, gh + h, z_line),
                    (gw + h, gh + h, z_line),
                ],
                halo,
                -40,
                GLOW_PACKET,
            );
            let (zl, h) = (z_line.signum(), GOAL_LINE_HALO);
            self.glow_quad(
                [
                    (-gw, -2, z_line - zl * h),
                    (gw, -2, z_line - zl * h),
                    (-gw, -2, z_line + zl * h),
                    (gw, -2, z_line + zl * h),
                ],
                halo,
                PAD_BIAS,
                GLOW_PACKET,
            );
        }
    }

    // ---- actors --------------------------------------------------------

    /// Rocket League's ball indicator: a hoop on the pitch under an airborne
    /// ball, with a disc inside it that grows to fill it as the ball comes
    /// down. The shadow says where the ball is; this says when it lands.
    /// Additive, and only while the ball is up.
    fn ball_ring(&mut self, s: &Sim, cull: &Cull) {
        let h = r(s.ball.p.y) - sim::BALL_R;
        if h < BALL_RING_MIN_H {
            return;
        }
        let (x, z) = (r(s.ball.p.x), r(s.ball.p.z));
        // Over the pitch only: past the foot of the ramp the floor curves
        // up and a flat hoop would cut through it. The end walls' curve is
        // smaller (`sim::END_RAMP_R`), but the hoop reaches 188 uu past the
        // ball: with the side walls' margin there too its edge stays within
        // a few uu of the end ramp's surface, where the end ramp's own
        // margin ran it through the wall.
        if x.abs() > sim::HALF_X - RAMP_R || z.abs() > sim::HALF_Z - RAMP_R {
            return;
        }
        // The hoop is geometry, the way the pitch markings are: a ring of
        // segments, each two additive quads across the band, dark at the
        // inner and outer edge and bright at the middle. Gouraud does the
        // falloff, and every quad samples the glow tile's one bright centre
        // texel, so there is no texture to map affinely and no texel steps:
        // the edge stays round and smooth at any range. The 32x32 hoop
        // texture it replaces stepped in blocks of several pixels near the
        // camera. Fewer segments with distance.
        let d = cull.flat_distance(x, z);
        // Sixteen near: a chord then strays under 4 uu from the circle, and
        // the soft edges hide that. Twenty cost two dropped frames on the
        // train tape's heaviest stretch (polls 833..834), where the ball is
        // overhead and the hoop is under the camera.
        let segs: usize = if d < 900 {
            16
        } else if d < 3500 {
            10
        } else {
            8
        };
        // One column of the ring at segment `k`: the inner and outer radius
        // projected, and the bright middle halfway between them on screen.
        // The band is 52 uu across, too narrow for perspective to tell the
        // midpoint from the projected middle radius, and it saves a third of
        // the projections. Only the first, the previous and the next column
        // are kept, which keeps the frame small enough for the scratchpad
        // stack.
        let column = |k: usize| {
            let a = (4096 * k / segs) as u16;
            let (sn, cs) = (sin_q12(a), cos_q12(a));
            let at = |rad: i32| {
                let p = project(Vec3I16::new(
                    (x + (rad * sn >> 12)) as i16,
                    -3,
                    (z + (rad * cs >> 12)) as i16,
                ));
                (p.sz != 0).then_some((p.sx, p.sy, p.sz as i32))
            };
            let (inner, outer) = (at(BALL_HOOP_IN), at(BALL_HOOP_OUT));
            let mid = match (inner, outer) {
                (Some(i), Some(o)) => Some(((i.0 + o.0) >> 1, (i.1 + o.1) >> 1, (i.2 + o.2) >> 1)),
                _ => None,
            };
            [inner, mid, outer]
        };
        const C: u8 = GLOW_W / 2;
        let uvs = [uvw(GLOW_U0 + C, GLOW_V0 + C); 4];
        let (dark, peak) = ((0, 0, 0), BALL_RING_TINT);
        let first = column(0);
        let mut prev = first;
        for k in 0..segs {
            let next = if k + 1 == segs { first } else { column(k + 1) };
            for band in 0..2 {
                count_offered!();
                let (Some(a), Some(bb), Some(c), Some(dd)) =
                    (prev[band], next[band], prev[band + 1], next[band + 1])
                else {
                    continue;
                };
                let sp = [(a.0, a.1), (bb.0, bb.1), (c.0, c.1), (dd.0, dd.1)];
                if !quad_overlaps_view(&sp) {
                    continue;
                }
                count_kept!();
                let tints = if band == 0 {
                    [dark, dark, peak, peak]
                } else {
                    [peak, peak, dark, dark]
                };
                let depth = (a.2 + bb.2 + c.2 + dd.2) / 4 + BALL_RING_BIAS;
                self.emit_glow(sp, depth, uvs, tints, GLOW_PACKET);
            }
            prev = next;
        }
        // The disc is the landing: a spot at the top of the flight, the
        // whole hoop on touchdown. A soft blob with no edge to bend, so one
        // quad over the whole glow tile.
        let fill = (4096 - h * 4096 / BALL_RING_FULL_H).clamp(600, 4096);
        let rad = BALL_RING_R * fill >> 12;
        self.glow_quad(
            [
                (x - rad, -3, z - rad),
                (x + rad, -3, z - rad),
                (x - rad, -3, z + rad),
                (x + rad, -3, z + rad),
            ],
            BALL_DISC_TINT,
            BALL_RING_BIAS,
            GLOW_PACKET,
        );
    }

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
        let rim = |side: usize| {
            let side = side.min(SHADOW_SIDES - 1);
            let a = ((4096 * side) / SHADOW_SIDES) as u16;
            let (ox, oz) = (
                (ex * cos_q12(a) as i32) >> 12,
                (ez * sin_q12(a) as i32) >> 12,
            );
            let px = x + ((ox * yc + oz * ys) >> 12);
            let pz = z + ((oz * yc - ox * ys) >> 12);
            Vec3I16::new(px as i16, -2, pz as i16)
        };
        // Three rim points to an RTPT (the last triple repeats the final
        // point): the same per-vertex projection as one RTPS each.
        let mut side = 0;
        while side < SHADOW_SIDES {
            let t = scene::project_triangle_scheduled(rim(side), rim(side + 1), rim(side + 2));
            for (j, v) in t.iter().enumerate() {
                if side + j >= SHADOW_SIDES {
                    break;
                }
                if v.sz == 0 {
                    return;
                }
                sp[side + j] = (v.sx, v.sy);
                z_sum += v.sz as i32;
                if on_view(v.sx, v.sy) {
                    on_screen = true;
                }
            }
            side += 3;
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
        let far = lod_far(
            unsafe { &mut BALL_FAR_LOD },
            view.camera_space(ball_pos).2,
            BALL_LOD_ENTER_DEPTH,
            BALL_LOD_EXIT_DEPTH,
        );
        let lon_step = if split_view() || far { 2 } else { 1 };
        let mesh = unsafe { &BALL_MESH };
        // Two latitude rows at a time, the band's top and bottom edge, so the
        // frame fits the scratchpad stack `build_view` runs this on.
        let mut sp = [[(0i16, 0i16); BALL_LON]; 2];
        let mut sz = [[0i32; BALL_LON]; 2];
        let mut tint = [[0u32; BALL_LON]; 2];
        // The eye direction and the light in the ball's own frame, once (the
        // rotation's transpose is its inverse), so the cull below is one dot
        // product against a facet's object-space normal and a vertex's light
        // is one against its own position. Rotating every facet's normal into
        // the world first cost nine multiplies a facet, and half the facets
        // are then thrown away.
        let wm = &world.m;
        let to_local = |v: (i32, i32, i32)| {
            (
                (wm[0][0] as i32 * v.0 + wm[1][0] as i32 * v.1 + wm[2][0] as i32 * v.2) >> 12,
                (wm[0][1] as i32 * v.0 + wm[1][1] as i32 * v.1 + wm[2][1] as i32 * v.2) >> 12,
                (wm[0][2] as i32 * v.0 + wm[1][2] as i32 * v.1 + wm[2][2] as i32 * v.2) >> 12,
            )
        };
        let to_cam_local = to_local(to_cam);
        let light_local = to_local(BALL_LIGHT);
        // Lit per vertex, so the sphere shades smoothly across its facets
        // instead of in flat steps, and the light's falloff is the same one
        // the flat facets used. The tint runs past 128 (the texture's own
        // brightness) on the lit side: the texture is mid-grey so that the
        // panels keep their contrast in shadow.
        let vertex_tint = |v: (i32, i32, i32)| {
            let k = 4096 / sim::BALL_R.max(1);
            let dot = ((v.0 * light_local.0 + v.1 * light_local.1 + v.2 * light_local.2) * k) >> 12;
            let lit = (2500 + dot / 2).clamp(1100, 4096);
            let t = (lit * BALL_TINT_LIT >> 12) as u8;
            rgbc((t, t, t))
        };
        let project_row = |j: usize,
                           sp: &mut [(i16, i16); BALL_LON],
                           sz: &mut [i32; BALL_LON],
                           tint: &mut [u32; BALL_LON]| {
            // The first and last rows are the poles: sixteen copies of one
            // point. Project it once.
            if j == 0 || j == BALL_LAT {
                let v = mesh[j][0];
                let p = project(Vec3I16::new(v.0 as i16, v.1 as i16, v.2 as i16));
                *sp = [(p.sx, p.sy); BALL_LON];
                *sz = [p.sz as i32; BALL_LON];
                *tint = [vertex_tint(v); BALL_LON];
                return;
            }
            // Project only the columns the quad loop below reads: a split
            // view was projecting all sixteen and then drawing every other
            // one, throwing half the GTE work away.
            // Three columns to an RTPT: the same per-vertex projection, a
            // third of the GTE round trips. Both column counts (16 and 8)
            // leave one column for RTPS.
            let at = |i: usize| {
                let v = mesh[j][i];
                Vec3I16::new(v.0 as i16, v.1 as i16, v.2 as i16)
            };
            let mut i = 0;
            while i < BALL_LON {
                if i + 2 * lon_step < BALL_LON {
                    let t = scene::project_triangle_scheduled(
                        at(i),
                        at(i + lon_step),
                        at(i + 2 * lon_step),
                    );
                    for (n, p) in t.iter().enumerate() {
                        let c = i + n * lon_step;
                        sp[c] = (p.sx, p.sy);
                        sz[c] = p.sz as i32;
                        tint[c] = vertex_tint(mesh[j][c]);
                    }
                    i += 3 * lon_step;
                } else {
                    let p = project(at(i));
                    sp[i] = (p.sx, p.sy);
                    sz[i] = p.sz as i32;
                    tint[i] = vertex_tint(mesh[j][i]);
                    i += lon_step;
                }
            }
        };
        project_row(0, &mut sp[0], &mut sz[0], &mut tint[0]);
        for j in 0..BALL_LAT {
            let (lo, hi) = (j & 1, (j + 1) & 1);
            project_row(j + 1, &mut sp[hi], &mut sz[hi], &mut tint[hi]);
            let (v_lo, v_hi) = (BALL_V0 + BALL_ROW_V[j], BALL_V0 + BALL_ROW_V[j + 1]);
            for i in (0..BALL_LON).step_by(lon_step) {
                let i2 = (i + lon_step) % BALL_LON;
                if sz[lo][i] == 0 || sz[lo][i2] == 0 || sz[hi][i] == 0 || sz[hi][i2] == 0 {
                    continue;
                }
                // A quad's four corners already sit on the sphere, so their sum
                // points straight out of it: that is the normal, for free.
                let (a, b) = (mesh[j][i], mesh[j][i2]);
                let (c, d) = (mesh[j + 1][i], mesh[j + 1][i2]);
                let k = 1024 / sim::BALL_R.max(1);
                let local = (
                    (a.0 + b.0 + c.0 + d.0) * k,
                    (a.1 + b.1 + c.1 + d.1) * k,
                    (a.2 + b.2 + c.2 + d.2) * k,
                );
                // Facing away: behind the front of the ball, always. Tested
                // before the lighting and the packet, so a culled facet costs
                // one dot product and nothing else.
                if local.0 * to_cam_local.0 + local.1 * to_cam_local.1 + local.2 * to_cam_local.2 <= 0 {
                    continue;
                }
                // The texture is baked through these exact coordinates (see
                // tools/cook-arena `ball_texel_direction`): eight texels a
                // column, and a pole's two corners at the column's middle, so
                // the polar triangle samples the panels the cooker put there
                // and the pattern keeps one size all over the ball. A split
                // view's double-width facets take the same rule over their
                // two columns.
                let (u_a, u_b) = (BALL_U0 + 8 * i as u8, BALL_U0 + 8 * (i + lon_step) as u8);
                let u_mid = (u_a + u_b) / 2;
                let (ua, ub) = if j == 0 { (u_mid, u_mid) } else { (u_a, u_b) };
                let (uc, ud) = if j + 1 == BALL_LAT { (u_mid, u_mid) } else { (u_a, u_b) };
                self.quad_tex_words(
                    [sp[lo][i], sp[lo][i2], sp[hi][i], sp[hi][i2]],
                    sz[lo][i] + sz[lo][i2] + sz[hi][i] + sz[hi][i2],
                    [uvw(ua, v_lo), uvw(ub, v_lo), uvw(uc, v_hi), uvw(ud, v_hi)],
                    [tint[lo][i], tint[lo][i2], tint[hi][i], tint[hi][i2]],
                    0,
                    BALL_PACKET,
                    false,
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
fn draw_cars<'a>(
    s: &Sim,
    cars: [usize; SEATS],
    view: &View,
    ot: &mut OtFrame<'a, OT_DEPTH>,
    lights: &LightRig,
) {
    let mut tris = unsafe { PrimitiveArena::new(&mut CAR_TRIS_SETS[SET]) };
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
        // Small on screen, draw the 60-face copy with the same paint; the full
        // mesh costs the same ~83k cycles at thirty pixels as it does filling
        // the screen. A split half keeps its own state and seat 0's sizes for
        // both cars (see `SPLIT_CAR_FAR_LOD`).
        let depth = view.camera_space(ground).2;
        let (state, size) = if split_view() {
            (unsafe { &mut SPLIT_CAR_FAR_LOD[SPLIT_HALF][seat] }, 0)
        } else {
            (unsafe { &mut CAR_FAR_LOD[seat] }, seat)
        };
        let far = lod_far(
            state,
            depth,
            CAR_LOD_ENTER_DEPTH[size],
            CAR_LOD_EXIT_DEPTH[size],
        );
        let which = if far { which + CAR_COUNT } else { which };
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
        // The wheels' own steer and roll go through the GTE's rotation slot,
        // so this runs before the car's transform is loaded into it.
        staged!(S_CAR_PROJECT, { pose_wheels(which, centres, &pose) });
        let view_rot = view.v.mul(&world);
        let t = view.camera_space(car_ground(body));
        ActorTransform::at(Vec3World::from_raw(t.0, t.1, t.2))
            .with_rotation(view_rot)
            .load_gte();
        lights.for_object(&view_rot).load();
        let materials = unsafe {
            if far && cfg!(feature = "diag-keys") {
                &DIAG_LOD_KEY
            } else if far {
                &PAINTED_LOD[seat]
            } else {
                &PAINTED_GAME[seat]
            }
        };
        let n = staged!(S_CAR_PROJECT, {
            on_scratchpad(|| project_car_animated(which, materials, CAR_WHEELS[which]))
        });
        let projected = unsafe { &mut CAR_PROJ[..n] };
        // The faces were checked against the full vertex count at boot, so
        // the face loop reads `projected` unchecked: refuse a wheel table
        // shorter than the mesh rather than let it read past what was
        // projected this frame.
        let faces = if n == unsafe { CAR_VERT_COUNT[which] } as usize {
            unsafe { &CAR_FACES[which][..CAR_FACE_COUNT[which] as usize] }
        } else {
            &[]
        };
        staged!(S_CAR_FACES, {
            on_scratchpad(|| submit_car_faces(faces, projected, &mut tris, ot))
        });
    }
}

/// How many rows the front end has, and where they sit on screen.
pub const MENU_ROWS: usize = 4;
const MENU_X: i16 = 10;
const MENU_W: i16 = 164;
/// Four rows since DEMO joined: the list moved up a row's height less a
/// pixel per gap so the bottom row keeps its old margin.
const MENU_TOP: i16 = 135;
const MENU_STEP: i16 = 24;

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
    unsafe {
        PENDING = false;
        SPLIT_PENDING = false;
    }
    build_view(s, cars, view, Viewport::FULL, Some(panels), 0);
    submit_detached();
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

/// Draw one frame of the match for one player, on the whole screen.
pub fn render(s: &Sim, cars: [usize; SEATS], ball_cam: bool, buffer_y: u16) {
    enter_view(Viewport::FULL, buffer_y);
    // EXPERIMENT: kick the table the previous call built, then build this
    // frame's into the other set while the GPU draws that one.
    unsafe {
        SPLIT_PENDING = false;
        if PENDING {
            submit_detached();
        }
        SET ^= 1;
    }
    render_view(s, cars, ball_cam, &s.car, Viewport::FULL, 0);
    unsafe { PENDING = true };
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
    // Pipelined the way the one-view path is: this frame draws the two
    // tables the last frame built, each kicked while the CPU builds this
    // frame's table for the same view, so the GPU's half of the work runs
    // under the CPU's instead of after it. The scissors travel inside the
    // tables (`AreaPacket`), because no immediate GP0 write may land while a
    // table is walking. The picture is one frame behind the sim, as it
    // already is with one view.
    //
    // The old order drew each view as soon as it was built and waited for
    // it, so a frame cost both views' CPU plus both views' GPU, and scenes
    // with both cars close together or ball cam down the length of the
    // arena ran past two vblanks (the boot-split-stress route).
    unsafe {
        PENDING = false;
        let (prev, cur) = (SET, SET ^ 1);
        let pending = SPLIT_PENDING;
        for (k, (vp, subject, cam, camera_slot)) in [
            (Viewport::TOP, near, near_cam, near_slot),
            (Viewport::BOTTOM, far, far_cam, far_slot),
        ]
        .into_iter()
        .enumerate()
        {
            let (old, new) = (prev + 2 * k, cur + 2 * k);
            if pending {
                if k == 1 {
                    // One DMA channel: the first view's walk must end
                    // before the second's starts.
                    psx_gpu::submit_linked_list_wait();
                }
                AREA_HEAD[old].set(vp, buffer_y);
                AREA_TAIL[old].set(Viewport::FULL, buffer_y);
                SET = old;
                submit_detached();
            }
            SET = new;
            SPLIT_HALF = k;
            VIEW_TAIL = k == 1;
            enter_view_cpu(vp);
            render_view(s, cars, cam, subject, vp, camera_slot);
            VIEW_TAIL = false;
            // Last into the first slot walked, so first in the table.
            OT_SETS[new].insert(
                SKY_SLOT,
                core::ptr::from_mut(&mut AREA_HEAD[new]).cast(),
                AREA_WORDS,
            );
        }
        SET = cur;
        SPLIT_PENDING = true;
    }
    // CPU side only: the second table, still walking, sets the whole screen
    // back as its last packet before the runner draws the HUD.
    enter_view_cpu(Viewport::FULL);
    hold_split_cadence();
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
            vp.h < SCREEN_H,
            camera_slot,
        )
    });
    build_view(s, cars, view, vp, None, subject.boost / sim::BOOST_SCALE);
}

/// The whole scratchpad, as a call stack for `build_view`'s phases.
type PhaseStack = psx_rt::scratchpad::ScratchpadStack<0, { psx_rt::scratchpad::SIZE }>;

/// Run one `build_view` phase with its frames in the scratchpad.
#[inline(always)]
fn on_scratchpad<R>(f: impl FnOnce() -> R) -> R {
    // SAFETY: nothing else in this game keeps data in the scratchpad, the
    // phases install no exception handler (psx-rt's vblank handler leaves
    // $sp alone), and the SDK's stack-guard proves each phase's call tree
    // fits PhaseStack::BUDGET after every link.
    unsafe { PhaseStack::run(f) }
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
                ot: unsafe { OtFrame::begin(&mut OT_SETS[SET]) },
                arena: unsafe { PrimitiveArena::new(&mut QUADS_SETS[SET]) },
                textured: unsafe { PrimitiveArena::new(&mut TEX_QUADS_SETS[SET]) },
                flats: unsafe { PrimitiveArena::new(&mut FLAT_QUADS_SETS[SET]) },
                glow: unsafe { PrimitiveArena::new(&mut GLOW_SETS[SET]) },
            };
            // First into slot 0, so walked after everything else.
            unsafe {
                if VIEW_TAIL {
                    b.ot.add_raw(
                        0,
                        core::ptr::from_mut(&mut AREA_TAIL[SET]).cast(),
                        AREA_WORDS,
                    );
                }
            }
            // The crowd's palettes for this frame, loaded before anything in
            // the table samples them.
            unsafe {
                for (seat, clut) in CROWD_CLUTS.iter().enumerate() {
                    let load = &mut CROWD_CLUT_LOAD[SET][seat];
                    load.xy = ((clut.y() as u32) << 16) | clut.x() as u32;
                    let key = Some((seat_signal(seat), (CAMERA_TICK / 6 % 3) as u8));
                    if CROWD_CLUT_KEY[SET][seat] != key {
                        load.data = crowd_clut(seat, CAMERA_TICK);
                        CROWD_CLUT_KEY[SET][seat] = key;
                    }
                    b.ot.add_raw(
                        SKY_SLOT,
                        core::ptr::from_mut(load).cast(),
                        CLUT_LOAD_WORDS,
                    );
                }
            }

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
        staged!(S_FLOOR, {
            on_scratchpad(|| {
                if split_view() {
                    b.floor::<true>(&cull);
                } else {
                    b.floor::<false>(&cull);
                }
                b.lines(&cull);
                b.tracks(&cull);
            });
            b.draw_pieces(&cull);
        });
        staged!(S_PADS, { on_scratchpad(|| b.pads(s, &cull)) });
        staged!(S_WALLS, {
            on_scratchpad(|| {
                b.walls(&cull);
                b.stands(&cull);
                b.lamps(&cull);
            });
            b.draw_pieces(&cull);
        });
        staged!(S_TRIM, {
            on_scratchpad(|| {
                // The dial belongs to whoever is looking through this view, and
                // sits the same distance in from that view's right edge.
                if let Some(panels) = &front {
                    match panels {
                        FrontPanels::Title(menu) => menu_panels(&mut b, menu),
                        FrontPanels::Select(select) => select_panels(&mut b, select),
                    }
                } else {
                    b.boost_gauge(boost_gauge_x(vp), boost_gauge_y(vp), boost);
                    // The scoreboard is not here: it belongs to the match rather
                    // than to a view, so the overlay draws it once over the whole
                    // screen (see `scoreboard`).
                    b.goal_burst(s);
                    b.demo_burst(s);
                }
                b.ceiling(&cull);
                b.goals(&view);
                b.ball_ring(s, &cull);
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
            })
        });
        // Off the scratchpad stack: the clip needs more frame than it has.
        b.flush_goal_jobs(&cull);

        staged!(S_BALL, { on_scratchpad(|| b.ball(s, &view)) });
        on_scratchpad(|| {
            for body in [&s.car, &s.opponent] {
                if !body.wrecked() {
                    b.car_flame(body, &view);
                }
            }
        });
        b.banner();
    }

    // Phase 2: the car mesh, appended into the same frame.
    // SAFETY: phase 1 linked only packets in this set's static arenas, which
    // nothing rewrites until this set comes round again, after its walk.
    let mut ot = unsafe { OtFrame::resume(&mut OT_SETS[SET]) };
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


/// Kick a whole-screen table and return while the walk runs.
///
/// Nothing between here and the present touches the GPU or the packets: the
/// runner drains the channel and the GPU before `render_overlay` and the
/// flip. What the CPU gains is the fixed update that falls due while the GPU
/// is still drawing, which a blocking kick left waiting behind the walk.
///
/// The engine's queued contract is the other way to overlap, and measures
/// worse here. It kicks this table only after the previous frame's flip and
/// waits for the walk before building the next one into the same packet
/// arena, and this game's build runs past one vblank, so on a FIFO-paced
/// DMA model the walk and the next build end up in series again
/// (2026-09-23, frozen frontend, train tape polls 396..1200: overrun frames
/// 11 -> 4 on the default model but 29 -> 75 with
/// PSOXIDE_EXPERIMENTAL_DMA_FIFO=1).
fn submit_detached() {
    staged!(S_SUBMIT, {
        apply_arena_draw_mode();
        // SAFETY: this set's table and packet arenas stay untouched until the
        // runner drains channel 2 before render_overlay and the flip; the next
        // frame builds into the other set.
        unsafe { psx_gpu::submit_linked_list_raw_async(OT_SETS[SET].submit_head()) };
    });
}

// ---- tyre tracks -----------------------------------------------------------
//
// A ring of floor points per rear wheel, laid by the sim tick while a wheel is
// sliding, and drawn as a strip of thin quads that darken the pitch. Drawn in
// their own slot just in front of the markings: the pitch is not depth-sorted,
// so nothing that lies on it can z-fight it, and everything standing on the
// pitch draws after the tracks.
//
// Paid for per chunk of four points rather than per point. Each chunk keeps
// the box its points (and the point before it, which its first segment starts
// from) cover, so a chunk behind the camera, off to the side or past the far
// fade costs one box test; a kept chunk projects its points three vertices to
// an RTPT.

/// Points kept per wheel. The oldest is overwritten first.
const TRACK_LEN: usize = 16;
/// Points per culling chunk, and chunks per wheel.
const TRACK_CHUNK: usize = 4;
const TRACK_CHUNKS: usize = TRACK_LEN / TRACK_CHUNK;
/// Two cars, two rear wheels each.
const TRACK_WHEELS: usize = 4;
/// How far a wheel travels, in uu, between two points of its track. Long,
/// because a mark is drawn a segment at a time and a 128 uu chord of a
/// powerslide arc sits under a pixel from the arc itself; the gap it would
/// leave behind the tyre is closed by a live segment to where the wheel is.
const TRACK_STEP: i32 = 128;
/// Past this a wheel has been teleported (kickoff, respawn), not driven.
const TRACK_JUMP: i32 = 400;
/// Ticks a point lives, and how many of them it spends fading out.
const TRACK_LIFE: u16 = 480;
const TRACK_FADE: u16 = 240;
/// Camera depth, in uu, at which a mark starts to fade, and past which it is
/// gone. Past the fade a mark is under two pixels wide: the fade is so that
/// it leaves gradually rather than at a line.
const TRACK_NEAR: i32 = 2600;
const TRACK_FAR: i32 = 3800;
/// Half the width of one tyre's mark, in uu.
const TRACK_HALF_W: i32 = 18;
/// Where the rear wheels touch the pitch, from the car's centre, in uu.
const TRACK_AXLE_Z: i32 = -50;
const TRACK_WHEEL_X: i32 = 44;
/// Sideways speed, in sim sub-units a tick, past which a tyre is scrubbing.
/// About 250 uu/s.
const TRACK_SLIP: i32 = 200;
/// How hard a car has to be cornering to leave a mark without sliding: its
/// heading change over the two ticks between its updates, in Q12 turns,
/// times its speed along the nose in uu a tick. A full-lock turn at speed
/// reaches about 960 (Manny's 2026-10-09 tape, polls 1535..1650); a gentle
/// arc stays under 400. The sim grips through every corner, so without this
/// only a handbrake turn ever marked and a player who steers and never
/// powerslides saw no marks at all.
const TRACK_CORNER: i32 = 840;
/// How dark a fresh mark is: subtracted from the pitch.
const TRACK_DARK: Rgb = (36, 40, 31);
/// The track slot: one in front of the markings.
const TRACK_SLOT: usize = LINE_SLOT - 1;

#[derive(Clone, Copy)]
struct TrackPoint {
    /// The two edges of the mark, in render uu on the floor plane.
    l: (i16, i16),
    r: (i16, i16),
    /// Clock tick it was laid on; 0 for a slot never written.
    born: u16,
    /// Whether this point continues the one before it.
    joined: bool,
}

const TRACK_POINT_NONE: TrackPoint = TrackPoint {
    l: (0, 0),
    r: (0, 0),
    born: 0,
    joined: false,
};

#[derive(Clone, Copy)]
struct TrackRing {
    pts: [TrackPoint; TRACK_LEN],
    /// Per chunk: the floor box (min x, min z, max x, max z) of its points
    /// and of the point before it, or `None` while it holds no point.
    boxes: [Option<(i16, i16, i16, i16)>; TRACK_CHUNKS],
    /// Per chunk: when its newest point was laid. Once that is older than
    /// a mark lives, the whole chunk is skipped without a box test.
    newest: [u16; TRACK_CHUNKS],
    /// Bit `c` set while chunk `c` has a box: a wheel with nothing on the
    /// pitch costs the renderer one test.
    chunks: u8,
    /// Next slot to write.
    head: u8,
    /// Whether the wheel was marking at its last point.
    open: bool,
    /// The wheel's position at its last point.
    last: (i16, i16),
    /// Where the tyre is now, as the edges of a mark, while it is marking
    /// and has not yet gone far enough for its next point.
    live: Option<((i16, i16), (i16, i16))>,
}

impl TrackRing {
    /// Rebuild chunk `c`'s box from its points and the one before it.
    fn rebox(&mut self, c: usize) {
        let mut b: Option<(i16, i16, i16, i16)> = None;
        for k in 0..=TRACK_CHUNK {
            let slot = (c * TRACK_CHUNK + TRACK_LEN + k - 1) % TRACK_LEN;
            let p = &self.pts[slot];
            if p.born == 0 {
                continue;
            }
            for (x, z) in [p.l, p.r] {
                b = Some(match b {
                    None => (x, z, x, z),
                    Some((x0, z0, x1, z1)) => (x0.min(x), z0.min(z), x1.max(x), z1.max(z)),
                });
            }
        }
        self.boxes[c] = b;
        if b.is_some() {
            self.chunks |= 1 << c;
        } else {
            self.chunks &= !(1 << c);
        }
    }

    /// Forget every chunk whose newest point has outlived a mark, points and
    /// all. Nothing is left to draw there, and a `born` left behind would
    /// come back to life when the 16-bit clock wraps.
    fn expire(&mut self, clock: u16) {
        for c in 0..TRACK_CHUNKS {
            if self.chunks & (1 << c) != 0 && clock.wrapping_sub(self.newest[c]) >= TRACK_LIFE {
                self.boxes[c] = None;
                self.chunks &= !(1 << c);
                for p in &mut self.pts[c * TRACK_CHUNK..(c + 1) * TRACK_CHUNK] {
                    p.born = 0;
                }
            }
        }
    }
}

const TRACK_RING_NONE: TrackRing = TrackRing {
    pts: [TRACK_POINT_NONE; TRACK_LEN],
    boxes: [None; TRACK_CHUNKS],
    newest: [0; TRACK_CHUNKS],
    chunks: 0,
    head: 0,
    open: false,
    last: (0, 0),
    live: None,
};
static mut TRACKS: [TrackRing; TRACK_WHEELS] = [TRACK_RING_NONE; TRACK_WHEELS];
/// Ticks since boot, from 1, so a `born` of 0 can mean "never".
static mut TRACK_CLOCK: u16 = 1;
/// Each car's heading when its tracks were last advanced.
static mut TRACK_YAW: [u16; 2] = [0; 2];
#[cfg(feature = "diag-log")]
static mut DIAG_TRK_QUADS: i32 = 0;
#[cfg(feature = "diag-log")]
static mut DIAG_TRK_FAIL: i32 = 0;

/// One chunk's vertices on their way through the GTE: two edges per point,
/// for the chunk's points and the one before it.
const TRACK_VERTS: usize = (TRACK_CHUNK + 1) * 2;
// `track_chunk` projects exactly five points.
const _: () = assert!(TRACK_CHUNK == 4);

/// A draw-mode word as an ordering-table packet: the tracks switch the GPU to
/// subtractive blending in front of themselves and back to the arena's
/// average behind themselves.
#[repr(C, align(4))]
struct ModePacket {
    tag: u32,
    word: u32,
}
static mut TRACK_MODE_ON: [ModePacket; SET_COUNT] =
    [const { ModePacket { tag: 0, word: 0 } }; SET_COUNT];
static mut TRACK_MODE_OFF: [ModePacket; SET_COUNT] =
    [const { ModePacket { tag: 0, word: 0 } }; SET_COUNT];
const TRACK_MODE_WORD: u32 = ARENA_MATERIAL
    .with_blend_mode(BlendMode::Subtract)
    .draw_mode_word();

/// Wipe every tyre mark. The marks age on the play clock, which stands still
/// between matches, so the last match's would otherwise sit on the new pitch
/// until it had played long enough to fade them.
pub fn reset_tracks() {
    unsafe {
        TRACKS = [TRACK_RING_NONE; TRACK_WHEELS];
        TRACK_YAW = [0; 2];
    }
}

/// Advance the tracks one sim tick: lay a point under every rear wheel that
/// is scrubbing and has moved far enough since its last one.
pub fn track_tick(s: &Sim) {
    let clock = unsafe {
        TRACK_CLOCK = TRACK_CLOCK.wrapping_add(1).max(1);
        TRACK_CLOCK
    };
    // One wheel a tick: a chunk outlives its marks by three ticks at most,
    // which the per-point age test already hides.
    unsafe { TRACKS[clock as usize % TRACK_WHEELS].expire(clock) };
    // One car a tick, alternating: a point is still laid every 128 uu (a car
    // covers under 40 uu a tick), and the live segment's end trails the tyre
    // by a tick at most, under the car.
    for (c, car) in [&s.car, &s.opponent].into_iter().enumerate() {
        if c != (clock & 1) as usize {
            continue;
        }
        // On the floor and in play, before anything is worked out: most
        // ticks most cars are not sliding.
        let on_floor = car.grounded && !car.wrecked() && car.up.y > 3900;
        let (fx, fz) = if on_floor { sim::heading(car.yaw) } else { (0, 0) };
        // Right of the nose, on the floor.
        let (rx, rz) = (fz, -fx);
        // Heading change since this car was last looked at, which is two ticks
        // ago (one car a tick), kept whether or not it is on the floor.
        let turned = {
            let last = unsafe { TRACK_YAW[c] };
            unsafe { TRACK_YAW[c] = car.yaw };
            ((car.yaw.wrapping_sub(last) as i16) as i32).abs()
        };
        let cornering = on_floor
            && turned * (((car.v.x * fx + car.v.z * fz) >> 12).abs() >> FP) >= TRACK_CORNER;
        let marking = on_floor
            && (cornering
                || car.slide > 512
                || ((car.v.x * rx + car.v.z * rz) >> 12).abs() > TRACK_SLIP);
        let (cx, cz) = (r(car.p.x), r(car.p.z));
        #[cfg(feature = "diag-log")]
        crate::diaglog::fill!(trk;
            clock as i32,
            c as i32,
            car.slide,
            ((car.v.x * rx + car.v.z * rz) >> 12),
            on_floor as i32
                | (marking as i32) << 1
                | (car.grounded as i32) << 2
                | (car.wrecked() as i32) << 3
                | (cornering as i32) << 4,
            car.yaw as i32,
            car.v.x,
            car.v.z,
        );
        for (w, side) in [-1i32, 1].into_iter().enumerate() {
            let ring = unsafe { &mut TRACKS[c * 2 + w] };
            if !marking {
                ring.open = false;
                ring.live = None;
                continue;
            }
            let ox = side * TRACK_WHEEL_X;
            let px = cx + ((rx * ox + fx * TRACK_AXLE_Z) >> 12);
            let pz = cz + ((rz * ox + fz * TRACK_AXLE_Z) >> 12);
            let (dx, dz) = (px - ring.last.0 as i32, pz - ring.last.1 as i32);
            let d = dx.abs().max(dz.abs());
            let (hx, hz) = ((rx * TRACK_HALF_W) >> 12, (rz * TRACK_HALF_W) >> 12);
            let edges = (
                ((px - hx) as i16, (pz - hz) as i16),
                ((px + hx) as i16, (pz + hz) as i16),
            );
            if ring.open && d < TRACK_STEP {
                ring.live = Some(edges);
                continue;
            }
            ring.live = None;
            let joined = ring.open && d < TRACK_JUMP;
            let slot = ring.head as usize;
            ring.pts[slot] = TrackPoint {
                l: edges.0,
                r: edges.1,
                born: clock,
                joined,
            };
            // The slot's own chunk, and the next one when this is the point
            // that chunk's first segment starts from.
            ring.newest[slot / TRACK_CHUNK] = clock;
            ring.rebox(slot / TRACK_CHUNK);
            if slot % TRACK_CHUNK == TRACK_CHUNK - 1 {
                ring.rebox((slot / TRACK_CHUNK + 1) % TRACK_CHUNKS);
            }
            ring.head = ((slot + 1) % TRACK_LEN) as u8;
            ring.open = true;
            ring.last = (px as i16, pz as i16);
        }
    }
}

/// How dark a mark of age `age` is at camera depth `sz`: the age fade times
/// the distance fade.
fn track_tint(age: u16, sz: u16) -> Rgb {
    let left = TRACK_LIFE.saturating_sub(age) as i32;
    let by_age = left.min(TRACK_FADE as i32) * 4096 / TRACK_FADE as i32;
    let by_depth = ((TRACK_FAR - sz as i32) * 4096 / (TRACK_FAR - TRACK_NEAR)).clamp(0, 4096);
    shade(TRACK_DARK, (by_age * by_depth) >> 12, 4096)
}

/// Is any of a floor box `(min x, min z, max x, max z)` in this view, and
/// nearer than the far fade? [`Cull::visible`] and
/// [`Cull::visible_vertically`] in one pass, with the box flat on the pitch
/// so its height terms drop out.
fn track_box_visible(cull: &TrackView, b: (i16, i16, i16, i16)) -> bool {
    let (cx, cz) = ((b.0 as i32 + b.2 as i32) >> 1, (b.1 as i32 + b.3 as i32) >> 1);
    let (hx, hz) = ((b.2 as i32 - b.0 as i32) >> 1, (b.3 as i32 - b.1 as i32) >> 1);
    let (dx, dz) = (cx - cull.pos.0, cz - cull.pos.1);
    // Each axis is `Cull::dot` over `(dx, -eye height, dz)` with the height
    // term folded in once per frame, and the extent is the same support
    // function over absolute components: the same integers either way.
    let at = |n: &TrackAxis| ((n.x * dx + n.z * dz + n.y_term) >> 12, (n.ax * hx + n.az * hz) >> 12);
    let (z, ef) = at(&cull.fwd);
    if z + ef <= 0 || z - ef > TRACK_FAR {
        return false;
    }
    let reach = z + ef;
    let (x, ex) = at(&cull.right);
    if x.abs() - ex > reach * cull.half_w / PROJ_H as i32 {
        return false;
    }
    let (y, ey) = at(&cull.vertical);
    y.abs() - ey <= reach * cull.half_h / PROJ_H as i32
}

/// One view axis with everything the floor-box test needs from it.
struct TrackAxis {
    x: i32,
    z: i32,
    ax: i32,
    az: i32,
    /// The axis' height component times the eye's height over the pitch.
    y_term: i32,
}

/// [`Cull`] reduced to what a box lying on the pitch needs, built once a
/// frame.
struct TrackView {
    /// The eye's ground position (x, z).
    pos: (i32, i32),
    fwd: TrackAxis,
    right: TrackAxis,
    vertical: TrackAxis,
    half_w: i32,
    half_h: i32,
}

impl TrackView {
    fn new(cull: &Cull) -> Self {
        let axis = |n: [i16; 3]| TrackAxis {
            x: n[0] as i32,
            z: n[2] as i32,
            ax: (n[0] as i32).abs(),
            az: (n[2] as i32).abs(),
            y_term: n[1] as i32 * -cull.pos.1,
        };
        TrackView {
            pos: (cull.pos.0, cull.pos.2),
            fwd: axis(cull.fwd),
            right: axis(cull.right),
            vertical: axis(cull.vertical),
            half_w: cull_half_w(),
            half_h: unsafe { VIEW_HALF_H },
        }
    }
}

impl Builder<'_> {
    /// Draw every live track as a strip of subtractive quads on the pitch.
    #[inline(never)]
    fn tracks(&mut self, cull: &Cull) {
        let clock = unsafe { TRACK_CLOCK };
        let set = unsafe { SET };
        let mut opened = false;
        // Most frames most wheels have laid nothing: one test then.
        #[cfg(not(feature = "diag-log"))]
        if (0..TRACK_WHEELS).all(|w| unsafe { TRACKS[w].chunks == 0 && TRACKS[w].live.is_none() }) {
            return;
        }
        let view = TrackView::new(cull);
        #[cfg(feature = "diag-log")]
        let (mut seen, mut kept) = (0i32, 0i32);
        for w in 0..TRACK_WHEELS {
            let ring = unsafe { &TRACKS[w] };
            if ring.chunks == 0 && ring.live.is_none() {
                continue;
            }
            #[cfg(feature = "diag-log")]
            {
                seen |= (ring.chunks as i32) << (w * 4);
            }
            for c in 0..TRACK_CHUNKS {
                if ring.chunks & (1 << c) == 0 {
                    continue;
                }
                let Some((x0, z0, x1, z1)) = ring.boxes[c] else {
                    continue;
                };
                if !track_box_visible(&view, (x0, z0, x1, z1)) {
                    continue;
                }
                #[cfg(feature = "diag-log")]
                {
                    kept |= 1 << (w * 4 + c);
                }
                if !opened {
                    // Inserted before the quads, so drawn after them: the
                    // slot prepends.
                    unsafe {
                        TRACK_MODE_OFF[set].word = GLOW_RESTORE;
                        self.ot.add_raw(
                            TRACK_SLOT,
                            core::ptr::from_mut(&mut TRACK_MODE_OFF[set]).cast(),
                            1,
                        );
                    }
                    opened = true;
                }
                self.track_chunk(w, c, clock);
            }
            // The live segment, from the newest point to the tyre.
            let Some((ll, lr)) = ring.live else {
                continue;
            };
            let p = &ring.pts[(ring.head as usize + TRACK_LEN - 1) % TRACK_LEN];
            if p.born == 0 {
                continue;
            }
            let (mut x0, mut z0, mut x1, mut z1) = (i16::MAX, i16::MAX, i16::MIN, i16::MIN);
            for (x, z) in [p.l, p.r, ll, lr] {
                (x0, x1) = (x0.min(x), x1.max(x));
                (z0, z1) = (z0.min(z), z1.max(z));
            }
            if !track_box_visible(&view, (x0, z0, x1, z1)) {
                continue;
            }
            let t = scene::project_triangle_scheduled(
                Vec3I16::new(p.l.0, 0, p.l.1),
                Vec3I16::new(p.r.0, 0, p.r.1),
                Vec3I16::new(ll.0, 0, ll.1),
            );
            let e = project(Vec3I16::new(lr.0, 0, lr.1));
            if t[0].sz == 0 || t[1].sz == 0 || t[2].sz == 0 || e.sz == 0 {
                continue;
            }
            let sp = [(t[0].sx, t[0].sy), (t[1].sx, t[1].sy), (t[2].sx, t[2].sy), (e.sx, e.sy)];
            let c = track_tint(0, e.sz);
            if c == (0, 0, 0) || !quad_overlaps_view(&sp) {
                continue;
            }
            if !opened {
                unsafe {
                    TRACK_MODE_OFF[set].word = GLOW_RESTORE;
                    self.ot.add_raw(
                        TRACK_SLOT,
                        core::ptr::from_mut(&mut TRACK_MODE_OFF[set]).cast(),
                        1,
                    );
                }
                opened = true;
            }
            if let Some(quad) = self.flats.push(QuadFlat::new(sp, c.0, c.1, c.2)) {
                quad.color_cmd |= SEMI_TRANSPARENT;
                self.ot.add_packet(TRACK_SLOT, quad);
            } else {
                #[cfg(feature = "diag-log")]
                unsafe {
                    DIAG_TRK_FAIL += 1;
                }
            }
        }
        #[cfg(feature = "diag-log")]
        unsafe {
            crate::diaglog::fill!(draw;
                clock as i32,
                seen,
                kept,
                opened as i32,
                DIAG_TRK_QUADS,
                DIAG_TRK_FAIL,
                (0..TRACK_WHEELS).fold(0, |a, w| a | (TRACKS[w].live.is_some() as i32) << w),
                0,
            );
            DIAG_TRK_QUADS = 0;
            DIAG_TRK_FAIL = 0;
        }
        if opened {
            unsafe {
                TRACK_MODE_ON[set].word = TRACK_MODE_WORD;
                self.ot.add_raw(
                    TRACK_SLOT,
                    core::ptr::from_mut(&mut TRACK_MODE_ON[set]).cast(),
                    1,
                );
            }
        }
    }

    /// One chunk of one track: project its points and the one before it in
    /// RTPT triples, then lay a quad for every joined, live pair. The
    /// projected corners stay in this frame, which is on the scratchpad.
    #[inline(never)]
    fn track_chunk(&mut self, w: usize, c: usize, clock: u16) {
        let ring = unsafe { &TRACKS[w] };
        let first = c * TRACK_CHUNK + TRACK_LEN - 1;
        // A chunk whose segments have all aged out or never joined (a lone
        // point) draws nothing: find that before the GTE is touched.
        let live = (1..=TRACK_CHUNK).any(|k| {
            let slot = (first + k) % TRACK_LEN;
            let p = &ring.pts[slot];
            if !p.joined || slot == ring.head as usize {
                return false;
            }
            let q = &ring.pts[(first + k - 1) % TRACK_LEN];
            let (age_p, age_q) = (clock.wrapping_sub(p.born), clock.wrapping_sub(q.born));
            p.born != 0 && q.born != 0 && age_p < TRACK_LIFE && age_q < TRACK_LIFE
        });
        if !live {
            return;
        }
        // The chunk's five points (the one before it and its own four), both
        // edges each: three RTPTs and an RTPS, written out rather than looped
        // so no vertex pays for an index sum, a modulo and an edge select.
        let pt = |k: usize| &ring.pts[(first + k) % TRACK_LEN];
        let (p0, p1, p2, p3, p4) = (pt(0), pt(1), pt(2), pt(3), pt(4));
        let v = |e: (i16, i16)| Vec3I16::new(e.0, 0, e.1);
        let t0 = scene::project_triangle_scheduled(v(p0.l), v(p0.r), v(p1.l));
        let t1 = scene::project_triangle_scheduled(v(p1.r), v(p2.l), v(p2.r));
        let t2 = scene::project_triangle_scheduled(v(p3.l), v(p3.r), v(p4.l));
        let t3 = project(v(p4.r));
        let all = [t0[0], t0[1], t0[2], t1[0], t1[1], t1[2], t2[0], t2[1], t2[2], t3];
        let head = ring.head as usize;
        // One shade for the chunk, from its newest point and its middle
        // depth: four segments laid within a few ticks of each other fade
        // together, and the depth fade only acts far off where a mark is a
        // pixel or two wide. A flat quad is a quarter of a Gouraud one's
        // setup on the GPU and a third fewer words to build.
        let tint = track_tint(clock.wrapping_sub(ring.newest[c]), all[TRACK_VERTS / 2].sz);
        if tint == (0, 0, 0) {
            return;
        }
        for k in 1..=TRACK_CHUNK {
            let slot = (first + k) % TRACK_LEN;
            let p = &ring.pts[slot];
            // The oldest point's predecessor is the newest one.
            if !p.joined || slot == head {
                continue;
            }
            let q = &ring.pts[(first + k - 1) % TRACK_LEN];
            let (age_p, age_q) = (clock.wrapping_sub(p.born), clock.wrapping_sub(q.born));
            if p.born == 0 || q.born == 0 || age_p >= TRACK_LIFE || age_q >= TRACK_LIFE {
                continue;
            }
            let (n0, n1) = (2 * k - 2, 2 * k);
            let (a, b, e, f) = (all[n0], all[n0 + 1], all[n1], all[n1 + 1]);
            if a.sz == 0 || b.sz == 0 || e.sz == 0 || f.sz == 0 {
                continue;
            }
            // No per-segment view test: the chunk passed the frustum, and a
            // segment hanging off the edge is the GPU's to clip.
            let sp = [(a.sx, a.sy), (b.sx, b.sy), (e.sx, e.sy), (f.sx, f.sy)];
            if let Some(quad) = self.flats.push(QuadFlat::new(sp, tint.0, tint.1, tint.2)) {
                quad.color_cmd |= SEMI_TRANSPARENT;
                self.ot.add_packet(TRACK_SLOT, quad);
                #[cfg(feature = "diag-log")]
                unsafe {
                    DIAG_TRK_QUADS += 1;
                }
            } else {
                #[cfg(feature = "diag-log")]
                unsafe {
                    DIAG_TRK_FAIL += 1;
                }
            }
        }
    }
}

// ---- explosions ------------------------------------------------------------
//
// The PS1 way: every layer is a soft round sprite cut from the radial glow
// tile already in VRAM, drawn additively (fire, flash, sparks, the ring) or
// subtractively (smoke), so nothing reads as a square and light piles up
// where pieces overlap. Five layers on one clock: a flash, a ring, fireballs
// that swell and cool from white through orange to nothing, sparks that fly
// and fall, and smoke that darkens what is behind it as it rises.

/// A goal's blast against a demolition's. The camera is most of an arena
/// away from the net, and the original blast is goal-sized only at this.
const GOAL_BURST_SCALE: i32 = 4096 * 7;
/// What pulls a spark back down, in uu per tick squared, Q8. Not the arena's:
/// a spark under 650 uu/s^2 barely moves in the half second it is alive.
const BURST_GRAVITY: i32 = 128;

const FX_SMOKE_PACKET: TexturedGouraudPacketMaterial =
    TextureMaterial::new(GLOW_CLUT.uv_clut_word(), TEX_TPAGE.uv_tpage_word(0))
        .with_blend_mode(BlendMode::Subtract)
        .with_dither(true)
        .textured_gouraud_packet_material();
const FX_RING_PACKET: TexturedGouraudPacketMaterial =
    TextureMaterial::new(RING_CLUT.uv_clut_word(), TEX_TPAGE.uv_tpage_word(0))
        .with_blend_mode(BlendMode::Add)
        .with_dither(true)
        .textured_gouraud_packet_material();

const FX_FLASH_LIFE: i32 = 7;
const FX_RING_LIFE: i32 = 20;
const FX_FIRE_COUNT: i32 = 10;
const FX_SPARK_COUNT: i32 = 14;
const FX_SMOKE_COUNT: i32 = 7;
const FX_SMOKE_LIFE: i32 = 56;
/// Everything is over by this age.
const FX_LIFE: i32 = 80;
/// The nearest a demolition's blast is ever drawn from, in uu of camera
/// depth; a goal's scales with it. See [`Builder::burst`].
const FX_NEAR: i32 = 700;
/// White-hot, then the flame colour fire cools through.
const FX_HOT: Rgb = (255, 246, 214);
const FX_FLAME: Rgb = (240, 128, 34);

/// What a blast works out from its particle's index and nothing else, done
/// once at boot: the trigonometry of every direction and the divisions by a
/// particle's own life. The detail variant is split screen's half set.
struct BurstTables {
    /// sin and cos of each puff's heading.
    smoke: [[(i32, i32); FX_SMOKE_COUNT as usize]; 2],
    /// A fireball's direction: sin(yaw) * cos(elev) and cos(yaw) * cos(elev)
    /// (Q12), and sin(elev).
    fire: [[(i32, i32, i32); FX_FIRE_COUNT as usize]; 2],
    /// A spark's direction on the three axes (Q12).
    spark: [[(i32, i32, i32); FX_SPARK_COUNT as usize]; 2],
    /// `t * 16 / life` and `80 * t / life` for each fireball, by its age `t`.
    fire_q: [[u8; FX_FIRE_LIFE_MAX]; FX_FIRE_COUNT as usize],
    fire_r: [[u8; FX_FIRE_LIFE_MAX]; FX_FIRE_COUNT as usize],
    /// `2^20 / life` rounded up, for [`shade_life`].
    fire_inv: [u32; FX_FIRE_COUNT as usize],
    spark_inv: [u32; FX_SPARK_COUNT as usize],
}
/// The longest fireball life, plus one: `26 + (i * 5) % 15` is at most 40.
const FX_FIRE_LIFE_MAX: usize = 41;
static mut BURST: BurstTables = BurstTables {
    smoke: [[(0, 0); FX_SMOKE_COUNT as usize]; 2],
    fire: [[(0, 0, 0); FX_FIRE_COUNT as usize]; 2],
    spark: [[(0, 0, 0); FX_SPARK_COUNT as usize]; 2],
    fire_q: [[0; FX_FIRE_LIFE_MAX]; FX_FIRE_COUNT as usize],
    fire_r: [[0; FX_FIRE_LIFE_MAX]; FX_FIRE_COUNT as usize],
    fire_inv: [0; FX_FIRE_COUNT as usize],
    spark_inv: [0; FX_SPARK_COUNT as usize],
};

const fn fx_fire_life(i: i32) -> i32 {
    26 + (i * 5) % 15
}
const fn fx_spark_life(i: i32) -> i32 {
    16 + (i * 7) % 14
}

fn build_burst() {
    let b = unsafe { &mut *core::ptr::addr_of_mut!(BURST) };
    for (v, half) in [false, true].into_iter().enumerate() {
        let puffs = if half { FX_SMOKE_COUNT / 2 + 1 } else { FX_SMOKE_COUNT };
        for i in 0..puffs {
            let yaw = ((4096 * i / puffs) + 700) as u16;
            b.smoke[v][i as usize] = (sin_q12(yaw), cos_q12(yaw));
        }
        let balls = if half { FX_FIRE_COUNT / 2 } else { FX_FIRE_COUNT };
        for i in 0..balls {
            let yaw = ((4096 * i / balls) + ((i * 997) & 511)) as u16;
            let elev = (300 + ((i * 331) & 511)) as u16;
            let (ce, se) = (cos_q12(elev), sin_q12(elev));
            b.fire[v][i as usize] = ((sin_q12(yaw) * ce) >> 12, (cos_q12(yaw) * ce) >> 12, se);
        }
        let sparks = if half { FX_SPARK_COUNT / 2 } else { FX_SPARK_COUNT };
        for i in 0..sparks {
            let yaw = ((4096 * i / sparks) + ((i * 1013) & 255)) as u16;
            let elev = (((i * 577) & 1023) + 96) as u16;
            let (ce, se) = (cos_q12(elev), sin_q12(elev));
            b.spark[v][i as usize] = ((sin_q12(yaw) * ce) >> 12, se, (cos_q12(yaw) * ce) >> 12);
        }
    }
    for i in 0..FX_FIRE_COUNT {
        let life = fx_fire_life(i);
        for t in 0..life {
            b.fire_q[i as usize][t as usize] = (t * 16 / life) as u8;
            b.fire_r[i as usize][t as usize] = (80 * t / life) as u8;
        }
        b.fire_inv[i as usize] = ((1 << 20) + life as u32 - 1) / life as u32;
    }
    for i in 0..FX_SPARK_COUNT {
        let life = fx_spark_life(i) as u32;
        b.spark_inv[i as usize] = ((1 << 20) + life - 1) / life;
    }
}

/// [`shade`] by `num / life` with `inv` = `2^20 / life` rounded up: the same
/// result for every `channel * num` below 2^20 / life (the rounding error of
/// `inv` is under `life`, so the product's error stays under one), which a
/// life of at most 40 and a channel times a numerator of at most 255 * 80
/// never leave, and with no divide.
fn shade_life(c: Rgb, num: i32, inv: u32) -> Rgb {
    let ch = |v: u8| ((v as u32 * num as u32 * inv) >> 20).min(255) as u8;
    (ch(c.0), ch(c.1), ch(c.2))
}

impl Builder<'_> {
    /// A camera-facing glow of `radius` uu, its screen half-size capped.
    #[allow(clippy::too_many_arguments)]
    fn fx_sprite(
        &mut self,
        c: (i32, i32, i32),
        radius: i32,
        cap: i32,
        tint: Rgb,
        packet: TexturedGouraudPacketMaterial,
        bias: i32,
    ) {
        if tint == (0, 0, 0) {
            return;
        }
        let v = project(Vec3I16::new(c.0 as i16, c.1 as i16, c.2 as i16));
        if v.sz == 0 {
            return;
        }
        let h = (radius * PROJ_H as i32 / v.sz.max(1) as i32).clamp(1, cap) as i16;
        let sp = [
            (v.sx - h, v.sy - h),
            (v.sx + h, v.sy - h),
            (v.sx - h, v.sy + h),
            (v.sx + h, v.sy + h),
        ];
        if !quad_overlaps_view(&sp) {
            return;
        }
        self.emit_glow(sp, v.sz as i32 + bias, Self::GLOW_UVS, [tint; 4], packet);
    }

    /// A spark: the glow tile stretched from where it was to where it is, so
    /// it is a soft streak of light rather than a hard-edged plank.
    fn fx_streak(&mut self, head: (i32, i32, i32), tail: (i32, i32, i32), w: i32, tint: Rgb) {
        if tint == (0, 0, 0) {
            return;
        }
        let h = project(Vec3I16::new(head.0 as i16, head.1 as i16, head.2 as i16));
        let t = project(Vec3I16::new(tail.0 as i16, tail.1 as i16, tail.2 as i16));
        if h.sz == 0 || t.sz == 0 {
            return;
        }
        let half = (w * PROJ_H as i32 / h.sz.max(1) as i32).clamp(2, 8);
        let (mut dx, mut dy) = (h.sx as i32 - t.sx as i32, h.sy as i32 - t.sy as i32);
        let mut len = isqrt_i32(dx * dx + dy * dy).max(1);
        if len > 40 {
            dx = dx * 40 / len;
            dy = dy * 40 / len;
            len = 40;
        }
        // Lengthen both ends by the width so the soft tip is not cut off.
        let (ex, ey) = (dx * half / len, dy * half / len);
        let (hx, hy) = (h.sx as i32 + ex, h.sy as i32 + ey);
        let (tx, ty) = (h.sx as i32 - dx - ex, h.sy as i32 - dy - ey);
        let (px, py) = (-dy * half / len, dx * half / len);
        let (px, py) = if px == 0 && py == 0 { (half, 0) } else { (px, py) };
        let sp = [
            ((hx + px) as i16, (hy + py) as i16),
            ((hx - px) as i16, (hy - py) as i16),
            ((tx + px) as i16, (ty + py) as i16),
            ((tx - px) as i16, (ty - py) as i16),
        ];
        if !quad_overlaps_view(&sp) {
            return;
        }
        self.emit_glow(sp, h.sz as i32 - 20, Self::GLOW_UVS, [tint; 4], GLOW_PACKET);
    }

    fn burst(&mut self, origin: (i32, i32, i32), age: i32, colour: Rgb, scale: i32) {
        if !(0..FX_LIFE).contains(&age) {
            return;
        }
        let half_detail = split_view();
        let big = scale > 4096;
        // One screen ceiling for every layer, ring and flash bloom included.
        // Those two reach past everything else, and at twice the ceiling each
        // was a full-screen blended quad close to the camera, where the GPU
        // rather than the CPU then missed the frame. Further out, where a
        // blast is normally seen, none of them is near it.
        let cap = if big { 96 } else { 64 };
        // Never nearer than FX_NEAR (times the blast's own scale): closer
        // in, the whole blast shrinks about its centre, so on screen it is
        // the size it would be from there. Ten fireballs at the ceiling
        // overlapping across the screen were most of a frame of blended
        // fill, and the GPU missed the frame; from FX_NEAR out nothing
        // changes.
        let near = (FX_NEAR * scale) >> 12;
        let o = project(Vec3I16::new(origin.0 as i16, origin.1 as i16, origin.2 as i16));
        let scale = if o.sz != 0 && (o.sz as i32) < near {
            (scale >> 4) * o.sz as i32 / (near >> 4).max(1)
        } else {
            scale
        };
        let s = |v: i32| (v * scale) >> 12;

        // Smoke first in code, last on screen: it sorts a little behind the
        // fire so the fire burns in front of it.
        let puffs = if half_detail { FX_SMOKE_COUNT / 2 + 1 } else { FX_SMOKE_COUNT };
        for i in 0..puffs {
            let tab = unsafe { &*core::ptr::addr_of!(BURST) };
            let born = 4 + i * 3;
            let t = age - born;
            if t < 0 || t >= FX_SMOKE_LIFE {
                continue;
            }
            let (sin_yaw, cos_yaw) = tab.smoke[half_detail as usize][i as usize];
            let travel = s(26) * t * (2 * FX_SMOKE_LIFE - t) / (2 * FX_SMOKE_LIFE) / 8;
            let p = (
                origin.0 + ((sin_yaw * travel) >> 12),
                origin.1 - s(50) - s(12) * t / 2,
                origin.2 + ((cos_yaw * travel) >> 12),
            );
            let radius = s(40 + 100 * t / FX_SMOKE_LIFE);
            // In over six ticks, out over the last two thirds.
            let k = (t * 4096 / 6).min(4096).min((FX_SMOKE_LIFE - t) * 4096 * 3 / (FX_SMOKE_LIFE * 2));
            let tint = shade((40, 42, 46), k, 4096);
            self.fx_sprite(p, radius, cap, tint, FX_SMOKE_PACKET, 40);
        }

        // Fireballs: swell, drift out and up, and cool.
        let balls = if half_detail { FX_FIRE_COUNT / 2 } else { FX_FIRE_COUNT };
        for i in 0..balls {
            let tab = unsafe { &*core::ptr::addr_of!(BURST) };
            let born = i % 3;
            let life = fx_fire_life(i);
            let t = age - born;
            if t < 0 || t >= life {
                continue;
            }
            let (dir_x, dir_z, sin_elev) = tab.fire[half_detail as usize][i as usize];
            let speed = s(9 + (i % 4) * 3);
            let travel = speed * t * (2 * life - t) / (2 * life);
            let p = (
                origin.0 + ((dir_x * travel) >> 12),
                origin.1 - ((sin_elev * travel) >> 12) - s(10) - s(2) * t,
                origin.2 + ((dir_z * travel) >> 12),
            );
            let radius = s(40 + tab.fire_r[i as usize][t as usize] as i32);
            // White for the first sixth, flame by half, then out to nothing.
            let q = tab.fire_q[i as usize][t as usize] as i32;
            let tint = if q < 3 {
                mix(FX_HOT, FX_FLAME, q * 5)
            } else if q < 8 {
                mix(mix(FX_FLAME, colour, 5), FX_FLAME, 16 - (q - 3) * 3)
            } else {
                shade_life(mix(FX_FLAME, colour, 6), (life - t) * 2, tab.fire_inv[i as usize])
            };
            self.fx_sprite(p, radius, cap, shade(tint, 3, 4), GLOW_PACKET, -10);
        }

        // Sparks: fast, eased out, pulled down hard.
        let sparks = if half_detail { FX_SPARK_COUNT / 2 } else { FX_SPARK_COUNT };
        for i in 0..sparks {
            let tab = unsafe { &*core::ptr::addr_of!(BURST) };
            let born = (i * 3) % 4;
            let life = fx_spark_life(i);
            let t = age - born;
            if t < 0 || t >= life {
                continue;
            }
            let dir = tab.spark[half_detail as usize][i as usize];
            let speed = s(40 - ((i * 7) & 15));
            let at = |t: i32| {
                let travel = speed * t * ((2 * life) - t) / (2 * life);
                let drop = (t * t * s(BURST_GRAVITY)) >> 8;
                (
                    origin.0 + ((dir.0 * travel) >> 12),
                    origin.1 - ((dir.1 * travel) >> 12) + drop,
                    origin.2 + ((dir.2 * travel) >> 12),
                )
            };
            let left = life - t;
            let tint = shade_life(
                mix(colour, FX_HOT, 6 + 10 * left / life),
                (left * 3).min(life),
                tab.spark_inv[i as usize],
            );
            self.fx_streak(at(t), at((t - 3).max(0)), s(7), tint);
        }

        // The ring: a shock front off the centre, quick and gone.
        if age < FX_RING_LIFE {
            let k = age * 4096 / FX_RING_LIFE;
            let ease = 4096 - (((4096 - k) * (4096 - k)) >> 12);
            let radius = s(30) + ((s(210) * ease) >> 12);
            let tint = shade(mix(FX_HOT, colour, 7), (FX_RING_LIFE - age) * 3, FX_RING_LIFE * 5);
            self.fx_sprite(origin, radius, cap, tint, FX_RING_PACKET, -30);
        }

        // The flash: a white core inside a wide coloured bloom, gone in a
        // ninth of a second. Nearest of everything.
        if age < FX_FLASH_LIFE {
            let grow = (age + 2) * 4096 / FX_FLASH_LIFE;
            let r0 = (s(120) * grow) >> 12;
            let fade = FX_FLASH_LIFE - age;
            self.fx_sprite(
                origin,
                r0 * 2,
                cap,
                shade(mix(colour, FX_HOT, 6), fade, FX_FLASH_LIFE),
                GLOW_PACKET,
                -40,
            );
            self.fx_sprite(
                origin,
                r0,
                cap,
                shade(FX_HOT, fade, FX_FLASH_LIFE),
                GLOW_PACKET,
                -50,
            );
        }
    }
}

// ---- goal banner -----------------------------------------------------------

/// `scale_q8_i16` of psx-font: a glyph side at a Q8 scale, rounded, at least 1.
fn banner_side(value: i16, q8: u16) -> i16 {
    ((i32::from(value) * i32::from(q8) + 128) >> 8).clamp(1, i32::from(i16::MAX)) as i16
}

/// `round_q8_to_i16` of psx-font for a non-negative cursor.
fn banner_round(q8: i32) -> i16 {
    ((q8.saturating_add(128)) >> 8).clamp(i32::from(i16::MIN), i32::from(i16::MAX)) as i16
}

/// One glyph of an atlas as the font crate lays it out: where it is in its
/// texture page, its CLUT and page words, and its size. `None` for a
/// character the atlas has no glyph for.
fn banner_glyph(font: &FontAtlas, ch: char) -> Option<(u8, u8, u16, u16, i16, i16, u32)> {
    let mut utf8 = [0u8; 4];
    let text: &str = ch.encode_utf8(&mut utf8);
    let (mut mode, mut found) = (0u32, None);
    font.emit_text_packets(0, 0, text, (0, 0, 0), |words| {
        match words.len() {
            2 => mode = words[0],
            4 => found = Some((words[2], words[3])),
            _ => {}
        }
        true
    });
    let (uv_clut, size) = found?;
    Some((
        uv_clut as u8,
        (uv_clut >> 8) as u8,
        (uv_clut >> 16) as u16,
        (mode & 0x1FF) as u16,
        (size & 0xFFFF) as i16,
        (size >> 16) as i16,
        mode,
    ))
}

impl Builder<'_> {
    /// File the banner's glyph packets in the last slot, drawn after the scene.
    /// Out of line: it is built on the main stack, after the scratchpad phases.
    #[inline(never)]
    fn banner(&mut self) {
        let Some(spec) = (unsafe { BANNER }) else {
            return;
        };
        let quads = unsafe { &mut BANNER_QUADS[SET] };
        let key: BannerKey = (spec.y, spec.q8, spec.parts);
        let built = unsafe { BANNER_BUILT[SET] };
        let (mut n, mut mode) = (0, 0);
        if let Some((was, count)) = built {
            if was == key {
                n = count;
                mode = unsafe { BANNER_MODE[SET].word };
            }
        }
        let rebuild = n == 0;
        for (x, text, ink) in spec.parts {
            if !rebuild {
                break;
            }
            // Each glyph's atlas entry once, not once per pass.
            let mut glyphs = [None; 12];
            for (slot, ch) in glyphs.iter_mut().zip(text.chars()) {
                *slot = Some((ch, banner_glyph(&spec.font, ch)));
            }
            for pass in 0..=BANNER_OUTLINE.len() {
                let ((dx, dy), tint) = match BANNER_OUTLINE.get(pass) {
                    Some(&d) => (d, (0, 0, 0)),
                    // Half strength, as `NitroXide::ink` has it.
                    None => ((0, 0), (ink.0.div_ceil(2), ink.1.div_ceil(2), ink.2.div_ceil(2))),
                };
                let mut cursor = i32::from(x) << 8;
                for (ch, glyph) in glyphs.iter().flatten() {
                    let cx = banner_round(cursor) + dx;
                    if let Some(&(u, v, clut, tpage, gw, gh, glyph_mode)) = glyph.as_ref() {
                        let (sw, sh) = (banner_side(gw, spec.q8), banner_side(gh, spec.q8));
                        let y = spec.y + dy;
                        // The font crate's UVs: the far edge saturates.
                        let (u1, v1) = (u.saturating_add(gw as u8), v.saturating_add(gh as u8));
                        if n < BANNER_MAX {
                            quads[n] = QuadTexturedMaterial::with_material(
                                [(cx, y), (cx + sw, y), (cx, y + sh), (cx + sw, y + sh)],
                                [(u, v), (u1, v), (u, v1), (u1, v1)],
                                TextureMaterial::opaque(clut, tpage, tint),
                            );
                            n += 1;
                            mode = glyph_mode;
                        }
                    }
                    cursor += i32::from(spec.font.glyph_advance(*ch)) * i32::from(spec.q8);
                }
            }
        }
        unsafe { BANNER_BUILT[SET] = Some((key, n)) };
        // The table prepends within a slot, so the last glyph goes in first.
        for k in (0..n).rev() {
            self.ot.add_packet(0, unsafe { &mut BANNER_QUADS[SET][k] });
        }
        if n > 0 {
            // Last in, so first drawn: the font's draw mode ahead of the glyphs.
            unsafe {
                BANNER_MODE[SET].word = mode;
                self.ot.add_raw(0, core::ptr::from_mut(&mut BANNER_MODE[SET]).cast(), 1);
            }
        }
    }
}
