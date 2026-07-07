use std::io;

pub const WORLD_MASK: u32 = 0x7FF;
pub const TYPE_MASK: u32 = 0x1F;

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

fn parse_int(s: &str) -> Option<i64> {
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