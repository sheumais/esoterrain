mod export;
mod landfile;
mod manifest;
mod reader;
mod colourmap;

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
--channels <list>    Comma-separated channel indices to export (default: all found)\n  \
--dry-run            Show what would happen without exporting/deleting\n  \
--skip-export        Skip calling the exporter; assume files are already on disk\n  \
--no-cleanup         Keep every file the archive export dumped",
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
        dry_run,
        skip_export,
        no_cleanup,
        channels,
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

    let cell_archs: HashSet<u32> = result.cell_matches.iter().map(|c| c.arch).collect();
    let toc_archs: HashSet<u32> = result.toc_matches.iter().map(|t| t.arch).collect();
    let cm_archs: HashSet<u32> = cm_matches.iter().map(|c| c.arch).collect();
    let mut all_archs: Vec<u32> = cell_archs
        .union(&toc_archs)
        .cloned()
        .collect::<HashSet<u32>>()
        .union(&cm_archs)
        .cloned()
        .collect();
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

    for channel in &planes4_channels {
        let (i0, i1, i2, i3) = &planes4_images[channel];
        let label = channel_label(*channel);

        let mut finalized: HashMap<&str, landfile::BigImage> = HashMap::new();
        for (component, img) in [("layer0", i0), ("layer1", i1), ("layer2", i2), ("layer3", i3)] {
            if img.coverage_fraction() == 0.0 {
                continue;
            }
            let big_image = img.finalize_grayscale(0.0, 255.0);
            // let out_path = output_dir.join(format!("{}_{}.png", label, component));
            // if let Err(e) = big_image.save(&out_path) {
            //     eprintln!("Error saving '{}': {}", out_path.display(), e);
            // } else {
            //     println!("Saved channel {} {} -> {}", channel, component, out_path.display());
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
                    match save_rgba_from_channels(r, g, b, a, &rgba_path) {
                        Ok(()) => println!(
                            "Saved channel {} as combined RGBA -> {}",
                            channel,
                            rgba_path.display()
                        ),
                        Err(e) => eprintln!("Error saving '{}': {}", rgba_path.display(), e),
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
        if let Err(e) = normal_grid.save(&normal_path) {
            eprintln!("Error saving '{}': {}", normal_path.display(), e);
        } else {
            println!("Saved colour map -> {}", normal_path.display());
        }
        if let Err(e) = overlay_grid.save(&overlay_path) {
            eprintln!("Error saving '{}': {}", overlay_path.display(), e);
        } else {
            println!("Saved colour map overlay -> {}", overlay_path.display());
        }
    } else {
        println!("\nNo colour map tiles found for this world.");
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