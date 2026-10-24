//! Extract one model (by MNF FileIndex) together with every texture it references.
//!
//! How the link works:
//!   * The model's .gr2 starts with a table of 0x60-byte records. Each record begins with the
//!     fixed byte pattern `REF_SIG` and holds the referenced texture's MNF FileIndex as a
//!     little-endian u32 at +0x38.
//!   * Models and textures are plain files, i.e. manifest rows with Unk1 == 0.
//!
//! Variant meshes (see `VARIANT_RULES`):
//!   Some models carry several alternative meshes in one .gr2 that occupy the same space and of which
//!   the game shows only ONE (keep pieces hold a banner/shield set per alliance: BlueS / RedS / YellowS).
//!   Exporting them together stacks all variants in one OBJ, so for a model that matches a rule we write
//!   one OBJ per variant (`<index>_<label>.obj`). Models that *look* like they have such a set but are not
//!   covered by a rule produce a console warning so a rule can be added.
//!
//!   Overlapping meshes that no rule covers are detected automatically (same space plus the same materials or bone bindings)
//!   and are ALSO exported one OBJ per mesh, named `<index>_<meshname>.obj`. Pass `--no-auto-variants` (or call
//!   `set_auto_variants(false)`) to get the old behaviour: warn and export them stacked in one OBJ.
//!
//! Material assignment:
//!   Noesis writes the OBJ meshes in a different order than the .gr2 stores them (keep pieces: .gr2 order is
//!   BlueS, RedS, YellowS, <body>; the OBJ is <body>, YellowS, RedS, BlueS). Materials and UVs are therefore
//!   matched through the OBJ group names (`<mesh>_<group>`), never through a global triangle position.
//!

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use crate::archive::{self, Archive};
use crate::out::Scratch;

const REF_SIG: [u8; 28] = [
    0x08, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00,
    0x08, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00,
];
const REF_FILE_INDEX_OFFSET: usize = 0x38;
const REF_SCAN_LIMIT: usize = 1 << 20;


const SLOT_ROLES: &[&str] = &["diffuse", "normal", "specular"];

pub struct Options<'a> {
    pub archive: &'a Archive,
    pub manifest: &'a RowMap,
    pub cache: &'a ReadCache,
    pub out_root: &'a Path,
    pub dry_run: bool,
    pub noesis: &'a str,
}

/// Bytes of entries read so far, shared by everything in one command (parts of an item share textures).
/// Bounded: `trim` drops everything once the limit is passed, so a long batch cannot grow without end.
#[derive(Default)]
pub struct ReadCache {
    map: RefCell<HashMap<u32, Rc<Vec<u8>>>>,
    bytes: Cell<usize>,
}

impl ReadCache {
    pub fn new() -> Self { Self::default() }

    /// Reads all of `indices` that are not cached yet in one pass over the archive (archive/offset order).
    pub fn prefetch(&self, archive: &Archive, indices: &[u32]) {
        let missing: Vec<u32> = {
            let m = self.map.borrow();
            indices.iter().copied().filter(|i| !m.contains_key(i)).collect()
        };
        if missing.is_empty() { return; }
        archive.read_each(&missing, |e, r| match r {
            Ok(b) => self.insert(e.index, b),
            Err(msg) => eprintln!("  Warning: {}", msg),
        });
    }

    fn insert(&self, index: u32, b: Vec<u8>) {
        self.bytes.set(self.bytes.get() + b.len());
        self.map.borrow_mut().insert(index, Rc::new(b));
    }

    pub fn get(&self, archive: &Archive, index: u32) -> Result<Rc<Vec<u8>>, String> {
        if let Some(b) = self.map.borrow().get(&index) { return Ok(b.clone()); }
        let b = archive.read(index)?;
        self.insert(index, b);
        Ok(self.map.borrow()[&index].clone())
    }

    pub fn trim(&self, limit_bytes: usize) {
        if self.bytes.get() > limit_bytes {
            self.map.borrow_mut().clear();
            self.bytes.set(0);
        }
    }
}

const CACHE_LIMIT: usize = 768 * 1024 * 1024;

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

pub type RowMap = HashMap<u32, Vec<MnfRow>>;

/// Entries with `unknown1 == 0`, keyed by `file_index`: how texture/model references (FileIndex) are resolved.
/// Several entries can share a FileIndex; callers take the first (see `pick_row`).
pub fn build_rows(entries: &[eso_common::mnf::MnfFileEntry]) -> RowMap {
    let mut out: RowMap = HashMap::new();
    for e in entries {
        if e.unknown1 != 0 {
            continue;
        }
        out.entry(e.file_index).or_default().push(MnfRow {
            index: e.index,
            id1: e.id1,
            file_index: e.file_index,
            size: e.size,
            arch: e.archive_index as u32,
        });
    }
    out
}

pub fn lookup_file_indexes(all: &RowMap, wanted: &HashSet<u32>) -> HashMap<u32, Vec<MnfRow>> {
    wanted.iter().filter_map(|k| all.get(k).map(|v| (*k, v.clone()))).collect()
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

/// How the game draws a material's diffuse alpha channel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlphaMode {
    /// Opaque: the alpha channel holds unrelated data and must be ignored.
    None,
    /// Cutout (alpha test): a pixel is either drawn or discarded. Foliage, trims, fences.
    Mask,
    /// Blended (true transparency): glass and the like.
    Blend,
}

impl AlphaMode {
    /// Name written into the .mtl as `# alpha_mode <name>` for the Blender scripts to read.
    pub fn mtl_name(self) -> Option<&'static str> {
        match self {
            AlphaMode::None => None,
            AlphaMode::Mask => Some("mask"),
            AlphaMode::Blend => Some("blend"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct MatDef {
    pub name: String,
    pub maps: Vec<MatMap>,
    pub alpha: AlphaMode,
    /// Where the material lives in the .gr2. Two materials can share a name (with different textures),
    /// but never a key.
    pub key: (u32, u32),
}

/// A set of meshes stored in one .gr2 that occupy the same space and of which the game shows only one.
pub struct VariantRule {
    pub name: &'static str,
    /// (mesh name inside the .gr2, label used in file names and for `--alliance`)
    pub members: &'static [(&'static str, &'static str)],
}

/// Add new rules here when the console warns about overlapping meshes ("look like alternative versions").
/// A model matching a rule (>= 2 of its meshes present) is exported once per label as `<index>_<label>.obj`.
pub const VARIANT_RULES: &[VariantRule] = &[
    VariantRule {
        name: "alliance banners/shields",
        members: &[("BlueS", "daggerfall"), ("RedS", "ebonheart"), ("YellowS", "aldmeri")],
    },
];

pub fn variant_labels() -> Vec<&'static str> {
    let mut v: Vec<&'static str> = Vec::new();
    for r in VARIANT_RULES {
        for (_, l) in r.members {
            if !v.contains(l) {
                v.push(*l);
            }
        }
    }
    v
}

/// OBJ files already written for a model: the plain `<index>.obj` and/or one `<index>_<label>.obj` per variant.
pub fn exported_objs(dir: &Path, index: u32) -> Vec<(Option<&'static str>, String)> {
    let mut v = Vec::new();
    let base = format!("{}.obj", index);
    if dir.join(&base).exists() {
        v.push((None, base));
    }
    for label in variant_labels() {
        let n = format!("{}_{}.obj", index, label);
        if dir.join(&n).exists() {
            v.push((Some(label), n));
        }
    }
    // Variants found automatically (named after their mesh) aren't in VARIANT_RULES, so look for them on disk.
    let prefix = format!("{}_", index);
    let mut extra: Vec<(String, String)> = Vec::new();
    if let Ok(rd) = fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let Some(label) = name.strip_prefix(&prefix).and_then(|r| r.strip_suffix(".obj")) else { continue };
            if label.is_empty() || v.iter().any(|(l, _)| *l == Some(label)) {
                continue;
            }
            extra.push((label.to_string(), name.clone()));
        }
    }
    extra.sort();
    for (label, name) in extra {
        v.push((Some(intern_label(&label)), name));
    }
    v
}

/// `exported_objs` hands out `&'static str` labels; auto-detected ones are interned so that stays true.
fn intern_label(s: &str) -> &'static str {
    static POOL: Mutex<Vec<&'static str>> = Mutex::new(Vec::new());
    let mut pool = POOL.lock().unwrap();
    if let Some(x) = pool.iter().find(|x| **x == s) {
        return *x;
    }
    let leaked: &'static str = Box::leak(s.to_string().into_boxed_str());
    pool.push(leaked);
    leaked
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
    
    /// `fill` = diffuse/normal/specular file indexes to use for map slots that carry no texture of their
    /// own (creature models: their textures live in the creature texture table, not in the .gr2).
    fn material(&self, m: (u32, u32), fill: Option<[u32; 3]>) -> Option<MatDef> {
        let (s, o) = m;
        let name = self.ptr(s, o).and_then(|p| self.cstr(p)).unwrap_or_default();
        let n = self.u32(s, o + 8)?;
        if n > 64 || (n == 0 && fill.is_none()) {
            return None;
        }
        let mut out = Vec::new();
        if let Some(maps) = self.ptr(s, o + 12) {
            for k in 0..n {
                let mo = maps.1 + 16 * k;
                let usage = self.ptr(maps.0, mo).and_then(|p| self.cstr(p)).unwrap_or_default();
                let own = (|| {
                    let sub = self.ptr(maps.0, mo + 8)?;
                    let tex = self.ptr(sub.0, sub.1 + 20)?;
                    self.u32(tex.0, tex.1 + 92)
                })()
                .unwrap_or(0);
                let fi = match (own, fill) {
                    (0, Some(f)) => role_of(&usage).map(|r| f[r]).unwrap_or(0),
                    _ => own,
                };
                if fi != 0 {
                    out.push(MatMap { usage, file_index: fi });
                }
            }
        }
        if let Some(f) = fill {
            for (r, role) in SLOT_ROLES.iter().enumerate() {
                if f[r] != 0 && !out.iter().any(|x| x.usage.trim().eq_ignore_ascii_case(role)) {
                    out.push(MatMap { usage: role.to_string(), file_index: f[r] });
                }
            }
        }
        if out.is_empty() {
            return None;
        }
        let alpha = self.ext_alpha(m).unwrap_or(AlphaMode::Blend);
        Some(MatDef { name, maps: out, alpha, key: m })
    }

    /// How the game draws this material's alpha, from two signals in the material's properties:
    /// * the `alpha` mode value: 1 = opaque, 3 = cutout, 5 = blended (seen on Water/BadFurnTrim,
    ///   Trim001, glass respectively). Some blended materials such as `glass` leave both flags at 0, so
    ///   this is the primary signal.
    /// * the `bAlphaMaskEnable` / `bAlphaTransparency` flags: used when the mode value is missing or 1.
    /// `None` means no alpha property was found at all.
    fn ext_alpha(&self, m: (u32, u32)) -> Option<AlphaMode> {
        let (s, o) = m;
        let (ts, mut t) = self.ptr(s, o + 28)?;
        let (os, oo) = self.ptr(s, o + 36)?;
        let (mut pos, mut seen) = (0u32, false);
        let (mut mask, mut trans) = (false, false);
        let mut mode: Option<u32> = None;
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
                    match nm.as_str() {
                        "bAlphaMaskEnable" => {
                            seen = true;
                            mask = self.u32(os, oo + pos)? != 0;
                        }
                        "bAlphaTransparency" => {
                            seen = true;
                            trans = self.u32(os, oo + pos)? != 0;
                        }
                        "alpha" => mode = self.u32(os, oo + pos),
                        _ => {}
                    }
                }
            }
            pos += size;
            t += 44;
        }
        if !seen && mode.is_none() {
            return None;
        }
        Some(match mode {
            Some(v) if v >= 5 => AlphaMode::Blend,
            Some(2..=4) => AlphaMode::Mask,
            _ if trans => AlphaMode::Blend,
            _ if mask => AlphaMode::Mask,
            _ => AlphaMode::None,
        })
    }

    fn at(&self, s: u32, o: u32, n: usize) -> Option<&'a [u8]> {
        let (d, sz) = *self.secs.get(s as usize)?;
        let o = o as usize;
        if o.checked_add(n)? > sz { return None; }
        self.b.get(d + o..d + o + n)
    }
}




pub fn parse_materials(blob: &[u8]) -> Option<Vec<MatDef>> {
    parse_materials_with(blob, None)
}

pub fn parse_materials_with(blob: &[u8], fill: Option<[u32; 3]>) -> Option<Vec<MatDef>> {
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
        if let Some(def) = g.material(mp, fill) {
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

/// One topology group of one mesh: a run of triangles that share a material.
#[derive(Debug, Clone)]
pub struct TriGroup {
    pub mesh: String,
    /// Group number inside the mesh. Noesis names its OBJ groups `<mesh>_<index>`.
    pub index: usize,
    /// Identifies the material inside the .gr2 (see `MatDef::key`).
    pub mat_key: (u32, u32),
    pub mat_name: String,
    /// First triangle, counted over every mesh in .gr2 order.
    pub first: u32,
    pub count: u32,
}

pub fn tri_material_groups(blob: &[u8]) -> Option<Vec<TriGroup>> {
    let g = Gr2::parse(blob)?;
    let (rs, ro) = g.root;
    let mc = g.u32(rs, ro + 84)?;
    let marr = g.ptr(rs, ro + 88)?;
    let mut out = Vec::new();
    let mut base_tri = 0u32;
    for i in 0..mc.min(4096) {
        let Some(mesh) = g.ptr(marr.0, marr.1 + 8 * i) else { continue };
        let mesh_name = g.ptr(mesh.0, mesh.1).and_then(|p| g.cstr(p)).unwrap_or_default();
        let bc = g.u32(mesh.0, mesh.1 + 36)?;
        let barr = g.ptr(mesh.0, mesh.1 + 40)?;
        let topo = g.ptr(mesh.0, mesh.1 + 28)?;
        let gc = g.u32(topo.0, topo.1)?;
        let garr = g.ptr(topo.0, topo.1 + 4)?;
        let mut mesh_tris = 0u32;
        for j in 0..gc.min(4096) {
            let o = garr.1 + 12 * j;
            let (mi, first, count) = (g.u32(garr.0, o)?, g.u32(garr.0, o + 4)?, g.u32(garr.0, o + 8)?);
            let key = if mi < bc.min(256) { g.ptr(barr.0, barr.1 + 8 * mi) } else { None };
            let mat_name = key.and_then(|mp| g.ptr(mp.0, mp.1)).and_then(|p| g.cstr(p)).unwrap_or_default();
            out.push(TriGroup {
                mesh: mesh_name.clone(),
                index: j as usize,
                mat_key: key.unwrap_or((u32::MAX, 0)),
                mat_name,
                first: base_tri + first,
                count,
            });
            mesh_tris = mesh_tris.max(first + count);
        }
        base_tri += mesh_tris;
    }
    if out.is_empty() { None } else { Some(out) }
}

/// What we know about one mesh, used to spot sets of alternative (mutually exclusive) meshes.
#[derive(Debug, Clone)]
pub struct MeshInfo {
    pub name: String,
    pub bones: Vec<String>,
    pub bbox: Option<([f32; 3], [f32; 3])>,
    pub materials: Vec<String>,
}

fn mesh_bbox(g: &Gr2, mesh: (u32, u32)) -> Option<([f32; 3], [f32; 3])> {
    let vd = g.ptr(mesh.0, mesh.1 + 8)?;
    let vcount = g.u32(vd.0, vd.1 + 8)?;
    let tdef = g.ptr(vd.0, vd.1)?;
    let verts = g.ptr(vd.0, vd.1 + 12)?;
    let (mut off, mut pos_at) = (0u32, None);
    for k in 0..32 {
        let o = tdef.1 + 44 * k;
        let ty = g.u32(tdef.0, o)?;
        if ty == 0 { break; }
        let width = g.u32(tdef.0, o + 20)?.max(1);
        let name = g.ptr(tdef.0, o + 4).and_then(|p| g.cstr(p)).unwrap_or_default();
        let size = match ty { 10 | 19 | 20 => 4, 11..=14 => 1, 15..=18 | 21 => 2, _ => return None } * width;
        if name == "Position" && ty == 10 && width >= 3 { pos_at = Some(off); }
        off += size;
    }
    let pos_off = pos_at?;
    let stride = off;
    let (mut mn, mut mx) = ([f32::MAX; 3], [f32::MIN; 3]);
    for n in 0..vcount.min(1 << 22) {
        let b = g.at(verts.0, verts.1 + n * stride + pos_off, 12)?;
        for a in 0..3 {
            let v = f32::from_le_bytes(b[a * 4..a * 4 + 4].try_into().ok()?);
            mn[a] = mn[a].min(v);
            mx[a] = mx[a].max(v);
        }
    }
    if vcount == 0 { None } else { Some((mn, mx)) }
}

/// Per-mesh info plus the skeleton's bone names.
pub fn mesh_infos(blob: &[u8]) -> Option<(Vec<MeshInfo>, Vec<String>)> {
    let g = Gr2::parse(blob)?;
    let (rs, ro) = g.root;
    let mc = g.u32(rs, ro + 84)?;
    let marr = g.ptr(rs, ro + 88)?;
    let mut out = Vec::new();
    for i in 0..mc.min(4096) {
        let Some(mesh) = g.ptr(marr.0, marr.1 + 8 * i) else { continue };
        let name = g.ptr(mesh.0, mesh.1).and_then(|p| g.cstr(p)).unwrap_or_default();
        let mut materials = Vec::new();
        if let (Some(bc), Some(barr)) = (g.u32(mesh.0, mesh.1 + 36), g.ptr(mesh.0, mesh.1 + 40)) {
            for k in 0..bc.min(256) {
                if let Some(mp) = g.ptr(barr.0, barr.1 + 8 * k) {
                    materials.push(g.ptr(mp.0, mp.1).and_then(|p| g.cstr(p)).unwrap_or_default());
                }
            }
        }
        // BoneBinding records are 44 bytes: name pointer, OBB min/max, triangle index list.
        let mut bones = Vec::new();
        if let (Some(nb), Some(arr)) = (g.u32(mesh.0, mesh.1 + 48), g.ptr(mesh.0, mesh.1 + 52)) {
            for k in 0..nb.min(512) {
                if let Some(n) = g.ptr(arr.0, arr.1 + 44 * k).and_then(|p| g.cstr(p)) {
                    bones.push(n);
                }
            }
        }
        out.push(MeshInfo { name, bones, bbox: mesh_bbox(&g, mesh), materials });
    }
    // Skeleton bone names (Bone records are 164 bytes, name pointer first).
    let mut skeleton = Vec::new();
    if let (Some(sc), Some(sarr)) = (g.u32(rs, ro + 48), g.ptr(rs, ro + 52)) {
        for si in 0..sc.min(16) {
            let Some(skel) = g.ptr(sarr.0, sarr.1 + 8 * si) else { continue };
            let (Some(n), Some(barr)) = (g.u32(skel.0, skel.1 + 8), g.ptr(skel.0, skel.1 + 12)) else { continue };
            for k in 0..n.min(1024) {
                if let Some(nm) = g.ptr(barr.0, barr.1 + 164 * k).and_then(|p| g.cstr(p)) {
                    skeleton.push(nm);
                }
            }
        }
    }
    if out.is_empty() { None } else { Some((out, skeleton)) }
}

/// The variants (label, mesh name) of the first rule that matches this model; empty if none does.
pub fn active_variants(meshes: &[MeshInfo]) -> Vec<(&'static str, String)> {
    for rule in VARIANT_RULES {
        let present: Vec<(&'static str, String)> = rule
            .members
            .iter()
            .filter(|t| meshes.iter().any(|m| m.name == t.0))
            .map(|t| (t.1, t.0.to_string()))
            .collect();
        if present.len() >= 2 {
            return present;
        }
        if present.len() == 1 {
            eprintln!("  Warning: only one mesh of the '{}' set ({}) is present; exporting normally.", rule.name, present[0].1);
        }
    }
    Vec::new()
}

static AUTO_VARIANTS: AtomicBool = AtomicBool::new(true);

/// Turn automatic splitting of overlapping meshes on (default) or off.
pub fn set_auto_variants(on: bool) {
    AUTO_VARIANTS.store(on, Ordering::Relaxed);
}

fn auto_variants_on() -> bool {
    AUTO_VARIANTS.load(Ordering::Relaxed)
}

fn file_label(s: &str) -> String {
    let l: String = s
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect();
    if l.is_empty() { "variant".to_string() } else { l }
}

/// Variants to export as separate OBJs: the `VARIANT_RULES` match if there is one, otherwise (unless disabled)
/// the largest cluster of overlapping meshes, labelled by mesh name. Only one cluster is split per model,
/// because every variant OBJ keeps all meshes that are not part of the split set.
fn resolve_variants(meshes: &[MeshInfo], skeleton: &[String]) -> Vec<(String, String)> {
    let by_rule: Vec<(String, String)> = active_variants(meshes)
        .into_iter()
        .map(|(l, m)| (l.to_string(), m))
        .collect();
    if !by_rule.is_empty() || !auto_variants_on() {
        return by_rule;
    }
    let mut clusters = overlap_clusters(meshes, skeleton);
    clusters.sort_by_key(|(m, _)| std::cmp::Reverse(m.len()));
    let Some((members, evidence)) = clusters.first() else { return by_rule };

    let names: Vec<&str> = members.iter().map(|i| meshes[*i].name.as_str()).collect();
    let distinct: HashSet<&str> = names.iter().copied().collect();
    if distinct.len() != names.len() {
        eprintln!("  Warning: overlapping meshes share a name so they can't be told apart; exporting them together.");
        return by_rule;
    }
    let mut used: HashSet<String> = HashSet::new();
    let mut out = Vec::new();
    for n in names {
        let base = file_label(n);
        let mut label = base.clone();
        let mut k = 2;
        while !used.insert(label.to_ascii_lowercase()) {
            label = format!("{}_{}", base, k);
            k += 1;
        }
        out.push((label, n.to_string()));
    }
    println!("  Overlapping meshes detected ({}); exporting one OBJ per version.", evidence);
    out
}

fn jaccard(a: &[String], b: &[String]) -> f32 {
    let sa: HashSet<&str> = a.iter().map(String::as_str).collect();
    let sb: HashSet<&str> = b.iter().map(String::as_str).collect();
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    sa.intersection(&sb).count() as f32 / sa.union(&sb).count() as f32
}

/// (intersection volume, volume of a, volume of b) of two padded bounding boxes.
fn bbox_overlap(a: &([f32; 3], [f32; 3]), b: &([f32; 3], [f32; 3])) -> (f32, f32, f32) {
    const PAD: f32 = 0.005; // keeps flat (zero-thickness) meshes from having zero volume
    let (mut inter, mut va, mut vb) = (1.0f32, 1.0f32, 1.0f32);
    for k in 0..3 {
        let (alo, ahi, blo, bhi) = (a.0[k] - PAD, a.1[k] + PAD, b.0[k] - PAD, b.1[k] + PAD);
        inter *= (ahi.min(bhi) - alo.max(blo)).max(0.0);
        va *= ahi - alo;
        vb *= bhi - blo;
    }
    (inter, va, vb)
}

/// Is there a skeleton node called like this mesh? Nodes may be path-style (`parent|name`).
fn in_skeleton(skeleton: &[String], name: &str) -> bool {
    skeleton.iter().any(|s| s == name || s.rsplit('|').next() == Some(name))
}

/// Groups of meshes that are alternative versions of the same thing. Two meshes belong together when either
///   * their bounding boxes overlap >= 85% and they are skinned to (nearly) the same bones or both have a
///     switch-marker node in the skeleton, or
///   * they use (nearly) the same materials, one sits inside the other's bounding box, and they are of
///     comparable size. (Damage/destruction stages of one building look like this: the bones differ per stage and
///     the bounding boxes can differ a lot, but the materials are the same.)
/// Returns (member mesh indexes, evidence text).
fn overlap_clusters(meshes: &[MeshInfo], skeleton: &[String]) -> Vec<(Vec<usize>, String)> {
    fn find(p: &mut Vec<usize>, mut x: usize) -> usize {
        while p[x] != x {
            p[x] = p[p[x]];
            x = p[x];
        }
        x
    }
    // (i, j, bbox overlap, containment, bone overlap, material overlap, both named in skeleton)
    type Pair = (usize, usize, f32, f32, f32, f32, bool);
    let n = meshes.len();
    let mut parent: Vec<usize> = (0..n).collect();
    let mut pairs: Vec<Pair> = Vec::new();
    for i in 0..n {
        for j in i + 1..n {
            let (Some(a), Some(b)) = (&meshes[i].bbox, &meshes[j].bbox) else { continue };
            let (inter, va, vb) = bbox_overlap(a, b);
            let uni = va + vb - inter;
            let iou = if uni <= 0.0 { 0.0 } else { inter / uni };
            let (small, large) = (va.min(vb), va.max(vb));
            let contained = if small <= 0.0 { 0.0 } else { inter / small };
            let size_ratio = if large <= 0.0 { 0.0 } else { small / large };
            let bones = jaccard(&meshes[i].bones, &meshes[j].bones);
            let mats = jaccard(&meshes[i].materials, &meshes[j].materials);
            let named = in_skeleton(skeleton, &meshes[i].name) && in_skeleton(skeleton, &meshes[j].name);
            let by_bones = iou >= 0.85 && (bones >= 0.75 || named);
            let by_materials = mats >= 0.7 && contained >= 0.6 && size_ratio >= 0.2;
            if by_bones || by_materials {
                pairs.push((i, j, iou, contained, bones, mats, named));
                let (ri, rj) = (find(&mut parent, i), find(&mut parent, j));
                parent[ri] = rj;
            }
        }
    }
    let mut out = Vec::new();
    for root in 0..n {
        let members: Vec<usize> = (0..n).filter(|i| find(&mut parent, *i) == root).collect();
        if members.len() < 2 {
            continue;
        }
        let mine: Vec<&Pair> = pairs.iter().filter(|p| members.contains(&p.0)).collect();
        let min_of = |f: fn(&Pair) -> f32| mine.iter().map(|p| f(p)).fold(1.0f32, f32::min);
        let named = mine.iter().any(|p| p.6);
        out.push((
            members,
            format!(
                "bounding boxes overlap >= {:.0}%, shared materials >= {:.0}%, shared bone bindings >= {:.0}%{}",
                min_of(|p| p.2) * 100.0,
                min_of(|p| p.5) * 100.0,
                min_of(|p| p.4) * 100.0,
                if named { ", meshes have same-named nodes in the skeleton (switch markers)" } else { "" }
            ),
        ));
    }
    out
}

/// Console warning for overlapping meshes that no `VARIANT_RULES` entry covers.
fn report_variant_patterns(index: u32, meshes: &[MeshInfo], skeleton: &[String], handled: &[(String, String)]) {
    let handled_names: HashSet<&str> = handled.iter().map(|(_, m)| m.as_str()).collect();
    for (members, evidence) in overlap_clusters(meshes, skeleton) {
        if members.iter().all(|i| handled_names.contains(meshes[*i].name.as_str())) {
            continue;
        }
        let names: Vec<&str> = members.iter().map(|i| meshes[*i].name.as_str()).collect();
        eprintln!("  WARNING [model {}]: meshes {} look like alternative versions of the same thing", index, names.join(", "));
        eprintln!("    ({}).", evidence);
        eprintln!("    They are exported together, so they will be stacked in one OBJ. If the game only shows one of them,");
        eprintln!("    add their mesh names to VARIANT_RULES in model_export.rs to get one OBJ per variant.");
        if !auto_variants_on() {
            eprintln!("    (or run without --no-auto-variants to split them automatically)");
        }
        for i in members {
            eprintln!("      mesh '{}': materials [{}]", meshes[i].name, meshes[i].materials.join(", "));
        }
    }
}

/// Note when one material name is used for several different texture sets (they are kept apart anyway).
fn report_duplicate_material_names(mats: &[MatDef]) {
    let mut by_name: Vec<(&str, usize)> = Vec::new();
    for m in mats {
        match by_name.iter_mut().find(|(n, _)| *n == m.name.as_str()) {
            Some(e) => e.1 += 1,
            None => by_name.push((m.name.as_str(), 1)),
        }
    }
    for (n, c) in by_name.into_iter().filter(|(_, c)| *c > 1) {
        println!("  Note: {} different materials are all called '{}' (different textures); exported as separate materials ('{}', '{}_2', ...).", c, n, n, n);
    }
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
            alpha: AlphaMode::Blend,
            key: (u32::MAX, i as u32),
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
    alpha: AlphaMode,
    key: (u32, u32),
}

fn role_of(usage: &str) -> Option<usize> {
    SLOT_ROLES.iter().position(|r| r.eq_ignore_ascii_case(usage.trim()))
}

fn norm_name(s: &str) -> String {
    s.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect()
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

fn texture_out_name(mat_i: usize, tex_i: usize, role: Option<usize>) -> String {
    let ext = "dds";
    match (mat_i, role) {
        (0, Some(r)) => format!("{}.{}", SLOT_ROLES[r], ext),
        _ => format!("extra{}_{}.{}", mat_i, tex_i, ext),
    }
}

/// DDS files from the archive can carry a wrong linear size; fix it in place.
fn fix_dds_linear_size(d: &mut [u8]) {
    if d.len() < 128 || &d[0..4] != b"DDS " {
        return;
    }
    let block = match &d[84..88] {
        b"DXT1" => 8u32,
        b"DXT3" | b"DXT5" => 16u32,
        _ => return,
    };
    let h = u32::from_le_bytes(d[12..16].try_into().unwrap());
    let w = u32::from_le_bytes(d[16..20].try_into().unwrap());
    let size = ((w + 3) / 4) * ((h + 3) / 4) * block;
    d[20..24].copy_from_slice(&size.to_le_bytes());
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









type UvData = (Vec<[f32; 2]>, Vec<[u32; 3]>);

struct ObjFace {
    group: String,
    /// `usemtl` that was active in the Noesis file (only used when the groups can't be matched to the .gr2).
    mtl: Option<String>,
    /// 0-based [v, vt, vn] per corner
    idx: Vec<[Option<usize>; 3]>,
    tris: usize,
}

struct ObjData {
    v: Vec<String>,
    vt: Vec<String>,
    vn: Vec<String>,
    faces: Vec<ObjFace>,
}

fn parse_obj(text: &str) -> Option<ObjData> {
    let (mut v, mut vt, mut vn, mut faces) = (Vec::new(), Vec::new(), Vec::new(), Vec::new());
    let (mut grp, mut mtl): (String, Option<String>) = (String::new(), None);
    for line in text.lines() {
        let l = line.trim();
        if let Some(r) = l.strip_prefix("vt ") {
            vt.push(r.trim().to_string());
        } else if let Some(r) = l.strip_prefix("vn ") {
            vn.push(r.trim().to_string());
        } else if let Some(r) = l.strip_prefix("v ") {
            v.push(r.trim().to_string());
        } else if l == "g" || l.starts_with("g ") {
            let n = l[1..].trim();
            if !n.is_empty() {
                grp = n.to_string();
            }
        } else if let Some(r) = l.strip_prefix("usemtl ") {
            mtl = Some(r.trim().to_string());
        } else if let Some(r) = l.strip_prefix("f ") {
            let mut idx = Vec::new();
            for tok in r.split_whitespace() {
                let mut it = tok.split('/');
                let mut a = [None; 3];
                for slot in a.iter_mut() {
                    match it.next() {
                        None | Some("") => {}
                        Some(x) => {
                            let n: i64 = x.parse().ok()?;
                            if n < 1 { return None; }
                            *slot = Some(n as usize - 1);
                        }
                    }
                }
                a[0]?;
                idx.push(a);
            }
            if idx.len() < 3 { return None; }
            let tris = idx.len() - 2;
            faces.push(ObjFace { group: grp.clone(), mtl: mtl.clone(), idx, tris });
        }
    }
    Some(ObjData { v, vt, vn, faces })
}

struct Align {
    /// OBJ face -> index into the .gr2 `TriGroup` list
    face_group: Vec<usize>,
    /// OBJ triangle (in file order) -> triangle number in the .gr2. `None` when the OBJ's triangle counts differ
    /// from the .gr2's: the groups are still known, but individual triangles can't be mapped.
    tri_perm: Option<Vec<u32>>,
}

/// (group name, first face, face count, triangle count) for every run of consecutive faces under one `g` name.
fn obj_runs(obj: &ObjData) -> Vec<(String, usize, usize, usize)> {
    let mut runs: Vec<(String, usize, usize, usize)> = Vec::new();
    for (fi, f) in obj.faces.iter().enumerate() {
        match runs.last_mut() {
            Some(r) if r.0 == f.group => { r.2 += 1; r.3 += f.tris; }
            _ => runs.push((f.group.clone(), fi, 1, f.tris)),
        }
    }
    runs
}

/// Last resort when Noesis' group names aren't `<mesh>_<group>`: find each mesh's run of groups in the OBJ by the
/// triangle counts of its groups. Gives up unless every mesh matches exactly one place.
fn match_by_counts(runs: &[(String, usize, usize, usize)], groups: &[TriGroup]) -> Option<Vec<usize>> {
    let mut run_group: Vec<Option<usize>> = vec![None; runs.len()];
    let mut start = 0usize;
    while start < groups.len() {
        let mut end = start;
        while end < groups.len() && groups[end].mesh == groups[start].mesh {
            end += 1;
        }
        let counts: Vec<usize> = groups[start..end].iter().map(|g| g.count as usize).collect();
        if runs.len() < counts.len() {
            return None;
        }
        let hits: Vec<usize> = (0..=runs.len() - counts.len())
            .filter(|p| runs[*p..*p + counts.len()].iter().map(|r| r.3).eq(counts.iter().copied()))
            .collect();
        if hits.len() != 1 {
            return None;
        }
        for k in 0..counts.len() {
            if run_group[hits[0] + k].is_some() {
                return None;
            }
            run_group[hits[0] + k] = Some(start + k);
        }
        start = end;
    }
    run_group.into_iter().collect()
}

/// Noesis prefixes every group with a mesh counter when it finds duplicate mesh names (`0003_<mesh>_<n>`) and may
/// split a group into several runs. Find the mesh from the group name, then lay that mesh's runs over its .gr2 groups
/// by cumulative triangle count. Returns, per run, (group index, triangle offset inside that group) and whether every
/// run fits inside one group.
fn match_by_mesh(runs: &[(String, usize, usize, usize)], groups: &[TriGroup]) -> Option<(Vec<(usize, u32)>, bool)> {
    // distinct mesh names, longest first so a name that is a prefix of another can't claim its groups
    let mut names: Vec<&str> = Vec::new();
    for g in groups {
        if !names.contains(&g.mesh.as_str()) {
            names.push(g.mesh.as_str());
        }
    }
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    let mesh_of = |run: &str| -> Option<usize> {
        let body = match run.find('_') {
            Some(p) if p > 0 && run[..p].bytes().all(|b| b.is_ascii_digit()) => &run[p + 1..],
            _ => run,
        };
        names.iter().position(|m| {
            body.strip_prefix(*m)
                .and_then(|r| r.strip_prefix('_'))
                .map_or(false, |r| !r.is_empty() && r.bytes().all(|b| b.is_ascii_digit()))
        })
    };
    let mut per_mesh: Vec<Vec<usize>> = vec![Vec::new(); names.len()];
    for (ri, r) in runs.iter().enumerate() {
        per_mesh[mesh_of(&r.0)?].push(ri);
    }
    let mut out: Vec<Option<(usize, u32)>> = vec![None; runs.len()];
    let mut exact = true;
    for (mi, name) in names.iter().enumerate() {
        let gidx: Vec<usize> = (0..groups.len()).filter(|i| groups[*i].mesh == *name).collect();
        let total: usize = gidx.iter().map(|i| groups[*i].count as usize).sum();
        let run_total: usize = per_mesh[mi].iter().map(|ri| runs[*ri].3).sum();
        if total != run_total {
            return None;
        }
        let mut starts: Vec<usize> = Vec::new();
        let mut acc = 0usize;
        for i in &gidx {
            starts.push(acc);
            acc += groups[*i].count as usize;
        }
        let mut pos = 0usize;
        for ri in &per_mesh[mi] {
            let len = runs[*ri].3;
            let k = starts.iter().rposition(|s| *s <= pos)?;
            let gi = gidx[k];
            let off = pos - starts[k];
            if off + len > groups[gi].count as usize {
                exact = false; // the run reaches into the next group
            }
            out[*ri] = Some((gi, off as u32));
            pos += len;
        }
    }
    let v: Option<Vec<(usize, u32)>> = out.into_iter().collect();
    Some((v?, exact))
}

/// Match the OBJ's groups to the .gr2's (mesh, group) list. Noesis names them `<mesh>_<group>` (with a
/// `<counter>_` prefix when mesh names repeat) and writes the meshes in a different order than the .gr2 stores
/// them, so position alone can't be trusted.
fn align_obj(obj: &ObjData, groups: &[TriGroup]) -> Option<Align> {
    let runs = obj_runs(obj);
    let by_name: HashMap<String, usize> = groups.iter().enumerate().map(|(i, g)| (format!("{}_{}", g.mesh, g.index), i)).collect();
    // per run: (group index, triangle offset inside that group)
    let mut run_group: Vec<(usize, u32)> = Vec::new();
    let mut exact = true;
    if by_name.len() == groups.len() && runs.iter().all(|r| by_name.contains_key(&r.0)) {
        let mut seen = HashSet::new();
        for r in &runs {
            let gi = by_name[&r.0];
            if !seen.insert(gi) {
                return None;
            }
            if groups[gi].count as usize != r.3 {
                exact = false;
            }
            run_group.push((gi, 0));
        }
    } else if let Some((rg, ex)) = match_by_mesh(&runs, groups) {
        run_group = rg;
        exact = ex;
    } else if runs.len() == groups.len() && runs.iter().zip(groups).all(|(r, g)| r.3 == g.count as usize) {
        // Group names weren't recognised, but everything lines up in file order.
        run_group = (0..groups.len()).map(|g| (g, 0)).collect();
    } else {
        run_group = match_by_counts(&runs, groups)?.into_iter().map(|g| (g, 0)).collect();
    }
    let mut face_group = vec![0usize; obj.faces.len()];
    let mut tri_perm = Vec::new();
    for (r, (gi, off)) in runs.iter().zip(run_group) {
        let g = &groups[gi];
        let mut t = off;
        for fi in r.1..r.1 + r.2 {
            face_group[fi] = gi;
            for _ in 0..obj.faces[fi].tris {
                tri_perm.push(g.first + t);
                t += 1;
            }
        }
    }
    Some(Align { face_group, tri_perm: if exact { Some(tri_perm) } else { None } })
}

/// Why `align_obj` failed, in one line, so a mismatch can be diagnosed from the console.
fn explain_align_failure(obj: &ObjData, groups: &[TriGroup]) -> String {
    let runs = obj_runs(obj);
    let meshes: HashSet<&str> = groups.iter().map(|g| g.mesh.as_str()).collect();
    let first_runs: Vec<String> = runs.iter().take(4).map(|r| format!("'{}' ({} tris)", r.0, r.3)).collect();
    let first_groups: Vec<String> = groups.iter().take(4).map(|g| format!("'{}_{}' ({} tris)", g.mesh, g.index, g.count)).collect();
    format!(
        "OBJ: {} group(s), {} triangle(s), starting {}.  .gr2: {} group(s) in {} mesh(es), {} triangle(s), starting {}.",
        runs.len(),
        runs.iter().map(|r| r.3).sum::<usize>(),
        first_runs.join(", "),
        groups.len(),
        meshes.len(),
        groups.iter().map(|g| g.count as usize).sum::<usize>(),
        first_groups.join(", ")
    )
}

fn unique_names<'a>(names: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut count: HashMap<&str, usize> = HashMap::new();
    names
        .map(|n| {
            let n = if n.is_empty() { "material" } else { n };
            let c = count.entry(n).or_insert(0);
            *c += 1;
            if *c == 1 { n.to_string() } else { format!("{}_{}", n, c) }
        })
        .collect()
}

fn remap(i: usize, map: &mut HashMap<usize, usize>, order: &mut Vec<usize>) -> usize {
    *map.entry(i).or_insert_with(|| {
        order.push(i);
        order.len()
    })
}

/// Writes `<stem>.obj/.mtl` into `out_dir`, or - when `variants` is non-empty - one `<stem>_<label>.obj/.mtl`
/// per variant containing the body plus only that variant's meshes. Returns (file name, material count).
fn finish_obj(src: &Path, out_dir: &Path, stem: &str, model_mats: &[MatPng],
              groups: Option<&[TriGroup]>, uvs: Option<&UvData>,
              variants: &[(String, String)]) -> io::Result<Vec<(String, usize)>> {
    let bad = |m: &str| io::Error::new(io::ErrorKind::InvalidData, m.to_string());
    let mut text = fs::read_to_string(src)?;
    let obj0 = parse_obj(&text).ok_or_else(|| bad("unsupported face syntax in the Noesis OBJ"))?;

    if groups.is_none() {
        eprintln!("  Warning: could not read the mesh/material groups from the .gr2; materials come from Noesis' usemtl lines and variants can't be split.");
    }
    let align = groups.and_then(|gs| {
        let a = align_obj(&obj0, gs);
        if a.is_none() {
            eprintln!("  Warning: the OBJ groups don't line up with the .gr2 mesh/material groups; keeping Noesis' usemtl lines.");
            eprintln!("    {}", explain_align_failure(&obj0, gs));
        } else if a.as_ref().map_or(false, |a| a.tri_perm.is_none()) {
            eprintln!("  Note: the OBJ's triangle counts differ from the .gr2's; groups/materials are matched by name, but the UVs stay as Noesis wrote them.");
        }
        a
    });

    if let Some((uv, gt)) = uvs {
        // Triangles in OBJ order, so UV matching works even though Noesis reorders the meshes.
        let ordered: Option<Vec<[u32; 3]>> = match &align {
            Some(Align { tri_perm: Some(p), .. }) => Some(
                p.iter().map(|t| gt.get(*t as usize).copied()).collect::<Option<Vec<_>>>().unwrap_or_else(|| gt.clone()),
            ),
            Some(_) => None,
            None => Some(gt.clone()),
        };
        match ordered.and_then(|o| patch_obj_uvs(&text, uv, &o)) {
            Some(t) => { println!("  UVs rewritten from the .gr2"); text = t; }
            None => eprintln!("  Warning: could not match the OBJ to the .gr2 triangles; keeping Noesis' UVs."),
        }
    }
    let obj = parse_obj(&text).ok_or_else(|| bad("unsupported face syntax after UV patch"))?;
    if obj.faces.len() != obj0.faces.len() {
        return Err(bad("face count changed while patching UVs"));
    }

    let split = !variants.is_empty() && align.is_some() && groups.is_some();
    if !variants.is_empty() && !split {
        eprintln!("  Warning: cannot split the variant meshes (OBJ/.gr2 groups did not line up); everything goes into one OBJ.");
    }

    // usemtl name per face (+ index into model_mats when known exactly)
    let uniq = unique_names(model_mats.iter().map(|m| m.name.as_str()));
    let face_mat: Vec<(String, Option<usize>)> = (0..obj.faces.len())
        .map(|fi| match (&align, groups) {
            (Some(a), Some(gs)) => {
                let g = &gs[a.face_group[fi]];
                match model_mats.iter().position(|m| m.key == g.mat_key) {
                    Some(j) => (uniq[j].clone(), Some(j)),
                    None => (if g.mat_name.is_empty() { "eso_material".to_string() } else { g.mat_name.clone() }, None),
                }
            }
            _ => (obj.faces[fi].mtl.clone().unwrap_or_else(|| "eso_material".to_string()), None),
        })
        .collect();
    let face_mesh = |fi: usize| -> Option<&str> {
        match (&align, groups) {
            (Some(a), Some(gs)) => Some(gs[a.face_group[fi]].mesh.as_str()),
            _ => None,
        }
    };

    let targets: Vec<(String, Option<&str>)> = if split {
        variants.iter().map(|(label, mesh)| (format!("{}_{}", stem, label), Some(mesh.as_str()))).collect()
    } else {
        vec![(stem.to_string(), None)]
    };

    let mut written = Vec::new();
    for (name, keep_mesh) in targets {
        let drop: HashSet<&str> = if split {
            variants.iter().map(|(_, m)| m.as_str()).filter(|m| Some(*m) != keep_mesh).collect()
        } else {
            HashSet::new()
        };
        let mtl_file = format!("{}.mtl", name);

        let (mut vm, mut tm, mut nm) = (HashMap::new(), HashMap::new(), HashMap::new());
        let (mut vo, mut to, mut no): (Vec<usize>, Vec<usize>, Vec<usize>) = (Vec::new(), Vec::new(), Vec::new());
        let mut body = String::new();
        let (mut cur_grp, mut cur_mtl) = (String::new(), String::new());
        let mut mats: Vec<String> = Vec::new();
        let mut pre: HashMap<String, usize> = HashMap::new();

        for (fi, f) in obj.faces.iter().enumerate() {
            if face_mesh(fi).map_or(false, |m| drop.contains(m)) {
                continue;
            }
            if f.group != cur_grp {
                cur_grp = f.group.clone();
                body.push_str(&format!("g {}\n", cur_grp));
            }
            let (mname, mj) = &face_mat[fi];
            if *mname != cur_mtl {
                cur_mtl = mname.clone();
                body.push_str(&format!("usemtl {}\n", cur_mtl));
                if !mats.contains(mname) { mats.push(mname.clone()); }
            }
            if let Some(j) = mj { pre.insert(mname.clone(), *j); }
            let mut line = String::from("f");
            for c in &f.idx {
                let v = remap(c[0].unwrap_or(0), &mut vm, &mut vo);
                let t = c[1].map(|i| remap(i, &mut tm, &mut to));
                let n = c[2].map(|i| remap(i, &mut nm, &mut no));
                line.push(' ');
                line.push_str(&match (t, n) {
                    (Some(t), Some(n)) => format!("{}/{}/{}", v, t, n),
                    (Some(t), None) => format!("{}/{}", v, t),
                    (None, Some(n)) => format!("{}//{}", v, n),
                    (None, None) => format!("{}", v),
                });
            }
            body.push_str(&line);
            body.push('\n');
        }

        let mut out = format!("mtllib {}\n", mtl_file);
        for i in &vo { out.push_str(&format!("v {}\n", obj.v.get(*i).map(String::as_str).unwrap_or("0 0 0"))); }
        for i in &to { out.push_str(&format!("vt {}\n", obj.vt.get(*i).map(String::as_str).unwrap_or("0 0"))); }
        for i in &no { out.push_str(&format!("vn {}\n", obj.vn.get(*i).map(String::as_str).unwrap_or("0 0 1"))); }
        out.push_str(&body);
        if mats.is_empty() {
            mats.push("eso_material".to_string());
        }
        fs::write(out_dir.join(format!("{}.obj", name)), out)?;

        // Pair every usemtl name with one of the model's materials: exact (.gr2 key) first, then by name, then by order.
        let mut assigned: Vec<Option<usize>> = vec![None; mats.len()];
        let mut used = vec![false; model_mats.len()];
        let mut by_name = vec![false; mats.len()];
        for (i, m) in mats.iter().enumerate() {
            if let Some(j) = pre.get(m) {
                if !used[*j] {
                    assigned[i] = Some(*j);
                    used[*j] = true;
                    by_name[i] = true;
                }
            }
        }
        for (i, m) in mats.iter().enumerate() {
            if assigned[i].is_some() { continue; }
            let key = norm_name(m);
            if key.is_empty() { continue; }
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

        println!("  {}.obj:", name);
        let mut mtl = String::new();
        for (mi, m) in mats.iter().enumerate() {
            let j = assigned[mi].or(model_mats.len().checked_sub(1));
            let slot = j.and_then(|j| model_mats.get(j));
            let tex = |role: usize| slot.and_then(|s| s.png[role].as_deref());
            match slot {
                Some(s) => println!(
                    "    usemtl '{}' -> material '{}' ({}), diffuse {}, normal {}, specular {}",
                    m,
                    s.name,
                    if by_name[mi] { "matched exactly" } else { "matched by order" },
                    tex(0).unwrap_or("none"),
                    tex(1).unwrap_or("none"),
                    tex(2).unwrap_or("none"),
                ),
                None => println!("    usemtl '{}' -> no textures", m),
            }
            mtl.push_str(&format!("newmtl {}\nKa 1.000 1.000 1.000\nKd 1.000 1.000 1.000\nKs 0.200 0.200 0.200\nNs 20.0\nd 1.0\nillum 2\n", m));
            let alpha = slot.map_or(AlphaMode::None, |s| s.alpha);
            if let Some(name) = alpha.mtl_name() {
                // Blender's importer ignores this line; the scene scripts read it to pick the blend mode.
                mtl.push_str(&format!("# alpha_mode {}\n", name));
            }
            if let Some(p) = tex(0) {
                mtl.push_str(&format!("map_Kd {}\n", p));
                // Only use the diffuse alpha as opacity when the material asks for it; otherwise the
                // alpha channel holds unrelated data and would make solid surfaces look see-through.
                if alpha != AlphaMode::None {
                    mtl.push_str(&format!("map_d {}\n", p));
                }
            }
            if let Some(p) = tex(1) { mtl.push_str(&format!("map_Bump {}\nbump {}\n", p, p)); }
            if let Some(p) = tex(2) { mtl.push_str(&format!("map_Ks {}\n", p)); }
            mtl.push('\n');
        }
        fs::write(out_dir.join(&mtl_file), mtl)?;
        written.push((format!("{}.obj", name), mats.len()));
    }
    Ok(written)
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
    run_with_textures(model_fi, None, None, "fbx", o)
}

/// Like `run`, but with `tex` = [diffuse, normal, specular] file indexes for models that don't
/// embed texture references (creatures). The model is exported with those textures on every material.
/// `skel` = FileIndex of the creature's skeleton .gr2, copied next to the model as `<index>.gr2`.
/// When `skel` is given, a skinned `<index>.<rig_ext>` (mesh + bones + weights, written by Noesis; the format
/// follows the extension, e.g. fbx, glb, dae, smd) is exported as well.
pub fn run_with_textures(model_fi: u32, tex: Option<[u32; 3]>, skel: Option<u32>, rig_ext: &str, o: &Options) -> Result<Summary, String> {
    let mut want = HashSet::new();
    want.insert(model_fi);
    let found = lookup_file_indexes(o.manifest, &want);
    let model = pick_row("Model", model_fi, found.get(&model_fi))?;
    println!(
        "Model: FileIndex 0x{:08X} -> Index {} (arch {}, {} bytes)",
        model_fi, model.index, model.arch, model.size
    );

    if o.dry_run {
        println!("\nDry run: stopping here (texture list needs the model file).");
        return Ok(Summary { out_dir: PathBuf::new(), textures_found: 0, textures_missing: 0 });
    }
    let blob_rc = o.cache.get(o.archive, model.index)
        .map_err(|e| format!("Model file (Index {}) could not be read: {}", model.index, e))?;
    let blob: &[u8] = &blob_rc;
    if tex.is_none() && texture_refs(blob).is_empty() {
        return Err(format!(
            "No texture reference records found in model Index {}. Is this a model .gr2 with the usual header table?",
            model.index
        ));
    }
    let mats: Vec<MatDef> = match parse_materials_with(blob, tex) {
        Some(m) => m,
        None => {
            eprintln!("  Warning: could not read the material table of this .gr2; guessing material groups from the texture records (3 maps per material).");
            materials_from_triples(blob)
        }
    };
    report_duplicate_material_names(&mats);
    println!("Model has {} material(s):", mats.len());
    for m in &mats {
        let maps: Vec<String> = m.maps.iter().map(|x| format!("{}=0x{:x}", x.usage, x.file_index)).collect();
        println!("  '{}': {}", m.name, maps.join(", "));
    }

    let wanted: HashSet<u32> = mats.iter().flat_map(|m| m.maps.iter().map(|x| x.file_index)).collect();
    let rows = lookup_file_indexes(o.manifest, &wanted);

    // One pass over the archive for every texture of this model (cached ones are skipped).
    let mut tex_indices: Vec<u32> = wanted.iter().filter_map(|fi| rows.get(fi).and_then(|v| v.first()).map(|r| r.index)).collect();
    tex_indices.sort_unstable();
    o.cache.prefetch(o.archive, &tex_indices);

    // Everything goes into one flat folder, keeping the archive's own file names:
    //   <index>.gr2, <textureIndex>.dds / .png, <index>.obj, <index>.mtl
    // Textures shared between models therefore just share a file instead of colliding.
    let out_dir = o.out_root.to_path_buf();
    fs::create_dir_all(&out_dir)
        .map_err(|e| format!("Failed to create '{}': {}", out_dir.display(), e))?;
    let model_name = format!("{}.gr2", model.index);
    fs::write(out_dir.join(&model_name), blob)
        .map_err(|e| format!("Failed to write model: {}", e))?;
    println!("Model -> {}", out_dir.join(&model_name).display());

    if let Some(sk) = skel.filter(|s| *s != 0) {
        let want: HashSet<u32> = [sk].into_iter().collect();
        let found = lookup_file_indexes(o.manifest, &want);
        let res = pick_row("Skeleton", sk, found.get(&sk)).and_then(|row| {
            let b = o.cache.get(o.archive, row.index)?;
            let name = format!("{}.gr2", row.index);
            fs::write(out_dir.join(&name), b.as_slice()).map_err(|e| e.to_string())?;
            Ok(name)
        });
        match res {
            Ok(name) => println!("Skeleton -> {}", out_dir.join(&name).display()),
            Err(e) => eprintln!("  Warning: skeleton 0x{:08X} not exported: {}", sk, e),
        }
    }

    // Noesis' GR2 plugin wants the textures next to the model under fixed role names
    // (diffuse/normal/specular), so it gets a private scratch folder (removed when it goes out of scope).
    let scratch_guard = Scratch::new(&format!("model_{}", model.index))
        .map_err(|e| format!("Failed to create scratch folder: {}", e))?;
    let scratch = scratch_guard.path().to_path_buf();
    fs::write(scratch.join(&model_name), blob)
        .map_err(|e| format!("Failed to write model to scratch: {}", e))?;

    // PNG name per material, per role (diffuse/normal/specular), used by the .mtl.
    let mut mat_png: Vec<MatPng> = mats
        .iter()
        .map(|m| MatPng { name: m.name.clone(), png: [None, None, None], alpha: m.alpha, key: m.key })
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
        let dds = o.cache.get(o.archive, row.index).and_then(|b| archive::dds_from_bytes(b.as_ref().clone()));
        match dds {
            Ok(mut data) => {
                fix_dds_linear_size(&mut data);
                let name = format!("{}.dds", row.index);
                if let Err(e) = fs::write(out_dir.join(&name), &data) {
                    eprintln!("  slot {}: write failed: {}", slot, e);
                    missing += 1;
                    continue;
                }
                let png_name = format!("{}.png", row.index);
                match image::load_from_memory(&data) {
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
                let _ = fs::write(scratch.join(texture_out_name(mat_i, tex_i, role)), &data);
                println!("  '{}' {}: 0x{:08X} -> {}", mats[mat_i].name, usage, fi, name);
                ok += 1;
            }
            Err(e) => {
                eprintln!("  slot {}: 0x{:08X} (Index {}) could not be read: {}", slot, fi, row.index, e);
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
            let groups = tri_material_groups(blob);
            let uvs = mesh_uvs(blob);
            let (infos, skeleton) = mesh_infos(blob).unwrap_or_default();
            let variants = resolve_variants(&infos, &skeleton);
            if !variants.is_empty() {
                let list: Vec<String> = variants.iter().map(|(l, m)| format!("{} ({})", l, m)).collect();
                println!("  Variant meshes found: {} -> one OBJ per variant.", list.join(", "));
            }
            report_variant_patterns(model.index, &infos, &skeleton, &variants);
            let stem = model.index.to_string();
            match finish_obj(&scratch_obj, &out_dir, &stem, &mat_png, groups.as_deref(), uvs.as_ref(), &variants) {
                Ok(files) => {
                    for (f, n) in &files {
                        println!("OBJ -> {} ({} material(s), textures linked)", out_dir.join(f).display(), n);
                    }
                    // An earlier export of this model may have left a combined OBJ with every variant stacked.
                    if files.iter().all(|(f, _)| *f != format!("{}.obj", model.index)) {
                        for ext in ["obj", "mtl"] {
                            let stale = out_dir.join(format!("{}.{}", model.index, ext));
                            if stale.exists() && fs::remove_file(&stale).is_ok() {
                                println!("  Removed outdated combined {}", stale.display());
                            }
                        }
                    }
                }
                Err(e) => eprintln!("Warning: could not finish obj: {}", e),
            }
        }
        Err(e) => eprintln!("Warning: obj export failed: {}", e),
    }
    if skel.is_some() {
        let n_bones = mesh_infos(blob).map(|(_, s)| s.len()).unwrap_or(0);
        println!("  The model file embeds a skeleton of {} bone(s).", n_bones);
        let fbx_res = plain_abs(&scratch.join(&model_name)).and_then(|src| {
            let fbx = src.with_extension(rig_ext);
            export_model(o.noesis, &src, &fbx).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
            Ok(fbx)
        });
        match fbx_res {
            Ok(fbx) => {
                let dest = out_dir.join(format!("{}.{}", model.index, rig_ext));
                match fs::copy(&fbx, &dest) {
                    Ok(_) => println!("Rigged {} -> {}", rig_ext, dest.display()),
                    Err(e) => eprintln!("Warning: could not copy the .{} file: {}", rig_ext, e),
                }
            }
            Err(e) => eprintln!("Warning: .{} export failed (does this Noesis build export that format?): {}", rig_ext, e),
        }
    }
    drop(scratch_guard);
    o.cache.trim(CACHE_LIMIT);

    Ok(Summary { out_dir, textures_found: ok, textures_missing: missing })
}

/// Name of the first mesh in a model .gr2 that has one (the same name Noesis uses for the OBJ group).
pub fn mesh_name(blob: &[u8]) -> Option<String> {
    let g = Gr2::parse(blob)?;
    let (rs, ro) = g.root;
    let mc = g.u32(rs, ro + 84)?;
    let marr = g.ptr(rs, ro + 88)?;
    for i in 0..mc.min(4096) {
        let Some(mesh) = g.ptr(marr.0, marr.1 + 8 * i) else { continue };
        if let Some(n) = g.ptr(mesh.0, mesh.1).and_then(|p| g.cstr(p)) {
            let n = n.trim();
            if !n.is_empty() {
                return Some(n.to_string());
            }
        }
    }
    None
}

/// Mesh name of each model in `ids` (MNF FileIndexes), read from the model's .gr2. All models are read in one pass over
/// the archive and dropped as soon as their name is taken. Models that can't be read are simply left out of the
/// result. Needs no Noesis.
pub fn mesh_names(ids: &[u32], o: &Options) -> HashMap<u32, String> {
    let mut out = HashMap::new();
    if o.dry_run || ids.is_empty() {
        return out;
    }
    let want: HashSet<u32> = ids.iter().copied().collect();
    let rows = lookup_file_indexes(o.manifest, &want);
    let mut by_index: HashMap<u32, Vec<u32>> = HashMap::new(); // entry index -> model ids
    let mut not_in_manifest = 0usize;
    for id in &want {
        match rows.get(id).and_then(|v| v.first()) {
            Some(r) => by_index.entry(r.index).or_default().push(*id),
            None => not_in_manifest += 1,
        }
    }
    let mut wanted: Vec<u32> = by_index.keys().copied().collect();
    wanted.sort_unstable();
    println!("{} model(s): reading them in one pass.", wanted.len());

    let mut unreadable = 0usize;
    o.archive.read_each(&wanted, |e, data| {
        let name = data.ok().and_then(|blob| mesh_name(&blob));
        for id in by_index.get(&e.index).into_iter().flatten() {
            match &name {
                Some(n) => { out.insert(*id, n.clone()); }
                None => unreadable += 1,
            }
        }
    });
    println!(
        "Mesh names: {} of {} model(s) named ({} not in the manifest, {} without a readable mesh name).",
        out.len(), ids.len(), not_in_manifest, unreadable
    );
    out
}

pub fn run_batch(ids: &[u32], o: &Options) -> Vec<(u32, Result<Summary, String>)> {
    if !o.dry_run && !ids.is_empty() {
        // All model files in one pass; textures are read per model (and kept while shared).
        let want: HashSet<u32> = ids.iter().copied().collect();
        let rows = lookup_file_indexes(o.manifest, &want);
        let mut idx: Vec<u32> = ids.iter().filter_map(|id| rows.get(id).and_then(|v| v.first()).map(|r| r.index)).collect();
        idx.sort_unstable();
        idx.dedup();
        println!("{} model(s): reading the model files in one pass.", idx.len());
        o.cache.prefetch(o.archive, &idx);
    }
    let mut results = Vec::with_capacity(ids.len());
    for (n, id) in ids.iter().enumerate() {
        println!("\n=== Model {}/{}: 0x{:x} ===", n + 1, ids.len(), id);
        results.push((*id, run(*id, o)));
    }
    results
}
