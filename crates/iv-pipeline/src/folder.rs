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
    /// unknown (it was opened explicitly, so the user wants to see it), unless it is a folder:
    /// a folder is never an entry of an image list.
    pub fn for_file(file: &Path) -> std::io::Result<Self> {
        // an archive (or a page inside one) browses the archive's images
        let archive = if crate::archive::is_archive(file) && file.is_file() {
            Some(file.to_path_buf())
        } else {
            crate::archive::split(file).map(|(a, _)| a)
        };
        if let Some(archive) = archive {
            let files = crate::archive::list_images(&archive).map_err(|e| std::io::Error::other(e.to_string()))?;
            return Ok(Self { dir: archive, files });
        }
        let dir = file.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let mut list = Self::scan(dir)?;
        if !file.is_dir() && list.position(file).is_none() {
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

    /// `old` was renamed to `new` on disk: the list follows, keeping its natural order. `old` is
    /// found the way [`Self::position`] finds it (so `x.jpg` matches the scanned `.\x.jpg`, and
    /// letter case need not match on Windows). Returns the new entry's index, or `None` when
    /// `old` is not in the list.
    pub fn rename(&mut self, old: &Path, new: &Path) -> Option<usize> {
        let i = self.position(old)?;
        self.files[i] = new.to_path_buf();
        self.files.sort_by(|a, b| natural_cmp(&name_of(a), &name_of(b)));
        self.files.iter().position(|f| f == new)
    }

    /// Index of `path` in the list: the exact path, else the same file name (case-insensitively
    /// on Windows).
    pub fn position(&self, path: &Path) -> Option<usize> {
        // exact first: archive pages in different folders can share a file name
        if let Some(i) = self.files.iter().position(|f| f == path) {
            return Some(i);
        }
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
    fn a_folder_is_never_listed_as_an_image() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.jpg"), b"x").unwrap();
        let sub = dir.path().join("photos");
        std::fs::create_dir(&sub).unwrap();
        for name in ["p2.png", "p10.png", "p1.png"] {
            std::fs::write(sub.join(name), b"x").unwrap();
        }
        let around = FolderList::for_file(&sub).unwrap();
        assert_eq!(around.files(), [dir.path().join("a.jpg")]);
        assert_eq!(around.position(&sub), None);
        // the folder's own images, in natural order
        let inside = FolderList::scan(&sub).unwrap();
        let names: Vec<_> = inside.files().iter().map(|p| name_of(p)).collect();
        assert_eq!(names, ["p1.png", "p2.png", "p10.png"]);
    }

    #[test]
    fn a_renamed_entry_keeps_the_natural_order() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.jpg", "b.jpg", "c.png"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let mut list = FolderList::scan(dir.path()).unwrap();
        // b.jpg became b.webp: still between a and c
        assert_eq!(list.rename(&dir.path().join("b.jpg"), &dir.path().join("b.webp")), Some(1));
        // a.jpg became z.png: moves to the end
        assert_eq!(list.rename(&dir.path().join("a.jpg"), &dir.path().join("z.png")), Some(2));
        let names: Vec<_> = list.files().iter().map(|p| name_of(p)).collect();
        assert_eq!(names, ["b.webp", "c.png", "z.png"]);
        assert_eq!(list.rename(&dir.path().join("gone.jpg"), &dir.path().join("x.jpg")), None);
        assert_eq!(list.len(), 3);
    }

    #[test]
    fn a_rename_finds_the_entry_the_way_position_does() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["a.jpg", "b.jpg", "c.png"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let mut list = FolderList::scan(dir.path()).unwrap();
        // the viewer's path for a file may be spelled unlike the scanned one (`x.jpg` given on
        // the command line, `.\x.jpg` listed); it still names the same entry
        let other_spelling = dir.path().join("sub").join("..").join("b.jpg");
        assert_ne!(other_spelling, list.files()[1]);
        assert_eq!(list.position(&other_spelling), Some(1));
        assert_eq!(list.rename(&other_spelling, &dir.path().join("b.webp")), Some(1));
        let names: Vec<_> = list.files().iter().map(|p| name_of(p)).collect();
        assert_eq!(names, ["a.jpg", "b.webp", "c.png"], "replaced, not added beside the old name");
        if cfg!(windows) {
            // Windows file names ignore letter case
            assert_eq!(list.rename(&dir.path().join("A.JPG"), &dir.path().join("a.png")), Some(0));
            let names: Vec<_> = list.files().iter().map(|p| name_of(p)).collect();
            assert_eq!(names, ["a.png", "b.webp", "c.png"]);
        }
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
