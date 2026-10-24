//! Terrain layer-mask table (manifest category 0x6000_0000, FileIndex 0x44): one world stitched into tilemaps.
//!
//! Layout of `<index>_Uncompressed.EsoFileData` (big-endian unless noted):
//!
//! ```text
//! file header (16)   FA FA EB EB | u32 version(23) | u32 entry_count | u32 unknown(6)
//! entry (32 + size)  "####" | u32 row | u32 size | u32 size                 (16, separator)
//!                    u32 record_len | u32 key | u32 record_offset | u32 size (16, sub-header)
//!                    payload (`size` bytes, offsets relative to the payload start):
//!                      +0  u32 key
//!                      +41 u32 grid width, +45 u32 grid height   (100x100; 10x10 for kind 7)
//!                      +49 u32 layer count
//!                      +53 layers, each: u32 layer_id | u32 mask_bytes | u32 strlen | base64 (strlen-1 chars) | 00
//! ```
//!
//! key = `world:11 | bx:6 | bz:6 | kind:5` (world = key >> 17, bx = (key >> 11) & 0x3F, bz = (key >> 5) & 0x3F,
//! kind = key & 0x1F). bx/bz are 500-unit blocks of the world. Entries are delimited by their size fields.
//!
//! Layer: base64 -> [u32 LE uncompressed size][u32 LE compressed size][zlib]. The inflated bytes are a 1-bit mask,
//! row-major, no row padding (cell i = bit i % 8 of byte i / 8, x = i % w, y = i / w).
//!
//! Stitching: image x = (bx - min_bx) * w + col, image y = (bz - min_bz) * h + row (tiles abut, none are mirrored
//! or transposed). Within a block layers are drawn in file order, later ones over earlier ones.
//!
//! Kind 2 is the zone map: its layer ids are zone ids, named in en.lang.csv group 162946485.
//!
//! Needs `flate2` in Cargo.toml (inflate): `cargo add flate2`.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::ops::Range;
use std::path::Path;

pub const TERRAIN_LAYER_FILE_INDEX: u32 = 0x44;
const ZONE_MAP_KIND: u32 = 2;
const ZONE_MAP_NAME_GROUP: &str = "162946485";

const MAGIC: [u8; 4] = [0xFA, 0xFA, 0xEB, 0xEB];
const FILE_HEADER_LEN: usize = 16;
const ENTRY_PREFIX_LEN: usize = 32; // separator (16) + sub-header (16)
const PAYLOAD_MIN: usize = 53; // everything up to the layer list

struct Layer {
    id: u32,
    b64: Range<usize>,
}

struct Block {
    bx: u32,
    bz: u32,
    kind: u32,
    w: usize,
    h: usize,
    layers: Vec<Layer>,
}

fn be32(b: &[u8], o: usize) -> u32 {
    u32::from_be_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// All entries of `world`. Only those are parsed; every other entry is skipped by its size field.
fn scan(d: &[u8], world: u32) -> Result<Vec<Block>, String> {
    if d.len() < FILE_HEADER_LEN || d[0..4] != MAGIC {
        return Err("not an EsoFileData table (bad magic).".to_string());
    }
    let count = be32(d, 8) as usize;
    let mut out = Vec::new();
    let mut pos = FILE_HEADER_LEN;

    for n in 0..count {
        if pos + ENTRY_PREFIX_LEN > d.len() || &d[pos..pos + 4] != b"####" {
            return Err(format!("entry {}: expected '####' at offset {}.", n + 1, pos));
        }
        let size = be32(d, pos + 8) as usize;
        if size != be32(d, pos + 12) as usize || size < PAYLOAD_MIN {
            return Err(format!("entry {}: bad size fields.", n + 1));
        }
        let end = pos + ENTRY_PREFIX_LEN + size;
        if end > d.len() {
            return Err(format!("entry {}: payload runs past the end of the file.", n + 1));
        }
        let p = pos + ENTRY_PREFIX_LEN;
        let key = be32(d, p);
        if key >> 17 == world {
            let mut layers = Vec::new();
            let mut q = p + PAYLOAD_MIN;
            for _ in 0..be32(d, p + 49) {
                if q + 12 > end {
                    return Err(format!("entry {}: layer header runs past the entry.", n + 1));
                }
                let strlen = be32(d, q + 8) as usize; // includes the trailing NUL
                let s = q + 12;
                if strlen == 0 || s + strlen > end {
                    return Err(format!("entry {}: layer data runs past the entry.", n + 1));
                }
                layers.push(Layer { id: be32(d, q), b64: s..s + strlen - 1 });
                q = s + strlen;
            }
            out.push(Block {
                bx: (key >> 11) & 0x3F,
                bz: (key >> 5) & 0x3F,
                kind: key & 0x1F,
                w: be32(d, p + 41) as usize,
                h: be32(d, p + 45) as usize,
                layers,
            });
        }
        pos = end;
    }
    Ok(out)
}

fn b64_decode(s: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3 + 3);
    let mut acc: u32 = 0;
    let mut nbits: u32 = 0;
    for &c in s {
        let v = (match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            _ => return None,
        }) as u32;
        acc = (acc << 6) | v;
        nbits += 6;
        if nbits >= 8 {
            nbits -= 8;
            out.push((acc >> nbits) as u8);
            acc &= (1u32 << nbits) - 1;
        }
    }
    Some(out)
}

fn decode_mask(d: &[u8], l: &Layer) -> Result<Vec<u8>, String> {
    let raw = b64_decode(&d[l.b64.clone()]).ok_or("invalid base64 in layer")?;
    if raw.len() < 9 {
        return Err("layer payload too short".to_string());
    }
    let usz = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) as usize;
    let mut out = Vec::with_capacity(usz);
    flate2::read::ZlibDecoder::new(&raw[8..])
        .read_to_end(&mut out)
        .map_err(|e| format!("zlib error: {}", e))?;
    Ok(out)
}

/// Stable colour per layer id (never too dark, so "no data" stays black).
fn id_colour(id: u32) -> [u8; 3] {
    let mut h = id.wrapping_mul(0x9E37_79B1) ^ 0x85EB_CA6B;
    h ^= h >> 15;
    h = h.wrapping_mul(0x2C1B_3C6D);
    h ^= h >> 12;
    [(h & 0xFF) as u8 | 0x40, ((h >> 8) & 0xFF) as u8 | 0x40, ((h >> 16) & 0xFF) as u8 | 0x40]
}

/// Which layer ids also get their own full-size mask image (`k<kind>_layers/id_<id>.png`).
#[derive(Default, Clone)]
pub struct Split {
    /// `None` = no split requested; `Some(empty)` = every kind; `Some(list)` = those kinds.
    pub kinds: Option<Vec<u32>>,
    /// `(kind, id)` pairs; when non-empty only these ids are written.
    pub only: Vec<(u32, u32)>,
}

impl Split {
    fn wants(&self, kind: u32, id: u32) -> bool {
        let kind_ok = match &self.kinds {
            Some(k) => k.is_empty() || k.contains(&kind),
            None => !self.only.is_empty(),
        };
        kind_ok && (self.only.is_empty() || self.only.contains(&(kind, id)))
    }
}

type IdMasks = BTreeMap<u32, Vec<(u32, u32, Vec<u8>)>>; // id -> (bx, bz, 1-bit mask)

fn write_split(kind: u32, masks: &IdMasks, (tw, th): (usize, usize), (cw, ch): (usize, usize), (min_bx, min_bz): (u32, u32),
               names: &HashMap<u32, String>, out_dir: &Path) -> Result<(), String> {
    let dir = out_dir.join(format!("k{}_layers", kind));
    std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create '{}': {}", dir.display(), e))?;
    let csv = dir.join("index.csv");
    let mut f = BufWriter::new(File::create(&csv).map_err(|e| format!("failed to create '{}': {}", csv.display(), e))?);
    let io = |e: std::io::Error| format!("failed to write '{}': {}", csv.display(), e);
    // The viewer reads columns 0-4 and the name from column 10.
    writeln!(f, "layer_id,r,g,b,pixels,x0,y0,x1,y1,blocks,name").map_err(io)?;
    let mut written = 0usize;
    for (id, parts) in masks {
        let mut img = vec![0u8; cw * ch];
        let (mut n, mut x0, mut y0, mut x1, mut y1) = (0u64, usize::MAX, usize::MAX, 0usize, 0usize);
        for (bx, bz, mask) in parts {
            for r in 0..th {
                let y = (bz - min_bz) as usize * th + r;
                for c in 0..tw {
                    let i = r * tw + c;
                    if mask.get(i / 8).map_or(false, |v| (v >> (i % 8)) & 1 == 1) {
                        let x = (bx - min_bx) as usize * tw + c;
                        img[y * cw + x] = 255;
                        n += 1;
                        x0 = x0.min(x); x1 = x1.max(x); y0 = y0.min(y); y1 = y1.max(y);
                    }
                }
            }
        }
        if n == 0 { continue; }
        let png = dir.join(format!("id_{}.png", id));
        image::GrayImage::from_raw(cw as u32, ch as u32, img)
            .ok_or("split buffer size mismatch")?
            .save(&png)
            .map_err(|e| format!("failed to save '{}': {}", png.display(), e))?;
        let c = id_colour(*id);
        let name = names.get(id).map(String::as_str).unwrap_or("");
        writeln!(f, "{},{},{},{},{},{},{},{},{},{},{}", id, c[0], c[1], c[2], n, x0, y0, x1, y1, parts.len(), crate::worldlut::csv_field(name)).map_err(io)?;
        written += 1;
    }
    f.flush().map_err(io)?;
    println!("  kind {}: {} per-id mask image(s) -> {}", kind, written, dir.display());
    Ok(())
}

/// Write `k<kind>_ids.png` (pixel colour = layer id) and `k<kind>_legend.csv` (layer_id, colour, pixels, name)
/// for every kind of `world` into `out_dir`. `lang` is en.lang; it names the kind-2 (zone map) ids.
pub fn run(data: &[u8], lang: Option<&crate::archive::LangTable>, world: u32, out_dir: &Path, split: &Split) -> Result<(), String> {
    let blocks = scan(data, world)?;
    if blocks.is_empty() {
        return Err(format!("no terrain layer entries for world 0x{:x} ({}).", world, world));
    }

    let zone_names: HashMap<u32, String> = match lang {
        Some(l) => {
            let n = l.group(ZONE_MAP_NAME_GROUP).clone();
            println!("Loaded {} zone names from en.lang", n.len());
            n
        }
        None => {
            eprintln!("Warning: en.lang not available; zone names will be empty.");
            HashMap::new()
        }
    };

    // one canvas origin for every kind, so all the images line up
    let min_bx = blocks.iter().map(|b| b.bx).min().unwrap();
    let max_bx = blocks.iter().map(|b| b.bx).max().unwrap();
    let min_bz = blocks.iter().map(|b| b.bz).min().unwrap();
    let max_bz = blocks.iter().map(|b| b.bz).max().unwrap();

    std::fs::create_dir_all(out_dir).map_err(|e| format!("failed to create '{}': {}", out_dir.display(), e))?;

    let mut by_kind: BTreeMap<u32, Vec<&Block>> = BTreeMap::new();
    for b in &blocks {
        by_kind.entry(b.kind).or_default().push(b);
    }

    for (kind, bs) in by_kind {
        let (tw, th) = (bs[0].w, bs[0].h);
        if tw == 0 || th == 0 || bs.iter().any(|b| b.w != tw || b.h != th) {
            eprintln!("  kind {}: tile sizes differ, skipped.", kind);
            continue;
        }
        let cw = (max_bx - min_bx + 1) as usize * tw;
        let ch = (max_bz - min_bz + 1) as usize * th;
        let mut rgb = vec![0u8; cw * ch * 3];
        let mut counts: BTreeMap<u32, u64> = BTreeMap::new();
        let mut split_masks: IdMasks = BTreeMap::new();

        for b in &bs {
            for l in &b.layers {
                let mask = decode_mask(&data, l)?;
                let col = id_colour(l.id);
                let mut n = 0u64;
                for r in 0..th {
                    let y = (b.bz - min_bz) as usize * th + r;
                    for c in 0..tw {
                        let i = r * tw + c;
                        if mask.get(i / 8).map_or(false, |v| (v >> (i % 8)) & 1 == 1) {
                            let o = (y * cw + (b.bx - min_bx) as usize * tw + c) * 3;
                            rgb[o..o + 3].copy_from_slice(&col);
                            n += 1;
                        }
                    }
                }
                *counts.entry(l.id).or_insert(0) += n;
                if split.wants(kind, l.id) {
                    split_masks.entry(l.id).or_default().push((b.bx, b.bz, mask));
                }
            }
        }

        let png = out_dir.join(format!("k{}_ids.png", kind));
        image::RgbImage::from_raw(cw as u32, ch as u32, rgb)
            .ok_or("stitched buffer size mismatch")?
            .save(&png)
            .map_err(|e| format!("failed to save '{}': {}", png.display(), e))?;

        let csv = out_dir.join(format!("k{}_legend.csv", kind));
        let mut f = BufWriter::new(File::create(&csv).map_err(|e| format!("failed to create '{}': {}", csv.display(), e))?);
        let io = |e: std::io::Error| format!("failed to write '{}': {}", csv.display(), e);
        writeln!(f, "layer_id,r,g,b,pixels,name").map_err(io)?;
        let mut unnamed = 0usize;
        for (id, n) in &counts {
            let c = id_colour(*id);
            let name = if kind == ZONE_MAP_KIND {
                match zone_names.get(id) {
                    Some(s) => s.as_str(),
                    None => { unnamed += 1; "" }
                }
            } else {
                ""
            };
            writeln!(f, "{},{},{},{},{},{}", id, c[0], c[1], c[2], n, crate::worldlut::csv_field(name)).map_err(io)?;
        }
        f.flush().map_err(io)?;


        if !split_masks.is_empty() {
            let empty = HashMap::new();
            let names = if kind == ZONE_MAP_KIND { &zone_names } else { &empty };
            write_split(kind, &split_masks, (tw, th), (cw, ch), (min_bx, min_bz), names, out_dir)?;
        }
        for (k, id) in &split.only {
            if *k == kind && !split_masks.contains_key(id) {
                eprintln!("  kind {}: layer id {} not present in this world.", kind, id);
            }
        }
        print!("kind {}: {}x{} px, {} layer id(s) -> {}", kind, cw, ch, counts.len(), png.display());
        if kind == ZONE_MAP_KIND {
            print!(" ({} without a name)", unnamed);
        }
        println!();
    }
    Ok(())
}
