//! `--diff-mnf`: which entries of the current MNF are brand new compared with an older MNF.
//!
//! Index, ID1, Arch and Offset change from patch to patch, so they are not used. An entry is NEW when no entry of
//! the old manifest has the same (Hash, Size). Entries that merely moved or were renumbered keep both, so they are
//! not reported. Unless asked otherwise only model/artwork entries (Unk1 == 0) are kept: per-world cell files
//! (terrain, colourmaps, LOD, cell placement), placement files and tables have a non-zero Unk1.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use eso_common::mnf::MnfFileEntry;

use crate::archive::{Archive, MANIFEST_HEADER};
use crate::manifest::parse_int;

/// Entries in this archive are never reported (and so never exported).
pub const SKIPPED_ARCHIVE: u8 = 0;

/// Unk1 of the entries that hold models, textures and other artwork (what the model/texture references resolve to).
const MODEL_ART_UNK1: u32 = 0;

/// (Hash, Size)
pub type Content = (u64, u64);

fn content(e: &MnfFileEntry) -> Content {
    (e.hash as u64, e.size as u64)
}

#[derive(Default)]
pub struct Diff {
    /// Absolute indices of the new entries, in index order.
    pub different: Vec<u32>,
    /// Entries whose (hash, size) exists in the old manifest.
    pub unchanged: usize,
    /// Entries ignored because they are in `SKIPPED_ARCHIVE`.
    pub skipped: usize,
    /// New by hash, but left out because Unk1 != 0: (Unk1 >> 16) -> count.
    pub excluded: BTreeMap<u32, usize>,
}

/// The (hash, size) of every entry of the old manifest: either a binary `.mnf`, or the `eso_mnf.csv` dump of one
/// (path ends in `.csv`).
pub fn load_old(path: &Path) -> Result<Vec<Content>, String> {
    let is_csv = path.extension().and_then(|x| x.to_str()).map(|x| x.eq_ignore_ascii_case("csv")).unwrap_or(false);
    if !is_csv {
        let entries = crate::archive::load_entries(path)?;
        return Ok(entries.iter().map(content).collect());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("failed to read '{}': {}", path.display(), e))?;
    parse_csv(&String::from_utf8_lossy(&bytes), path)
}

/// Lines of the old `eso_mnf.csv`: Index, ID1, FileIndex, Unk1, Size, ZSize, Hash, Offset, ZTyp, Arch, Unk2, Filename.
/// The first line is the header.
fn parse_csv(text: &str, path: &Path) -> Result<Vec<Content>, String> {
    let mut out = Vec::new();
    for (n, line) in text.trim_start_matches('\u{feff}').lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.splitn(12, ',').collect();
        let num = |i: usize| f.get(i).and_then(|s| parse_int(s));
        let (Some(size), Some(hash)) = (num(4), num(6)) else {
            if n == 0 {
                continue;
            }
            return Err(format!("'{}' line {}: not a manifest line: {}", path.display(), n + 1, line));
        };
        out.push((hash as u64, size as u64));
    }
    if out.is_empty() {
        return Err(format!("'{}' has no manifest lines", path.display()));
    }
    Ok(out)
}

pub fn compare(old: &[Content], new: &[MnfFileEntry], all_types: bool) -> Diff {
    let known: HashSet<Content> = old.iter().copied().collect();

    let mut diff = Diff::default();
    for e in new {
        if e.archive_index == SKIPPED_ARCHIVE {
            diff.skipped += 1;
        } else if known.contains(&content(e)) {
            diff.unchanged += 1;
        } else if !all_types && e.unknown1 != MODEL_ART_UNK1 {
            *diff.excluded.entry(e.unknown1 >> 16).or_insert(0) += 1;
        } else {
            diff.different.push(e.index);
        }
    }
    diff
}

/// One line per new entry, in the format of the old `eso_mnf.csv` with the name of the exported file
/// (`-` if it was not exported) in front.
pub fn write_csv(
    path: &Path,
    archive: &Archive,
    different: &[u32],
    exported: &HashMap<u32, String>,
) -> Result<(), String> {
    let mut out = String::new();
    out.push_str(&format!("{:<14}, {}\n", "Exported file", MANIFEST_HEADER));
    for idx in different {
        let Some(e) = archive.entries().get(*idx as usize) else { continue };
        let file = exported.get(idx).map(String::as_str).unwrap_or("-");
        out.push_str(&format!("{:<14}, {}\n", file, archive.manifest_line(e)));
    }
    std::fs::write(path, out).map_err(|e| format!("failed to write '{}': {}", path.display(), e))
}

/// A mesh name made safe to use as a file name (no path or reserved characters, trimmed, at most 100 characters).
pub fn safe_file_name(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() || "<>:\"/\\|?*".contains(c) { '_' } else { c })
        .collect();
    let cleaned = cleaned.trim().trim_end_matches('.').trim();
    cleaned.chars().take(100).collect()
}
