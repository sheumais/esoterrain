mod archive;
mod out;
mod landfile;
mod lodmesh;
mod manifest;
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
use archive::Archive;
use manifest::filter_manifest;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::Path;

use crate::colourmap::{ColourMapGrid};
use crate::landfile::save_rgba_from_channels;
use crate::manifest::filter_colourmap_manifest;

const FINAL_EXPORT_DIR: &str = out::EXPORT_DIR;
const LUT_DIR: &str = out::LUT_DIR; // all *_lut.csv files; per-world model LUTs go in LUT_DIR/worlds/
/// Folder (or file) holding `oo2core_8_win64.dll`; needed to read Oodle-compressed entries.
const DEFAULT_OODLE: &str = "./tool";
const RAW_MNF: &str = "C:/Program Files (x86)/Steam/steamapps/common/Zenimax Online/The Elder Scrolls Online/depot/eso.mnf";
const NOESIS: &str = "./tool/Noesis64.exe";
const MAX_SANE_GRID_SPAN: u32 = 128;

struct Args {
    oodle: String,
    world: u32,
    mnf: String,
    dry_run: bool,
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
    tl_split: Option<Vec<u32>>,
    tl_only: Vec<(u32, u32)>,
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

/// Reads the world placement file and the per-cell placement files straight from the archive and writes the JSON.
/// Returns the number of failures.
fn write_placements(
    archive: &Archive,
    world_pm: Option<&manifest::PlacementMatch>,
    cell_pms: &[manifest::CellPlacementMatch],
    output_dir: &Path,
) -> usize {
    let mut failures = 0usize;
    let mut cells_by_index: HashMap<u32, Vec<&manifest::CellPlacementMatch>> = HashMap::new();
    for c in cell_pms {
        cells_by_index.entry(c.index).or_default().push(c);
    }
    let mut wanted: Vec<u32> = cells_by_index.keys().copied().collect();
    if let Some(pm) = world_pm {
        wanted.push(pm.index);
    }

    let cell_dir = output_dir.join("cell_placement");
    let mut ok = 0usize;
    let mut seen: HashSet<u32> = HashSet::new();
    archive.read_each(&wanted, |e, data| {
        seen.insert(e.index);
        let is_world = world_pm.map_or(false, |pm| pm.index == e.index);
        let uses = (if is_world { 1 } else { 0 }) + cells_by_index.get(&e.index).map_or(0, |v| v.len());
        let bytes = match data {
            Ok(b) => b,
            Err(msg) => {
                eprintln!("Error reading placement file (index {}): {}", e.index, msg);
                failures += uses;
                return;
            }
        };
        if is_world {
            let out_path = output_dir.join("placement.json");
            match placement::parse_and_write(&bytes, &out_path) {
                Ok(()) => println!("Parsed placement/instance list -> {}", out_path.display()),
                Err(err) => { eprintln!("Error parsing placement file (index {}): {}", e.index, err); failures += 1; }
            }
        }
        for c in cells_by_index.get(&e.index).into_iter().flatten() {
            let out_path = cell_dir.join(format!("cell_x{}_z{}.json", c.x, c.z));
            match cell_placement::parse_and_write_cell(&bytes, &out_path) {
                Ok(()) => ok += 1,
                Err(err) => { eprintln!("Error parsing cell placement (index {}, cell {},{}): {}", c.index, c.x, c.z, err); failures += 1; }
            }
        }
    });
    // Entries `read_each` could not even locate (index outside the table).
    for idx in &wanted {
        if !seen.contains(idx) {
            eprintln!("Warning: placement file (index {}) is not in the archive table.", idx);
            failures += 1;
        }
    }
    if !cell_pms.is_empty() {
        println!("Parsed {}/{} cell placement files -> {}", ok, cell_pms.len(), cell_dir.display());
    }
    failures
}

/// Terrain LOD meshes (manifest type 0x0A) -> `<output_dir>/lod_x<X>_z<Z>_ch<N>.obj`, one per record in each file.
/// Returns the number of failed files.
fn write_lod_meshes(
    archive: &Archive,
    lods: &[manifest::LodMatch],
    cell_size: f64,
    skirts: bool,
    output_dir: &Path,
) -> usize {
    let mut by_index: HashMap<u32, Vec<&manifest::LodMatch>> = HashMap::new();
    for l in lods {
        by_index.entry(l.index).or_default().push(l);
    }
    let wanted: Vec<u32> = by_index.keys().copied().collect();
    let (mut ok_files, mut obj_count, mut failures) = (0usize, 0usize, 0usize);
    archive.read_each(&wanted, |e, data| {
        for l in by_index.get(&e.index).into_iter().flatten() {
            let label = format!("LOD mesh index {} (cell {},{})", l.index, l.x, l.z);
            match &data {
                Ok(bytes) => match lodmesh::export_file(bytes, &label, l.x, l.z, cell_size, skirts, output_dir) {
                    Ok(written) => { ok_files += 1; obj_count += written.len(); }
                    Err(err) => { eprintln!("Error parsing {}: {}", label, err); failures += 1; }
                },
                Err(msg) => { eprintln!("Error reading {}: {}", label, msg); failures += 1; }
            }
        }
    });
    println!("Exported {}/{} terrain LOD files ({} OBJ) -> {}", ok_files, lods.len(), obj_count, output_dir.display());
    failures
}

/// `--dry-run`: how many entries would be read, per archive. Nothing is read.
fn print_plan(archive: &Archive, what: &str, indices: &[u32]) {
    let mut per_arch: std::collections::BTreeMap<u8, usize> = std::collections::BTreeMap::new();
    for i in indices {
        if let Some(e) = archive.entries().get(*i as usize) {
            *per_arch.entry(e.archive_index).or_insert(0) += 1;
        }
    }
    println!("\nDry run: would read {} {} entr{} from {} archive(s):", indices.len(), what, if indices.len() == 1 { "y" } else { "ies" }, per_arch.len());
    for (a, n) in per_arch {
        println!("  archive {:>4}: {:>6} entr{}", a, n, if n == 1 { "y" } else { "ies" });
    }
    println!("\nDry run complete.");
}

fn print_usage() {
    eprintln!(
        "Usage: land_export (or cargo run) <world> [options]\n\n\
Required:\n  \
<world>              World ID, hex (0xe9) or decimal\n\n\
Options:\n  \
--mnf <path>         Path to raw .mnf (default: {})\n  \
--oodle <path>       Folder (or file) holding oo2core_8_win64.dll (default: {})\n  \
--channels <list>    Comma-separated channel indices to export (default: all found)\n  \
--dry-run            Show what would happen without exporting/deleting\n  \
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
--terrain-layer-split <kinds|all>  With --terrain-layers: also write k<kind>_layers/id_<id>.png (one full-size mask per layer id) + index.csv
--terrain-layer-id <kind>:<id>[,...]  With --terrain-layers: only write these ids (implies a split for their kinds)
--world-model-lut    For EVERY world: export its placement files and write luts/worlds/world_0x<id>.csv\n  \
                       (model_id,x,y,z; sorted; from the world placement file only). Each placement file is read once.\n \
--model-summary      Read every luts/worlds/world_0x<id>.csv and write luts/model_usage_summary.csv (each model: total count, worlds it appears in)\n                       and luts/model_manifest_entries.csv (the raw manifest line(s) for each model). Also runs automatically after --world-model-lut / --all-luts\n \
--worlds <list>      With --world-model-lut: only these worlds (comma-separated, hex or decimal)\n \
--with-rot           With --world-model-lut: also write rx,ry,rz columns\n \
--all-luts           Write every LUT (run once per patch): the world, model, creature, item and keep LUTs are read straight from the archives, then\n                       --world-model-lut runs. --mnf/--oodle/--worlds/--with-rot/--dry-run apply\n \
--
",
        RAW_MNF, DEFAULT_OODLE, NOESIS
    );
}

fn parse_args() -> Args {
    let raw: Vec<String> = env::args().skip(1).collect();
    let mut positional = Vec::new();
    let mut oodle = DEFAULT_OODLE.to_string();
    let mut mnf = RAW_MNF.to_string();
    let mut dry_run = false;
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
    let mut tl_split: Option<Vec<u32>> = None;
    let mut tl_only: Vec<(u32, u32)> = Vec::new();

    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--oodle" => {
                i += 1;
                oodle = raw.get(i).cloned().expect("--oodle needs a value");
            }
            "--mnf" => {
                i += 1;
                mnf = raw.get(i).cloned().expect("--mnf needs a value");
            }
            "--channels" => {
                i += 1;
                let val = raw.get(i).expect("--channels needs a value");
                channels = Some(parse_channels_arg(val));
            }
            "--dry-run" => dry_run = true,
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
                eprintln!("Note: the manifest argument is no longer used (entries are read from the MNF directly); ignoring '{}'.", positional[0]);
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
        oodle,
        world,
        mnf,
        dry_run,
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
        tl_split,
        tl_only,
    }
}

/// Looks up a table entry by its FileIndex and says what was found.
fn table_match(archive: &Archive, fi: u32, label: &str) -> Result<manifest::TableMatch, String> {
    match manifest::filter_table_manifest(archive.entries(), fi) {
        Some(m) => { println!("{}: index {} (arch {})", label, m.index, m.arch); Ok(m) }
        None => Err(format!("{} (file_index 0x{:x}) not found in the archive table.", label, fi)),
    }
}

fn table_matches(archive: &Archive, which: &[(u32, &str)]) -> Result<Vec<manifest::TableMatch>, String> {
    which.iter().map(|(fi, label)| table_match(archive, *fi, label)).collect()
}

/// The EsoFileData table of `m`, in the layout the old `*_Uncompressed.EsoFileData` files had.
fn table_bytes(archive: &Archive, m: &manifest::TableMatch, label: &str) -> Result<Vec<u8>, String> {
    archive.table(m.index).map_err(|e| format!("{} table: {}", label, e))
}

/// One name group of en.lang. A missing en.lang gives a warning and an empty map (names are optional).
fn lang_group(archive: &Archive, group: &str) -> HashMap<u32, String> {
    match archive.lang() {
        Ok(l) => l.group(group).clone(),
        Err(e) => { eprintln!("Warning: {}; names will be empty.", e); HashMap::new() }
    }
}

fn build_world_lut(archive: &Archive, m: &manifest::TableMatch) -> Result<(), String> {
    let data = table_bytes(archive, m, "World LUT")?;
    println!("Parsing world table (index {})", m.index);
    std::fs::create_dir_all(LUT_DIR).map_err(|e| format!("failed to create luts dir: {}", e))?;
    let out_path = Path::new(LUT_DIR).join("worlds_lut.csv");

    let zone_names = match archive.lang() {
        Ok(l) => {
            let n = l.group(worldlut::ZONE_NAME_GROUP).clone();
            println!("Loaded {} zone names from en.lang", n.len());
            n
        }
        Err(e) => {
            eprintln!("Warning: {}; zone_name will be empty.", e);
            Default::default()
        }
    };
    let n = worldlut::parse_and_write(&data, &zone_names, &out_path)
        .map_err(|e| format!("Error parsing world table (index {}): {}", m.index, e))?;
    println!("Wrote {} worlds -> {}", n, out_path.display());
    Ok(())
}

fn build_model_lut(archive: &Archive, m: &manifest::TableMatch) -> Result<(), String> {
    let data = table_bytes(archive, m, "Model list")?;
    let names = lang_group(archive, model::MODEL_NAME_GROUP);
    std::fs::create_dir_all(LUT_DIR).map_err(|e| format!("failed to create luts dir: {}", e))?;
    let out_path = Path::new(LUT_DIR).join("models_lut.csv");
    let n = model::parse_and_write(&data, &names, &out_path)
        .map_err(|e| format!("Error parsing model list (index {}): {}", m.index, e))?;
    println!("Wrote {} models -> {}", n, out_path.display());
    Ok(())
}

fn load_table(archive: &Archive, m: &manifest::TableMatch, label: &str) -> Result<creature::Table, String> {
    let data = table_bytes(archive, m, label)?;
    creature::Table::from_bytes(data, &format!("{} table (index {})", label, m.index)).map_err(|e| e.to_string())
}

/// `found` = [textures, links, models, creatures]
fn build_creature_lut(archive: &Archive, found: &[manifest::TableMatch]) -> Result<(), String> {
    let textures = load_table(archive, &found[0], "Texture")?;
    let links = load_table(archive, &found[1], "Link")?;
    let models = load_table(archive, &found[2], "Model")?;
    let creature_tbl = load_table(archive, &found[3], "Creature")?;
    let names = lang_group(archive, creature::CREATURE_NAME_GROUP);
    println!("Loaded {} creature names.", names.len());
    std::fs::create_dir_all(LUT_DIR).map_err(|e| format!("failed to create luts dir: {}", e))?;
    let out_path = Path::new(LUT_DIR).join("creatures_lut.csv");
    let n = creature::build_csv(&textures, &links, &models, &creature_tbl, &names, &out_path)
        .map_err(|e| format!("Error writing '{}': {}", out_path.display(), e))?;
    println!("Wrote {} creatures -> {}", n, out_path.display());
    Ok(())
}

/// `found` = [materials, part materials, item visual templates, item names]
fn build_item_lut(archive: &Archive, rows: &model_export::RowMap, found: &[manifest::TableMatch], args: &Args) -> Result<(), String> {
    let materials = load_table(archive, &found[0], "Material")?;
    let part_mats = load_table(archive, &found[1], "Part material")?;
    let templates = load_table(archive, &found[2], "Item visual template")?;
    let name_tbl = load_table(archive, &found[3], "Item name")?;
    let names = lang_group(archive, items::ITEM_NAME_GROUP);
    println!("Loaded {} item names.", names.len());
    std::fs::create_dir_all(LUT_DIR).map_err(|e| format!("failed to create luts dir: {}", e))?;
    let out_path = Path::new(LUT_DIR).join("items_lut.csv");
    // Parts with no lang name get the mesh name of their model instead (read straight from the archive).
    let item_cache = model_export::ReadCache::new();
    let mesh_names = |ids: &[u32]| -> HashMap<u32, String> {
        let opts = model_export::Options {
            archive,
            manifest: rows,
            cache: &item_cache,
            out_root: Path::new(LUT_DIR),
            dry_run: args.dry_run,
            noesis: &args.noesis,
        };
        model_export::mesh_names(ids, &opts)
    };
    let n = items::build_csv(&templates, &part_mats, &materials, &name_tbl, &names, &mesh_names, &out_path)
        .map_err(|e| format!("Error writing '{}': {}", out_path.display(), e))?;
    println!("Wrote {} item parts -> {}", n, out_path.display());
    Ok(())
}

fn build_keep_lut(archive: &Archive, m: &manifest::TableMatch) -> Result<(), String> {
    let data = table_bytes(archive, m, "Keeps")?;
    let out_path = Path::new(LUT_DIR).join("keeps_lut.csv");
    let n = keep_export::write_lut(&data, &out_path)
        .map_err(|e| format!("Error parsing keeps table (index {}): {}", m.index, e))?;
    println!("Wrote {} keeps -> {}", n, out_path.display());
    Ok(())
}

const CREATURE_TABLES: [(u32, &str); 4] = [(0x05, "textures"), (0x06, "links"), (0x07, "models"), (0x08, "creatures")];
const ITEM_TABLES: [(u32, &str); 4] = [
    (items::MATERIAL_DEF, "materials"),
    (items::ITEM_PART_MATERIAL_DEF, "part materials"),
    (items::ITEM_VISUAL_TEMPLATE_DEF, "item visual templates"),
    (items::ITEM_NAME_TABLE_DEF, "item names"),
];

/// World, model, creature, item and keep LUTs: every table is read by its entry index, en.lang once.
fn run_core_luts(archive: &Archive, rows: &model_export::RowMap, args: &Args) -> Result<(), String> {
    let world = table_match(archive, manifest::WORLD_LUT_FILE_INDEX, "world table")?;
    let model_m = table_match(archive, manifest::MODEL_LIST_FILE_INDEX, "model list")?;
    let creature_set = table_matches(archive, &CREATURE_TABLES)?;
    let keep_m = table_match(archive, keep_export::KEEPS_FILE_INDEX, "keeps")?;
    let item_set = table_matches(archive, &ITEM_TABLES)?;

    if args.dry_run {
        let mut all: Vec<u32> = [world.index, model_m.index, keep_m.index].to_vec();
        all.extend(creature_set.iter().chain(item_set.iter()).map(|m| m.index));
        print_plan(archive, "table", &all);
        return Ok(());
    }

    let mut errors: Vec<String> = Vec::new();
    println!();
    if let Err(e) = build_world_lut(archive, &world) { errors.push(e); }
    if let Err(e) = build_model_lut(archive, &model_m) { errors.push(e); }
    if let Err(e) = build_creature_lut(archive, &creature_set) { errors.push(e); }
    if let Err(e) = build_item_lut(archive, rows, &item_set, args) { errors.push(e); }
    if let Err(e) = build_keep_lut(archive, &keep_m) { errors.push(e); }

    if errors.is_empty() { Ok(()) } else { Err(errors.join("\n")) }
}

fn run_world_model_lut(archive: &Archive, args: &Args) -> Result<(), String> {
    let out_dir = Path::new(LUT_DIR).join("worlds");
    let a = world_models::RunArgs {
        archive,
        out_dir: &out_dir,
        only_worlds: &args.lut_worlds,
        with_rot: args.lut_rot,
        dry_run: args.dry_run,
    };
    world_models::run(&a)?;
    if args.dry_run { return Ok(()); }
    run_model_summary(archive)
}

/// Summarise every luts/worlds/*.csv (model usage + the manifest line for each model).
fn run_model_summary(archive: &Archive) -> Result<(), String> {
    model_summary::run(&Path::new(LUT_DIR).join("worlds"), archive, Path::new(LUT_DIR))
}

/// Stitch the terrain layer masks (table 0x44) of one world into export/world_0x<id>/terrain_layers/.
/// Zone names for the kind-2 layer come from en.lang.
fn run_terrain_layers(archive: &Archive, args: &Args, world: u32) -> Result<(), String> {
    let table = table_match(archive, terrain_layers::TERRAIN_LAYER_FILE_INDEX, "terrain layer table")?;
    if args.dry_run { print_plan(archive, "table", &[table.index]); return Ok(()); }

    let data = table_bytes(archive, &table, "Terrain layer")?;
    let lang = match archive.lang() {
        Ok(l) => Some(l),
        Err(e) => { eprintln!("Warning: {}", e); None }
    };
    let out_dir = Path::new(FINAL_EXPORT_DIR).join(format!("world_0x{:x}", world)).join("terrain_layers");
    terrain_layers::run(&data, lang, world, &out_dir,
        &terrain_layers::Split { kinds: args.tl_split.clone(), only: args.tl_only.clone() })
}

/// Parse the terrain texture table (file id 0x29), write every texture it references to export/terrain_textures
/// (skipping ones already there, so it can be re-run) and write the LUT.
fn run_terrain_textures(archive: &Archive, rows: &model_export::RowMap, args: &Args) -> Result<(), String> {
    use std::collections::{BTreeMap, BTreeSet};

    let table = table_match(archive, terrain_textures::TERRAIN_TEXTURE_FILE_INDEX, "terrain textures table")?;
    let dest = Path::new(FINAL_EXPORT_DIR).join("terrain_textures");
    let lut_path = Path::new(LUT_DIR).join("terrain_textures_lut.csv");

    // 1. read and parse the table
    let data = table_bytes(archive, &table, "Terrain texture")?;
    let entries = terrain_textures::parse(&data)?;
    let wanted: BTreeSet<u32> = terrain_textures::texture_indices(&entries);
    println!("Parsed {} terrain textures referencing {} distinct texture FileIndexes.", entries.len(), wanted.len());

    // 2. resolve FileIndex -> entry (Index + archive)
    let found = model_export::lookup_file_indexes(rows, &wanted.iter().copied().collect());
    let mut resolved: BTreeMap<u32, model_export::MnfRow> = BTreeMap::new();
    for fi in &wanted {
        if let Some(r) = found.get(fi).and_then(|v| v.first()) {
            resolved.insert(*fi, r.clone());
        }
    }
    if resolved.len() < wanted.len() {
        eprintln!("Warning: {} texture FileIndex(es) not found in the archive table.", wanted.len() - resolved.len());
    }
    let fi_to_index: BTreeMap<u32, u32> = resolved.iter().map(|(fi, r)| (*fi, r.index)).collect();

    // 3. what still has to be written (resumable)
    let have = terrain_textures::exported_names(&dest, &fi_to_index);
    let mut todo: Vec<model_export::MnfRow> = Vec::new();
    let mut skipped_unavailable = 0usize;
    for (fi, r) in &resolved {
        if have.contains_key(fi) { continue; }
        if r.arch >= manifest::UNAVAILABLE_ARCHIVE_THRESHOLD {
            skipped_unavailable += 1;
            continue;
        }
        todo.push(r.clone());
    }
    println!("{} texture(s) already exported, {} to go ({} in archives not available in the client files).", have.len(), todo.len(), skipped_unavailable);

    if args.dry_run {
        let idxs: Vec<u32> = todo.iter().map(|r| r.index).collect();
        print_plan(archive, "texture", &idxs);
        return Ok(());
    }

    // 4. read exactly those entries and write them as .dds
    std::fs::create_dir_all(&dest).map_err(|e| format!("failed to create '{}': {}", dest.display(), e))?;
    let idxs: Vec<u32> = todo.iter().map(|r| r.index).collect();
    let (mut written, mut failed) = (0usize, 0usize);
    archive.read_each(&idxs, |e, data| {
        let dds = data.and_then(|b| archive::dds_from_bytes(b).map_err(|m| format!("entry {}: {}", e.index, m)));
        match dds.and_then(|b| std::fs::write(dest.join(format!("{}.dds", e.index)), b).map_err(|m| m.to_string())) {
            Ok(()) => written += 1,
            Err(msg) => { eprintln!("  Texture (Index {}, archive {}) could not be exported: {}", e.index, e.archive_index, msg); failed += 1; }
        }
    });
    println!("Wrote {} of {} texture(s) -> {} ({} failed).", written, idxs.len(), dest.display(), failed);

    // 5. LUT
    let files = terrain_textures::exported_names(&dest, &fi_to_index);
    let n = terrain_textures::write_lut(&entries, &files, &lut_path)?;
    println!("\nWrote {} terrain textures -> {}", n, lut_path.display());
    println!("{} of {} texture file(s) present in {}", files.len(), wanted.len(), dest.display());
    Ok(())
}

const ANSI_RED: &str = "\x1b[31m";
const ANSI_RESET: &str = "\x1b[0m";

fn main() {
    let args = parse_args();
    model_export::set_auto_variants(!args.no_auto_variants);

    let archive = match Archive::open(Path::new(&args.mnf), Some(Path::new(&args.oodle))) {
        Ok(a) => a,
        Err(e) => { eprintln!("{}", e); std::process::exit(1); }
    };
    // FileIndex -> entries (Unk1 == 0): how texture/model references are resolved.
    let rows = model_export::build_rows(archive.entries());
    // Entries read for models/textures, shared within this run (item parts share textures); bounded by `trim`.
    let cache = model_export::ReadCache::new();

    if args.list_worlds {
        println!("Scanning '{}' for worlds with terrain cell data...", args.mnf);
        let summaries = manifest::list_worlds_with_cells(archive.entries());

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
        let mut failed: Vec<&str> = Vec::new();
        println!("===== [1/2] world, model, creature, item and keep LUTs =====");
        if let Err(e) = run_core_luts(&archive, &rows, &args) { eprintln!("{}", e); failed.push("world/model/creature/item/keep LUTs"); }
        println!("\n===== [2/2] per-world model LUTs =====");
        if let Err(e) = run_world_model_lut(&archive, &args) { eprintln!("{}", e); failed.push("per-world model LUTs"); }
        if failed.is_empty() {
            println!("\nAll LUTs written to {}.", LUT_DIR);
            return;
        }
        eprintln!("\nFailed: {}", failed.join(", "));
        std::process::exit(1);
    }

    if args.model_summary {
        if let Err(e) = run_model_summary(&archive) { eprintln!("{}", e); std::process::exit(1); }
        return;
    }

    if args.world_model_lut {
        if let Err(e) = run_world_model_lut(&archive, &args) { eprintln!("{}", e); std::process::exit(1); }
        return;
    }

    if args.terrain_textures {
        if let Err(e) = run_terrain_textures(&archive, &rows, &args) { eprintln!("{}", e); std::process::exit(1); }
        return;
    }

    if let Some(world) = args.terrain_layers {
        if let Err(e) = run_terrain_layers(&archive, &args, world) { eprintln!("{}", e); std::process::exit(1); }
        return;
    }

    if !args.scene.is_empty() {
        let models_dir = Path::new(FINAL_EXPORT_DIR).join("models");
        let scenes_dir = Path::new(FINAL_EXPORT_DIR).join("scenes");
        let opts = model_export::Options {
            archive: &archive,
            manifest: &rows,
            cache: &cache,
            out_root: &models_dir,
            dry_run: args.dry_run,
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
            archive: &archive,
            manifest: &rows,
            cache: &cache,
            out_root: &models_dir,
            dry_run: args.dry_run,
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
            archive: &archive,
            manifest: &rows,
            cache: &cache,
            out_root: &models_dir,
            dry_run: args.dry_run,
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
        if failed > 0 { std::process::exit(1); }
        return;
    }

    if !args.model_fis.is_empty() {
        let models_dir = Path::new(FINAL_EXPORT_DIR).join("models");
        let opts = model_export::Options {
            archive: &archive,
            manifest: &rows,
            cache: &cache,
            out_root: &models_dir,
            dry_run: args.dry_run,
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
            archive.entries(),
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
        let pm = manifest::filter_placement_manifest(archive.entries(), args.world);
        let cell_pms = manifest::filter_cell_placement_manifest(archive.entries(), args.world);
        if pm.is_none() && cell_pms.is_empty() {
            eprintln!("No placement/instance lists found for this world.");
            std::process::exit(1);
        }
        if let Some(pm) = &pm {
            println!("Placement/instance list: index {} (arch {})", pm.index, pm.arch);
        }
        println!("Cell placement files: {}", cell_pms.len());

        if args.dry_run {
            let wanted: Vec<u32> = pm.iter().map(|p| p.index).chain(cell_pms.iter().map(|c| c.index)).collect();
            print_plan(&archive, "placement", &wanted);
            return;
        }

        let output_dir = Path::new(FINAL_EXPORT_DIR).join(format!("world_0x{:x}", args.world)).join("placements");
        if let Err(e) = std::fs::create_dir_all(&output_dir) {
            eprintln!("Failed to create output directory '{}': {}", output_dir.display(), e);
            std::process::exit(1);
        }

        if write_placements(&archive, pm.as_ref(), &cell_pms, &output_dir) > 0 {
            std::process::exit(1);
        }

        println!("\nFinished (placement-only).");
        return;
    }

    if args.lod_only {
        println!("LOD-only mode: looking up terrain LOD mesh files for world 0x{:x}...", args.world);
        let lods = manifest::filter_lod_manifest(archive.entries(), args.world);
        if lods.is_empty() {
            eprintln!("No terrain LOD mesh files found for this world.");
            std::process::exit(1);
        }
        println!("Terrain LOD mesh files: {}", lods.len());

        if args.dry_run {
            let wanted: Vec<u32> = lods.iter().map(|l| l.index).collect();
            print_plan(&archive, "terrain LOD mesh", &wanted);
            return;
        }

        let output_dir = Path::new(FINAL_EXPORT_DIR).join(format!("world_0x{:x}", args.world)).join("terrain_lod");
        let failures = write_lod_meshes(&archive, &lods, args.cell_size, !args.lod_no_skirts, &output_dir);

        println!("\nFinished (lod-only).");
        if failures > 0 { std::process::exit(1); }
        return;
    }

    if args.world_lut {
        let m = match table_match(&archive, manifest::WORLD_LUT_FILE_INDEX, "World LUT") {
            Ok(m) => m,
            Err(e) => { eprintln!("{}", e); std::process::exit(1); }
        };
        if args.dry_run { print_plan(&archive, "table", &[m.index]); return; }
        if let Err(e) = build_world_lut(&archive, &m) { eprintln!("{}", e); std::process::exit(1); }
        return;
    }

    if args.model_list {
        let m = match table_match(&archive, manifest::MODEL_LIST_FILE_INDEX, "Model list") {
            Ok(m) => m,
            Err(e) => { eprintln!("{}", e); std::process::exit(1); }
        };
        if args.dry_run { print_plan(&archive, "table", &[m.index]); return; }
        if let Err(e) = build_model_lut(&archive, &m) { eprintln!("{}", e); std::process::exit(1); }
        return;
    }

    if args.keep_list {
        let m = match table_match(&archive, keep_export::KEEPS_FILE_INDEX, "Keeps") {
            Ok(m) => m,
            Err(e) => { eprintln!("{}", e); std::process::exit(1); }
        };
        if args.dry_run { print_plan(&archive, "table", &[m.index]); return; }
        if let Err(e) = build_keep_lut(&archive, &m) { eprintln!("{}", e); std::process::exit(1); }
        return;
    }

    if args.keeps {
        let found = match table_matches(&archive, &[(keep_export::KEEPS_FILE_INDEX, "keeps"), (keep_export::PIECES_FILE_INDEX, "keep pieces")]) {
            Ok(f) => f,
            Err(e) => { eprintln!("{}", e); std::process::exit(1); }
        };
        if args.dry_run {
            let idx: Vec<u32> = found.iter().map(|m| m.index).collect();
            print_plan(&archive, "table", &idx);
            return;
        }
        let keeps_data = match table_bytes(&archive, &found[0], "Keeps") { Ok(d) => d, Err(e) => { eprintln!("{}", e); std::process::exit(1); } };
        let pieces_data = match table_bytes(&archive, &found[1], "Keep pieces") { Ok(d) => d, Err(e) => { eprintln!("{}", e); std::process::exit(1); } };

        let a = keep_export::KeepArgs {
            keeps_data: &keeps_data,
            pieces_data: &pieces_data,
            cell_m: 3.0,
            units_per_metre: 100.0,
            only_key: args.keep_key,
            out_file: Path::new("export/keeps_placement.json"),
        };
        if let Err(e) = keep_export::run(&a) { eprintln!("{}", e); std::process::exit(1); }
        return;
    }

    if args.creature_list {
        let found = match table_matches(&archive, &CREATURE_TABLES) {
            Ok(f) => f,
            Err(e) => { eprintln!("{}", e); std::process::exit(1); }
        };
        if args.dry_run {
            let idx: Vec<u32> = found.iter().map(|m| m.index).collect();
            print_plan(&archive, "table", &idx);
            return;
        }
        if let Err(e) = build_creature_lut(&archive, &found) { eprintln!("{}", e); std::process::exit(1); }
        return;
    }
    if args.item_list {
        let found = match table_matches(&archive, &ITEM_TABLES) {
            Ok(f) => f,
            Err(e) => { eprintln!("{}", e); std::process::exit(1); }
        };
        if args.dry_run {
            let idx: Vec<u32> = found.iter().map(|m| m.index).collect();
            print_plan(&archive, "table", &idx);
            return;
        }
        if let Err(e) = build_item_lut(&archive, &rows, &found, &args) { eprintln!("{}", e); std::process::exit(1); }
        return;
    }
    println!("Filtering manifest for world 0x{:x}...", args.world);
    let result = filter_manifest(archive.entries(), args.world);

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

    let cm_matches = filter_colourmap_manifest(archive.entries(), args.world);
    println!("Colour map matches: {}", cm_matches.len());
    for m in &cm_matches {
        wanted_indices.insert(m.index);
    }

    let placement_match = manifest::filter_placement_manifest(archive.entries(), args.world);
    let cell_pm = manifest::filter_cell_placement_manifest(archive.entries(), args.world);
    println!("Cell placement files: {}", cell_pm.len());
    for c in &cell_pm {
        wanted_indices.insert(c.index);
    }
    let lod_pm = manifest::filter_lod_manifest(archive.entries(), args.world);
    println!("Terrain LOD mesh files: {}", lod_pm.len());
    for l in &lod_pm {
        wanted_indices.insert(l.index);
    }

    // Cells and colour maps are read here; placement and LOD files are read by their own writers further down.
    let stitch_colourmap = !cm_matches.is_empty() && args.channels.is_none();
    let mut read_indices: Vec<u32> = result.cell_matches.iter().map(|c| c.index).collect();
    if stitch_colourmap {
        read_indices.extend(cm_matches.iter().map(|m| m.index));
    }
    read_indices.sort_unstable();
    read_indices.dedup();

    if args.dry_run {
        print_plan(&archive, "terrain cell / colour map", &read_indices);
        let mut other: Vec<u32> = placement_match.iter().map(|p| p.index).chain(cell_pm.iter().map(|c| c.index)).chain(lod_pm.iter().map(|l| l.index)).collect();
        other.sort_unstable();
        other.dedup();
        print_plan(&archive, "placement / LOD", &other);
        return;
    }

    // Colour map grids are filled while reading, so the DDS data is dropped right after decoding.
    let cm_bounds = if stitch_colourmap {
        Some((
            cm_matches.iter().map(|c| c.x).min().unwrap(),
            cm_matches.iter().map(|c| c.x).max().unwrap(),
            cm_matches.iter().map(|c| c.z).min().unwrap(),
            cm_matches.iter().map(|c| c.z).max().unwrap(),
        ))
    } else {
        None
    };
    let (cm_min_x, cm_min_z) = cm_bounds.map(|b| (b.0, b.2)).unwrap_or((0, 0));
    let (cm_grid_w, cm_grid_h) = cm_bounds.map(|b| (b.1 - b.0 + 1, b.3 - b.2 + 1)).unwrap_or((1, 1));
    let mut normal_grid = ColourMapGrid::new(cm_grid_w, cm_grid_h);
    let mut overlay_grid = ColourMapGrid::new(cm_grid_w, cm_grid_h);
    let mut cm_by_index: HashMap<u32, Vec<&manifest::ColourMapMatch>> = HashMap::new();
    if stitch_colourmap {
        for m in &cm_matches {
            cm_by_index.entry(m.index).or_default().push(m);
        }
    }

    println!("\nReading cell files and accumulating raw channel data...");
    let mut scalar_images: HashMap<usize, RawBigImage> = HashMap::new();
    let mut vec3_images: HashMap<usize, (RawBigImage, RawBigImage, RawBigImage)> = HashMap::new();
    let mut planes4_images: HashMap<usize, (RawBigImage, RawBigImage, RawBigImage, RawBigImage)> = HashMap::new();
    let mut loaded_per_channel: HashMap<usize, usize> = HashMap::new();
    let mut files_loaded = 0usize;
    let mut errors = 0usize;

    let mut read_cb = |e: &eso_common::mnf::MnfFileEntry, data: Result<Vec<u8>, String>| {
        let bytes = match data {
            Ok(b) => b,
            Err(msg) => { eprintln!("Error reading entry {}: {}", e.index, msg); errors += 1; return; }
        };
        if let Some((x, z)) = index_to_xz.get(&e.index).copied() {
            let label = format!("cell file index {} (cell {},{})", e.index, x, z);
            match LandFile::from_bytes(&bytes, &label) {
                Ok(None) => {}
                Ok(Some(file)) => {
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
                Err(err) => {
                    eprintln!("Error loading {}: {}", label, err);
                    errors += 1;
                }
            }
        }
        if let Some(ms) = cm_by_index.get(&e.index) {
            match archive::dds_from_bytes(bytes).and_then(|d| image::load_from_memory(&d).map_err(|er| er.to_string())) {
                Ok(img) => {
                    for m in ms {
                        let col = m.x - cm_min_x;
                        let row = m.z - cm_min_z;
                        if m.overlay { overlay_grid.set_tile(col, row, &img); } else { normal_grid.set_tile(col, row, &img); }
                    }
                }
                Err(er) => eprintln!("\tWarning: failed to decode colour map index {}: {}", e.index, er),
            }
        }
    };
    archive.read_each(&read_indices, &mut read_cb);
    drop(read_cb);

    if files_loaded == 0 {
        eprintln!("No terrain cell files were found/loaded for this world.");
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

    if stitch_colourmap {
        println!(
            "\nStitched colour map: block grid {}x{} (x from {}, z from {})",
            cm_grid_w, cm_grid_h, cm_min_x, cm_min_z
        );

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

    write_placements(&archive, placement_match.as_ref(), &cell_pm, &output_dir);
    if !lod_pm.is_empty() {
        write_lod_meshes(&archive, &lod_pm, args.cell_size, !args.lod_no_skirts, &output_dir.join("terrain_lod"));
    }

    let total_channels = scalar_channels.len() + vec3_channels.len() + planes4_channels.len();
    if total_channels == 0 {
        eprintln!("No channels produced any tiles; nothing to save.");
    }

    println!(
        "\nFinished. Loaded {} file(s) across {} channel(s), {} error(s). Output: {}",
        files_loaded,
        total_channels,
        errors,
        output_dir.display()
    );
}
