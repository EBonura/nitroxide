// SPDX-License-Identifier: GPL-2.0-or-later
//! NitroXide -- rocket-powered car soccer for the PlayStation 1, on PSoXide.
//!
//! The whole match lives in `nitroxide-sim` (host-testable integer physics);
//! this crate polls the pad, hands the sim one `Input` per 60 Hz tick, and
//! draws the result. Title -> Play -> Results run inside one `Scene`, the same
//! way the sibling PS1 projects do it.

#![no_std]
#![no_main]
#![allow(static_mut_refs)]

extern crate psx_rt;

use psx_engine::{
    button, ActionBinding, ActionMap, App, Config, Ctx, Deadzone, PadState, Scene, VisualPacing,
};
use psx_font::{
    fonts::{KENNEY_PIXEL, KENNEY_ROCKET},
    FontAtlas,
};
use psx_math::fmt::{u32_dec, U32_DEC_MAX};
use psx_math::sincos::{cos_q12, sin_q12};
use psx_settings::Profile;
use psx_vram::{Clut, TexDepth, Tpage};

use nitroxide_sim::{Input, Sim, Team, WinCondition, BOOST_MAX_PIPS, BOOST_SCALE};

mod assets;
mod audio;
mod bonnie;
mod draw;
mod music;

/// Two faces, the way most games do it: a wide display one for headings and a
/// compact one for anything you read at a glance mid-match. Kenney Rocket is
/// 18 px wide, which is right for a title and far too wide for a clock.
const DISPLAY_TPAGE: Tpage = Tpage::new(320, 0, TexDepth::Bit4);
const DISPLAY_CLUT: Clut = Clut::new(320, 256);
/// Clear of the display atlas at 320 and the arena's own texture page at 384.
const HUD_TPAGE: Tpage = Tpage::new(512, 0, TexDepth::Bit4);
const HUD_CLUT: Clut = Clut::new(512, 256);

/// Bonnie Studios intro logo: 128x128 4bpp in an otherwise free VRAM column,
/// its CLUT in the spare row under the HUD's. Same art and placement family as
/// VoXide and the Celeste collection.
const BONNIE_TPAGE: Tpage = Tpage::new(896, 0, TexDepth::Bit4);
const BONNIE_CLUT: Clut = Clut::new(896, 256);

/// Intro cadence, matched to the other games on the demo disc: fade in, hold,
/// fade out, in engine ticks.
const INTRO_FADE_IN: i32 = 32;
const INTRO_HOLD: i32 = 74;
const INTRO_TOTAL: i32 = 150;
const INTRO_FADE_OUT: i32 = INTRO_TOTAL - INTRO_FADE_IN - INTRO_HOLD;

/// Analog deflection below this is treated as centred: a worn DualShock never
/// reads exactly 0x80.
///
/// The one-axis form, applied to X and Y separately, because the two axes mean
/// unrelated things: X steers on the ground, Y pitches in the air. A radial
/// test would let noise on one enable the other. scaled_axis rather than a bare
/// gate, so the wheel eases off centre instead of jumping straight to a quarter
/// lock the moment the stick clears the zone.
const SETTINGS_FILE: &str = "BESLES-00000NITRO01";
const SETTINGS_TITLE: &str = "NitroXide Settings";
const ACT_LEFT: usize = 0;
const ACT_RIGHT: usize = 1;
const ACT_UP: usize = 2;
const ACT_DOWN: usize = 3;
const ACT_THROTTLE: usize = 4;
const ACT_REVERSE: usize = 5;
const ACT_BOOST: usize = 6;
const ACT_JUMP: usize = 7;
const ACT_AIR_ROLL: usize = 8;
const DRIVE_ACTIONS: ActionMap<9> = ActionMap::new([
    ActionBinding::new(button::LEFT, 0),
    ActionBinding::new(button::RIGHT, 0),
    ActionBinding::new(button::UP, 0),
    ActionBinding::new(button::DOWN, 0),
    ActionBinding::new(button::R2, 0),
    ActionBinding::new(button::L2, 0),
    ActionBinding::new(button::CIRCLE, 0),
    ActionBinding::new(button::CROSS, 0),
    ActionBinding::new(button::L1, 0),
]);

#[derive(Clone, Copy, PartialEq)]
enum Phase {
    /// Boot splash: the Bonnie Studios logo and the "Built with PSoXide"
    /// line, matched to VoXide's and hl-psx's cadence. Any face button or
    /// Start skips it once the opening frames have passed.
    Intro,
    Title,
    /// Both seats pick a car and a colour, staged on the pitch the title uses.
    Select,
    Play,
    Results,
}

/// Two seat-owned rows plus shared arena and win-condition rows.
const PLAYER_SELECT_ROWS: usize = 2;
const SELECT_ROWS: usize = 4;
/// Rows the display face's capitals actually ink, out of its fifteen-row cell.
/// Measured off a frame rather than derived: nothing in `psx-font` reports a
/// cap height, and every label the front end sets in this face is capitals.
const DISPLAY_CAP_H: i16 = 10;

/// Top of a seat's labels, spacing between its separate option bars, and how
/// far out from the centre the arrows sit.
const SELECT_TOP: i16 = 134;
const SELECT_ROW_STEP: i16 = 20;
/// Kenney Pixel is 11 pixels high inside an 18-pixel option bar. Four pixels
/// above and three below is the nearest exact vertical centre on whole pixels.
const SELECT_TEXT_OFFSET: i16 = 18;
const SELECT_ARENA_Y: i16 = 188;
const SELECT_ARENA_TEXT_Y: i16 = SELECT_ARENA_Y + 4;
const SELECT_RULE_Y: i16 = 208;
const SELECT_RULE_TEXT_Y: i16 = SELECT_RULE_Y + 4;
const SELECT_HINT_Y: i16 = 229;
const SELECT_ARROW: i16 = 52;
/// Same inset as the per-seat arrows: the shared bars are the same width as
/// the player cards, so their arrows sit at the same distance.
const SELECT_SHARED_ARROW: i16 = SELECT_ARROW;
const ROW_CAR: usize = 0;
const ROW_PAINT: usize = 1;
const ROW_ARENA: usize = 2;
const ROW_RULE: usize = 3;

/// Match rules offered by the shared pre-match option. One condition at a
/// time: the goal choices have no clock and the time choices have no score
/// cap. Five minutes remains the default players already know.
const MATCH_RULES: [WinCondition; 6] = [
    WinCondition::TimeLimit(60 * 60),
    WinCondition::TimeLimit(3 * 60 * 60),
    WinCondition::TimeLimit(5 * 60 * 60),
    WinCondition::GoalLimit(1),
    WinCondition::GoalLimit(3),
    WinCondition::GoalLimit(5),
];
const MATCH_RULE_NAMES: [&str; MATCH_RULES.len()] = [
    "TIME: 1:00",
    "TIME: 3:00",
    "TIME: 5:00",
    "FIRST TO: 1",
    "FIRST TO: 3",
    "FIRST TO: 5",
];
const DEFAULT_MATCH_RULE: usize = 2;

/// The front-end rows, in order. There used to be a VERSUS row and a GARAGE
/// row as well. Versus is decided by whether a second pad answers, which is a
/// fact about the room rather than something to ask about; the garage was the
/// select screen with one seat on it.
const MENU: [&str; draw::MENU_ROWS] = ["MATCH", "PRACTICE", "SETTINGS"];

/// Index of the row that parks the opponent instead of driving it.
const ROW_PRACTICE: usize = 1;
/// Index of the row that opens the settings panel instead of a match.
const ROW_SETTINGS: usize = 2;

/// One entry in the in-match pause menu. Which of these are offered depends on
/// the match: swapping sides means nothing with one pad on one screen. The
/// presentation toggles that used to sit here directly live in the settings
/// panel now, shared with the front end.
#[derive(Clone, Copy, PartialEq)]
enum PauseRow {
    Resume,
    Swap,
    Settings,
    Quit,
}

/// One entry in the settings panel, reached from the title menu and the pause
/// menu alike. One list, so the two places cannot drift apart.
#[derive(Clone, Copy, PartialEq)]
enum SettingsRow {
    Arena,
    Sound,
    Music,
    Track,
    Back,
}

/// Analog re-assert attempts per plug-in before a pad is taken as
/// digital-only (the boot handshake in `init` is on top of these).
const ANALOG_ATTEMPTS: u8 = 4;

struct NitroXide {
    /// Frames since boot, used only to pace the analog re-assert.
    analog_retry: u32,
    /// Analog re-assert attempts left per port since it was last seen
    /// connected. A pad that stays digital after these is digital-only.
    analog_attempts: [u8; 2],
    /// Whether each port answered last frame, to re-arm the attempts when a
    /// pad is plugged in mid-session.
    pad_seen: [bool; 2],
    /// Wide display face: title, menu rows, the goal headline.
    display: Option<FontAtlas>,
    /// Compact face: score, clock, boost, captions.
    hud: Option<FontAtlas>,
    phase: Phase,
    sim: Sim,
    /// Which car each seat drives. Seat 0 defends -Z, seat 1 defends +Z.
    cars: [usize; 2],
    /// Highlighted menu row, and the tick it last moved on, which is what the
    /// highlight sweeps from.
    menu: usize,
    menu_at: u32,
    /// Triangle toggles between the ordinary chase camera and ball cam, per
    /// seat: in a split game each player owns their own view and one pad must
    /// not be able to swing the other's camera.
    ///
    /// Keep the ordinary car-facing camera as the boot default. Ball cam is a
    /// mode the player asks for, not an unconditional property of a match.
    ball_cam: [bool; 2],
    /// Split-screen versus: player two drives the orange car off port 2 and
    /// the AI stands down.
    two_player: bool,
    /// The match is held and the pause menu is up.
    paused: bool,
    /// Highlighted pause row, as an index into [`NitroXide::pause_rows`].
    pause_row: usize,
    /// The settings panel is open, holding its highlighted row. `None` when
    /// closed. One field serves the title menu and the pause menu both, since
    /// only one of them can have opened it.
    settings: Option<usize>,
    /// Player two takes the left half. Whoever is sitting on the left of the
    /// sofa should be looking at the left of the screen, and that is not
    /// something the pad they picked up should decide.
    swap_seats: bool,
    /// CD-DA borrowed from the disc this program booted from.
    music: music::Music,
    /// Which paint each seat wears. Appearance is not physics, so it lives
    /// here rather than on `sim::Car`.
    paints: [usize; 2],
    /// Shared arena lighting preset. Matches do not advance a world clock;
    /// this is an authored presentation choice that can change while paused.
    arena_time: draw::ArenaTime,
    /// Index into [`MATCH_RULES`], edited as the second shared select row.
    match_rule: usize,
    /// Highlighted row within each seat's panel.
    sel_row: [usize; 2],
    /// Which seats have locked their choice in. Only seats with a pad on them
    /// have to: nobody readies up on the CPU's behalf.
    ready: [bool; 2],
    /// Which panel pad one is editing. Fixed to seat 0 in a two-pad game,
    /// where pad two owns the other; L1 and R1 move it in a one-pad game so
    /// the player can dress the opponent.
    focus: usize,
    /// Seed for the opponent's opening car and colour, taken off the tick the
    /// select screen opened. There is no RNG on this hardware and none is
    /// wanted: the only thing that has to look unplanned is one draw.
    seed: u32,
    /// Ticks spent on the boot splash, driving its fade and its skip grace.
    intro_t: i32,
    profile: Profile<9, 0>,
    settings_dirty: bool,
}

impl NitroXide {
    fn new() -> Self {
        NitroXide {
            analog_retry: 0,
            analog_attempts: [ANALOG_ATTEMPTS; 2],
            pad_seen: [false; 2],
            display: None,
            hud: None,
            phase: Phase::Intro,
            intro_t: 0,
            sim: Sim::new(),
            cars: [0, 1],
            menu: 0,
            menu_at: 0,
            ball_cam: [false; 2],
            two_player: false,
            paused: false,
            pause_row: 0,
            settings: None,
            swap_seats: false,
            music: music::Music::new(),
            paints: [0, 5],
            arena_time: draw::ArenaTime::Night,
            match_rule: DEFAULT_MATCH_RULE,
            sel_row: [0; 2],
            ready: [false; 2],
            focus: 0,
            seed: 0,
            profile: Profile::new(DRIVE_ACTIONS),
            settings_dirty: false,
        }
    }

    /// The pause rows on offer, in order.
    fn pause_rows(&self) -> &'static [PauseRow] {
        if self.two_player {
            &[
                PauseRow::Resume,
                PauseRow::Swap,
                PauseRow::Settings,
                PauseRow::Quit,
            ]
        } else {
            &[PauseRow::Resume, PauseRow::Settings, PauseRow::Quit]
        }
    }

    /// The settings rows on offer, in order. The music rows only appear when
    /// the disc actually carries the tracks, rather than offering switches
    /// that do nothing.
    fn settings_rows(&self) -> &'static [SettingsRow] {
        if self.music.available() {
            &[
                SettingsRow::Arena,
                SettingsRow::Sound,
                SettingsRow::Music,
                SettingsRow::Track,
                SettingsRow::Back,
            ]
        } else {
            &[SettingsRow::Arena, SettingsRow::Sound, SettingsRow::Back]
        }
    }

    /// One tick of settings-panel input, from whichever menu opened it. The
    /// caller has already merged its pads into these edges; `back` is circle
    /// or start, and closes the panel back to that menu.
    fn settings_input(
        &mut self,
        up: bool,
        down: bool,
        left: bool,
        right: bool,
        cross: bool,
        back: bool,
        tick: u32,
    ) {
        let rows = self.settings_rows();
        let Some(mut row) = self.settings else {
            return;
        };
        if up {
            row = (row + rows.len() - 1) % rows.len();
        } else if down {
            row = (row + 1) % rows.len();
        }
        self.settings = Some(row);
        if back || (cross && rows[row] == SettingsRow::Back) {
            self.settings = None;
            self.persist_settings();
            return;
        }
        // X steps forward like the old pause toggles did; left and right step
        // both ways for the rows where direction means something.
        let step = if right || cross {
            1
        } else if left {
            -1
        } else {
            0
        };
        if step == 0 {
            return;
        }
        match rows[row] {
            SettingsRow::Arena => {
                self.arena_time = if step > 0 {
                    self.arena_time.next()
                } else {
                    self.arena_time.prev()
                };
                draw::set_arena_time(self.arena_time);
            }
            SettingsRow::Sound => {
                audio::set_muted(!audio::muted());
                self.profile.sfx_volume = if audio::muted() { 0 } else { 100 };
                self.settings_dirty = true;
            }
            SettingsRow::Music => {
                let on = !self.music.enabled();
                self.music.set_enabled(on, tick);
                self.profile.music_volume = if on { 100 } else { 0 };
                self.settings_dirty = true;
            }
            SettingsRow::Track => self.music.cycle_track(step, tick),
            SettingsRow::Back => {}
        }
    }

    /// One pad's state, this tick and last, into one tick of intent. D-pad and
    /// left stick both steer; the stick wins when it is pushed, so either
    /// controller works without a menu. Both
    /// players go through this: a split-screen game where the two seats do not
    /// drive identically is a bug waiting to be reported as one.
    fn read_pad(map: &ActionMap<9>, deadzone: Deadzone, pad: &PadState, prev: &PadState) -> Input {
        let actions = map.input(*pad, *prev);
        let held = |action: usize| actions.held(action);

        let mut steer = 0;
        if held(ACT_LEFT) {
            steer -= 128;
        }
        if held(ACT_RIGHT) {
            steer += 128;
        }
        // Nose down on a forward push, the way Rocket League has it. The pad
        // reports a stick pushed forward as a value below centre, so the axis
        // is negated. Airborne pitch and the dodge direction both read this;
        // neither reads the throttle, because the throttle is a trigger you
        // hold the whole time you are driving.
        let mut pitch = 0;
        if held(ACT_UP) {
            pitch += 128;
        }
        if held(ACT_DOWN) {
            pitch -= 128;
        }
        let (sx, sy) = pad.sticks.left_centered();
        if let Some(v) = deadzone.scaled_axis(sx) {
            steer = v as i32; // -127..=127 already, the sim's steer range
        }
        if let Some(v) = deadzone.scaled_axis(sy) {
            pitch = -(v as i32);
        }

        let mut throttle = 0;
        if held(ACT_THROTTLE) {
            throttle += 128;
        }
        if held(ACT_REVERSE) {
            throttle -= 128;
        }

        Input {
            throttle,
            steer: steer.clamp(-128, 128),
            pitch: pitch.clamp(-128, 128),
            boost: held(ACT_BOOST),
            jump_pressed: actions.pressed(ACT_JUMP),
            // Held as well as tapped: the sim extends a jump for up to a fifth
            // of a second while this is down, which is what makes jump height
            // something the player controls.
            jump_held: held(ACT_JUMP),
            // Held, not tapped: the modifier that turns steer into roll.
            // L1 is both powerslide and air roll, the way Rocket League binds
            // it: on the ground it breaks the back end loose, in the air it
            // rolls. One button, and which it means is decided by the wheels.
            air_roll: held(ACT_AIR_ROLL),
            handbrake: held(ACT_AIR_ROLL),
        }
    }

    fn persist_settings(&mut self) {
        if !self.settings_dirty {
            return;
        }
        if psx_settings::save_slot_one(SETTINGS_FILE, SETTINGS_TITLE, &self.profile).is_ok() {
            self.settings_dirty = false;
        }
    }

    /// Kick off the match the select screen just dressed. Two pads make it a
    /// versus game; one pad makes it a match against the AI, or free play with
    /// the opponent parked if PRACTICE was the row.
    fn start_match(&mut self) {
        self.sim = Sim::with_win_condition(MATCH_RULES[self.match_rule]);
        self.sim.opponent_ai = self.menu != ROW_PRACTICE && !self.two_player;
        self.ball_cam = [false; 2];
        self.paused = false;
        self.pause_row = 0;
        draw::set_seat_paints(self.paints);
        self.phase = Phase::Play;
    }

    /// Back to the front end, with the highlight sweeping in from scratch.
    fn to_title(&mut self, tick: u32) {
        self.menu_at = tick;
        self.settings = None;
        self.phase = Phase::Title;
    }

    /// One seat on the select screen rather than two: PRACTICE dressed alone
    /// has no opponent to preview, so the CPU panel would be noise. A second
    /// pad turns practice into a two-player match, seats and all.
    fn select_solo(&self) -> bool {
        !self.two_player && self.menu == ROW_PRACTICE
    }

    /// Open the select screen: nobody ready, and an opponent drawn out of the
    /// tick the screen opened on.
    fn open_select(&mut self, tick: u32) {
        self.ready = [false; 2];
        self.sel_row = [0; 2];
        self.focus = 0;
        self.seed = tick;
        // One multiply-and-shift each, off the same seed. This is the whole
        // randomness budget: a car and a colour that are not the same two
        // every time the screen opens.
        let n = tick.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        self.cars[1] = (n >> 16) as usize % draw::CAR_COUNT;
        self.paints[1] = (n >> 24) as usize % draw::PAINT_COUNT;
        if self.paints[1] == self.paints[0] {
            self.paints[1] = (self.paints[1] + 1) % draw::PAINT_COUNT;
        }
        self.phase = Phase::Select;
    }

    /// Park the cars where the front-end camera is looking, and turn them.
    ///
    /// The front end draws the arena through the match renderer, so a staged
    /// car is an ordinary sim car standing still: no separate showroom scene,
    /// no second lighting rig, and the same mesh the match will use. The sim is
    /// not ticked in these phases, so writing the pose every frame is the whole
    /// animation.
    fn stage_cars(&mut self, tick: u32, pair: bool) {
        // Three-quarter view that drifts rather than spins, so the flank and
        // the nose are both readable at rest.
        let spin = (1500 + ((tick * 3) & 0xFFF) / 5) as u16;
        for seat in 0..2 {
            let car = if seat == 0 {
                &mut self.sim.car
            } else {
                &mut self.sim.opponent
            };
            car.p.x = nitroxide_sim::uu(draw::stage_car_x(seat, pair));
            car.p.y = nitroxide_sim::uu(nitroxide_sim::CAR_REST_Y);
            car.p.z = nitroxide_sim::uu(if pair || seat == 0 {
                draw::STAGE_Z
            } else {
                // Behind the camera rather than hidden by a flag: the cull
                // already throws away what is not in frame.
                draw::STAGE_Z - 3000
            });
            car.yaw = spin;
            car.v = nitroxide_sim::V3::ZERO;
            car.grounded = true;
            car.steer = 0;
            car.boosting = false;
            car.suspension = [0; 2];
        }
        // The ball would sit between the two staged cars, which is where the
        // eye should be. Park it behind the camera with the spare car.
        self.sim.ball.p.z = nitroxide_sim::uu(draw::STAGE_Z - 3000);
        self.sim.ball.v = nitroxide_sim::V3::ZERO;
    }

    /// Step a seat's paint, stepping over whatever the other seat is wearing.
    /// With eight paints and two seats there is always somewhere to land, so
    /// this cannot loop: no confirm-time rejection, no clash to report.
    fn cycle_paint(&mut self, seat: usize, step: i32) {
        let n = draw::PAINT_COUNT as i32;
        let taken = self.paints[1 - seat];
        let mut next = self.paints[seat] as i32;
        loop {
            next = (next + step + n) % n;
            if next as usize != taken {
                break;
            }
        }
        self.paints[seat] = next as usize;
    }

    /// True when every seat that has a pad on it has locked in.
    fn all_ready(&self) -> bool {
        self.ready[0] && (!self.two_player || self.ready[1])
    }

    // ---- HUD ---------------------------------------------------------------

    /// Centres of the two team blocks, matching `draw::scoreboard`'s geometry.
    const SCORE_BLUE_X: i16 = 104;
    const SCORE_ORANGE_X: i16 = 216;

    /// One string with a hard shadow under it.
    ///
    /// Bare type on this hardware survives only while whatever is behind it
    /// stays dark. Put the ball over the crossbar and the score sits on grey
    /// wall instead. One offset copy costs a second draw and makes the type
    /// hold at any brightness, which is what every PS1 game with a readable
    /// HUD did.
    fn shadowed(font: &FontAtlas, x: i16, y: i16, text: &str, tint: (u8, u8, u8)) {
        font.draw_text(x + 1, y + 1, text, (0, 0, 0));
        font.draw_text(x, y, text, tint);
    }

    /// As `shadowed`, at an exact 2x. Q8 512 is whole-pixel doubling, not the
    /// fractional scaling that chewed the old title.
    fn shadowed_big(font: &FontAtlas, x: i16, y: i16, text: &str, tint: (u8, u8, u8)) {
        font.draw_text_scaled_q8(x + 1, y + 1, text, 512, 512, (0, 0, 0));
        font.draw_text_scaled_q8(x, y, text, 512, 512, tint);
    }

    fn draw_hud(&self, font: &FontAtlas) {
        let s = &self.sim;
        let mut dec = [0u8; U32_DEC_MAX];

        // Scores sit on the team blocks `draw::scoreboard` lays down, so the
        // colour does the work the BLU and ORG labels used to. Both are white:
        // tinting a digit that already sits on its team's colour only costs
        // contrast, which was the old HUD's problem, not its cure.
        // Centred by measuring, not by counting. This face is proportional
        // (a per-glyph advance table on top of a base of 10), so multiplying
        // the character count by a guessed cell width lands everything off
        // centre, most visibly on the clock where the colon is much narrower
        // than a digit.
        let blue = u32_dec(&mut dec, s.score_blue as u32);
        let bw = font.text_width(blue) as i16 * 2;
        Self::shadowed_big(font, Self::SCORE_BLUE_X - bw / 2, 3, blue, (255, 255, 255));
        let orange = u32_dec(&mut dec, s.score_orange as u32);
        let ow = font.text_width(orange) as i16 * 2;
        Self::shadowed_big(
            font,
            Self::SCORE_ORANGE_X - ow / 2,
            3,
            orange,
            (255, 255, 255),
        );

        // The centre states the active win condition. Timed matches keep the
        // familiar M:SS clock; first-to matches show their target instead of
        // a frozen or fake countdown.
        match s.win_condition {
            WinCondition::TimeLimit(_) => {
                let secs = s.clock / 60;
                let mut clock = [b'0'; 4];
                clock[0] = b'0' + (secs / 60) as u8;
                clock[1] = b':';
                clock[2] = b'0' + ((secs % 60) / 10) as u8;
                clock[3] = b'0' + ((secs % 60) % 10) as u8;
                let clock = unsafe { core::str::from_utf8_unchecked(&clock) };
                // Under a minute, and in the colour of trouble.
                let urgent = s.clock < 60 * 60;
                let tint = if urgent && (s.clock / 15) % 2 == 0 {
                    (255, 120, 96)
                } else if urgent {
                    (255, 196, 170)
                } else {
                    (226, 232, 244)
                };
                let cw = font.text_width(clock) as i16;
                Self::shadowed(font, draw::HUD_CENTRE_X - cw / 2, 8, clock, tint);
            }
            WinCondition::GoalLimit(target) => {
                let prefix = "TO ";
                let target = u32_dec(&mut dec, target as u32);
                let x = draw::HUD_CENTRE_X
                    - (font.text_width(prefix) + font.text_width(target)) as i16 / 2;
                Self::shadowed(font, x, 8, prefix, (226, 232, 244));
                Self::shadowed(
                    font,
                    x + font.text_width(prefix) as i16,
                    8,
                    target,
                    (255, 220, 120),
                );
            }
        }

        // Everything below here belongs to a seat rather than to the match, so
        // it repeats once per view and follows that view's geometry. One
        // player gets one pass over the whole screen.
        let seats: &[(draw::Viewport, &nitroxide_sim::Car, bool)] = if self.two_player {
            // Follows `swap_seats`, or the readouts sit over the other
            // player's dial.
            if self.swap_seats {
                &[
                    (draw::Viewport::TOP, &s.opponent, self.ball_cam[1]),
                    (draw::Viewport::BOTTOM, &s.car, self.ball_cam[0]),
                ]
            } else {
                &[
                    (draw::Viewport::TOP, &s.car, self.ball_cam[0]),
                    (draw::Viewport::BOTTOM, &s.opponent, self.ball_cam[1]),
                ]
            }
        } else {
            &[(draw::Viewport::FULL, &s.car, self.ball_cam[0])]
        };
        for &(vp, car, ball_cam) in seats {
            // Boost, as the number inside the dial `draw` puts on the right.
            let pips = (car.boost / BOOST_SCALE).clamp(0, BOOST_MAX_PIPS) as u32;
            let text = u32_dec(&mut dec, pips);
            let w = font.text_width(text) as i16;
            let cx = draw::boost_gauge_x(vp);
            let cy = draw::boost_gauge_y(vp);
            Self::shadowed(font, cx - w / 2, cy - 6, text, (255, 214, 120));

            if ball_cam {
                Self::shadowed(
                    font,
                    vp.x + 8,
                    vp.y + vp.h - 22,
                    "BALL CAM",
                    (146, 202, 255),
                );
            }
        }
    }

    fn draw_goal_banner(&self, font: &FontAtlas) {
        // Who scored, not what happened. `last_scorer` is the team credited,
        // so an own goal already reads as the other side scoring and there is
        // no separate case for it: the scoreboard is the thing that has to say
        // whose goal it was, and it does.
        let seat = match self.sim.last_scorer {
            Team::Blue => 0,
            Team::Orange => 1,
        };
        let text = self.seat_name(seat);
        let color = draw::seat_signal(seat);
        // The one place a bigger cell earns itself. Q8 scaling steps in
        // 1/256ths instead of whole pixels, so 1.75x keeps the strokes even.
        // Scales up over the first half-second, so it lands rather than
        // appears. Q8 scaling steps in 1/256ths, which is what makes a growing
        // headline possible at all on a bitmap font.
        let age = (nitroxide_sim::GOAL_FREEZE_TICKS - self.sim.goal_freeze) as i32;
        let grow = (256 + age * 16).min(448) as u16;
        // Measured rather than counted: the display face is proportional, so
        // a per-character estimate put a two-letter name and a three-letter one
        // in visibly different places.
        let w =
            (font.text_width(text) as i32 + font.text_width(" SCORED") as i32) * grow as i32 / 256;
        let x = (160 - w / 2) as i16;
        // High, in the stands. Centred it sat exactly where the celebration
        // camera puts the net, so the headline covered the explosion it was
        // announcing.
        const Y: i16 = 34;
        font.draw_text_scaled_q8(x, Y, text, grow, grow, color);
        let after = x + (font.text_width(text) as i32 * grow as i32 / 256) as i16;
        font.draw_text_scaled_q8(after, Y, " SCORED", grow, grow, (240, 240, 220));
    }

    /// What to call a seat on screen.
    fn seat_name(&self, seat: usize) -> &'static str {
        if seat == 0 {
            "P1"
        } else if self.two_player {
            "P2"
        } else {
            "CPU"
        }
    }

    /// Text over the front end. The panels and the car are 3D, drawn by
    /// `draw`; this only writes on top of them.
    fn draw_title(&self, font: &FontAtlas, _small: &FontAtlas, tick: u32) {
        // Animated title. Drawn as one string rather than letter by letter:
        // this font is proportional, so stepping a fixed advance leaves gaps
        // after narrow glyphs like I, which is what the first attempt did.
        // Cycling the gradient gets the movement without touching layout.
        let phase = ((tick / 2) & 0x3F) as i32;
        // Triangle wave, so the colour breathes rather than snapping at the
        // wrap. The gradient variants only take integer scale, and this face
        // wants 1.375x, so the animation lives in the colour instead.
        let wave = if phase < 32 { phase } else { 64 - phase };
        // Two offset copies under it, not one. The title sits over the pitch
        // now rather than over a dark plane, and a single one-pixel shadow at
        // 1.375x leaves the light strokes of this face reading as grass.
        for (dx, dy) in [(3, 3), (2, 2)] {
            font.draw_text_scaled_q8(10 + dx, 10 + dy, "NITROXIDE", 352, 352, (0, 0, 0));
        }
        font.draw_text_scaled_q8(
            10,
            10,
            "NITROXIDE",
            352,
            352,
            ((110 + wave * 4) as u8, (188 + wave) as u8, 255),
        );

        // Centred on the ink, not on the cell. `line_height` is fifteen rows
        // for this face and the caps fill only the top ten of them; the rest is
        // descender space no menu label uses, so centring the cell leaves an
        // all-caps row sitting three pixels high in its panel.
        let inset = (draw::MENU_ROW_H - DISPLAY_CAP_H) / 2;
        for (row, label) in MENU.iter().enumerate() {
            let lit = row == self.menu;
            // Nudged right on the upper rows to sit inside the shear.
            Self::shadowed(
                font,
                26,
                draw::menu_row_y(row) + inset,
                label,
                if lit {
                    (255, 255, 255)
                } else {
                    (148, 158, 182)
                },
            );
        }
    }

    /// One string centred on `cx`, with the same hard shadow `shadowed` gives.
    fn centred(font: &FontAtlas, cx: i16, y: i16, text: &str, tint: (u8, u8, u8)) {
        Self::shadowed(font, cx - font.text_width(text) as i16 / 2, y, text, tint);
    }

    /// The pause menu, over the held frame.
    ///
    /// The plate is a GP0 fill straight into the back buffer rather than an
    /// ordering-table quad: the table has already been submitted by the time
    /// the overlay runs, and in a split game it was submitted twice, so a
    /// panel spanning the seam has no single pass to belong to. A fill takes
    /// VRAM coordinates and ignores both the drawing area and the offset,
    /// which is why it needs `buffer_y` and why it can cross the seam at all.
    /// Its X and width are rounded to 16, which the command requires.
    fn draw_pause_menu(&self, display: &FontAtlas, font: &FontAtlas, buffer_y: u16) {
        const X: u16 = 64;
        const W: u16 = 192;
        let rows = self.pause_rows();
        let h = 56 + rows.len() as u16 * 16;
        let y = (draw::SCREEN_H as u16 - h) / 2;

        psx_gpu::fill_rect(X, buffer_y + y, W, h, 10, 12, 22);
        psx_gpu::fill_rect(X + 4, buffer_y + y + 4, W - 8, h - 8, 24, 28, 44);

        Self::shadowed(
            display,
            X as i16 + 46,
            y as i16 + 12,
            "PAUSED",
            (255, 255, 255),
        );

        for (i, row) in rows.iter().enumerate() {
            let label = match row {
                PauseRow::Resume => "RESUME",
                PauseRow::Swap => {
                    if self.swap_seats {
                        "SIDES: P2 P1"
                    } else {
                        "SIDES: P1 P2"
                    }
                }
                PauseRow::Settings => "SETTINGS",
                PauseRow::Quit => "QUIT TO MENU",
            };
            let lit = i == self.pause_row;
            let row_y = y as i16 + 40 + i as i16 * 16;
            if lit {
                Self::shadowed(font, X as i16 + 14, row_y, ">", (255, 210, 110));
            }
            Self::shadowed(
                font,
                X as i16 + 28,
                row_y,
                label,
                if lit {
                    (255, 255, 255)
                } else {
                    (148, 158, 182)
                },
            );
        }
    }

    /// The boot splash: black frame, the Bonnie Studios logo, and the "Built
    /// with PSoXide" sheen line. Ported from hl-psx's intro, drawn through
    /// this game's overlay pass instead of a pre-loop of its own.
    fn draw_intro(&self, font: &FontAtlas, buffer_y: u16) {
        let t = self.intro_t;
        let level = if t < INTRO_FADE_IN {
            t * 128 / INTRO_FADE_IN
        } else if t < INTRO_FADE_IN + INTRO_HOLD {
            128
        } else {
            (INTRO_TOTAL - t) * 128 / INTRO_FADE_OUT
        }
        .clamp(0, 128);

        psx_gpu::fill_rect(
            0,
            buffer_y,
            draw::SCREEN_W as u16,
            draw::SCREEN_H as u16,
            0,
            0,
            0,
        );
        // The 128px source logo drawn as the 96x96 mark the other games use.
        let l = level as u8;
        psx_gpu::draw_quad_textured(
            [(112, 34), (208, 34), (112, 130), (208, 130)],
            [(0, 0), (128, 0), (0, 128), (128, 128)],
            BONNIE_CLUT.uv_clut_word(),
            BONNIE_TPAGE.uv_tpage_word(0),
            (l, l, l),
        );

        // "Built with PSoXide", with the sweeping sheen.
        const TAG: &str = "Built with PSoXide";
        let span = TAG.chars().count() as i32 + 18;
        let head = (t / 2).rem_euclid(span);
        let mut x = draw::SCREEN_W / 2 - font.text_width(TAG) as i16 / 2;
        for (index, ch) in TAG.char_indices() {
            let glyph = &TAG[index..index + ch.len_utf8()];
            let amount = (18 - (index as i32 - head).abs() * 6).max(0);
            let channel = |base: i32| {
                let dim = base * level / 128;
                (dim + (level - dim) * amount / 18) as u8
            };
            font.draw_text(x, 150, glyph, (channel(76), channel(108), channel(128)));
            x += font.text_width(glyph) as i16;
        }
    }

    /// The settings panel, over whichever menu opened it. Same plate as the
    /// pause menu, wider: "TRACK: CHAINSAW HEART" is a longer row than
    /// anything the pause list carries.
    fn draw_settings(&self, display: &FontAtlas, font: &FontAtlas, buffer_y: u16) {
        const X: u16 = 48;
        const W: u16 = 224;
        let rows = self.settings_rows();
        let sel = self.settings.unwrap_or(0);
        let h = 56 + rows.len() as u16 * 16;
        let y = (draw::SCREEN_H as u16 - h) / 2;

        psx_gpu::fill_rect(X, buffer_y + y, W, h, 10, 12, 22);
        psx_gpu::fill_rect(X + 4, buffer_y + y + 4, W - 8, h - 8, 24, 28, 44);

        let title = "SETTINGS";
        Self::shadowed(
            display,
            X as i16 + (W as i16 - display.text_width(title) as i16) / 2,
            y as i16 + 12,
            title,
            (255, 255, 255),
        );

        for (i, row) in rows.iter().enumerate() {
            // Label and value drawn separately: the track name is not a
            // static string this `no_std` crate could concatenate.
            let (label, value) = match row {
                SettingsRow::Arena => (
                    "ARENA: ",
                    match self.arena_time {
                        draw::ArenaTime::Day => "DAY",
                        draw::ArenaTime::Sunset => "SUNSET",
                        draw::ArenaTime::Night => "NIGHT",
                    },
                ),
                SettingsRow::Sound => ("SOUND: ", if audio::muted() { "OFF" } else { "ON" }),
                SettingsRow::Music => ("MUSIC: ", if self.music.enabled() { "ON" } else { "OFF" }),
                SettingsRow::Track => ("TRACK: ", self.music.track_name()),
                SettingsRow::Back => ("BACK", ""),
            };
            let lit = i == sel;
            let row_y = y as i16 + 40 + i as i16 * 16;
            if lit {
                Self::shadowed(font, X as i16 + 14, row_y, ">", (255, 210, 110));
            }
            let tint = if lit {
                (255, 255, 255)
            } else {
                (148, 158, 182)
            };
            Self::shadowed(font, X as i16 + 28, row_y, label, tint);
            Self::shadowed(
                font,
                X as i16 + 28 + font.text_width(label) as i16,
                row_y,
                value,
                tint,
            );
        }
    }

    /// The now-playing popup, upper right, while a song's start is fresh.
    /// Below the scoreboard fascia so the match HUD keeps its corner.
    ///
    /// The plate is the title menu's fascia -- same shear, same gradient --
    /// anchored to the screen edge so only its sheared left end shows. It
    /// slides in from the right when a song starts and back out as the
    /// banner expires, with a spinning disc ahead of the type. Drawn
    /// immediate like the rest of the overlay: the ordering table is long
    /// submitted, and `leave_view` has already handed back the full-screen
    /// scissor, so screen coordinates land in the right buffer and the
    /// GPU clips the plate's off-screen end.
    fn draw_now_playing(&self, font: &FontAtlas, tick: u32) {
        let Some((name, elapsed)) = self.music.now_playing(tick) else {
            return;
        };
        const RIGHT: i16 = draw::SCREEN_W - 8;
        const Y: i16 = 30;
        /// Ticks the plate takes to slide in, and again to leave. The menu
        /// highlight's own sweep time, so the two animations feel related.
        const SLIDE: u32 = draw::MENU_SWEEP_TICKS;

        let tw = font.text_width("NOW PLAYING").max(font.text_width(name)) as i16;
        // Sheared left end, then the disc, then the type, right-aligned at
        // the same margin the old text-only banner used. The plate leads the
        // disc by enough that the shear never crowds it.
        let x0 = RIGHT - tw - 38;
        let y0 = Y - 5;
        let h = 30;

        // How far the plate still is from its seat: all the way out at the
        // banner's first tick, home by SLIDE, and out again over the last
        // SLIDE ticks. Everything drawn here shifts right by this.
        let travel = (draw::SCREEN_W - x0) as u32;
        let remaining = music::BANNER_TICKS - elapsed;
        let slide = if elapsed < SLIDE {
            travel * (SLIDE - elapsed) / SLIDE
        } else if remaining < SLIDE {
            travel * (SLIDE - remaining) / SLIDE
        } else {
            0
        };
        let off = slide as i16;

        // The menu row's plate: top edge leading by the shared shear, the
        // same shallow gradient, split into two Gouraud triangles because
        // the immediate API has no Gouraud quad. The right end runs past
        // the screen edge, so only the sheared edge reads as shape.
        let (top, bottom) = ((40, 46, 66), (24, 28, 44));
        let x1 = draw::SCREEN_W + draw::MENU_SLANT;
        let (tl, tr) = ((x0 + draw::MENU_SLANT + off, y0), (x1 + off, y0));
        let (bl, br) = ((x0 + off, y0 + h), (x1 + off, y0 + h));
        psx_gpu::draw_tri_gouraud([tl, tr, bl], [top, top, bottom]);
        psx_gpu::draw_tri_gouraud([bl, tr, br], [bottom, top, bottom]);

        Self::draw_disc(x0 + 23 + off, y0 + h / 2, tick);
        Self::shadowed(
            font,
            RIGHT - font.text_width("NOW PLAYING") as i16 + off,
            Y,
            "NOW PLAYING",
            (146, 202, 255),
        );
        Self::shadowed(
            font,
            RIGHT - font.text_width(name) as i16 + off,
            Y + 12,
            name,
            (255, 255, 255),
        );
    }

    /// A compact disc for the now-playing plate, drawn as immediate fans:
    /// silver body, two bright sheen wedges that carry the spin, a brighter
    /// hub ring, and the spindle hole picked back out in the plate's colour.
    /// Geometry rather than a texture: at eighteen pixels a rotated texture
    /// shimmers, while a rotating wedge on a still circle stays clean, and
    /// it costs no VRAM or pack chunk.
    fn draw_disc(cx: i16, cy: i16, tick: u32) {
        /// Q12 angle per tick: one revolution every ~48 ticks, so about 0.8
        /// seconds -- quick enough to read as a spinning disc, slow enough
        /// that the 30 Hz overlay shows the sweep rather than strobing.
        const SPIN: u32 = 85;
        /// Twelve segments keep an r=9 fan within a quarter pixel of round.
        const SEGS: u32 = 12;
        const R_BODY: i32 = 9;

        let point = |a: u16, r: i32| {
            (
                (cx as i32 + ((cos_q12(a) * r + 2048) >> 12)) as i16,
                (cy as i32 + ((sin_q12(a) * r + 2048) >> 12)) as i16,
            )
        };
        let fan = |r: i32, segs: u32, tint: (u8, u8, u8)| {
            for i in 0..segs {
                let a0 = (i * 4096 / segs) as u16;
                let a1 = ((i + 1) * 4096 / segs) as u16;
                psx_gpu::draw_tri_flat(
                    [(cx, cy), point(a0, r), point(a1, r)],
                    tint.0,
                    tint.1,
                    tint.2,
                );
            }
        };

        fan(R_BODY, SEGS, (170, 178, 196));
        // The sheen: two opposed wedges from the hub ring to the rim, the
        // only part that moves. Single quads; at this radius the chord of
        // a 40-degree arc is indistinguishable from the arc.
        let spin = (tick.wrapping_mul(SPIN) & 4095) as u16;
        for half in 0..2u16 {
            let a0 = spin.wrapping_add(half * 2048);
            let a1 = a0.wrapping_add(460);
            psx_gpu::draw_quad_flat(
                [
                    point(a0, 5),
                    point(a0, R_BODY),
                    point(a1, 5),
                    point(a1, R_BODY),
                ],
                244,
                248,
                255,
            );
        }
        fan(4, 8, (226, 232, 244));
        fan(2, 8, (24, 28, 44));
    }

    /// The select screen: one panel a seat, under the car that seat drives.
    /// Solo practice shows seat 0 alone.
    fn draw_select(&self, display: &FontAtlas, small: &FontAtlas, tick: u32) {
        Self::shadowed(display, 10, 6, "SELECT", (146, 202, 255));

        let solo = self.select_solo();
        for seat in 0..if solo { 1 } else { 2 } {
            // Centred on wherever the renderer actually put that seat's car,
            // rather than on a hand-tuned column that drifts the moment the
            // stage moves. Solo, the car stands at screen centre.
            let cx = if solo {
                draw::SCREEN_W / 2
            } else {
                draw::stage_car_screen_x(seat, true)
            };
            let owner = if seat == 0 {
                "P1"
            } else if self.two_player {
                "P2"
            } else {
                "CPU"
            };
            // Only a seat someone is editing gets a lit header. In a two-pad
            // game both are, because both pads are live at once.
            let live = self.two_player || seat == self.focus;
            let ready = self.ready[seat] && (self.two_player || seat == 0);
            Self::centred(
                small,
                cx,
                SELECT_TOP,
                if ready { "READY" } else { owner },
                match (ready, live) {
                    (true, _) => (140, 240, 160),
                    (_, true) => (255, 210, 110),
                    _ => (120, 130, 152),
                },
            );
            for row in 0..PLAYER_SELECT_ROWS {
                let y = SELECT_TOP + SELECT_TEXT_OFFSET + row as i16 * SELECT_ROW_STEP;
                let lit = live && !self.ready[seat] && row == self.sel_row[seat];
                let tint = if lit {
                    (255, 255, 255)
                } else {
                    (148, 158, 182)
                };
                let value = match row {
                    ROW_CAR => draw::CAR_NAMES[self.cars[seat].min(draw::CAR_COUNT - 1)],
                    ROW_PAINT => draw::PAINTS[self.paints[seat]].0,
                    _ => "",
                };
                // Arrows only on the row that acts on them, so the screen says
                // what left and right will do rather than hoping you try.
                if lit {
                    Self::shadowed(small, cx - SELECT_ARROW, y, "<", (255, 210, 110));
                    Self::shadowed(small, cx + SELECT_ARROW - 6, y, ">", (255, 210, 110));
                }
                Self::centred(small, cx, y, value, tint);
            }
        }

        // Arena and win condition belong to the match rather than either
        // seat. Both pads navigate to the shared rows and either can edit.
        let shared_lit = |row| {
            (0..2).any(|seat| {
                let live = self.two_player || seat == self.focus;
                let ready = self.ready[seat] && (self.two_player || seat == 0);
                live && !ready && self.sel_row[seat] == row
            })
        };
        let arena_lit = shared_lit(ROW_ARENA);
        let arena = match self.arena_time {
            draw::ArenaTime::Day => "ARENA: DAY",
            draw::ArenaTime::Sunset => "ARENA: SUNSET",
            draw::ArenaTime::Night => "ARENA: NIGHT",
        };
        let draw_shared = |y, label, lit| {
            if lit {
                Self::shadowed(
                    small,
                    draw::SCREEN_W / 2 - SELECT_SHARED_ARROW,
                    y,
                    "<",
                    (255, 210, 110),
                );
                Self::shadowed(
                    small,
                    draw::SCREEN_W / 2 + SELECT_SHARED_ARROW - 6,
                    y,
                    ">",
                    (255, 210, 110),
                );
            }
            Self::centred(
                small,
                draw::SCREEN_W / 2,
                y,
                label,
                if lit {
                    (255, 255, 255)
                } else {
                    (148, 158, 182)
                },
            );
        };
        draw_shared(SELECT_ARENA_TEXT_Y, arena, arena_lit);
        draw_shared(
            SELECT_RULE_TEXT_Y,
            MATCH_RULE_NAMES[self.match_rule],
            shared_lit(ROW_RULE),
        );

        if (tick >> 5) & 1 == 0 {
            let hint = if self.all_ready() {
                "STARTING"
            } else if self.two_player || solo {
                "X READY"
            } else {
                "L1/R1 SWAP  X READY"
            };
            Self::centred(
                small,
                draw::SCREEN_W / 2,
                SELECT_HINT_Y,
                hint,
                (240, 240, 160),
            );
        }
    }

    fn draw_results(&self, font: &FontAtlas, small: &FontAtlas) {
        let s = &self.sim;
        let (verdict, color) = if s.score_blue > s.score_orange {
            ("WINNER", (140, 240, 160))
        } else if s.score_blue < s.score_orange {
            ("DEFEAT", (240, 130, 130))
        } else {
            ("DRAW", (230, 230, 150))
        };
        font.draw_text_scaled_q8(96, 48, verdict, 448, 448, color);
        let mut dec = [0u8; U32_DEC_MAX];
        small.draw_text(84, 120, "GOALS", (160, 175, 205));
        small.draw_text(
            200,
            120,
            u32_dec(&mut dec, s.score_blue as u32),
            (245, 245, 190),
        );
        small.draw_text(84, 144, "CONCEDED", (160, 175, 205));
        small.draw_text(
            200,
            144,
            u32_dec(&mut dec, s.score_orange as u32),
            (245, 245, 190),
        );
        small.draw_text(92, 206, "PRESS START", (230, 230, 160));
    }
}

impl Scene for NitroXide {
    fn init(&mut self, _ctx: &mut Ctx) {
        // Ask both pads for analog mode up front. A DualShock boots in digital
        // and reports the sticks centred until told otherwise, so without this
        // the steering deadzone is reading a stick that never moves and the car
        // is D-pad only. VoXide has always done this; the retry in `update`
        // covers a pad that was not ready at boot or gets re-plugged.
        let _ = psx_pad::enable_analog_port1();
        let _ = psx_pad::enable_analog_port2();
        if let Ok(profile) = psx_settings::load_slot_one(SETTINGS_FILE) {
            self.profile = profile;
        }
        draw::setup();
        assert!(
            assets::load_arena_texture(),
            "arena texture missing or invalid in WORLD.PAK"
        );
        #[cfg(feature = "arena-day")]
        {
            self.arena_time = draw::ArenaTime::Day;
            draw::set_arena_time(self.arena_time);
        }
        #[cfg(feature = "arena-sunset")]
        {
            self.arena_time = draw::ArenaTime::Sunset;
            draw::set_arena_time(self.arena_time);
        }
        audio::init();
        audio::set_muted(self.profile.sfx_volume == 0);
        self.music.set_enabled(self.profile.music_volume != 0, 0);
        self.sim = Sim::new();
        self.display = Some(FontAtlas::upload(
            &KENNEY_ROCKET,
            DISPLAY_TPAGE,
            DISPLAY_CLUT,
        ));
        self.hud = Some(FontAtlas::upload(&KENNEY_PIXEL, HUD_TPAGE, HUD_CLUT));
        psx_vram::upload_16bpp(
            psx_vram::VramRect::new(BONNIE_TPAGE.x(), BONNIE_TPAGE.y(), 32, 128),
            &bonnie::COVER_BONNIE,
        );
        // Entry 0 opaque near-black: the logo's holes must cover the backdrop,
        // not show through as transparent texels.
        let mut clut = bonnie::BONNIE_CLUT;
        clut[0] = 0x0421;
        psx_vram::upload_16bpp(
            psx_vram::VramRect::new(BONNIE_CLUT.x(), BONNIE_CLUT.y(), 16, 1),
            &clut,
        );
        #[cfg(feature = "boot-play")]
        {
            self.phase = Phase::Play;
        }
        #[cfg(feature = "boot-split-play")]
        {
            self.phase = Phase::Play;
            self.two_player = true;
            self.sim.opponent_ai = false;
        }
        #[cfg(feature = "boot-hatch")]
        {
            self.cars[0] = 1;
        }
        #[cfg(feature = "boot-sprinter")]
        {
            self.cars[0] = 2;
        }
        #[cfg(feature = "boot-wheels")]
        {
            self.phase = Phase::Play;
            self.sim.opponent_ai = false;
            self.sim.car.p.z = nitroxide_sim::uu(-1800);
            self.sim.ball.p.x = nitroxide_sim::uu(3000);
            self.sim.opponent.p.x = nitroxide_sim::uu(-3000);
        }
        #[cfg(feature = "boot-goal")]
        {
            self.phase = Phase::Play;
            self.sim.opponent_ai = false;
            // Park it out of the way: left where it spawns it blocks the shot.
            self.sim.opponent.p.x = nitroxide_sim::uu(3000);
            self.sim.ball.v.z = 5500;
            psx_rt::tty::println("boot-goal: ball kicked");
        }
        // Ball parked in the air a short way in front of the car, which is the
        // only view that shows a shadow whole. On the ground the ball sits on
        // its own shadow and the car covers its own, so a normal frame shows
        // fringe pixels and nothing to judge.
        #[cfg(feature = "boot-airball")]
        {
            self.phase = Phase::Play;
            self.sim.opponent_ai = false;
            self.ball_cam[0] = true;
            self.sim.car.p.z = nitroxide_sim::uu(-2600);
            self.sim.ball.p.x = 0;
            self.sim.ball.p.y = nitroxide_sim::uu(560);
            self.sim.ball.p.z = nitroxide_sim::uu(-1500);
            self.sim.ball.grounded = false;
            self.sim.opponent.p.x = nitroxide_sim::uu(3000);
        }
        #[cfg(feature = "boot-roof")]
        {
            self.phase = Phase::Play;
            self.sim.opponent_ai = false;
            self.ball_cam[0] = true;
            self.sim.car.p.z = nitroxide_sim::uu(-1800);
            self.sim.ball.p.x = 0;
            self.sim.ball.p.y = nitroxide_sim::uu(1700);
            self.sim.ball.p.z = nitroxide_sim::uu(400);
            self.sim.ball.v = nitroxide_sim::V3::ZERO;
            self.sim.ball.grounded = false;
            self.sim.opponent.p.x = nitroxide_sim::uu(3000);
        }
        // Fixed camera stations for headless capture. Ball cam frames the
        // car-to-ball line, so both have to be placed: the ball says where
        // the camera looks, the car says where it looks from.
        #[cfg(feature = "boot-wall")]
        {
            self.phase = Phase::Play;
            self.sim.opponent_ai = false;
            self.sim.car.p.x = nitroxide_sim::uu(nitroxide_sim::HALF_X - nitroxide_sim::CAR_REST_Y);
            self.sim.car.p.y = nitroxide_sim::uu(620);
            self.sim.car.p.z = nitroxide_sim::uu(-1200);
            self.sim.car.up = nitroxide_sim::V3::new(-4096, 0, 0);
            self.sim.car.grounded = true;
            self.sim.ball.p.x = nitroxide_sim::uu(3900);
            self.sim.ball.p.z = nitroxide_sim::uu(600);
            self.sim.opponent.p.x = nitroxide_sim::uu(-3000);
        }
        #[cfg(feature = "boot-split-wall")]
        {
            self.phase = Phase::Play;
            self.two_player = true;
            self.sim.opponent_ai = false;
            self.ball_cam = [true; 2];
            self.sim.car.p.x = nitroxide_sim::uu(3300);
            self.sim.car.p.z = nitroxide_sim::uu(-1200);
            self.sim.opponent.p.x = nitroxide_sim::uu(3300);
            self.sim.opponent.p.z = nitroxide_sim::uu(1200);
            self.sim.ball.p.x = nitroxide_sim::uu(nitroxide_sim::HALF_X - 80);
            self.sim.ball.p.y = nitroxide_sim::uu(nitroxide_sim::CEIL / 2);
            self.sim.ball.p.z = 0;
            self.sim.ball.v = nitroxide_sim::V3::ZERO;
            self.sim.ball.grounded = false;
        }
        #[cfg(feature = "boot-corner")]
        {
            self.phase = Phase::Play;
            self.sim.opponent_ai = false;
            self.sim.car.p.x = nitroxide_sim::uu(-2900);
            self.sim.car.p.z = nitroxide_sim::uu(3900);
            self.sim.ball.p.x = nitroxide_sim::uu(-3500);
            self.sim.ball.p.z = nitroxide_sim::uu(4400);
            self.sim.opponent.p.x = nitroxide_sim::uu(3000);
        }
    }

    fn update(&mut self, ctx: &mut Ctx) {
        // Re-assert analog for a pad that missed the boot handshake or was
        // plugged in mid-session. The handshake is three config transactions
        // with spin-delays plus a poll, so it only runs for a port that is
        // connected and not yet analog, a bounded number of times per
        // plug-in: unconditionally every fifteen frames it was 4.7% of the
        // split route's instructions.
        self.analog_retry = self.analog_retry.wrapping_add(1);
        for port in 0..2 {
            let pad = ctx.pad_for(port);
            let connected = pad.is_connected();
            if connected && !self.pad_seen[port] {
                self.analog_attempts[port] = ANALOG_ATTEMPTS;
            }
            self.pad_seen[port] = connected;
            if self.analog_retry % 15 == 0
                && connected
                && !pad.is_analog()
                && self.analog_attempts[port] > 0
            {
                self.analog_attempts[port] -= 1;
                let _ = if port == 0 {
                    psx_pad::enable_analog_port1()
                } else {
                    psx_pad::enable_analog_port2()
                };
            }
        }
        // Outside the phase machine: the disc's music plays over the front
        // end and the match alike, and a pause holds the game, not the song.
        self.music.update(ctx.sim_tick.as_u32());
        match self.phase {
            Phase::Intro => {
                self.intro_t += 1;
                // Held-from-boot presses are ignored for a short grace, so
                // walking out of the launcher with X held does not blink the
                // splash away before it is seen.
                let skip = self.intro_t > 8
                    && (ctx.just_pressed(button::CROSS)
                        || ctx.just_pressed(button::CIRCLE)
                        || ctx.just_pressed(button::START));
                if skip || self.intro_t >= INTRO_TOTAL {
                    self.to_title(ctx.sim_tick.as_u32());
                }
            }
            Phase::Title => {
                // An open settings panel owns the pad until it is closed.
                if self.settings.is_some() {
                    self.settings_input(
                        ctx.just_pressed(button::UP),
                        ctx.just_pressed(button::DOWN),
                        ctx.just_pressed(button::LEFT),
                        ctx.just_pressed(button::RIGHT),
                        ctx.just_pressed(button::CROSS),
                        ctx.just_pressed(button::CIRCLE) || ctx.just_pressed(button::START),
                        ctx.sim_tick.as_u32(),
                    );
                    return;
                }
                // Up and down walk the rows; left and right change the car on
                // show from any row, since it is always on screen and gating
                // that behind the garage row would only add a step.
                if ctx.just_pressed(button::UP) {
                    self.menu = (self.menu + draw::MENU_ROWS - 1) % draw::MENU_ROWS;
                    self.menu_at = ctx.sim_tick.as_u32();
                } else if ctx.just_pressed(button::DOWN) {
                    self.menu = (self.menu + 1) % draw::MENU_ROWS;
                    self.menu_at = ctx.sim_tick.as_u32();
                } else if ctx.just_pressed(button::LEFT) {
                    self.cars[0] = (self.cars[0] + draw::CAR_COUNT - 1) % draw::CAR_COUNT;
                } else if ctx.just_pressed(button::RIGHT) {
                    self.cars[0] = (self.cars[0] + 1) % draw::CAR_COUNT;
                }
                if ctx.just_pressed(button::CROSS) || ctx.just_pressed(button::START) {
                    if self.menu == ROW_SETTINGS {
                        self.settings = Some(0);
                    } else {
                        self.open_select(ctx.sim_tick.as_u32());
                    }
                }
            }
            Phase::Select => {
                // Port 2 every frame, so plugging a pad in makes the second
                // panel come alive while the player is still looking at it.
                // The count is fixed at `start_match` and not touched again.
                ctx.refresh_second_pad();
                let was_two = self.two_player;
                self.two_player = ctx.pad_for(1).is_connected();
                if self.two_player != was_two {
                    // A pad arriving takes its own panel over, and a pad
                    // leaving hands it back. Either way nothing stays locked
                    // in on someone else's behalf.
                    self.focus = 0;
                    self.ready[1] = false;
                }
                let p2 = |b: u16| self.two_player && ctx.just_pressed_for(1, b);
                // Pad one always owns seat 0 in a two-pad game; alone it owns
                // whichever panel it last hopped to.
                let seat1 = if self.two_player { 0 } else { self.focus };
                for (seat, up, down, left, right, cross, circle) in [
                    (
                        seat1,
                        ctx.just_pressed(button::UP),
                        ctx.just_pressed(button::DOWN),
                        ctx.just_pressed(button::LEFT),
                        ctx.just_pressed(button::RIGHT),
                        ctx.just_pressed(button::CROSS),
                        ctx.just_pressed(button::CIRCLE),
                    ),
                    (
                        1,
                        p2(button::UP),
                        p2(button::DOWN),
                        p2(button::LEFT),
                        p2(button::RIGHT),
                        p2(button::CROSS),
                        p2(button::CIRCLE),
                    ),
                ] {
                    // The second entry is pad two's, and `p2` is already false
                    // without one, so a one-pad game runs the loop twice and
                    // does nothing the second time round.
                    if self.ready[seat] {
                        if circle {
                            self.ready[seat] = false;
                        }
                        continue;
                    }
                    if up {
                        self.sel_row[seat] = (self.sel_row[seat] + SELECT_ROWS - 1) % SELECT_ROWS;
                    } else if down {
                        self.sel_row[seat] = (self.sel_row[seat] + 1) % SELECT_ROWS;
                    }
                    // Left and right act on whichever row is lit. Wrapping both
                    // ways, so the end of a list is never a dead end.
                    let step = if right {
                        1
                    } else if left {
                        -1
                    } else {
                        0
                    };
                    if step != 0 {
                        match self.sel_row[seat] {
                            ROW_CAR => {
                                let n = draw::CAR_COUNT as i32;
                                self.cars[seat] =
                                    ((self.cars[seat] as i32 + step + n) % n) as usize;
                            }
                            ROW_PAINT => self.cycle_paint(seat, step),
                            ROW_ARENA => {
                                self.arena_time = if step > 0 {
                                    self.arena_time.next()
                                } else {
                                    self.arena_time.prev()
                                };
                                draw::set_arena_time(self.arena_time);
                            }
                            ROW_RULE => {
                                let n = MATCH_RULES.len() as i32;
                                self.match_rule =
                                    ((self.match_rule as i32 + step + n) % n) as usize;
                            }
                            _ => {}
                        }
                    }
                    if cross {
                        // A pad readies its own seat. Pad one's is seat 0 even
                        // when it is dressing the CPU panel, which is what
                        // makes X mean "start" in a one-pad game.
                        self.ready[if self.two_player { seat } else { 0 }] = true;
                    } else if circle {
                        self.to_title(ctx.sim_tick.as_u32());
                        return;
                    }
                }
                // Only pad one hops panels, and only when it is alone with a
                // CPU panel to hop to. Practice shows one seat, so there is
                // nowhere to go.
                if !self.select_solo()
                    && !self.two_player
                    && (ctx.just_pressed(button::L1) || ctx.just_pressed(button::R1))
                {
                    self.focus = 1 - self.focus;
                }
                if self.all_ready() {
                    self.start_match();
                }
            }
            Phase::Play if self.paused => {
                // Port 2 keeps being polled while held, so the pause menu can
                // be driven from either seat. Whoever called it is usually the
                // one who wants to change something.
                if self.two_player {
                    ctx.refresh_second_pad();
                }
                let rows = self.pause_rows();
                let p2 = |b: u16| self.two_player && ctx.just_pressed_for(1, b);
                // An open settings panel owns both pads until it is closed.
                // The edges are collected up front because the closure holds
                // a borrow the handler's `&mut self` cannot share.
                if self.settings.is_some() {
                    let up = ctx.just_pressed(button::UP) || p2(button::UP);
                    let down = ctx.just_pressed(button::DOWN) || p2(button::DOWN);
                    let left = ctx.just_pressed(button::LEFT) || p2(button::LEFT);
                    let right = ctx.just_pressed(button::RIGHT) || p2(button::RIGHT);
                    let cross = ctx.just_pressed(button::CROSS) || p2(button::CROSS);
                    let back = ctx.just_pressed(button::CIRCLE)
                        || p2(button::CIRCLE)
                        || ctx.just_pressed(button::START)
                        || p2(button::START);
                    self.settings_input(up, down, left, right, cross, back, ctx.sim_tick.as_u32());
                    return;
                }
                if ctx.just_pressed(button::UP) || p2(button::UP) {
                    self.pause_row = (self.pause_row + rows.len() - 1) % rows.len();
                } else if ctx.just_pressed(button::DOWN) || p2(button::DOWN) {
                    self.pause_row = (self.pause_row + 1) % rows.len();
                }
                if ctx.just_pressed(button::START) || p2(button::START) {
                    self.paused = false;
                } else if ctx.just_pressed(button::CROSS) || p2(button::CROSS) {
                    match rows[self.pause_row] {
                        PauseRow::Resume => self.paused = false,
                        // Stays on the menu: the world is still drawn behind
                        // it, so the swap is visible the moment it happens and
                        // can be put straight back if it went the wrong way.
                        PauseRow::Swap => self.swap_seats = !self.swap_seats,
                        PauseRow::Settings => self.settings = Some(0),
                        PauseRow::Quit => {
                            audio::stop_all();
                            self.paused = false;
                            self.to_title(ctx.sim_tick.as_u32());
                        }
                    }
                }
            }
            Phase::Play => {
                if ctx.just_pressed(button::START) {
                    audio::stop_all();
                    self.paused = true;
                    self.pause_row = 0;
                    return;
                }
                if ctx.just_pressed(button::TRIANGLE) {
                    self.ball_cam[0] = !self.ball_cam[0];
                }
                let actions = self.profile.actions;
                let deadzone = Deadzone::new(self.profile.move_deadzone as i16);
                let input = Self::read_pad(&actions, deadzone, &ctx.pad, &ctx.pad_prev);
                if self.two_player {
                    // The engine context owns both samples, but port 2 stays
                    // opt-in so solo play pays no second SIO transaction.
                    ctx.refresh_second_pad();
                    if ctx.just_pressed_for(1, button::TRIANGLE) {
                        self.ball_cam[1] = !self.ball_cam[1];
                    }
                    let p2 = Self::read_pad(
                        &actions,
                        deadzone,
                        &ctx.pad_for(1),
                        &ctx.previous_pad_for(1),
                    );
                    self.sim.tick_versus(&input, &p2);
                } else {
                    self.sim.tick(&input);
                }
                #[cfg(feature = "boot-roof")]
                {
                    // This is a camera station, not a physics scenario. Keep
                    // its look target against the roof after the sim tick so
                    // gravity cannot level the camera before a late HW or
                    // wireframe capture is taken.
                    self.sim.ball.p.x = 0;
                    self.sim.ball.p.y = nitroxide_sim::uu(1700);
                    self.sim.ball.p.z = nitroxide_sim::uu(400);
                    self.sim.ball.v = nitroxide_sim::V3::ZERO;
                    self.sim.ball.grounded = false;
                }
                #[cfg(feature = "boot-wheels")]
                {
                    // Hold a representative pose so a headless final frame
                    // proves all three local wheel transforms at once without
                    // presenting a tyre almost edge-on to the QA camera.
                    self.sim.car.p.x = 0;
                    self.sim.car.p.y = nitroxide_sim::uu(nitroxide_sim::CAR_REST_Y);
                    self.sim.car.p.z = nitroxide_sim::uu(-1800);
                    self.sim.car.v = nitroxide_sim::V3::ZERO;
                    self.sim.car.steer = -1100;
                    self.sim.car.wheel_spin = 384;
                    self.sim.car.suspension = [
                        (2 << nitroxide_sim::SUSPENSION_VISUAL_FP) as i16,
                        (-2 << nitroxide_sim::SUSPENSION_VISUAL_FP) as i16,
                    ];
                }
                audio::update(&self.sim);
                #[cfg(feature = "boot-goal")]
                if self.sim.goal_freeze == nitroxide_sim::GOAL_FREEZE_TICKS {
                    psx_rt::tty::println("boot-goal: GOAL, freeze started");
                }
                if self.sim.finished() {
                    audio::stop_all();
                    self.phase = Phase::Results;
                }
            }
            Phase::Results => {
                if ctx.just_pressed(button::START) || ctx.just_pressed(button::CROSS) {
                    self.to_title(ctx.sim_tick.as_u32());
                }
            }
        }
    }

    fn render(&mut self, ctx: &mut Ctx) {
        let tick = ctx.sim_tick.as_u32();
        // Where the back buffer starts in VRAM, which is what turns a
        // display-space viewport into the GPU's scissor rectangle.
        let buffer_y = ctx.fb.buffer_y(ctx.fb.drawing);
        // Keep the working colour tables in step before anything is drawn
        // from them. A frame that changes nothing does nothing here.
        for seat in 0..2 {
            draw::set_appearance(seat, self.cars[seat], self.paints[seat]);
        }
        // The front end stands in the arena, so the barrier at the foot of its
        // walls should already be wearing what the select screen has picked.
        // Idempotent: this only does work on the frame a colour moved.
        draw::set_seat_paints(self.paints);
        draw::set_arena_time(self.arena_time);
        match self.phase {
            // Nothing behind the splash: the overlay paints the whole frame.
            Phase::Intro => {}
            Phase::Title => {
                self.stage_cars(tick, false);
                draw::render_menu(
                    &self.sim,
                    self.cars,
                    draw::FrontPanels::Title(draw::MenuRows {
                        selected: self.menu,
                        rows: draw::MENU_ROWS,
                        fill: draw::menu_sweep(tick.wrapping_sub(self.menu_at)),
                    }),
                    false,
                    buffer_y,
                )
            }
            // The select screen stands both cars on the pitch and asks the
            // renderer for a mirrored fascia under each one's overlay text.
            // Practice has no opponent worth previewing, so it stages the one
            // car the way the title does and drops the CPU panel entirely.
            Phase::Select => {
                let pair = !self.select_solo();
                self.stage_cars(tick, pair);
                if !pair {
                    // The title parks its solo car right of centre to clear
                    // the row list; the solo select has no list, so the one
                    // car stands in the middle and its text centres with it.
                    self.sim.car.p.x = nitroxide_sim::uu(0);
                }
                draw::render_menu(
                    &self.sim,
                    self.cars,
                    draw::FrontPanels::Select(draw::SelectPanels {
                        top: SELECT_TOP,
                        row_step: SELECT_ROW_STEP,
                        arena_y: SELECT_ARENA_Y,
                        rule_y: SELECT_RULE_Y,
                        live: [
                            self.two_player || self.focus == 0,
                            self.two_player || self.focus == 1,
                        ],
                        ready: [self.ready[0], self.two_player && self.ready[1]],
                        selected: self.sel_row,
                        pair,
                    }),
                    pair,
                    buffer_y,
                )
            }
            // Keep the last world frame behind the results overlay.
            Phase::Play | Phase::Results => {
                if self.two_player {
                    draw::render_split(
                        &self.sim,
                        self.cars,
                        self.ball_cam,
                        buffer_y,
                        self.swap_seats,
                    )
                } else {
                    draw::render(&self.sim, self.cars, self.ball_cam[0], buffer_y)
                }
            }
        }
    }

    fn render_overlay(&mut self, ctx: &mut Ctx) {
        let display = self.display.as_ref().expect("display font");
        let hud = self.hud.as_ref().expect("hud font");
        match self.phase {
            Phase::Intro => {
                self.draw_intro(hud, ctx.fb.buffer_y(ctx.fb.drawing));
            }
            Phase::Title => {
                self.draw_title(display, hud, ctx.sim_tick.as_u32());
                if self.settings.is_some() {
                    self.draw_settings(display, hud, ctx.fb.buffer_y(ctx.fb.drawing));
                }
            }
            Phase::Select => self.draw_select(display, hud, ctx.sim_tick.as_u32()),
            Phase::Play => {
                self.draw_hud(hud);
                if self.sim.goal_freeze > 0 {
                    self.draw_goal_banner(display);
                }
                if self.paused {
                    if self.settings.is_some() {
                        self.draw_settings(display, hud, ctx.fb.buffer_y(ctx.fb.drawing));
                    } else {
                        self.draw_pause_menu(display, hud, ctx.fb.buffer_y(ctx.fb.drawing));
                    }
                }
            }
            Phase::Results => self.draw_results(display, hud),
        }
        // Over every phase: the music plays over the front end and the match
        // alike, so its announcement does too.
        self.draw_now_playing(hud, ctx.sim_tick.as_u32());
    }
}

#[no_mangle]
fn main() -> ! {
    // Sim every vblank, render every second one: 60 Hz control, 30 Hz picture.
    //
    // The default renders as fast as it can, which gives an uneven frame time
    // as the camera moves through the arena. For a car that changing
    // stick-to-picture delay feels worse than a steady rate. Queued submission
    // brings a representative match frame to about 779,000 cycles. The
    // detailed garage, including its denser foreground floor, is about
    // 1,043,000; two vblanks provide roughly 1,127,000, so both remain inside
    // the deadline while physics stays at the full 60 Hz.
    let config = Config {
        clear_color: (6, 8, 16),
        visual_pacing: VisualPacing::EveryNVBlanks(2),
        ..Config::default()
    };
    let mut game = NitroXide::new();
    App::run(config, &mut game);
}
