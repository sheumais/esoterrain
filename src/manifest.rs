use std::collections::{HashMap, HashSet};

use eso_common::mnf::MnfFileEntry;

pub const WORLD_MASK: u32 = 0x7FF;
pub const TYPE_MASK: u32 = 0x1F;
pub const PLACEMENT_CATEGORY: u32 = 0x4800_0000;
pub const PLACEMENT_TYPE: u32 = 0x02;
/// Second per-world placement file (same record layout as PLACEMENT_TYPE). It references models the primary
/// placement file does not, so the world model LUT reads both.
pub const PLACEMENT_TYPE_SECONDARY: u32 = 0x01;
pub const CELL_PLACEMENT_TYPE: u32 = 0x15;

#[derive(Debug, Clone, Copy)]
pub struct TocMatch {
    pub index: u32,
    pub arch: u32,
}

#[derive(Debug, Clone, Copy)]
pub struct CellMatch {
    pub index: u32,
    pub x: u32,
    pub z: u32,
    pub arch: u32,
}

pub struct ManifestResult {
    pub toc_matches: Vec<TocMatch>,
    pub cell_matches: Vec<CellMatch>,
}


#[derive(Debug, Clone, Copy)]
pub struct PlacementMatch {
    pub index: u32,
    pub arch: u32,
}

fn placement_high(world: u32) -> u32 {
    PLACEMENT_CATEGORY | ((world & WORLD_MASK) << 5) | (PLACEMENT_TYPE & TYPE_MASK)
}

fn toc_file_id(world: u32) -> u64 {
    (world as u64) | 0x4400_0000_0000_0000
}

fn decode_cell_from_fileid(file_id: u64, world: u32) -> Option<(u32, u32)> {
    let high = ((file_id >> 32) & 0xFFFF_FFFF) as u32;
    let low = (file_id & 0xFFFF_FFFF) as u32;
    let expected_high =
        (0x4000_0000u32 | ((world & WORLD_MASK) << 5) | (1 & TYPE_MASK)) & 0xFFFF_FFFF;
    if high != expected_high {
        return None;
    }
    Some((low >> 16, low & 0xFFFF))
}

pub fn parse_int(s: &str) -> Option<i64> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Some(hex) = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
        i64::from_str_radix(hex, 16).ok()
    } else {
        s.parse::<i64>().ok()
    }
}

pub fn filter_manifest(entries: &[MnfFileEntry], world: u32) -> ManifestResult {
    let target_toc = toc_file_id(world);
    let mut toc_matches = Vec::new();
    let mut cell_matches = Vec::new();

    for e in entries {
        let file_id = ((e.unknown1 as u64) << 32) | (e.file_index as u64);

        if file_id == target_toc {
            toc_matches.push(TocMatch { index: e.index, arch: e.archive_index as u32 });
            continue;
        }

        if let Some((x, z)) = decode_cell_from_fileid(file_id, world) {
            cell_matches.push(CellMatch { index: e.index, x, z, arch: e.archive_index as u32 });
        }
    }

    ManifestResult { toc_matches, cell_matches }
}

pub const COLOURMAP_TYPE_NORMAL: u32 = 0x0C;
pub const COLOURMAP_TYPE_OVERLAY: u32 = 0x0D;

fn colourmap_high(world: u32, ty: u32) -> u32 {
    0x4000_0000u32 | ((world & WORLD_MASK) << 5) | (ty & TYPE_MASK)
}

#[derive(Debug, Clone, Copy)]
pub struct ColourMapMatch {
    pub index: u32,
    pub x: u32,
    pub z: u32,
    pub overlay: bool,
    pub arch: u32,
}

pub fn filter_colourmap_manifest(entries: &[MnfFileEntry], world: u32) -> Vec<ColourMapMatch> {
    let target_normal = colourmap_high(world, COLOURMAP_TYPE_NORMAL);
    let target_overlay = colourmap_high(world, COLOURMAP_TYPE_OVERLAY);

    let mut matches = Vec::new();
    for e in entries {
        let overlay = if e.unknown1 == target_normal {
            false
        } else if e.unknown1 == target_overlay {
            true
        } else {
            continue;
        };
        matches.push(ColourMapMatch {
            index: e.index,
            x: e.file_index >> 16,
            z: e.file_index & 0xFFFF,
            overlay,
            arch: e.archive_index as u32,
        });
    }
    matches
}

/// Every per-world placement file (category 0x4800, FileIndex 0) of the requested types in ONE pass:
/// (world, type) -> first matching entry.
pub fn filter_placement_manifest_all(
    entries: &[MnfFileEntry],
    types: &[u32],
) -> HashMap<(u32, u32), PlacementMatch> {
    let mut out: HashMap<(u32, u32), PlacementMatch> = HashMap::new();
    for e in entries {
        let unk1 = e.unknown1;
        if (unk1 >> 16) != (PLACEMENT_CATEGORY >> 16) || e.file_index != 0 {
            continue;
        }
        let ty = unk1 & TYPE_MASK;
        if !types.contains(&ty) {
            continue;
        }
        let world = (unk1 >> 5) & WORLD_MASK;
        out.entry((world, ty)).or_insert(PlacementMatch { index: e.index, arch: e.archive_index as u32 });
    }
    out
}

pub fn filter_placement_manifest(entries: &[MnfFileEntry], world: u32) -> Option<PlacementMatch> {
    let target = placement_high(world);
    entries
        .iter()
        .find(|e| e.unknown1 == target && e.file_index == 0)
        .map(|e| PlacementMatch { index: e.index, arch: e.archive_index as u32 })
}

const TERRAIN_CELL_TYPE: u32 = 0x01;
const CELL_HIGH_MARKER: u32 = 0x4000;
pub const UNAVAILABLE_ARCHIVE_THRESHOLD: u32 = 242;

#[derive(Debug, Clone)]
pub struct WorldSummary {
    pub world: u32,
    pub min_x: u32,
    pub max_x: u32,
    pub min_z: u32,
    pub max_z: u32,
    pub cell_count: usize,
    pub archives: Vec<u32>,
}

impl WorldSummary {
    pub fn grid_w(&self) -> u32 {
        self.max_x - self.min_x + 1
    }

    pub fn grid_h(&self) -> u32 {
        self.max_z - self.min_z + 1
    }

    pub fn has_unavailable_archive(&self) -> bool {
        self.archives
            .iter()
            .any(|a| *a >= UNAVAILABLE_ARCHIVE_THRESHOLD)
    }
}

pub fn list_worlds_with_cells(entries: &[MnfFileEntry]) -> Vec<WorldSummary> {
    let mut per_world: HashMap<u32, (u32, u32, u32, u32, usize, HashSet<u32>)> = HashMap::new();

    for e in entries {
        let (file_index, unk1, arch) = (e.file_index, e.unknown1, e.archive_index as u32);

        if (unk1 >> 16) != CELL_HIGH_MARKER {
            continue;
        }
        if (unk1 & TYPE_MASK) != TERRAIN_CELL_TYPE {
            continue;
        }

        let world = (unk1 >> 5) & WORLD_MASK;
        let x = file_index >> 16;
        let z = file_index & 0xFFFF;

        let entry = per_world
            .entry(world)
            .or_insert((x, x, z, z, 0, HashSet::new()));
        entry.0 = entry.0.min(x);
        entry.1 = entry.1.max(x);
        entry.2 = entry.2.min(z);
        entry.3 = entry.3.max(z);
        entry.4 += 1;
        entry.5.insert(arch);
    }

    let mut summaries: Vec<WorldSummary> = per_world
        .into_iter()
        .map(|(world, (min_x, max_x, min_z, max_z, cell_count, archives))| {
            let mut archives: Vec<u32> = archives.into_iter().collect();
            archives.sort_unstable();
            WorldSummary { world, min_x, max_x, min_z, max_z, cell_count, archives }
        })
        .collect();
    summaries.sort_unstable_by_key(|w| w.world);
    summaries
}

#[derive(Debug, Clone, Copy)]
pub struct TableMatch {
    pub index: u32,
    pub arch: u32,
}

pub const TABLE_HIGH: u32 = 0x6000_0000;
pub const WORLD_LUT_FILE_INDEX: u32 = 0x3C;
pub const MODEL_LIST_FILE_INDEX: u32 = 0x145;

pub fn filter_table_manifest(entries: &[MnfFileEntry], target_file_index: u32) -> Option<TableMatch> {
    entries
        .iter()
        .find(|e| e.unknown1 == TABLE_HIGH && e.file_index == target_file_index && e.id1 == 0)
        .map(|e| TableMatch { index: e.index, arch: e.archive_index as u32 })
}

#[derive(Debug, Clone, Copy)]
pub struct CellPlacementMatch {
    pub index: u32,
    pub x: u32,
    pub z: u32,
    pub arch: u32,
}

pub fn filter_cell_placement_manifest(entries: &[MnfFileEntry], world: u32) -> Vec<CellPlacementMatch> {
    let mut out = Vec::new();
    for e in entries {
        let unk1 = e.unknown1;
        if (unk1 >> 16) != CELL_HIGH_MARKER
            || ((unk1 >> 5) & WORLD_MASK) != world
            || (unk1 & TYPE_MASK) != CELL_PLACEMENT_TYPE
        { continue; }
        out.push(CellPlacementMatch { index: e.index, x: e.file_index >> 16, z: e.file_index & 0xFFFF, arch: e.archive_index as u32 });
    }
    out
}

/// Manifest file type (low 5 bits of Unk1) of the terrain LOD mesh files - entry 0x0A ("terrain-lod") in the world TOC.
pub const TERRAIN_LOD_TYPE: u32 = 0x0A;

#[derive(Debug, Clone, Copy)]
pub struct LodMatch {
    pub index: u32,
    pub x: u32,
    pub z: u32,
    pub arch: u32,
}

pub fn filter_lod_manifest(entries: &[MnfFileEntry], world: u32) -> Vec<LodMatch> {
    let mut out = Vec::new();
    for e in entries {
        let unk1 = e.unknown1;
        if (unk1 >> 16) != CELL_HIGH_MARKER
            || ((unk1 >> 5) & WORLD_MASK) != world
            || (unk1 & TYPE_MASK) != TERRAIN_LOD_TYPE
        { continue; }
        out.push(LodMatch { index: e.index, x: e.file_index >> 16, z: e.file_index & 0xFFFF, arch: e.archive_index as u32 });
    }
    out
}

#[derive(Debug, Clone, Copy)]
pub struct SizeEntry {
    pub ty: u32,
    pub x: u32,
    pub z: u32,
    pub size: u32,
}

pub fn filter_size_manifest(entries: &[MnfFileEntry], world: u32) -> Vec<SizeEntry> {
    let mut out = Vec::new();
    for e in entries {
        let unk1 = e.unknown1;
        if (unk1 >> 16) != CELL_HIGH_MARKER || ((unk1 >> 5) & WORLD_MASK) != world {
            continue;
        }
        out.push(SizeEntry {
            ty: unk1 & TYPE_MASK,
            x: e.file_index >> 16,
            z: e.file_index & 0xFFFF,
            size: e.size,
        });
    }
    out
}
