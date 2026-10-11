use std::{collections::HashMap, fs, io, io::Write, path::Path};

use crate::model::be32;

pub const CREATURE_NAME_GROUP: &str = "8290981";

const HEADER_LEN: usize = 16;
const ONE_F32: [u8; 4] = [0x00, 0x00, 0x80, 0x3F];
const MODEL_SCAN: std::ops::Range<usize> = 60..400;
const TEX_OFFSET: usize = 57;

const CREATURE_LINK_OFFSET: usize = 0x98;
const LINK_MODEL_ROW_OFFSET: usize = 57;
const DEFAULT_MODEL: u32 = 55707;
const _DEFAULT_SKELETON: u32 = 849;
const _DEFAULT_TEX_ROW: u32 = 1043;

pub fn be24(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o + 3).map(|s| u32::from_be_bytes([0, s[0], s[1], s[2]]))
}

fn model_record(e: &[u8]) -> Option<(u32, u32, u32)> {
    for i in MODEL_SCAN {
        let c = *e.get(i.checked_sub(1)?)? as usize;
        if !(1..=32).contains(&c) || e.get(i + 4..i + 8)? != ONE_F32 {
            continue;
        }
        if c > 1 && e.get(i + 20..i + 24) != Some(&ONE_F32[..]) {
            continue;
        }
        let end = i + 16 * c;
        let len = *e.get(end + 9)? as usize; // optional string (e.g. "BretonMale")
        return Some((be32(e, i + 8)?, be32(e, i + 12)?, be32(e, end + 55 + len)?));
    }
    None
}

pub struct Appearance {
    pub model: u32,
    pub skeleton: u32,
    pub tex: [u32; 3],
}

fn resolve(c: &[u8], links: &Table, models: &Table, textures: &Table) -> Option<Appearance> {
    let link = links.get_by_row(be24(c, CREATURE_LINK_OFFSET)?)?;
    let model = models.get_by_row(be32(link, LINK_MODEL_ROW_OFFSET)?)?;
    let (m, tex_row, sk) = model_record(model)?;
    if matches!(m,  0 | DEFAULT_MODEL) {
        return None;
    }
    let tex = texture_ids(textures.get_by_row(tex_row)?)?;
    Some(Appearance { model: m, skeleton: sk, tex })
}

pub struct Table {
    data: Vec<u8>,
    spans: Vec<(u32, usize, usize)>,
    #[allow(unused)]
    by_id: HashMap<u32, usize>,
    by_row: HashMap<u32, usize>,
}

impl Table {
    pub fn load(path: &Path) -> io::Result<Self> {
        let data = fs::read(path)?;
        let bad = |m: String| io::Error::new(io::ErrorKind::InvalidData, m);
        if data.len() < HEADER_LEN || data[..4] != [0xFA, 0xFA, 0xEB, 0xEB] {
            return Err(bad(format!("'{}': bad magic (is this the _Uncompressed file?)", path.display())));
        }
        let count = be32(&data, 8).unwrap_or(0) as usize;
        let (mut spans, mut by_id, mut by_row) = (Vec::with_capacity(count), HashMap::with_capacity(count), HashMap::with_capacity(count));
        let mut pos = HEADER_LEN;
        for _ in 0..count {
            if data.get(pos..pos + 4) != Some(b"####") {
                return Err(bad(format!("missing #### at 0x{:x} in '{}'", pos, path.display())));
            }
            let size = be32(&data, pos + 8).ok_or_else(|| bad("short entry".into()))? as usize;
            let id = be32(&data, pos + 4).unwrap();
            let len = 32 + size;
            if pos + len > data.len() {
                return Err(bad(format!("entry at 0x{:x} runs past end of file", pos)));
            }
            by_id.insert(id, spans.len());
            let row = be32(&data, pos + 20).unwrap_or(0);
            by_row.insert(row, spans.len());
            spans.push((id, pos, len));
            pos += len;
        }
        Ok(Table { data, spans, by_id, by_row })
    }

    fn entry(&self, i: usize) -> &[u8] {
        let (_, s, l) = self.spans[i];
        &self.data[s..s + l]
    }

    pub fn get_by_row(&self, row: u32) -> Option<&[u8]> {
    self.by_row.get(&row).map(|&i| self.entry(i))
}
}

fn texture_ids(e: &[u8]) -> Option<[u32; 3]> {
    Some([be32(e, TEX_OFFSET)?, be32(e, TEX_OFFSET + 4)?, be32(e, TEX_OFFSET + 8)?])
}


pub fn build_csv(
    textures: &Table, links: &Table, models: &Table, creatures: &Table,
    names: &HashMap<u32, String>, out_path: &Path,
) -> io::Result<usize> {
    let mut f = io::BufWriter::new(fs::File::create(out_path)?);
    writeln!(f, "table_id,creature_id,model,skeleton,diffuse,normal,specular,name")?;
    let (mut unresolved, mut no_name) = (0usize, 0usize);

    for i in 0..creatures.spans.len() {
        let e = creatures.entry(i);
        let id = creatures.spans[i].0;
        let name_id = be32(e, 20).unwrap_or(0);
        let a = resolve(e, links, models, textures);
        let name = names.get(&name_id);

        let fmt = |v: Option<u32>| v.map(|x| format!("{:07}", x)).unwrap_or_default();
        if a.is_some() {
            writeln!(
                f, "{:05},{:06},{},{},{},{},{},{}", id, name_id,
                fmt(a.as_ref().map(|x| x.model)), fmt(a.as_ref().map(|x| x.skeleton)),
                fmt(a.as_ref().map(|x| x.tex[0])), fmt(a.as_ref().map(|x| x.tex[1])),
                fmt(a.as_ref().map(|x| x.tex[2])),
                crate::worldlut::csv_field(name.map(String::as_str).unwrap_or(""))
            )?;
        }
        unresolved += a.is_none() as usize;
        no_name += name.is_none() as usize;
    }
    f.flush()?;
    eprintln!("Note: {} creatures without an appearance, {} without a name.", unresolved, no_name);
    Ok(creatures.spans.len())
}