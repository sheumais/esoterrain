use crate::reader::BinReader;
use image::{ImageBuffer, Luma, Rgb};
use std::io;
use std::path::{Path, PathBuf};

const MAGIC_HEADER: u32 = 0xBEEF000C;
const UNKNOWN3_SIZE: usize = 20;
const MAX_KNOWN_CHANNEL: u8 = 8;

#[derive(Debug, Default, Clone)]
struct RecInfo {
    channel: u8,
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

        self.warn_on_duplicate_or_unknown_channels();

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
                channel: reader.read_u8()?,
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
                    "failed to jump to start of data section #{} (channel byte {}, offset 0x{:08X})",
                    index, rec_info.channel, rec_info.offset
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
                "\tWarning: Height mismatch in data section #{} (channel byte {}) ({} != {})",
                index, rec_info.channel, data.height, data.height2
            );
        }

        let data_size = data.height as usize * data.width as usize;
        let rec_size = 16 + data.height as usize * (data.width as usize + 2) + 4;
        if rec_info.length as usize != rec_size {
            eprintln!(
                "\tWarning: Data section #{} (channel byte {}) has record length mismatch ({} != {})",
                index, rec_info.channel, rec_info.length, rec_size
            );
        }

        data.data = vec![0u32; data_size / 4];

        for y in 0..data.height as usize {
            let width = reader.read_u16()? as u32;
            if width != data.width {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "row width mismatch in data section #{}:{} (channel byte {}) ({} != {})",
                        index, y, rec_info.channel, width, data.width
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

    fn warn_on_duplicate_or_unknown_channels(&self) {
        let mut seen = [false; (MAX_KNOWN_CHANNEL as usize) + 1];
        for rec in &self.header.rec_infos {
            if rec.offset == 0 || rec.length == 0 {
                continue;
            }
            if rec.channel as usize > MAX_KNOWN_CHANNEL as usize {
                eprintln!(
                    "\tWarning: '{}' has a data record with channel byte {} (> {}), \
                     which the game's own loader never dispatches on - treating as dead data.",
                    self.filename.display(),
                    rec.channel,
                    MAX_KNOWN_CHANNEL
                );
                continue;
            }
            let idx = rec.channel as usize;
            if seen[idx] {
                eprintln!(
                    "\tWarning: '{}' has more than one data record claiming channel {}; \
                     only the first will be used.",
                    self.filename.display(),
                    idx
                );
            }
            seen[idx] = true;
        }
    }

    fn find_record(&self, channel: usize) -> Option<(usize, &RecInfo)> {
        self.header
            .rec_infos
            .iter()
            .enumerate()
            .find(|(_, r)| r.channel as usize == channel)
    }

    pub fn available_channels(&self) -> Vec<usize> {
        let mut chans: Vec<usize> = self
            .header
            .rec_infos
            .iter()
            .filter(|r| r.offset != 0 && r.length != 0 && r.channel <= MAX_KNOWN_CHANNEL)
            .map(|r| r.channel as usize)
            .collect();
        chans.sort_unstable();
        chans.dedup();
        chans
    }

    pub fn channel_tile_raw(&self, channel: usize) -> Option<RawTile> {
        if channel > MAX_KNOWN_CHANNEL as usize {
            return None;
        }
        let (pos, rec_info) = self.find_record(channel)?;
        if rec_info.offset == 0 || rec_info.length == 0 {
            return None;
        }
        let section = self.data.get(pos)?;
        if section.width == 0 || section.height == 0 {
            return None;
        }

        let words_per_row = (section.width / 4) as usize;
        let height = section.height as usize;
        if words_per_row == 0 || height == 0 || section.data.len() < words_per_row * height {
            return None;
        }

        match channel {
            6 => {
                if words_per_row % 3 != 0 {
                    eprintln!(
                        "\tWarning: normal channel row width {} isn't divisible by 3, skipping",
                        words_per_row
                    );
                    return None;
                }
                let px_w = words_per_row / 3;
                let mut xs = vec![0.0f32; px_w * height];
                let mut ys = vec![0.0f32; px_w * height];
                let mut zs = vec![0.0f32; px_w * height];
                for row in 0..height {
                    for px in 0..px_w {
                        let base = row * words_per_row + px * 3;
                        xs[row * px_w + px] = f32::from_bits(section.data[base]);
                        ys[row * px_w + px] = f32::from_bits(section.data[base + 1]);
                        zs[row * px_w + px] = f32::from_bits(section.data[base + 2]);
                    }
                }
                Some(RawTile::Vec3 {
                    x: xs,
                    y: ys,
                    z: zs,
                    width: px_w as u32,
                    height: height as u32,
                })
            }
            7 => {
                let px_w = words_per_row * 2;
                let mut values = vec![0.0f32; px_w * height];
                for row in 0..height {
                    for w in 0..words_per_row {
                        let word = section.data[row * words_per_row + w];
                        values[row * px_w + w * 2] = (word & 0xFFFF) as f32;
                        values[row * px_w + w * 2 + 1] = (word >> 16) as f32;
                    }
                }
                Some(RawTile::Scalar {
                    values,
                    width: px_w as u32,
                    height: height as u32,
                })
            }
            0 => {
                let values: Vec<f32> = section.data[..words_per_row * height]
                    .iter()
                    .map(|&d| f32::from_bits(d))
                    .collect();
                Some(RawTile::Scalar {
                    values,
                    width: words_per_row as u32,
                    height: height as u32,
                })
            }
            _ => {
                let values: Vec<f32> = section.data[..words_per_row * height]
                    .iter()
                    .map(|&d| d as f32)
                    .collect();
                Some(RawTile::Scalar {
                    values,
                    width: words_per_row as u32,
                    height: height as u32,
                })
            }
        }
    }
}

pub enum RawTile {
    Scalar {
        values: Vec<f32>,
        width: u32,
        height: u32,
    },
    Vec3 {
        x: Vec<f32>,
        y: Vec<f32>,
        z: Vec<f32>,
        width: u32,
        height: u32,
    },
}

pub fn fixed_channel_range(index: usize) -> Option<(f32, f32)> {
    match index {
        0 => Some((0.0, 1000.0)),
        6 => Some((-1.0, 1.0)),
        _ => None,
    }
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
    pub fn from_pixels(
        pixels: Vec<u16>,
        dim_x: u32,
        dim_y: u32,
        tile_w: u32,
        tile_h: u32,
        grid_w: u32,
        grid_h: u32,
    ) -> Self {
        Self {
            pixels,
            dim_x,
            dim_y,
            tile_w,
            tile_h,
            grid_w,
            grid_h,
            initialized: true,
        }
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

pub fn save_rgb_from_channels(r: &BigImage, g: &BigImage, b: &BigImage, path: &Path) -> io::Result<()> {
    if !(r.initialized && g.initialized && b.initialized) {
        eprintln!("\tWarning: not all normal-map components were populated, skipping RGB save");
        return Ok(());
    }
    if r.dim_x != g.dim_x || r.dim_x != b.dim_x || r.dim_y != g.dim_y || r.dim_y != b.dim_y {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "normal-map component dimensions don't match",
        ));
    }

    let mut buf = vec![0u8; (r.dim_x as usize) * (r.dim_y as usize) * 3];
    for y in 0..r.dim_y {
        let src_y = r.dim_y - 1 - y;
        for x in 0..r.dim_x {
            let src_idx = (src_y * r.dim_x + x) as usize;
            let dst_idx = ((y * r.dim_x + x) as usize) * 3;
            buf[dst_idx] = (r.pixels[src_idx] >> 8) as u8;
            buf[dst_idx + 1] = (g.pixels[src_idx] >> 8) as u8;
            buf[dst_idx + 2] = (b.pixels[src_idx] >> 8) as u8;
        }
    }

    let img = ImageBuffer::<Rgb<u8>, _>::from_vec(r.dim_x, r.dim_y, buf)
        .expect("rgb buffer size always matches dim_x*dim_y*3");
    img.save(path)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
}

pub struct RawBigImage {
    pixels: Vec<f32>,
    written: Vec<bool>,
    dim_x: u32,
    dim_y: u32,
    tile_w: u32,
    tile_h: u32,
    grid_w: u32,
    grid_h: u32,
    initialized: bool,
}

impl RawBigImage {
    pub fn new(grid_w: u32, grid_h: u32) -> Self {
        Self {
            pixels: Vec::new(),
            written: Vec::new(),
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
        let n = (self.dim_x as usize) * (self.dim_y as usize);
        self.pixels = vec![0.0f32; n];
        self.written = vec![false; n];
        self.initialized = true;
    }

    pub fn set_tile_at(&mut self, col: u32, row: u32, tile: &[f32], tile_w: u32, tile_h: u32) -> bool {
        if tile_w == 0 || tile_h == 0 || tile.len() != (tile_w * tile_h) as usize {
            eprintln!(
                "\tWarning: skipping raw tile with inconsistent dimensions ({} values for {}x{})",
                tile.len(),
                tile_w,
                tile_h
            );
            return false;
        }

        self.ensure_initialized(tile_w, tile_h);

        if tile_w != self.tile_w || tile_h != self.tile_h {
            eprintln!(
                "\tWarning: raw tile size {}x{} doesn't match established tile size {}x{}, skipping",
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
            self.pixels[dst_start..dst_start + self.tile_w as usize].copy_from_slice(src_row);
            for i in 0..self.tile_w as usize {
                self.written[dst_start + i] = true;
            }
        }

        true
    }

    pub fn coverage_fraction(&self) -> f64 {
        if self.written.is_empty() {
            return 0.0;
        }
        let present = self.written.iter().filter(|&&w| w).count();
        present as f64 / self.written.len() as f64
    }

    pub fn value_range(&self) -> Option<(f32, f32)> {
        let mut mn = f32::INFINITY;
        let mut mx = f32::NEG_INFINITY;
        for (i, &was_written) in self.written.iter().enumerate() {
            if was_written {
                let v = self.pixels[i];
                if v < mn {
                    mn = v;
                }
                if v > mx {
                    mx = v;
                }
            }
        }
        if mn.is_finite() && mx.is_finite() {
            Some((mn, mx))
        } else {
            None
        }
    }

    pub fn finalize_grayscale(&self, min: f32, max: f32) -> BigImage {
        let range = if max > min { max - min } else { 1.0 };
        let pixels: Vec<u16> = self
            .pixels
            .iter()
            .zip(self.written.iter())
            .map(|(&v, &was_written)| {
                if !was_written {
                    0
                } else {
                    (((v - min) / range) * 65535.0).round().clamp(0.0, 65535.0) as u16
                }
            })
            .collect();
        BigImage::from_pixels(
            pixels, self.dim_x, self.dim_y, self.tile_w, self.tile_h, self.grid_w, self.grid_h,
        )
    }
}