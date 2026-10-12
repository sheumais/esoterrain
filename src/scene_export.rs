//! Turn placement JSON files into a Blender scene made of *references*.
//!
//! Pipeline:
//!   1. Read one or more placement JSONs (world `placement.json` and/or `cell_x#_z#.json`).
//!   2. Export every distinct model (`model_type_id` == MNF FileIndex) through `model_export::run_batch`
//!      (each needed archive is dumped once, not one `-f` call per file), skipping models whose
//!      `<index>.obj` is already in the models folder.
//!   3. Write `<name>.json` (compact instance list) and `<name>.py` (Blender import script).
//!
//! In Blender the script imports each unique OBJ once into its own hidden collection and then
//! creates one *collection-instance empty* per placement. Edit a model once and every instance
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
}

pub fn run(a: &SceneArgs, mo: &Options) -> Result<PathBuf, String> {
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

    let rows = model_export::lookup_file_indexes(mo.manifest, &ids.iter().copied().collect())
        .map_err(|e| format!("Failed to read manifest '{}': {}", mo.manifest, e))?;
    let obj_path = |index: u32| mo.out_root.join(format!("{}.obj", index));

    let mut index_of: HashMap<u32, u32> = HashMap::new();
    let mut not_in_manifest = 0usize;
    for id in &ids {
        match rows.get(id).and_then(|r| r.first()) {
            Some(r) => { index_of.insert(*id, r.index); }
            None => { eprintln!("  Model FileIndex 0x{:08X} ({}) not in manifest; its instances will be dropped.", id, id); not_in_manifest += 1; }
        }
    }

    let todo: Vec<u32> = ids.iter().copied().filter(|id| index_of.get(id).map_or(false, |i| !obj_path(*i).exists())).collect();
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

    let mut models = serde_json::Map::new();
    for id in &ids {
        if let Some(idx) = index_of.get(id) {
            if obj_path(*idx).exists() {
                models.insert(id.to_string(), Value::from(format!("{}.obj", idx)));
            }
        }
    }
    let kept: Vec<&Inst> = insts.iter().filter(|i| models.contains_key(&i.model.to_string())).collect();
    let dropped = insts.len() - kept.len();
    println!("\n{} model(s) have an OBJ; {} instance(s) kept, {} dropped (model missing).", models.len(), kept.len(), dropped);
    if kept.is_empty() {
        return Err("No instances have an exported OBJ; nothing to write.".into());
    }

    fs::create_dir_all(a.scenes_dir).map_err(|e| format!("Failed to create '{}': {}", a.scenes_dir.display(), e))?;
    let name = a.name.map(|s| s.to_string()).unwrap_or_else(|| default_name(a.inputs));

    let inst_json: Vec<Value> = kept.iter().map(|i| json!([i.model, i.pos[0], i.pos[1], i.pos[2], i.rot[0], i.rot[1], i.rot[2]])).collect();
    let sidecar = json!({ "models": Value::Object(models), "instances": inst_json });
    let json_name = format!("{}.json", name);
    fs::write(a.scenes_dir.join(&json_name), serde_json::to_string(&sidecar).unwrap())
        .map_err(|e| format!("Failed to write scene json: {}", e))?;

    let models_abs = plain_abs(mo.out_root);
    let script = BLENDER_TEMPLATE
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


def import_obj(path):
    before = set(bpy.data.objects)
    if hasattr(bpy.ops.wm, 'obj_import'):
        bpy.ops.wm.obj_import(filepath=path, forward_axis='NEGATIVE_Z', up_axis='Y')
    else:
        bpy.ops.import_scene.obj(filepath=path, axis_forward='-Z', axis_up='Y')
    return [o for o in bpy.data.objects if o not in before]


root = bpy.data.collections.new('%s_models' % SCENE_NAME)
bpy.context.scene.collection.children.link(root)
placed = bpy.data.collections.new('%s_instances' % SCENE_NAME)
bpy.context.scene.collection.children.link(placed)

# 1. one library collection per model
lib = {}
for fid, obj_name in data['models'].items():
    path = os.path.join(MODELS_DIR, obj_name)
    try:
        objs = import_obj(path)
    except Exception as e:
        print('Import failed for', path, e)
        continue
    if not objs:
        continue
    col = bpy.data.collections.new('model_%s' % fid)
    root.children.link(col)
    for o in objs:
        for c in list(o.users_collection):
            c.objects.unlink(o)
        col.objects.link(o)
    lib[fid] = col

# hide the library but keep it instanceable
bpy.context.view_layer.layer_collection.children[root.name].exclude = True

# 2. one empty per placement, instancing the model's collection
insts = data['instances']
if LIMIT > 0:
    insts = insts[:LIMIT]
count = 0
for n, (fid, px, py, pz, rx, ry, rz) in enumerate(insts):
    col = lib.get(str(fid))
    if col is None:
        continue
    R = C @ Euler((rx, ry, rz), EULER_ORDER).to_matrix() @ C_INV
    p = C @ Vector((px, py, pz))
    m = Matrix.Translation(p * UNIT_SCALE) @ (R * UNIT_SCALE).to_4x4()
    m[3][3] = 1.0
    e = bpy.data.objects.new('%s_%d' % (fid, n), None)
    e.instance_type = 'COLLECTION'
    e.instance_collection = col
    e.empty_display_size = 0.01
    e.matrix_world = m
    placed.objects.link(e)
    count += 1

print('Placed %d instance(s) of %d model(s).' % (count, len(lib)))
"#;