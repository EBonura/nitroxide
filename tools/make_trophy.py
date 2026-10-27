# SPDX-License-Identifier: GPL-2.0-or-later
"""Build the victory trophy: a low-poly gold cup on a dark plinth, as a glTF.

Run headless:

    "/Applications/Blender 5.2.app/Contents/MacOS/Blender" --background \
        --python tools/make_trophy.py -- build/trophy/trophy.glb

The cup is a lathe of six sides plus two flat handles, about 110 triangles,
with one material per part. The cooker (`tools/cook-models`, props) bakes each
material's base colour into the face table and the game lights it with its own
Gouraud rig, so the highlight on the bowl is the light rig's and not a texture.
Blender is Z up; the glTF exporter turns that into Y up, the plinth on y = 0.
"""

import math
import sys

import bpy
import bmesh

SIDES = 6
# Whether to turn every face over after orienting it outward. Not needed: the
# cooker takes glTF's counter-clockwise front as it is, so a face looking out of
# the model in Blender looks out in the game, and its exported normal with it.
FLIP = False

# (name, base colour). The names are what the cooker sees.
MATERIALS = {
    "plinth": (0.150, 0.190, 0.440),
    "plinth_top": (0.260, 0.310, 0.620),
    "gold": (1.000, 0.700, 0.100),
    "gold_bright": (1.000, 0.900, 0.480),
    "gold_dark": (0.780, 0.440, 0.040),
}

# The lathe, from the foot up: (radius, height, material of the band that ends
# at this point). The first point has no band under it.
PROFILE = [
    (1.00, 0.00, None),
    (0.86, 0.30, "plinth"),  # the plinth's sides
    (0.22, 0.30, "plinth_top"),  # its top face, out to the stem
    (0.22, 0.62, "gold_dark"),  # the stem
    (0.66, 1.14, "gold"),  # the bowl widening
    (0.64, 1.62, "gold_bright"),  # the bowl's straight upper band, catching the light
    (0.00, 1.40, "gold_dark"),  # looking down into the cup
]

# One handle: a flat ear, outline in (x, z) away from the bowl and mirrored.
HANDLE = [(0.60, 1.46), (1.00, 1.56), (1.10, 1.32), (0.90, 1.02), (0.60, 1.06)]
HANDLE_THICK = 0.12


def material(name):
    mat = bpy.data.materials.new(name)
    mat.use_nodes = True
    bsdf = next(n for n in mat.node_tree.nodes if n.type == "BSDF_PRINCIPLED")
    bsdf.inputs["Base Color"].default_value = (*MATERIALS[name], 1.0)
    mat.diffuse_color = (*MATERIALS[name], 1.0)
    return mat


def build():
    bpy.ops.wm.read_factory_settings(use_empty=True)
    mesh = bpy.data.meshes.new("trophy")
    obj = bpy.data.objects.new("trophy", mesh)
    bpy.context.scene.collection.objects.link(obj)
    names = list(MATERIALS)
    for name in names:
        mesh.materials.append(material(name))
    index = {name: i for i, name in enumerate(names)}

    bm = bmesh.new()
    rings = []
    for r, z, _ in PROFILE:
        if r == 0.0:
            rings.append([bm.verts.new((0.0, 0.0, z))])
        else:
            rings.append(
                [
                    bm.verts.new(
                        (
                            r * math.cos(2 * math.pi * k / SIDES),
                            r * math.sin(2 * math.pi * k / SIDES),
                            z,
                        )
                    )
                    for k in range(SIDES)
                ]
            )

    def face(verts, name, smooth):
        f = bm.faces.new(verts)
        f.material_index = index[name]
        f.smooth = smooth
        return f

    for band in range(1, len(PROFILE)):
        name = PROFILE[band][2]
        lower, upper = rings[band - 1], rings[band]
        # The plinth's sides and the flat top are hard-edged; the cup is round.
        smooth = name not in ("plinth", "plinth_top")
        for k in range(SIDES):
            k2 = (k + 1) % SIDES
            if len(upper) == 1:
                face([lower[k], lower[k2], upper[0]], name, smooth)
            else:
                face([lower[k], lower[k2], upper[k2], upper[k]], name, smooth)

    handles = []
    # The handles: a flat prism through the outline, one each side of the cup.
    for sign in (1, -1):
        outline = [(sign * x, z) for x, z in HANDLE]
        near = [bm.verts.new((x, -HANDLE_THICK / 2, z)) for x, z in outline]
        far = [bm.verts.new((x, HANDLE_THICK / 2, z)) for x, z in outline]
        if sign < 0:
            near.reverse()
            far.reverse()
        handle = [face(near[::-1], "gold", False), face(far, "gold", False)]
        for k in range(len(near)):
            k2 = (k + 1) % len(near)
            handle.append(face([near[k], near[k2], far[k2], far[k]], "gold", False))
        for f in handle:
            f.tag = True
            f.hide_set(False)
        handles.append((sign, handle))

    # Turn every face to look outward by its own part's rule (the mesh is open, so
    # a global recalculation cannot be trusted to pick the same side everywhere),
    # and, if `FLIP` is set, all of them over.
    bm.normal_update()
    for f in bm.faces:
        c, n = f.calc_center_median(), f.normal
        if f.tag:
            # A handle: its flat faces look out along y, its walls away from its middle.
            mid_x = 0.85 if c.x > 0 else -0.85
            if abs(n.y) > 0.5:
                want = n.y * c.y >= 0.0 if abs(c.y) > 0.01 else True
            else:
                want = (n.x * (c.x - mid_x) + n.z * (c.z - 1.28)) > 0.0
        elif f.material_index == index["plinth_top"] or (
            f.material_index == index["gold_dark"] and len(f.verts) == 3
        ):
            want = n.z > 0.0  # the plinth's top and the cup's inside look up
        else:
            want = (c.x * n.x + c.y * n.y) > 0.0  # a lathe band: away from the axis
        if not want:
            f.normal_flip()
    if FLIP:
        bmesh.ops.reverse_faces(bm, faces=bm.faces)
    bmesh.ops.triangulate(bm, faces=bm.faces)
    bm.to_mesh(mesh)
    bm.free()
    tris = len(mesh.polygons)
    print(f"TROPHY {tris} triangles {len(mesh.vertices)} vertices")
    return obj


def main():
    argv = sys.argv[sys.argv.index("--") + 1 :] if "--" in sys.argv else []
    if not argv:
        raise SystemExit("usage: blender --background --python make_trophy.py -- out.glb")
    obj = build()
    bpy.context.view_layer.objects.active = obj
    obj.select_set(True)
    bpy.ops.export_scene.gltf(
        filepath=argv[0],
        export_format="GLB",
        use_selection=True,
        export_yup=True,
        export_normals=True,
        export_materials="EXPORT",
        export_image_format="NONE",
        export_apply=True,
    )


main()
