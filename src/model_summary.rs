//! Summarise model usage across every per-world model LUT (luts/worlds/world_0x<id>.csv).
//!
//! Writes two files into `out_dir`:
//!   model_usage_summary.csv    one row per model id: total placements, number of worlds,
//!                              per-world counts, and the manifest row it maps to
//!   model_manifest_entries.csv the raw manifest lines for those models (same format as the
//!                              manifest, header included) - hand this to someone/something
//!                              that wants to study where models live in the archives
//!
//! A model id maps to a manifest row where `Unk1 + FileIndex == model_id`. That sum is not
//! unique over the whole manifest, so every matching row is kept; `manifest_matches` in the
//! summary tells you when it was ambiguous (>1) or missing (0).
use std::collections::{BTreeMap, HashMap};
use std::io::{self, BufWriter, Write};
use std::path::Path;

use crate::archive::{Archive, MANIFEST_HEADER};
use crate::manifest::parse_int;

struct Usage {
    total: u64,
    per_world: BTreeMap<u32, u64>,
}

struct ManifestRow {
    line: String,
    index: i64,
    arch: i64,
    size: i64,
    file_index: i64,
    unk1: i64,
}

fn world_id_from_name(name: &str) -> Option<u32> {
    let stem = name.strip_suffix(".csv")?;
    let h = stem.strip_prefix("world_0x")?;
    u32::from_str_radix(h, 16).ok()
}

/// model_id -> usage, plus the number of world files read.
fn scan_world_luts(dir: &Path) -> io::Result<(HashMap<u32, Usage>, usize)> {
    let mut files: Vec<(u32, std::path::PathBuf)> = Vec::new();
    for e in std::fs::read_dir(dir)? {
        let p = e?.path();
        if let Some(w) = p.file_name().and_then(|n| n.to_str()).and_then(world_id_from_name) {
            files.push((w, p));
        }
    }
    files.sort();

    let mut usage: HashMap<u32, Usage> = HashMap::new();
    for (world, path) in &files {
        let content = std::fs::read_to_string(path)?;
        for line in content.lines() {
            // first column is model_id; the header (and any junk line) fails to parse and is skipped
            let first = line.split(',').next().unwrap_or("");
            let id = match parse_int(first) {
                Some(v) if v >= 0 && v <= u32::MAX as i64 => v as u32,
                _ => continue,
            };
            let u = usage.entry(id).or_insert_with(|| Usage { total: 0, per_world: BTreeMap::new() });
            u.total += 1;
            *u.per_world.entry(*world).or_insert(0) += 1;
        }
    }
    Ok((usage, files.len()))
}

/// One pass over the entries, keeping only rows whose Unk1 + FileIndex is a wanted model id.
fn collect_manifest_rows(
    archive: &Archive,
    wanted: &HashMap<u32, Usage>,
) -> (String, HashMap<u32, Vec<ManifestRow>>) {
    let mut rows: HashMap<u32, Vec<ManifestRow>> = HashMap::new();

    for e in archive.entries() {
        let key = e.unknown1 as i64 + e.file_index as i64;
        if key < 0 || key > u32::MAX as i64 {
            continue;
        }
        let key = key as u32;
        if !wanted.contains_key(&key) {
            continue;
        }
        rows.entry(key).or_default().push(ManifestRow {
            line: archive.manifest_line(e),
            index: e.index as i64,
            arch: e.archive_index as i64,
            size: e.size as i64,
            file_index: e.file_index as i64,
            unk1: e.unknown1 as i64,
        });
    }
    (MANIFEST_HEADER.to_string(), rows)
}

pub fn run(worlds_dir: &Path, archive: &Archive, out_dir: &Path) -> Result<(), String> {
    let (usage, n_files) = scan_world_luts(worlds_dir)
        .map_err(|e| format!("failed to read world LUTs in '{}': {}", worlds_dir.display(), e))?;
    if n_files == 0 {
        return Err(format!(
            "no world_0x<id>.csv files found in '{}' (run --world-model-lut first)",
            worlds_dir.display()
        ));
    }
    println!("Read {} world LUT file(s): {} distinct model ids.", n_files, usage.len());

    let (header, rows) = collect_manifest_rows(archive, &usage);

    std::fs::create_dir_all(out_dir).map_err(|e| format!("failed to create '{}': {}", out_dir.display(), e))?;
    let summary_path = out_dir.join("model_usage_summary.csv");
    let entries_path = out_dir.join("model_manifest_entries.csv");

    let mut ids: Vec<u32> = usage.keys().copied().collect();
    ids.sort_unstable();

    let werr = |p: &Path, e: io::Error| format!("failed to write '{}': {}", p.display(), e);
    let mut sum = BufWriter::new(std::fs::File::create(&summary_path).map_err(|e| werr(&summary_path, e))?);
    let mut ent = BufWriter::new(std::fs::File::create(&entries_path).map_err(|e| werr(&entries_path, e))?);

    writeln!(
        sum,
        "model_id,model_id_hex,total_count,world_count,worlds,manifest_matches,manifest_index,arch,size,file_index,unk1"
    )
    .map_err(|e| werr(&summary_path, e))?;
    writeln!(ent, "{}", header).map_err(|e| werr(&entries_path, e))?;

    let (mut missing, mut ambiguous) = (0usize, 0usize);
    let mut missing_ids: Vec<u32> = Vec::new();
    for id in &ids {
        let u = &usage[id];
        // "0x02b:123;0x0eb:4" - world id (hex) and how many times the model appears in it
        let worlds = u
            .per_world
            .iter()
            .map(|(w, c)| format!("0x{:03x}:{}", w, c))
            .collect::<Vec<_>>()
            .join(";");
        let matches = rows.get(id).map(|v| v.as_slice()).unwrap_or(&[]);
        match matches.len() {
            0 => {
                missing += 1;
                missing_ids.push(*id);
            }
            1 => {}
            _ => ambiguous += 1,
        }
        let (mi, ar, sz, fi, u1) = match matches.first() {
            Some(m) => (m.index.to_string(), m.arch.to_string(), m.size.to_string(), m.file_index.to_string(), format!("0x{:08X}", m.unk1)),
            None => (String::new(), String::new(), String::new(), String::new(), String::new()),
        };
        writeln!(
            sum,
            "{},0x{:X},{},{},{},{},{},{},{},{},{}",
            id, id, u.total, u.per_world.len(), worlds, matches.len(), mi, ar, sz, fi, u1
        )
        .map_err(|e| werr(&summary_path, e))?;
        for m in matches {
            writeln!(ent, "{}", m.line).map_err(|e| werr(&entries_path, e))?;
        }
    }
    sum.flush().map_err(|e| werr(&summary_path, e))?;
    ent.flush().map_err(|e| werr(&entries_path, e))?;

    println!("Wrote {}", summary_path.display());
    println!("Wrote {}", entries_path.display());
    if missing > 0 {
        let shown: Vec<String> = missing_ids.iter().take(20).map(|i| i.to_string()).collect();
        println!("{} model id(s) have no manifest row (first {}: {}).", missing, shown.len(), shown.join(", "));
    }
    if ambiguous > 0 {
        println!("{} model id(s) match more than one manifest row (all kept in the entries file).", ambiguous);
    }
    Ok(())
}