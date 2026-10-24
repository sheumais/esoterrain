mod export;
mod landfile;
mod lodmesh;
mod manifest;
mod reader;
mod colourmap;
mod placement;
mod worldlut;
mod model;
mod creature;
mod items;
mod cell_placement;
mod sizemap;
mod model_export;
mod scene_export;
mod keep_export;
mod world_models;
mod terrain_textures;
mod model_summary;
mod terrain_layers;

use landfile::{fixed_channel_range, save_rgb_from_channels, LandFile, RawBigImage, RawTile};
use manifest::filter_manifest;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::Path;
use walkdir::WalkDir;

use crate::colourmap::{ColourMapGrid};
use crate::landfile::save_rgba_from_channels;
use crate::manifest::filter_colourmap_manifest;

const DEFAULT_EXE: &str = "C:/Users/H/Documents/EsoExtractData v0.53/EsoExtractData.exe";
const DEFAULT_OUT: &str = "./temp"; // WILL GET DELETED AFTER
const FINAL_EXPORT_DIR: &str = "./export";
const LUT_DIR: &str = "./luts"; // all *_lut.csv files; per-world model LUTs go in LUT_DIR/worlds/
const MNF_EXPORT: &str = "eso_mnf.csv"; // can't upload to github due to file size limits.
const RAW_MNF: &str = "C:/Program Files (x86)/Steam/steamapps/common/Zenimax Online/The Elder Scrolls Online/depot/eso.mnf";
const NOESIS: &str = "C:/Users/H/Documents/EsoExtractData v0.53/noesisv4474/Noesis64.exe";
const MAX_SANE_GRID_SPAN: u32 = 128;

struct Args {
    manifest: String,
    world: u32,
    exe: String,
    mnf: String,
    out: String,
    dry_run: bool,
    skip_export: bool,
    no_cleanup: bool,
    channels: Option<Vec<usize>>,
    placement_only: bool,
    lod_only: bool,
    lod_no_skirts: bool,
    list_worlds: bool,
    world_lut: bool,
    model_list: bool,
    creature_list: bool,
    item_list: bool,
    size_map: bool,
    size_type: Option<u32>,
    size_scale: u32,
    model_fis: Vec<u32>,
    creature_ids: Vec<u32>,
    item_models: Vec<u32>,
    rig_format: String,
    noesis: String,
    scene: Vec<String>,
    cell_size: f64,
    scene_name: Option<String>,
    scene_script_only: bool,
    scene_grid: bool,
    keeps: bool,
    keep_key: Option<u32>,
    keep_list: bool,
    alliance: Option<String>,
    no_auto_variants: bool,
    world_model_lut: bool,
    lut_worlds: Vec<u32>,
    lut_rot: bool,
    all_luts: bool,
    model_summary: bool,
    terrain_textures: bool,
    terrain_layers: Option<u32>,
    terrain_layer_only: Vec<(u32, u32)>, // (kind, layer id) pairs for --terrain-layer-id
    terrain_layer_split: Option<Vec<u32>>, // --terrain-layer-split: kinds to write one file per id for (empty = all kinds)
}

fn parse_int_arg(s: &str) -> u32 {
    let s = s.trim();
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        u32::from_str_radix(hex, 16).unwrap_or_else(|_| panic!("invalid hex value: {}", s))
    } else {
        s.parse::<u32>()
            .unwrap_or_else(|_| panic!("invalid integer value: {}", s))
    }
}

fn parse_channels_arg(s: &str) -> Vec<usize> {
    s.split(',')
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .map(|part| {
            part.parse::<usize>()
                .unwrap_or_else(|_| panic!("invalid channel index: {}", part))
        })
        .collect()
}

/// Names follow the world TOC: its `terrain-*` .dat entries, in order, are the land file's 10 records.
/// 0/6 are confirmed by their data (heights, unit normals); 2-5 sum to ~255 per pixel across their 16 bytes (blend
/// weights); 7 is ~all 0xFFFF with a few 0s (hole mask); the rest are matched by TOC order and shape only.
fn channel_label(index: usize) -> String {
    match index {
        0 => "00_heightmap".to_string(),
        1 => "01_color_raw".to_string(),
        2 => "02_blend0".to_string(),
        3 => "03_blend1".to_string(),
        4 => "04_blend2".to_string(),
        5 => "05_blend3".to_string(),
        6 => "06_normal".to_string(),
        7 => "07_hole".to_string(),
        8 => "08_clutter".to_string(),
        9 => "09_clutter_pts".to_string(),
        other => format!("{:02}_unknown", other),
    }
}

fn write_placements(
    scan_dir: &str,
    world_pm: Option<&manifest::PlacementMatch>,
    cell_pms: &[manifest::CellPlacementMatch],
    output_dir: &Path,
) -> usize {
    let mut paths: HashMap<u32, std::path::PathBuf> = HashMap::new();
    for entry in WalkDir::new(scan_dir).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() { continue; }
        if let Some(idx) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u32>().ok()) {
            paths.entry(idx).or_insert_with(|| path.to_path_buf());
        }
    }
    let mut failures = 0usize;

    if let Some(pm) = world_pm {
        match paths.get(&pm.index) {
            Some(path) => {
                let out_path = output_dir.join("placement.json");
                match placement::parse_and_write(path, &out_path) {
                    Ok(()) => println!("Parsed placement/instance list -> {}", out_path.display()),
                    Err(e) => { eprintln!("Error parsing placement file '{}': {}", path.display(), e); failures += 1; }
                }
            }
            None => { eprintln!("Warning: placement file (index {}) not found under '{}'.", pm.index, scan_dir); failures += 1; }
        }
    }

    if !cell_pms.is_empty() {
        let cell_dir = output_dir.join("cell_placement");
        let mut ok = 0usize;
        for c in cell_pms {
            let Some(path) = paths.get(&c.index) else {
                eprintln!("Warning: cell placement file (index {}, cell {},{}) not found under '{}'.", c.index, c.x, c.z, scan_dir);
                failures += 1;
                continue;
            };
            let out_path = cell_dir.join(format!("cell_x{}_z{}.json", c.x, c.z));
            match cell_placement::parse_and_write_cell(path, &out_path) {
                Ok(()) => ok += 1,
                Err(e) => { eprintln!("Error parsing cell placement '{}': {}", path.display(), e); failures += 1; }
            }
        }
        println!("Parsed {}/{} cell placement files -> {}", ok, cell_pms.len(), cell_dir.display());
    }
    failures
}

fn index_files(scan_dir: &str) -> HashMap<u32, std::path::PathBuf> {
    let mut paths: HashMap<u32, std::path::PathBuf> = HashMap::new();
    for entry in WalkDir::new(scan_dir).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() { continue; }
        if let Some(idx) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u32>().ok()) {
            paths.entry(idx).or_insert_with(|| path.to_path_buf());
        }
    }
    paths
}

/// Terrain LOD meshes (manifest type 0x0A) -> `<output_dir>/lod_x<X>_z<Z>_ch<N>.obj`, one per record in each file.
/// Returns the number of failed files.
fn write_lod_meshes(
    scan_dir: &str,
    lods: &[manifest::LodMatch],
    cell_size: f64,
    skirts: bool,
    output_dir: &Path,
) -> usize {
    let paths = index_files(scan_dir);
    let (mut ok_files, mut obj_count, mut failures) = (0usize, 0usize, 0usize);
    for l in lods {
        let Some(path) = paths.get(&l.index) else {
            eprintln!("Warning: LOD mesh file (index {}, cell {},{}) not found under '{}'.", l.index, l.x, l.z, scan_dir);
            failures += 1;
            continue;
        };
        match lodmesh::export_file(path, l.x, l.z, cell_size, skirts, output_dir) {
            Ok(written) => { ok_files += 1; obj_count += written.len(); }
            Err(e) => { eprintln!("Error parsing LOD mesh '{}': {}", path.display(), e); failures += 1; }
        }
    }
    println!("Exported {}/{} terrain LOD files ({} OBJ) -> {}", ok_files, lods.len(), obj_count, output_dir.display());
    failures
}

fn print_usage() {
    eprintln!(
        "Usage: land_export (or cargo run) <world> [options]\n\n\
Required:\n  \
<world>              World ID, hex (0xe9) or decimal\n\n\
Options:\n  \
--manifest <path>    Path to manifest export (default: {})\n  \
--mnf <path>         Path to raw .mnf (default: {})\n  \
--exe <path>         Path to esoextractdata.exe (default: {})\n  \
--out <dir>          Export/working directory (default: {})\n  \
--channels <list>    Comma-separated channel indices to export (default: all found)\n  \
--dry-run            Show what would happen without exporting/deleting\n  \
--skip-export        Skip calling the exporter; assume files are already on disk\n  \
--no-cleanup         Keep every file the archive export dumped\n  \
--no-auto-variants  Don't split overlapping meshes (alternative versions) into separate OBJs; by default every\n                       model with such meshes is exported once per version (<index>_<mesh>.obj)\n  \
--model <FileIndex>  Export a model + textures + .obj/.mtl into export/models/ (one flat folder).\n                       Repeat the flag or use commas for several: --model 0x15b253,0x15b254\n  \
--creature <id>      Export a creature's model + diffuse/normal/specular textures + .obj/.mtl into export/models/.\n                       <id> is a table_id (first column) or creature_id (second column) of luts/creatures_lut.csv;\n                       repeat the flag or use commas for several. Run --creature-lut first to build the CSV.\n  \
--item-model <id>    Export an item's model(s) + diffuse/normal/specular textures + .obj/.mtl into export/models/.\n                       <id> is a template_id (first column) of luts/items_lut.csv; every part of the item is exported\n                       (plus a part's second model, if it has one). Repeat the flag or use commas for several.\n                       Run --item-lut first. Files are extracted one by one, never as whole archives.\n  \
--rig-format <ext>   With --creature: format of the skinned (mesh + skeleton + weights) export written by Noesis,\n                       by extension (default fbx; others to try: glb, dae, smd)\n  \
--noesis <path>      Path to Noesis64.exe (default: {})\n  \
--scene <paths>      Export every model used by placement JSON file(s) or folder(s) (comma-separated or repeated;\n                       world placement.json and cell_x#_z#.json both work), then write a Blender script that\n                       places them as collection-instance references into export/scenes/\n  \
--cell-size <n>      World units per cell for cell_x#_z# files and the LOD meshes (default 250; 0 = don't offset)\n  \
--scene-name <name>  Output name for the scene .py/.json (default: derived from the world_0x.. folder)\n  \
--scene-script-only  With --scene: don't export models, only (re)write the scene files from existing OBJs\n  \
--scene-grid         With --scene: write <name>_grid.py instead of the placed scene: every model once, in a grid, with all of its\n                       versions stacked vertically in its cell\n  \
--alliance <name>    With --scene: which version of a multi-version model starts visible, e.g. daggerfall, ebonheart, aldmeri\n                       (default: the first one). The other versions stay hidden in the same spot; unhide one in Blender to switch.\n  \
--size-map           Write a rough zone-shape image from manifest file sizes (needs <world>; no export)\n  \
--size-type <n>      File type (low 5 bits of Unk1) to use for --size-map (default: 0x15, cell placement files)\n  \
--size-scale <n>     Pixels per cell in the --size-map image (default: 16)\n  \
--placement-only     Only export/parse the placement (instance) file for this world; skip terrain/colourmap entirely\n  \
--lod-only           Only export the terrain LOD meshes (file type 0x0A, one per 2x2 cells) for this world as OBJ files in\n                       export/world_0x<id>/terrain_lod/; skip everything else\n  \
--lod-no-skirts      Leave the edge-skirt triangles out of the LOD meshes (by default they are a separate `g skirt` group)\n  \
--list-worlds        List every world found in the manifest with terrain cell data, and its grid size (no export/world arg needed)\n \
--world-lut          Export the world table and write luts/worlds_lut.csv\n \
--model-lut          Export the models table and write luts/models_lut.csv\n \
--creature-lut       Export the creatures table, creature models and creature textures table and write luts/creatures_lut.csv\n \
--keep-lut           Export the keeps table (0x5a) and write luts/keeps_lut.csv\n \
--item-lut           Export the item visual templates (0x58), part materials (0x50), materials (0x05) and item names (0x171) tables and write luts/items_lut.csv\n \
--terrain-textures  Parse the terrain texture table (file id 0x29), export every texture it references into export/terrain_textures/\n                       and write luts/terrain_textures_lut.csv. Textures already exported are skipped, so it can be re-run/resumed.\n                       Archives needing fewer than 10 of the textures are exported file-by-file instead of as a whole archive\n \
--terrain-layers <world>  Export the terrain layer table (0x44) and stitch one world into export/world_0x<id>/terrain_layers/:\n                       one k<kind>_ids.png per kind (colour = layer id) plus k<kind>_legend.csv; kind 2 is the zone map and its legend has the zone names\n \
--terrain-layer-split <kinds>  With --terrain-layers: write EVERY layer id of the given kinds to its own file, nothing drawn over anything else.\n                       <kinds> = 6, 6,2 or all. Writes k<kind>_layers/id_<id>.png (white = cells where that id is set; same canvas as the full images)\n                       plus k<kind>_layers/index.csv (layer_id,r,g,b,pixels,blocks,x0,y0,x1,y1,name). The viewer can load these folders.\n--terrain-layer-id <kind>:<id>  With --terrain-layers: export ONLY that layer value instead of every id. Writes k<kind>_id<id>.png\n                       (white = cells where that id is set, black = not) on the same canvas as the full images, e.g. --terrain-layers 0x47f --terrain-layer-id 6:3732\n                       Several: 6:3732,6:3771 (comma-separated or repeat the flag). The kind may be written 6 or k6.\n \
--world-model-lut    For EVERY world: export its placement files and write luts/worlds/world_0x<id>.csv\n  \
                       (model_id,x,y,z; sorted; from the world placement file only). Each archive is exported once.\n \
--model-summary      Read every luts/worlds/world_0x<id>.csv and write luts/model_usage_summary.csv (each model: total count, worlds it appears in)\n                       and luts/model_manifest_entries.csv (the raw manifest line(s) for each model). Also runs automatically after --world-model-lut / --all-luts\n \
--worlds <list>      With --world-model-lut: only these worlds (comma-separated, hex or decimal)\n \
--with-rot           With --world-model-lut: also write rx,ry,rz columns\n \
--all-luts           Write every LUT (run once per patch): the world, model, creature, item and keep LUTs share one archive export, then\n                       --world-model-lut runs. --exe/--mnf/--manifest/--out/--worlds/--with-rot/--dry-run/--no-cleanup apply\n \
--
",
        MNF_EXPORT, RAW_MNF, DEFAULT_EXE, DEFAULT_OUT, NOESIS
    );
}

fn parse_args() -> Args {
    let raw: Vec<String> = env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut manifest = MNF_EXPORT.to_string();
    let mut exe = DEFAULT_EXE.to_string();
    let mut mnf = RAW_MNF.to_string();
    let mut out = DEFAULT_OUT.to_string();
    let mut dry_run = false;
    let mut skip_export = false;
    let mut no_cleanup = false;
    let mut channels: Option<Vec<usize>> = None;
    let mut placement_only = false;
    let mut lod_only = false;
    let mut lod_no_skirts = false;
    let mut list_worlds = false;
    let mut world_lut = false;
    let mut model_list = false;
    let mut creature_list = false;
    let mut item_list = false;
    let mut size_map = false;
    let mut size_type: Option<u32> = None;
    let mut size_scale: u32 = 16;
    let mut model_fis: Vec<u32> = Vec::new();
    let mut creature_ids: Vec<u32> = Vec::new();
    let mut item_models: Vec<u32> = Vec::new();
    let mut rig_format = String::from("fbx");
    let mut noesis = NOESIS.to_string();
    let mut scene: Vec<String> = Vec::new();
    let mut cell_size: f64 = 250.0;
    let mut scene_name: Option<String> = None;
    let mut scene_script_only = false;
    let mut scene_grid = false;
    let mut keeps = false;
    let mut keep_list = false;
    let mut keep_key = None;
    let mut alliance: Option<String> = None;
    let mut no_auto_variants = false;
    let mut world_model_lut = false;
    let mut lut_worlds: Vec<u32> = Vec::new();
    let mut lut_rot = false;
    let mut all_luts = false;
    let mut model_summary = false;
    let mut terrain_textures = false;
    let mut tl_world: Option<u32> = None;
    let mut tl_only: Vec<(u32, u32)> = Vec::new();
    let mut tl_split: Option<Vec<u32>> = None;

    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--manifest" => {
                i += 1;
                manifest = raw.get(i).cloned().expect("--manifest needs a value");
            }
            "--exe" => {
                i += 1;
                exe = raw.get(i).cloned().expect("--exe needs a value");
            }
            "--mnf" => {
                i += 1;
                mnf = raw.get(i).cloned().expect("--mnf needs a value");
            }
            "--out" => {
                i += 1;
                out = raw.get(i).cloned().expect("--out needs a value");
            }
            "--channels" => {
                i += 1;
                let val = raw.get(i).expect("--channels needs a value");
                channels = Some(parse_channels_arg(val));
            }
            "--dry-run" => dry_run = true,
            "--skip-export" => skip_export = true,
            "--no-cleanup" => no_cleanup = true,
            "--no-auto-variants" => no_auto_variants = true,
            "--placement-only" => placement_only = true,
            "--lod-only" => lod_only = true,
            "--lod-no-skirts" => lod_no_skirts = true,
            "--list-worlds" => list_worlds = true,
            "--world-lut" => world_lut = true,
            "--model-lut" => model_list = true,
            "--creature-lut" => creature_list = true,
            "--item-lut" => item_list = true,
            "--keep-lut" => keep_list = true,
            "--size-map" => size_map = true,
            "--world-model-lut" => world_model_lut = true,
            "--with-rot" => lut_rot = true,
            "--all-luts" => all_luts = true,
            "--model-summary" => model_summary = true,
            "--terrain-textures" => terrain_textures = true,
            "--terrain-layers" => { i += 1; tl_world = Some(parse_int_arg(raw.get(i).expect("--terrain-layers needs a world id"))); }
            "--terrain-layer-split" => {
                i += 1;
                let val = raw.get(i).expect("--terrain-layer-split needs a kind list (6 or 6,2) or 'all'");
                let kinds = tl_split.get_or_insert_with(Vec::new);
                for part in val.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    if part.eq_ignore_ascii_case("all") { continue; }
                    kinds.push(parse_int_arg(part.trim_start_matches(|c| c == 'k' || c == 'K')));
                }
            }
            "--terrain-layer-id" => {
                i += 1;
                let val = raw.get(i).expect("--terrain-layer-id needs <kind>:<id>[,<kind>:<id>...]");
                for part in val.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    let (k, id) = part.split_once(':').unwrap_or_else(|| panic!("--terrain-layer-id expects <kind>:<id>, got '{}'", part));
                    let k = k.trim().trim_start_matches(|c| c == 'k' || c == 'K');
                    tl_only.push((parse_int_arg(k), parse_int_arg(id)));
                }
            }
            "--worlds" => {
                i += 1;
                let val = raw.get(i).expect("--worlds needs a value");
                for part in val.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    lut_worlds.push(parse_int_arg(part));
                }
            }


            "--noesis" => {
                i += 1;
                noesis = raw.get(i).cloned().expect("--noesis needs a path");
            }
            "--model" => {
                i += 1;
                let val = raw.get(i).expect("--model needs a FileIndex");
                for part in val.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    model_fis.push(parse_int_arg(part));
                }
            }
            "--creature" => {
                i += 1;
                let val = raw.get(i).expect("--creature needs an id");
                for part in val.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    creature_ids.push(parse_int_arg(part));
                }
            }
            "--item-model" => {
                i += 1;
                let val = raw.get(i).expect("--item-model needs a template id");
                for part in val.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    item_models.push(parse_int_arg(part));
                }
            }
            "--rig-format" => {
                i += 1;
                let val = raw.get(i).expect("--rig-format needs an extension (fbx, glb, dae, smd, ...)");
                rig_format = val.trim().trim_start_matches('.').to_ascii_lowercase();
                if rig_format.is_empty() || !rig_format.chars().all(|c| c.is_ascii_alphanumeric()) {
                    panic!("invalid --rig-format: {}", val);
                }
            }
            "--scene" => {
                i += 1;
                let val = raw.get(i).expect("--scene needs a path");
                for part in val.split(',').map(str::trim).filter(|p| !p.is_empty()) {
                    scene.push(part.to_string());
                }
            }
            "--cell-size" => {
                i += 1;
                cell_size = raw.get(i).expect("--cell-size needs a value").trim().parse::<f64>()
                    .unwrap_or_else(|_| panic!("invalid --cell-size"));
            }
            "--scene-name" => {
                i += 1;
                scene_name = Some(raw.get(i).cloned().expect("--scene-name needs a value"));
            }
            "--scene-script-only" => scene_script_only = true,
            "--scene-grid" => scene_grid = true,
            "--alliance" => {
                i += 1;
                alliance = Some(raw.get(i).cloned().expect("--alliance needs a value"));
            }
            "--size-type" => {
                i += 1;
                size_type = Some(parse_int_arg(raw.get(i).expect("--size-type needs a value")));
            }
            "--size-scale" => {
                i += 1;
                size_scale = parse_int_arg(raw.get(i).expect("--size-scale needs a value"));
            }
            "--keeps" => keeps = true,
            "--keep-key" => {
                i += 1;
                keep_key = Some(parse_int_arg(raw.get(i).expect("--keep-key needs a value")));
            }


            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

    let world = if !model_fis.is_empty() || !creature_ids.is_empty() || !item_models.is_empty() || !scene.is_empty() || list_worlds || world_lut || model_list || creature_list || item_list || keep_list || keeps || world_model_lut || all_luts || model_summary || terrain_textures || tl_world.is_some() {
        0
    } else {
        let world_str: &String = match positional.len() {
            1 => &positional[0],
            2 => {
                manifest = positional[0].clone();
                &positional[1]
            }
            _ => {
                print_usage();
                std::process::exit(1);
            }
        };
        parse_int_arg(world_str)
    };

    Args {
        manifest,
        world,
        exe,
        mnf,
        out,
        dry_run,
        skip_export,
        no_cleanup,
        channels,
        placement_only,
        lod_only,
        lod_no_skirts,
        list_worlds,
        world_lut,
        model_list,
        creature_list,
        item_list,
        size_map,
        size_type,
        size_scale,
        model_fis,
        creature_ids,
        item_models,
        rig_format,
        noesis,
        scene,
        cell_size,
        scene_name,
        scene_script_only,
        scene_grid,
        keeps,
        keep_key,
        keep_list,
        alliance,
        no_auto_variants,
        world_model_lut,
        lut_worlds,
        lut_rot,
        all_luts,
        model_summary,
        terrain_textures,
        terrain_layers: tl_world,
        terrain_layer_only: tl_only,
        terrain_layer_split: tl_split,
    }
}

fn find_uncompressed(dir: &str, index: u32) -> Option<std::path::PathBuf> {
    let want = format!("{}_Uncompressed", index);
    WalkDir::new(dir).into_iter().filter_map(Result::ok).map(|e| e.into_path())
        .find(|p| p.is_file() && p.file_stem().and_then(|s| s.to_str()) == Some(want.as_str()))
}

fn find_lang(dir: &str) -> Option<std::path::PathBuf> {
    WalkDir::new(dir).into_iter().filter_map(Result::ok).map(|e| e.into_path())
        .find(|p| p.is_file() && p.file_name().and_then(|n| n.to_str()) == Some("en.lang.csv"))
}

fn build_world_lut(m: &manifest::TableMatch, dir: &str) -> Result<(), String> {
    let entry = find_uncompressed(dir, m.index)
        .ok_or_else(|| format!("Uncompressed World LUT file '{}_Uncompressed.*' not found under '{}'.", m.index, dir))?;
    println!("Parsing file: {}", entry.display());
    std::fs::create_dir_all(LUT_DIR).map_err(|e| format!("failed to create luts dir: {}", e))?;
    let out_path = Path::new(LUT_DIR).join("worlds_lut.csv");

    let zone_names = match find_lang(dir) {
        Some(p) => match worldlut::load_zone_names(&p) {
            Ok(n) => { println!("Loaded {} zone names from {}", n.len(), p.display()); n }
            Err(e) => { eprintln!("Failed to read '{}': {}", p.display(), e); Default::default() }
        },
        None => {
            eprintln!("Warning: en.lang.csv not found under '{}'; zone_name will be empty.", dir);
            Default::default()
        }
    };
    let n = worldlut::parse_and_write(&entry, &zone_names, &out_path)
        .map_err(|e| format!("Error parsing '{}': {}", entry.display(), e))?;
    println!("Wrote {} worlds -> {}", n, out_path.display());
    Ok(())
}

fn build_model_lut(m: &manifest::TableMatch, dir: &str) -> Result<(), String> {
    let entry = find_uncompressed(dir, m.index)
        .ok_or_else(|| format!("Model list file for index {}_Uncompressed not found under '{}'.", m.index, dir))?;
    let names = find_lang(dir)
        .and_then(|p| worldlut::load_lang_group(&p, model::MODEL_NAME_GROUP).ok())
        .unwrap_or_default();
    std::fs::create_dir_all(LUT_DIR).map_err(|e| format!("failed to create luts dir: {}", e))?;
    let out_path = Path::new(LUT_DIR).join("models_lut.csv");
    let n = model::parse_and_write(&entry, &names, &out_path)
        .map_err(|e| format!("Error parsing '{}': {}", entry.display(), e))?;
    println!("Wrote {} models -> {}", n, out_path.display());
    Ok(())
}

fn load_table(m: &manifest::TableMatch, label: &str, dir: &str) -> Result<creature::Table, String> {
    let p = find_uncompressed(dir, m.index)
        .ok_or_else(|| format!("{} file '{}_Uncompressed' not found under '{}'.", label, m.index, dir))?;
    creature::Table::load(&p).map_err(|e| e.to_string())
}

/// `found` = [textures, links, models, creatures]
fn build_creature_lut(found: &[manifest::TableMatch], dir: &str) -> Result<(), String> {
    let textures = load_table(&found[0], "Texture", dir)?;
    let links = load_table(&found[1], "Link", dir)?;
    let models = load_table(&found[2], "Model", dir)?;
    let creature_tbl = load_table(&found[3], "Creature", dir)?;
    let names = find_lang(dir)
        .and_then(|p| worldlut::load_lang_group(&p, creature::CREATURE_NAME_GROUP).ok())
        .unwrap_or_default();
    println!("Loaded {} creature names.", names.len());
    std::fs::create_dir_all(LUT_DIR).map_err(|e| format!("failed to create luts dir: {}", e))?;
    let out_path = Path::new(LUT_DIR).join("creatures_lut.csv");
    let n = creature::build_csv(&textures, &links, &models, &creature_tbl, &names, &out_path)
        .map_err(|e| format!("Error writing '{}': {}", out_path.display(), e))?;
    println!("Wrote {} creatures -> {}", n, out_path.display());
    Ok(())
}

/// `found` = [materials, part materials, item visual templates, item names]
fn build_item_lut(found: &[manifest::TableMatch], args: &Args) -> Result<(), String> {
    let dir = args.out.as_str();
    let materials = load_table(&found[0], "Material", dir)?;
    let part_mats = load_table(&found[1], "Part material", dir)?;
    let templates = load_table(&found[2], "Item visual template", dir)?;
    let name_tbl = load_table(&found[3], "Item name", dir)?;
    let names = find_lang(dir)
        .and_then(|p| worldlut::load_lang_group(&p, items::ITEM_NAME_GROUP).ok())
        .unwrap_or_default();
    println!("Loaded {} item names.", names.len());
    std::fs::create_dir_all(LUT_DIR).map_err(|e| format!("failed to create luts dir: {}", e))?;
    let out_path = Path::new(LUT_DIR).join("items_lut.csv");
    // Parts with no lang name get the mesh name of their model instead; the models are exported into a subfolder of
    // the working dir (it is removed together with it).
    let model_dir = Path::new(dir).join("item_models").to_string_lossy().into_owned();
    let mesh_names = |ids: &[u32]| -> HashMap<u32, String> {
        let opts = model_export::Options {
            manifest: &args.manifest,
            exe: &args.exe,
            mnf: &args.mnf,
            temp_dir: &model_dir,
            out_root: Path::new(LUT_DIR),
            dry_run: args.dry_run,
            skip_export: args.skip_export,
            keep_temp: args.no_cleanup,
            noesis: &args.noesis,
        };
        model_export::mesh_names(ids, &opts)
    };
    let n = items::build_csv(&templates, &part_mats, &materials, &name_tbl, &names, &mesh_names, &out_path)
        .map_err(|e| format!("Error writing '{}': {}", out_path.display(), e))?;
    println!("Wrote {} item parts -> {}", n, out_path.display());
    Ok(())
}

fn build_keep_lut(m: &manifest::TableMatch, dir: &str) -> Result<(), String> {
    let entry = find_uncompressed(dir, m.index)
        .ok_or_else(|| format!("Keeps file '{}_Uncompressed' not found under '{}'.", m.index, dir))?;
    std::fs::create_dir_all(LUT_DIR).map_err(|e| format!("failed to create luts dir: {}", e))?;
    let out_path = Path::new(LUT_DIR).join("keeps_lut.csv");
    let n = keep_export::write_lut(&entry, &out_path)
        .map_err(|e| format!("Error parsing '{}': {}", entry.display(), e))?;
    println!("Wrote {} keeps -> {}", n, out_path.display());
    Ok(())
}

fn table_match(args: &Args, fi: u32, label: &str) -> Result<manifest::TableMatch, String> {
    match manifest::filter_table_manifest(&args.manifest, fi) {
        Ok(Some(m)) => { println!("{}: index {} (arch {})", label, m.index, m.arch); Ok(m) }
        Ok(None) => Err(format!("{} (file_index 0x{:x}) not found in manifest.", label, fi)),
        Err(e) => Err(format!("Failed to read manifest '{}': {}", args.manifest, e)),
    }
}

/// World, model, creature, item and keep LUTs share their archive(s): export it once, then build all five.
fn run_core_luts(args: &Args) -> Result<(), String> {
    let world = table_match(args, manifest::WORLD_LUT_FILE_INDEX, "world table")?;
    let model_m = table_match(args, manifest::MODEL_LIST_FILE_INDEX, "model list")?;
    let mut creature_set = Vec::new();
    for (fi, label) in [(0x05u32, "textures"), (0x06, "links"), (0x07, "models"), (0x08, "creatures")] {
        creature_set.push(table_match(args, fi, label)?);
    }
    let keep_m = table_match(args, keep_export::KEEPS_FILE_INDEX, "keeps")?;
    let mut item_set = Vec::new();
    for (fi, label) in [
        (items::MATERIAL_DEF, "materials"),
        (items::ITEM_PART_MATERIAL_DEF, "part materials"),
        (items::ITEM_VISUAL_TEMPLATE_DEF, "item visual templates"),
        (items::ITEM_NAME_TABLE_DEF, "item names"),
    ] {
        item_set.push(table_match(args, fi, label)?);
    }

    let all: Vec<&manifest::TableMatch> = std::iter::once(&world)
        .chain(std::iter::once(&model_m))
        .chain(std::iter::once(&keep_m))
        .chain(creature_set.iter())
        .chain(item_set.iter())
        .collect();
    let mut archs: Vec<u32> = all.iter().map(|m| m.arch).collect();
    archs.sort_unstable();
    archs.dedup();
    let wanted: HashSet<u32> = all.iter().map(|m| m.index).collect();

    if !args.skip_export {
        for a in &archs {
            println!("\nExporting archive {} (shared by all tables)...", a);
            if !export::export_archive_combined(&args.exe, &args.mnf, &args.out, *a, args.dry_run) {
                eprintln!("Archive export reported an error; check output above before continuing.");
            }
        }
        if !args.no_cleanup {
            let (kept, removed) = export::cleanup_export_dir_keeping(&args.out, &wanted, &["en.lang.csv"], args.dry_run);
            println!("Kept {} files, removed {} files.", kept, removed);
        }
    }
    if args.dry_run { println!("\nDry run complete."); return Ok(()); }

    let mut errors: Vec<String> = Vec::new();
    println!();
    if let Err(e) = build_world_lut(&world, &args.out) { errors.push(e); }
    if let Err(e) = build_model_lut(&model_m, &args.out) { errors.push(e); }
    if let Err(e) = build_creature_lut(&creature_set, &args.out) { errors.push(e); }
    if let Err(e) = build_item_lut(&item_set, args) { errors.push(e); }
    if let Err(e) = build_keep_lut(&keep_m, &args.out) { errors.push(e); }

    if !args.no_cleanup { let _ = std::fs::remove_dir_all(&args.out); }
    if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
}

fn run_world_model_lut(args: &Args) -> Result<(), String> {
    let out_dir = Path::new(LUT_DIR).join("worlds");
    let a = world_models::RunArgs {
        manifest: &args.manifest,
        exe: &args.exe,
        mnf: &args.mnf,
        temp_dir: &args.out,
        out_dir: &out_dir,
        only_worlds: &args.lut_worlds,
        with_rot: args.lut_rot,
        dry_run: args.dry_run,
        skip_export: args.skip_export,
        keep_temp: args.no_cleanup,
    };
    world_models::run(&a)?;
    if args.dry_run { return Ok(()); }
    run_model_summary(args)
}

/// Summarise every luts/worlds/*.csv (model usage + the manifest line for each model).
fn run_model_summary(args: &Args) -> Result<(), String> {
    model_summary::run(&Path::new(LUT_DIR).join("worlds"), &args.manifest, Path::new(LUT_DIR))
}

/// Archives that need fewer textures than this are exported file-by-file instead of dumping the whole archive.
const INDIVIDUAL_EXPORT_LIMIT: usize = 10;

/// Export one file with `-a <arch> -f <filter>` (same call model_export uses).
fn export_single(args: &Args, arch: u32, filter: u32) -> bool {
    println!("Running: {} {} {} -a {} -f {}", args.exe, args.mnf, args.out, arch, filter);
    if let Err(e) = std::fs::create_dir_all(&args.out) {
        eprintln!("  Failed to create '{}': {}", args.out, e);
        return false;
    }
    match std::process::Command::new(&args.exe)
        .arg(&args.mnf)
        .arg(&args.out)
        .arg("-a")
        .arg(arch.to_string())
        .arg("-f")
        .arg(filter.to_string())
        .output()
    {
        Ok(out) => {
            if !out.status.success() {
                eprintln!("  Warning: exporter exited with status {:?}", out.status.code());
            }
            true
        }
        Err(e) => {
            eprintln!("  Failed to launch exporter '{}': {}", args.exe, e);
            false
        }
    }
}

/// Export a single texture (by manifest Index, falling back to FileIndex like model_export) and copy it to `dest`.
fn export_texture_individually(args: &Args, row: &model_export::MnfRow, dest: &Path) -> bool {
    let one: std::collections::BTreeSet<u32> = std::iter::once(row.index).collect();
    let mut tries = vec![row.index, row.file_index];
    if row.index == row.file_index {
        tries.truncate(1);
    }
    for filter in tries {
        if !export_single(args, row.arch, filter) {
            return false;
        }
        if let Ok(n) = terrain_textures::copy_found(Path::new(&args.out), &one, dest) {
            if n > 0 {
                return true;
            }
        }
    }
    false
}

/// Parse the terrain texture table (file id 0x29), export every texture it references and write the LUT.
/// Stitch the terrain layer masks (table 0x44) of one world into export/world_0x<id>/terrain_layers/.
/// Zone names for the kind-2 layer come from en.lang.csv, which is exported together with the table.
fn run_terrain_layers(args: &Args, world: u32) -> Result<(), String> {
    let table = table_match(args, terrain_layers::TERRAIN_LAYER_FILE_INDEX, "terrain layer table")?;

    // 1. export the table (tables are only available through the combined archive export)
    if !args.skip_export {
        println!("\nExporting archive {} (terrain layer table)...", table.arch);
        if !export::export_archive_combined(&args.exe, &args.mnf, &args.out, table.arch, args.dry_run) {
            eprintln!("Archive export reported an error; check output above before continuing.");
        }
        // en.lang.csv comes with the world table's archive (see --world-lut)
        let world_m = table_match(args, manifest::WORLD_LUT_FILE_INDEX, "world table")?;
        if world_m.arch != table.arch {
            println!("\nExporting archive {} (en.lang.csv)...", world_m.arch);
            if !export::export_archive_combined(&args.exe, &args.mnf, &args.out, world_m.arch, args.dry_run) {
                eprintln!("Archive export reported an error; check output above before continuing.");
            }
        }
        if !args.no_cleanup {
            let keep: HashSet<u32> = std::iter::once(table.index).collect();
            let (kept, removed) = export::cleanup_export_dir_keeping(&args.out, &keep, &["en.lang.csv"], args.dry_run);
            println!("Kept {} files, removed {} files.", kept, removed);
        }
    }
    if args.dry_run { println!("\nDry run complete."); return Ok(()); }

    // 2. stitch
    let entry = find_uncompressed(&args.out, table.index)
        .ok_or_else(|| format!("Terrain layer table '{}_Uncompressed' not found under '{}'.", table.index, args.out))?;
    let lang = find_lang(&args.out);
    let out_dir = Path::new(FINAL_EXPORT_DIR).join(format!("world_0x{:x}", world)).join("terrain_layers");
    terrain_layers::run(&entry, lang.as_deref(), world, &out_dir, &args.terrain_layer_only, args.terrain_layer_split.as_deref())?;

    if !args.no_cleanup { let _ = std::fs::remove_dir_all(&args.out); }
    Ok(())
}

fn run_terrain_textures(args: &Args) -> Result<(), String> {
    use std::collections::{BTreeMap, BTreeSet};

    let table = table_match(args, terrain_textures::TERRAIN_TEXTURE_FILE_INDEX, "terrain textures table")?;
    let dest = Path::new(FINAL_EXPORT_DIR).join("terrain_textures");
    let lut_path = Path::new(LUT_DIR).join("terrain_textures_lut.csv");

    // 1. export the table itself (tables are only available through the combined archive export)
    if !args.skip_export {
        println!("\nExporting archive {} (terrain texture table)...", table.arch);
        if !export::export_archive_combined(&args.exe, &args.mnf, &args.out, table.arch, args.dry_run) {
            eprintln!("Archive export reported an error; check output above before continuing.");
        }
        if !args.no_cleanup {
            let keep: HashSet<u32> = std::iter::once(table.index).collect();
            let (kept, removed) = export::cleanup_export_dir_keeping(&args.out, &keep, &[], args.dry_run);
            println!("Kept {} files, removed {} files.", kept, removed);
        }
    }
    if args.dry_run {
        println!("\nDry run complete (the table has to be exported before its textures can be listed).");
        return Ok(());
    }

    // 2. parse it
    let entry = find_uncompressed(&args.out, table.index)
        .ok_or_else(|| format!("Terrain texture table '{}_Uncompressed' not found under '{}'.", table.index, args.out))?;
    println!("Parsing file: {}", entry.display());
    let entries = terrain_textures::parse(&entry)?;
    let wanted: HashSet<u32> = terrain_textures::texture_indices(&entries).into_iter().collect();
    println!("Parsed {} terrain textures referencing {} distinct texture FileIndexes.", entries.len(), wanted.len());

    // 3. resolve FileIndex -> manifest row (Index + archive)
    let rows = model_export::lookup_file_indexes(&args.manifest, &wanted)
        .map_err(|e| format!("Failed to read manifest '{}': {}", args.manifest, e))?;
    let mut resolved: BTreeMap<u32, model_export::MnfRow> = BTreeMap::new();
    for fi in &wanted {
        if let Some(r) = rows.get(fi).and_then(|v| v.first()) {
            resolved.insert(*fi, r.clone());
        }
    }
    if resolved.len() < wanted.len() {
        eprintln!("Warning: {} texture FileIndex(es) not found in manifest '{}'.", wanted.len() - resolved.len(), args.manifest);
    }
    let fi_to_index: BTreeMap<u32, u32> = resolved.iter().map(|(fi, r)| (*fi, r.index)).collect();

    std::fs::create_dir_all(&dest).map_err(|e| format!("failed to create '{}': {}", dest.display(), e))?;

    // 4. work out what still has to be exported (resumable) and group it by archive
    let have = terrain_textures::exported_names(&dest, &fi_to_index);
    let mut by_arch: BTreeMap<u32, Vec<model_export::MnfRow>> = BTreeMap::new();
    for (fi, r) in &resolved {
        if !have.contains_key(fi) {
            by_arch.entry(r.arch).or_default().push(r.clone());
        }
    }
    let todo_total: usize = by_arch.values().map(|v| v.len()).sum();
    println!("{} texture(s) already exported, {} to go.", have.len(), todo_total);

    if args.skip_export {
        if todo_total > 0 {
            let idxs: BTreeSet<u32> = by_arch.values().flatten().map(|r| r.index).collect();
            match terrain_textures::copy_found(Path::new(&args.out), &idxs, &dest) {
                Ok(n) => println!("Copied {} texture(s) already on disk under '{}' -> {}", n, args.out, dest.display()),
                Err(e) => eprintln!("{}", e),
            }
        }
    } else if todo_total > 0 {
        // always show how many files each archive needs before exporting anything
        println!("\nExport plan ({} archive(s)):", by_arch.len());
        for (arch, items) in &by_arch {
            let mode = if *arch >= manifest::UNAVAILABLE_ARCHIVE_THRESHOLD as u32 {
                "SKIPPED (not available in the client files)"
            } else if items.len() < INDIVIDUAL_EXPORT_LIMIT {
                "file-by-file"
            } else {
                "whole archive"
            };
            println!("  archive {:>4}: {:>5} texture(s) -> {}", arch, items.len(), mode);
        }

        for (arch, items) in &by_arch {
            if *arch >= manifest::UNAVAILABLE_ARCHIVE_THRESHOLD as u32 {
                continue;
            }
            if items.len() < INDIVIDUAL_EXPORT_LIMIT {
                let mut ok = 0usize;
                for r in items {
                    if export_texture_individually(args, r, &dest) {
                        ok += 1;
                    } else {
                        eprintln!("  Texture FileIndex 0x{:x} (Index {}, archive {}) could not be exported.", r.file_index, r.index, r.arch);
                    }
                }
                println!("Archive {}: exported {} of {} texture(s) individually.", arch, ok, items.len());
            } else {
                println!("\nExporting archive {} ({} texture(s) needed)...", arch, items.len());
                if !export::export_archive(&args.exe, &args.mnf, &args.out, *arch, false) {
                    eprintln!("Archive export reported an error; check output above before continuing.");
                }
                let idxs: BTreeSet<u32> = items.iter().map(|r| r.index).collect();
                match terrain_textures::copy_found(Path::new(&args.out), &idxs, &dest) {
                    Ok(n) => println!("Copied {} of {} texture(s) -> {}", n, idxs.len(), dest.display()),
                    Err(e) => eprintln!("{}", e),
                }
            }
            if !args.no_cleanup { let _ = std::fs::remove_dir_all(&args.out); }
        }
    }

    // 5. LUT
    let files = terrain_textures::exported_names(&dest, &fi_to_index);
    let n = terrain_textures::write_lut(&entries, &files, &lut_path)?;
    println!("\nWrote {} terrain textures -> {}", n, lut_path.display());
    println!("{} of {} texture file(s) present in {}", files.len(), wanted.len(), dest.display());
    if !args.no_cleanup { let _ = std::fs::remove_dir_all(&args.out); }
    Ok(())
}

const ANSI_RED: &str = "\x1b[31m";
const ANSI_RESET: &str = "\x1b[0m";

fn main() {
    let args = parse_args();
    model_export::set_auto_variants(!args.no_auto_variants);

    if args.list_worlds {
        println!("Scanning manifest '{}' for worlds with terrain cell data...", args.manifest);
        let summaries = match manifest::list_worlds_with_cells(&args.manifest) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("Failed to read manifest '{}': {}", args.manifest, e);
                std::process::exit(1);
            }
        };

        if summaries.is_empty() {
            println!("No worlds with terrain cell data found.");
            return;
        }

        let lut = match worldlut::load_lut_csv(&Path::new(LUT_DIR).join("worlds_lut.csv")) {
            Ok(m) => m,
            Err(_) => {
                HashMap::new()
            }
        };

        println!(
            "{:<12} {:>6} {:>6} {:>10}   {:<14} range",
            "World", "W", "H", "Cells", "Archive(s)"
        );
        for s in &summaries {
            let archives_str = s
                .archives
                .iter()
                .map(|a| a.to_string())
                .collect::<Vec<_>>()
                .join(",");

            let range = format!("x {}..{}, z {}..{}", s.min_x, s.max_x, s.min_z, s.max_z);
            let (internal, name) = match lut.get(&s.world) {
                Some(i) => (i.world_name.as_str(), i.zone_name.as_str()),
                None => ("", ""),
            };

            let line = format!(
                "0x{:<10x} {:>6} {:>6} {:>10}   {:<10} {:<34} {:<28} {}",
                s.world,
                s.grid_w(),
                s.grid_h(),
                s.cell_count,
                archives_str,
                range,
                internal,
                name
            );

            if s.has_unavailable_archive() {
                println!("{ANSI_RED}{line}  [NOT AVAILABLE]{ANSI_RESET}");
            } else {
                println!("{line}");
            }
        }

        let unavailable_count = summaries.iter().filter(|s| s.has_unavailable_archive()).count();
        println!("\n{} world(s) found.", summaries.len());
        if unavailable_count > 0 {
            println!(
                "{ANSI_RED}{} world(s) reference archive {}+ and are not available in the client files.{ANSI_RESET}",
                unavailable_count, manifest::UNAVAILABLE_ARCHIVE_THRESHOLD
            );
        }
        return;
    }

    if args.all_luts {
        if args.skip_export {
            eprintln!("--all-luts can't be combined with --skip-export (the LUTs export their own tables).");
            std::process::exit(1);
        }
        let mut failed: Vec<&str> = Vec::new();
        println!("===== [1/2] world, model, creature, item and keep LUTs (one export) =====");
        if let Err(e) = run_core_luts(&args) { eprintln!("{}", e); failed.push("world/model/creature/item/keep LUTs"); }
        println!("\n===== [2/2] per-world model LUTs =====");
        if let Err(e) = run_world_model_lut(&args) { eprintln!("{}", e); failed.push("per-world model LUTs"); }
        if failed.is_empty() {
            println!("\nAll LUTs written to {}.", LUT_DIR);
            return;
        }
        eprintln!("\nFailed: {}", failed.join(", "));
        std::process::exit(1);
    }

    if args.model_summary {
        if let Err(e) = run_model_summary(&args) { eprintln!("{}", e); std::process::exit(1); }
        return;
    }

    if args.world_model_lut {
        if let Err(e) = run_world_model_lut(&args) { eprintln!("{}", e); std::process::exit(1); }
        return;
    }

    if args.terrain_textures {
        if let Err(e) = run_terrain_textures(&args) { eprintln!("{}", e); std::process::exit(1); }
        return;
    }

    if let Some(world) = args.terrain_layers {
        if let Err(e) = run_terrain_layers(&args, world) { eprintln!("{}", e); std::process::exit(1); }
        return;
    }

    if !args.scene.is_empty() {
        let models_dir = Path::new(FINAL_EXPORT_DIR).join("models");
        let scenes_dir = Path::new(FINAL_EXPORT_DIR).join("scenes");
        let opts = model_export::Options {
            manifest: &args.manifest,
            exe: &args.exe,
            mnf: &args.mnf,
            temp_dir: &args.out,
            out_root: &models_dir,
            dry_run: args.dry_run,
            skip_export: args.skip_export,
            keep_temp: args.no_cleanup,
            noesis: &args.noesis,
        };
        let sa = scene_export::SceneArgs {
            inputs: &args.scene,
            cell_size: args.cell_size,
            name: args.scene_name.as_deref(),
            script_only: args.scene_script_only,
            scenes_dir: &scenes_dir,
            alliance: args.alliance.as_deref(),
            grid: args.scene_grid,
        };
        match scene_export::run(&sa, &opts) {
            Ok(p) => {
                if !args.dry_run {
                    println!("\nFinished. In Blender: Scripting tab -> Open -> {} -> Run Script.", p.display());
                }
            }
            Err(e) => { eprintln!("{}", e); std::process::exit(1); }
        }
        return;
    }

    if !args.creature_ids.is_empty() {
        let lut = Path::new(LUT_DIR).join("creatures_lut.csv");
        let models_dir = Path::new(FINAL_EXPORT_DIR).join("models");
        let opts = model_export::Options {
            manifest: &args.manifest,
            exe: &args.exe,
            mnf: &args.mnf,
            temp_dir: &args.out,
            out_root: &models_dir,
            dry_run: args.dry_run,
            skip_export: args.skip_export,
            keep_temp: args.no_cleanup,
            noesis: &args.noesis,
        };
        let mut failed = 0usize;
        for id in &args.creature_ids {
            let rows = match creature::find_in_lut(&lut, *id) {
                Ok(r) => r,
                Err(e) => { eprintln!("Cannot read '{}': {} (run --creature-lut first)", lut.display(), e); std::process::exit(1); }
            };
            let Some(row) = rows.first() else {
                eprintln!("Creature {} not found in '{}'.", id, lut.display());
                failed += 1;
                continue;
            };
            if rows.len() > 1 {
                eprintln!("Warning: {} creatures match {}; using table_id {} ({}). Others: {}", rows.len(), id, row.table_id, row.name,
                    rows.iter().skip(1).take(8).map(|r| r.table_id.to_string()).collect::<Vec<_>>().join(", "));
            }
            println!("\n=== Creature {} '{}' (creature_id {:06}): model {}, textures {}/{}/{} ===", row.table_id, row.name, row.name_id, row.model, row.tex[0], row.tex[1], row.tex[2]);
            match model_export::run_with_textures(row.model, Some(row.tex), Some(row.skeleton), &args.rig_format, &opts) {
                Ok(s) => println!("\nFinished creature {}. {} texture(s) exported, {} missing -> {}", row.table_id, s.textures_found, s.textures_missing, s.out_dir.display()),
                Err(e) => { eprintln!("{}", e); failed += 1; }
            }
        }
        if failed > 0 { std::process::exit(1); }
        return;
    }

    if !args.item_models.is_empty() {
        let lut = Path::new(LUT_DIR).join("items_lut.csv");
        let models_dir = Path::new(FINAL_EXPORT_DIR).join("models");
        // keep_temp: the parts of an item share textures, so the temp dir is only wiped once at the end.
        // Every file is fetched individually (`-f`); there is deliberately no whole-archive path here.
        let opts = model_export::Options {
            manifest: &args.manifest,
            exe: &args.exe,
            mnf: &args.mnf,
            temp_dir: &args.out,
            out_root: &models_dir,
            dry_run: args.dry_run,
            skip_export: args.skip_export,
            keep_temp: true,
            noesis: &args.noesis,
        };
        let mut failed = 0usize;
        let mut done: HashSet<u32> = HashSet::new();
        for id in &args.item_models {
            let rows = match items::find_in_lut(&lut, *id) {
                Ok(r) => r,
                Err(e) => { eprintln!("Cannot read '{}': {} (run --item-lut first)", lut.display(), e); std::process::exit(1); }
            };
            if rows.is_empty() {
                eprintln!("Item template {} not found in '{}'.", id, lut.display());
                failed += 1;
                continue;
            }
            let name = rows.iter().map(|r| r.name.as_str()).find(|n| !n.is_empty()).unwrap_or("");
            println!("\n=== Item template {} '{}': {} part(s) ===", id, name, rows.len());
            for r in &rows {
                let tex = [r.diffuse, r.normal, r.specular];
                if r.tint != 0 {
                    println!("  (part {}: tint map {} is not exported)", r.part, r.tint);
                }
                // No texture ids in the LUT (unresolved material) -> fall back to whatever the model embeds.
                let fill = if tex.iter().all(|t| *t == 0) { None } else { Some(tex) };
                let mut models = vec![r.model];
                if r.model2 != 0 { models.push(r.model2); }
                for m in models {
                    if !done.insert(m) {
                        println!("  part {}: model {} already exported, skipping.", r.part, m);
                        continue;
                    }
                    println!("\n--- part {} model {} (textures {}/{}/{}) ---", r.part, m, r.diffuse, r.normal, r.specular);
                    match model_export::run_with_textures(m, fill, None, &args.rig_format, &opts) {
                        Ok(s) => println!("Finished model {}. {} texture(s) exported, {} missing -> {}", m, s.textures_found, s.textures_missing, s.out_dir.display()),
                        Err(e) => { eprintln!("{}", e); failed += 1; }
                    }
                }
            }
        }
        if !args.no_cleanup && !args.skip_export && !args.dry_run {
            if let Err(e) = std::fs::remove_dir_all(&args.out) {
                eprintln!("Warning: failed to remove temporary directory '{}': {}", args.out, e);
            }
        }
        if failed > 0 { std::process::exit(1); }
        return;
    }

    if !args.model_fis.is_empty() {
        let models_dir = Path::new(FINAL_EXPORT_DIR).join("models");
        let opts = model_export::Options {
            manifest: &args.manifest,
            exe: &args.exe,
            mnf: &args.mnf,
            temp_dir: &args.out,
            out_root: &models_dir,
            dry_run: args.dry_run,
            skip_export: args.skip_export,
            keep_temp: args.no_cleanup,
            noesis: &args.noesis,
        };
        let total = args.model_fis.len();
        let mut failed = 0usize;
        for (fi, r) in model_export::run_batch(&args.model_fis, &opts) {
            match r {
                Ok(s) => println!("\nFinished 0x{:x}. {} texture(s) exported, {} missing -> {}",
                                fi, s.textures_found, s.textures_missing, s.out_dir.display()),
                Err(e) => { eprintln!("{}", e); failed += 1; }
            }
        }
        if total > 1 {
            println!("\nAll done: {} ok, {} failed.", total - failed, failed);
        }
        if failed > 0 { std::process::exit(1); }
        return;
    }

    if args.size_map {
        println!("Building size map for world 0x{:x}...", args.world);
        match sizemap::run(
            &args.manifest,
            args.world,
            args.size_type,
            args.size_scale,
            Path::new(FINAL_EXPORT_DIR),
        ) {
            Ok(p) => println!("Saved size map -> {}", p.display()),
            Err(e) => {
                eprintln!("{}", e);
                std::process::exit(1);
            }
        }
        return;
    }

    if args.placement_only {
        println!("Placement-only mode: looking up instance list for world 0x{:x}...", args.world);
        let pm = match manifest::filter_placement_manifest(&args.manifest, args.world) {
            Ok(p) => p,
            Err(e) => { eprintln!("Failed to read manifest '{}': {}", args.manifest, e); std::process::exit(1); }
        };
        let cell_pms = match manifest::filter_cell_placement_manifest(&args.manifest, args.world) {
            Ok(v) => v,
            Err(e) => { eprintln!("Failed to read manifest '{}': {}", args.manifest, e); std::process::exit(1); }
        };
        if pm.is_none() && cell_pms.is_empty() {
            eprintln!("No placement/instance lists found for this world.");
            std::process::exit(1);
        }
        if let Some(pm) = &pm {
            println!("Placement/instance list: index {} (arch {})", pm.index, pm.arch);
        }
        println!("Cell placement files: {}", cell_pms.len());

        let mut archs: Vec<u32> = pm.iter().map(|p| p.arch).chain(cell_pms.iter().map(|c| c.arch)).collect();
        archs.sort_unstable();
        archs.dedup();
        let mut wanted: HashSet<u32> = pm.iter().map(|p| p.index).chain(cell_pms.iter().map(|c| c.index)).collect();
        if let Some(p) = pm {
            wanted.insert(p.index);
        }

        if !args.skip_export {
            for arch in &archs {
                println!("\nExporting archive {}...", arch);
                if !export::export_archive(&args.exe, &args.mnf, &args.out, *arch, args.dry_run) {
                    eprintln!("Archive export reported an error; check output above before continuing.");
                }
            }
            if !args.no_cleanup {
                println!("\nCleaning up export directory (keeping {} relevant files)...", wanted.len());
                let (kept, removed) = export::cleanup_export_dir(&args.out, &wanted, args.dry_run);
                println!("Kept {} files, removed {} files.", kept, removed);
            } else {
                println!("\nSkipping cleanup (--no-cleanup).");
            }
        } else {
            println!("\nSkipping export step (--skip-export).");
        }

        if args.dry_run {
            println!("\nDry run complete; no file was parsed.");
            return;
        }

        let output_dir = Path::new(FINAL_EXPORT_DIR).join(format!("world_0x{:x}", args.world)).join("placements");
        if let Err(e) = std::fs::create_dir_all(&output_dir) {
            eprintln!("Failed to create output directory '{}': {}", output_dir.display(), e);
            std::process::exit(1);
        }

        if write_placements(&args.out, pm.as_ref(), &cell_pms, &output_dir) > 0 {
            std::process::exit(1);
        }

        if !args.no_cleanup {
            match std::fs::remove_dir_all(&args.out) {
                Ok(()) => println!("Removed temporary export directory '{}'.", args.out),
                Err(e) => eprintln!("Warning: failed to remove temporary directory '{}': {}", args.out, e),
            }
        }

        println!("\nFinished (placement-only).");
        return;
    }

    if args.lod_only {
        println!("LOD-only mode: looking up terrain LOD mesh files for world 0x{:x}...", args.world);
        let lods = match manifest::filter_lod_manifest(&args.manifest, args.world) {
            Ok(v) => v,
            Err(e) => { eprintln!("Failed to read manifest '{}': {}", args.manifest, e); std::process::exit(1); }
        };
        if lods.is_empty() {
            eprintln!("No terrain LOD mesh files found for this world.");
            std::process::exit(1);
        }
        println!("Terrain LOD mesh files: {}", lods.len());

        let mut archs: Vec<u32> = lods.iter().map(|l| l.arch).collect();
        archs.sort_unstable();
        archs.dedup();
        let wanted: HashSet<u32> = lods.iter().map(|l| l.index).collect();

        if !args.skip_export {
            for arch in &archs {
                println!("\nExporting archive {}...", arch);
                if !export::export_archive(&args.exe, &args.mnf, &args.out, *arch, args.dry_run) {
                    eprintln!("Archive export reported an error; check output above before continuing.");
                }
            }
            if !args.no_cleanup {
                println!("\nCleaning up export directory (keeping {} relevant files)...", wanted.len());
                let (kept, removed) = export::cleanup_export_dir(&args.out, &wanted, args.dry_run);
                println!("Kept {} files, removed {} files.", kept, removed);
            } else {
                println!("\nSkipping cleanup (--no-cleanup).");
            }
        } else {
            println!("\nSkipping export step (--skip-export).");
        }

        if args.dry_run {
            println!("\nDry run complete; no file was parsed.");
            return;
        }

        let output_dir = Path::new(FINAL_EXPORT_DIR).join(format!("world_0x{:x}", args.world)).join("terrain_lod");
        let failures = write_lod_meshes(&args.out, &lods, args.cell_size, !args.lod_no_skirts, &output_dir);

        if !args.no_cleanup {
            match std::fs::remove_dir_all(&args.out) {
                Ok(()) => println!("Removed temporary export directory '{}'.", args.out),
                Err(e) => eprintln!("Warning: failed to remove temporary directory '{}': {}", args.out, e),
            }
        }
        println!("\nFinished (lod-only).");
        if failures > 0 { std::process::exit(1); }
        return;
    }

    if args.world_lut {
        let m = match manifest::filter_table_manifest(&args.manifest, manifest::WORLD_LUT_FILE_INDEX) {
            Ok(Some(m)) => m,
            Ok(None) => { eprintln!("World LUT entry not found in manifest."); std::process::exit(1); }
            Err(e) => { eprintln!("Failed to read manifest '{}': {}", args.manifest, e); std::process::exit(1); }
        };
        println!("World LUT: index {} (arch {})", m.index, m.arch);

        if !args.skip_export {
            println!("\nExporting archive {}...", m.arch);
            if !export::export_archive_combined(&args.exe, &args.mnf, &args.out, m.arch, args.dry_run) {
                eprintln!("Archive export reported an error; check output above before continuing.");
            }

            if !args.no_cleanup {
                let mut wanted = HashSet::new();
                wanted.insert(m.index);
                let (kept, removed) = export::cleanup_export_dir_keeping(
                &args.out, &wanted, &["en.lang.csv"], args.dry_run,
                );
                println!("Kept {} files, removed {} files.", kept, removed);
            }
        }
        if args.dry_run { println!("\nDry run complete."); return; }

        if let Err(e) = build_world_lut(&m, &args.out) { eprintln!("{}", e); std::process::exit(1); }

        if !args.no_cleanup {
            let _ = std::fs::remove_dir_all(&args.out);
        }
        return;
    }

    if args.model_list {
        let m = match manifest::filter_table_manifest(&args.manifest, manifest::MODEL_LIST_FILE_INDEX) {
            Ok(Some(m)) => m,
            Ok(None) => { eprintln!("Model list (file_index 0x145) not found in manifest."); std::process::exit(1); }
            Err(e) => { eprintln!("Failed to read manifest '{}': {}", args.manifest, e); std::process::exit(1); }
        };
        println!("Model list: index {} (arch {})", m.index, m.arch);

        if !args.skip_export {
            if !export::export_archive_combined(&args.exe, &args.mnf, &args.out, m.arch, args.dry_run) {
                eprintln!("Archive export reported an error; check output above before continuing.");
            }
            if !args.no_cleanup {
                let mut wanted = HashSet::new();
                wanted.insert(m.index);
                let (kept, removed) = export::cleanup_export_dir_keeping(
                    &args.out, &wanted, &["en.lang.csv"], args.dry_run,
                );
                println!("Kept {} files, removed {} files.", kept, removed);
            }
        }
        if args.dry_run { println!("\nDry run complete."); return; }

        if let Err(e) = build_model_lut(&m, &args.out) { eprintln!("{}", e); std::process::exit(1); }
        if !args.no_cleanup { let _ = std::fs::remove_dir_all(&args.out); }
        return;
    }

    if args.keep_list {
        let m = match manifest::filter_table_manifest(&args.manifest, keep_export::KEEPS_FILE_INDEX) {
            Ok(Some(m)) => m,
            Ok(None) => { eprintln!("Keeps (file_index 0x{:x}) not found in manifest.", keep_export::KEEPS_FILE_INDEX); std::process::exit(1); }
            Err(e) => { eprintln!("Failed to read manifest '{}': {}", args.manifest, e); std::process::exit(1); }
        };
        println!("Keeps: index {} (arch {})", m.index, m.arch);

        if !args.skip_export {
            if !export::export_archive_combined(&args.exe, &args.mnf, &args.out, m.arch, args.dry_run) {
                eprintln!("Archive export reported an error; check output above before continuing.");
            }
            if !args.no_cleanup {
                let mut wanted = HashSet::new();
                wanted.insert(m.index);
                let (kept, removed) = export::cleanup_export_dir_keeping(&args.out, &wanted, &[], args.dry_run);
                println!("Kept {} files, removed {} files.", kept, removed);
            }
        }
        if args.dry_run { println!("\nDry run complete."); return; }

        if let Err(e) = build_keep_lut(&m, &args.out) { eprintln!("{}", e); std::process::exit(1); }
        if !args.no_cleanup { let _ = std::fs::remove_dir_all(&args.out); }
        return;
    }

    if args.keeps {
        let mut found = Vec::new();
        for (fi, label) in [
            (keep_export::KEEPS_FILE_INDEX, "keeps"),
            (keep_export::PIECES_FILE_INDEX, "keep pieces"),
        ] {
            match manifest::filter_table_manifest(&args.manifest, fi) {
                Ok(Some(m)) => { println!("{}: index {} (arch {})", label, m.index, m.arch); found.push(m); }
                Ok(None) => { eprintln!("{} (file_index 0x{:x}) not found in manifest.", label, fi); std::process::exit(1); }
                Err(e) => { eprintln!("Failed to read manifest '{}': {}", args.manifest, e); std::process::exit(1); }
            }
        }
        let mut archs: Vec<u32> = found.iter().map(|m| m.arch).collect();
        archs.sort_unstable(); archs.dedup();

        if !args.skip_export {
            for a in &archs {
                println!("\nExporting archive {}...", a);
                if !export::export_archive_combined(&args.exe, &args.mnf, &args.out, *a, args.dry_run) {
                    eprintln!("Archive export reported an error; check output above before continuing.");
                }
            }
            if !args.no_cleanup {
                let wanted: HashSet<u32> = found.iter().map(|m| m.index).collect();
                let (kept, removed) = export::cleanup_export_dir_keeping(&args.out, &wanted, &[], args.dry_run);
                println!("Kept {} files, removed {} files.", kept, removed);
            }
        }
        if args.dry_run { println!("\nDry run complete."); return; }

        let locate = |m: &manifest::TableMatch, label: &str| -> std::path::PathBuf {
            find_uncompressed(&args.out, m.index).unwrap_or_else(|| {
                eprintln!("{} file '{}_Uncompressed' not found under '{}'.", label, m.index, args.out);
                std::process::exit(1);
            })
        };
        let keeps_path = locate(&found[0], "Keeps");
        let pieces_path = locate(&found[1], "Keep pieces");

        let a = keep_export::KeepArgs {
            keeps_file: &keeps_path,
            pieces_file: &pieces_path,
            cell_m: 3.0,
            units_per_metre: 100.0,
            only_key: args.keep_key,
            out_file: Path::new("export/keeps_placement.json"),
        };
        if let Err(e) = keep_export::run(&a) { eprintln!("{}", e); std::process::exit(1); }

        if !args.no_cleanup { let _ = std::fs::remove_dir_all(&args.out); }
        return;
    }

    if args.creature_list {
        let mut found = Vec::new();
        for (fi, label) in [(0x05u32, "textures"), (0x06, "links"), (0x07, "models"), (0x08, "creatures")] {
            match manifest::filter_table_manifest(&args.manifest, fi) {
                Ok(Some(m)) => { println!("{}: index {} (arch {})", label, m.index, m.arch); found.push(m); }
                Ok(None) => { eprintln!("{} (file_index 0x{:x}) not found in manifest.", label, fi); std::process::exit(1); }
                Err(e) => { eprintln!("Failed to read manifest '{}': {}", args.manifest, e); std::process::exit(1); }
            }
        }
        let mut archs: Vec<u32> = found.iter().map(|m| m.arch).collect();
        archs.sort_unstable(); archs.dedup();

        if !args.skip_export {
            for a in &archs {
                println!("\nExporting archive {}...", a);
                if !export::export_archive_combined(&args.exe, &args.mnf, &args.out, *a, args.dry_run) {
                    eprintln!("Archive export reported an error; check output above before continuing.");
                }
            }
            if !args.no_cleanup {
                let wanted: HashSet<u32> = found.iter().map(|m| m.index).collect();
                let (kept, removed) = export::cleanup_export_dir_keeping(&args.out, &wanted, &["en.lang.csv"], args.dry_run);
                println!("Kept {} files, removed {} files.", kept, removed);
            }
        }
        if args.dry_run { println!("\nDry run complete."); return; }

        if let Err(e) = build_creature_lut(&found, &args.out) { eprintln!("{}", e); std::process::exit(1); }
        if !args.no_cleanup { let _ = std::fs::remove_dir_all(&args.out); }
        return;
    }
    if args.item_list {
        let mut found = Vec::new();
        for (fi, label) in [
            (items::MATERIAL_DEF, "materials"),
            (items::ITEM_PART_MATERIAL_DEF, "part materials"),
            (items::ITEM_VISUAL_TEMPLATE_DEF, "item visual templates"),
            (items::ITEM_NAME_TABLE_DEF, "item names"),
        ] {
            match manifest::filter_table_manifest(&args.manifest, fi) {
                Ok(Some(m)) => { println!("{}: index {} (arch {})", label, m.index, m.arch); found.push(m); }
                Ok(None) => { eprintln!("{} (file_index 0x{:x}) not found in manifest.", label, fi); std::process::exit(1); }
                Err(e) => { eprintln!("Failed to read manifest '{}': {}", args.manifest, e); std::process::exit(1); }
            }
        }
        let mut archs: Vec<u32> = found.iter().map(|m| m.arch).collect();
        archs.sort_unstable(); archs.dedup();

        if !args.skip_export {
            for a in &archs {
                println!("\nExporting archive {}...", a);
                if !export::export_archive_combined(&args.exe, &args.mnf, &args.out, *a, args.dry_run) {
                    eprintln!("Archive export reported an error; check output above before continuing.");
                }
            }
            if !args.no_cleanup {
                let wanted: HashSet<u32> = found.iter().map(|m| m.index).collect();
                let (kept, removed) = export::cleanup_export_dir_keeping(&args.out, &wanted, &["en.lang.csv"], args.dry_run);
                println!("Kept {} files, removed {} files.", kept, removed);
            }
        }
        if args.dry_run { println!("\nDry run complete."); return; }

        if let Err(e) = build_item_lut(&found, &args) { eprintln!("{}", e); std::process::exit(1); }
        if !args.no_cleanup { let _ = std::fs::remove_dir_all(&args.out); }
        return;
    }
    println!("Filtering manifest for world 0x{:x}...", args.world);
    let result = match filter_manifest(&args.manifest, args.world) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("Failed to read manifest '{}': {}", args.manifest, e);
            std::process::exit(1);
        }
    };

    println!("TOC matches: {}", result.toc_matches.len());
    println!("Terrain cell matches: {}", result.cell_matches.len());

    if result.cell_matches.is_empty() {
        eprintln!("No terrain cells found for this world. Aborting.");
        std::process::exit(1);
    }

    let min_x = result.cell_matches.iter().map(|c| c.x).min().unwrap();
    let max_x = result.cell_matches.iter().map(|c| c.x).max().unwrap();
    let min_z = result.cell_matches.iter().map(|c| c.z).min().unwrap();
    let max_z = result.cell_matches.iter().map(|c| c.z).max().unwrap();
    let grid_w = max_x - min_x + 1;
    let grid_h = max_z - min_z + 1;
    let expected = (grid_w as u64) * (grid_h as u64);

    println!("  x range: {}..{}  z range: {}..{}", min_x, max_x, min_z, max_z);
    println!(
        "  grid size: {} x {} (expected {} cells, found {})",
        grid_w,
        grid_h,
        expected,
        result.cell_matches.len()
    );
    if expected != result.cell_matches.len() as u64 {
        eprintln!(
            "  Warning: {} cells missing from an otherwise rectangular grid.",
            expected - result.cell_matches.len() as u64
        );
    }
    if grid_w > MAX_SANE_GRID_SPAN || grid_h > MAX_SANE_GRID_SPAN {
        eprintln!(
            "Decoded coordinate span ({}x{}) looks too large to be a real cell grid; \
             aborting rather than guessing.",
            grid_w, grid_h
        );
        std::process::exit(1);
    }

    let mut index_to_xz: HashMap<u32, (u32, u32)> = HashMap::with_capacity(result.cell_matches.len());
    let mut wanted_indices: HashSet<u32> = HashSet::new();
    for c in &result.cell_matches {
        index_to_xz.insert(c.index, (c.x, c.z));
        wanted_indices.insert(c.index);
    }
    for t in &result.toc_matches {
        wanted_indices.insert(t.index);
    }

    let cm_matches = match filter_colourmap_manifest(&args.manifest, args.world) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Failed to read colour map matches: {}", e);
            Vec::new()
        }
    };
    println!("Colour map matches: {}", cm_matches.len());
    for m in &cm_matches {
        wanted_indices.insert(m.index);
    }

    let placement_match = match manifest::filter_placement_manifest(&args.manifest, args.world) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Failed to read placement match: {}", e);
            None
        }
    };
    let cell_pm = match manifest::filter_cell_placement_manifest(&args.manifest, args.world) {
        Ok(v) => v,
        Err(e) => { eprintln!("Failed to read cell placement matches: {}", e); Vec::new() }
    };
    println!("Cell placement files: {}", cell_pm.len());
    for c in &cell_pm {
        wanted_indices.insert(c.index);
    }
    let lod_pm = match manifest::filter_lod_manifest(&args.manifest, args.world) {
        Ok(v) => v,
        Err(e) => { eprintln!("Failed to read terrain LOD matches: {}", e); Vec::new() }
    };
    println!("Terrain LOD mesh files: {}", lod_pm.len());
    for l in &lod_pm {
        wanted_indices.insert(l.index);
    }

    let cell_archs: HashSet<u32> = result.cell_matches.iter().map(|c| c.arch).collect();
    let toc_archs: HashSet<u32> = result.toc_matches.iter().map(|t| t.arch).collect();
    let cm_archs: HashSet<u32> = cm_matches.iter().map(|c| c.arch).collect();
    let placement_archs: HashSet<u32> = placement_match.iter().map(|p| p.arch).collect();
    let mut all_archs: Vec<u32> = cell_archs
        .union(&toc_archs)
        .cloned()
        .collect::<HashSet<u32>>()
        .union(&cm_archs)
        .cloned()
        .collect::<HashSet<u32>>()
        .union(&placement_archs)
        .cloned()
        .collect();
    all_archs.extend(cell_pm.iter().map(|c| c.arch));
    all_archs.extend(lod_pm.iter().map(|l| l.arch));
    all_archs.sort_unstable();
    all_archs.dedup();
    println!("Archive(s) involved: {:?}", all_archs);

    if !args.skip_export {
        for arch in &all_archs {
            println!("\nExporting archive {}...", arch);
            if !export::export_archive(&args.exe, &args.mnf, &args.out, *arch, args.dry_run) {
                eprintln!("Archive export reported an error; check output above before continuing.");
            }
        }

        if !args.no_cleanup {
            println!("\nCleaning up export directory...");
            let (kept, removed) = export::cleanup_export_dir(&args.out, &wanted_indices, args.dry_run);
            println!("Kept {} files, removed {} files.", kept, removed);
        } else {
            println!("\nSkipping cleanup (--no-cleanup).");
        }
    } else {
        println!("\nSkipping export step (--skip-export).");
    }

    if args.dry_run {
        println!("\nDry run complete; no image was built.");
        return;
    }

    println!("\nReading cell files and accumulating raw channel data...");
    let mut scalar_images: HashMap<usize, RawBigImage> = HashMap::new();
    let mut vec3_images: HashMap<usize, (RawBigImage, RawBigImage, RawBigImage)> = HashMap::new();
    let mut planes4_images: HashMap<usize, (RawBigImage, RawBigImage, RawBigImage, RawBigImage)> = HashMap::new();
    let mut loaded_per_channel: HashMap<usize, usize> = HashMap::new();
    let mut files_loaded = 0usize;
    let mut errors = 0usize;

    for entry in WalkDir::new(&args.out).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if path.extension().and_then(|e| e.to_str()) != Some("dat") {
            continue;
        }

        let idx = match path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok())
        {
            Some(i) => i,
            None => continue,
        };
        let (x, z) = match index_to_xz.get(&idx) {
            Some(xz) => *xz,
            None => continue,
        };

        if !LandFile::is_land_file(path) {
            continue;
        }

        let mut file = LandFile::new();
        match file.load(path) {
            Ok(()) => {
                files_loaded += 1;
                let col = x - min_x;
                let row = max_z - z;

                let channel_range: Vec<usize> = match &args.channels {
                    Some(list) => list.clone(),
                    None => file.available_channels(),
                };

                for channel in channel_range {
                    match file.channel_tile_raw(channel) {
                        Some(RawTile::Scalar { values, width, height }) => {
                            let img = scalar_images
                                .entry(channel)
                                .or_insert_with(|| RawBigImage::new(grid_w, grid_h));
                            if img.set_tile_at(col, row, &values, width, height) {
                                *loaded_per_channel.entry(channel).or_insert(0) += 1;
                            }
                        }
                        Some(RawTile::Vec3 { x: xs, y: ys, z: zs, width, height }) => {
                            let (ix, iy, iz) = vec3_images.entry(channel).or_insert_with(|| {
                                (
                                    RawBigImage::new(grid_w, grid_h),
                                    RawBigImage::new(grid_w, grid_h),
                                    RawBigImage::new(grid_w, grid_h),
                                )
                            });
                            let ok_x = ix.set_tile_at(col, row, &xs, width, height);
                            let ok_y = iy.set_tile_at(col, row, &ys, width, height);
                            let ok_z = iz.set_tile_at(col, row, &zs, width, height);
                            if ok_x && ok_y && ok_z {
                                *loaded_per_channel.entry(channel).or_insert(0) += 1;
                            }
                        }
                        Some(RawTile::Planes4 { p0, p1, p2, p3, width, height }) => {
                            let (i0, i1, i2, i3) = planes4_images.entry(channel).or_insert_with(|| {
                                (
                                    RawBigImage::new(grid_w, grid_h),
                                    RawBigImage::new(grid_w, grid_h),
                                    RawBigImage::new(grid_w, grid_h),
                                    RawBigImage::new(grid_w, grid_h),
                                )
                            });
                            let ok0 = i0.set_tile_at(col, row, &p0, width, height);
                            let ok1 = i1.set_tile_at(col, row, &p1, width, height);
                            let ok2 = i2.set_tile_at(col, row, &p2, width, height);
                            let ok3 = i3.set_tile_at(col, row, &p3, width, height);
                            if ok0 && ok1 && ok2 && ok3 {
                                *loaded_per_channel.entry(channel).or_insert(0) += 1;
                            }
                        }
                        None => {}
                    }
                }
            }
            Err(e) => {
                eprintln!("Error loading '{}': {}", path.display(), e);
                errors += 1;
            }
        }
    }

    if files_loaded == 0 {
        eprintln!(
            "No terrain cell files were found/loaded under '{}'. \
             Did the export step actually run (see --skip-export)?",
            args.out
        );
        std::process::exit(1);
    }

    let output_dir = Path::new(FINAL_EXPORT_DIR).join(format!(
        "world_0x{:x}",
        args.world,
    ));
    if let Err(e) = std::fs::create_dir_all(&output_dir) {
        eprintln!("Failed to create output directory '{}': {}", output_dir.display(), e);
        std::process::exit(1);
    }

    let mut scalar_channels: Vec<usize> = scalar_images.keys().cloned().collect();
    scalar_channels.sort_unstable();

    for channel in &scalar_channels {
        let raw = &scalar_images[channel];
        if raw.coverage_fraction() == 0.0 {
            eprintln!("Channel {} has no data at all, skipping.", channel);
            continue;
        }

        let big_image = match fixed_channel_range(*channel) {
            Some((min, max)) => {
                raw.finalize_grayscale(min, max)
            }
            None if *channel == 1 => {
                match raw.value_range() {
                    Some((min, max)) => {
                        raw.finalize_grayscale(min, max)
                    }
                    None => {
                        raw.finalize_raw()
                    }
                }
            }
            None => {
                raw.finalize_raw()
            }
        };

        let out_path = output_dir.join(format!("{}.png", channel_label(*channel)));
        match big_image.save(&out_path) {
            Ok(()) => println!(
                "Saved channel {} ({} tile(s)) -> {}",
                channel,
                loaded_per_channel.get(channel).copied().unwrap_or(0),
                out_path.display()
            ),
            Err(e) => eprintln!("Error saving '{}': {}", out_path.display(), e),
        }
    }

    let mut vec3_channels: Vec<usize> = vec3_images.keys().cloned().collect();
    vec3_channels.sort_unstable();

    for channel in &vec3_channels {
        let (ix, iy, iz) = &vec3_images[channel];
        let (min, max) = fixed_channel_range(*channel).unwrap_or((-1.0, 1.0));
        let bx = ix.finalize_grayscale(min, max);
        let by = iy.finalize_grayscale(min, max);
        let bz = iz.finalize_grayscale(min, max);
        let label = channel_label(*channel);

        // for (component, img) in [("x", &bx), ("y", &by), ("z", &bz)] {
        //     let out_path = output_dir.join(format!("{}_{}.png", label, component));
        //     if let Err(e) = img.save(&out_path) {
        //         eprintln!("Error saving '{}': {}", out_path.display(), e);
        //     } else {
        //         println!("Saved channel {} component {} -> {}", channel, component, out_path.display());
        //     }
        // }

        let rgb_path = output_dir.join(format!("{}_rgb.png", label));
        match save_rgb_from_channels(&bx, &by, &bz, &rgb_path) {
            Ok(()) => println!(
                "Saved channel {} ({} tile(s)) as combined RGB -> {}",
                channel,
                loaded_per_channel.get(channel).copied().unwrap_or(0),
                rgb_path.display()
            ),
            Err(e) => eprintln!("Error saving '{}': {}", rgb_path.display(), e),
        }
    }

    let mut planes4_channels: Vec<usize> = planes4_images.keys().cloned().collect();
    planes4_channels.sort_unstable();
    let mut channel1_dims: Option<(u32, u32)> = None;

    for channel in &planes4_channels {
        let (i0, i1, i2, i3) = &planes4_images[channel];
        let label = channel_label(*channel);

        let mut finalized: HashMap<&str, landfile::BigImage> = HashMap::new();
        for (component, img) in [("layer0", i0), ("layer1", i1), ("layer2", i2), ("layer3", i3)] {
            if img.coverage_fraction() == 0.0 {
                continue;
            }
            let big_image = img.finalize_grayscale(0.0, 255.0);
            // if *channel == 1 {
            //     let out_path = output_dir.join(format!("{}_{}.png", label, component));
            //     if let Err(e) = big_image.save(&out_path) {
            //         eprintln!("Error saving '{}': {}", out_path.display(), e);
            //     } else {
            //         println!("Saved channel {} {} -> {}", channel, component, out_path.display());
            //     }
            // }
            finalized.insert(component, big_image);
        }

        // if *channel == 1 {
            match (
                finalized.get("layer0"),
                finalized.get("layer1"),
                finalized.get("layer2"),
                finalized.get("layer3"),
            ) {
                (Some(r), Some(g), Some(b), Some(a)) => {
                    let rgba_path = output_dir.join(format!("{}_rgba.png", label));
                    if *channel == 1 {
                        channel1_dims = Some(r.dimensions());
                        match save_rgba_from_channels(b, g, r, a, &rgba_path) { // flip the blue and red channels
                            Ok(()) => println!(
                                "Saved channel 1 as combined RGBA -> {}",
                                rgba_path.display()
                            ),
                            Err(e) => eprintln!("Error saving '{}': {}", rgba_path.display(), e),
                        }
                    } else {
                        match save_rgba_from_channels(r, g, b, a, &rgba_path) {
                            Ok(()) => println!(
                                "Saved channel {} as combined RGBA -> {}",
                                channel,
                                rgba_path.display()
                            ),
                            Err(e) => eprintln!("Error saving '{}': {}", rgba_path.display(), e),
                        }
                    }
                }
                _ => eprintln!(
                    "\tWarning: channel {} is missing one or more RGBA planes, skipping combined image",
                    channel
                ),
            }
        // }
    }

    if !cm_matches.is_empty() && args.channels.is_none() {
        let cm_min_x = cm_matches.iter().map(|c| c.x).min().unwrap();
        let cm_max_x = cm_matches.iter().map(|c| c.x).max().unwrap();
        let cm_min_z = cm_matches.iter().map(|c| c.z).min().unwrap();
        let cm_max_z = cm_matches.iter().map(|c| c.z).max().unwrap();
        let cm_grid_w = cm_max_x - cm_min_x + 1;
        let cm_grid_h = cm_max_z - cm_min_z + 1;

        println!(
            "\nStitching colour map: block grid {}x{} (x {}..{}, z {}..{})",
            cm_grid_w, cm_grid_h, cm_min_x, cm_max_x, cm_min_z, cm_max_z
        );

        let mut normal_grid = ColourMapGrid::new(cm_grid_w, cm_grid_h);
        let mut overlay_grid = ColourMapGrid::new(cm_grid_w, cm_grid_h);

        let mut dds_paths: HashMap<u32, std::path::PathBuf> = HashMap::new();
        for entry in WalkDir::new(&args.out).into_iter().filter_map(Result::ok) {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("dds") {
                continue;
            }
            if let Some(idx) = path.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u32>().ok()) {
                dds_paths.insert(idx, path.to_path_buf());
            }
        }

        for m in &cm_matches {
            let Some(path) = dds_paths.get(&m.index) else {
                eprintln!("\tWarning: couldn't find dds for colour map index {}", m.index);
                continue;
            };
            match image::open(path) {
                Ok(img) => {
                    let col = m.x - cm_min_x;
                    let row = m.z - cm_min_z;
                    if m.overlay {
                        overlay_grid.set_tile(col, row, &img);
                    } else {
                        normal_grid.set_tile(col, row, &img);
                    }
                }
                Err(e) => eprintln!("\tWarning: failed to decode {}: {}", path.display(), e),
            }
        }

        let normal_path = output_dir.join("colourmap.png");
        let overlay_path = output_dir.join("colourmap_overlay.png");

        let save_result = |grid: &ColourMapGrid, path: &Path| -> std::io::Result<()> {
            match channel1_dims {
                Some((w, h)) => grid.save_cropped(path, w, h),
                None => {
                    eprintln!("\tWarning: channel 1 dimensions unavailable, saving colour map uncropped");
                    grid.save(path)
                }
            }
        };

        if let Err(e) = save_result(&normal_grid, &normal_path) {
            eprintln!("Error saving '{}': {}", normal_path.display(), e);
        } else {
            println!("Saved colour map -> {}", normal_path.display());
        }
        if let Err(e) = save_result(&overlay_grid, &overlay_path) {
            eprintln!("Error saving '{}': {}", overlay_path.display(), e);
        } else {
            println!("Saved colour map overlay -> {}", overlay_path.display());
        }
    } else {
        println!("\nNo colour map tiles found for this world.");
    }

    write_placements(&args.out, placement_match.as_ref(), &cell_pm, &output_dir);
    if !lod_pm.is_empty() {
        write_lod_meshes(&args.out, &lod_pm, args.cell_size, !args.lod_no_skirts, &output_dir.join("terrain_lod"));
    }

    let total_channels = scalar_channels.len() + vec3_channels.len() + planes4_channels.len();
    if total_channels == 0 {
        eprintln!("No channels produced any tiles; nothing to save.");
    }

    if !args.no_cleanup {
        match std::fs::remove_dir_all(&args.out) {
            Ok(()) => println!("Removed temporary export directory '{}'.", args.out),
            Err(e) => eprintln!(
                "Warning: failed to remove temporary directory '{}': {}",
                args.out, e
            ),
        }
    }

    println!(
        "\nFinished. Loaded {} file(s) across {} channel(s), {} error(s). Output: {}",
        files_loaded,
        total_channels,
        errors,
        output_dir.display()
    );
}