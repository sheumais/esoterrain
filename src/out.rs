//! Output locations, and the scratch directory Noesis needs.

use std::io;
use std::path::{Path, PathBuf};

pub const EXPORT_DIR: &str = "./export";
/// All `*_lut.csv` files; per-world model LUTs go in `LUT_DIR/worlds/`.
pub const LUT_DIR: &str = "./luts";

/// Scratch directory in the system temp dir, removed on drop (also on error). Only for Noesis,
/// which needs real files on disk.
pub struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    pub fn new(tag: &str) -> io::Result<Scratch> {
        let dir = std::env::temp_dir().join(format!("esoland_{}_{}", tag, std::process::id()));
        std::fs::create_dir_all(&dir)?;
        Ok(Scratch { dir })
    }

    pub fn path(&self) -> &Path {
        &self.dir
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}
