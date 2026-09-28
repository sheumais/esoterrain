use std::{collections::HashSet, io};

pub const WORLD_MASK: u32 = 0x7FF;
pub const TYPE_MASK: u32 = 0x1F;
pub const PLACEMENT_CATEGORY: u32 = 0x4800_0000;
pub const PLACEMENT_TYPE: u32 = 0x02;

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

pub fn filter_manifest(manifest_path: &str, world: u32) -> io::Result<ManifestResult> {
    let target_toc = toc_file_id(world);
    let mut toc_matches = Vec::new();
    let mut cell_matches = Vec::new();

    let content = std::fs::read_to_string(manifest_path)?;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Vec<&str> = line.split(',').map(|c| c.trim()).collect();
        if row.len() < 11 {
            continue;
        }

        let (index, file_index, unk1, arch) = match (
            parse_int(row[0]),
            parse_int(row[2]),
            parse_int(row[3]),
            parse_int(row[9]),
        ) {
            (Some(i), Some(fi), Some(u), Some(a)) => (i, fi, u, a),
            _ => continue,
        };

        let file_id = (((unk1 as u64) & 0xFFFF_FFFF) << 32) | ((file_index as u64) & 0xFFFF_FFFF);

        if file_id == target_toc {
            toc_matches.push(TocMatch {
                index: index as u32,
                arch: arch as u32,
            });
            continue;
        }

        if let Some((x, z)) = decode_cell_from_fileid(file_id, world) {
            cell_matches.push(CellMatch {
                index: index as u32,
                x,
                z,
                arch: arch as u32,
            });
        }
    }

    Ok(ManifestResult {
        toc_matches,
        cell_matches,
    })
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

pub fn filter_colourmap_manifest(manifest_path: &str, world: u32) -> io::Result<Vec<ColourMapMatch>> {
    let target_normal = colourmap_high(world, COLOURMAP_TYPE_NORMAL);
    let target_overlay = colourmap_high(world, COLOURMAP_TYPE_OVERLAY);

    let mut matches = Vec::new();
    let content = std::fs::read_to_string(manifest_path)?;
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Vec<&str> = line.split(',').map(|c| c.trim()).collect();
        if row.len() < 11 {
            continue;
        }
        let (index, file_index, unk1, arch) = match (
            parse_int(row[0]), parse_int(row[2]), parse_int(row[3]), parse_int(row[9]),
        ) {
            (Some(i), Some(fi), Some(u), Some(a)) => (i as u32, fi as u32, u as u32, a as u32),
            _ => continue,
        };

        let overlay = if unk1 == target_normal {
            false
        } else if unk1 == target_overlay {
            true
        } else {
            continue;
        };

        matches.push(ColourMapMatch {
            index,
            x: file_index >> 16,
            z: file_index & 0xFFFF,
            overlay,
            arch,
        });
    }
    Ok(matches)
}

pub fn filter_placement_manifest(manifest_path: &str, world: u32) -> io::Result<Option<PlacementMatch>> {
    let target = placement_high(world);
    let content = std::fs::read_to_string(manifest_path)?;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Vec<&str> = line.split(',').map(|c| c.trim()).collect();
        if row.len() < 11 {
            continue;
        }
        let (index, file_index, unk1, arch) = match (
            parse_int(row[0]), parse_int(row[2]), parse_int(row[3]), parse_int(row[9]),
        ) {
            (Some(i), Some(fi), Some(u), Some(a)) => (i as u32, fi as u32, u as u32, a as u32),
            _ => continue,
        };

        if unk1 == target && file_index == 0 {
            return Ok(Some(PlacementMatch { index, arch }));
        }
    }
    Ok(None)
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

pub fn list_worlds_with_cells(manifest_path: &str) -> io::Result<Vec<WorldSummary>> {
    let content = std::fs::read_to_string(manifest_path)?;

    let mut per_world: std::collections::HashMap<u32, (u32, u32, u32, u32, usize, HashSet<u32>)> = std::collections::HashMap::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let row: Vec<&str> = line.split(',').map(|c| c.trim()).collect();
        if row.len() < 11 {
            continue;
        }

        let (file_index, unk1, arch) = match (
            parse_int(row[2]),
            parse_int(row[3]),
            parse_int(row[9]),
        ) {
            (Some(fi), Some(u), Some(a)) => (fi as u32, u as u32, a as u32),
            _ => continue,
        };

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
        .map(
            |(world, (min_x, max_x, min_z, max_z, cell_count, archives))| {
                let mut archives: Vec<u32> = archives.into_iter().collect();
                archives.sort_unstable();
                WorldSummary {
                    world,
                    min_x,
                    max_x,
                    min_z,
                    max_z,
                    cell_count,
                    archives,
                }
            },
        )
        .collect();
    summaries.sort_unstable_by_key(|w| w.world);
    Ok(summaries)
}

#[derive(Debug, Clone, Copy)]
pub struct WorldLutMatch {
    pub index: u32,
    pub arch: u32,
}

pub fn filter_world_lut_manifest(manifest_path: &str) -> io::Result<Option<WorldLutMatch>> {
    let content = std::fs::read_to_string(manifest_path)?;
    for line in content.lines() {
        let row: Vec<&str> = line.split(',').map(|c| c.trim()).collect();
        if row.len() < 11 {
            continue;
        }
        let (index, id1, file_index, unk1, arch) = match (
            parse_int(row[0]), parse_int(row[1]), parse_int(row[2]), parse_int(row[3]), parse_int(row[9]),
        ) {
            (Some(i), Some(id), Some(fi), Some(u), Some(a)) => (i as u32, id as u32, fi as u32, u as u32, a as u32),
            _ => continue,
        };
        if unk1 == 0x6000_0000 && file_index == 0x3C && id1 == 0x0000_0000 {
            return Ok(Some(WorldLutMatch { index, arch }));
        }
    }
    Ok(None)
}