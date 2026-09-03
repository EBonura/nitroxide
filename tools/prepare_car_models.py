# SPDX-License-Identifier: GPL-2.0-or-later
"""Bake transforms and decimate one source car without welding its parts."""

import bpy
import math
import sys
from mathutils import Vector


source, output, target_text = sys.argv[-3:]
target_faces = int(target_text)

bpy.ops.wm.read_factory_settings(use_empty=True)
bpy.ops.import_scene.gltf(filepath=source)
objects = [obj for obj in bpy.context.scene.objects if obj.type == "MESH"]


def rebuild_topology(obj):
    """Undo glTF's UV/normal corner splits, then rebuild useful hard edges."""
    # These cars use flat material colours, not textures. Keeping their unused
    # UV seams makes the glTF exporter duplicate nearly every face corner.
    # psxed-gltf then sees hundreds of disconnected triangles, so its normal
    # builder lights every wheel facet independently and the PS1 rasterizer can
    # expose the resulting sub-pixel cracks.
    while obj.data.uv_layers:
        obj.data.uv_layers.remove(obj.data.uv_layers[0])
    while obj.data.color_attributes:
        obj.data.color_attributes.remove(obj.data.color_attributes[0])

    for other in bpy.context.scene.objects:
        other.select_set(False)
    obj.select_set(True)
    bpy.context.view_layer.objects.active = obj
    bpy.ops.object.mode_set(mode="EDIT")
    bpy.ops.mesh.select_all(action="SELECT")
    bpy.ops.mesh.remove_doubles(threshold=0.00001)
    bpy.ops.mesh.normals_make_consistent(inside=False)
    bpy.ops.object.mode_set(mode="OBJECT")

    # Smooth curved wheel walls, but retain the cap/rim and body creases. The
    # exporter represents those marked sharp edges with intentional duplicate
    # vertices; every other duplicate is now gone.
    bpy.ops.object.shade_smooth_by_angle(
        angle=math.radians(40.0),
        keep_sharp_edges=True,
    )
    obj.select_set(False)


# Applying transforms inside Blender handles mirrored nodes correctly. Baking
# only the transformed positions in psxed-gltf leaves the original winding on
# negative-scale wheels and panels, making those parts inside-out on PS1.
for obj in bpy.context.scene.objects:
    obj.select_set(False)
for obj in objects:
    obj.select_set(True)
    bpy.context.view_layer.objects.active = obj
    bpy.ops.object.transform_apply(location=True, rotation=True, scale=True)
    obj.select_set(False)
    rebuild_topology(obj)


def material_stem(obj):
    """Lower-case material name without Blender's numeric duplicate suffix."""
    if not obj.material_slots or not obj.material_slots[0].material:
        return ""
    name = obj.material_slots[0].material.name.lower()
    head, dot, suffix = name.rpartition(".")
    return head if dot and suffix.isdigit() else name


def split_loose_parts(obj):
    """Make each disconnected wheel/rim in a combined source object separate."""
    for other in bpy.context.scene.objects:
        other.select_set(False)
    obj.select_set(True)
    bpy.context.view_layer.objects.active = obj
    bpy.ops.object.mode_set(mode="EDIT")
    bpy.ops.mesh.select_all(action="SELECT")
    bpy.ops.mesh.separate(type="LOOSE")
    bpy.ops.object.mode_set(mode="OBJECT")


def replace_wheel(obj, sides=10):
    """Replace collapse-prone source wheel topology with one clean cylinder."""
    material = obj.material_slots[0].material if obj.material_slots else None
    corners = [obj.matrix_world @ Vector(corner) for corner in obj.bound_box]
    minimum = Vector(tuple(min(v[axis] for v in corners) for axis in range(3)))
    maximum = Vector(tuple(max(v[axis] for v in corners) for axis in range(3)))
    centre = (minimum + maximum) * 0.5
    dimensions = maximum - minimum
    axis = min(range(3), key=lambda index: dimensions[index])
    radial = [index for index in range(3) if index != axis]
    radius = (dimensions[radial[0]] + dimensions[radial[1]]) * 0.25
    # Several source cars use a chrome cylinder almost as large as the tyre.
    # At garage scale its bright cap covers the rubber completely and reads as
    # an inside-out solid wheel. Preserve the tyre bounds, but inset rims far
    # enough to leave an unmistakable dark ring.
    if material_stem(obj) in {"chrome", "dark_chrome"}:
        radius *= 0.72
    depth = dimensions[axis]
    rotation = {
        0: (0.0, math.pi / 2.0, 0.0),
        1: (math.pi / 2.0, 0.0, 0.0),
        2: (0.0, 0.0, 0.0),
    }[axis]
    name = obj.name
    bpy.data.objects.remove(obj, do_unlink=True)
    bpy.ops.mesh.primitive_cylinder_add(
        vertices=sides,
        radius=radius,
        depth=depth,
        end_fill_type="NGON",
        location=centre,
        rotation=rotation,
    )
    wheel = bpy.context.object
    wheel.name = name
    if material is not None:
        wheel.data.materials.append(material)
    bpy.ops.object.transform_apply(location=True, rotation=True, scale=True)
    rebuild_topology(wheel)


def replace_rim_disc(obj, sides=6):
    """Replace a gameplay rim with only its visible outward-facing disc."""
    material = obj.material_slots[0].material if obj.material_slots else None
    corners = [obj.matrix_world @ Vector(corner) for corner in obj.bound_box]
    minimum = Vector(tuple(min(v[axis] for v in corners) for axis in range(3)))
    maximum = Vector(tuple(max(v[axis] for v in corners) for axis in range(3)))
    centre = (minimum + maximum) * 0.5
    dimensions = maximum - minimum
    axis = min(range(3), key=lambda index: dimensions[index])
    radial = [index for index in range(3) if index != axis]
    radius = (dimensions[radial[0]] + dimensions[radial[1]]) * 0.25 * 0.72
    outward_sign = 1.0 if centre[axis] >= 0.0 else -1.0
    plane = maximum[axis] if outward_sign > 0 else minimum[axis]

    vertices = []
    for index in range(sides):
        angle = 2.0 * math.pi * index / sides
        point = centre.copy()
        point[axis] = plane
        point[radial[0]] += math.cos(angle) * radius
        point[radial[1]] += math.sin(angle) * radius
        vertices.append(tuple(point))

    # radial axes [Y,Z] face +X; [X,Z] face -Y; [X,Y] face +Z.
    base_sign = 1.0 if axis in {0, 2} else -1.0
    face = list(range(sides))
    if base_sign != outward_sign:
        face.reverse()

    name = obj.name
    bpy.data.objects.remove(obj, do_unlink=True)
    mesh = bpy.data.meshes.new(name + "Mesh")
    mesh.from_pydata(vertices, [], [face])
    mesh.update()
    rim = bpy.data.objects.new(name, mesh)
    bpy.context.collection.objects.link(rim)
    if material is not None:
        rim.data.materials.append(material)
    rebuild_topology(rim)


# Blender's collapse modifier turns the source pack's very dense, combined
# axle meshes into long radial spikes at PS1 budgets. That failure is not
# limited to the close-up garage: at gameplay scale it is the "exploded car"
# silhouette, with wheels and long slivers detached from the body. Split the
# loose tyres/rims and replace them before *either* LOD is decimated. Six sides
# hold the two-car match budget; the close-up garage keeps ten.
wheel_materials = {"wheel", "chrome", "dark_chrome"}


def is_wheel_part(obj):
    name = obj.name.lower()
    return material_stem(obj) in wheel_materials and (
        name.startswith("tire") or name.startswith("disk")
    )


for obj in list(objects):
    if is_wheel_part(obj):
        split_loose_parts(obj)
objects = [obj for obj in bpy.context.scene.objects if obj.type == "MESH"]
for obj in list(objects):
    if is_wheel_part(obj):
        if target_faces >= 500:
            replace_wheel(obj, sides=10)
        elif (
            obj.name.lower().startswith("disk")
            or material_stem(obj) in {"chrome", "dark_chrome"}
        ):
            replace_rim_disc(obj, sides=6 if target_faces >= 100 else 4)
        else:
            replace_wheel(obj, sides=6 if target_faces >= 100 else 4)
objects = [obj for obj in bpy.context.scene.objects if obj.type == "MESH"]

# At gameplay scale, budget by visual role rather than by the source object's
# original polygon count. Preserving every <=32-face lamp/bumper verbatim used
# the whole 150-face target before the main body was considered; the body then
# collapsed to two long triangles and read as an exploded chassis. The caps
# below reserve the silhouette first and retain only enough small detail to
# identify each material at thirty screen pixels.
def gameplay_face_cap(obj):
    name = obj.name.lower()
    material = material_stem(obj)
    if is_wheel_part(obj):
        return len(obj.data.polygons)
    if material in {
        "metal_dark_blue",
        "metalyellow",
        "metal_green",
        "metal_light_orange",
        "metal_red",
    }:
        return 64
    if material == "metal_black":
        return 24
    if material == "windows":
        return 16
    if material in {"metal_gray", "white"}:
        return 8
    if material in {"light", "stop_light"}:
        return 6
    if material in {"metal_dark_gray", "metal_dark_gray.001"}:
        return 6
    if material == "turn_light":
        return 4
    if material == "black" and "mirror" in name:
        return 4
    return 10


if target_faces < 500:
    # The role caps above add up to the 150-face gameplay car. A smaller
    # target scales every cap down proportionally (never below a triangle
    # pair), which is how the split-screen distance LOD is cooked from the
    # same sources.
    scale = min(1.0, target_faces / 150.0)
    decimate = [
        (obj, min(len(obj.data.polygons), max(2, round(gameplay_face_cap(obj) * scale))))
        for obj in objects
    ]
else:
    # The garage has enough budget to preserve small authored trim verbatim
    # and distribute what remains proportionally over the large assemblies.
    small = [obj for obj in objects if len(obj.data.polygons) <= 32]
    large = [obj for obj in objects if len(obj.data.polygons) > 32]
    fixed_faces = sum(len(obj.data.polygons) for obj in small)
    large_faces = sum(len(obj.data.polygons) for obj in large)
    ratio = min(1.0, max(0.01, (target_faces - fixed_faces) / max(1, large_faces)))
    decimate = [
        (obj, max(1, round(len(obj.data.polygons) * ratio)))
        for obj in large
    ]

for obj, face_cap in decimate:
    polygons = len(obj.data.polygons)
    if polygons <= face_cap:
        continue
    bpy.context.view_layer.objects.active = obj
    modifier = obj.modifiers.new(name="PS1 Decimate", type="DECIMATE")
    modifier.decimate_type = "COLLAPSE"
    modifier.ratio = min(1.0, max(0.01, face_cap / max(1, polygons)))
    modifier.use_collapse_triangulate = True
    bpy.ops.object.modifier_apply(modifier=modifier.name)
    rebuild_topology(obj)

bpy.ops.export_scene.gltf(
    filepath=output,
    export_format="GLB",
    use_active_scene=True,
    export_apply=True,
)

print(
    "PREPARED",
    source,
    "->",
    output,
    "faces",
    sum(len(obj.data.polygons) for obj in objects),
)
