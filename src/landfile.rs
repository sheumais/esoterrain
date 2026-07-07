use crate::reader::BinReader;
use image::{ImageBuffer, Luma};
use std::io;
use std::path::{Path, PathBuf};

const MAGIC_HEADER: u32 = 0xBEEF000C;
const UNKNOWN3_SIZE: usize = 20;

#[derive(Debug, Default, Clone)]
struct RecInfo {
    _index: u8,
    offset: u32,
    length: u32,
}

#[derive(Debug, Default)]
struct Header {
    magic: u32,
    unknown1: u8,
    magic2: u32,
    num_records: u8,
    rec_infos: Vec<RecInfo>,
    unknown2: u16,
    unknown3: [u32; UNKNOWN3_SIZE],
}

#[derive(Debug, Default, Clone)]
struct LandData {
    magic: u32,
    height: u32,
    height2: u32,
    width: u32,
    data: Vec<u32>,
    footer: u32,
}

pub struct LandFile {
    filename: PathBuf,
    header: Header,
    data: Vec<LandData>,
}

impl LandFile {
    pub fn new() -> Self {
        Self {
            filename: PathBuf::new(),
            header: Header::default(),
            data: Vec::new(),
        }
    }

    pub fn is_land_file<P: AsRef<Path>>(path: P) -> bool {
        match BinReader::open(&path).and_then(|mut r| r.read_u32()) {
            Ok(magic) => magic == MAGIC_HEADER,
            Err(_) => false,
        }
    }

    pub fn load<P: AsRef<Path>>(&mut self, path: P) -> io::Result<()> {
        self.filename = path.as_ref().to_path_buf();
        let mut reader = BinReader::open(&path)?;

        self.read_header(&mut reader)?;
        self.data = vec![LandData::default(); self.header.num_records as usize];

        for i in 0..self.header.num_records as usize {
            self.read_data_section(&mut reader, i)?;
        }

        Ok(())
    }

    fn read_header(&mut self, reader: &mut BinReader) -> io::Result<()> {
        let h = &mut self.header;

        h.magic = reader.read_u32()?;
        if h.magic != MAGIC_HEADER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file is not a recognized ESO land data file",
            ));
        }

        h.unknown1 = reader.read_u8()?;
        h.magic2 = reader.read_u32()?;
        h.num_records = reader.read_u8()?;

        if h.num_records != 10 {
            eprintln!(
                "\tWarning: expected 10 records, found {}",
                h.num_records
            );
        }

        h.rec_infos = Vec::with_capacity(h.num_records as usize);
        for _ in 0..h.num_records {
            h.rec_infos.push(RecInfo {
                _index: reader.read_u8()?,
                offset: reader.read_u32()?,
                length: reader.read_u32()?,
            });
        }

        h.unknown2 = reader.read_u16()?;
        for i in 0..UNKNOWN3_SIZE {
            h.unknown3[i] = reader.read_u32()?;
        }

        Ok(())
    }

    fn read_data_section(&mut self, reader: &mut BinReader, index: usize) -> io::Result<()> {
        let rec_info = self.header.rec_infos[index].clone();

        if rec_info.offset == 0 || rec_info.length == 0 {
            return Ok(());
        }

        reader.seek(rec_info.offset as u64).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!(
                    "failed to jump to start of data section #{} (offset 0x{:08X})",
                    index, rec_info.offset
                ),
            )
        })?;

        let mut data = LandData::default();
        data.magic = reader.read_u32()?;
        data.height = reader.read_u32()?;
        data.height2 = reader.read_u32()?;
        data.width = reader.read_u32()?;

        if data.height != data.height2 {
            eprintln!(
                "\tWarning: Height mismatch in data section #{} ({} != {})",
                index, data.height, data.height2
            );
        }

        let data_size = data.height as usize * data.width as usize;
        let rec_size = 16 + data.height as usize * (data.width as usize + 2) + 4;
        if rec_info.length as usize != rec_size {
            eprintln!(
                "\tWarning: Data section #{} has record length mismatch ({} != {})",
                index, rec_info.length, rec_size
            );
        }

        data.data = vec![0u32; data_size / 4];

        for y in 0..data.height as usize {
            let width = reader.read_u16()? as u32;
            if width != data.width {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "row width mismatch in data section #{}:{} ({} != {})",
                        index, y, width, data.width
                    ),
                ));
            }

            for x in 0..(data.width as usize) / 4 {
                data.data[x + y * (data.width as usize) / 4] = reader.read_u32()?;
            }
        }

        data.footer = reader.read_u32()?;
        self.data[index] = data;
        Ok(())
    }

    pub fn heightmap_tile(&self) -> Option<(Vec<u16>, u32, u32)> {
        let rec_info = self.header.rec_infos.get(0)?;
        if rec_info.offset == 0 || rec_info.length == 0 {
            return None;
        }
        let section = &self.data[0];
        if section.width == 0 || section.height == 0 {
            return None;
        }
        let tex_width = section.width / 4;
        let tex_height = section.height;
        let tmp_data = normalize_section_to_u16(&section.data, 0);
        Some((tmp_data, tex_width, tex_height))
    }
}

fn is_float_channel(index: usize) -> bool {
    matches!(index, 0 | 6)
}

fn fixed_channel_range(index: usize) -> Option<(f64, f64)> {
    match index {
        0 => Some((0.0, 1000.0)),
        6 => Some((-1.0, 1.0)),
        _ => None,
    }
}

fn normalize_section_to_u16(data: &[u32], index: usize) -> Vec<u16> {
    let is_float = is_float_channel(index);

    let values: Vec<f64> = data
        .iter()
        .map(|&dword| {
            if is_float {
                f32::from_bits(dword) as f64
            } else {
                dword as i32 as f64
            }
        })
        .collect();

    let (min, max) = fixed_channel_range(index).unwrap_or_else(|| {
        values
            .iter()
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(mn, mx), &v| {
                (mn.min(v), mx.max(v))
            })
    });

    let range = if max > min { max - min } else { 1.0 };

    values
        .iter()
        .map(|&v| {
            (((v - min) / range) * 65535.0)
                .round()
                .clamp(0.0, 65535.0) as u16
        })
        .collect()
}

pub struct BigImage {
    pixels: Vec<u16>,
    dim_x: u32,
    dim_y: u32,
    tile_w: u32,
    tile_h: u32,
    grid_w: u32,
    grid_h: u32,
    initialized: bool,
}

impl BigImage {
    pub fn new(grid_w: u32, grid_h: u32) -> Self {
        Self {
            pixels: Vec::new(),
            dim_x: 0,
            dim_y: 0,
            tile_w: 0,
            tile_h: 0,
            grid_w: grid_w.max(1),
            grid_h: grid_h.max(1),
            initialized: false,
        }
    }

    fn ensure_initialized(&mut self, tile_w: u32, tile_h: u32) {
        if self.initialized {
            return;
        }
        self.tile_w = tile_w;
        self.tile_h = tile_h;
        self.dim_x = tile_w * self.grid_w;
        self.dim_y = tile_h * self.grid_h;
        self.pixels = vec![0u16; (self.dim_x as usize) * (self.dim_y as usize)];
        self.initialized = true;
    }

    pub fn set_tile_at(
        &mut self,
        col: u32,
        row: u32,
        tile: &[u16],
        tile_w: u32,
        tile_h: u32,
    ) -> bool {
        if tile_w == 0 || tile_h == 0 || tile.len() != (tile_w * tile_h) as usize {
            eprintln!(
                "\tWarning: skipping tile with inconsistent dimensions ({} px for {}x{})",
                tile.len(),
                tile_w,
                tile_h
            );
            return false;
        }

        self.ensure_initialized(tile_w, tile_h);

        if tile_w != self.tile_w || tile_h != self.tile_h {
            eprintln!(
                "\tWarning: tile size {}x{} doesn't match established tile size {}x{}, skipping",
                tile_w, tile_h, self.tile_w, self.tile_h
            );
            return false;
        }

        if col >= self.grid_w || row >= self.grid_h {
            eprintln!(
                "\tWarning: grid position ({},{}) is out of bounds for a {}x{} grid, dropping tile",
                col, row, self.grid_w, self.grid_h
            );
            return false;
        }

        let base_x = col * self.tile_w;
        let base_y = row * self.tile_h;

        for r in 0..self.tile_h {
            let src_y = self.tile_h - 1 - r;
            let src_start = (src_y * self.tile_w) as usize;
            let src_row = &tile[src_start..src_start + self.tile_w as usize];

            let dst_y = base_y + r;
            let dst_start = (dst_y * self.dim_x + base_x) as usize;
            self.pixels[dst_start..dst_start + self.tile_w as usize]
                .copy_from_slice(src_row);
        }

        true
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if !self.initialized {
            eprintln!("\tWarning: no tiles were added to the combined image, skipping save");
            return Ok(());
        }

        let mut flipped = vec![0u16; self.pixels.len()];

        for y in 0..self.dim_y {
            let src_y = self.dim_y - 1 - y;

            let src_start = (src_y * self.dim_x) as usize;
            let dst_start = (y * self.dim_x) as usize;

            flipped[dst_start..dst_start + self.dim_x as usize]
                .copy_from_slice(&self.pixels[src_start..src_start + self.dim_x as usize]);
        }

        let img =
            ImageBuffer::<Luma<u16>, _>::from_vec(self.dim_x, self.dim_y, flipped)
                .expect("combined image buffer size always matches dim_x*dim_y");

        img.save(path)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
    }
}
