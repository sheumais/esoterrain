mod export;
mod landfile;
mod manifest;
mod reader;
mod colourmap;
mod placement;
mod worldlut;
mod model;
mod creature;
mod cell_placement;
mod sizemap;

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
const MNF_EXPORT: &str = "eso_mnf.csv"; // can't upload to github due to file size limits.
const RAW_MNF: &str = "C:/Program Files (x86)/Steam/steamapps/common/Zenimax Online/The Elder Scrolls Online/depot/eso.mnf";
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
    list_worlds: bool,
    world_lut: bool,
    model_list: bool,
    creature_list: bool,
    size_map: bool,
    size_type: Option<u32>,
    size_scale: u32,
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

fn channel_label(index: usize) -> String {
    match index {
        0 => "00_heightmap".to_string(),
        1 => "01_dev_overlay".to_string(),
        2 => "02_unknown".to_string(),
        3 => "03_unknown".to_string(),
        4 => "04_unknown".to_string(),
        5 => "05_unknown".to_string(),
        6 => "06_normal".to_string(),
        7 => "07_unknown".to_string(),
        8 => "08_unknown".to_string(),
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
--size-map           Write a rough zone-shape image from manifest file sizes (needs <world>; no export)\n  \
--size-type <n>     File type (low 5 bits of Unk1) to use for --size-map (default: 0x15, cell placement files)\n  \
--size-scale <n>    Pixels per cell in the --size-map image (default: 16)\n  \
--placement-only     Only export/parse the placement (instance) file for this world; skip terrain/colourmap entirely\n  \
--list-worlds        List every world found in the manifest with terrain cell data, and its grid size (no export/world arg needed)\n \
--world-lut          Export the world table and write export/worlds_lut.csv\n \
--models-lut         Export the models table and write export/models_lut.csv\n \
--creatures-lut      Export the creatures table, creature models and creature textures table and write export/creatures_lut.csv\n \
--
",
        MNF_EXPORT, RAW_MNF, DEFAULT_EXE, DEFAULT_OUT
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
    let mut list_worlds = false;
    let mut world_lut = false;
    let mut model_list = false;
    let mut creature_list = false;
    let mut size_map = false;
    let mut size_type: Option<u32> = None;
    let mut size_scale: u32 = 16;

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
            "--placement-only" => placement_only = true,
            "--list-worlds" => list_worlds = true,
            "--world-lut" => world_lut = true,
            "--model-lut" => model_list = true,
            "--creature-lut" => creature_list = true,
            "--size-map" => size_map = true,


            "--size-type" => {
                i += 1;
                size_type = Some(parse_int_arg(raw.get(i).expect("--size-type needs a value")));
            }
            "--size-scale" => {
                i += 1;
                size_scale = parse_int_arg(raw.get(i).expect("--size-scale needs a value"));
            }


            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

    let world = if list_worlds || world_lut || model_list || creature_list {
        // No world id needed for this mode; ignore any stray positional args.
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
        list_worlds,
        world_lut,
        model_list,
        creature_list,
        size_map,
        size_type,
        size_scale,
    }
}

const ANSI_RED: &str = "\x1b[31m";
const ANSI_RESET: &str = "\x1b[0m";

fn main() {
    let args = parse_args();

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

        let lut = match worldlut::load_lut_csv(Path::new("export/worlds_lut.csv")) {
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
        let wanted: HashSet<u32> = pm.iter().map(|p| p.index).chain(cell_pms.iter().map(|c| c.index)).collect();

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

        let output_dir = Path::new(FINAL_EXPORT_DIR).join(format!("world_0x{:x}_placement", args.world));
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

        let wanted_name = format!("{}_Uncompressed", m.index);
        let found = WalkDir::new(&args.out)
            .into_iter()
            .filter_map(Result::ok)
            .find(|e| {
                e.path().is_file()
                    && e.path()
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .map_or(false, |s| s == wanted_name)
            });
        let Some(entry) = found else {
            eprintln!(
                "Uncompressed World LUT file '{}.*' not found under '{}'.",
                wanted_name, args.out
            );
            std::process::exit(1);
        };
        println!("Parsing file: {}", entry.path().display());
    
        std::fs::create_dir_all(FINAL_EXPORT_DIR).expect("failed to create export dir");
        let out_path = Path::new(FINAL_EXPORT_DIR).join("worlds_lut.csv");
        let lang_path = WalkDir::new(&args.out)
            .into_iter()
            .filter_map(Result::ok)
            .map(|e| e.into_path())
            .find(|p| p.is_file() && p.file_name().and_then(|n| n.to_str()) == Some("en.lang.csv"));

        let zone_names = match &lang_path {
            Some(p) => match worldlut::load_zone_names(p) {
                Ok(n) => { println!("Loaded {} zone names from {}", n.len(), p.display()); n }
                Err(e) => { eprintln!("Failed to read '{}': {}", p.display(), e); Default::default() }
            },
            None => {
                eprintln!("Warning: en.lang.csv not found under '{}'; zone_name will be empty.", args.out);
                Default::default()
            }
        };

        match worldlut::parse_and_write(entry.path(), &zone_names, &out_path) {
            Ok(n) => println!("Wrote {} worlds -> {}", n, out_path.display()),
            Err(e) => { eprintln!("Error parsing '{}': {}", entry.path().display(), e); std::process::exit(1); }
        }

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

        let wanted_name = format!("{}_Uncompressed", m.index);
        let found = WalkDir::new(&args.out).into_iter().filter_map(Result::ok).find(|e| {
            e.path().is_file()
                && e.path().file_stem().and_then(|s| s.to_str()).map_or(false, |s| s == wanted_name)
        });
        let Some(entry) = found else {
            eprintln!("Model list file for index {} not found under '{}'.", wanted_name, args.out);
            std::process::exit(1);
        };

        let names = WalkDir::new(&args.out).into_iter().filter_map(Result::ok)
            .map(|e| e.into_path())
            .find(|p| p.is_file() && p.file_name().and_then(|n| n.to_str()) == Some("en.lang.csv"))
            .and_then(|p| worldlut::load_lang_group(&p, model::MODEL_NAME_GROUP).ok())
            .unwrap_or_default();

        std::fs::create_dir_all(FINAL_EXPORT_DIR).expect("failed to create export dir");
        let out_path = Path::new(FINAL_EXPORT_DIR).join("models_lut.csv");
        match model::parse_and_write(entry.path(), &names, &out_path) {
            Ok(n) => println!("Wrote {} models -> {}", n, out_path.display()),
            Err(e) => { eprintln!("Error parsing '{}': {}", entry.path().display(), e); std::process::exit(1); }
        }
        if !args.no_cleanup { let _ = std::fs::remove_dir_all(&args.out); }
        return;
    }

    fn find_uncompressed(dir: &str, index: u32) -> Option<std::path::PathBuf> {
        let want = format!("{}_Uncompressed", index);
        WalkDir::new(dir).into_iter().filter_map(Result::ok).map(|e| e.into_path())
            .find(|p| p.is_file() && p.file_stem().and_then(|s| s.to_str()) == Some(want.as_str()))
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

        let load = |m: &manifest::TableMatch, label: &str| -> creature::Table {
            let Some(p) = find_uncompressed(&args.out, m.index) else {
                eprintln!("{} file '{}_Uncompressed' not found under '{}'.", label, m.index, args.out);
                std::process::exit(1);
            };
            creature::Table::load(&p).unwrap_or_else(|e| { eprintln!("{}", e); std::process::exit(1); })
        };
        let (textures, links, models, creature_tbl) = (
            load(&found[0], "Texture"), load(&found[1], "Link"),
            load(&found[2], "Model"), load(&found[3], "Creature"),
        );

        let names = WalkDir::new(&args.out).into_iter().filter_map(Result::ok).map(|e| e.into_path())
            .find(|p| p.is_file() && p.file_name().and_then(|n| n.to_str()) == Some("en.lang.csv"))
            .and_then(|p| worldlut::load_lang_group(&p, creature::CREATURE_NAME_GROUP).ok())
            .unwrap_or_default();
        println!("Loaded {} creature names.", names.len());

        std::fs::create_dir_all(FINAL_EXPORT_DIR).expect("failed to create export dir");
        let out_path = Path::new(FINAL_EXPORT_DIR).join("creatures_lut.csv");
        match creature::build_csv(&textures, &links, &models, &creature_tbl, &names, &out_path) {
            Ok(n) => println!("Wrote {} creatures -> {}", n, out_path.display()),
            Err(e) => { eprintln!("Error writing '{}': {}", out_path.display(), e); std::process::exit(1); }
        }
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
        "world_0x{:x}_x{}-{}_z{}-{}_channels",
        args.world, min_x, max_x, min_z, max_z
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