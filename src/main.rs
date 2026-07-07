mod export;
mod landfile;
mod manifest;
mod reader;

use landfile::{BigImage, LandFile};
use manifest::filter_manifest;
use std::collections::{HashMap, HashSet};
use std::env;
use std::path::Path;
use walkdir::WalkDir;

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
    arch: Option<u32>,
    dry_run: bool,
    skip_export: bool,
    no_cleanup: bool,
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

fn print_usage() {
    eprintln!(
        "Usage: land_export <world> [options]\n\n\
Required:\n  \
<world>              World ID, hex (0xe9) or decimal\n\n\
Options:\n  \
--manifest <path>    Path to manifest export (default: {})\n  \
--mnf <path>         Path to raw .mnf (default: {})\n  \
--exe <path>         Path to esoextractdata.exe (default: {})\n  \
--out <dir>          Export/working directory (default: {})\n  \
--arch <n>           Force a single archive number instead of auto-detecting\n  \
--dry-run            Show what would happen without exporting/deleting\n  \
--skip-export        Skip calling the exporter; assume files are already on disk\n  \
--no-cleanup         Keep every file the archive export dumped\n\n\
For backward compatibility, `land_export <manifest> <world>` (two positionals)\n\
still works too -- the first positional overrides --manifest's default.\n",
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
    let mut arch = None;
    let mut dry_run = false;
    let mut skip_export = false;
    let mut no_cleanup = false;

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
            "--arch" => {
                i += 1;
                arch = Some(parse_int_arg(raw.get(i).expect("--arch needs a value")));
            }
            "--dry-run" => dry_run = true,
            "--skip-export" => skip_export = true,
            "--no-cleanup" => no_cleanup = true,
            "-h" | "--help" => {
                print_usage();
                std::process::exit(0);
            }
            other => positional.push(other.to_string()),
        }
        i += 1;
    }

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
    let world = parse_int_arg(world_str);

    Args {
        manifest,
        world,
        exe,
        mnf,
        out,
        arch,
        dry_run,
        skip_export,
        no_cleanup,
    }
}

fn main() {
    let args = parse_args();

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

    let cell_archs: HashSet<u32> = result.cell_matches.iter().map(|c| c.arch).collect();
    let toc_archs: HashSet<u32> = result.toc_matches.iter().map(|t| t.arch).collect();
    let mut all_archs: Vec<u32> = cell_archs.union(&toc_archs).cloned().collect();
    all_archs.sort_unstable();
    println!("Archive(s) involved: {:?}", all_archs);

    if !args.skip_export {
        let archs_to_export: Vec<u32> = match args.arch {
            Some(a) => vec![a],
            None => all_archs.clone(),
        };
        for arch in archs_to_export {
            println!("\nExporting archive {}...", arch);
            if !export::export_archive(&args.exe, &args.mnf, &args.out, arch, args.dry_run) {
                eprintln!("Archive export reported an error; check output above before continuing.");
            }
        }

        if !args.no_cleanup {
            println!(
                "\nCleaning up export directory (keeping {} relevant files)...",
                wanted_indices.len()
            );
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

    println!("\nBuilding heightmap image...");
    let mut big_image = BigImage::new(grid_w, grid_h);
    let mut loaded = 0usize;
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
                if let Some((tile, tw, th)) = file.heightmap_tile() {
                    let col = x - min_x;
                    let row = max_z - z;
                    big_image.set_tile_at(col, row, &tile, tw, th);
                    loaded += 1;
                }
            }
            Err(e) => {
                eprintln!("Error loading '{}': {}", path.display(), e);
                errors += 1;
            }
        }
    }

    if loaded == 0 {
        eprintln!(
            "No terrain cell files were found/loaded under '{}'. \
             Did the export step actually run (see --skip-export)?",
            args.out
        );
        std::process::exit(1);
    }

    let output_dir = Path::new(FINAL_EXPORT_DIR);
    if let Err(e) = std::fs::create_dir_all(output_dir) {
        eprintln!("Failed to create output directory '{}': {}", output_dir.display(), e);
        std::process::exit(1);
    }
    let output = output_dir.join(format!(
        "world_0x{:x}_x{}-{}_z{}-{}.png",
        args.world, min_x, max_x, min_z, max_z
    ));

    match big_image.save(&output) {
        Ok(()) => println!("Saved {}", output.display()),
        Err(e) => eprintln!("Error saving '{}': {}", output.display(), e),
    }

    match std::fs::remove_dir_all(&args.out) {
        Ok(()) => println!("Removed temporary export directory '{}'.", args.out),
        Err(e) => eprintln!(
            "Warning: failed to remove temporary directory '{}': {}",
            args.out, e
        ),
    }

    println!("\nFinished. Loaded {} cell(s), {} error(s).", loaded, errors);
}