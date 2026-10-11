//! Extract one model (by MNF FileIndex) together with every texture it references.
//!
//! How the link works (found by comparing several .gr2 files):
//!   * The model's .gr2 starts with a table of 0x60-byte records. Each record begins with the
//!     fixed byte pattern `REF_SIG` and holds the referenced texture's MNF FileIndex as a
//!     little-endian u32 at +0x38.
//!   * Models and textures are plain files, i.e. manifest rows with Unk1 == 0.
//!
//! Extraction uses EsoExtractData's `-a <archive> -f <fileindex>` so only the wanted file is
//! written, not the whole archive.

use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::manifest::parse_int;

const REF_SIG: [u8; 28] = [
    0x08, 0x00, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x18, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00,
    0x08, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00, 0x08, 0x00, 0x00, 0x00,
];
const REF_FILE_INDEX_OFFSET: usize = 0x38;
const REF_SCAN_LIMIT: usize = 1 << 20;

/// Map order inside the .gr2 material (Maps #0 / #1 / #2). The Noesis script looks for these names.
const SLOT_ROLES: &[&str] = &["diffuse", "normal", "specular"];

const MODEL_EXTS: &[&str] = &["gr2"];
const TEXTURE_EXTS: &[&str] = &["dds", "xv4"];

pub struct Options<'a> {
    pub manifest: &'a str,
    pub exe: &'a str,
    pub mnf: &'a str,
    pub temp_dir: &'a str,
    pub out_root: &'a Path,
    pub dry_run: bool,
    pub skip_export: bool,
    pub keep_temp: bool,
    /// Path to the Noesis exe, used to convert the model to .obj via `?cmode`.
    pub noesis: &'a str,
}

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

pub fn lookup_file_indexes(
    manifest_path: &str,
    wanted: &HashSet<u32>,
) -> io::Result<HashMap<u32, Vec<MnfRow>>> {
    let content = fs::read_to_string(manifest_path)?;
    let mut out: HashMap<u32, Vec<MnfRow>> = HashMap::new();

    for line in content.lines() {
        let row: Vec<&str> = line.split(',').map(|c| c.trim()).collect();
        if row.len() < 11 {
            continue;
        }
        let (index, id1, file_index, unk1, size, arch) = match (
            parse_int(row[0]),
            parse_int(row[1]),
            parse_int(row[2]),
            parse_int(row[3]),
            parse_int(row[4]),
            parse_int(row[9]),
        ) {
            (Some(i), Some(id), Some(fi), Some(u), Some(sz), Some(a)) => {
                (i as u32, id as u32, fi as u32, u as u32, sz as u32, a as u32)
            }
            _ => continue,
        };
        if unk1 != 0 || !wanted.contains(&file_index) {
            continue;
        }
        out.entry(file_index).or_default().push(MnfRow {
            index,
            id1,
            file_index,
            size,
            arch,
        });
    }
    Ok(out)
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

struct Ctx<'a> {
    o: &'a Options<'a>,
    prefer_file_index: Cell<bool>,
}

fn run_exporter(c: &Ctx, arch: u32, filter: u32) -> bool {
    let o = c.o;
    println!("Running: {} {} {} -a {} -f {}", o.exe, o.mnf, o.temp_dir, arch, filter);
    if o.dry_run {
        println!("  (dry run, not executing)");
        return true;
    }
    if let Err(e) = fs::create_dir_all(o.temp_dir) {
        eprintln!("  Failed to create '{}': {}", o.temp_dir, e);
        return false;
    }
    match Command::new(o.exe)
        .arg(o.mnf)
        .arg(o.temp_dir)
        .arg("-a")
        .arg(arch.to_string())
        .arg("-f")
        .arg(filter.to_string())
        .output()
    {
        Ok(out) => {
            if !out.status.success() {
                eprintln!("  Warning: exporter exited with status {:?}", out.status.code());
                if !out.stderr.is_empty() {
                    eprintln!("{}", String::from_utf8_lossy(&out.stderr));
                }
            }
            true
        }
        Err(e) => {
            eprintln!("  Failed to launch exporter '{}': {}", o.exe, e);
            false
        }
    }
}

fn find_by_stem(dir: &Path, stem: u32, exts: &[&str]) -> Option<PathBuf> {
    let want = stem.to_string();
    let mut stack = vec![dir.to_path_buf()];
    let mut best: Option<(usize, PathBuf)> = None;

    while let Some(d) = stack.pop() {
        let Ok(rd) = fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
                continue;
            }
            if p.file_stem().and_then(|s| s.to_str()) != Some(want.as_str()) {
                continue;
            }
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if let Some(rank) = exts.iter().position(|x| *x == ext) {
                if best.as_ref().map_or(true, |(r, _)| rank < *r) {
                    best = Some((rank, p));
                }
            }
        }
    }
    best.map(|(_, p)| p)
}

fn fetch(c: &Ctx, row: &MnfRow, exts: &[&str]) -> Option<PathBuf> {
    let temp = Path::new(c.o.temp_dir);
    if let Some(p) = find_by_stem(temp, row.index, exts) {
        return Some(p);
    }
    if c.o.skip_export {
        return None;
    }

    let mut tries = vec![row.index, row.file_index];
    if row.index == row.file_index {
        tries.truncate(1);
    } else if c.prefer_file_index.get() {
        tries.reverse();
    }
    for filter in tries {
        if !run_exporter(c, row.arch, filter) {
            return None;
        }
        if c.o.dry_run {
            return None;
        }
        if let Some(p) = find_by_stem(temp, row.index, exts) {
            if filter == row.file_index && filter != row.index {
                c.prefer_file_index.set(true);
            }
            return Some(p);
        }
    }
    None
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

fn copy_into(src: &Path, name: &str, out_dir: &Path) -> io::Result<String> {
    fs::copy(src, out_dir.join(name))?;
    Ok(name.to_string())
}

fn texture_out_name(slot: usize, src: &Path) -> String {
    let ext = src.extension().and_then(|e| e.to_str()).unwrap_or("dds").to_ascii_lowercase();
    match SLOT_ROLES.get(slot) {
        Some(role) => format!("{}.{}", role, ext),
        None => format!("extra{}.{}", slot, ext),
    }
}

/// ESO's DDS headers set the LINEARSIZE flag but store the row pitch there. Write the real size.
fn fix_dds_linear_size(path: &Path) -> io::Result<()> {
    let mut d = fs::read(path)?;
    if d.len() < 128 || &d[0..4] != b"DDS " {
        return Ok(());
    }
    let block = match &d[84..88] {
        b"DXT1" => 8u32,
        b"DXT3" | b"DXT5" => 16u32,
        _ => return Ok(()),
    };
    let h = u32::from_le_bytes(d[12..16].try_into().unwrap());
    let w = u32::from_le_bytes(d[16..20].try_into().unwrap());
    let size = ((w + 3) / 4) * ((h + 3) / 4) * block;
    d[20..24].copy_from_slice(&size.to_le_bytes());
    fs::write(path, d)
}

/// Absolute path without the Windows `\\?\` prefix (Noesis dislikes it).
fn plain_abs(p: &Path) -> io::Result<PathBuf> {
    let c = fs::canonicalize(p)?;
    let s = c.to_string_lossy().into_owned();
    Ok(match s.strip_prefix(r"\\?\") {
        Some(rest) => PathBuf::from(rest),
        None => c,
    })
}

/// Quote a string as a PowerShell single-quoted literal.
fn ps_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Copy a Noesis-written OBJ to `dest`, pointing it at `<name>.mtl`, and write that .mtl so it uses
/// the PNG textures (diffuse -> map_Kd, normal -> map_Bump, specular -> map_Ks).
/// Returns the number of materials.
fn finish_obj(src: &Path, dest: &Path, slot_png: &[Option<String>]) -> io::Result<usize> {
    let text = fs::read_to_string(src)?;
    let stem = dest.file_stem().and_then(|s| s.to_str()).unwrap_or("model").to_string();
    let mtl_file = format!("{}.mtl", stem);

    let mut mats: Vec<String> = Vec::new();
    let mut body = String::new();
    for line in text.lines() {
        let l = line.trim();
        if l.starts_with("mtllib ") {
            continue; // replaced below
        }
        if let Some(r) = l.strip_prefix("usemtl ") {
            let m = r.trim().to_string();
            if !mats.contains(&m) {
                mats.push(m);
            }
        }
        body.push_str(line);
        body.push('\n');
    }
    if mats.is_empty() {
        mats.push("eso_material".to_string());
        body = format!("usemtl eso_material\n{}", body);
    }
    fs::write(dest, format!("mtllib {}\n{}", mtl_file, body))?;

    let tex = |i: usize| slot_png.get(i).and_then(|p| p.as_deref());
    let mut mtl = String::new();
    for m in &mats {
        mtl.push_str(&format!("newmtl {}\nKa 1.000 1.000 1.000\nKd 1.000 1.000 1.000\nKs 0.200 0.200 0.200\nNs 20.0\nd 1.0\nillum 2\n", m));
        if let Some(p) = tex(0) { mtl.push_str(&format!("map_Kd {}\n", p)); }
        if let Some(p) = tex(1) { mtl.push_str(&format!("map_Bump {}\nbump {}\n", p, p)); }
        if let Some(p) = tex(2) { mtl.push_str(&format!("map_Ks {}\n", p)); }
        mtl.push('\n');
    }
    fs::write(dest.with_file_name(&mtl_file), mtl)?;
    Ok(mats.len())
}

/// Convert `model` to `dest` with Noesis console mode.
///
/// Noesis' embedded Python fails to start (error -206) in some launch contexts, which leaves it
/// unable to recognise .gr2 files. So: run it directly with its own folder as the working
/// directory, and if no output file appears, retry through PowerShell's `Start-Process -Wait`, which
/// is known to work.
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
    let ctx = Ctx { o, prefer_file_index: Cell::new(false) };

    let mut want = HashSet::new();
    want.insert(model_fi);
    let found = lookup_file_indexes(o.manifest, &want)
        .map_err(|e| format!("Failed to read manifest '{}': {}", o.manifest, e))?;
    let model = pick_row("Model", model_fi, found.get(&model_fi))?;
    println!(
        "Model: FileIndex 0x{:08X} -> Index {} (arch {}, {} bytes)",
        model_fi, model.index, model.arch, model.size
    );

    let model_path = fetch(&ctx, &model, MODEL_EXTS);
    if o.dry_run {
        println!("\nDry run: stopping here (texture list needs the model file).");
        return Ok(Summary { out_dir: PathBuf::new(), textures_found: 0, textures_missing: 0 });
    }
    let model_path = model_path.ok_or_else(|| {
        format!(
            "Model file '{}.gr2' not found under '{}'. Did the export run (see --skip-export)?",
            model.index, o.temp_dir
        )
    })?;
    let blob = fs::read(&model_path).map_err(|e| format!("Failed to read '{}': {}", model_path.display(), e))?;
    let refs = texture_refs(&blob);
    if refs.is_empty() {
        return Err(format!(
            "No texture reference records found in '{}'. Is this a model .gr2 with the usual header table?",
            model_path.display()
        ));
    }
    println!("Model references {} texture(s).", refs.len());

    let wanted: HashSet<u32> = refs.iter().copied().collect();
    let rows = lookup_file_indexes(o.manifest, &wanted)
        .map_err(|e| format!("Failed to read manifest '{}': {}", o.manifest, e))?;

    // Everything goes into one flat folder, keeping the archive's own file names:
    //   <index>.gr2, <textureIndex>.dds / .png, <index>.obj, <index>.mtl
    // Textures shared between models therefore just share a file instead of colliding.
    let out_dir = o.out_root.to_path_buf();
    fs::create_dir_all(&out_dir)
        .map_err(|e| format!("Failed to create '{}': {}", out_dir.display(), e))?;
    let model_name = copy_into(&model_path, &format!("{}.gr2", model.index), &out_dir)
        .map_err(|e| format!("Failed to copy model: {}", e))?;
    println!("Model -> {}", out_dir.join(&model_name).display());

    // Noesis' GR2 plugin wants the textures next to the model under fixed role names
    // (diffuse/normal/specular), so it gets a private scratch folder.
    let scratch = out_dir.join(format!("_noesis_tmp_{}", model.index));
    fs::create_dir_all(&scratch)
        .map_err(|e| format!("Failed to create '{}': {}", scratch.display(), e))?;
    fs::copy(out_dir.join(&model_name), scratch.join(&model_name))
        .map_err(|e| format!("Failed to copy model to scratch: {}", e))?;

    // PNG name for slots 0/1/2 (diffuse/normal/specular), used by the .mtl.
    let mut slot_png: Vec<Option<String>> = vec![None; SLOT_ROLES.len()];

    let (mut ok, mut missing) = (0usize, 0usize);
    for (slot, fi) in refs.iter().enumerate() {
        let row = match pick_row("Texture", *fi, rows.get(fi)) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("  slot {}: {}", slot, e);
                missing += 1;
                continue;
            }
        };
        match fetch(&ctx, &row, TEXTURE_EXTS) {
            Some(src) => {
                // keep the original file name (e.g. 608841.dds)
                let orig = src.file_name().and_then(|n| n.to_str()).unwrap_or("texture.dds").to_ascii_lowercase();
                match copy_into(&src, &orig, &out_dir) {
                    Ok(name) => {
                        if name.ends_with(".dds") {
                            if let Err(e) = fix_dds_linear_size(&out_dir.join(&name)) {
                                eprintln!("  Warning: could not fix DDS header of {}: {}", name, e);
                            }
                            let png_name = name.replace(".dds", ".png");
                            match image::open(out_dir.join(&name)) {
                                Ok(img) => match img.save(out_dir.join(&png_name)) {
                                    Ok(()) => {
                                        if slot < slot_png.len() {
                                            slot_png[slot] = Some(png_name.clone());
                                        }
                                    }
                                    Err(e) => eprintln!("  Warning: PNG save failed for {}: {}", name, e),
                                },
                                Err(e) => eprintln!("  Warning: could not decode {} to PNG: {}", name, e),
                            }
                        }
                        let _ = fs::copy(out_dir.join(&name), scratch.join(texture_out_name(slot, &src)));
                        println!("  slot {} ({}): 0x{:08X} -> {}", slot, SLOT_ROLES.get(slot).unwrap_or(&"extra"), fi, name);
                        ok += 1;
                    }
                    Err(e) => {
                        eprintln!("  slot {}: copy failed: {}", slot, e);
                        missing += 1;
                    }
                }
            }
            None => {
                eprintln!("  slot {}: 0x{:08X} (Index {}) was not produced by the exporter", slot, fi, row.index);
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
            let final_obj = out_dir.join(format!("{}.obj", model.index));
            match finish_obj(&scratch_obj, &final_obj, &slot_png) {
                Ok(n) => println!("OBJ -> {} ({} material(s), textures linked)", final_obj.display(), n),
                Err(e) => eprintln!("Warning: could not finish obj: {}", e),
            }
        }
        Err(e) => eprintln!("Warning: obj export failed: {}", e),
    }
    let _ = fs::remove_dir_all(&scratch);

    if !o.keep_temp && !o.skip_export {
        if let Err(e) = fs::remove_dir_all(o.temp_dir) {
            eprintln!("Warning: failed to remove temporary directory '{}': {}", o.temp_dir, e);
        }
    }

    Ok(Summary { out_dir, textures_found: ok, textures_missing: missing })
}