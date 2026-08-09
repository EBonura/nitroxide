// SPDX-License-Identifier: GPL-2.0-or-later
//! The paint job: what each named glTF material becomes on the console.
//!
//! The downloaded cars ship no textures. Every part is a flat `baseColorFactor`
//! on a material with a name that says what the part *is* (`windows`, `wheel`,
//! `stop_light`, `chrome`), and the cooker bakes those colours straight into
//! the PSXM face table. So the material names, not the colours, are the art
//! direction: this module throws the authored colours away and assigns a
//! console palette by role.
//!
//! Two things are going on here, and the second one matters more than the
//! first.
//!
//! **Colour.** The authored materials are photographic: a body at linear
//! (12, 10, 16), glass at (11, 17, 27), tyres at (3, 3, 3). Pushed through a
//! transfer curve they all land in the same dim band, which is why the car read
//! as one lozenge. The palette below is display-referred and deliberately
//! separated: near-black tyres, dark blue glass, a bumper light enough to be a
//! different object, lights bright enough to clip. Body colour is the one thing
//! the runtime overrides, so the two cars on the pitch are told apart by the
//! paints their drivers picked.
//!
//! **Which vertex wins.** The runtime draws Gouraud triangles from *per-vertex*
//! colours, and a vertex takes the colour of the first face in the table that
//! uses it (`draw.rs::build_car_materials`, mirroring the engine's own forward
//! scan). The body is the largest material and the glTF exporter writes it in
//! mesh order, so on most of these cars the body claimed nearly every shared
//! vertex and the tail lights, glass and grille were left with none. That is
//! the real reason the detail was invisible: it was in the face table and never
//! reached a vertex.
//!
//! [`Role::claim_order`] fixes it by sorting the face table so the small
//! high-contrast parts are written first. It costs nothing: same vertices, same
//! triangles, just a permutation. A light that claims its vertices bleeds into
//! the bodywork around it through the Gouraud interpolation, which at thirty
//! screen pixels is exactly what a lamp should do, right up until the panel it
//! bleeds into is the whole side of the car. [`Role::bleed_limit`] is where
//! that line is drawn.

/// What a part of the car is, independent of which model it came from.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Role {
    /// The main painted panels. Carries whichever paint the player picked;
    /// the colour below is only the key the runtime matches on.
    Body,
    /// Secondary bodywork: bed sides, rear quarter, the truck's box. A much
    /// darker tint of the body colour, so the flank has a break in it rather
    /// than being one field of paint.
    BodyDark,
    /// Glass. One material on every car, and the single most valuable one:
    /// a dark band across the cabin is what separates a car from a wedge.
    Glass,
    /// Tyre.
    Tyre,
    /// Wheel centre. On three of these cars `chrome` is the rim cylinder, on
    /// the trucks it is a small front cube, and a mid-dark grey suits both.
    Rim,
    /// Bumper, the bright band under the lights.
    Bumper,
    /// Grille and other dark inlets, above the bumper and below the bonnet.
    Grille,
    /// Headlight.
    Headlight,
    /// Rear light cluster.
    Taillight,
    /// Amber indicator, on the trucks only.
    Indicator,
    /// Chassis, sills, running boards: the dark mass a car sits on.
    Chassis,
}

impl Role {
    /// Classify a glTF material name.
    ///
    /// Blender's `.001` duplicate suffixes are stripped first, and an unnamed
    /// or absent material (one car has a side sill with no material at all)
    /// falls through to [`Role::Chassis`], which is where every one of them
    /// happens to be.
    pub fn of(name: &str) -> Role {
        let name = name.to_ascii_lowercase();
        let stem = match name.rfind('.') {
            Some(i) if name[i + 1..].chars().all(|c| c.is_ascii_digit()) => &name[..i],
            _ => &name[..],
        };
        match stem {
            "windows" => Role::Glass,
            "wheel" | "black" => Role::Tyre,
            "chrome" | "dark_chrome" => Role::Rim,
            "metal_gray" | "white" => Role::Bumper,
            "metal_dark_gray" => Role::Grille,
            "light" => Role::Headlight,
            "stop_light" => Role::Taillight,
            "turn_light" => Role::Indicator,
            "bege_gray_metal" => Role::Chassis,
            "metal_black" => Role::BodyDark,
            // Every car's main paint has its own material name, because the
            // source models are each a different colour. They all mean "body".
            "metal_dark_blue" | "metalyellow" | "metal_green" | "metal_light_orange"
            | "metal_red" => Role::Body,
            _ => Role::Chassis,
        }
    }

    /// Display-referred colour, pre-lighting.
    ///
    /// Measured off a captured frame rather than guessed: the engine's rig
    /// multiplies these by about 1.55x on a panel facing the key light and by
    /// about 0.45x on one facing away. So anything above ~160 in a channel
    /// clips, and a body colour that clips its strongest channel first loses
    /// its hue on exactly the panels you look at most. Everything that has to
    /// stay coloured is authored under that ceiling; the two lamps are the only
    /// roles deliberately above it, because a lamp that clips is a lamp.
    pub const fn colour(self) -> (u8, u8, u8) {
        match self {
            // The two roles the garage repaints. Held under the clip ceiling
            // so the highlight side keeps its hue instead of going white, and
            // authored as Rocket League blue because these are the exact bytes
            // `draw.rs` scans for: change either and the repaint silently
            // stops matching.
            Role::Body => (32, 80, 168),
            Role::BodyDark => (16, 34, 80),
            // Blue-grey rather than black: black glass on a black-shadowed
            // flank disappears, and a cool tint reads as a reflection.
            Role::Glass => (34, 44, 68),
            Role::Tyre => (16, 16, 20),
            // Bright enough to be a disc inside the arch, dark enough not to
            // be mistaken for bodywork.
            Role::Rim => (146, 148, 156),
            // Half the brightness it started at. The bumper's vertices sit on
            // the car's lower edge and are shared with the long flank panels,
            // so whatever colour it claims bleeds the length of the car: at
            // 170 it painted a chrome swoosh across the whole side.
            Role::Bumper => (152, 156, 164),
            Role::Grille => (30, 30, 34),
            Role::Headlight => (250, 240, 198),
            Role::Taillight => (240, 52, 36),
            Role::Indicator => (255, 148, 24),
            Role::Chassis => (40, 38, 38),
        }
    }

    /// Biggest neighbouring panel, in square uu, this role may bleed across
    /// when it claims a vertex. See `main.rs`'s `repaint`.
    ///
    /// Split by how big the part is meant to look, not by how important it is.
    /// A lamp is a point: bleeding it over a full-length panel turns it into a
    /// racing stripe, so it is held to panels around the size of the nose
    /// cluster. Glass and tyres are already broad dark masses and a soft edge
    /// on them reads as a tinted roof or a shadowed sill, which is free
    /// modelling, so they get to spread twice as far.
    pub const fn bleed_limit(self) -> i64 {
        match self {
            Role::Glass | Role::Tyre => 600,
            _ => 300,
        }
    }

    /// Sort key for the face table. Lower claims its vertices first.
    ///
    /// Order of the argument: the parts that are small and have to be seen go
    /// before the parts that are large and will be seen regardless. The body
    /// is last precisely because it is the majority of the mesh, and under the
    /// old exporter order it swallowed every shared vertex.
    pub const fn claim_order(self) -> u8 {
        match self {
            Role::Taillight => 0,
            Role::Headlight => 1,
            Role::Indicator => 2,
            Role::Grille => 3,
            Role::Glass => 4,
            Role::Tyre => 5,
            Role::Rim => 6,
            // Below the wheels on purpose. The bumper wraps the bottom corners
            // and shares vertices with both, and a wheel that loses its
            // vertices to the bumper stops being a wheel.
            Role::Bumper => 7,
            Role::Chassis => 8,
            Role::BodyDark => 9,
            Role::Body => 10,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blender_duplicate_suffixes_classify_the_same_as_the_original() {
        assert_eq!(Role::of("metal_black.001"), Role::of("metal_black"));
        assert_eq!(Role::of("turn_light.001"), Role::Indicator);
        assert_eq!(Role::of("metal_dark_gray.001"), Role::Grille);
    }

    #[test]
    fn a_dotted_name_that_is_not_a_duplicate_suffix_is_left_alone() {
        // Only trailing digits are a Blender duplicate marker.
        assert_eq!(Role::of("wheel.rear"), Role::Chassis);
    }

    #[test]
    fn an_unknown_material_falls_back_to_chassis_dark() {
        assert_eq!(Role::of(""), Role::Chassis);
        assert_eq!(Role::of("Material.042"), Role::Chassis);
    }

    /// The runtime garage repaints by matching these exact colours, because
    /// the cooker writes per-vertex colours and not per-vertex roles. That
    /// makes them an interface, not an implementation detail: change one here
    /// without changing `draw.rs`'s `BODY_KEY` / `BODY_DARK_KEY` and the
    /// garage silently stops repainting anything, with no build error and no
    /// failing test anywhere else.
    #[test]
    fn the_body_colours_are_the_keys_the_garage_repaints() {
        assert_eq!(Role::Body.colour(), (32, 80, 168), "draw.rs BODY_KEY must match");
        assert_eq!(
            Role::BodyDark.colour(),
            (16, 34, 80),
            "draw.rs BODY_DARK_KEY must match"
        );
    }

    #[test]
    fn the_body_claims_its_vertices_last() {
        // The whole point of the reorder. If this inverts, the small parts go
        // back to being invisible.
        let body = Role::Body.claim_order();
        for role in [
            Role::Taillight,
            Role::Headlight,
            Role::Indicator,
            Role::Glass,
            Role::Bumper,
            Role::Grille,
            Role::Tyre,
        ] {
            assert!(role.claim_order() < body, "{role:?} must outrank the body");
        }
    }

    /// Nothing but the two body roles may sit on the repaint keys. A part that
    /// happens to share those exact bytes would be repainted with the body,
    /// which is how a wheel or a windscreen ends up the colour of the paint.
    #[test]
    fn no_other_role_collides_with_a_repaint_key() {
        let keys = [Role::Body.colour(), Role::BodyDark.colour()];
        for role in [
            Role::Glass,
            Role::Tyre,
            Role::Rim,
            Role::Bumper,
            Role::Grille,
            Role::Headlight,
            Role::Taillight,
            Role::Indicator,
            Role::Chassis,
        ] {
            assert!(!keys.contains(&role.colour()), "{role:?} sits on a repaint key");
        }
    }
}
