// SPDX-License-Identifier: GPL-2.0-or-later
//! Cook low-poly car GLBs into `.psxm` blobs the game embeds.
//!
//! Cooks the downloaded car GLBs straight through PSoXide's `psxed-gltf`,
//! which bakes glTF material base colours into the PSXM face-colour table.
//! This wrapper then applies component-aware vertex clustering, repaints the
//! table (see `paint.rs`), and writes blue and orange variants of every car.
//!
//! `tools/bake_car_atlas.py` used to run first and sample ambient occlusion
//! into per-face materials. It is kept, and it works, but its output is no
//! longer used: see `DECIMATE_GRID` for the measurement that retired it.
//!
//! `.psxm` rather than `.psxmdl` on purpose. The textured model format carries
//! UVs and would take the atlas itself, but its runtime path is not working
//! here yet (see the `car-textured-model` branch); `.psxm` goes through the
//! engine's Gouraud pass, which is proven. Per-face colour holds the AO
//! perfectly well at thirty screen pixels.
//!
//! On top of the cooker this adds one thing it deliberately does not handle:
//! **placement**.
//!
//! `psxed-gltf` normalises every mesh to its own vertex centroid at unit
//! extent, which is the right default for "show me this model" but wrong for a
//! game object that has to stand at a known size on a known spot. Driving that
//! back down with a tiny `ActorTransform` scale would work, except the scale
//! folds into the GTE rotation matrix: a car at 1/68 scale gets rotation cells
//! of ~60 instead of ~4096, and you can watch it quantise as it turns.
//!
//! So after cooking, this refits the vertex table in place: uniform scale to a
//! target length in Rocket League uu, origin moved to the centre of the
//! bounding box in X/Z and to the wheels in Y. The blob then holds real game
//! units, the runtime draws it at 1.0x scale, and the rotation keeps all
//! twelve bits.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

use psxed_gltf::Config;

mod paint;

use paint::Role;

/// Cars to cook: `(source stem, target length in uu)`.
///
/// Sized to match the Octane's *bulk*, not its length. Fitting the length made
/// every car exactly 120 uu, the real hitbox, and they still read as toys:
/// these are realistically proportioned saloons at roughly half an Octane's
/// width, 52% as wide and 40% of the volume. Rocket League's cars are
/// deliberately stubby and wide. Scaling by the cube root of that volume gap,
/// about 1.36x, puts them in the right weight class on the pitch, and the
/// collision box in `sim` follows them up.
///
/// Length is the fit axis because these are realistically proportioned models
/// (about 2.6 long to 1 wide) rather than Rocket League's stubby 1.4 to 1, so
/// fitting any other axis distorts them. 163 uu matches the widened gameplay
/// hitbox (`sim::CAR_HALF_L * 2`); the trucks get a small visual overhang.
const CARS: [(&str, i32); 5] = [
    ("sedan", 163),
    ("hatchback", 163),
    ("hatchback2", 163),
    ("truck", 180),
    ("truck_2", 180),
];

/// Component-aware vertex-cluster grid.
///
/// PSoXide's importer offers a global cluster, but these cars are assemblies
/// of overlapping primitives. A global cell can contain a wheel, fender, lamp,
/// and chassis vertex and weld all of them to one centroid. That is what made
/// the first cooked cars grow long spikes and lose their wheels. Keeping the
/// connected-component id in the cluster key allows the same spatial budget
/// without joining unrelated parts.
const DECIMATE_GRID: Option<u32> = None;
const COMPONENT_GRID: u32 = 1024;

// PSXM layout, from `psxed-format/src/mesh.rs`:
//   AssetHeader 12 bytes, MeshHeader 8 bytes, then vert_count * 6 bytes of
//   i16 LE x/y/z in Q3.12. Everything after that is indices and colours, which
//   a rescale does not touch.
const ASSET_HEADER_LEN: usize = 12;
const MESH_HEADER_LEN: usize = 8;
const VERT_STRIDE: usize = 6;
const VERT_TABLE_OFFSET: usize = ASSET_HEADER_LEN + MESH_HEADER_LEN;
/// Per-vertex marker used by `.psxw` when the vertex is not part of a wheel.
const WHEEL_NONE: u8 = u8::MAX;

fn main() {
    let mut args = std::env::args().skip(1);
    let src = PathBuf::from(args.next().unwrap_or_else(|| usage()));
    let dst = PathBuf::from(args.next().unwrap_or_else(|| usage()));
    let emit_components = match args.next().as_deref() {
        None => false,
        Some("--components") => true,
        Some(_) => usage(),
    };
    if args.next().is_some() {
        usage();
    }

    std::fs::create_dir_all(&dst).expect("create asset dir");

    for (name, length) in CARS {
        let glb = src.join(format!("{name}.glb"));
        if !glb.exists() {
            eprintln!("skip {name}: {} not found", glb.display());
            continue;
        }
        let out = dst.join(format!("{name}.psxm"));
        match cook(&glb, length) {
            Ok((blob, components, wheels, stats)) => {
                std::fs::write(&out, &blob).expect("write psxm");
                if emit_components {
                    let component_out = dst.join(format!("{name}.psxc"));
                    std::fs::write(component_out, &components).expect("write psxc");
                    let wheel_out = dst.join(format!("{name}.psxw"));
                    std::fs::write(wheel_out, &wheels).expect("write psxw");
                }
                println!(
                    "{name:12} {:4} tris {:4} verts {:2} parts {:5} B  {:>4}x{:>3}x{:>3} uu  {}",
                    stats.faces,
                    stats.verts,
                    stats.components,
                    blob.len(),
                    stats.size.2,
                    stats.size.0,
                    stats.size.1,
                    // The vertex tally, not the face tally: a part that
                    // reaches no vertex is not in the picture, whatever the
                    // face table says. This is the line to read when a car
                    // comes out looking like a lozenge again.
                    stats.claims,
                );
            }
            Err(e) => eprintln!("{name}: {e}"),
        }
    }
}

fn usage() -> ! {
    eprintln!("usage: cook-models <glb-dir> <out-dir> [--components]");
    std::process::exit(2);
}

struct Stats {
    verts: u16,
    faces: u16,
    /// Final bounding size in uu, `(x, y, z)`.
    size: (i32, i32, i32),
    /// How many vertices each part ended up owning, filled in by `repaint`.
    claims: Claims,
    /// Disconnected geometry groups retained for component-aware PS1 sorting.
    components: u8,
}

/// Per-role vertex tally, formatted for the cook's one-line report.
#[derive(Default)]
struct Claims(Vec<(Role, usize)>);

impl std::fmt::Display for Claims {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (role, count) in &self.0 {
            write!(f, "{role:?}:{count} ")?;
        }
        Ok(())
    }
}

fn cook(glb: &Path, target_length: i32) -> Result<(Vec<u8>, Vec<u8>, Vec<u8>, Stats), String> {
    let mut face_components = components_by_face(glb)?;
    let cfg = Config {
        decimate_grid: DECIMATE_GRID,
        // The models carry named materials (metal_dark_blue, chrome, windows,
        // stop_light), so their base colours are the whole art direction. No
        // textures anywhere in these files, which suits a PS1 fine.
        use_material_colors: true,
        include_face_colors: true,
        // Per-vertex normals feed the engine's GTE lighting pass.
        include_normals: true,
        ..Config::default()
    };
    let mut blob = psxed_gltf::convert_path(glb, &cfg).map_err(|e| format!("{e:?}"))?;
    let mut stats = refit(&mut blob, target_length)?;
    repaint(
        &mut blob,
        &stats,
        &roles_by_colour(glb)?,
        &mut face_components,
    )?;
    let (verts, faces, components) =
        cluster_connected_components(&mut blob, COMPONENT_GRID, &face_components)?;
    stats.verts = verts;
    stats.faces = faces;
    let (verts, components) = split_vertices_by_face_colour(&mut blob, &components)?;
    stats.verts = verts;
    stats.components = components.iter().copied().max().map_or(0, |id| id + 1);
    stats.claims = tally_claims(&blob)?;
    let wheels = wheel_slots(&blob, &components)?;
    Ok((blob, components, wheels, stats))
}

/// Assign every cooked vertex to one of the four wheel corners, or
/// [`WHEEL_NONE`] for rigid bodywork.
///
/// PSXM deliberately discards glTF nodes. The `.psxc` table preserves their
/// component ids, while the repainted vertex colours preserve whether a
/// component is tyre/rim geometry. Combining those two facts after refitting
/// gives a stable wheel grouping in final object space:
///
/// * tyre/rim material;
/// * low in the car;
/// * away from both centre planes;
/// * the whole source component moves together, including tiny hub details.
///
/// Slots are `(rear-left, rear-right, front-left, front-right)`, with +Z the
/// nose and +X the right side. The renderer derives each pivot from the
/// vertices in its slot, so no per-model wheel coordinates are hard-coded.
fn wheel_slots(blob: &[u8], components: &[u8]) -> Result<Vec<u8>, String> {
    if blob.len() < VERT_TABLE_OFFSET || &blob[..4] != b"PSXM" {
        return Err("wheel map source is not PSXM".into());
    }
    let verts = u16::from_le_bytes([blob[12], blob[13]]) as usize;
    let faces = u16::from_le_bytes([blob[14], blob[15]]) as usize;
    if components.len() != verts {
        return Err("wheel map component table length mismatch".into());
    }
    let index_table = VERT_TABLE_OFFSET + verts * VERT_STRIDE;
    let colour_table = index_table + faces * 6;
    if blob.len() < colour_table + faces * 3 {
        return Err("wheel map source tables are truncated".into());
    }

    let mut minimum = [[i32::MAX; 3]; 256];
    let mut maximum = [[i32::MIN; 3]; 256];
    let mut count = [0u16; 256];
    let mut car_minimum = [i32::MAX; 3];
    let mut car_maximum = [i32::MIN; 3];
    for (vertex, &component) in components.iter().enumerate() {
        let component = component as usize;
        count[component] = count[component].saturating_add(1);
        for axis in 0..3 {
            let offset = VERT_TABLE_OFFSET + vertex * VERT_STRIDE + axis * 2;
            let value = i16::from_le_bytes([blob[offset], blob[offset + 1]]) as i32;
            minimum[component][axis] = minimum[component][axis].min(value);
            maximum[component][axis] = maximum[component][axis].max(value);
            car_minimum[axis] = car_minimum[axis].min(value);
            car_maximum[axis] = car_maximum[axis].max(value);
        }
    }

    let tyre = Role::Tyre.colour();
    let rim = Role::Rim.colour();
    let mut wheel_material = [false; 256];
    for face in 0..faces {
        let colour = (
            blob[colour_table + face * 3],
            blob[colour_table + face * 3 + 1],
            blob[colour_table + face * 3 + 2],
        );
        if colour != tyre && colour != rim {
            continue;
        }
        for corner in 0..3 {
            let offset = index_table + face * 6 + corner * 2;
            let vertex = u16::from_le_bytes([blob[offset], blob[offset + 1]]) as usize;
            if let Some(&component) = components.get(vertex) {
                wheel_material[component as usize] = true;
            }
        }
    }

    let car_size = [
        car_maximum[0] - car_minimum[0],
        car_maximum[1] - car_minimum[1],
        car_maximum[2] - car_minimum[2],
    ];
    let mut component_slot = [WHEEL_NONE; 256];
    let mut slots_found = [false; 4];
    for component in 0..256 {
        if count[component] == 0 || !wheel_material[component] {
            continue;
        }
        let centre = [
            (minimum[component][0] + maximum[component][0]) / 2,
            (minimum[component][1] + maximum[component][1]) / 2,
            (minimum[component][2] + maximum[component][2]) / 2,
        ];
        let low_and_outboard = centre[0].abs() * 4 >= car_size[0]
            && centre[2].abs() * 5 >= car_size[2]
            && centre[1] * 2 <= car_size[1];
        if !low_and_outboard {
            continue;
        }
        let slot = if centre[2] >= 0 { 2 } else { 0 } + if centre[0] >= 0 { 1 } else { 0 };
        component_slot[component] = slot;
        slots_found[slot as usize] = true;
    }
    if !slots_found.iter().all(|&found| found) {
        return Err(format!(
            "wheel map did not find all four corners: {slots_found:?}"
        ));
    }

    let slots: Vec<u8> = components
        .iter()
        .map(|&component| component_slot[component as usize])
        .collect();

    // A triangle must move as one rigid piece. If a future source model welds
    // a wheel to the body (or two wheel objects together), transforming only
    // part of that face would stretch it into the long spikes this sidecar is
    // specifically meant to avoid.
    for face in 0..faces {
        let mut face_slot = WHEEL_NONE;
        for corner in 0..3 {
            let offset = index_table + face * 6 + corner * 2;
            let vertex = u16::from_le_bytes([blob[offset], blob[offset + 1]]) as usize;
            let slot = slots.get(vertex).copied().unwrap_or(WHEEL_NONE);
            if slot != WHEEL_NONE {
                if face_slot != WHEEL_NONE && face_slot != slot {
                    return Err(format!(
                        "wheel face {face} crosses slots {face_slot} and {slot}"
                    ));
                }
                face_slot = slot;
            }
        }
        if face_slot != WHEEL_NONE {
            for corner in 0..3 {
                let offset = index_table + face * 6 + corner * 2;
                let vertex = u16::from_le_bytes([blob[offset], blob[offset + 1]]) as usize;
                if slots.get(vertex).copied().unwrap_or(WHEEL_NONE) != face_slot {
                    return Err(format!("wheel face {face} is welded to the rigid body"));
                }
            }
        }
    }

    Ok(slots)
}

/// One stable component id per source triangle, grouped by glTF node.
///
/// Blender exports each car object as a node. PSoXide appends those nodes'
/// primitives in scene order, but PSXM intentionally carries no node table.
/// Retaining the ids alongside the cook lets the menu identify a wheel,
/// chassis, window shell, or body as an assembly and apply role-specific depth
/// corrections without flattening all bodywork into one draw order.
fn components_by_face(glb: &Path) -> Result<Vec<u8>, String> {
    fn primitive_faces(primitive: &gltf::Primitive<'_>) -> Result<usize, String> {
        let indices = primitive
            .indices()
            .map_or_else(
                || primitive.get(&gltf::Semantic::Positions).map(|a| a.count()),
                |a| Some(a.count()),
            )
            .ok_or_else(|| "component source primitive has no positions".to_string())?;
        match primitive.mode() {
            gltf::mesh::Mode::Triangles => Ok(indices / 3),
            gltf::mesh::Mode::TriangleStrip | gltf::mesh::Mode::TriangleFan => {
                Ok(indices.saturating_sub(2))
            }
            mode => Err(format!("unsupported component source mode {mode:?}")),
        }
    }

    fn visit(node: gltf::Node<'_>, out: &mut Vec<u8>, next: &mut u16) -> Result<(), String> {
        if let Some(mesh) = node.mesh() {
            let component = u8::try_from(*next).map_err(|_| "car exceeded 255 source objects")?;
            *next += 1;
            for primitive in mesh.primitives() {
                out.extend(std::iter::repeat_n(component, primitive_faces(&primitive)?));
            }
        }
        for child in node.children() {
            visit(child, out, next)?;
        }
        Ok(())
    }

    let document = gltf::Gltf::open(glb)
        .map_err(|e| format!("read component source: {e}"))?
        .document;
    let mut out = Vec::new();
    let mut next = 0u16;
    if let Some(scene) = document.default_scene() {
        for node in scene.nodes() {
            visit(node, &mut out, &mut next)?;
        }
    } else {
        for scene in document.scenes() {
            for node in scene.nodes() {
                visit(node, &mut out, &mut next)?;
            }
        }
    }
    if out.is_empty() {
        for mesh in document.meshes() {
            let component = u8::try_from(next).map_err(|_| "car exceeded 255 source meshes")?;
            next += 1;
            for primitive in mesh.primitives() {
                out.extend(std::iter::repeat_n(component, primitive_faces(&primitive)?));
            }
        }
    }
    Ok(out)
}

/// Map every material's baked colour back to the part it belongs to.
///
/// `psxed-gltf` writes `baseColorFactor * 255` into the face table and keeps no
/// record of which material a face came from, so that quantised colour is the
/// only handle left. It is a usable one: no two materials in any of these five
/// files share a base colour. A primitive with no material at all cooks to the
/// glTF default white, which gets the same treatment under the key (255,255,255)
/// and lands on [`Role::Chassis`] with the rest of the unnamed parts.
fn roles_by_colour(glb: &Path) -> Result<HashMap<(u8, u8, u8), Role>, String> {
    let doc = gltf::Gltf::open(glb).map_err(|e| format!("read materials: {e}"))?;
    let mut out = HashMap::new();
    out.insert((255, 255, 255), Role::of(""));
    for material in doc.materials() {
        let f = material.pbr_metallic_roughness().base_color_factor();
        let key = (quantise(f[0]), quantise(f[1]), quantise(f[2]));
        let role = Role::of(material.name().unwrap_or(""));
        if let Some(previous) = out.insert(key, role) {
            if previous != role {
                return Err(format!(
                    "two materials share base colour {key:?}: {previous:?} and {role:?}"
                ));
            }
        }
    }
    Ok(out)
}

/// The same rounding `psxed-gltf` applies on the way into the face table, so
/// the keys match exactly.
fn quantise(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Replace the baked material colours with the console palette, then sort the
/// face table so the small parts claim their vertices before the body does.
///
/// Both halves are needed and the second is the one that was missing. See
/// `paint.rs` for why: the runtime shades per vertex, and a vertex takes the
/// first face colour that claims it, so a tail light that comes after the body
/// in the table contributes nothing at all to the picture.
///
/// The sort is in two tiers, and the second tier is what makes the first one
/// usable. Colouring a detail is paid for by its neighbours: a vertex has one
/// colour, so a red tail light on a vertex shared with a body panel drags that
/// panel red for its whole width. On a car decimated to ninety vertices the
/// panels are not all the same size and the difference decides everything. The
/// sedan's flank is a pair of 750-uu2 triangles running most of the length of
/// the car; its nose is a cluster of 150-uu2 ones. Letting the headlight claim
/// a vertex on the nose cluster paints a lamp; letting it claim the one vertex
/// it also shares with the flank painted a chrome swoosh from the bumper to the
/// door, which is exactly what the first cut of this looked like.
///
/// So a detail face goes in the first tier only if every vertex it would claim
/// is somewhere the bleed stays local (`Role::bleed_limit`). One that would
/// smear across a full-length panel is pushed behind the body instead and
/// quietly contributes nothing. That is the right trade: a lamp nobody can see
/// costs nothing, and a lamp smeared down the whole side costs the car its
/// shape.
///
/// A material the palette does not recognise keeps its authored colour, run
/// through [`linear_to_srgb`] as it always was, so an unclassified part is dim
/// but never invisible.
fn repaint(
    blob: &mut [u8],
    stats: &Stats,
    roles: &HashMap<(u8, u8, u8), Role>,
    face_components: &mut [u8],
) -> Result<Claims, String> {
    // Flags live in the AssetHeader at byte 6; bit 0 is HAS_FACE_COLORS.
    let flags = u16::from_le_bytes([blob[6], blob[7]]);
    if flags & 1 == 0 {
        return Ok(Claims::default());
    }
    let verts = stats.verts as usize;
    let faces = stats.faces as usize;
    if face_components.len() != faces {
        return Err(format!(
            "component face table has {} entries for {faces} faces",
            face_components.len()
        ));
    }
    let index_table = VERT_TABLE_OFFSET + verts * VERT_STRIDE;
    let colour_table = index_table + faces * 6;
    if blob.len() < colour_table + faces * 3 {
        return Err("face-colour table truncated".into());
    }

    let position = |v: usize| -> [i64; 3] {
        let mut p = [0i64; 3];
        for (axis, out) in p.iter_mut().enumerate() {
            let o = VERT_TABLE_OFFSET + v * VERT_STRIDE + axis * 2;
            *out = i16::from_le_bytes([blob[o], blob[o + 1]]) as i64;
        }
        p
    };

    // Read the table out, decide each face's role, then write it back in claim
    // order. Faces sharing a role and a tier keep their original order, which
    // keeps the cook reproducible.
    let cooked: Vec<Face> = (0..faces)
        .map(|f| {
            let mut index = [0u8; 6];
            index.copy_from_slice(&blob[index_table + f * 6..index_table + f * 6 + 6]);
            let corner = |k: usize| u16::from_le_bytes([index[k * 2], index[k * 2 + 1]]) as usize;
            let o = colour_table + f * 3;
            let baked = (blob[o], blob[o + 1], blob[o + 2]);
            let role = roles.get(&baked).copied();
            let colour = match role {
                Some(role) => role.colour(),
                None => (
                    linear_to_srgb(baked.0),
                    linear_to_srgb(baked.1),
                    linear_to_srgb(baked.2),
                ),
            };
            let corners = [corner(0), corner(1), corner(2)];
            let limit = role.map_or(0, Role::bleed_limit);
            Face {
                index,
                colour,
                role,
                order: role.map_or(u8::MAX, Role::claim_order),
                // Compared against `double_area`, which is `(2 * area)^2`.
                bleed_limit: (2 * limit) * (2 * limit),
                corners,
                double_area: double_area(
                    position(corners[0]),
                    position(corners[1]),
                    position(corners[2]),
                ),
            }
        })
        .collect();

    // Which faces meet at each vertex, so a face can ask how big the panels it
    // shares its corners with are.
    let mut incident = vec![Vec::new(); verts];
    for (f, face) in cooked.iter().enumerate() {
        for corner in face.corners {
            if corner < verts {
                incident[corner].push(f);
            }
        }
    }
    let contained: Vec<bool> = cooked
        .iter()
        .map(|face| {
            face.corners.iter().all(|&corner| {
                incident.get(corner).into_iter().flatten().all(|&other| {
                    // Only faces this one would win the vertex from can bleed.
                    cooked[other].order <= face.order
                        || cooked[other].double_area <= face.bleed_limit
                })
            })
        })
        .collect();

    let mut order: Vec<usize> = (0..faces).collect();
    order.sort_by_key(|&f| (!contained[f], cooked[f].order, f));
    let original_components = face_components.to_vec();

    // Replay the runtime's own forward scan while writing, so the tally is the
    // vertex ownership the console will actually see rather than a prediction.
    let mut owner: Vec<Option<Role>> = vec![None; verts];
    let mut claimed = vec![false; verts];
    for (slot, &f) in order.iter().enumerate() {
        let face = &cooked[f];
        blob[index_table + slot * 6..index_table + slot * 6 + 6].copy_from_slice(&face.index);
        let o = colour_table + slot * 3;
        blob[o] = face.colour.0;
        blob[o + 1] = face.colour.1;
        blob[o + 2] = face.colour.2;
        face_components[slot] = original_components[f];
        for corner in face.corners {
            if corner < verts && !claimed[corner] {
                claimed[corner] = true;
                owner[corner] = face.role;
            }
        }
    }

    let mut tally: Vec<(Role, usize)> = Vec::new();
    for role in owner.into_iter().flatten() {
        match tally.iter_mut().find(|(r, _)| *r == role) {
            Some((_, count)) => *count += 1,
            None => tally.push((role, 1)),
        }
    }
    tally.sort_by_key(|(role, _)| role.claim_order());
    Ok(Claims(tally))
}

/// Cluster each authored mesh component independently.
///
/// Hard normals can split one Blender object into many indexed islands, so
/// connectivity is not a reliable component identity. `face_components`
/// follows the glTF node table through the repaint sort and keeps every island
/// from one authored object together while still preventing overlapping
/// wheels, bodywork, glass, and lamps from welding to each other.
fn cluster_connected_components(
    blob: &mut Vec<u8>,
    grid: u32,
    face_components: &[u8],
) -> Result<(u16, u16, Vec<u8>), String> {
    if grid == 0 {
        return Err("component cluster grid must be non-zero".into());
    }
    if blob.len() < VERT_TABLE_OFFSET || &blob[0..4] != b"PSXM" {
        return Err("not a PSXM blob".into());
    }
    let version = u16::from_le_bytes([blob[4], blob[5]]);
    let flags = u16::from_le_bytes([blob[6], blob[7]]);
    if version != 2 || flags & 1 == 0 || flags & 2 == 0 {
        return Err("component cluster needs PSXM v2 colours and normals".into());
    }

    let old_vert_count = u16::from_le_bytes([blob[12], blob[13]]) as usize;
    let old_face_count = u16::from_le_bytes([blob[14], blob[15]]) as usize;
    if face_components.len() != old_face_count {
        return Err("component cluster face table length mismatch".into());
    }
    let old_vertex_end = VERT_TABLE_OFFSET + old_vert_count * VERT_STRIDE;
    let old_index_end = old_vertex_end + old_face_count * 6;
    let old_colour_end = old_index_end + old_face_count * 3;
    let old_normal_end = old_colour_end + old_vert_count * VERT_STRIDE;
    if blob.len() < old_normal_end {
        return Err("PSXM tables truncated before component cluster".into());
    }

    let vertices: Vec<[i16; 3]> = (0..old_vert_count)
        .map(|index| {
            let offset = VERT_TABLE_OFFSET + index * VERT_STRIDE;
            [
                i16::from_le_bytes([blob[offset], blob[offset + 1]]),
                i16::from_le_bytes([blob[offset + 2], blob[offset + 3]]),
                i16::from_le_bytes([blob[offset + 4], blob[offset + 5]]),
            ]
        })
        .collect();
    let faces: Vec<[u16; 3]> = (0..old_face_count)
        .map(|index| {
            let offset = old_vertex_end + index * 6;
            [
                u16::from_le_bytes([blob[offset], blob[offset + 1]]),
                u16::from_le_bytes([blob[offset + 2], blob[offset + 3]]),
                u16::from_le_bytes([blob[offset + 4], blob[offset + 5]]),
            ]
        })
        .collect();
    let colours: Vec<[u8; 3]> = (0..old_face_count)
        .map(|index| {
            let offset = old_index_end + index * 3;
            [blob[offset], blob[offset + 1], blob[offset + 2]]
        })
        .collect();

    let mut vertex_components = vec![u8::MAX; old_vert_count];
    for (face_index, face) in faces.iter().enumerate() {
        let [a, b, c] = face.map(usize::from);
        if a >= old_vert_count || b >= old_vert_count || c >= old_vert_count {
            return Err("PSXM face index outside vertex table".into());
        }
        let component = face_components[face_index];
        for vertex in [a, b, c] {
            match vertex_components[vertex] {
                u8::MAX => vertex_components[vertex] = component,
                previous if previous == component => {}
                _ => return Err("one imported vertex belongs to two source objects".into()),
            }
        }
    }
    if vertex_components.contains(&u8::MAX) {
        return Err("component source left an imported vertex unassigned".into());
    }

    let mut minimum = [i32::MAX; 3];
    let mut maximum = [i32::MIN; 3];
    for vertex in &vertices {
        for axis in 0..3 {
            minimum[axis] = minimum[axis].min(vertex[axis] as i32);
            maximum[axis] = maximum[axis].max(vertex[axis] as i32);
        }
    }
    let cell_of = |vertex: [i16; 3]| -> [u32; 3] {
        let mut cell = [0; 3];
        for axis in 0..3 {
            let span = (maximum[axis] - minimum[axis] + 1).max(1);
            let offset = vertex[axis] as i32 - minimum[axis];
            cell[axis] = ((offset * grid as i32) / span).clamp(0, grid as i32 - 1) as u32;
        }
        cell
    };

    type ClusterKey = (u8, u32, u32, u32);
    let mut sums: BTreeMap<ClusterKey, ([i64; 3], u32)> = BTreeMap::new();
    let mut keys = Vec::with_capacity(vertices.len());
    for (index, &vertex) in vertices.iter().enumerate() {
        let cell = cell_of(vertex);
        let key = (vertex_components[index], cell[0], cell[1], cell[2]);
        let entry = sums.entry(key).or_insert(([0; 3], 0));
        for axis in 0..3 {
            entry.0[axis] += vertex[axis] as i64;
        }
        entry.1 += 1;
        keys.push(key);
    }

    let mut clustered_vertices = Vec::with_capacity(sums.len());
    let mut clustered_components = Vec::with_capacity(sums.len());
    let mut cluster_index = BTreeMap::new();
    for (key, (sum, count)) in sums {
        let count = count as i64;
        let index = u16::try_from(clustered_vertices.len())
            .map_err(|_| "component cluster exceeded u16 vertices")?;
        clustered_vertices.push([
            (sum[0] / count) as i16,
            (sum[1] / count) as i16,
            (sum[2] / count) as i16,
        ]);
        clustered_components.push(key.0);
        cluster_index.insert(key, index);
    }

    let mut clustered_faces = Vec::new();
    let mut clustered_colours = Vec::new();
    let mut seen = HashSet::new();
    for (face, &colour) in faces.iter().zip(&colours) {
        let remapped = [
            cluster_index[&keys[face[0] as usize]],
            cluster_index[&keys[face[1] as usize]],
            cluster_index[&keys[face[2] as usize]],
        ];
        if remapped[0] == remapped[1] || remapped[1] == remapped[2] || remapped[0] == remapped[2] {
            continue;
        }
        let mut canonical = remapped;
        canonical.sort_unstable();
        if seen.insert((canonical, colour)) {
            clustered_faces.push(remapped);
            clustered_colours.push(colour);
        }
    }

    let mut normal_sums = vec![[0i64; 3]; clustered_vertices.len()];
    for face in &clustered_faces {
        let a = clustered_vertices[face[0] as usize].map(i64::from);
        let b = clustered_vertices[face[1] as usize].map(i64::from);
        let c = clustered_vertices[face[2] as usize].map(i64::from);
        let ab = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
        let ac = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
        let normal = [
            ab[1] * ac[2] - ab[2] * ac[1],
            ab[2] * ac[0] - ab[0] * ac[2],
            ab[0] * ac[1] - ab[1] * ac[0],
        ];
        for &index in face {
            for axis in 0..3 {
                normal_sums[index as usize][axis] += normal[axis];
            }
        }
    }
    let clustered_normals: Vec<[i16; 3]> = normal_sums
        .into_iter()
        .map(|normal| {
            let length = ((normal[0] * normal[0] + normal[1] * normal[1] + normal[2] * normal[2])
                as f64)
                .sqrt();
            if length == 0.0 {
                [0, 0x1000, 0]
            } else {
                normal.map(|axis| ((axis as f64 * 4096.0) / length).round() as i16)
            }
        })
        .collect();

    let new_vert_count = u16::try_from(clustered_vertices.len())
        .map_err(|_| "component cluster exceeded u16 vertices")?;
    let new_face_count =
        u16::try_from(clustered_faces.len()).map_err(|_| "component cluster exceeded u16 faces")?;
    let mut rebuilt = Vec::with_capacity(
        VERT_TABLE_OFFSET
            + clustered_vertices.len() * VERT_STRIDE
            + clustered_faces.len() * 9
            + clustered_normals.len() * VERT_STRIDE,
    );
    rebuilt.extend_from_slice(&blob[..VERT_TABLE_OFFSET]);
    rebuilt[12..14].copy_from_slice(&new_vert_count.to_le_bytes());
    rebuilt[14..16].copy_from_slice(&new_face_count.to_le_bytes());
    for vertex in clustered_vertices {
        for axis in vertex {
            rebuilt.extend_from_slice(&axis.to_le_bytes());
        }
    }
    for face in clustered_faces {
        for index in face {
            rebuilt.extend_from_slice(&index.to_le_bytes());
        }
    }
    for colour in clustered_colours {
        rebuilt.extend_from_slice(&colour);
    }
    for normal in clustered_normals {
        for axis in normal {
            rebuilt.extend_from_slice(&axis.to_le_bytes());
        }
    }
    let payload_len =
        u32::try_from(rebuilt.len() - ASSET_HEADER_LEN).map_err(|_| "PSXM payload too large")?;
    rebuilt[8..12].copy_from_slice(&payload_len.to_le_bytes());
    *blob = rebuilt;
    Ok((new_vert_count, new_face_count, clustered_components))
}

/// Split a welded vertex wherever adjacent faces use different materials.
///
/// PSXM stores colours per face, while the fast runtime path lights colours
/// per vertex. A welded body/window edge therefore cannot represent both the
/// blue paint and dark glass: whichever face is scanned first colours the
/// shared vertex for every neighbouring triangle. The old cook tried to pick
/// the least-bad winner; on a 90-vertex car that still erased most windows,
/// tyres, lamps, and grilles.
///
/// Duplicating only `(vertex, face colour)` pairs preserves the decimated
/// geometry and smooth normals while giving every material boundary its own
/// endpoints. Faces within one material still share vertices, so this is much
/// smaller than expanding every triangle to three independent vertices.
fn split_vertices_by_face_colour(
    blob: &mut Vec<u8>,
    old_components: &[u8],
) -> Result<(u16, Vec<u8>), String> {
    if blob.len() < VERT_TABLE_OFFSET || &blob[0..4] != b"PSXM" {
        return Err("not a PSXM blob".into());
    }
    let version = u16::from_le_bytes([blob[4], blob[5]]);
    if version != 2 {
        return Err("material split needs PSXM v2 indices".into());
    }
    let flags = u16::from_le_bytes([blob[6], blob[7]]);
    if flags & 1 == 0 || flags & 2 == 0 {
        return Err("material split needs face colours and vertex normals".into());
    }

    let old_verts = u16::from_le_bytes([blob[12], blob[13]]) as usize;
    if old_components.len() != old_verts {
        return Err("material split component table length mismatch".into());
    }
    let faces = u16::from_le_bytes([blob[14], blob[15]]) as usize;
    let old_vertex_end = VERT_TABLE_OFFSET + old_verts * VERT_STRIDE;
    let index_end = old_vertex_end + faces * 6;
    let colour_end = index_end + faces * 3;
    let normal_end = colour_end + old_verts * VERT_STRIDE;
    if blob.len() < normal_end {
        return Err("PSXM tables truncated before material split".into());
    }

    let old_vertices = blob[VERT_TABLE_OFFSET..old_vertex_end].to_vec();
    let old_indices = blob[old_vertex_end..index_end].to_vec();
    let colours = blob[index_end..colour_end].to_vec();
    let old_normals = blob[colour_end..normal_end].to_vec();

    let mut remap: HashMap<(u16, [u8; 3]), u16> = HashMap::new();
    let mut vertices = Vec::new();
    let mut normals = Vec::new();
    let mut components = Vec::new();
    let mut indices = Vec::with_capacity(old_indices.len());
    for face in 0..faces {
        let colour = [
            colours[face * 3],
            colours[face * 3 + 1],
            colours[face * 3 + 2],
        ];
        for corner in 0..3 {
            let o = face * 6 + corner * 2;
            let old = u16::from_le_bytes([old_indices[o], old_indices[o + 1]]);
            if old as usize >= old_verts {
                return Err("PSXM face index outside vertex table".into());
            }
            let key = (old, colour);
            let new = match remap.get(&key) {
                Some(&index) => index,
                None => {
                    let index = u16::try_from(vertices.len() / VERT_STRIDE)
                        .map_err(|_| "material split exceeded u16 vertices")?;
                    let v = old as usize * VERT_STRIDE;
                    vertices.extend_from_slice(&old_vertices[v..v + VERT_STRIDE]);
                    normals.extend_from_slice(&old_normals[v..v + VERT_STRIDE]);
                    components.push(old_components[old as usize]);
                    remap.insert(key, index);
                    index
                }
            };
            indices.extend_from_slice(&new.to_le_bytes());
        }
    }

    let new_verts = u16::try_from(vertices.len() / VERT_STRIDE)
        .map_err(|_| "material split exceeded u16 vertices")?;
    let mut rebuilt = Vec::with_capacity(
        VERT_TABLE_OFFSET + vertices.len() + indices.len() + colours.len() + normals.len(),
    );
    rebuilt.extend_from_slice(&blob[..VERT_TABLE_OFFSET]);
    rebuilt[12..14].copy_from_slice(&new_verts.to_le_bytes());
    rebuilt.extend_from_slice(&vertices);
    rebuilt.extend_from_slice(&indices);
    rebuilt.extend_from_slice(&colours);
    rebuilt.extend_from_slice(&normals);
    let payload_len = u32::try_from(rebuilt.len() - 12).map_err(|_| "PSXM payload too large")?;
    rebuilt[8..12].copy_from_slice(&payload_len.to_le_bytes());
    *blob = rebuilt;
    Ok((new_verts, components))
}

/// Reproduce the runtime's first-face material lookup for the cook report.
fn tally_claims(blob: &[u8]) -> Result<Claims, String> {
    let verts = u16::from_le_bytes([blob[12], blob[13]]) as usize;
    let faces = u16::from_le_bytes([blob[14], blob[15]]) as usize;
    let index_table = VERT_TABLE_OFFSET + verts * VERT_STRIDE;
    let colour_table = index_table + faces * 6;
    if blob.len() < colour_table + faces * 3 {
        return Err("PSXM tables truncated while tallying materials".into());
    }

    let roles = [
        Role::Taillight,
        Role::Headlight,
        Role::Indicator,
        Role::Grille,
        Role::Glass,
        Role::Tyre,
        Role::Rim,
        Role::Bumper,
        Role::Chassis,
        Role::BodyDark,
        Role::Body,
    ];
    let mut owner = vec![None; verts];
    for face in 0..faces {
        let o = colour_table + face * 3;
        let colour = (blob[o], blob[o + 1], blob[o + 2]);
        let role = roles.iter().copied().find(|role| role.colour() == colour);
        for corner in 0..3 {
            let i = index_table + face * 6 + corner * 2;
            let vertex = u16::from_le_bytes([blob[i], blob[i + 1]]) as usize;
            if vertex < verts && owner[vertex].is_none() {
                owner[vertex] = role;
            }
        }
    }

    let mut tally: Vec<(Role, usize)> = Vec::new();
    for role in owner.into_iter().flatten() {
        match tally.iter_mut().find(|(candidate, _)| *candidate == role) {
            Some((_, count)) => *count += 1,
            None => tally.push((role, 1)),
        }
    }
    tally.sort_by_key(|(role, _)| role.claim_order());
    Ok(Claims(tally))
}

/// One triangle, mid-repaint.
struct Face {
    /// Raw index-table bytes, moved as a unit.
    index: [u8; 6],
    colour: (u8, u8, u8),
    /// `None` for a material the palette does not recognise.
    role: Option<Role>,
    /// `Role::claim_order`, or `u8::MAX` for an unrecognised material.
    order: u8,
    /// `Role::bleed_limit`, already squared to match `double_area`.
    bleed_limit: i64,
    corners: [usize; 3],
    /// Twice the triangle's area, squared. Kept in that form so the whole
    /// comparison stays in integers.
    double_area: i64,
}

/// The squared length of the edge cross product: `(2 * area)^2`.
fn double_area(a: [i64; 3], b: [i64; 3], c: [i64; 3]) -> i64 {
    let u = [b[0] - a[0], b[1] - a[1], b[2] - a[2]];
    let w = [c[0] - a[0], c[1] - a[1], c[2] - a[2]];
    let cross = [
        u[1] * w[2] - u[2] * w[1],
        u[2] * w[0] - u[0] * w[2],
        u[0] * w[1] - u[1] * w[0],
    ];
    cross[0] * cross[0] + cross[1] * cross[1] + cross[2] * cross[2]
}

/// Black level the lift raises the darkest material to. Small now: the Blender
/// stage bakes ambient occlusion into these colours and floors it at 0.45, so
/// the darks are already meaningful shading rather than the near-black the raw
/// material factors were. Lifting hard on top of that flattens the car out.
const BLACK_LIFT: u32 = 12;

/// The sRGB transfer function plus that lift, on 8-bit values.
fn linear_to_srgb(v: u8) -> u8 {
    let l = v as f64 / 255.0;
    let s = if l <= 0.003_130_8 {
        12.92 * l
    } else {
        1.055 * l.powf(1.0 / 2.4) - 0.055
    };
    let srgb = (s * 255.0).round().clamp(0.0, 255.0) as u32;
    (BLACK_LIFT + srgb * (255 - BLACK_LIFT) / 255) as u8
}

/// Rescale and recentre a cooked blob's vertex table in place.
///
/// Origin lands at the bounding-box centre in X and Z, and at the lowest
/// vertex in Y, so a car's origin is where its wheels meet the ground. The
/// game then places it at `car.p.y - CAR_REST_Y` and never has to know
/// anything about how the mesh was authored.
fn refit(blob: &mut [u8], target_length: i32) -> Result<Stats, String> {
    if blob.len() < VERT_TABLE_OFFSET || &blob[0..4] != b"PSXM" {
        return Err("not a PSXM blob".into());
    }
    let verts = u16::from_le_bytes([blob[12], blob[13]]);
    let faces = u16::from_le_bytes([blob[14], blob[15]]);
    let table_len = verts as usize * VERT_STRIDE;
    if blob.len() < VERT_TABLE_OFFSET + table_len {
        return Err("vertex table truncated".into());
    }

    let read = |b: &[u8], i: usize, axis: usize| -> i32 {
        let o = VERT_TABLE_OFFSET + i * VERT_STRIDE + axis * 2;
        i16::from_le_bytes([b[o], b[o + 1]]) as i32
    };

    let mut min = [i32::MAX; 3];
    let mut max = [i32::MIN; 3];
    for i in 0..verts as usize {
        for axis in 0..3 {
            let v = read(blob, i, axis);
            min[axis] = min[axis].min(v);
            max[axis] = max[axis].max(v);
        }
    }
    let length = (max[2] - min[2]).max(1);
    // Fixed-point rather than float so the arithmetic matches what the console
    // would do with the same numbers, and so rounding is inspectable.
    let scale_q16 = (target_length << 16) / length;
    let centre = [(min[0] + max[0]) / 2, min[1], (min[2] + max[2]) / 2];

    for i in 0..verts as usize {
        for axis in 0..3 {
            let v = scale_round(read(blob, i, axis) - centre[axis], scale_q16);
            let v = i16::try_from(v).map_err(|_| "refit overflowed i16".to_string())?;
            let o = VERT_TABLE_OFFSET + i * VERT_STRIDE + axis * 2;
            blob[o..o + 2].copy_from_slice(&v.to_le_bytes());
        }
    }

    let size = |axis: usize| scale_round(max[axis] - min[axis], scale_q16);
    Ok(Stats {
        verts,
        faces,
        size: (size(0), size(1), size(2)),
        claims: Claims::default(),
        components: 0,
    })
}

/// Q16 scale, rounded to nearest and symmetric about zero. An arithmetic shift
/// rounds negatives toward minus infinity, which quietly makes a symmetric
/// model one unit wider on its left than its right.
fn scale_round(v: i32, scale_q16: i32) -> i32 {
    let p = v * scale_q16;
    if p >= 0 {
        (p + 32768) >> 16
    } else {
        -((-p + 32768) >> 16)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal PSXM: header, two vertices, one degenerate face.
    fn blob(verts: &[[i16; 3]]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(b"PSXM");
        b.extend_from_slice(&2u16.to_le_bytes()); // version
        b.extend_from_slice(&0u16.to_le_bytes()); // flags
        b.extend_from_slice(&0u32.to_le_bytes()); // payload_len, unused here
        b.extend_from_slice(&(verts.len() as u16).to_le_bytes());
        b.extend_from_slice(&0u16.to_le_bytes()); // face_count
        b.extend_from_slice(&0u32.to_le_bytes()); // reserved
        for v in verts {
            for a in v {
                b.extend_from_slice(&a.to_le_bytes());
            }
        }
        b
    }

    fn read_back(b: &[u8], i: usize) -> [i32; 3] {
        let mut out = [0; 3];
        for (axis, o) in out.iter_mut().enumerate() {
            let p = VERT_TABLE_OFFSET + i * VERT_STRIDE + axis * 2;
            *o = i16::from_le_bytes([b[p], b[p + 1]]) as i32;
        }
        out
    }

    fn coloured_blob(verts: &[[i16; 3]], faces: &[[u16; 3]], colours: &[[u8; 3]]) -> Vec<u8> {
        assert_eq!(faces.len(), colours.len());
        let mut b = Vec::new();
        b.extend_from_slice(b"PSXM");
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&3u16.to_le_bytes()); // colours + normals
        b.extend_from_slice(&0u32.to_le_bytes());
        b.extend_from_slice(&(verts.len() as u16).to_le_bytes());
        b.extend_from_slice(&(faces.len() as u16).to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes());
        for vertex in verts {
            for axis in vertex {
                b.extend_from_slice(&axis.to_le_bytes());
            }
        }
        for face in faces {
            for vertex in face {
                b.extend_from_slice(&vertex.to_le_bytes());
            }
        }
        for colour in colours {
            b.extend_from_slice(colour);
        }
        for _ in verts {
            b.extend_from_slice(&0i16.to_le_bytes());
            b.extend_from_slice(&4096i16.to_le_bytes());
            b.extend_from_slice(&0i16.to_le_bytes());
        }
        let payload_len = (b.len() - 12) as u32;
        b[8..12].copy_from_slice(&payload_len.to_le_bytes());
        b
    }

    #[test]
    fn material_boundaries_get_distinct_vertices() {
        let mut b = coloured_blob(
            &[[0, 0, 0], [10, 0, 0], [0, 10, 0], [10, 10, 0]],
            &[[0, 1, 2], [1, 3, 2]],
            &[[10, 20, 30], [40, 50, 60]],
        );
        let (count, components) = split_vertices_by_face_colour(&mut b, &[0, 0, 0, 0]).unwrap();
        assert_eq!(
            count, 6,
            "the two differently coloured triangles share no runtime vertices"
        );
        assert_eq!(components, [0; 6]);
        let index_table = VERT_TABLE_OFFSET + count as usize * VERT_STRIDE;
        let first = &b[index_table..index_table + 6];
        let second = &b[index_table + 6..index_table + 12];
        assert!(
            first
                .chunks_exact(2)
                .all(|a| second.chunks_exact(2).all(|z| a != z)),
            "a material boundary still shared a projected colour slot"
        );
        assert_eq!(
            u32::from_le_bytes([b[8], b[9], b[10], b[11]]) as usize,
            b.len() - 12,
            "rebuilt payload length should match the file"
        );
    }

    #[test]
    fn overlapping_disconnected_parts_never_share_a_cluster() {
        let mut b = coloured_blob(
            &[
                [0, 0, 0],
                [100, 0, 0],
                [0, 100, 0],
                [0, 0, 0],
                [100, 0, 0],
                [0, 100, 0],
            ],
            &[[0, 1, 2], [3, 4, 5]],
            &[[10, 20, 30], [10, 20, 30]],
        );
        let (verts, faces, components) = cluster_connected_components(&mut b, 2, &[0, 1]).unwrap();
        assert_eq!(verts, 6, "overlapping components were welded together");
        assert_eq!(faces, 2, "one overlapping component was discarded");
        assert_eq!(
            components.iter().copied().max().map_or(0, |id| id + 1),
            2,
            "overlapping islands lost their component identities"
        );
    }

    #[test]
    fn refit_scales_to_the_target_length() {
        // 4096 long on Z, off-centre on every axis.
        let mut b = blob(&[[1000, 500, -2048], [3000, 1500, 2048]]);
        let stats = refit(&mut b, 150).unwrap();
        assert_eq!(stats.size.2, 150, "length should hit the target exactly");
        let (lo, hi) = (read_back(&b, 0), read_back(&b, 1));
        assert_eq!(hi[2] - lo[2], 150);
    }

    #[test]
    fn refit_centres_x_and_z_but_sits_y_on_the_ground() {
        let mut b = blob(&[[1000, 500, -2048], [3000, 1500, 2048]]);
        refit(&mut b, 150).unwrap();
        let (lo, hi) = (read_back(&b, 0), read_back(&b, 1));
        assert_eq!(lo[0], -hi[0], "X should straddle the origin");
        assert_eq!(lo[2], -hi[2], "Z should straddle the origin");
        assert_eq!(lo[1], 0, "lowest vertex is the ground plane");
        assert!(hi[1] > 0);
    }

    #[test]
    fn srgb_encoding_lifts_dark_linear_colours() {
        // The sedan's body material bakes to linear (12, 10, 16), which is the
        // colour that made the first import render as a black cutout.
        assert_eq!(
            linear_to_srgb(0),
            BLACK_LIFT as u8,
            "black lifts to the floor"
        );
        assert_eq!(linear_to_srgb(255), 255, "white stays white");
        // What actually matters: a car has to be distinguishable from the
        // grass it stands on. The pitch's brightest channel is 62, and the
        // sedan's paint bakes from linear 12. Asserting that margin rather
        // than an exact value, so tuning BLACK_LIFT cannot break this test for
        // a reason unrelated to what it checks.
        assert!(
            linear_to_srgb(12) > 62,
            "the sedan's paint has to clear the pitch"
        );
        // Monotonic, and never darker than it started.
        for v in 0..=255u8 {
            assert!(linear_to_srgb(v) >= v, "{v} got darker");
        }
    }

    #[test]
    fn refit_rejects_a_non_psxm_blob() {
        let mut junk = vec![0u8; 64];
        assert!(refit(&mut junk, 150).is_err());
    }

    /// A PSXM with face colours: one small triangle and one that shares a
    /// corner with it, whose size the caller picks.
    fn two_face_blob(neighbour_reach: i16) -> (Vec<u8>, Stats) {
        let verts: [[i16; 3]; 5] = [
            [0, 0, 0],
            [4, 0, 0],
            [0, 4, 0],
            [0, 0, neighbour_reach],
            [neighbour_reach, 0, neighbour_reach],
        ];
        let mut b = Vec::new();
        b.extend_from_slice(b"PSXM");
        b.extend_from_slice(&2u16.to_le_bytes()); // version
        b.extend_from_slice(&1u16.to_le_bytes()); // flags: HAS_FACE_COLORS
        b.extend_from_slice(&0u32.to_le_bytes()); // payload_len, unused here
        b.extend_from_slice(&(verts.len() as u16).to_le_bytes());
        b.extend_from_slice(&2u16.to_le_bytes());
        b.extend_from_slice(&0u32.to_le_bytes()); // reserved
        for v in verts {
            for a in v {
                b.extend_from_slice(&a.to_le_bytes());
            }
        }
        for face in [[0u16, 1, 2], [0, 3, 4]] {
            for i in face {
                b.extend_from_slice(&i.to_le_bytes());
            }
        }
        b.extend_from_slice(&[1, 1, 1]); // the lamp's baked colour
        b.extend_from_slice(&[2, 2, 2]); // the panel's
        let stats = Stats {
            verts: verts.len() as u16,
            faces: 2,
            size: (0, 0, 0),
            claims: Claims::default(),
            components: 0,
        };
        (b, stats)
    }

    fn lamp_and_panel() -> HashMap<(u8, u8, u8), Role> {
        HashMap::from([((1, 1, 1), Role::Taillight), ((2, 2, 2), Role::Body)])
    }

    /// The first face's colour after a repaint, which is the one that gets to
    /// claim the shared vertex.
    fn first_colour(blob: &[u8], stats: &Stats) -> (u8, u8, u8) {
        let o = VERT_TABLE_OFFSET + stats.verts as usize * VERT_STRIDE + stats.faces as usize * 6;
        (blob[o], blob[o + 1], blob[o + 2])
    }

    fn claimed(claims: &Claims, role: Role) -> usize {
        claims
            .0
            .iter()
            .find(|&&(r, _)| r == role)
            .map_or(0, |&(_, n)| n)
    }

    #[test]
    fn a_lamp_beside_a_small_panel_claims_the_shared_vertex() {
        let (mut blob, stats) = two_face_blob(6);
        let claims = repaint(&mut blob, &stats, &lamp_and_panel(), &mut [0, 1]).unwrap();
        assert_eq!(first_colour(&blob, &stats), Role::Taillight.colour());
        assert_eq!(claimed(&claims, Role::Taillight), 3);
    }

    #[test]
    fn a_lamp_beside_a_full_length_panel_gives_the_shared_vertex_up() {
        // Same two triangles, but the neighbour is now big enough that the lamp
        // would bleed the length of it. It goes behind the body instead, and
        // keeps only the two corners the body never touches.
        let (mut blob, stats) = two_face_blob(200);
        let claims = repaint(&mut blob, &stats, &lamp_and_panel(), &mut [0, 1]).unwrap();
        assert_eq!(first_colour(&blob, &stats), Role::Body.colour());
        assert_eq!(claimed(&claims, Role::Taillight), 2);
    }

    #[test]
    fn a_repaint_keeps_every_triangle() {
        let (mut blob, stats) = two_face_blob(200);
        let before = index_table(&blob, &stats);
        let mut components = [0, 1];
        repaint(&mut blob, &stats, &lamp_and_panel(), &mut components).unwrap();
        let mut after = index_table(&blob, &stats);
        let mut before = before;
        before.sort_unstable();
        after.sort_unstable();
        assert_eq!(before, after, "the sort must permute, never drop or edit");
        assert_eq!(
            components,
            [1, 0],
            "component ids must follow the face sort"
        );
    }

    fn index_table(blob: &[u8], stats: &Stats) -> Vec<[u8; 6]> {
        let start = VERT_TABLE_OFFSET + stats.verts as usize * VERT_STRIDE;
        (0..stats.faces as usize)
            .map(|f| {
                let mut out = [0u8; 6];
                out.copy_from_slice(&blob[start + f * 6..start + f * 6 + 6]);
                out
            })
            .collect()
    }
}

#[cfg(test)]
mod runtime_caps {
    //! The renderer's per-car scratch buffers are fixed-size arrays, sized in
    //! `game/src/draw.rs`. Overrun one and nothing errors: `project_car`
    //! clamps and the car draws with its tail missing, which looks like a
    //! modelling mistake rather than a buffer. Since cooking is what decides
    //! these counts, the check belongs here, against what is committed.

    use super::WHEEL_NONE;

    /// `CAR_MAX_VERTS` in `game/src/draw.rs`.
    const MAX_VERTS: u16 = 1344;
    /// `CAR_TRI_CAP` in `game/src/draw.rs`, shared by both cars.
    const MAX_PAIR_FACES: u16 = 1248;

    #[test]
    fn every_cooked_car_fits_the_renderer_arrays() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../game/assets")
            .canonicalize()
            .expect("game/assets");
        let mut seen = 0;
        let mut car_faces = [0u16; 3];
        let names = ["sedan", "hatchback", "hatchback2"];
        for entry in std::fs::read_dir(&dir).expect("read game/assets") {
            let path = entry.expect("dir entry").path();
            if path.extension().and_then(|e| e.to_str()) != Some("psxm") {
                continue;
            }
            let bytes = std::fs::read(&path).expect("read blob");
            // AssetHeader is 12 bytes, then MeshHeader's vert/face counts.
            let verts = u16::from_le_bytes([bytes[12], bytes[13]]);
            let faces = u16::from_le_bytes([bytes[14], bytes[15]]);
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            assert!(
                verts <= MAX_VERTS,
                "{name}: {verts} vertices, renderer holds {MAX_VERTS}"
            );
            for (index, stem) in names.iter().enumerate() {
                if name == format!("{stem}.psxm") {
                    car_faces[index] = faces;
                    let sidecar = path.with_extension("psxw");
                    let wheels = std::fs::read(&sidecar)
                        .unwrap_or_else(|_| panic!("missing {}", sidecar.display()));
                    assert_eq!(
                        wheels.len(),
                        verts as usize,
                        "{}: wheel sidecar must have one slot per vertex",
                        sidecar.display()
                    );
                    let mut slot_vertices = [0usize; 4];
                    for &slot in &wheels {
                        assert!(
                            slot < 4 || slot == WHEEL_NONE,
                            "{}: invalid wheel slot {slot}",
                            sidecar.display()
                        );
                        if slot < 4 {
                            slot_vertices[slot as usize] += 1;
                        }
                    }
                    assert!(
                        slot_vertices.iter().all(|&count| count >= 6),
                        "{}: incomplete wheel corners {slot_vertices:?}",
                        sidecar.display()
                    );
                }
            }
            seen += 1;
        }
        assert!(seen > 0, "no .psxm blobs found in {}", dir.display());
        assert!(
            car_faces.iter().all(|&faces| faces > 0),
            "not every selectable car had a runtime blob"
        );
        // Both seats now pick freely from the same three, so every ordered
        // pair has to fit, including a car against itself. Checking the two
        // heaviest covers all of them.
        let mut heaviest = car_faces;
        heaviest.sort_unstable();
        // Nothing stops both seats picking the same model, so the worst
        // pair is the heaviest car twice.
        let pair = heaviest[2] * 2;
        assert!(
            pair <= MAX_PAIR_FACES,
            "the two heaviest cars use {pair} faces, shared renderer holds {MAX_PAIR_FACES}"
        );
    }
}

#[cfg(test)]
mod rigid_smoke {
    //! Smoke test for the textured path: can a baked GLB cook to .psxmdl with
    //! a real .psxt texture attached? Ignored by default because it needs the
    //! Blender bake output, which is not in the repo.
    use psxed_gltf::RigidModelConfig;

    #[test]
    #[ignore = "needs tools/bake_car_atlas.py output at /tmp/bake"]
    fn baked_glb_cooks_to_a_textured_model() {
        let cfg = RigidModelConfig {
            texture_width: 128,
            texture_height: 128,
            world_height: 120,
            ..RigidModelConfig::default()
        };
        let pkg = psxed_gltf::convert_rigid_model_path("/tmp/bake/sedan_baked.glb", &cfg)
            .expect("cook the baked sedan");
        let tex = pkg.texture.as_ref().expect("a texture came through");
        println!(
            "model {} bytes, texture {} bytes, clips {}",
            pkg.model.len(),
            tex.len(),
            pkg.clips.len()
        );
        assert!(pkg.model.len() > 64);
        assert!(tex.len() > 64);
    }
}
