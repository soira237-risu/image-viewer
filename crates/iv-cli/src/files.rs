use std::path::{Path, PathBuf};

use iv_core::FormatId;

/// Expands the inputs: files are kept as given, directories are walked recursively for files
/// whose extension names a known image format, and archives named explicitly expand to their
/// images. The result is naturally sorted and deduplicated.
pub fn collect(inputs: &[String]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for input in inputs {
        let p = Path::new(input);
        if p.is_dir() {
            walk(p, &mut out);
        } else if iv_pipeline::archive::is_archive(p) {
            // an archive named explicitly stands for the images inside it
            match iv_pipeline::archive::list_images(p) {
                Ok(pages) => out.extend(pages),
                Err(e) => eprintln!("{}: {e}", p.display()),
            }
        } else {
            out.push(p.to_path_buf());
        }
    }
    out.sort_by(|a, b| iv_pipeline::natord::natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
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
