use crate::manifest::{filter_size_manifest, SizeEntry};
use image::{ImageBuffer, Luma};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Manifest type of the per-cell placement files (see cellplacement parser).
const CELL_PLACEMENT_TYPE: u32 = 0x15;

/// Brightness given to the smallest existing file, so present-but-tiny cells
/// stay distinguishable from missing cells (pure black).
const MIN_BRIGHTNESS: f32 = 24.0;

/// Builds a rough zone-shape image: one pixel (scaled up by `scale`) per cell,
/// brightness = uncompressed size of the per-cell placement file (type 0x15 by default). Missing cells are black.
pub fn run(
    manifest: &str,
    world: u32,
    forced_type: Option<u32>,
    scale: u32,
    out_dir: &Path,
) -> Result<PathBuf, String> {
    let entries = filter_size_manifest(manifest, world)
        .map_err(|e| format!("Failed to read manifest '{}': {}", manifest, e))?;
    if entries.is_empty() {
        return Err(format!("No per-cell manifest entries found for world 0x{:x}.", world));
    }

    let mut by_type: HashMap<u32, Vec<SizeEntry>> = HashMap::new();
    for e in &entries {
        by_type.entry(e.ty).or_default().push(*e);
    }
    let ty = forced_type.unwrap_or(CELL_PLACEMENT_TYPE);
    if !by_type.contains_key(&ty) {
        let mut avail: Vec<String> = by_type.keys().map(|t| format!("0x{:x}", t)).collect();
        avail.sort();
        return Err(format!(
            "Type 0x{:x} has no entries for world 0x{:x}. Types present: {}",
            ty, world, avail.join(", ")
        ));
    }
    println!("Using per-cell file type 0x{:x}.", ty);

    // Same (x,z) can appear more than once for a type; keep the largest.
    let mut cells: HashMap<(u32, u32), u32> = HashMap::new();
    for e in &by_type[&ty] {
        let s = cells.entry((e.x, e.z)).or_insert(0);
        *s = (*s).max(e.size);
    }

    let min_x = cells.keys().map(|k| k.0).min().unwrap();
    let max_x = cells.keys().map(|k| k.0).max().unwrap();
    let min_z = cells.keys().map(|k| k.1).min().unwrap();
    let max_z = cells.keys().map(|k| k.1).max().unwrap();
    let (gw, gh) = (max_x - min_x + 1, max_z - min_z + 1);
    let lo = *cells.values().min().unwrap() as f32;
    let hi = *cells.values().max().unwrap() as f32;
    println!(
        "Grid {}x{} (x {}..{}, z {}..{}), {} cell(s), size range {}..{} bytes",
        gw, gh, min_x, max_x, min_z, max_z, cells.len(), lo as u32, hi as u32
    );

    let scale = scale.max(1);
    let mut img = ImageBuffer::<Luma<u8>, Vec<u8>>::new(gw * scale, gh * scale);
    for (&(x, z), &size) in &cells {
        let t = if hi > lo { (size as f32 - lo) / (hi - lo) } else { 1.0 };
        let v = (MIN_BRIGHTNESS + t * (255.0 - MIN_BRIGHTNESS)).round() as u8;
        let col = x - min_x;
        let row = max_z - z; // north (high z) at the top, same as the terrain export
        for dy in 0..scale {
            for dx in 0..scale {
                img.put_pixel(col * scale + dx, row * scale + dy, Luma([v]));
            }
        }
    }

    std::fs::create_dir_all(out_dir)
        .map_err(|e| format!("Failed to create '{}': {}", out_dir.display(), e))?;
    let path = out_dir.join(format!("world_0x{:x}_sizemap_type0x{:x}.png", world, ty));
    img.save(&path).map_err(|e| format!("Failed to save '{}': {}", path.display(), e))?;
    Ok(path)
}