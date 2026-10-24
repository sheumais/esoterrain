use image::DynamicImage;
use std::io;
use std::path::Path;

pub struct ColourMapGrid {
    img: Option<DynamicImage>,
    tile_w: u32,
    tile_h: u32,
    grid_w: u32,
    grid_h: u32,
}

impl ColourMapGrid {
    pub fn new(grid_w: u32, grid_h: u32) -> Self {
        Self {
            img: None,
            tile_w: 0,
            tile_h: 0,
            grid_w: grid_w.max(1),
            grid_h: grid_h.max(1),
        }
    }

    pub fn set_tile(&mut self, col: u32, row: u32, tile: &DynamicImage) {
        let (tw, th) = (tile.width(), tile.height());
        if self.img.is_none() {
            self.tile_w = tw;
            self.tile_h = th;
            self.img = Some(DynamicImage::new_rgba8(tw * self.grid_w, th * self.grid_h));
        }
        if tw != self.tile_w || th != self.tile_h {
            eprintln!(
                "\tWarning: colour map tile {}x{} at ({},{}) doesn't match established tile size {}x{}, skipping",
                tw, th, col, row, self.tile_w, self.tile_h
            );
            return;
        }
        if col >= self.grid_w || row >= self.grid_h {
            eprintln!(
                "\tWarning: colour map tile position ({},{}) is out of bounds for a {}x{} grid, dropping",
                col, row, self.grid_w, self.grid_h
            );
            return;
        }
        if let Some(img) = self.img.as_mut() {
            let base_x = col * self.tile_w;
            let base_y = row * self.tile_h;
            image::imageops::overlay(img, tile, base_x as i64, base_y as i64);
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        match &self.img {
            Some(img) => img
                .save(path)
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string())),
            None => {
                eprintln!("\tWarning: no colour map tiles were collected, skipping save");
                Ok(())
            }
        }
    }

    pub fn save_cropped(&self, path: &Path, target_w: u32, target_h: u32) -> io::Result<()> {
        match &self.img {
            Some(img) => {
                let w = target_w.min(img.width());
                let h = target_h.min(img.height());
                if w == img.width() && h == img.height() {
                    img.save(path)
                } else {
                    img.crop_imm(0, 0, w, h).save(path)
                }
                .map_err(|e| io::Error::new(io::ErrorKind::Other, e.to_string()))
            }
            None => {
                eprintln!("\tWarning: no colour map tiles were collected, skipping save");
                Ok(())
            }
        }
    }
}