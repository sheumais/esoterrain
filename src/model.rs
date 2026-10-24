use std::{collections::HashMap, io::{self, Write}, path::Path};

const HEADER_LEN: usize = 16;
const ENTRY_HEADER_LEN: usize = 32;

pub struct ModelRecord {
    pub row_id: u32,
    pub value_a: u32,
    pub value_b: u32,
}

pub fn be32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap()))
}

pub fn parse(data: &[u8]) -> io::Result<Vec<ModelRecord>> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());

    if data.len() < HEADER_LEN || data[0..4] != [0xFA, 0xFA, 0xEB, 0xEB] {
        return Err(bad("bad magic"));
    }
    let count = be32(&data, 8).ok_or_else(|| bad("short header"))? as usize;

    let mut out = Vec::with_capacity(count);
    let mut pos = HEADER_LEN;
    for _ in 0..count {
        if data.get(pos..pos + 4) != Some(b"####") {
            return Err(bad(&format!("missing #### separator at 0x{:x}", pos)));
        }
        let size = be32(&data, pos + 8).ok_or_else(|| bad("short entry"))? as usize;
        let len = ENTRY_HEADER_LEN + size;
        out.push(ModelRecord {
            row_id: be32(&data, pos + 20).ok_or_else(|| bad("short entry"))?,
            value_a: be32(&data, pos + 57).ok_or_else(|| bad("short entry"))?,
            value_b: be32(&data, pos + len - 8).ok_or_else(|| bad("short entry"))?,
        });
        pos += len;
    }
    Ok(out)
}

pub const MODEL_NAME_GROUP: &str = "242841733";

pub fn parse_and_write(
    data: &[u8],
    names: &HashMap<u32, String>,
    out_path: &Path,
) -> io::Result<usize> {
    let records = parse(data)?;
    let mut unnamed = 0usize;

    let mut f = io::BufWriter::new(std::fs::File::create(out_path)?);
    writeln!(
        f,
        "furniture_id,model_id,lang_file_id,name"
    )?;
    for r in &records {
        let name = match names.get(&r.value_b) {
            Some(n) => n.as_str(),
            None => { unnamed += 1; "" }
        };
        writeln!(
            f,
            "{},{:08},{:08},\"{}\"",
            r.row_id, r.value_a, r.value_b, name
        )?;
    }
    if unnamed > 0 {
        eprintln!("Note: {} model(s) have no name in the lang file.", unnamed);
    }
    f.flush()?;
    Ok(records.len())
}