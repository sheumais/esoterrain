use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

const MAGIC: [u8; 4] = [0xFA, 0xFA, 0xEB, 0xEB];
const ROW_MARK: &[u8; 4] = b"####";
const ROW_HDR: usize = 32;

const PIECE_MODEL_OFF: usize = 73;
const KEEP_WORLD_OFF: usize = 59;
const KEEP_POS_OFF: usize = 63;
const KEEP_HEAD_OFF: usize = 75;
const KEEP_GRID_OFF: usize = 79;
const KEEP_ENTRIES_OFF: usize = 91;

pub const KEEPS_FILE_INDEX: u32 = 0x5a;
pub const PIECES_FILE_INDEX: u32 = 0x5b;

pub struct KeepArgs<'a> {
    pub keeps_file: &'a Path,
    pub pieces_file: &'a Path,
    pub cell_m: f64,
    pub units_per_metre: f64,
    pub only_key: Option<u32>,
    pub out_file: &'a Path,
}

#[derive(Debug)]
struct Placed {
    piece: i32,
    gx: i32,
    gz: i32,
    rot_deg: f32,
}

#[derive(Debug)]
struct Keep {
    key: u32,
    world: u32,
    pos_cm: [i32; 3],
    heading_deg: f32,
    grid: i32,
    placed: Vec<Placed>,
}

fn be_u32(b: &[u8], o: usize) -> Result<u32, String> {
    b.get(o..o + 4)
        .map(|s| u32::from_be_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| format!("read past end at {}", o))
}

fn be_i32(b: &[u8], o: usize) -> Result<i32, String> {
    be_u32(b, o).map(|v| v as i32)
}

fn le_f32(b: &[u8], o: usize) -> Result<f32, String> {
    b.get(o..o + 4)
        .map(|s| f32::from_le_bytes([s[0], s[1], s[2], s[3]]))
        .ok_or_else(|| format!("read past end at {}", o))
}

fn split_rows<'a>(data: &'a [u8], what: &str) -> Result<Vec<&'a [u8]>, String> {
    if data.len() < 16 || data[0..4] != MAGIC {
        return Err(format!("{}: not an EsoFileData table (bad magic)", what));
    }
    let declared = be_u32(data, 8)? as usize;
    let mut rows = Vec::with_capacity(declared);
    let mut pos = 16usize;
    while pos + ROW_HDR <= data.len() {
        if &data[pos..pos + 4] != ROW_MARK {
            return Err(format!("{}: expected row marker at 0x{:x}", what, pos));
        }
        let size = be_u32(data, pos + 8)? as usize;
        let end = pos + ROW_HDR + size;
        if end > data.len() {
            return Err(format!("{}: row at 0x{:x} runs past end of file", what, pos));
        }
        rows.push(&data[pos..end]);
        pos = end;
    }
    if rows.len() != declared {
        eprintln!("  warning: {} declares {} rows but {} were found", what, declared, rows.len());
    }
    Ok(rows)
}

fn parse_pieces(path: &Path) -> Result<HashMap<i32, u32>, String> {
    let data = fs::read(path).map_err(|e| format!("Failed to read '{}': {}", path.display(), e))?;
    let mut map = HashMap::new();
    for row in split_rows(&data, "pieces (1146)")? {
        let key = be_i32(row, 32)?;
        let model = be_u32(row, PIECE_MODEL_OFF)?;
        if model != 0 {
            map.insert(key, model);
        }
    }
    Ok(map)
}

fn parse_keeps(path: &Path) -> Result<Vec<Keep>, String> {
    let data = fs::read(path).map_err(|e| format!("Failed to read '{}': {}", path.display(), e))?;
    let mut keeps = Vec::new();
    for row in split_rows(&data, "keeps (1144)")? {
        let key = be_u32(row, 32)?;
        let world = be_u32(row, KEEP_WORLD_OFF)?;
        let pos_cm = [
            be_i32(row, KEEP_POS_OFF)?,
            be_i32(row, KEEP_POS_OFF + 4)?,
            be_i32(row, KEEP_POS_OFF + 8)?,
        ];
        let heading_deg = le_f32(row, KEEP_HEAD_OFF)?;
        let grid = be_i32(row, KEEP_GRID_OFF)?;
        let n = be_i32(row, KEEP_GRID_OFF + 8)?;
        if !(0..=4096).contains(&n) {
            return Err(format!("keep {}: implausible placement count {}", key, n));
        }

        let mut placed = Vec::with_capacity(n as usize);
        let mut o = KEEP_ENTRIES_OFF;
        for _ in 0..n {
            let piece = be_i32(row, o)?;
            let gx = be_i32(row, o + 4)?;
            let gz = be_i32(row, o + 8)?;
            let rot_deg = le_f32(row, o + 12)?;
            let k = be_i32(row, o + 16)?;
            if !(0..=64).contains(&k) {
                return Err(format!("keep {}: implausible extra count {} at 0x{:x}", key, k, o + 16));
            }
            o += 20 + 4 * k as usize;
            placed.push(Placed { piece, gx, gz, rot_deg });
        }
        keeps.push(Keep { key, world, pos_cm, heading_deg, grid, placed });
    }
    Ok(keeps)
}

pub fn write_lut(keeps_file: &Path, out_path: &Path) -> Result<usize, String> {
    let mut keeps = parse_keeps(keeps_file)?;
    keeps.sort_by_key(|k| k.key);

    let mut out = String::from("key,world,grid,pos_x_cm,pos_y_cm,pos_z_cm,heading_deg,pieces\n");
    for k in &keeps {
        out.push_str(&format!(
            "{},{},{},{},{},{},{},{}\n",
            k.key, k.world, k.grid, k.pos_cm[0], k.pos_cm[1], k.pos_cm[2], k.heading_deg,
            k.placed.len()
        ));
    }
    if let Some(dir) = out_path.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir).map_err(|e| format!("Failed to create '{}': {}", dir.display(), e))?;
        }
    }
    fs::write(out_path, out).map_err(|e| format!("Failed to write '{}': {}", out_path.display(), e))?;
    Ok(keeps.len())
}

pub fn run(a: &KeepArgs) -> Result<PathBuf, String> {
    let pieces = parse_pieces(a.pieces_file)?;
    let keeps = parse_keeps(a.keeps_file)?;
    println!("Read {} keep(s), {} piece model(s).", keeps.len(), pieces.len());

    let mut out = String::from("{\"instances\":[\n");
    let (mut n_keeps, mut n_inst, mut n_unknown) = (0usize, 0usize, 0usize);
    let mut first = true;

    for k in &keeps {
        if a.only_key.map_or(false, |want| want != k.key) {
            continue;
        }
        if k.pos_cm == [0, 0, 0] {
            continue;
        }
        n_keeps += 1;

        let (gxmin, gxmax) = k.placed.iter().fold((i32::MAX, i32::MIN), |a, p| (a.0.min(p.gx), a.1.max(p.gx)));
        let (gzmin, gzmax) = k.placed.iter().fold((i32::MAX, i32::MIN), |a, p| (a.0.min(p.gz), a.1.max(p.gz)));
        println!("keep {}: world {}, grid {}, pos_cm {:?}, heading {}, {} pieces, gx {}..{}, gz {}..{}",
            k.key, k.world, k.grid, k.pos_cm, k.heading_deg, k.placed.len(), gxmin, gxmax, gzmin, gzmax);

        let kx = k.pos_cm[0] as f64 / 100.0;
        let ky = k.pos_cm[1] as f64 / 100.0;
        let kz = k.pos_cm[2] as f64 / 100.0;
        let heading = k.heading_deg as f64;
        let theta = heading.to_radians();
        let (s, c) = theta.sin_cos();
        // GUESS: grid origin is the middle of the S x S grid
        let half = k.grid as f64 / 2.0;

        for p in &k.placed {
            let Some(&model) = pieces.get(&p.piece) else {
                eprintln!("  keep {}: piece {} not in catalogue, skipped", k.key, p.piece);
                n_unknown += 1;
                continue;
            };
            let lx = (p.gx as f64 - half) * a.cell_m;
            let lz = (p.gz as f64 - half) * a.cell_m;
            let wx = kx + lx * c + lz * s;
            let wz = kz - lx * s + lz * c;
            let rot_y = (heading + p.rot_deg as f64).to_radians();

            if !first {
                out.push_str(",\n");
            }
            first = false;
            out.push_str(&format!(
                "{{\"model_type_id\":{},\"pos_x\":{:.4},\"pos_y\":{:.4},\"pos_z\":{:.4},\"rot_x\":0.0,\"rot_y\":{:.6},\"rot_z\":0.0}}",
                model,
                wx * a.units_per_metre,
                ky * a.units_per_metre,
                wz * a.units_per_metre,
                rot_y
            ));
            n_inst += 1;
        }
    }
    out.push_str("\n]}\n");

    if n_inst == 0 {
        return Err("No keep instances produced.".into());
    }
    if let Some(dir) = a.out_file.parent() {
        if !dir.as_os_str().is_empty() {
            fs::create_dir_all(dir).map_err(|e| format!("Failed to create '{}': {}", dir.display(), e))?;
        }
    }
    fs::write(a.out_file, out).map_err(|e| format!("Failed to write '{}': {}", a.out_file.display(), e))?;

    println!(
        "{} keep(s) -> {} instance(s) ({} unknown piece(s) skipped)\nWrote {}",
        n_keeps, n_inst, n_unknown, a.out_file.display()
    );
    Ok(a.out_file.to_path_buf())
}