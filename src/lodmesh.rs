//! Terrain LOD mesh files: manifest file type `0x0A` ("terrain-lod" in the world TOC).
//!
//! One file per 2x2 block of terrain cells (only even x/z exist in the manifest). Each file is a decimated copy of
//! the heightmap as a triangle mesh, so it does NOT add height data that the land files don't already have - it's
//! useful as a ready-made far-terrain mesh (65x65 grid + a skirt around the edge to hide cracks between blocks).
//!
//! All little-endian, packed (no alignment):
//!
//!     u32  magic        0xDEBEEF01
//!     u8   unknown      0
//!     u32  file_index   (x << 16) | z  -- same as the manifest FileIndex (block origin cell)
//!     u8   num_records  2
//!     num_records * { u8 channel, u32 offset, u32 length }   (offset 0 / length 0 = empty)
//!
//! Observed: channel 0 holds the mesh, channel 1 is empty in the 88,188-byte files. The 176,348-byte files
//! (x and z both multiples of 4) are exactly 2x as long, so they almost certainly carry a second mesh in channel 1
//! (probably a coarser level covering 4x4 cells). Not verified - no such file has been inspected yet; any record
//! that doesn't parse as the layout below is skipped with a warning instead of aborting.
//!
//! Record (`length` bytes, starting at `offset`):
//!
//!     u32  0xDEAD0002   section magic
//!     u32  0x12345678   sentinel
//!     u32  vertex_count            (4485 = 65*65 grid + 260 skirt)
//!     u32  vertex_bytes            (== vertex_count * 8)
//!     vertex_count * { u16 x, u16 h, u16 z, u16 pad(0) }
//!     u32  0x12345678, u32 0x12345678
//!     u32  index_count             (26112 = 8704 triangles = 8192 grid + 512 skirt)
//!     u32  index_bytes             (== index_count * 2)
//!     index_count * u16            triangle list; 3 per triangle, indexes the vertex array
//!     u32  0x12345678
//!     16 bytes unknown             (seen as four big-endian f32: 153.43, 100.0, 59.52, 100.0 - meaning unknown)
//!     u32  0xDEAD0002   footer magic
//!
//! * Vertices 0..4225 are the 65x65 grid, `index = row * 65 + col`, `x = 2 * col`, `z = 2 * row`. Row 0 (z = 0) is
//!   a "no data" row at h = 0x8001. Vertices 4225.. are skirt copies of the border with a lowered height.
//! * Horizontal unit = ONE HEIGHTMAP SAMPLE (1/64 of a cell), measured from the block's origin cell, so a 2x2 block
//!   spans 0..128. x grows with the land-file column and z with the land-file row (row 0 = low z), same as the
//!   cell grid.
//! * Height: `y = (h - 32769) * 1000 / 65535` - the same units as land-file channel 0, with h = 0x8001 meaning 0.0
//!   (that's the "no data" row). Checked against the overlapping edges of land tiles (1,0) and (2,2): 65 samples,
//!   mean error +0.0005, max error 0.0076 = exactly half a 16-bit step, i.e. the mesh is the heightmap rounded
//!   to 16 bits and decimated 2:1.
//! * Triangles are wound counter-clockwise seen from +Y (normal = (b-a) x (c-a) points up) and are kept as stored.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub const FILE_MAGIC: u32 = 0xDEBE_EF01;
pub const SECTION_MAGIC: u32 = 0xDEAD_0002;
pub const SENTINEL: u32 = 0x1234_5678;

const HEADER_FIXED: usize = 10; // magic(4) + unknown(1) + file_index(4) + num_records(1)
const RECORD_INFO: usize = 9; // channel(1) + offset(4) + length(4)
const VERTEX_SIZE: usize = 8;
/// Vertices per side of the main grid; anything after `GRID * GRID` is skirt.
pub const GRID: usize = 65;
/// Heightmap samples per cell (64 quads, 65 samples).
const SAMPLES_PER_CELL: f64 = 64.0;

#[derive(Debug, Clone, Copy)]
pub struct Vertex {
    pub x: u16,
    pub h: u16,
    pub z: u16,
}

#[derive(Debug, Clone)]
#[allow(dead_code)] // `trailer` isn't used yet
pub struct Section {
    pub channel: u8,
    pub verts: Vec<Vertex>,
    pub indices: Vec<u16>,
    /// The 16 bytes before the footer; purpose unknown, kept so they can be looked at later.
    pub trailer: [u8; 16],
}

#[derive(Debug, Clone)]
pub struct LodFile {
    pub file_index: u32,
    pub sections: Vec<Section>,
    /// Records that were present but didn't parse: (channel, reason).
    pub skipped: Vec<(u8, String)>,
}

/// Quantised height -> world height. 0x8001 is "zero"; one step is 1000/65535 = ~0.01526 units.
pub fn height(h: u16) -> f64 {
    (h as f64 - 32769.0) * 1000.0 / 65535.0
}

fn bad(msg: String) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

/// Bounds-checked little-endian reader; never panics on a short/corrupt file.
struct Cur<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], String> {
        let end = self.p.checked_add(n).filter(|&e| e <= self.b.len()).ok_or_else(|| {
            format!("unexpected end of data at +{} (wanted {} bytes, {} left)", self.p, n, self.b.len().saturating_sub(self.p))
        })?;
        let s = &self.b[self.p..end];
        self.p = end;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, String> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16, String> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32, String> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn expect_u32(&mut self, want: u32, what: &str) -> Result<(), String> {
        let got = self.u32()?;
        if got != want {
            return Err(format!("{}: expected 0x{:08X}, found 0x{:08X} (at +{})", what, want, got, self.p - 4));
        }
        Ok(())
    }
}

fn parse_section(rec: &[u8], channel: u8) -> Result<Section, String> {
    let mut c = Cur { b: rec, p: 0 };
    c.expect_u32(SECTION_MAGIC, "section magic")?;
    c.expect_u32(SENTINEL, "sentinel after magic")?;

    let vcount = c.u32()? as usize;
    let vbytes = c.u32()? as usize;
    if vcount.checked_mul(VERTEX_SIZE) != Some(vbytes) {
        return Err(format!("vertex_bytes {} != vertex_count {} * {}", vbytes, vcount, VERTEX_SIZE));
    }
    let raw = c.take(vbytes)?;
    let verts: Vec<Vertex> = raw
        .chunks_exact(VERTEX_SIZE)
        .map(|v| Vertex {
            x: u16::from_le_bytes([v[0], v[1]]),
            h: u16::from_le_bytes([v[2], v[3]]),
            z: u16::from_le_bytes([v[4], v[5]]),
        })
        .collect();

    c.expect_u32(SENTINEL, "sentinel before indices (1)")?;
    c.expect_u32(SENTINEL, "sentinel before indices (2)")?;
    let icount = c.u32()? as usize;
    let ibytes = c.u32()? as usize;
    if icount.checked_mul(2) != Some(ibytes) {
        return Err(format!("index_bytes {} != index_count {} * 2", ibytes, icount));
    }
    if icount % 3 != 0 {
        return Err(format!("index_count {} is not a multiple of 3", icount));
    }
    let mut indices = Vec::with_capacity(icount);
    for _ in 0..icount {
        indices.push(c.u16()?);
    }
    if let Some(&m) = indices.iter().max() {
        if m as usize >= verts.len() {
            return Err(format!("index {} out of range ({} vertices)", m, verts.len()));
        }
    }

    c.expect_u32(SENTINEL, "sentinel before trailer")?;
    let trailer: [u8; 16] = c.take(16)?.try_into().unwrap();
    c.expect_u32(SECTION_MAGIC, "footer magic")?;
    if c.p != rec.len() {
        return Err(format!("{} unexpected trailing byte(s) after footer", rec.len() - c.p));
    }
    Ok(Section { channel, verts, indices, trailer })
}

pub fn parse_bytes(b: &[u8]) -> Result<LodFile, String> {
    let mut c = Cur { b, p: 0 };
    c.expect_u32(FILE_MAGIC, "file magic")?;
    let _unknown = c.u8()?;
    let file_index = c.u32()?;
    let n = c.u8()? as usize;
    debug_assert_eq!(c.p, HEADER_FIXED);

    let mut recs = Vec::with_capacity(n);
    for _ in 0..n {
        let ch = c.u8()?;
        let off = c.u32()? as usize;
        let len = c.u32()? as usize;
        recs.push((ch, off, len));
    }
    debug_assert_eq!(c.p, HEADER_FIXED + n * RECORD_INFO);

    let mut out = LodFile { file_index, sections: Vec::new(), skipped: Vec::new() };
    for (ch, off, len) in recs {
        if off == 0 || len == 0 {
            continue;
        }
        let Some(rec) = off.checked_add(len).and_then(|end| b.get(off..end)) else {
            out.skipped.push((ch, format!("record (offset 0x{:X}, length {}) runs past the end of the file", off, len)));
            continue;
        };
        match parse_section(rec, ch) {
            Ok(s) => out.sections.push(s),
            Err(e) => out.skipped.push((ch, e)),
        }
    }
    Ok(out)
}

/// Writes one section as a Wavefront OBJ. Vertices are placed in world units so neighbouring blocks line up when
/// imported together: `X = (cell_x * 64 + x) * cell_size / 64`, `Z` likewise, `Y` = height.
/// Grid triangles go in `g terrain`, skirt triangles in `g skirt` (or are left out with `skirts == false`).
pub fn write_obj(sec: &Section, cell_x: u32, cell_z: u32, cell_size: f64, skirts: bool, out: &Path) -> io::Result<()> {
    use std::fmt::Write as _;
    let unit = if cell_size > 0.0 { cell_size / SAMPLES_PER_CELL } else { 1.0 };
    let (ox, oz) = if cell_size > 0.0 {
        (cell_x as f64 * SAMPLES_PER_CELL, cell_z as f64 * SAMPLES_PER_CELL)
    } else {
        (0.0, 0.0)
    };

    let grid_verts = GRID * GRID;
    let mut s = String::with_capacity(sec.verts.len() * 40 + sec.indices.len() * 8 + 256);
    let _ = writeln!(s, "# terrain LOD mesh, block origin cell ({}, {}), channel {}", cell_x, cell_z, sec.channel);
    let _ = writeln!(s, "# {} vertices ({} grid + {} skirt), {} triangles; 1 unit = {:.5} world units per heightmap sample",
        sec.verts.len(), sec.verts.len().min(grid_verts), sec.verts.len().saturating_sub(grid_verts), sec.indices.len() / 3, unit);
    let _ = writeln!(s, "o lod_x{}_z{}_ch{}", cell_x, cell_z, sec.channel);
    for v in &sec.verts {
        let _ = writeln!(s, "v {:.3} {:.3} {:.3}", (ox + v.x as f64) * unit, height(v.h), (oz + v.z as f64) * unit);
    }
    for (name, want_skirt) in [("terrain", false), ("skirt", true)] {
        if want_skirt && !skirts {
            continue;
        }
        let mut header_done = false;
        for t in sec.indices.chunks_exact(3) {
            let is_skirt = t.iter().any(|&i| i as usize >= grid_verts);
            if is_skirt != want_skirt {
                continue;
            }
            if !header_done {
                let _ = writeln!(s, "g {}", name);
                header_done = true;
            }
            let _ = writeln!(s, "f {} {} {}", t[0] as u32 + 1, t[1] as u32 + 1, t[2] as u32 + 1);
        }
    }

    if let Some(dir) = out.parent() {
        fs::create_dir_all(dir)?;
    }
    fs::write(out, s)
}

/// Parses one exported `.dat` and writes `<out_dir>/lod_x<X>_z<Z>_ch<N>.obj` for every record that parses.
/// Returns the files written; `Err` only if nothing could be written.
pub fn export_file(bytes: &[u8], label: &str, cell_x: u32, cell_z: u32, cell_size: f64, skirts: bool, out_dir: &Path) -> io::Result<Vec<PathBuf>> {
    let lf = parse_bytes(bytes).map_err(|e| bad(format!("{}: {}", label, e)))?;
    let expected = (cell_x << 16) | cell_z;
    if lf.file_index != expected {
        eprintln!(
            "\tWarning: '{}' header says cell 0x{:08X}, manifest says 0x{:08X}; using the manifest.",
            label, lf.file_index, expected
        );
    }
    for (ch, why) in &lf.skipped {
        eprintln!("\tWarning: '{}' record for channel {} skipped: {}", label, ch, why);
    }
    let mut written = Vec::new();
    for sec in &lf.sections {
        let out = out_dir.join(format!("lod_x{}_z{}_ch{}.obj", cell_x, cell_z, sec.channel));
        write_obj(sec, cell_x, cell_z, cell_size, skirts, &out)?;
        written.push(out);
    }
    if written.is_empty() {
        return Err(bad(format!("{}: no usable mesh record", label)));
    }
    Ok(written)
}