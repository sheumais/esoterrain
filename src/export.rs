use std::collections::HashSet;
use std::process::Command;

pub fn export_archive(exe_path: &str, mnf_path: &str, out_dir: &str, arch: u32, dry_run: bool) -> bool {
    println!("Running: {} {} {} -a {}", exe_path, mnf_path, out_dir, arch);
    if dry_run {
        println!("  (dry run, not executing)");
        return true;
    }

    if let Err(e) = std::fs::create_dir_all(out_dir) {
        eprintln!("  Failed to create output directory '{}': {}", out_dir, e);
        return false;
    }

    match Command::new(exe_path)
        .arg(mnf_path)
        .arg(out_dir)
        .arg("-a")
        .arg(arch.to_string())
        .output()
    {
        Ok(output) => {
            if !output.stdout.is_empty() {
                println!("{}", String::from_utf8_lossy(&output.stdout));
            }
            if !output.status.success() {
                eprintln!(
                    "  Warning: archive export exited with status {:?}",
                    output.status.code()
                );
                if !output.stderr.is_empty() {
                    eprintln!("{}", String::from_utf8_lossy(&output.stderr));
                }
                return false;
            }
            true
        }
        Err(e) => {
            eprintln!("  Failed to launch exporter '{}': {}", exe_path, e);
            false
        }
    }
}

pub fn cleanup_export_dir(out_dir: &str, wanted: &HashSet<u32>, dry_run: bool) -> (usize, usize) {
    let mut kept = 0usize;
    let mut removed = 0usize;

    for entry in walkdir::WalkDir::new(out_dir).into_iter().filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }

        let stem_idx = path
            .file_stem()
            .and_then(|s| s.to_str())
            .and_then(|s| s.parse::<u32>().ok());

        if stem_idx.map(|i| wanted.contains(&i)).unwrap_or(false) {
            kept += 1;
            continue;
        }

        if dry_run {
            println!("  would delete {}", path.display());
        } else if let Err(e) = std::fs::remove_file(path) {
            eprintln!("  Warning: failed to remove {}: {}", path.display(), e);
            continue;
        }
        removed += 1;
    }

    (kept, removed)
}