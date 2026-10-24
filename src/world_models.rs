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
//! Every placement file needed is read once, straight from the archives (archive/offset order), and parsed as it arrives.
//! A world whose placement file fails to parse is not written (and its old CSV is left alone), so a bad
//! export can't show up as a flood of fake "removed" lines in the next diff.

use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::io;
use std::path::Path;

use crate::archive::Archive;
use crate::manifest;

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
fn parse_instances(b: &[u8], label: &str) -> io::Result<Vec<Row>> {
    let bad = |m: String| io::Error::new(io::ErrorKind::InvalidData, m);
    if b.len() < 8 {
        return Err(bad(format!("{}: file too short", label)));
    }
    let count = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
    let fits = count
        .checked_mul(INSTANCE_SIZE)
        .and_then(|n| n.checked_add(8))
        .map_or(false, |n| n <= b.len());
    if !fits {
        return Err(bad(format!("{}: instance_count {} does not fit in file", label, count)));
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
    pub archive: &'a Archive,
    /// Usually `luts/worlds`.
    pub out_dir: &'a Path,
    /// Empty = every world in the manifest.
    pub only_worlds: &'a [u32],
    pub with_rot: bool,
    pub dry_run: bool,
}

fn process(
    j: &Job,
    data: Result<Vec<u8>, String>,
    states: &mut HashMap<u32, WorldState>,
    a: &RunArgs,
    t: &mut Tally,
) {
    {
        let Some(st) = states.get_mut(&j.world) else { return };
        match data {
            Ok(bytes) => match parse_instances(&bytes, &format!("index {}", j.index)) {
                Ok(mut r) => st.rows.append(&mut r),
                Err(e) => {
                    eprintln!("  world 0x{:x} (index {}): {}", j.world, j.index, e);
                    st.failed = true;
                }
            },
            Err(e) => {
                eprintln!("  world 0x{:x} (index {}): {}", j.world, j.index, e);
                st.failed = true;
            }
        }
        st.remaining -= 1;
        if st.remaining > 0 {
            return;
        }

        let mut st = states.remove(&j.world).unwrap();
        if st.failed {
            t.failed.push(j.world);
            return;
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
    let entries = a.archive.entries();
    let summaries = manifest::list_worlds_with_cells(entries);
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

    // One pass over the entries for both placement types.
    let placements = manifest::filter_placement_manifest_all(
        entries,
        &[manifest::PLACEMENT_TYPE, manifest::PLACEMENT_TYPE_SECONDARY],
    );

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

    let mut jobs_by_index: HashMap<u32, Vec<Job>> = HashMap::new();
    for (arch, jobs) in by_arch {
        println!("Archive {}: {} placement file(s)", arch, jobs.len());
        for j in jobs {
            jobs_by_index.entry(j.index).or_default().push(j);
        }
    }
    let mut wanted: Vec<u32> = jobs_by_index.keys().copied().collect();
    wanted.sort_unstable();

    if a.dry_run {
        println!("\nDry run complete; {} placement file(s) would be read, nothing was parsed or written.", wanted.len());
        return Ok(());
    }

    a.archive.read_each(&wanted, |e, data| {
        let Some(jobs) = jobs_by_index.get(&e.index) else { return };
        for j in jobs.iter() {
            let d = data.clone();
            process(j, d, &mut states, a, &mut tally);
        }
    });

    println!("\nWrote {} world file(s), {} instance row(s) -> {}", tally.written, tally.rows, a.out_dir.display());
    if !tally.failed.is_empty() {
        tally.failed.sort_unstable();
        let list: Vec<String> = tally.failed.iter().map(|w| format!("0x{:x}", w)).collect();
        eprintln!("{} world(s) NOT written (incomplete data, existing CSV left untouched): {}", tally.failed.len(), list.join(", "));
    }
    if !states.is_empty() {
        eprintln!("{} world(s) never completed (a placement file could not be read); existing CSVs left untouched.", states.len());
    }
    if tally.failed.is_empty() && states.is_empty() { Ok(()) } else { Err("Some worlds were not written.".to_string()) }
}
