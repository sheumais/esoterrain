//! Terrain texture table (manifest file id 0x29).
//!
//! Layout of `<index>_Uncompressed.EsoFileData` (big-endian unless noted):
//!
//! ```text
//! file header (16)   FA FA EB EB | u32 version(23) | u32 entry_count | u32 unknown(8)
//! entry (32 + size)  "####" | u32 row | u32 size | u32 size                 (16, separator)
//!                    u32 record_len | u32 id | u32 record_offset | u32 size (16, sub-header)
//!                    payload (size = 54):
//!                      +0  u32 id            (same as sub-header id)
//!                      +14 u8  flag          (always 1)
//!                      +21 u32 unix timestamp (0 = unset)
//!                      +25 f32 LE scale      (texture tiling size, 0.5 .. 6.0)
//!                      +29 u32 hash/group    (0 or one of ~25 shared values)
//!                      +33 u32 texture file index 1   (mnf index)
//!                      +37 u32 texture file index 2   (mnf index, often tex1 + 2)
//!                      +41 f32 LE param1     (mostly 0.5)
//!                      +45 f32 LE param2     (mostly 0.0)
//!                      +49 u8  flag2
//! ```
//!
//! `record_offset` follows `off[n+1] = off[n] + record_len[n] + 12` (starting at 16): it indexes a
//! companion record stream and is not needed for the texture list.
//! `id` can skip values (row 1374.. have id = row + 2), so `id` and `row` are kept separately.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

use walkdir::WalkDir;

/// File id (type) of the terrain texture table in the manifest.
pub const TERRAIN_TEXTURE_FILE_INDEX: u32 = 0x29;

const MAGIC: [u8; 4] = [0xFA, 0xFA, 0xEB, 0xEB];
const FILE_HEADER_LEN: usize = 16;
const ENTRY_PREFIX_LEN: usize = 32; // separator (16) + sub-header (16)
const MIN_PAYLOAD_LEN: usize = 54;

#[derive(Debug, Clone)]
pub struct TerrainTexture {
    /// 1-based sequence number of the entry in the file.
    pub row: u32,
    /// Terrain texture id (usually == row; skips a few values near the end).
    pub id: u32,
    pub record_len: u32,
    pub record_offset: u32,
    /// Unix timestamp, 0 when unset.
    pub timestamp: u32,
    /// Tiling scale (float32 LE).
    pub scale: f32,
    /// Shared hash/group value, 0 when unset.
    pub hash: u32,
    /// First texture FileIndex (manifest column 2), 0 = none.
    pub tex1: u32,
    /// Second texture FileIndex (manifest column 2), 0 = none.
    pub tex2: u32,
    pub param1: f32,
    pub param2: f32,
    pub flag: u8,
    pub flag2: u8,
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn le_f32(b: &[u8], o: usize) -> f32 {
    f32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

pub fn parse(path: &Path) -> Result<Vec<TerrainTexture>, String> {
    let d = std::fs::read(path).map_err(|e| format!("failed to read '{}': {}", path.display(), e))?;
    if d.len() < FILE_HEADER_LEN || d[0..4] != MAGIC {
        return Err(format!("'{}' is not an EsoFileData table (bad magic).", path.display()));
    }
    let count = be32(&d, 8) as usize;
    let mut out = Vec::with_capacity(count);
    let mut pos = FILE_HEADER_LEN;

    for n in 0..count {
        if pos + ENTRY_PREFIX_LEN > d.len() {
            return Err(format!("entry {} at offset {} runs past the end of the file.", n + 1, pos));
        }
        if &d[pos..pos + 4] != b"####" {
            return Err(format!("entry {}: expected '####' at offset {}.", n + 1, pos));
        }
        let size = be32(&d, pos + 8) as usize;
        if size != be32(&d, pos + 12) as usize {
            return Err(format!("entry {}: the two size fields differ.", n + 1));
        }
        if size < MIN_PAYLOAD_LEN {
            return Err(format!("entry {}: payload size {} is smaller than the expected {}.", n + 1, size, MIN_PAYLOAD_LEN));
        }
        let end = pos + ENTRY_PREFIX_LEN + size;
        if end > d.len() {
            return Err(format!("entry {}: payload runs past the end of the file.", n + 1));
        }
        let e = &d[pos..end];
        let p = ENTRY_PREFIX_LEN; // payload start within the entry
        out.push(TerrainTexture {
            row: be32(e, 4),
            record_len: be32(e, 16),
            record_offset: be32(e, 24),
            id: be32(e, p),
            flag: e[p + 14],
            timestamp: be32(e, p + 21),
            scale: le_f32(e, p + 25),
            hash: be32(e, p + 29),
            tex1: be32(e, p + 33),
            tex2: be32(e, p + 37),
            param1: le_f32(e, p + 41),
            param2: le_f32(e, p + 45),
            flag2: e[p + 49],
        });
        pos = end;
    }
    if pos != d.len() {
        eprintln!("Warning: {} trailing byte(s) after the last terrain texture entry.", d.len() - pos);
    }
    Ok(out)
}

/// Every distinct non-zero texture FileIndex referenced by the table.
pub fn texture_indices(entries: &[TerrainTexture]) -> BTreeSet<u32> {
    entries
        .iter()
        .flat_map(|t| [t.tex1, t.tex2])
        .filter(|&i| i != 0)
        .collect()
}

fn unix_to_iso(ts: u32) -> String {
    if ts == 0 {
        return String::new();
    }
    let days = (ts / 86_400) as i64;
    let secs = ts % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let mut y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    if m <= 2 {
        y += 1;
    }
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, secs / 3600, (secs % 3600) / 60, secs % 60)
}

/// Write the LUT. `files` maps a texture FileIndex to the exported file name (may be empty).
pub fn write_lut(entries: &[TerrainTexture], files: &HashMap<u32, String>, out_path: &Path) -> Result<usize, String> {
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("failed to create '{}': {}", parent.display(), e))?;
    }
    let mut f = File::create(out_path).map_err(|e| format!("failed to create '{}': {}", out_path.display(), e))?;
    let io = |e: std::io::Error| format!("failed to write '{}': {}", out_path.display(), e);

    writeln!(
        f,
        "row,id,tex1,tex1_file,tex2,tex2_file,scale,param1,param2,timestamp,timestamp_utc,hash,flag,record_len,record_offset"
    )
    .map_err(io)?;
    let name = |i: u32| if i == 0 { String::new() } else { files.get(&i).cloned().unwrap_or_default() };
    for t in entries {
        writeln!(
            f,
            "{},{},{},{},{},{},{},{},{},{},{},{},{},{},{}",
            t.row,
            t.id,
            t.tex1,
            name(t.tex1),
            t.tex2,
            name(t.tex2),
            t.scale,
            t.param1,
            t.param2,
            t.timestamp,
            unix_to_iso(t.timestamp),
            if t.hash == 0 { String::new() } else { format!("0x{:08x}", t.hash) },
            t.flag2, // the varying flag; the constant one at payload+14 is not written
            t.record_len,
            t.record_offset
        )
        .map_err(io)?;
    }
    Ok(entries.len())
}

// ---------------------------------------------------------------------------------------------
// Collecting exported texture files
//
// Terminology (as in model_export): the table stores texture *FileIndex* values (manifest column 2).
// The exporter names the files after the manifest *Index* (column 0), e.g. `608841.dds`.
// ---------------------------------------------------------------------------------------------

fn index_of(path: &Path) -> Option<u32> {
    let stem = path.file_stem()?.to_str()?;
    let digits: String = stem.chars().take_while(|c| c.is_ascii_digit()).collect();
    if digits.is_empty() {
        return None;
    }
    digits.parse::<u32>().ok()
}

fn is_dds(p: &Path) -> bool {
    p.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("dds")).unwrap_or(false)
}

/// Find the exported file for each wanted manifest Index under `dir` (prefers `.dds`).
pub fn scan_files(dir: &Path, wanted: &BTreeSet<u32>) -> HashMap<u32, PathBuf> {
    let mut found: HashMap<u32, PathBuf> = HashMap::new();
    for entry in WalkDir::new(dir).into_iter().filter_map(Result::ok) {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let Some(idx) = index_of(p) else { continue };
        if !wanted.contains(&idx) {
            continue;
        }
        match found.get(&idx) {
            Some(old) if is_dds(old) || !is_dds(p) => {}
            _ => {
                found.insert(idx, p.to_path_buf());
            }
        }
    }
    found
}

/// Copy the wanted manifest Indexes found under `scan_dir` into `dest`. Returns how many were copied.
pub fn copy_found(scan_dir: &Path, wanted: &BTreeSet<u32>, dest: &Path) -> Result<usize, String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("failed to create '{}': {}", dest.display(), e))?;
    let mut copied = 0usize;
    for (idx, src) in scan_files(scan_dir, wanted) {
        let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("dds");
        let to = dest.join(format!("{}.{}", idx, ext));
        std::fs::copy(&src, &to).map_err(|e| format!("failed to copy '{}' -> '{}': {}", src.display(), to.display(), e))?;
        copied += 1;
    }
    Ok(copied)
}

/// FileIndex -> file name for the textures already present in `dest`.
/// `fi_to_index` maps each texture FileIndex to its manifest Index.
pub fn exported_names(dest: &Path, fi_to_index: &BTreeMap<u32, u32>) -> HashMap<u32, String> {
    let indexes: BTreeSet<u32> = fi_to_index.values().copied().collect();
    let by_index = scan_files(dest, &indexes);
    fi_to_index
        .iter()
        .filter_map(|(fi, idx)| {
            by_index
                .get(idx)
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(|n| (*fi, n.to_string()))
        })
        .collect()
}