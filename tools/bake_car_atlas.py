# SPDX-License-Identifier: GPL-2.0-or-later
"""Bake a car's flat material colours into a texture atlas, headless in Blender.

These models ship no textures. Every material is a flat glTF `baseColorFactor`,
which is why the cars cook to per-face colours and read as untextured lumps.
They do carry UVs, but not usable ones: each part was unwrapped into its own
0-1 square, so the parts overlap, and one of the sedan's tyres has all 1160 of
its UVs collapsed onto a single point.

So this repacks and bakes:

  1. join every mesh into one object, which is what the PS1 draws anyway
  2. decimate to a console triangle budget, before the unwrap, so the UV
     islands belong to the geometry that actually ships
  3. Smart UV Project into a single non-overlapping layout
  4. bake albedo, bake ambient occlusion, multiply them together
  5. sample that back out per face and rebuild the material list from it

The AO is the point, and step 5 is how it reaches the console. The PS1 runtime
draws this model from a `.psxm`, which carries one flat colour per triangle and
has no UV table at all, so the atlas itself cannot ship. Sampling it per face
and baking the result into flat materials gets the same shading through a
format that can hold it: wheel arches, panel gaps and the dark under the bumper
all survive, as per-face colour rather than per-texel.

Run:
    blender --background --python tools/bake_car_atlas.py -- \
        <in.glb> <out.png> <out.glb> [size] [target_tris]
"""

import sys
import os

import bpy

ARGS = sys.argv[sys.argv.index("--") + 1:] if "--" in sys.argv else []
if len(ARGS) < 3:
    raise SystemExit("usage: ... -- <in.glb> <out.png> <out.glb> [size]")

SRC, OUT_PNG, OUT_GLB = ARGS[0], ARGS[1], ARGS[2]
SIZE = int(ARGS[3]) if len(ARGS) > 3 else 256
TARGET_TRIS = int(ARGS[4]) if len(ARGS) > 4 else 320
# Samples for the AO pass. These are a few hundred triangles; this is cheap.
AO_SAMPLES = 64
# Quantisation of the sampled face colours. 1/24 keeps the palette around forty
# entries per car, fine enough that the AO gradients still read as gradients.
COLOR_STEP = 1.0 / 24.0


def clear_scene():
    bpy.ops.wm.read_factory_settings(use_empty=True)


def import_and_join(path):
    bpy.ops.import_scene.gltf(filepath=path)
    meshes = [o for o in bpy.context.scene.objects if o.type == "MESH"]
    if not meshes:
        raise SystemExit(f"{path}: no meshes")
    for o in bpy.context.scene.objects:
        o.select_set(o.type == "MESH")
    bpy.context.view_layer.objects.active = meshes[0]
    if len(meshes) > 1:
        bpy.ops.object.join()
    return bpy.context.view_layer.objects.active


def decimate(obj, target):
    """Collapse down to roughly `target` triangles.

    Before the unwrap, not after: baking a 2700-triangle model and then
    decimating would throw away most of the texel budget on geometry that
    never ships, and would move the UVs out from under the bake.
    """
    tris = sum(len(p.vertices) - 2 for p in obj.data.polygons)
    if tris <= target:
        return tris
    mod = obj.modifiers.new("decimate", "DECIMATE")
    mod.decimate_type = "COLLAPSE"
    mod.ratio = target / tris
    bpy.ops.object.modifier_apply(modifier=mod.name)
    return sum(len(p.vertices) - 2 for p in obj.data.polygons)


def repack_uvs(obj):
    """One non-overlapping layout for the whole car."""
    bpy.ops.object.mode_set(mode="EDIT")
    bpy.ops.mesh.select_all(action="SELECT")
    # angle_limit in radians; 1.15 (66 deg) keeps flat panels in one island.
    bpy.ops.uv.smart_project(angle_limit=1.15, island_margin=0.02)
    bpy.ops.object.mode_set(mode="OBJECT")


def add_bake_target(obj, name, color):
    """Give every material an image node the baker writes into."""
    img = bpy.data.images.new(name, SIZE, SIZE)
    img.generated_color = color
    for slot in obj.material_slots:
        mat = slot.material
        if mat is None:
            continue
        mat.use_nodes = True
        node = mat.node_tree.nodes.new("ShaderNodeTexImage")
        node.image = img
        node.name = name
        mat.node_tree.nodes.active = node
    return img


def bake(kind, **kwargs):
    scene = bpy.context.scene
    scene.render.engine = "CYCLES"
    scene.cycles.device = "CPU"
    scene.cycles.samples = AO_SAMPLES
    scene.render.bake.use_selected_to_active = False
    scene.render.bake.margin = 4
    bpy.ops.object.bake(type=kind, **kwargs)


def unlink_nodes(obj, name):
    for slot in obj.material_slots:
        mat = slot.material
        if mat is None:
            continue
        node = mat.node_tree.nodes.get(name)
        if node is not None:
            mat.node_tree.nodes.remove(node)


def assign_face_colors(obj, image, step):
    """Rebuild the material list as flat colours sampled from the bake.

    One material per distinct colour, each face pointing at the one nearest its
    own UV centroid. `step` quantises to keep the list short; the cars land
    around forty materials, which the cooker turns into forty face colours.
    """
    width, height = image.size
    pixels = list(image.pixels)
    mesh = obj.data
    uv = mesh.uv_layers.active.data

    def sample(u, v):
        x = min(width - 1, max(0, int(u * width)))
        y = min(height - 1, max(0, int(v * height)))
        i = (y * width + x) * 4
        return pixels[i], pixels[i + 1], pixels[i + 2]

    palette = {}
    mesh.materials.clear()
    for poly in mesh.polygons:
        # Centroid rather than a corner: a corner can sit in the island's
        # bleed margin and pick up whatever was padded in next to it.
        us = [uv[i].uv[0] for i in poly.loop_indices]
        vs = [uv[i].uv[1] for i in poly.loop_indices]
        r, g, b = sample(sum(us) / len(us), sum(vs) / len(vs))
        key = (round(r / step), round(g / step), round(b / step))
        if key not in palette:
            mat = bpy.data.materials.new(f"baked_{len(palette):03}")
            mat.use_nodes = True
            # glTF baseColorFactor is linear, which is what the bake already
            # is, so no transfer here. The cooker converts on the way to 8-bit.
            mat.node_tree.nodes["Principled BSDF"].inputs["Base Color"].default_value = (
                key[0] * step,
                key[1] * step,
                key[2] * step,
                1.0,
            )
            mesh.materials.append(mat)
            palette[key] = len(palette)
        poly.material_index = palette[key]
    return len(palette)


def multiply_into(base, shade, floor):
    """base *= max(shade, floor), in place. `floor` keeps AO from going black."""
    b = list(base.pixels)
    s = list(shade.pixels)
    for i in range(0, len(b), 4):
        occl = max(s[i], floor)
        b[i] *= occl
        b[i + 1] *= occl
        b[i + 2] *= occl
        b[i + 3] = 1.0
    base.pixels = b


def main():
    clear_scene()
    obj = import_and_join(SRC)
    tris = decimate(obj, TARGET_TRIS)
    repack_uvs(obj)

    albedo = add_bake_target(obj, "BAKE_ALBEDO", (0, 0, 0, 1))
    bake("DIFFUSE", pass_filter={"COLOR"})
    unlink_nodes(obj, "BAKE_ALBEDO")

    ao = add_bake_target(obj, "BAKE_AO", (1, 1, 1, 1))
    bake("AO")
    unlink_nodes(obj, "BAKE_AO")

    # Floor the occlusion: a PS1 has no bounce light, and a fully dark crease
    # just reads as a hole once the palette is quantised.
    multiply_into(albedo, ao, 0.45)

    albedo.filepath_raw = OUT_PNG
    albedo.file_format = "PNG"
    albedo.save()

    colors = assign_face_colors(obj, albedo, COLOR_STEP)

    bpy.ops.export_scene.gltf(
        filepath=OUT_GLB,
        export_format="GLB",
        export_materials="EXPORT",
    )
    print(f"BAKED {os.path.basename(SRC)} {tris} tris {colors} colours -> {OUT_GLB}")


main()
