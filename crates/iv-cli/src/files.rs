use std::path::{Path, PathBuf};

use iv_core::FormatId;

/// Expands the inputs: files are kept as given, directories are walked recursively for files
/// whose extension names a known image format. The result is sorted and deduplicated.
pub fn collect(inputs: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for input in inputs {
        let p = Path::new(input);
        if p.is_dir() {
            walk(p, &mut out);
        } else {
            out.push(p.to_path_buf());
        }
    }
    out.sort();
    out.dedup();
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        match entry.file_type() {
            Ok(t) if t.is_dir() => walk(&path, out),
            Ok(t) if t.is_file() => {
                let known = path.extension().and_then(|e| e.to_str()).and_then(FormatId::from_extension).is_some();
                if known {
                    out.push(path);
                }
            }
            _ => {}
        }
    }
}
