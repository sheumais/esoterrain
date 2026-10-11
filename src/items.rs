use std::{collections::HashMap, fs, io, io::Write, path::Path};

use crate::creature::Table;
use crate::model::be32;

pub const MATERIAL_DEF: u32 = 0x05;
pub const ITEM_PART_MATERIAL_DEF: u32 = 0x50;
pub const ITEM_VISUAL_TEMPLATE_DEF: u32 = 0x58;
pub const ITEM_NAME_TABLE_DEF: u32 = 0x171;
pub const ITEM_NAME_GROUP: &str = "18173141";

const NAME_TEMPLATE_OFFSET: usize = 57;
const NAME_ID_OFFSET: usize = 69;

const FF: u32 = 0xFFFF_FFFF;
const TEX_OFFSET: usize = 57;
const SCAN_START: usize = 40;

struct Part {
    model: u32,
    mat_key: u32,
}

fn parts(e: &[u8]) -> Vec<Part> {
    let mut out = Vec::new();
    let mut i = SCAN_START;
    while i + 16 <= e.len() {
        let hit = (|| {
            if be32(e, i - 4)? != FF || be32(e, i - 12)? != FF || be32(e, i + 4)? != 0 {
                return None;
            }
            let (m, k1, k2) = (be32(e, i)?, be32(e, i + 8)?, be32(e, i + 12)?);
            (m != 0 && m != FF && k1 != 0 && k1 == k2).then_some(Part { model: m, mat_key: k1 })
        })();
        match hit {
            Some(p) => {
                out.push(p);
                i += 16;
            }
            None => i += 1,
        }
    }
    out
}

struct Resolved {
    table: &'static str,
    tex: [Option<u32>; 4],
    ambiguous: bool,
}

fn resolve(key: u32, part_mats: &Table, materials: &Table) -> Option<Resolved> {
    let m4 = part_mats.get_by_row(key).and_then(|e| {
        Some([be32(e, TEX_OFFSET)?, be32(e, TEX_OFFSET + 4)?, be32(e, TEX_OFFSET + 8)?, be32(e, TEX_OFFSET + 12)?])
    });
    let m3 = materials.get_by_row(key).and_then(|e| {
        Some([be32(e, TEX_OFFSET)?, be32(e, TEX_OFFSET + 4)?, be32(e, TEX_OFFSET + 8)?])
    });
    match (m4, m3) {
        (Some(t), other) => Some(Resolved {
            table: "0x50",
            tex: [Some(t[0]), Some(t[1]), Some(t[2]), Some(t[3])],
            ambiguous: other.is_some(),
        }),
        (None, Some(t)) => Some(Resolved {
            table: "0x05",
            tex: [Some(t[0]), None, Some(t[1]), Some(t[2])],
            ambiguous: false,
        }),
        (None, None) => None,
    }
}

fn template_names(name_tbl: &Table) -> HashMap<u32, u32> {
    let mut m = HashMap::new();
    for i in 0..name_tbl.len() {
        let e = name_tbl.entry_at(i);
        if let (Some(t), Some(n)) = (be32(e, NAME_TEMPLATE_OFFSET), be32(e, NAME_ID_OFFSET)) {
            m.entry(t).or_insert(n);
        }
    }
    m
}

pub fn build_csv(
    templates: &Table, part_mats: &Table, materials: &Table, name_tbl: &Table,
    names: &HashMap<u32, String>, out_path: &Path,
) -> io::Result<usize> {
    let tmpl_name = template_names(name_tbl);
    let mut f = io::BufWriter::new(fs::File::create(out_path)?);
    writeln!(f, "template_id,part,model,diffuse,tint,normal,specular,name")?;
    let fmt = |v: Option<u32>| v.map(|x| format!("{:07}", x)).unwrap_or_default();
    let (mut rows, mut no_parts, mut unresolved, mut unnamed) = (0usize, 0usize, 0usize, 0usize);

    for i in 0..templates.len() {
        let e = templates.entry_at(i);
        let id = be32(e, 20).unwrap_or(0);
        let ps = parts(e);
        if ps.is_empty() {
            no_parts += 1;
        }
        let name = tmpl_name.get(&id).and_then(|nid| names.get(nid));
        for (n, p) in ps.iter().enumerate() {
            let Some(r) = resolve(p.mat_key, part_mats, materials) else {
                unresolved += 1;
                continue;
            };
            unnamed += name.is_none() as usize;
            writeln!(
                f, "{},{},{:07},{},{},{},{},{}",
                id, n, p.model, fmt(r.tex[0]), fmt(r.tex[1]), fmt(r.tex[2]), fmt(r.tex[3]),
                crate::worldlut::csv_field(name.map(String::as_str).unwrap_or(""))
            )?;
            rows += 1;
        }
    }
    f.flush()?;
    eprintln!(
        "Note: {} templates without a model/material part, {} parts whose material row was not found, {} rows without a name.",
        no_parts, unresolved, unnamed
    );
    Ok(rows)
}