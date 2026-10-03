use std::collections::HashMap;
use std::io::{self, Write};
use std::path::Path;

pub const ZONE_NAME_GROUP: &str = "162658389";

const MAGIC: [u8; 4] = [0xFA, 0xFA, 0xEB, 0xEB];
const DELIM: &[u8] = b"####";

const OFF_INDEX: usize = 0;
const OFF_WORLD: usize = 16;
const OFF_WIDTH: usize = 53;
const OFF_HEIGHT: usize = 57;
const OFF_NAME_LEN: usize = 61;
const OFF_NAME: usize = 63;
const ZONE_AFTER_NAME: usize = 17;

#[derive(Debug, Clone)]
pub struct WorldEntry {
    pub record: u32,
    pub world_id: u32,
    pub zone_id: u32,
    pub width: u32,
    pub height: u32,
    pub name: String,
}

fn be_u32(b: &[u8], o: usize) -> Option<u32> {
    let s = b.get(o..o + 4)?;
    Some(u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
}

fn be_u16(b: &[u8], o: usize) -> Option<u16> {
    let s = b.get(o..o + 2)?;
    Some(u16::from_be_bytes([s[0], s[1]]))
}

fn parse_record(r: &[u8]) -> Option<WorldEntry> {
    let name_len = be_u16(r, OFF_NAME_LEN)? as usize;
    let name_bytes = r.get(OFF_NAME..OFF_NAME + name_len)?;
    Some(WorldEntry {
        record: be_u32(r, OFF_INDEX)?,
        world_id: be_u32(r, OFF_WORLD)?,
        width: be_u32(r, OFF_WIDTH)?,
        height: be_u32(r, OFF_HEIGHT)?,
        name: String::from_utf8_lossy(name_bytes).into_owned(),
        zone_id: be_u32(r, OFF_NAME + name_len + ZONE_AFTER_NAME)?,
    })
}

pub fn parse(data: &[u8]) -> io::Result<(Vec<WorldEntry>, usize)> {
    if data.len() < 16 || data[..4] != MAGIC {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "not a world LUT file (bad magic)",
        ));
    }
    let declared = be_u32(data, 8).unwrap_or(0) as usize;

    let mut chunks: Vec<&[u8]> = Vec::new();
    let mut start = 0usize;
    let mut i = 0usize;
    while i + DELIM.len() <= data.len() {
        if &data[i..i + DELIM.len()] == DELIM {
            chunks.push(&data[start..i]);
            i += DELIM.len();
            start = i;
        } else {
            i += 1;
        }
    }
    chunks.push(&data[start..]);

    let mut entries = Vec::new();
    let mut skipped = 0usize;
    for chunk in chunks.iter().skip(1) {
        match parse_record(chunk) {
            Some(e) => entries.push(e),
            None => skipped += 1,
        }
    }

    if declared != 0 && declared != entries.len() + skipped {
        eprintln!(
            "Warning: header declares {} records but {} were found.",
            declared,
            entries.len() + skipped
        );
    }
    Ok((entries, skipped))
}

pub fn csv_field(s: &str) -> String {
    if s.contains(',') || s.contains('"') || s.contains('\n') || s.contains('\r') {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

fn for_each_csv_row(text: &str, mut f: impl FnMut(&[String])) {
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut in_quotes = false;
    let mut chars = text.chars().peekable();

    while let Some(c) = chars.next() {
        if in_quotes {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                field.push(c);
            }
        } else {
            match c {
                '"' => in_quotes = true,
                ',' => row.push(std::mem::take(&mut field)),
                '\r' => {}
                '\n' => {
                    row.push(std::mem::take(&mut field));
                    f(&row);
                    row.clear();
                }
                _ => field.push(c),
            }
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        f(&row);
    }
}

pub fn load_lang_group(lang_csv: &Path, group: &str) -> io::Result<HashMap<u32, String>> {
    let bytes = std::fs::read(lang_csv)?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_start_matches('\u{feff}');

    let mut names = HashMap::new();
    for_each_csv_row(text, |row| {
        if row.len() < 5 || row[0] != group {
            return;
        }
        if let Ok(idx) = row[2].trim().parse::<u32>() {
            names.insert(idx, row[4].clone());
        }
    });
    Ok(names)
}

pub fn load_zone_names(lang_csv: &Path) -> io::Result<HashMap<u32, String>> {
    load_lang_group(lang_csv, ZONE_NAME_GROUP)
}

pub fn write_csv(
    entries: &[WorldEntry],
    zone_names: &HashMap<u32, String>,
    out_path: &Path,
) -> io::Result<()> {
    let mut sorted: Vec<&WorldEntry> = entries.iter().collect();
    sorted.sort_by_key(|e| (e.world_id, e.record));

    let mut f = io::BufWriter::new(std::fs::File::create(out_path)?);
    writeln!(
        f,
        "world_id,world_id_hex,zone_id,zone_id_hex,width,height,world_name,zone_name"
    )?;
    for e in sorted {
        let zone_name = zone_names.get(&e.zone_id).map(String::as_str).unwrap_or("");
        writeln!(
            f,
            "{},0x{:04X},{},0x{:04X},{},{},{},{}",
            e.world_id,
            e.world_id,
            e.zone_id,
            e.zone_id,
            e.width,
            e.height,
            csv_field(&e.name),
            csv_field(zone_name)
        )?;
    }
    f.flush()
}

pub fn parse_and_write(
    in_path: &Path,
    zone_names: &HashMap<u32, String>,
    out_path: &Path,
) -> io::Result<usize> {
    let data = std::fs::read(in_path)?;
    let (entries, skipped) = parse(&data)?;
    if skipped > 0 {
        eprintln!("Warning: skipped {} malformed record(s).", skipped);
    }
    let unnamed = entries
        .iter()
        .filter(|e| !zone_names.contains_key(&e.zone_id))
        .count();
    if unnamed > 0 {
        eprintln!("Note: {} world(s) have no zone name in the lang file.", unnamed);
    }
    write_csv(&entries, zone_names, out_path)?;
    Ok(entries.len())
}

#[derive(Debug, Clone)]
pub struct WorldInfo {
    pub zone_id: u32,
    pub width: u32,
    pub height: u32,
    pub world_name: String,
    pub zone_name: String,
}


pub fn load_lut_csv(path: &Path) -> io::Result<HashMap<u32, WorldInfo>> {
    let bytes = std::fs::read(path)?;
    let text = String::from_utf8_lossy(&bytes);
    let text = text.trim_start_matches('\u{feff}');
 
    let mut map = HashMap::new();
    for_each_csv_row(text, |row| {
        if row.len() < 7 {
            return;
        }
        let (Ok(world), Ok(zone), Ok(w), Ok(h)) = (
            row[0].trim().parse::<u32>(),
            row[2].trim().parse::<u32>(),
            row[4].trim().parse::<u32>(),
            row[5].trim().parse::<u32>(),
        ) else {
            return;
        };
        map.insert(
            world,
            WorldInfo {
                zone_id: zone,
                width: w,
                height: h,
                world_name: row[6].clone(),
                zone_name: row.get(7).cloned().unwrap_or_default(),
            },
        );
    });
    Ok(map)
}
