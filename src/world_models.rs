//! Per-world model placement LUT: `luts/worlds/world_0x<id>.csv`.
//!
//! One CSV per world, one row per placed model instance:
//!
//!     model_id,x,y,z[,rx,ry,rz]
//!
//! * `model_id` is `model_type_id` (== MNF FileIndex), decimal.
//! * Source is the world's two per-world placement files: the primary one (manifest type 0x02) and the secondary one
//!   (type 0x01, same record layout, different models), merged into one CSV. The secondary file is optional: if its
//!   archive is not available the world is still written from the primary file alone. The per-cell placement files
//!   are not read here.
//! * Positions are rounded to 0.01, rotations to 0.0001 rad, and written without trailing zeros
//!   ("1234.5", not "1234.50").
//! * Rows are sorted (model, x, y, z, rot), so a patch that adds/removes one instance changes exactly one
//!   line in `git diff` regardless of the order the game files list them in.
//! * The instance id and every reserved/unknown field are deliberately NOT stored (storage cost).
//!
//! Only the 8-byte header and the 96-byte instance records are read; the R-trees
//! are never touched, and no intermediate JSON is built, so a full run over ~1200 worlds stays cheap.
//!
//! Each archive is exported once, every file we need from it is parsed, then the temp dir is wiped.
//! A world whose placement file fails to parse is not written (and its old CSV is left alone), so a bad
//! export can't show up as a flood of fake "removed" lines in the next diff.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::io;
use std::path::{Path, PathBuf};
use walkdir::WalkDir;

use crate::{export, manifest};

// DISABLED: esoextractdata has a bug that breaks single-file (-f) extraction of placement files, so every run
// currently exports whole archives. To re-enable: restore `export::export_file` (same as `export_archive` but
// with `.arg("-f").arg(file_index)`), uncomment SMALL_BATCH and the `small` block in `run`.
// /// Below this many worlds, extract just the placement files instead of whole archives (much faster for one zone).
// const SMALL_BATCH: usize = 10;

const INSTANCE_SIZE: usize = 96;
const OFF_ROT: usize = 16;
const OFF_POS: usize = 28;
const OFF_MODEL: usize = 68;

const POS_SCALE: i64 = 100; // 2 decimals
const ROT_SCALE: i64 = 10_000; // 4 decimals

/// Quantised to integers so sorting/dedup is exact and formatting is deterministic.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Row {
    model: u32,
    pos: [i32; 3],
    rot: [i32; 3],
}

fn f32_at(b: &[u8], o: usize) -> f32 {
    f32::from_le_bytes(b[o..o + 4].try_into().unwrap())
}

fn quant(v: f64, scale: i64) -> i32 {
    // `as` saturates and maps NaN to 0.
    (v * scale as f64).round() as i32
}

/// World `placement` file: u32 tag, u32 count, then count * 96-byte instance records.
fn parse_instances(path: &Path) -> io::Result<Vec<Row>> {
    let bad = |m: String| io::Error::new(io::ErrorKind::InvalidData, m);
    let b = std::fs::read(path)?;
    if b.len() < 8 {
        return Err(bad(format!("{}: file too short", path.display())));
    }
    let count = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
    let fits = count
        .checked_mul(INSTANCE_SIZE)
        .and_then(|n| n.checked_add(8))
        .map_or(false, |n| n <= b.len());
    if !fits {
        return Err(bad(format!("{}: instance_count {} does not fit in file", path.display(), count)));
    }
    let mut rows = Vec::with_capacity(count);
    for i in 0..count {
        let s = 8 + i * INSTANCE_SIZE;
        let r = &b[s..s + INSTANCE_SIZE];
        rows.push(Row {
            model: u32::from_le_bytes(r[OFF_MODEL..OFF_MODEL + 4].try_into().unwrap()),
            pos: [
                quant(f32_at(r, OFF_POS) as f64, POS_SCALE),
                quant(f32_at(r, OFF_POS + 4) as f64, POS_SCALE),
                quant(f32_at(r, OFF_POS + 8) as f64, POS_SCALE),
            ],
            rot: [
                quant(f32_at(r, OFF_ROT) as f64, ROT_SCALE),
                quant(f32_at(r, OFF_ROT + 4) as f64, ROT_SCALE),
                quant(f32_at(r, OFF_ROT + 8) as f64, ROT_SCALE),
            ],
        });
    }
    Ok(rows)
}

/// Fixed-point integer -> shortest decimal text ("-12.5", "3", "0.07").
fn push_fixed(s: &mut String, v: i32, scale: i64) {
    let v = v as i64;
    if v < 0 {
        s.push('-');
    }
    let a = v.abs();
    let _ = write!(s, "{}", a / scale);
    let frac = a % scale;
    if frac != 0 {
        let width = if scale == POS_SCALE { 2 } else { 4 };
        let f = format!("{:0w$}", frac, w = width);
        s.push('.');
        s.push_str(f.trim_end_matches('0'));
    }
}

fn write_csv(path: &Path, rows: &mut Vec<Row>, with_rot: bool) -> io::Result<()> {
    rows.sort_unstable();
    let mut s = String::with_capacity(rows.len() * 36 + 64);
    s.push_str(if with_rot { "model_id,x,y,z,rx,ry,rz\n" } else { "model_id,x,y,z\n" });
    for r in rows.iter() {
        let _ = write!(s, "{}", r.model);
        for p in r.pos {
            s.push(',');
            push_fixed(&mut s, p, POS_SCALE);
        }
        if with_rot {
            for p in r.rot {
                s.push(',');
                push_fixed(&mut s, p, ROT_SCALE);
            }
        }
        s.push('\n'); // always LF so git doesn't see line-ending churn
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(path, s)
}

struct Job {
    world: u32,
    index: u32,
}

struct WorldState {
    remaining: usize,
    rows: Vec<Row>,
    failed: bool,
}

#[derive(Default)]
struct Tally {
    written: usize,
    rows: usize,
    failed: Vec<u32>,
}

pub struct RunArgs<'a> {
    pub manifest: &'a str,
    pub exe: &'a str,
    pub mnf: &'a str,
    pub temp_dir: &'a str,
    /// Usually `luts/worlds`.
    pub out_dir: &'a Path,
    /// Empty = every world in the manifest.
    pub only_worlds: &'a [u32],
    pub with_rot: bool,
    pub dry_run: bool,
    pub skip_export: bool,
    pub keep_temp: bool,
}

fn index_paths(dir: &str, wanted: &HashSet<u32>) -> HashMap<u32, PathBuf> {
    let mut m = HashMap::new();
    for e in WalkDir::new(dir).into_iter().filter_map(Result::ok) {
        let p = e.path();
        if !p.is_file() {
            continue;
        }
        if let Some(i) = p.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u32>().ok()) {
            if wanted.contains(&i) {
                m.entry(i).or_insert_with(|| p.to_path_buf());
            }
        }
    }
    m
}

fn process(
    jobs: &[Job],
    paths: &HashMap<u32, PathBuf>,
    states: &mut HashMap<u32, WorldState>,
    a: &RunArgs,
    t: &mut Tally,
) {
    for j in jobs {
        let Some(st) = states.get_mut(&j.world) else { continue };
        match paths.get(&j.index) {
            Some(p) => match parse_instances(p) {
                Ok(mut r) => st.rows.append(&mut r),
                Err(e) => {
                    eprintln!("  world 0x{:x} (index {}): {}", j.world, j.index, e);
                    st.failed = true;
                }
            },
            None => {
                eprintln!("  world 0x{:x} (index {}): file not found after export", j.world, j.index);
                st.failed = true;
            }
        }
        st.remaining -= 1;
        if st.remaining > 0 {
            continue;
        }

        let mut st = states.remove(&j.world).unwrap();
        if st.failed {
            t.failed.push(j.world);
            continue;
        }
        let out = a.out_dir.join(format!("world_0x{:03x}.csv", j.world));
        match write_csv(&out, &mut st.rows, a.with_rot) {
            Ok(()) => {
                t.written += 1;
                t.rows += st.rows.len();
            }
            Err(e) => {
                eprintln!("  failed to write {}: {}", out.display(), e);
                t.failed.push(j.world);
            }
        }
    }
}

pub fn run(a: &RunArgs) -> Result<(), String> {
    let summaries = manifest::list_worlds_with_cells(a.manifest)
        .map_err(|e| format!("Failed to read manifest '{}': {}", a.manifest, e))?;
    let mut worlds: Vec<u32> = summaries.iter().map(|s| s.world).collect();
    worlds.sort_unstable();
    worlds.dedup();
    if !a.only_worlds.is_empty() {
        worlds.retain(|w| a.only_worlds.contains(w));
    }
    if worlds.is_empty() {
        return Err("No matching worlds found in manifest.".to_string());
    }
    println!("Looking up placement files for {} world(s)...", worlds.len());

    let available = |arch: u32| (arch as u64) < (manifest::UNAVAILABLE_ARCHIVE_THRESHOLD as u64);

    // One pass over the manifest for both placement types (instead of re-reading it once per world).
    let placements = manifest::filter_placement_manifest_all(
        a.manifest,
        &[manifest::PLACEMENT_TYPE, manifest::PLACEMENT_TYPE_SECONDARY],
    )
    .map_err(|e| format!("Failed to read manifest '{}': {}", a.manifest, e))?;

    let mut by_arch: BTreeMap<u32, Vec<Job>> = BTreeMap::new();
    let mut states: HashMap<u32, WorldState> = HashMap::new();
    let (mut no_placement, mut unavailable) = (0usize, 0usize);
    let (mut with_secondary, mut secondary_unavailable) = (0usize, 0usize);

    for &w in &worlds {
        let Some(p) = placements.get(&(w, manifest::PLACEMENT_TYPE)) else {
            no_placement += 1;
            continue;
        };
        if !available(p.arch) {
            unavailable += 1;
            continue;
        }
        let mut remaining = 1;
        by_arch.entry(p.arch).or_default().push(Job { world: w, index: p.index });
        match placements.get(&(w, manifest::PLACEMENT_TYPE_SECONDARY)) {
            Some(s) if available(s.arch) => {
                remaining += 1;
                with_secondary += 1;
                by_arch.entry(s.arch).or_default().push(Job { world: w, index: s.index });
            }
            Some(_) => secondary_unavailable += 1,
            None => {}
        }
        states.insert(w, WorldState { remaining, rows: Vec::new(), failed: false });
    }
    println!(
        "{} world(s) to process across {} archive(s); {} without placement files, {} skipped (archive not available).",
        states.len(), by_arch.len(), no_placement, unavailable
    );
    println!(
        "{} world(s) also have a secondary placement file; {} more have one in an unavailable archive (written from the primary file only).",
        with_secondary, secondary_unavailable
    );

    let mut tally = Tally::default();

    if a.skip_export {
        println!("\nSkipping export step (--skip-export).");
        let all: Vec<Job> = by_arch.into_values().flatten().collect();
        let wanted: HashSet<u32> = all.iter().map(|j| j.index).collect();
        let paths = index_paths(a.temp_dir, &wanted);
        process(&all, &paths, &mut states, a, &mut tally);
    } else {
        let total = by_arch.len();
        // let small = states.len() < SMALL_BATCH;
        // if small && !states.is_empty() {
        //     println!("{} world(s): exporting individual files instead of whole archives.", states.len());
        // }
        for (k, (arch, jobs)) in by_arch.iter().enumerate() {
            println!("\n[{}/{}] Archive {}: {} placement file(s)", k + 1, total, arch, jobs.len());
            // if small {
            //     for j in jobs {
            //         if !export::export_file(a.exe, a.mnf, a.temp_dir, *arch, j.index, a.dry_run) {
            //             eprintln!("  File export reported an error for index {}; that world will be skipped.", j.index);
            //         }
            //     }
            // } else
            if !export::export_archive(a.exe, a.mnf, a.temp_dir, *arch, a.dry_run) {
                eprintln!("  Archive export reported an error; affected worlds will be skipped.");
            }
            if a.dry_run {
                continue;
            }
            let wanted: HashSet<u32> = jobs.iter().map(|j| j.index).collect();
            let paths = index_paths(a.temp_dir, &wanted);
            process(jobs, &paths, &mut states, a, &mut tally);
            if !a.keep_temp {
                if let Err(e) = std::fs::remove_dir_all(a.temp_dir) {
                    eprintln!("  Warning: failed to clear '{}': {}", a.temp_dir, e);
                }
            }
        }
    }

    if a.dry_run {
        println!("\nDry run complete; nothing was parsed or written.");
        return Ok(());
    }

    println!("\nWrote {} world file(s), {} instance row(s) -> {}", tally.written, tally.rows, a.out_dir.display());
    if !tally.failed.is_empty() {
        tally.failed.sort_unstable();
        let list: Vec<String> = tally.failed.iter().map(|w| format!("0x{:x}", w)).collect();
        eprintln!("{} world(s) NOT written (incomplete data, existing CSV left untouched): {}", tally.failed.len(), list.join(", "));
    }
    if !states.is_empty() {
        eprintln!("{} world(s) never completed (export failed for their archive); existing CSVs left untouched.", states.len());
    }
    if !a.keep_temp && a.skip_export == false {
        let _ = std::fs::remove_dir_all(a.temp_dir);
    }
    if tally.failed.is_empty() && states.is_empty() { Ok(()) } else { Err("Some worlds were not written.".to_string()) }
}