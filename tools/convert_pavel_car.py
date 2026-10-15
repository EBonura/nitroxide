# SPDX-License-Identifier: GPL-2.0-or-later
"""Turn one Pavel 3D "Low Poly City Vehicles Pack" car into a source GLB.

Usage (Blender, headless):
    blender -b --python tools/convert_pavel_car.py -- <car.fbx> <palette.png> <out.glb>

The pack's cars are one body mesh plus four Wheel_FL/FR/RL/RR objects. Every
face picks a flat swatch from one shared palette PNG through its UVs, and the
same swatches are used by every car, so the swatch colour says what the face
is. This script samples the swatch per face and splits the body into one
object per role, named and materialled the way tools/prepare_car_models.py
and tools/cook-models/src/paint.rs expect (`metal_dark_blue` is the body,
`windows`, `light`, `stop_light`, ...), and each wheel into `Tire.xx` and
`Disk.xx`. The car faces +Z (glTF) like the other sources. Nothing from the
FBX is executed: it is only parsed by Blender's importer.

Pack: https://pavel-3d.itch.io/low-poly-city-vehicles-pack (CC BY 4.0).
"""
import bpy, bmesh, sys, math
from mathutils import Vector, Matrix
src, PAL, out = sys.argv[-3:]
bpy.ops.wm.read_factory_settings(use_empty=True)
bpy.ops.import_scene.fbx(filepath=src)
pal = bpy.data.images.load(PAL); W, H = pal.size; px = pal.pixels[:]
def colour(u, v):
    x = min(W-1, max(0, int(u*W))); y = min(H-1, max(0, int(v*H))); i = (y*W+x)*4
    return tuple(round(px[i+k]*255) for k in range(3))
# palette entries used by every car in the pack (sRGB), see REPORT
ROLE_OF = {
    (87, 87, 87): 'bege_gray_metal',      # lower body, sills
    (52, 52, 52): 'windows',
    (35, 36, 31): 'metal_dark_gray',      # grille, black trim
    (207, 234, 243): 'light',
    (120, 34, 37): 'stop_light',
    (196, 122, 77): 'turn_light',
    (204, 208, 207): 'metal_gray',
    (245, 245, 245): 'white',
    (81, 81, 81): 'bege_gray_metal',
}
# Swatches that are left out: the pack models the wing mirrors and the number
# plate as sub-pixel slivers, which the decimation turns into dark twigs.
DROP = {(204, 208, 207), (245, 245, 245)}
def role(c):
    if c in ROLE_OF: return ROLE_OF[c]
    mx, mn = max(c), min(c)
    sat = (mx-mn)/max(1, mx)
    return 'metal_dark_blue' if sat > 0.2 else 'bege_gray_metal'
MAT_COL = {'metal_dark_blue': (0.04, 0.12, 0.45), 'bege_gray_metal': (0.40, 0.38, 0.35), 'windows': (0.02, 0.05, 0.10),
           'metal_dark_gray': (0.10, 0.10, 0.11), 'light': (0.95, 0.95, 0.80), 'stop_light': (0.7, 0.05, 0.05),
           'turn_light': (0.9, 0.5, 0.05), 'metal_gray': (0.6, 0.6, 0.62), 'white': (0.99, 0.99, 0.99),
           'wheel': (0.01, 0.01, 0.012), 'chrome': (0.55, 0.57, 0.6)}
mats = {}
def mat(name):
    if name not in mats:
        m = bpy.data.materials.new(name); m.use_nodes = True
        b = next(n for n in m.node_tree.nodes if n.type == 'BSDF_PRINCIPLED')
        b.inputs['Base Color'].default_value = MAT_COL[name] + (1.0,)
        mats[name] = m
    return mats[name]
srcobjs = [o for o in bpy.context.scene.objects if o.type == 'MESH']
for o in srcobjs:
    mw = o.matrix_world.copy(); o.parent = None; o.matrix_world = mw
for o in srcobjs:
    for p in o.data.polygons: pass
# orientation: nose = bright front lights; want nose at -Y (exports to +Z)
fy = []; ty = []
for o in srcobjs:
    uv = o.data.uv_layers.active.data
    for p in o.data.polygons:
        n = len(p.loop_indices)
        c = colour(sum(uv[l].uv[0] for l in p.loop_indices)/n, sum(uv[l].uv[1] for l in p.loop_indices)/n)
        w = (o.matrix_world @ p.center)
        if c == (207, 234, 243): fy.append(w.y)
        if c == (120, 34, 37): ty.append(w.y)
flip = bool(fy and ty and sum(fy)/len(fy) > sum(ty)/len(ty))
print('NOSE', 'flip' if flip else 'ok', len(fy), len(ty))
newobjs = []
def split(obj, keyfn, namefn, collection):
    """duplicate obj once per key, keeping only faces with that key"""
    keys = {}
    uv = obj.data.uv_layers.active.data
    for p in obj.data.polygons:
        n = len(p.loop_indices)
        c = colour(sum(uv[l].uv[0] for l in p.loop_indices)/n, sum(uv[l].uv[1] for l in p.loop_indices)/n)
        if c in DROP:
            continue
        keys.setdefault(keyfn(c), []).append(p.index)
    for key, idx in keys.items():
        dup = obj.copy(); dup.data = obj.data.copy(); collection.objects.link(dup)
        dup.name = namefn(key)
        bm = bmesh.new(); bm.from_mesh(dup.data); bm.faces.ensure_lookup_table()
        keep = set(idx)
        kill = [f for f in bm.faces if f.index not in keep]
        bmesh.ops.delete(bm, geom=kill, context='FACES')
        loose = [v for v in bm.verts if not v.link_faces]
        bmesh.ops.delete(bm, geom=loose, context='VERTS')
        # The pack is quad-built; the face budgets in prepare_car_models.py count
        # triangles (as the other sources, which come triangulated), so split quads.
        bmesh.ops.triangulate(bm, faces=bm.faces[:])
        bm.to_mesh(dup.data); bm.free()
        dup.data.uv_layers.clear() if hasattr(dup.data.uv_layers, 'clear') else None
        dup.data.materials.clear(); dup.data.materials.append(mat(key if isinstance(key, str) else 'wheel'))
        newobjs.append(dup)
col = bpy.context.scene.collection
for o in srcobjs:
    if o.name.startswith('Wheel'):
        tag = o.name.split('_', 1)[1]
        # tyre faces dark, rim faces light grey
        split(o, lambda c: 'wheel' if c == (35, 36, 31) else 'chrome', lambda k, tag=tag: ('Tire.' if k == 'wheel' else 'Disk.') + tag, col)
    else:
        split(o, role, lambda k: 'Body.' + k, col)
for o in srcobjs: bpy.data.objects.remove(o, do_unlink=True)
for o in newobjs:
    bpy.context.view_layer.objects.active = o
    o.select_set(True)
if flip:
    for o in newobjs:
        o.matrix_world = Matrix.Rotation(math.pi, 4, 'Z') @ o.matrix_world
# origin: move so the car sits at the origin, wheels at z=0 (cook refits anyway)
bpy.ops.object.select_all(action='SELECT')
for o in newobjs:
    o.data.update()
bpy.ops.export_scene.gltf(filepath=out, export_format='GLB', use_active_scene=True, export_apply=True, export_yup=True)
print('CONVERTED', src, '->', out, [(o.name, len(o.data.polygons)) for o in newobjs])
