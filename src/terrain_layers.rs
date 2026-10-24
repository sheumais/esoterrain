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

/// Write `k<kind>_ids.png` (pixel colour = layer id) and `k<kind>_legend.csv` (layer_id, colour, pixels, name)
/// for every kind of `world` into `out_dir`. `lang` is en.lang.csv; it names the kind-2 (zone map) ids.
pub fn run(table: &Path, lang: Option<&Path>, world: u32, out_dir: &Path, only: &[(u32, u32)], split: Option<&[u32]>) -> Result<(), String> {
    println!("Parsing file: {}", table.display());
    let data = std::fs::read(table).map_err(|e| format!("failed to read '{}': {}", table.display(), e))?;
    let blocks = scan(&data, world)?;
    if blocks.is_empty() {
        return Err(format!("no terrain layer entries for world 0x{:x} ({}).", world, world));
    }

    // single-value mode: skip the full ids/legend export (and the lang file) and write one mask image per (kind, id)
    if !only.is_empty() {
        return run_only(&data, &blocks, out_dir, only);
    }

    let zone_names: HashMap<u32, String> = match lang {
        Some(p) => {
            let n = crate::worldlut::load_lang_group(p, ZONE_MAP_NAME_GROUP)
                .map_err(|e| format!("failed to read '{}': {}", p.display(), e))?;
            println!("Loaded {} zone names from {}", n.len(), p.display());
            n
        }
        None => {
            eprintln!("Warning: en.lang.csv not found; zone names will be empty.");
            HashMap::new()
        }
    };

    // split mode: one file per layer id (kind-2 ids are named from the lang file)
    if let Some(kinds) = split {
        return run_split(&data, &blocks, out_dir, kinds, &zone_names);
    }

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

        print!("kind {}: {}x{} px, {} layer id(s) -> {}", kind, cw, ch, counts.len(), png.display());
        if kind == ZONE_MAP_KIND {
            print!(" ({} without a name)", unnamed);
        }
        println!();
    }
    Ok(())
}

/// Single-value mode: for every requested (kind, layer id) write `k<kind>_id<id>.png`, a white-on-black mask of the cells
/// where that layer id is set. The canvas origin/size is the same as the full `k<kind>_ids.png` images (it spans every
/// block of the world), so the masks line up with them. Unlike the full image, nothing is drawn over the mask by later
/// layers, so it is the raw layer data.
fn run_only(data: &[u8], blocks: &[Block], out_dir: &Path, only: &[(u32, u32)]) -> Result<(), String> {
    let min_bx = blocks.iter().map(|b| b.bx).min().unwrap();
    let max_bx = blocks.iter().map(|b| b.bx).max().unwrap();
    let min_bz = blocks.iter().map(|b| b.bz).min().unwrap();
    let max_bz = blocks.iter().map(|b| b.bz).max().unwrap();
    std::fs::create_dir_all(out_dir).map_err(|e| format!("failed to create '{}': {}", out_dir.display(), e))?;

    let mut wanted: Vec<(u32, u32)> = only.to_vec();
    wanted.sort_unstable();
    wanted.dedup();

    for (kind, id) in wanted {
        let bs: Vec<&Block> = blocks.iter().filter(|b| b.kind == kind).collect();
        if bs.is_empty() {
            eprintln!("  kind {}: no entries in this world, skipped.", kind);
            continue;
        }
        let (tw, th) = (bs[0].w, bs[0].h);
        if tw == 0 || th == 0 || bs.iter().any(|b| b.w != tw || b.h != th) {
            eprintln!("  kind {}: tile sizes differ, skipped.", kind);
            continue;
        }
        let cw = (max_bx - min_bx + 1) as usize * tw;
        let ch = (max_bz - min_bz + 1) as usize * th;
        let mut px = vec![0u8; cw * ch];
        let (mut cells, mut tiles) = (0u64, 0usize);

        for b in &bs {
            let mut hit = false;
            for l in b.layers.iter().filter(|l| l.id == id) {
                let mask = decode_mask(data, l)?;
                for r in 0..th {
                    let y = (b.bz - min_bz) as usize * th + r;
                    for c in 0..tw {
                        let i = r * tw + c;
                        if mask.get(i / 8).map_or(false, |v| (v >> (i % 8)) & 1 == 1) {
                            px[y * cw + (b.bx - min_bx) as usize * tw + c] = 255;
                            cells += 1;
                            hit = true;
                        }
                    }
                }
            }
            if hit {
                tiles += 1;
            }
        }

        if cells == 0 {
            eprintln!("  kind {} id {}: not present in this world (nothing written).", kind, id);
            continue;
        }
        let png = out_dir.join(format!("k{}_id{}.png", kind, id));
        image::GrayImage::from_raw(cw as u32, ch as u32, px)
            .ok_or("stitched buffer size mismatch")?
            .save(&png)
            .map_err(|e| format!("failed to save '{}': {}", png.display(), e))?;
        println!("kind {} id {}: {} cell(s) in {} block(s), {}x{} px -> {}", kind, id, cells, tiles, cw, ch, png.display());
    }
    Ok(())
}

/// Split mode: for every layer id of the requested kinds (all kinds when `kinds` is empty) write
/// `k<kind>_layers/id_<id>.png` (white = set, black = not; same canvas as the full `k<kind>_ids.png`) and an
/// `index.csv` (layer_id,r,g,b,pixels,blocks,x0,y0,x1,y1,name) beside them. Every layer is drawn on its own, so
/// overlapping ids no longer hide each other as they do in the stitched image.
fn run_split(data: &[u8], blocks: &[Block], out_dir: &Path, kinds: &[u32], zone_names: &HashMap<u32, String>) -> Result<(), String> {
    let min_bx = blocks.iter().map(|b| b.bx).min().unwrap();
    let max_bx = blocks.iter().map(|b| b.bx).max().unwrap();
    let min_bz = blocks.iter().map(|b| b.bz).min().unwrap();
    let max_bz = blocks.iter().map(|b| b.bz).max().unwrap();

    let mut by_kind: BTreeMap<u32, Vec<&Block>> = BTreeMap::new();
    for b in blocks {
        if kinds.is_empty() || kinds.contains(&b.kind) {
            by_kind.entry(b.kind).or_default().push(b);
        }
    }
    if by_kind.is_empty() {
        return Err("none of the requested kinds exist in this world.".to_string());
    }

    for (kind, bs) in by_kind {
        let (tw, th) = (bs[0].w, bs[0].h);
        if tw == 0 || th == 0 || bs.iter().any(|b| b.w != tw || b.h != th) {
            eprintln!("  kind {}: tile sizes differ, skipped.", kind);
            continue;
        }
        let cw = (max_bx - min_bx + 1) as usize * tw;
        let ch = (max_bz - min_bz + 1) as usize * th;

        // layer id -> every (block, layer) that carries it, so each layer is inflated exactly once
        let mut by_id: BTreeMap<u32, Vec<(&Block, &Layer)>> = BTreeMap::new();
        for b in &bs {
            for l in &b.layers {
                by_id.entry(l.id).or_default().push((b, l));
            }
        }

        let dir = out_dir.join(format!("k{}_layers", kind));
        std::fs::create_dir_all(&dir).map_err(|e| format!("failed to create '{}': {}", dir.display(), e))?;
        let csv = dir.join("index.csv");
        let mut f = BufWriter::new(File::create(&csv).map_err(|e| format!("failed to create '{}': {}", csv.display(), e))?);
        let io = |e: std::io::Error| format!("failed to write '{}': {}", csv.display(), e);
        writeln!(f, "layer_id,r,g,b,pixels,blocks,x0,y0,x1,y1,name").map_err(io)?;

        let mut px = vec![0u8; cw * ch];
        let mut written = 0usize;
        for (id, parts) in &by_id {
            px.iter_mut().for_each(|p| *p = 0);
            let (mut cells, mut tiles) = (0u64, 0usize);
            let (mut x0, mut y0, mut x1, mut y1) = (usize::MAX, usize::MAX, 0usize, 0usize);
            for (b, l) in parts {
                let mask = decode_mask(data, l)?;
                let mut hit = false;
                for r in 0..th {
                    let y = (b.bz - min_bz) as usize * th + r;
                    for c in 0..tw {
                        let i = r * tw + c;
                        if mask.get(i / 8).map_or(false, |v| (v >> (i % 8)) & 1 == 1) {
                            let x = (b.bx - min_bx) as usize * tw + c;
                            px[y * cw + x] = 255;
                            cells += 1;
                            hit = true;
                            x0 = x0.min(x); y0 = y0.min(y); x1 = x1.max(x); y1 = y1.max(y);
                        }
                    }
                }
                if hit { tiles += 1; }
            }
            let col = id_colour(*id);
            let name = if kind == ZONE_MAP_KIND { zone_names.get(id).map(|s| s.as_str()).unwrap_or("") } else { "" };
            if cells == 0 {
                writeln!(f, "{},{},{},{},0,0,,,,,{}", id, col[0], col[1], col[2], crate::worldlut::csv_field(name)).map_err(io)?;
                continue;
            }
            let png = dir.join(format!("id_{}.png", id));
            image::GrayImage::from_raw(cw as u32, ch as u32, px.clone())
                .ok_or("stitched buffer size mismatch")?
                .save(&png)
                .map_err(|e| format!("failed to save '{}': {}", png.display(), e))?;
            writeln!(f, "{},{},{},{},{},{},{},{},{},{},{}", id, col[0], col[1], col[2], cells, tiles, x0, y0, x1, y1, crate::worldlut::csv_field(name)).map_err(io)?;
            written += 1;
        }
        f.flush().map_err(io)?;
        println!("kind {}: {}x{} px, {} layer id(s) -> {} file(s) in {}", kind, cw, ch, by_id.len(), written, dir.display());
    }
    Ok(())
}