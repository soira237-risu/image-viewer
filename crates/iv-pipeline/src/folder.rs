//! The images of one folder, in natural order, and stepping through them.

use std::path::{Path, PathBuf};

use iv_core::FormatId;

use crate::natord::natural_cmp;

#[derive(Clone, Debug, Default)]
pub struct FolderList {
    dir: PathBuf,
    files: Vec<PathBuf>,
}

fn is_image(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).and_then(FormatId::from_extension).is_some()
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

impl FolderList {
    /// Lists the files in `dir` whose extension names a known image format (not recursive,
    /// hidden files included), naturally sorted by file name.
    pub fn scan(dir: &Path) -> std::io::Result<Self> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| e.path())
            .filter(|p| is_image(p))
            .collect();
        files.sort_by(|a, b| natural_cmp(&name_of(a), &name_of(b)));
        Ok(Self { dir: dir.to_path_buf(), files })
    }

    /// The folder holding `file`, scanned. `file` itself is included even when its extension is
    /// unknown (it was opened explicitly, so the user wants to see it).
    pub fn for_file(file: &Path) -> std::io::Result<Self> {
        let dir = file.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let mut list = Self::scan(dir)?;
        if list.position(file).is_none() {
            list.files.push(file.to_path_buf());
            list.files.sort_by(|a, b| natural_cmp(&name_of(a), &name_of(b)));
        }
        Ok(list)
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn get(&self, index: usize) -> Option<&Path> {
        self.files.get(index).map(PathBuf::as_path)
    }

    pub fn files(&self) -> &[PathBuf] {
        &self.files
    }

    /// Index of `path` in the list, matching by file name (case-insensitively on Windows).
    pub fn position(&self, path: &Path) -> Option<usize> {
        let name = path.file_name()?;
        self.files.iter().position(|f| match f.file_name() {
            Some(n) if cfg!(windows) => n.eq_ignore_ascii_case(name),
            Some(n) => n == name,
            None => false,
        })
    }
}

/// Moves `delta` steps from `index` in a list of `len`. With `wrap`, stepping past either end
/// continues at the other; without, it stops at the end (returns `None` if already there).
pub fn step(index: usize, len: usize, delta: isize, wrap: bool) -> Option<usize> {
    if len == 0 {
        return None;
    }
    let target = index as isize + delta;
    if wrap {
        Some(target.rem_euclid(len as isize) as usize)
    } else {
        let clamped = target.clamp(0, len as isize - 1) as usize;
        (clamped != index).then_some(clamped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scan_filters_and_sorts_naturally() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["img10.jpg", "img2.PNG", "notes.txt", "img1.webp", "Img3.gif"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        std::fs::create_dir(dir.path().join("sub.jpg")).unwrap(); // a directory named like an image
        let list = FolderList::scan(dir.path()).unwrap();
        let names: Vec<_> = list.files().iter().map(|p| name_of(p)).collect();
        assert_eq!(names, ["img1.webp", "img2.PNG", "Img3.gif", "img10.jpg"]);
        assert_eq!(list.position(&dir.path().join("IMG10.JPG")), if cfg!(windows) { Some(3) } else { None });
    }

    #[test]
    fn for_file_includes_an_unrecognised_opened_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jpg"), b"x").unwrap();
        let odd = dir.path().join("b.dat");
        std::fs::write(&odd, b"x").unwrap();
        let list = FolderList::for_file(&odd).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list.position(&odd), Some(1));
    }

    #[test]
    fn stepping_with_and_without_wrap() {
        assert_eq!(step(0, 5, 1, false), Some(1));
        assert_eq!(step(4, 5, 1, false), None);
        assert_eq!(step(4, 5, 1, true), Some(0));
        assert_eq!(step(0, 5, -1, true), Some(4));
        assert_eq!(step(2, 5, -10, false), Some(0));
        assert_eq!(step(0, 5, -10, false), None);
        assert_eq!(step(0, 0, 1, true), None);
    }
}
