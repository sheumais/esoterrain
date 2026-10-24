//! Turn placement JSON files into a Blender scene made of *references*.
//!
//! Pipeline:
//!   1. Read one or more placement JSONs (world `placement.json` and/or `cell_x#_z#.json`).
//!   2. Export every distinct model (`model_type_id` == MNF FileIndex) through `model_export::run_batch`
//!      (each needed archive is dumped once, not one `-f` call per file), skipping models whose
//!      `<index>.obj` is already in the models folder.
//!   3. Write `<name>.json` (compact data) and `<name>.py` (Blender import script).
//!      Models can exist in several versions (see `model_export`: alliance banner/shield sets and automatically
//!      detected overlapping meshes, exported as `<index>_<label>.obj`).
//!      * Normal scene: a placement of a multi-version model becomes a collection holding one instance per
//!        version, all in the same spot. Only the first version (or the `--alliance` one) is visible; you pick the
//!        right one per piece in Blender by hiding/unhiding.
//!      * Grid scene (`--scene-grid`, `<name>_grid.py`): every model once in a grid cell, all its versions stacked
//!        vertically in that cell, for looking at everything at a glance.
//!
//! The .mtl files carry `# alpha_mode mask|blend` lines (written by `model_export`); the Blender scripts read them
//! and set cutout (mask) or blended materials accordingly.
//!
//! In Blender the script imports each unique OBJ once into its own hidden collection and then
//! creates one *collection-instance empty* per placement.
//! The library copies are spread out in a grid for inspection (`LIBRARY_GRID` in the script); each version collection's
//! `instance_offset` is set to its grid displacement, so the instances still sit exactly where they are placed. Edit a model once and every instance
//! follows; the scene stays light because geometry exists only once per model.

use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use crate::model_export::{self, Options};

#[derive(Debug, Clone)]
struct Inst {
    model: u32,
    pos: [f64; 3],
    rot: [f64; 3],
}


fn parse_cell_name(path: &Path) -> Option<(i64, i64)> {
    let stem = path.file_stem()?.to_str()?;
    let rest = stem.strip_prefix("cell_x")?;
    let (x, z) = rest.split_once("_z")?;
    Some((x.parse().ok()?, z.parse().ok()?))
}

fn collect_json_files(inputs: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for s in inputs {
        let p = Path::new(s);
        if p.is_dir() {
            let mut found: Vec<PathBuf> = walkdir::WalkDir::new(p)
                .into_iter()
                .filter_map(Result::ok)
                .map(|e| e.into_path())
                .filter(|f| {
                    f.is_file()
                        && f.extension().and_then(|e| e.to_str()).map_or(false, |e| e.eq_ignore_ascii_case("json"))
                })
                .collect();
            found.sort();
            out.extend(found);
        } else {
            out.push(p.to_path_buf());
        }
    }
    out
}

fn load_instances(files: &[PathBuf], cell_size: f64) -> (Vec<Inst>, usize, usize) {
    let mut all = Vec::new();
    let (mut used, mut skipped) = (0usize, 0usize);
    for f in files {
        let parsed = fs::read_to_string(f).ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
        let Some(v) = parsed else { eprintln!("  Skipping unreadable JSON: {}", f.display()); skipped += 1; continue };
        let Some(arr) = v.get("instances").and_then(|i| i.as_array()) else { skipped += 1; continue };

        let (ox, oz) = match parse_cell_name(f) {
            Some((cx, cz)) => (cx as f64 * cell_size, cz as f64 * cell_size),
            None => (0.0, 0.0),
        };
        for i in arr {
            let g = |k: &str| i.get(k).and_then(|x| x.as_f64()).unwrap_or(0.0);
            let Some(model) = i.get("model_type_id").and_then(|x| x.as_u64()) else { continue };
            all.push(Inst {
                model: model as u32,
                pos: [g("pos_x") + ox, g("pos_y"), g("pos_z") + oz],
                rot: [g("rot_x"), g("rot_y"), g("rot_z")],
            });
        }
        used += 1;
    }
    (all, used, skipped)
}

fn default_name(inputs: &[String]) -> String {
    for s in inputs {
        for c in Path::new(s).components() {
            let c = c.as_os_str().to_string_lossy();
            if c.starts_with("world_") {
                return format!("scene_{}", c);
            }
        }
    }
    "scene".to_string()
}

fn plain_abs(p: &Path) -> String {
    let c = fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = c.to_string_lossy().into_owned();
    s.strip_prefix(r"\\?\").map(|r| r.to_string()).unwrap_or(s)
}

pub struct SceneArgs<'a> {
    pub inputs: &'a [String],
    pub cell_size: f64,
    pub name: Option<&'a str>,
    pub script_only: bool,
    pub scenes_dir: &'a Path,
    /// Which version of a multi-version model starts visible (a label such as "daggerfall" or a mesh name).
    /// None / "all" = the first version of each model.
    pub alliance: Option<&'a str>,
    /// Write a viewing grid of every model (all versions stacked vertically in one cell) instead of a placed scene.
    pub grid: bool,
}

/// One OBJ of a model: (label, file name).
type Version = (String, String);

/// The versions to use for a model. If any labelled OBJ exists those are the versions (a leftover combined
/// `<index>.obj` from an older export is ignored); otherwise the plain OBJ is the single version.
fn versions_of(files: &[(Option<&'static str>, String)]) -> Vec<Version> {
    let mut v: Vec<Version> = files
        .iter()
        .filter_map(|(l, f)| (*l).map(|l| (l.to_string(), f.clone())))
        .collect();
    if v.is_empty() {
        v = files.iter().take(1).map(|(_, f)| ("default".to_string(), f.clone())).collect();
    }
    v
}

fn versions_json(v: &[Version]) -> Value {
    Value::Array(v.iter().map(|(l, f)| json!([l, f])).collect())
}

pub fn run(a: &SceneArgs, mo: &Options) -> Result<PathBuf, String> {
    let default_label: Option<&str> = match a.alliance.map(str::trim) {
        None | Some("") => None,
        Some(x) if x.eq_ignore_ascii_case("all") => None,
        Some(x) => Some(x),
    };

    let files = collect_json_files(a.inputs);
    if files.is_empty() {
        return Err("No placement JSON files found for --scene.".into());
    }
    let (insts, used, skipped) = load_instances(&files, a.cell_size);
    println!("Read {} placement file(s) ({} skipped as non-placement), {} instance(s).", used, skipped, insts.len());
    if insts.is_empty() {
        return Err("No instances found.".into());
    }

    let ids: BTreeSet<u32> = insts.iter().map(|i| i.model).collect();
    println!("{} distinct model(s) referenced.", ids.len());

    let rows = model_export::lookup_file_indexes(mo.manifest, &ids.iter().copied().collect());

    let mut index_of: HashMap<u32, u32> = HashMap::new();
    let mut not_in_manifest = 0usize;
    for id in &ids {
        match rows.get(id).and_then(|r| r.first()) {
            Some(r) => { index_of.insert(*id, r.index); }
            None => { eprintln!("  Model FileIndex 0x{:08X} ({}) not in manifest; its instances will be dropped.", id, id); not_in_manifest += 1; }
        }
    }

    // A model counts as exported if it has a plain OBJ or per-version OBJs.
    let todo: Vec<u32> = ids.iter().copied()
        .filter(|id| index_of.get(id).map_or(false, |i| model_export::exported_objs(mo.out_root, *i).is_empty()))
        .collect();
    println!("{} model(s) already exported, {} to export{}.",
        index_of.len() - todo.len(), todo.len(), if not_in_manifest > 0 { format!(", {} unknown", not_in_manifest) } else { String::new() });

    if mo.dry_run {
        for id in &todo { println!("  would export 0x{:x} ({})", id, id); }
        println!("\nDry run: no models exported, no scene files written.");
        return Ok(PathBuf::new());
    }

    if !a.script_only {
        for (id, r) in model_export::run_batch(&todo, mo) {
            match r {
                Ok(s) => println!("  0x{:x}: {} texture(s), {} missing", id, s.textures_found, s.textures_missing),
                Err(e) => eprintln!("  0x{:x}: {}", id, e),
            }
        }
    }

    // The OBJ versions of every model that has been exported.
    let exported: HashMap<u32, Vec<Version>> = ids.iter()
        .filter_map(|id| {
            let f = model_export::exported_objs(mo.out_root, *index_of.get(id)?);
            if f.is_empty() { None } else { Some((*id, versions_of(&f))) }
        })
        .collect();
    if exported.is_empty() {
        return Err("No model has an exported OBJ; nothing to write.".to_string());
    }
    let multi = exported.values().filter(|v| v.len() > 1).count();
    println!("{} model(s) have an OBJ, {} of them in several versions.", exported.len(), multi);
    if let Some(d) = default_label {
        let hit = exported.values().any(|v| v.iter().any(|(l, _)| l.eq_ignore_ascii_case(d)));
        if !hit && multi > 0 {
            println!("No model has a version called '{}'; showing the first version of each.", d);
        }
    }

    fs::create_dir_all(a.scenes_dir).map_err(|e| format!("Failed to create '{}': {}", a.scenes_dir.display(), e))?;
    let base_name = a.name.map(|s| s.to_string()).unwrap_or_else(|| default_name(a.inputs));
    let models_abs = plain_abs(mo.out_root);

    let (name, template, sidecar): (String, &str, Value) = if a.grid {
        // Grid: every model once; all of its versions stacked vertically in its cell.
        let mut list: Vec<(u32, &Vec<Version>)> = exported.iter().map(|(k, v)| (*k, v)).collect();
        list.sort_by_key(|(k, _)| *k);
        let models: Vec<Value> = list.iter().map(|(id, v)| json!([id, versions_json(v)])).collect();
        println!("\nGrid of {} model(s) ({} with several versions stacked).", models.len(), multi);
        (format!("{}_grid", base_name), GRID_TEMPLATE, json!({ "models": models }))
    } else {
        // Placed scene: one entry per model, plus the instances. Multi-version models become a choice collection
        // per placement in Blender, showing only the default version.
        let mut models = serde_json::Map::new();
        for (id, v) in &exported {
            let show = default_label
                .and_then(|d| v.iter().position(|(l, _)| l.eq_ignore_ascii_case(d)))
                .unwrap_or(0);
            models.insert(id.to_string(), json!({ "show": show, "versions": versions_json(v) }));
        }
        let kept: Vec<&Inst> = insts.iter().filter(|i| exported.contains_key(&i.model)).collect();
        let dropped = insts.len() - kept.len();
        println!("\n[{}] {} model(s) have an OBJ; {} instance(s) kept, {} dropped (model missing).", base_name, models.len(), kept.len(), dropped);
        if kept.is_empty() {
            return Err("No instances have an exported OBJ; nothing to write.".to_string());
        }
        let placements_with_choice = kept.iter().filter(|i| exported.get(&i.model).map_or(false, |v| v.len() > 1)).count();
        if placements_with_choice > 0 {
            println!("{} placement(s) have several versions: only the first is visible, pick the right one in Blender.", placements_with_choice);
        }
        let inst_json: Vec<Value> = kept.iter().map(|i| json!([i.model, i.pos[0], i.pos[1], i.pos[2], i.rot[0], i.rot[1], i.rot[2]])).collect();
        (base_name.clone(), BLENDER_TEMPLATE, json!({ "models": Value::Object(models), "instances": inst_json }))
    };

    let json_name = format!("{}.json", name);
    fs::write(a.scenes_dir.join(&json_name), serde_json::to_string(&sidecar).unwrap())
        .map_err(|e| format!("Failed to write scene json: {}", e))?;

    let script = template
        .replace("@NAME@", &name)
        .replace("@SIDECAR@", &serde_json::to_string(&json_name).unwrap())
        .replace("@MODELS_ABS@", &serde_json::to_string(&models_abs).unwrap())
        .replace("@SCENES_ABS@", &serde_json::to_string(&plain_abs(a.scenes_dir)).unwrap());
    let py_path = a.scenes_dir.join(format!("{}.py", name));
    fs::write(&py_path, script).map_err(|e| format!("Failed to write script: {}", e))?;

    println!("Scene data  -> {}", a.scenes_dir.join(&json_name).display());
    println!("Blender script -> {}", py_path.display());
    Ok(py_path)
}

const BLENDER_TEMPLATE: &str = r#"# Generated by land_export --scene. Run in Blender: Scripting tab -> Open -> Run Script.
# Imports every distinct model once, then places collection-instance empties that REFERENCE it.
# The imported library models are laid out in a grid (LIBRARY_GRID) so you can inspect them; each collection's
# instance_offset undoes that displacement, so the placed instances are unaffected.
# Models that exist in several versions (alliance banners/shields, ...) get, per placement, one collection in
# '<scene>_choices' that holds one instance empty per version, all in the same spot. Only the first (or the
# --alliance one) is visible. To pick another version for that piece: hide the visible empty and unhide the
# one you want (eye icon in the Outliner).
import bpy, json, os
from mathutils import Matrix, Euler, Vector

SCENE_NAME = '@NAME@'
SIDECAR = @SIDECAR@
BAKED_SCENES_DIR = @SCENES_ABS@
BAKED_MODELS_DIR = @MODELS_ABS@

# ---- knobs ---------------------------------------------------------------------------------
SOURCE_Y_UP = True       # game data is Y-up;
EULER_ORDER = 'XYZ'      # rotation order of rot_x/rot_y/rot_z (radians)
UNIT_SCALE = 1.0         # multiply positions and instance size
LIMIT = 0                # >0: only place the first N instances
LIBRARY_GRID = True      # lay the library models out in a grid so you can inspect them; instances still line up
GAP = 0.15               # grid: space between neighbouring cells, as a fraction of the largest model
STACK_GAP = 0.10         # grid: space between stacked versions, as a fraction of the version's height (min 2% of largest)
ROW_WIDTH_FACTOR = 1.3   # grid: wider = fewer, longer rows
KEEP_EXISTING_MATERIALS = True   # reuse materials already in this .blend (same name + same textures) instead of the freshly imported ones
# ---------------------------------------------------------------------------------------------

try:
    HERE = os.path.dirname(os.path.abspath(__file__))
except NameError:
    HERE = BAKED_SCENES_DIR
if not os.path.exists(os.path.join(HERE, SIDECAR)):
    HERE = BAKED_SCENES_DIR
MODELS_DIR = os.path.normpath(os.path.join(HERE, '..', 'models'))
if not os.path.isdir(MODELS_DIR):
    MODELS_DIR = BAKED_MODELS_DIR

with open(os.path.join(HERE, SIDECAR), 'r') as fh:
    data = json.load(fh)

if SOURCE_Y_UP:
    C = Matrix(((1, 0, 0), (0, 0, -1), (0, 1, 0)))   # (x, y, z) -> (x, -z, y), same as the OBJ importer's Y-up -> Z-up
else:
    C = Matrix.Identity(3)
C_INV = C.inverted()


import re

_ALPHA_DONE = set()


def read_alpha_modes(obj_path):
    """{material name: 'mask' | 'blend'} from the '# alpha_mode' lines the exporter writes into the .mtl."""
    modes = {}
    mtl = None
    try:
        with open(obj_path, 'r') as fh:
            for line in fh:
                if line.startswith('mtllib '):
                    mtl = line.split(None, 1)[1].strip()
                    break
        if mtl is None:
            return modes
        cur = None
        with open(os.path.join(os.path.dirname(obj_path), mtl), 'r') as fh:
            for line in fh:
                line = line.strip()
                if line.startswith('newmtl '):
                    cur = line.split(None, 1)[1].strip()
                elif line.startswith('# alpha_mode ') and cur:
                    modes[cur] = line.split()[-1]
    except OSError as e:
        print('Could not read alpha modes for', obj_path, e)
    return modes


def set_alpha_mode(mat, mode):
    """mask = hard cutout (alpha test), blend = true transparency."""
    if mat is None or mat.name in _ALPHA_DONE:
        return
    _ALPHA_DONE.add(mat.name)
    if mode == 'blend':
        if hasattr(mat, 'surface_render_method'):      # Blender 4.2+
            mat.surface_render_method = 'BLENDED'
        else:
            mat.blend_method = 'BLEND'
            if hasattr(mat, 'show_transparent_back'):
                mat.show_transparent_back = False
        return
    # mask
    if hasattr(mat, 'blend_method') and not hasattr(mat, 'surface_render_method'):   # before 4.2
        mat.blend_method = 'CLIP'
        mat.alpha_threshold = 0.5
        return
    # 4.2+ has no clip mode: threshold the alpha so a pixel is fully drawn or fully discarded
    if not mat.use_nodes:
        return
    nt = mat.node_tree
    for node in list(nt.nodes):
        if node.type != 'BSDF_PRINCIPLED':
            continue
        sock = node.inputs['Alpha']
        if not sock.is_linked:
            continue
        link = sock.links[0]
        if link.from_node.type == 'MATH' and link.from_node.operation == 'GREATER_THAN':
            continue
        th = nt.nodes.new('ShaderNodeMath')
        th.operation = 'GREATER_THAN'
        th.inputs[1].default_value = 0.5
        th.location = (node.location.x - 200, node.location.y - 300)
        nt.links.new(link.from_socket, th.inputs[0])
        nt.links.new(th.outputs[0], sock)


def apply_alpha_modes(obj_path, objs):
    modes = read_alpha_modes(obj_path)
    if not modes:
        return
    for o in objs:
        if o.type != 'MESH':
            continue
        for slot in o.material_slots:
            mat = slot.material
            if mat is None:
                continue
            base = mat.name if mat.name in modes else re.sub(r'\.\d+$', '', mat.name)
            if base in modes:
                set_alpha_mode(mat, modes[base])


_KEEP = {}   # base material name -> [materials that existed before this script ran]


def mat_base(name):
    return re.sub(r'\.\d+$', '', name)


def mat_textures(mat):
    """Set of texture file names a material uses."""
    out = set()
    if mat is not None and mat.use_nodes and mat.node_tree:
        for n in mat.node_tree.nodes:
            if n.type == 'TEX_IMAGE' and n.image:
                out.add(os.path.basename(n.image.filepath) or n.image.name)
    return out


if KEEP_EXISTING_MATERIALS:
    for _m in bpy.data.materials:
        _KEEP.setdefault(mat_base(_m.name), []).append(_m)
    for _l in _KEEP.values():
        _l.sort(key=lambda m: (mat_base(m.name) != m.name, -m.users))   # unsuffixed name first, then most used


def reuse_materials(objs, before):
    """Swap freshly imported materials for ones that already existed with the same name and textures, so
    edits made in Blender survive a delete and re-import. Materials with no match are left as imported."""
    if not _KEEP:
        return
    dropped = set()
    for o in objs:
        if o.type != 'MESH':
            continue
        for slot in o.material_slots:
            new = slot.material
            if new is None or new in before:
                continue
            want = mat_textures(new)
            if not want:
                continue
            for old in _KEEP.get(mat_base(new.name), []):
                if old is new or old.name not in bpy.data.materials:
                    continue
                if want <= mat_textures(old):
                    slot.material = old
                    old.use_fake_user = True    # survives while no model uses it
                    _ALPHA_DONE.add(old.name)   # keep its blend mode as you left it
                    dropped.add(new)
                    break
    for m in dropped:
        if m.users == 0:
            bpy.data.materials.remove(m)


def import_obj(path):
    before = set(bpy.data.objects)
    before_mats = set(bpy.data.materials)
    if hasattr(bpy.ops.wm, 'obj_import'):
        bpy.ops.wm.obj_import(filepath=path, forward_axis='NEGATIVE_Z', up_axis='Y')
    else:
        bpy.ops.import_scene.obj(filepath=path, axis_forward='-Z', axis_up='Y')
    new = [o for o in bpy.data.objects if o not in before]
    reuse_materials(new, before_mats)
    apply_alpha_modes(path, new)
    return new


def make_empty(name, col, m, target):
    e = bpy.data.objects.new(name, None)
    e.instance_type = 'COLLECTION'
    e.instance_collection = col
    e.empty_display_size = 0.01
    e.matrix_world = m
    target.objects.link(e)
    return e


root = bpy.data.collections.new('%s_models' % SCENE_NAME)
bpy.context.scene.collection.children.link(root)
placed = bpy.data.collections.new('%s_instances' % SCENE_NAME)
bpy.context.scene.collection.children.link(placed)
choices = None

# 1. one library collection per model version
col_objs = {}   # collection name -> objects imported into it
lib = {}   # fid -> (index of the version to show, [(label, collection), ...])
for fid, info in data['models'].items():
    versions = info['versions']
    cols = []
    for label, obj_name in versions:
        path = os.path.join(MODELS_DIR, obj_name)
        try:
            objs = import_obj(path)
        except Exception as e:
            print('Import failed for', path, e)
            continue
        if not objs:
            continue
        cname = 'model_%s' % fid if len(versions) == 1 else 'model_%s_%s' % (fid, label)
        col = bpy.data.collections.new(cname)
        root.children.link(col)
        for o in objs:
            for c in list(o.users_collection):
                c.objects.unlink(o)
            col.objects.link(o)
        col_objs[col.name] = objs
        cols.append((label, col))
    if cols:
        lib[fid] = (min(info.get('show', 0), len(cols) - 1), cols)


def bounds(objs):
    mn = Vector((1e30, 1e30, 1e30))
    mx = Vector((-1e30, -1e30, -1e30))
    found = False
    for o in objs:
        if o.type != 'MESH':
            continue
        for corner in o.bound_box:
            w = o.matrix_world @ Vector(corner)
            for i in range(3):
                mn[i] = min(mn[i], w[i])
                mx[i] = max(mx[i], w[i])
            found = True
    if not found:
        return Vector((0, 0, 0)), Vector((0, 0, 0))
    return mn, mx


def layout_library():
    # Every model is built around the origin, so they all overlap there. Move each one to its own grid cell
    # (versions of one model stacked vertically) and set the version collection's instance_offset to the same
    # displacement. An instance draws the collection's objects minus that offset, so the instances land
    # exactly where they did before; only the library copies you inspect are spread out.
    bpy.context.view_layer.update()
    items = []
    for fid in sorted(lib, key=lambda k: int(k)):
        vs = []
        for label, col in lib[fid][1]:
            objs = col_objs.get(col.name, [])
            mn, mx = bounds(objs)
            vs.append({'col': col, 'objs': objs, 'mn': mn, 'mx': mx})
        items.append({
            'fid': fid, 'versions': vs,
            'w': max(v['mx'].x - v['mn'].x for v in vs),
            'd': max(v['mx'].y - v['mn'].y for v in vs),
        })
    if not items:
        return
    biggest = max(max(it['w'], it['d']) for it in items) or 1.0
    gap = biggest * GAP
    total = sum((it['w'] + gap) * (it['d'] + gap) for it in items)
    row_limit = max(total ** 0.5 * ROW_WIDTH_FACTOR, max(it['w'] for it in items) + gap)
    x = y = row_d = 0.0
    for it in items:
        if x > 0 and x + it['w'] > row_limit:
            x = 0.0
            y += row_d + gap
            row_d = 0.0
        cx = x + it['w'] / 2.0
        cy = y + it['d'] / 2.0
        z = 0.0
        for v in it['versions']:
            mn, mx = v['mn'], v['mx']
            delta = Vector((cx - (mn.x + mx.x) / 2.0, cy - (mn.y + mx.y) / 2.0, z - mn.z))
            for o in v['objs']:
                if o.parent is None:
                    o.matrix_world = Matrix.Translation(delta) @ o.matrix_world
            v['col'].instance_offset = delta
            h = mx.z - mn.z
            z += h + max(h * STACK_GAP, biggest * 0.02)
        marker = bpy.data.objects.new('model_%s' % it['fid'], None)
        marker.empty_display_type = 'PLAIN_AXES'
        marker.empty_display_size = biggest * 0.05
        marker.location = (x, y, 0.0)
        root.objects.link(marker)      # in root itself, not in a version collection, so it is never instanced
        x += it['w'] + gap
        row_d = max(row_d, it['d'])


if LIBRARY_GRID:
    layout_library()
    # The library stays visible so you can inspect it. Untick it in the Outliner (exclude) to hide it;
    # instances are not affected by that.
else:
    # hide the library but keep it instanceable
    bpy.context.view_layer.layer_collection.children[root.name].exclude = True

# 2. one placement per instance
insts = data['instances']
if LIMIT > 0:
    insts = insts[:LIMIT]
count = 0
with_choice = 0
for n, (fid, px, py, pz, rx, ry, rz) in enumerate(insts):
    entry = lib.get(str(fid))
    if entry is None:
        continue
    show, cols = entry
    R = C @ Euler((rx, ry, rz), EULER_ORDER).to_matrix() @ C_INV
    p = C @ Vector((px, py, pz))
    m = Matrix.Translation(p * UNIT_SCALE) @ (R * UNIT_SCALE).to_4x4()
    m[3][3] = 1.0
    if len(cols) == 1:
        make_empty('%s_%d' % (fid, n), cols[0][1], m, placed)
    else:
        if choices is None:
            choices = bpy.data.collections.new('%s_choices' % SCENE_NAME)
            bpy.context.scene.collection.children.link(choices)
        grp = bpy.data.collections.new('%s_%d' % (fid, n))
        choices.children.link(grp)
        for k, (label, col) in enumerate(cols):
            e = make_empty('%s_%d_%s' % (fid, n, label), col, m, grp)
            if k != show:
                e.hide_set(True)
                e.hide_render = True
        with_choice += 1
    count += 1

print('Placed %d instance(s) of %d model(s); %d placement(s) have several versions (one visible, the rest hidden in place).' % (count, len(lib), with_choice))
"#;

const GRID_TEMPLATE: &str = r#"# Generated by land_export --scene --scene-grid. Run in Blender: Scripting tab -> Open -> Run Script.
# Lays out every model once. All versions of one model (alliance banners/shields, ...) sit in the same cell,
# stacked vertically with a small gap, so you can see each of them and know they come from one model file.
# Each model has a collection 'model_<fileindex>' (with one sub-collection per version) and a small axes empty
# named 'model_<fileindex>' at the corner of its cell.
import bpy, json, os
from mathutils import Vector

SCENE_NAME = '@NAME@'
SIDECAR = @SIDECAR@
BAKED_SCENES_DIR = @SCENES_ABS@
BAKED_MODELS_DIR = @MODELS_ABS@

# ---- knobs ---------------------------------------------------------------------------------
GAP = 0.15               # space between neighbouring cells, as a fraction of the largest model
STACK_GAP = 0.10         # space between stacked versions, as a fraction of the version's height (min 2% of largest model)
ROW_WIDTH_FACTOR = 1.3   # wider = fewer, longer rows
KEEP_EXISTING_MATERIALS = True   # reuse materials already in this .blend (same name + same textures) instead of the freshly imported ones
# ---------------------------------------------------------------------------------------------

try:
    HERE = os.path.dirname(os.path.abspath(__file__))
except NameError:
    HERE = BAKED_SCENES_DIR
if not os.path.exists(os.path.join(HERE, SIDECAR)):
    HERE = BAKED_SCENES_DIR
MODELS_DIR = os.path.normpath(os.path.join(HERE, '..', 'models'))
if not os.path.isdir(MODELS_DIR):
    MODELS_DIR = BAKED_MODELS_DIR

with open(os.path.join(HERE, SIDECAR), 'r') as fh:
    data = json.load(fh)


import re

_ALPHA_DONE = set()


def read_alpha_modes(obj_path):
    """{material name: 'mask' | 'blend'} from the '# alpha_mode' lines the exporter writes into the .mtl."""
    modes = {}
    mtl = None
    try:
        with open(obj_path, 'r') as fh:
            for line in fh:
                if line.startswith('mtllib '):
                    mtl = line.split(None, 1)[1].strip()
                    break
        if mtl is None:
            return modes
        cur = None
        with open(os.path.join(os.path.dirname(obj_path), mtl), 'r') as fh:
            for line in fh:
                line = line.strip()
                if line.startswith('newmtl '):
                    cur = line.split(None, 1)[1].strip()
                elif line.startswith('# alpha_mode ') and cur:
                    modes[cur] = line.split()[-1]
    except OSError as e:
        print('Could not read alpha modes for', obj_path, e)
    return modes


def set_alpha_mode(mat, mode):
    """mask = hard cutout (alpha test), blend = true transparency."""
    if mat is None or mat.name in _ALPHA_DONE:
        return
    _ALPHA_DONE.add(mat.name)
    if mode == 'blend':
        if hasattr(mat, 'surface_render_method'):      # Blender 4.2+
            mat.surface_render_method = 'BLENDED'
        else:
            mat.blend_method = 'BLEND'
            if hasattr(mat, 'show_transparent_back'):
                mat.show_transparent_back = False
        return
    # mask
    if hasattr(mat, 'blend_method') and not hasattr(mat, 'surface_render_method'):   # before 4.2
        mat.blend_method = 'CLIP'
        mat.alpha_threshold = 0.5
        return
    # 4.2+ has no clip mode: threshold the alpha so a pixel is fully drawn or fully discarded
    if not mat.use_nodes:
        return
    nt = mat.node_tree
    for node in list(nt.nodes):
        if node.type != 'BSDF_PRINCIPLED':
            continue
        sock = node.inputs['Alpha']
        if not sock.is_linked:
            continue
        link = sock.links[0]
        if link.from_node.type == 'MATH' and link.from_node.operation == 'GREATER_THAN':
            continue
        th = nt.nodes.new('ShaderNodeMath')
        th.operation = 'GREATER_THAN'
        th.inputs[1].default_value = 0.5
        th.location = (node.location.x - 200, node.location.y - 300)
        nt.links.new(link.from_socket, th.inputs[0])
        nt.links.new(th.outputs[0], sock)


def apply_alpha_modes(obj_path, objs):
    modes = read_alpha_modes(obj_path)
    if not modes:
        return
    for o in objs:
        if o.type != 'MESH':
            continue
        for slot in o.material_slots:
            mat = slot.material
            if mat is None:
                continue
            base = mat.name if mat.name in modes else re.sub(r'\.\d+$', '', mat.name)
            if base in modes:
                set_alpha_mode(mat, modes[base])


_KEEP = {}   # base material name -> [materials that existed before this script ran]


def mat_base(name):
    return re.sub(r'\.\d+$', '', name)


def mat_textures(mat):
    """Set of texture file names a material uses."""
    out = set()
    if mat is not None and mat.use_nodes and mat.node_tree:
        for n in mat.node_tree.nodes:
            if n.type == 'TEX_IMAGE' and n.image:
                out.add(os.path.basename(n.image.filepath) or n.image.name)
    return out


if KEEP_EXISTING_MATERIALS:
    for _m in bpy.data.materials:
        _KEEP.setdefault(mat_base(_m.name), []).append(_m)
    for _l in _KEEP.values():
        _l.sort(key=lambda m: (mat_base(m.name) != m.name, -m.users))   # unsuffixed name first, then most used


def reuse_materials(objs, before):
    """Swap freshly imported materials for ones that already existed with the same name and textures, so
    edits made in Blender survive a delete and re-import. Materials with no match are left as imported."""
    if not _KEEP:
        return
    dropped = set()
    for o in objs:
        if o.type != 'MESH':
            continue
        for slot in o.material_slots:
            new = slot.material
            if new is None or new in before:
                continue
            want = mat_textures(new)
            if not want:
                continue
            for old in _KEEP.get(mat_base(new.name), []):
                if old is new or old.name not in bpy.data.materials:
                    continue
                if want <= mat_textures(old):
                    slot.material = old
                    old.use_fake_user = True    # survives while no model uses it
                    _ALPHA_DONE.add(old.name)   # keep its blend mode as you left it
                    dropped.add(new)
                    break
    for m in dropped:
        if m.users == 0:
            bpy.data.materials.remove(m)


def import_obj(path):
    before = set(bpy.data.objects)
    before_mats = set(bpy.data.materials)
    if hasattr(bpy.ops.wm, 'obj_import'):
        bpy.ops.wm.obj_import(filepath=path, forward_axis='NEGATIVE_Z', up_axis='Y')
    else:
        bpy.ops.import_scene.obj(filepath=path, axis_forward='-Z', axis_up='Y')
    new = [o for o in bpy.data.objects if o not in before]
    reuse_materials(new, before_mats)
    apply_alpha_modes(path, new)
    return new


def bounds(objs):
    mn = Vector((1e30, 1e30, 1e30))
    mx = Vector((-1e30, -1e30, -1e30))
    found = False
    for o in objs:
        if o.type != 'MESH':
            continue
        for corner in o.bound_box:
            w = o.matrix_world @ Vector(corner)
            for i in range(3):
                mn[i] = min(mn[i], w[i])
                mx[i] = max(mx[i], w[i])
            found = True
    if not found:
        return Vector((0, 0, 0)), Vector((0, 0, 0))
    return mn, mx


root = bpy.data.collections.new(SCENE_NAME)
bpy.context.scene.collection.children.link(root)

# 1. import every version of every model
items = []
for fid, versions in data['models']:
    grp = bpy.data.collections.new('model_%s' % fid)
    root.children.link(grp)
    vs = []
    for label, obj_name in versions:
        path = os.path.join(MODELS_DIR, obj_name)
        try:
            objs = import_obj(path)
        except Exception as e:
            print('Import failed for', path, e)
            continue
        if not objs:
            continue
        col = bpy.data.collections.new('model_%s_%s' % (fid, label))
        grp.children.link(col)
        for o in objs:
            for c in list(o.users_collection):
                c.objects.unlink(o)
            col.objects.link(o)
        vs.append({'label': label, 'objs': objs})
    if vs:
        items.append({'fid': fid, 'grp': grp, 'versions': vs})

bpy.context.view_layer.update()
if not items:
    raise RuntimeError('Nothing could be imported from %s' % MODELS_DIR)

for it in items:
    it['w'] = 0.0
    it['d'] = 0.0
    for v in it['versions']:
        v['mn'], v['mx'] = bounds(v['objs'])
        it['w'] = max(it['w'], v['mx'].x - v['mn'].x)
        it['d'] = max(it['d'], v['mx'].y - v['mn'].y)

# 2. lay the cells out in rows
biggest = max(max(it['w'], it['d']) for it in items) or 1.0
gap = biggest * GAP
total = sum((it['w'] + gap) * (it['d'] + gap) for it in items)
row_limit = max(total ** 0.5 * ROW_WIDTH_FACTOR, max(it['w'] for it in items) + gap)

x = y = row_d = 0.0
for it in items:
    if x > 0 and x + it['w'] > row_limit:
        x = 0.0
        y += row_d + gap
        row_d = 0.0
    cx = x + it['w'] / 2.0
    cy = y + it['d'] / 2.0
    z = 0.0
    for v in it['versions']:
        mn, mx = v['mn'], v['mx']
        delta = Vector((cx - (mn.x + mx.x) / 2.0, cy - (mn.y + mx.y) / 2.0, z - mn.z))
        for o in v['objs']:
            o.location = o.location + delta
        h = mx.z - mn.z
        z += h + max(h * STACK_GAP, biggest * 0.02)
    marker = bpy.data.objects.new('model_%s' % it['fid'], None)
    marker.empty_display_type = 'PLAIN_AXES'
    marker.empty_display_size = biggest * 0.05
    marker.location = (x, y, 0.0)
    it['grp'].objects.link(marker)
    x += it['w'] + gap
    row_d = max(row_d, it['d'])

print('Laid out %d model(s); %d have several versions stacked vertically.' % (len(items), sum(1 for it in items if len(it['versions']) > 1)))
"#;