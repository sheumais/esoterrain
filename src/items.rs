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

/// Ids at or above this are never model FileIndexes (0xFFFFFFFF sentinels, 0xFF0008xx flag words, ...).
const BOGUS_ID: u32 = 0xFF00_0000;

struct Part {
    model: u32,
    /// Second model id of the part (0 = none). Usually `model + 2`.
    model2: u32,
    mat_key: u32,
}

/// A part record is 16 bytes starting at `i`:  `[model][model2 or 0][key][key]`, preceded by a 0xFFFFFFFF word at `i-4`
/// and another at `i-12` (single-model shape: `FF, flag, FF, model, 0, key, key`) or at `i-8` (two-model shape:
/// `.., FF, FF, model, model2, key, key`). The old scan only accepted `model2 == 0` with the FF at `i-12`, which silently
/// dropped every part that carries a second model id.
fn parts(e: &[u8]) -> Vec<Part> {
    let mut out = Vec::new();
    let mut i = SCAN_START;
    while i + 16 <= e.len() {
        let hit = (|| {
            if be32(e, i - 4)? != FF || (be32(e, i - 12)? != FF && be32(e, i - 8)? != FF) {
                return None;
            }
            let (m, m2, k1, k2) = (be32(e, i)?, be32(e, i + 4)?, be32(e, i + 8)?, be32(e, i + 12)?);
            (m != 0 && m < BOGUS_ID && m2 < BOGUS_ID && k1 != 0 && k1 == k2)
                .then_some(Part { model: m, model2: m2, mat_key: k1 })
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

struct Row {
    id: u32,
    part: usize,
    model: u32,
    model2: u32,
    tex: [Option<u32>; 4],
    name: Option<String>,
}

/// `mesh_names` receives the model ids of every part that has no (or an empty) name in the lang file and returns
/// model id -> mesh name read from that model's .gr2. The mesh name is then written instead of a blank name;
/// models it can't name stay blank.
pub fn build_csv(
    templates: &Table, part_mats: &Table, materials: &Table, name_tbl: &Table,
    names: &HashMap<u32, String>, mesh_names: &dyn Fn(&[u32]) -> HashMap<u32, String>, out_path: &Path,
) -> io::Result<usize> {
    let tmpl_name = template_names(name_tbl);
    let fmt = |v: Option<u32>| v.map(|x| format!("{:07}", x)).unwrap_or_default();
    let (mut no_parts, mut unresolved) = (0usize, 0usize);

    let mut rows: Vec<Row> = Vec::new();
    for i in 0..templates.len() {
        let e = templates.entry_at(i);
        let id = be32(e, 20).unwrap_or(0);
        let ps = parts(e);
        if ps.is_empty() {
            no_parts += 1;
        }
        let name = tmpl_name.get(&id).and_then(|nid| names.get(nid)).filter(|s| !s.trim().is_empty());
        for (n, p) in ps.iter().enumerate() {
            // Keep parts whose material row can't be resolved (blank texture columns) so their model ids are not lost.
            let r = resolve(p.mat_key, part_mats, materials);
            unresolved += r.is_none() as usize;
            rows.push(Row {
                id,
                part: n,
                model: p.model,
                model2: p.model2,
                tex: r.map(|r| r.tex).unwrap_or([None; 4]),
                name: name.cloned(),
            });
        }
    }

    // Parts without a lang name: fall back to the mesh name stored in the model file itself.
    let mut blank: Vec<u32> = rows.iter().filter(|r| r.name.is_none()).map(|r| r.model).collect();
    let blank_rows = blank.len();
    blank.sort_unstable();
    blank.dedup();
    let mut from_mesh = 0usize;
    if !blank.is_empty() {
        println!(
            "{} part(s) have no name in the lang file; reading the mesh name of their {} distinct model(s)...",
            blank_rows, blank.len()
        );
        let found = mesh_names(&blank);
        for r in rows.iter_mut().filter(|r| r.name.is_none()) {
            if let Some(n) = found.get(&r.model) {
                r.name = Some(n.clone());
                from_mesh += 1;
            }
        }
    }
    let unnamed = rows.iter().filter(|r| r.name.is_none()).count();

    let mut f = io::BufWriter::new(fs::File::create(out_path)?);
    writeln!(f, "template_id,part,model,diffuse,tint,normal,specular,name,model2")?;
    for r in &rows {
        writeln!(
            f, "{},{},{:07},{},{},{},{},{},{}",
            r.id, r.part, r.model, fmt(r.tex[0]), fmt(r.tex[1]), fmt(r.tex[2]), fmt(r.tex[3]),
            crate::worldlut::csv_field(r.name.as_deref().unwrap_or("")),
            if r.model2 != 0 { format!("{:07}", r.model2) } else { String::new() }
        )?;
    }
    f.flush()?;
    eprintln!(
        "Note: {} templates without a model/material part, {} parts whose material row was not found (written with blank textures), {} rows named from the model's mesh, {} rows without a name.",
        no_parts, unresolved, from_mesh, unnamed
    );
    Ok(rows.len())
}