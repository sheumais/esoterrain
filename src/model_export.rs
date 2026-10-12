//! Extract one model (by MNF FileIndex) together with every texture it references.
//!
//! How the link works:
//!   * The model's .gr2 starts with a table of 0x60-byte records. Each record begins with the
//!     fixed byte pattern `REF_SIG` and holds the referenced texture's MNF FileIndex as a
//!     little-endian u32 at +0x38.
//!   * Models and textures are plain files, i.e. manifest rows with Unk1 == 0.
//!

use std::cell::{Cell, RefCell};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use crate::export;
use crate::manifest::parse_int;

const REF_SIG: [u8; 28] = [
    0x08, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00,
    0x08, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00,
];
const REF_FILE_INDEX_OFFSET: usize = 0x38;
const REF_SCAN_LIMIT: usize = 1 << 20;


const SLOT_ROLES: &[&str] = &["diffuse", "normal", "specular"];

const MODEL_EXTS: &[&str] = &["gr2"];
const TEXTURE_EXTS: &[&str] = &["dds", "xv4"];

pub struct Options<'a> {
    pub manifest: &'a str,
    pub exe: &'a str,
    pub mnf: &'a str,
    pub temp_dir: &'a str,
    pub out_root: &'a Path,
    pub dry_run: bool,
    pub skip_export: bool,
    pub keep_temp: bool,
    
    pub noesis: &'a str,
}

#[derive(Debug, Clone)]
pub struct MnfRow {
    pub index: u32,
    pub id1: u32,
    pub file_index: u32,
    pub size: u32,
    pub arch: u32,
}

pub struct Summary {
    pub out_dir: PathBuf,
    pub textures_found: usize,
    pub textures_missing: usize,
}

type RowMap = HashMap<u32, Vec<MnfRow>>;

fn manifest_rows(manifest_path: &str) -> io::Result<Arc<RowMap>> {
    static CACHE: Mutex<Option<(String, Arc<RowMap>)>> = Mutex::new(None);
    let mut guard = CACHE.lock().unwrap();
    if let Some((p, m)) = guard.as_ref() {
        if p == manifest_path {
            return Ok(m.clone());
        }
    }
    let content = fs::read_to_string(manifest_path)?;
    let mut out: RowMap = HashMap::new();
    for line in content.lines() {
        let row: Vec<&str> = line.split(',').map(|c| c.trim()).collect();
        if row.len() < 11 {
            continue;
        }
        let (index, id1, file_index, unk1, size, arch) = match (
            parse_int(row[0]),
            parse_int(row[1]),
            parse_int(row[2]),
            parse_int(row[3]),
            parse_int(row[4]),
            parse_int(row[9]),
        ) {
            (Some(i), Some(id), Some(fi), Some(u), Some(sz), Some(a)) => {
                (i as u32, id as u32, fi as u32, u as u32, sz as u32, a as u32)
            }
            _ => continue,
        };
        if unk1 != 0 {
            continue;
        }
        out.entry(file_index).or_default().push(MnfRow { index, id1, file_index, size, arch });
    }
    let arc = Arc::new(out);
    *guard = Some((manifest_path.to_string(), arc.clone()));
    Ok(arc)
}

pub fn lookup_file_indexes(
    manifest_path: &str,
    wanted: &HashSet<u32>,
) -> io::Result<HashMap<u32, Vec<MnfRow>>> {
    let all = manifest_rows(manifest_path)?;
    Ok(wanted.iter().filter_map(|k| all.get(k).map(|v| (*k, v.clone()))).collect())
}

pub fn texture_refs(blob: &[u8]) -> Vec<u32> {
    let end = blob.len().min(REF_SCAN_LIMIT);
    let scan = &blob[..end];
    let mut out = Vec::new();
    let mut seen = HashSet::new();

    let mut pos = 0usize;
    while pos + REF_SIG.len() <= scan.len() {
        if scan[pos..pos + REF_SIG.len()] == REF_SIG {
            let o = pos + REF_FILE_INDEX_OFFSET;
            if let Some(b) = blob.get(o..o + 4) {
                let fi = u32::from_le_bytes(b.try_into().unwrap());
                if fi != 0 && seen.insert(fi) {
                    out.push(fi);
                }
            }
            pos += REF_SIG.len();
        } else {
            pos += 1;
        }
    }
    out
}

pub fn texture_ref_sets(blob: &[u8]) -> Vec<Vec<u32>> {
    let end = blob.len().min(REF_SCAN_LIMIT);
    let scan = &blob[..end];
    let mut all = Vec::new();

    let mut pos = 0usize;
    while pos + REF_SIG.len() <= scan.len() {
        if scan[pos..pos + REF_SIG.len()] == REF_SIG {
            let o = pos + REF_FILE_INDEX_OFFSET;
            if let Some(b) = blob.get(o..o + 4) {
                all.push(u32::from_le_bytes(b.try_into().unwrap()));
            }
            pos += REF_SIG.len();
        } else {
            pos += 1;
        }
    }
    all.chunks(SLOT_ROLES.len())
        .map(|c| c.to_vec())
        .filter(|set| set.iter().any(|fi| *fi != 0))
        .collect()
}

#[derive(Debug, Clone)]
pub struct MatMap {
    pub usage: String,
    pub file_index: u32,
}

#[derive(Debug, Clone)]
pub struct MatDef {
    pub name: String,
    pub maps: Vec<MatMap>,
    pub alpha: bool,
}



struct Gr2<'a> {
    b: &'a [u8],
    
    secs: Vec<(usize, usize)>,
    
    ptrs: HashMap<(u32, u32), (u32, u32)>,
    root: (u32, u32),
}

fn le32(b: &[u8], o: usize) -> Option<u32> {
    b.get(o..o.checked_add(4)?).map(|s| u32::from_le_bytes(s.try_into().unwrap()))
}

impl<'a> Gr2<'a> {
    fn parse(b: &'a [u8]) -> Option<Gr2<'a>> {
        let base = 32usize;
        if le32(b, 20)? != 0 {
            return None;
        }
        let sec_off = le32(b, base + 12)? as usize;
        let sec_cnt = le32(b, base + 16)? as usize;
        let root = (le32(b, base + 28)?, le32(b, base + 32)?);
        if sec_cnt == 0 || sec_cnt > 64 {
            return None;
        }
        let mut secs = Vec::new();
        let mut ptrs = HashMap::new();
        for s in 0..sec_cnt {
            let h = base + sec_off + s * 44;
            let compression = le32(b, h)?;
            let (data_off, data_size) = (le32(b, h + 4)? as usize, le32(b, h + 8)? as usize);
            let (rel_off, rel_cnt) = (le32(b, h + 28)? as usize, le32(b, h + 32)? as usize);
            if compression != 0 && data_size != 0 {
                return None;
            }
            for i in 0..rel_cnt {
                let r = rel_off + i * 12;
                ptrs.insert((s as u32, le32(b, r)?), (le32(b, r + 4)?, le32(b, r + 8)?));
            }
            secs.push((data_off, data_size));
        }
        Some(Gr2 { b, secs, ptrs, root })
    }

    fn u32(&self, s: u32, o: u32) -> Option<u32> {
        let (d, sz) = *self.secs.get(s as usize)?;
        if (o as usize).checked_add(4)? > sz {
            return None;
        }
        le32(self.b, d + o as usize)
    }

    fn ptr(&self, s: u32, o: u32) -> Option<(u32, u32)> {
        self.ptrs.get(&(s, o)).copied()
    }

    fn cstr(&self, p: (u32, u32)) -> Option<String> {
        let (d, sz) = *self.secs.get(p.0 as usize)?;
        let start = d.checked_add(p.1 as usize)?;
        let end = d.checked_add(sz)?.min(self.b.len());
        let bytes = self.b.get(start..end)?;
        let n = bytes.iter().position(|c| *c == 0)?;
        Some(String::from_utf8_lossy(&bytes[..n]).into_owned())
    }
    
    fn material(&self, m: (u32, u32)) -> Option<MatDef> {
        let (s, o) = m;
        let name = self.ptr(s, o).and_then(|p| self.cstr(p)).unwrap_or_default();
        let n = self.u32(s, o + 8)?;
        if n == 0 || n > 64 {
            return None;
        }
        let maps = self.ptr(s, o + 12)?;
        let mut out = Vec::new();
        for k in 0..n {
            let mo = maps.1 + 16 * k;
            let usage = self.ptr(maps.0, mo).and_then(|p| self.cstr(p)).unwrap_or_default();
            let Some(sub) = self.ptr(maps.0, mo + 8) else { continue };
            let Some(tex) = self.ptr(sub.0, sub.1 + 20) else { continue };
            let Some(fi) = self.u32(tex.0, tex.1 + 92) else { continue };
            if fi != 0 {
                out.push(MatMap { usage, file_index: fi });
            }
        }
        if out.is_empty() {
            return None;
        }
        let alpha = self.ext_alpha(m).unwrap_or(true);
        Some(MatDef { name, maps: out, alpha })
    }

    fn ext_alpha(&self, m: (u32, u32)) -> Option<bool> {
        let (s, o) = m;
        let (ts, mut t) = self.ptr(s, o + 28)?;
        let (os, oo) = self.ptr(s, o + 36)?;
        let (mut pos, mut flag, mut seen) = (0u32, false, false);
        for _ in 0..512 {
            let mt = self.u32(ts, t)?;
            if mt == 0 {
                break;
            }
            let n = self.u32(ts, t + 20)?.max(1);
            let size = match mt {
                8 | 2 | 22 => 8,
                5 => 16,
                10 | 19 | 20 => 4 * n,
                11 | 12 | 13 | 14 => n,
                15 | 16 | 17 | 18 | 21 => 2 * n,
                _ => break,
            };
            if mt == 19 || mt == 20 {
                if let Some(nm) = self.ptr(ts, t + 4).and_then(|p| self.cstr(p)) {
                    if nm == "bAlphaTransparency" || nm == "bAlphaMaskEnable" {
                        seen = true;
                        if self.u32(os, oo + pos)? != 0 {
                            flag = true;
                        }
                    }
                }
            }
            pos += size;
            t += 44;
        }
        if seen { Some(flag) } else { None }
    }

    fn at(&self, s: u32, o: u32, n: usize) -> Option<&'a [u8]> {
        let (d, sz) = *self.secs.get(s as usize)?;
        let o = o as usize;
        if o.checked_add(n)? > sz { return None; }
        self.b.get(d + o..d + o + n)
    }
}




pub fn parse_materials(blob: &[u8]) -> Option<Vec<MatDef>> {
    let g = Gr2::parse(blob)?;
    let (rs, ro) = g.root;

    let count = g.u32(rs, ro + 36)?;
    let arr = g.ptr(rs, ro + 40)?;
    if count == 0 || count > 4096 {
        return None;
    }
    let mut mats: Vec<((u32, u32), MatDef)> = Vec::new();
    for i in 0..count {
        let Some(mp) = g.ptr(arr.0, arr.1 + 8 * i) else { continue };
        if let Some(def) = g.material(mp) {
            mats.push((mp, def));
        }
    }
    if mats.is_empty() {
        return None;
    }

    let mut order: Vec<(u32, u32)> = Vec::new();
    if let (Some(mc), Some(marr)) = (g.u32(rs, ro + 84), g.ptr(rs, ro + 88)) {
        for i in 0..mc.min(4096) {
            let Some(mesh) = g.ptr(marr.0, marr.1 + 8 * i) else { continue };
            let (Some(bc), Some(barr)) = (g.u32(mesh.0, mesh.1 + 36), g.ptr(mesh.0, mesh.1 + 40)) else { continue };
            for k in 0..bc.min(256) {
                if let Some(mp) = g.ptr(barr.0, barr.1 + 8 * k) {
                    if !order.contains(&mp) {
                        order.push(mp);
                    }
                }
            }
        }
    }

    let mut out: Vec<MatDef> = Vec::new();
    for key in &order {
        if let Some(pos) = mats.iter().position(|(k, _)| k == key) {
            out.push(mats.remove(pos).1);
        }
    }
    out.extend(mats.into_iter().map(|(_, d)| d));
    Some(out)
}

pub fn tri_material_groups(blob: &[u8]) -> Option<Vec<(String, u32, u32)>> {
    let g = Gr2::parse(blob)?;
    let (rs, ro) = g.root;
    let mc = g.u32(rs, ro + 84)?;
    let marr = g.ptr(rs, ro + 88)?;
    let mut out = Vec::new();
    let mut base_tri = 0u32;
    for i in 0..mc.min(4096) {
        let Some(mesh) = g.ptr(marr.0, marr.1 + 8 * i) else { continue };
        let bc = g.u32(mesh.0, mesh.1 + 36)?;
        let barr = g.ptr(mesh.0, mesh.1 + 40)?;
        let names: Vec<String> = (0..bc.min(256))
            .map(|k| {
                g.ptr(barr.0, barr.1 + 8 * k)
                    .and_then(|mp| g.ptr(mp.0, mp.1))
                    .and_then(|p| g.cstr(p))
                    .unwrap_or_default()
            })
            .collect();
        let topo = g.ptr(mesh.0, mesh.1 + 28)?;
        let gc = g.u32(topo.0, topo.1)?;
        let garr = g.ptr(topo.0, topo.1 + 4)?;
        let mut mesh_tris = 0u32;
        for j in 0..gc.min(4096) {
            let o = garr.1 + 12 * j;
            let (mi, first, count) = (g.u32(garr.0, o)?, g.u32(garr.0, o + 4)?, g.u32(garr.0, o + 8)?);
            out.push((names.get(mi as usize).cloned().unwrap_or_default(), base_tri + first, count));
            mesh_tris = mesh_tris.max(first + count);
        }
        base_tri += mesh_tris;
    }
    if out.is_empty() { None } else { Some(out) }
}

pub fn mesh_uvs(blob: &[u8]) -> Option<(Vec<[f32; 2]>, Vec<[u32; 3]>)> {
    let g = Gr2::parse(blob)?;
    let (rs, ro) = g.root;
    let mc = g.u32(rs, ro + 84)?;
    let marr = g.ptr(rs, ro + 88)?;
    let (mut uvs, mut tris) = (Vec::new(), Vec::new());
    for i in 0..mc.min(4096) {
        let Some(mesh) = g.ptr(marr.0, marr.1 + 8 * i) else { continue };
        let vd = g.ptr(mesh.0, mesh.1 + 8)?;
        let vcount = g.u32(vd.0, vd.1 + 8)?;
        let tdef = g.ptr(vd.0, vd.1)?;
        let verts = g.ptr(vd.0, vd.1 + 12)?;

        let (mut off, mut uv_at) = (0u32, None);
        for k in 0..32 {
            let o = tdef.1 + 44 * k;
            let ty = g.u32(tdef.0, o)?;
            if ty == 0 { break; }
            let width = g.u32(tdef.0, o + 20)?.max(1);
            let name = g.ptr(tdef.0, o + 4).and_then(|p| g.cstr(p)).unwrap_or_default();
            let size = match ty { 10 | 19 | 20 => 4, 11..=14 => 1, 15..=18 | 21 => 2, _ => return None } * width;
            if name == "TextureCoord0" { uv_at = Some((off, ty)); }
            off += size;
        }
        let (uv_off, uv_ty) = uv_at?;
        let stride = off;
        let base = uvs.len() as u32;
        for n in 0..vcount {
            let o = verts.1 + n * stride + uv_off;
            let (u, v) = match uv_ty {
                10 => {
                    let b = g.at(verts.0, o, 8)?;
                    (f32::from_le_bytes(b[0..4].try_into().ok()?), f32::from_le_bytes(b[4..8].try_into().ok()?))
                }
                15 | 21 => {   // ESO: int16 pair, 1024 = 1.0
                    let b = g.at(verts.0, o, 4)?;
                    (i16::from_le_bytes([b[0], b[1]]) as f32 / 1024.0,
                     i16::from_le_bytes([b[2], b[3]]) as f32 / 1024.0)
                }
                _ => return None,
            };
            uvs.push([u, 1.0 - v]);
        }

        let topo = g.ptr(mesh.0, mesh.1 + 28)?;
        let (n32, n16) = (g.u32(topo.0, topo.1 + 12)?, g.u32(topo.0, topo.1 + 24)?);
        let mut flat: Vec<u32> = Vec::new();
        if n32 > 0 {
            let p = g.ptr(topo.0, topo.1 + 16)?;
            let b = g.at(p.0, p.1, n32 as usize * 4)?;
            flat.extend(b.chunks_exact(4).map(|c| u32::from_le_bytes(c.try_into().unwrap())));
        } else if n16 > 0 {
            let p = g.ptr(topo.0, topo.1 + 28)?;
            let b = g.at(p.0, p.1, n16 as usize * 2)?;
            flat.extend(b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]]) as u32));
        }
        tris.extend(flat.chunks_exact(3).map(|t| [base + t[0], base + t[1], base + t[2]]));
    }
    if uvs.is_empty() || tris.is_empty() { None } else { Some((uvs, tris)) }
}

fn patch_obj_uvs(text: &str, uv: &[[f32; 2]], gtris: &[[u32; 3]]) -> Option<String> {
    let vi = |tok: &str| -> Option<usize> { tok.split('/').next()?.parse::<usize>().ok()?.checked_sub(1) };
    let mut otris: Vec<[usize; 3]> = Vec::new();
    for l in text.lines().filter(|l| l.starts_with("f ")) {
        let c: Vec<&str> = l.split_whitespace().skip(1).collect();
        if c.len() != 3 { return None; }
        otris.push([vi(c[0])?, vi(c[1])?, vi(c[2])?]);
    }
    if otris.len() != gtris.len() { return None; }

    const PERMS: [[usize; 3]; 6] = [[0,1,2],[0,2,1],[1,2,0],[2,1,0],[1,0,2],[2,0,1]];
    let mut map: Option<HashMap<usize, u32>> = None;
    'p: for p in PERMS {
        let mut m: HashMap<usize, u32> = HashMap::new();
        for (o, g) in otris.iter().zip(gtris) {
            for c in 0..3 {
                if *m.entry(o[c]).or_insert(g[p[c]]) != g[p[c]] { continue 'p; }
            }
        }
        map = Some(m);
        break;
    }
    let map = map?;

    let nv = text.lines().filter(|l| l.starts_with("v ")).count();
    let mut out = String::new();
    let mut wrote_vt = false;
    for line in text.lines() {
        if line.starts_with("vt ") { continue; }
        if line.starts_with("f ") {
            if !wrote_vt {
                for n in 0..nv {
                    let t = map.get(&n).and_then(|g| uv.get(*g as usize)).copied().unwrap_or([0.0, 0.0]);
                    out.push_str(&format!("vt {:.6} {:.6}\n", t[0], t[1]));
                }
                wrote_vt = true;
            }
            let parts: Vec<String> = line.split_whitespace().skip(1).map(|tok| {
                let s: Vec<&str> = tok.split('/').collect();
                match s.get(2) { Some(n) => format!("{}/{}/{}", s[0], s[0], n), None => format!("{}/{}", s[0], s[0]) }
            }).collect();
            out.push_str(&format!("f {}\n", parts.join(" ")));
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    Some(out)
}



fn materials_from_triples(blob: &[u8]) -> Vec<MatDef> {
    texture_ref_sets(blob)
        .into_iter()
        .enumerate()
        .map(|(i, set)| MatDef {
            name: format!("material{}", i),
            alpha: true,
            maps: set
                .into_iter()
                .enumerate()
                .filter(|(_, fi)| *fi != 0)
                .map(|(r, fi)| MatMap { usage: SLOT_ROLES[r].to_string(), file_index: fi })
                .collect(),
        })
        .collect()
}


struct MatPng {
    name: String,
    png: [Option<String>; 3],
    alpha: bool,
}

fn role_of(usage: &str) -> Option<usize> {
    SLOT_ROLES.iter().position(|r| r.eq_ignore_ascii_case(usage.trim()))
}

fn norm_name(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
}

struct Ctx<'a> {
    o: &'a Options<'a>,
    prefer_file_index: Cell<bool>,
}

fn run_exporter(c: &Ctx, arch: u32, filter: u32) -> bool {
    let o = c.o;
    println!("Running: {} {} {} -a {} -f {}", o.exe, o.mnf, o.temp_dir, arch, filter);
    if o.dry_run {
        println!("  (dry run, not executing)");
        return true;
    }
    if let Err(e) = fs::create_dir_all(o.temp_dir) {
        eprintln!("  Failed to create '{}': {}", o.temp_dir, e);
        return false;
    }
    invalidate_index();
    match Command::new(o.exe)
        .arg(o.mnf)
        .arg(o.temp_dir)
        .arg("-a")
        .arg(arch.to_string())
        .arg("-f")
        .arg(filter.to_string())
        .output()
    {
        Ok(out) => {
            if !out.status.success() {
                eprintln!("  Warning: exporter exited with status {:?}", out.status.code());
                if !out.stderr.is_empty() {
                    eprintln!("{}", String::from_utf8_lossy(&out.stderr));
                }
            }
            true
        }
        Err(e) => {
            eprintln!("  Failed to launch exporter '{}': {}", o.exe, e);
            false
        }
    }
}

thread_local! {
    static STEM_INDEX: RefCell<Option<HashMap<u32, Vec<PathBuf>>>> = RefCell::new(None);
}

fn invalidate_index() {
    STEM_INDEX.with(|c| *c.borrow_mut() = None);
}

fn build_index(dir: &Path) -> HashMap<u32, Vec<PathBuf>> {
    let mut map: HashMap<u32, Vec<PathBuf>> = HashMap::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()).and_then(|s| s.parse::<u32>().ok()) {
                map.entry(stem).or_default().push(p);
            }
        }
    }
    map
}

fn find_by_stem(dir: &Path, stem: u32, exts: &[&str]) -> Option<PathBuf> {
    STEM_INDEX.with(|c| {
        let mut c = c.borrow_mut();
        let idx = c.get_or_insert_with(|| build_index(dir));
        let mut best: Option<(usize, &PathBuf)> = None;
        for p in idx.get(&stem)? {
            let ext = p.extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
            if let Some(rank) = exts.iter().position(|x| *x == ext) {
                if best.map_or(true, |(r, _)| rank < r) {
                    best = Some((rank, p));
                }
            }
        }
        best.map(|(_, p)| p.clone())
    })
}

fn fetch(c: &Ctx, row: &MnfRow, exts: &[&str]) -> Option<PathBuf> {
    let temp = Path::new(c.o.temp_dir);
    if let Some(p) = find_by_stem(temp, row.index, exts) {
        return Some(p);
    }
    if c.o.skip_export {
        return None;
    }

    let mut tries = vec![row.index, row.file_index];
    if row.index == row.file_index {
        tries.truncate(1);
    } else if c.prefer_file_index.get() {
        tries.reverse();
    }
    for filter in tries {
        if !run_exporter(c, row.arch, filter) {
            return None;
        }
        if c.o.dry_run {
            return None;
        }
        if let Some(p) = find_by_stem(temp, row.index, exts) {
            if filter == row.file_index && filter != row.index {
                c.prefer_file_index.set(true);
            }
            return Some(p);
        }
    }
    None
}

fn pick_row(label: &str, fi: u32, rows: Option<&Vec<MnfRow>>) -> Result<MnfRow, String> {
    match rows.map(|r| r.as_slice()) {
        None | Some([]) => Err(format!(
            "{} FileIndex 0x{:08X} ({}) not found in the manifest (looked for rows with Unk1 == 0).",
            label, fi, fi
        )),
        Some([only]) => Ok(only.clone()),
        Some(many) => {
            eprintln!(
                "Warning: {} rows share {} FileIndex 0x{:08X}; using Index {}.",
                many.len(),
                label,
                fi,
                many[0].index
            );
            Ok(many[0].clone())
        }
    }
}

fn copy_into(src: &Path, name: &str, out_dir: &Path) -> io::Result<String> {
    fs::copy(src, out_dir.join(name))?;
    Ok(name.to_string())
}

fn texture_out_name(mat_i: usize, tex_i: usize, role: Option<usize>, src: &Path) -> String {
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("dds").to_ascii_lowercase();
    match (mat_i, role) {
        (0, Some(r)) => format!("{}.{}", SLOT_ROLES[r], ext),
        _ => format!("extra{}_{}.{}", mat_i, tex_i, ext),
    }
}

fn fix_dds_linear_size(path: &Path) -> io::Result<()> {
    let mut d = fs::read(path)?;
    if d.len() < 128 || &d[0..4] != b"DDS " {
        return Ok(());
    }
    let block = match &d[84..88] {
        b"DXT1" => 8u32,
        b"DXT3" | b"DXT5" => 16u32,
        _ => return Ok(()),
    };
    let h = u32::from_le_bytes(d[12..16].try_into().unwrap());
    let w = u32::from_le_bytes(d[16..20].try_into().unwrap());
    let size = ((w + 3) / 4) * ((h + 3) / 4) * block;
    d[20..24].copy_from_slice(&size.to_le_bytes());
    fs::write(path, d)
}


fn plain_abs(p: &Path) -> io::Result<PathBuf> {
    let c = fs::canonicalize(p)?;
    let s = c.to_string_lossy().into_owned();
    Ok(match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => c,
    })
}


fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}









fn finish_obj(src: &Path, dest: &Path, model_mats: &[MatPng],
              groups: Option<&[(String, u32, u32)]>,
              uvs: Option<&(Vec<[f32; 2]>, Vec<[u32; 3]>)>) -> io::Result<usize> {
    let mut text = fs::read_to_string(src)?;
    if let Some((uv, gt)) = uvs {
        match patch_obj_uvs(&text, uv, gt) {
            Some(t) => { println!("  UVs rewritten from the .gr2"); text = t; }
            None => eprintln!("  Warning: could not match the OBJ to the .gr2 triangles; keeping Noesis' UVs."),
        }
    }
    let stem = dest.file_stem().and_then(|s| s.to_str()).unwrap_or("model").to_string();
    let mtl_file = format!("{}.mtl", stem);

    // Per-triangle material name from the .gr2 groups; only trusted if the triangle counts agree.
    let mut tri_mat: Vec<&str> = Vec::new();
    if let Some(gs) = groups {
        for (name, first, count) in gs {
            let end = (first + count) as usize;
            if tri_mat.len() < end { tri_mat.resize(end, ""); }
            for t in *first..(first + count) { tri_mat[t as usize] = name.as_str(); }
        }
    }
    let obj_tris: usize = text.lines()
        .filter(|l| l.starts_with("f "))
        .map(|l| l.split_whitespace().count().saturating_sub(3)) // verts - 2
        .sum();
    let by_group = !tri_mat.is_empty() && tri_mat.len() == obj_tris && tri_mat.iter().all(|n| !n.is_empty());
    if !by_group && groups.is_some() {
        eprintln!("  Warning: OBJ has {} triangles but .gr2 groups cover {}; keeping Noesis' usemtl lines.",
                  obj_tris, tri_mat.len());
    }

    let mut mats: Vec<String> = Vec::new();
    let mut body = String::new();
    let (mut tri, mut cur) = (0usize, String::new());
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("mtllib ") { continue; }
        if by_group {
            if l.starts_with("usemtl ") { continue; }
            if l.starts_with("f ") {
                let m = tri_mat[tri];
                if m != cur {
                    cur = m.to_string();
                    body.push_str(&format!("usemtl {}\n", cur));
                    if !mats.contains(&cur) { mats.push(cur.clone()); }
                }
                tri += l.split_whitespace().count().saturating_sub(3);
            }
        } else if let Some(r) = l.strip_prefix("usemtl ") {
            let m = r.trim().to_string();
            if !mats.contains(&m) { mats.push(m); }
        }
        body.push_str(line);
        body.push('\n');
    }
    if mats.is_empty() {
        mats.push("eso_material".to_string());
        body = format!("usemtl eso_material\n{}", body);
    }
    fs::write(dest, format!("mtllib {}\n{}", mtl_file, body))?;

    // Pair every usemtl name with one of the model's materials.
    let mut assigned: Vec<Option<usize>> = vec![None; mats.len()];
    let mut used = vec![false; model_mats.len()];
    let mut by_name = vec![false; mats.len()];
    for (i, m) in mats.iter().enumerate() {
        let key = norm_name(m);
        if key.is_empty() {
            continue;
        }
        if let Some(j) = (0..model_mats.len()).find(|j| !used[*j] && norm_name(&model_mats[*j].name) == key) {
            assigned[i] = Some(j);
            used[j] = true;
            by_name[i] = true;
        }
    }
    for i in 0..mats.len() {
        if assigned[i].is_none() {
            if let Some(j) = (0..model_mats.len()).find(|j| !used[*j]) {
                assigned[i] = Some(j);
                used[j] = true;
            }
        }
    }

    let mut mtl = String::new();
    for (mi, m) in mats.iter().enumerate() {
        let j = assigned[mi].or(model_mats.len().checked_sub(1));
        let slot = j.and_then(|j| model_mats.get(j));
        let tex = |role: usize| slot.and_then(|s| s.png[role].as_deref());
        match slot {
            Some(s) => println!(
                "  usemtl '{}' -> material '{}' ({}), diffuse {}, normal {}, specular {}",
                m,
                s.name,
                if by_name[mi] { "matched by name" } else { "matched by order" },
                tex(0).unwrap_or("none"),
                tex(1).unwrap_or("none"),
                tex(2).unwrap_or("none"),
            ),
            None => println!("  usemtl '{}' -> no textures", m),
        }
        mtl.push_str(&format!("newmtl {}\nKa 1.000 1.000 1.000\nKd 1.000 1.000 1.000\nKs 0.200 0.200 0.200\nNs 20.0\nd 1.0\nillum 2\n", m));
        if let Some(p) = tex(0) {
            mtl.push_str(&format!("map_Kd {}\n", p));
            // Only use the diffuse alpha as opacity when the material asks for it; otherwise the
            // alpha channel holds unrelated data and would make solid surfaces look see-through.
            if slot.map_or(false, |s| s.alpha) {
                mtl.push_str(&format!("map_d {}\n", p));
            }
        }
        if let Some(p) = tex(1) { mtl.push_str(&format!("map_Bump {}\nbump {}\n", p, p)); }
        if let Some(p) = tex(2) { mtl.push_str(&format!("map_Ks {}\n", p)); }
        mtl.push('\n');
    }
    fs::write(dest.with_file_name(&mtl_file), mtl)?;
    Ok(mats.len())
}







fn export_model(noesis: &str, model: &Path, dest: &Path) -> Result<(), String> {
    let noesis_path = plain_abs(Path::new(noesis))
        .map_err(|e| format!("Noesis not found at '{}': {}", noesis, e))?;
    let noesis_dir = noesis_path
        .parent()
        .ok_or_else(|| format!("Cannot find the folder of '{}'", noesis_path.display()))?
        .to_path_buf();

    // A leftover output file would make a failed run look successful.
    let _ = fs::remove_file(dest);

    println!("Running: {} ?cmode {} {}", noesis_path.display(), model.display(), dest.display());
    let direct = Command::new(&noesis_path)
        .current_dir(&noesis_dir)
        .arg("?cmode")
        .arg(model)
        .arg(dest)
        .output();
    let mut log = String::new();
    match direct {
        Ok(out) => {
            log.push_str(&String::from_utf8_lossy(&out.stdout));
            log.push_str(&String::from_utf8_lossy(&out.stderr));
        }
        Err(e) => log.push_str(&format!("direct launch failed: {}\n", e)),
    }
    if dest.exists() {
        return Ok(());
    }

    println!("  Direct launch produced no output file; retrying through PowerShell Start-Process...");
    let script = format!(
        "Start-Process -FilePath {} -WorkingDirectory {} -Wait -ArgumentList '?cmode',{},{}",
        ps_quote(&noesis_path.to_string_lossy()),
        ps_quote(&noesis_dir.to_string_lossy()),
        ps_quote(&format!("\"{}\"", model.display())),
        ps_quote(&format!("\"{}\"", dest.display())),
    );
    match Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script.as_str()])
        .output()
    {
        Ok(out) => {
            log.push_str(&String::from_utf8_lossy(&out.stdout));
            log.push_str(&String::from_utf8_lossy(&out.stderr));
        }
        Err(e) => log.push_str(&format!("powershell launch failed: {}\n", e)),
    }
    if dest.exists() {
        return Ok(());
    }
    Err(format!(
        "Noesis ran but '{}' was not created. Is NOESIS ({}) the 64-bit exe, and does the GUI open the .gr2?\n{}",
        dest.display(),
        noesis_path.display(),
        log
    ))
}

pub fn run(model_fi: u32, o: &Options) -> Result<Summary, String> {
    let ctx = Ctx { o, prefer_file_index: Cell::new(false) };

    let mut want = HashSet::new();
    want.insert(model_fi);
    let found = lookup_file_indexes(o.manifest, &want)
        .map_err(|e| format!("Failed to read manifest '{}': {}", o.manifest, e))?;
    let model = pick_row("Model", model_fi, found.get(&model_fi))?;
    println!(
        "Model: FileIndex 0x{:08X} -> Index {} (arch {}, {} bytes)",
        model_fi, model.index, model.arch, model.size
    );

    let model_path = fetch(&ctx, &model, MODEL_EXTS);
    if o.dry_run {
        println!("\nDry run: stopping here (texture list needs the model file).");
        return Ok(Summary { out_dir: PathBuf::new(), textures_found: 0, textures_missing: 0 });
    }
    let model_path = model_path.ok_or_else(|| {
        format!(
            "Model file '{}.gr2' not found under '{}'. Did the export run (see --skip-export)?",
            model.index, o.temp_dir
        )
    })?;
    let blob = fs::read(&model_path).map_err(|e| format!("Failed to read '{}': {}", model_path.display(), e))?;
    if texture_refs(&blob).is_empty() {
        return Err(format!(
            "No texture reference records found in '{}'. Is this a model .gr2 with the usual header table?",
            model_path.display()
        ));
    }
    let mats: Vec<MatDef> = match parse_materials(&blob) {
        Some(m) => m,
        None => {
            eprintln!("  Warning: could not read the material table of this .gr2; guessing material groups from the texture records (3 maps per material).");
            materials_from_triples(&blob)
        }
    };
    println!("Model has {} material(s):", mats.len());
    for m in &mats {
        let maps: Vec<String> = m.maps.iter().map(|x| format!("{}=0x{:x}", x.usage, x.file_index)).collect();
        println!("  '{}': {}", m.name, maps.join(", "));
    }

    let wanted: HashSet<u32> = mats.iter().flat_map(|m| m.maps.iter().map(|x| x.file_index)).collect();
    let rows = lookup_file_indexes(o.manifest, &wanted)
        .map_err(|e| format!("Failed to read manifest '{}': {}", o.manifest, e))?;

    // Everything goes into one flat folder, keeping the archive's own file names:
    //   <index>.gr2, <textureIndex>.dds / .png, <index>.obj, <index>.mtl
    // Textures shared between models therefore just share a file instead of colliding.
    let out_dir = o.out_root.to_path_buf();
    fs::create_dir_all(&out_dir)
        .map_err(|e| format!("Failed to create '{}': {}", out_dir.display(), e))?;
    let model_name = copy_into(&model_path, &format!("{}.gr2", model.index), &out_dir)
        .map_err(|e| format!("Failed to copy model: {}", e))?;
    println!("Model -> {}", out_dir.join(&model_name).display());

    // Noesis' GR2 plugin wants the textures next to the model under fixed role names
    // (diffuse/normal/specular), so it gets a private scratch folder.
    let scratch = out_dir.join(format!("_noesis_tmp_{}", model.index));
    fs::create_dir_all(&scratch)
        .map_err(|e| format!("Failed to create '{}': {}", scratch.display(), e))?;
    fs::copy(out_dir.join(&model_name), scratch.join(&model_name))
        .map_err(|e| format!("Failed to copy model to scratch: {}", e))?;

    // PNG name per material, per role (diffuse/normal/specular), used by the .mtl.
    let mut mat_png: Vec<MatPng> = mats
        .iter()
        .map(|m| MatPng { name: m.name.clone(), png: [None, None, None], alpha: m.alpha })
        .collect();

    let (mut ok, mut missing) = (0usize, 0usize);
    let slots: Vec<(usize, usize, String, u32)> = mats
        .iter()
        .enumerate()
        .flat_map(|(mi, m)| m.maps.iter().enumerate().map(move |(ti, x)| (mi, ti, x.usage.clone(), x.file_index)))
        .collect();
    for (mat_i, tex_i, usage, fi) in &slots {
        let (mat_i, tex_i, fi) = (*mat_i, *tex_i, fi);
        let role = role_of(usage);
        let slot = format!("material {} map '{}'", mat_i, usage);
        let row = match pick_row("Texture", *fi, rows.get(fi)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  slot {}: {}", slot, e);
                missing += 1;
                continue;
            }
        };
        match fetch(&ctx, &row, TEXTURE_EXTS) {
            Some(src) => {
                // keep the original file name (e.g. 608841.dds)
                let orig = src.file_name().and_then(|n| n.to_str()).unwrap_or("texture.dds").to_ascii_lowercase();
                match copy_into(&src, &orig, &out_dir) {
                    Ok(name) => {
                        if name.ends_with(".dds") {
                            if let Err(e) = fix_dds_linear_size(&out_dir.join(&name)) {
                                eprintln!("  Warning: could not fix DDS header of {}: {}", name, e);
                            }
                            let png_name = name.replace(".dds", ".png");
                            match image::open(out_dir.join(&name)) {
                                Ok(img) => match img.save(out_dir.join(&png_name)) {
                                    Ok(()) => {
                                        if let Some(r) = role {
                                            if mat_png[mat_i].png[r].is_none() {
                                                mat_png[mat_i].png[r] = Some(png_name.clone());
                                            }
                                        }
                                    }
                                    Err(e) => eprintln!("  Warning: PNG save failed for {}: {}", name, e),
                                },
                                Err(e) => eprintln!("  Warning: could not decode {} to PNG: {}", name, e),
                            }
                        }
                        let _ = fs::copy(out_dir.join(&name), scratch.join(texture_out_name(mat_i, tex_i, role, &src)));
                        println!("  '{}' {}: 0x{:08X} -> {}", mats[mat_i].name, usage, fi, name);
                        ok += 1;
                    }
                    Err(e) => {
                        eprintln!("  slot {}: copy failed: {}", slot, e);
                        missing += 1;
                    }
                }
            }
            None => {
                eprintln!("  slot {}: 0x{:08X} (Index {}) was not produced by the exporter", slot, fi, row.index);
                missing += 1;
            }
        }
    }

    let result = plain_abs(&scratch.join(&model_name)).and_then(|src| {
        let obj = src.with_extension("obj");
        export_model(o.noesis, &src, &obj).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        Ok(obj)
    });
    match result {
        Ok(scratch_obj) => {
            let final_obj = out_dir.join(format!("{}.obj", model.index));
            let groups = tri_material_groups(&blob);
            let uvs = mesh_uvs(&blob);
            match finish_obj(&scratch_obj, &final_obj, &mat_png, groups.as_deref(), uvs.as_ref()) {
                Ok(n) => println!("OBJ -> {} ({} material(s), textures linked)", final_obj.display(), n),
                Err(e) => eprintln!("Warning: could not finish obj: {}", e),
            }
        }
        Err(e) => eprintln!("Warning: obj export failed: {}", e),
    }
    let _ = fs::remove_dir_all(&scratch);

    if !o.keep_temp && !o.skip_export {
        invalidate_index();
        if let Err(e) = fs::remove_dir_all(o.temp_dir) {
            eprintln!("Warning: failed to remove temporary directory '{}': {}", o.temp_dir, e);
        }
    }

    Ok(Summary { out_dir, textures_found: ok, textures_missing: missing })
}

fn prefetch(ids: &[u32], o: &Options) -> Result<(), String> {
    let want: HashSet<u32> = ids.iter().copied().collect();
    let rows = lookup_file_indexes(o.manifest, &want)
        .map_err(|e| format!("Failed to read manifest '{}': {}", o.manifest, e))?;

    let mut keep: HashSet<u32> = HashSet::new();
    let mut model_idx: Vec<u32> = Vec::new();
    let mut model_archs: BTreeSet<u32> = BTreeSet::new();
    for id in ids {
        if let Some(r) = rows.get(id).and_then(|v| v.first()) {
            keep.insert(r.index);
            model_idx.push(r.index);
            model_archs.insert(r.arch);
        }
    }

    let dump_and_prune = |arch: u32, keep: &HashSet<u32>| {
        println!("\nExporting archive {} (one pass)...", arch);
        if !export::export_archive(o.exe, o.mnf, o.temp_dir, arch, false) {
            eprintln!("Archive {} export reported an error; check output above.", arch);
        }
        let (kept, removed) = export::cleanup_export_dir(o.temp_dir, keep, false);
        println!("  Pruned archive {}: kept {} files total, removed {}.", arch, kept, removed);
        invalidate_index();
    };

    for a in &model_archs {
        dump_and_prune(*a, &keep);
    }

    let temp = Path::new(o.temp_dir);
    let mut tex_fis: HashSet<u32> = HashSet::new();
    for idx in &model_idx {
        let Some(p) = find_by_stem(temp, *idx, MODEL_EXTS) else { continue };
        let Ok(blob) = fs::read(&p) else { continue };
        let mats = parse_materials(&blob).unwrap_or_else(|| materials_from_triples(&blob));
        for m in &mats {
            for x in &m.maps {
                tex_fis.insert(x.file_index);
            }
        }
    }
    let tex_rows = lookup_file_indexes(o.manifest, &tex_fis)
        .map_err(|e| format!("Failed to read manifest '{}': {}", o.manifest, e))?;
    let mut tex_archs: BTreeSet<u32> = BTreeSet::new();
    for fi in &tex_fis {
        if let Some(r) = tex_rows.get(fi).and_then(|v| v.first()) {
            keep.insert(r.index);
            tex_archs.insert(r.arch);
        }
    }
    println!("\n{} model(s) reference {} distinct texture(s) in {} archive(s).", model_idx.len(), tex_fis.len(), tex_archs.len());

    for a in &tex_archs {
        dump_and_prune(*a, &keep);
    }
    Ok(())
}

pub fn run_batch(ids: &[u32], o: &Options) -> Vec<(u32, Result<Summary, String>)> {
    let mut prefetched = false;
    if !o.dry_run && !o.skip_export && !ids.is_empty() {
        match prefetch(ids, o) {
            Ok(()) => prefetched = true,
            Err(e) => eprintln!("Batch export failed ({}); falling back to per-file extraction.", e),
        }
    }
    let inner = Options {
        manifest: o.manifest,
        exe: o.exe,
        mnf: o.mnf,
        temp_dir: o.temp_dir,
        out_root: o.out_root,
        dry_run: o.dry_run,
        skip_export: o.skip_export || prefetched,
        keep_temp: true,
        noesis: o.noesis,
    };
    let mut results = Vec::with_capacity(ids.len());
    for (n, id) in ids.iter().enumerate() {
        println!("\n=== Model {}/{}: 0x{:x} ===", n + 1, ids.len(), id);
        results.push((*id, run(*id, &inner)));
    }
    if !o.keep_temp && !o.skip_export && !o.dry_run {
        invalidate_index();
        if let Err(e) = fs::remove_dir_all(o.temp_dir) {
            eprintln!("Warning: failed to remove temporary directory '{}': {}", o.temp_dir, e);
        }
    }
    results
}