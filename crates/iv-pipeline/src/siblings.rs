//! The folder (or archive) beside the one being browsed, for going on past its last image.
//!
//! The siblings of a folder are the other folders in its parent, in natural name order, without
//! hidden or system folders. The siblings of an archive are the other archives (by extension)
//! in its folder, in natural name order. Ones without a single image are skipped, so the search
//! may list many folders or open many archives: run it off the UI thread.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};

use crate::folder::{FolderList, SortOrder};
use crate::natord::natural_cmp;

/// Hidden or system: such folders are not browsed into (`$RECYCLE.BIN`, `System Volume
/// Information`, a hidden `.thumbnails`).
fn hidden_or_system(entry: &std::fs::DirEntry) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const HIDDEN: u32 = 0x2;
        const SYSTEM: u32 = 0x4;
        entry.metadata().is_ok_and(|m| m.file_attributes() & (HIDDEN | SYSTEM) != 0)
    }
    #[cfg(not(windows))]
    {
        entry.file_name().to_string_lossy().starts_with('.')
    }
}

/// Natural name order, then the raw name, so the order is total.
fn by_name(a: &str, b: &str) -> Ordering {
    natural_cmp(a, b).then_with(|| a.cmp(b))
}

/// Whether two names in one folder name the same entry: on Windows names are compared without
/// case, as the file system does.
fn same_name(a: &str, b: &str) -> bool {
    if cfg!(windows) {
        a.chars().flat_map(char::to_lowercase).eq(b.chars().flat_map(char::to_lowercase))
    } else {
        a == b
    }
}

/// The candidates beside `current` in its parent (folders, or archives when `archive`), in
/// natural order. Empty when `current` has no parent or the parent cannot be read.
fn candidates(current: &Path, archive: bool) -> Vec<(String, PathBuf)> {
    let Some(parent) = current.parent().filter(|p| !p.as_os_str().is_empty()) else { return Vec::new() };
    let Ok(read) = std::fs::read_dir(parent) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf)> = read
        .flatten()
        .filter(|e| {
            let path = e.path();
            if archive {
                crate::archive::is_archive(&path) && path.is_file()
            } else {
                // `is_dir` follows links and junctions, so a linked folder is a sibling too
                path.is_dir() && !hidden_or_system(e)
            }
        })
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path()))
        .collect();
    out.sort_by(|a, b| by_name(&a.0, &b.0));
    out
}

/// The images of `path`, or `None` when it has none or cannot be listed (a broken archive, a
/// folder without permission). An archive reader that crashes on a malformed file only skips
/// that archive.
fn images_of(path: &Path, archive: bool, order: SortOrder) -> Option<FolderList> {
    let list = if archive {
        iv_core::panic::catch("archive reader", || FolderList::for_archive(path).map_err(iv_core::Error::Io)).ok()
    } else {
        FolderList::scan_sorted(path, order).ok()
    };
    list.filter(|l| !l.is_empty())
}

/// The nearest sibling of `current` after it (`forward`) or before it that has images, listed
/// in `order` (an archive's pages in natural order). `archive` says that `current` is an
/// archive file and its siblings are archives; otherwise `current` is a folder. `None` when
/// there is no such sibling: `current` is the last (or first) one with images, or it has no
/// parent folder (a drive's root).
pub fn neighbour(current: &Path, archive: bool, forward: bool, order: SortOrder) -> Option<FolderList> {
    let current = std::path::absolute(current).unwrap_or_else(|_| current.to_path_buf());
    let name = current.file_name()?.to_string_lossy().into_owned();
    let all = candidates(&current, archive);
    // the name as listed: a path typed in another case (`CH1` for `ch1`) would otherwise sort
    // beside the listed name by the raw tie-break, and `current` would be its own sibling
    let name = all.iter().find(|(n, _)| same_name(n, &name)).map_or(name, |(n, _)| n.clone());
    // by name, not by position: `current` itself may be absent (hidden, or deleted meanwhile)
    let wanted = if forward { Ordering::Greater } else { Ordering::Less };
    let mut side: Vec<PathBuf> = all.into_iter().filter(|(n, _)| by_name(n, &name) == wanted).map(|(_, p)| p).collect();
    if !forward {
        side.reverse();
    }
    side.into_iter().find_map(|p| images_of(&p, archive, order))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, names: &[&str]) {
        std::fs::create_dir_all(dir).unwrap();
        for n in names {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
    }

    fn dir_name(list: &FolderList) -> String {
        list.dir().file_name().unwrap().to_string_lossy().into_owned()
    }

    #[test]
    fn folders_go_in_natural_order_and_empty_ones_are_skipped() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        touch(&r.join("ch1"), &["a.jpg", "b.png"]);
        touch(&r.join("ch2"), &["notes.txt"]); // no images
        touch(&r.join("ch3"), &[]); // empty
        touch(&r.join("ch10"), &["p2.png", "p1.png"]);
        touch(&r.join("ch9"), &["x.webp"]);
        std::fs::write(r.join("loose.jpg"), b"x").unwrap(); // a file, not a sibling
        let order = SortOrder::default();

        let next = neighbour(&r.join("ch1"), false, true, order).expect("ch9 follows");
        assert_eq!(dir_name(&next), "ch9", "ch2 has no images, ch3 is empty, 9 < 10");
        let next = neighbour(&r.join("ch9"), false, true, order).unwrap();
        assert_eq!(dir_name(&next), "ch10");
        let names: Vec<_> = next.files().iter().map(|p| p.file_name().unwrap().to_owned()).collect();
        assert_eq!(names, ["p1.png", "p2.png"], "the new folder is listed in the order asked for");
        assert!(neighbour(&r.join("ch10"), false, true, order).is_none(), "ch10 is the last with images");

        let prev = neighbour(&r.join("ch10"), false, false, order).unwrap();
        assert_eq!(dir_name(&prev), "ch9");
        let prev = neighbour(&r.join("ch9"), false, false, order).unwrap();
        assert_eq!(dir_name(&prev), "ch1", "skipping ch3 and ch2 backwards");
        assert!(neighbour(&r.join("ch1"), false, false, order).is_none(), "ch1 is the first");
        // a folder that is gone (or hidden) still has a place among its siblings
        assert_eq!(dir_name(&neighbour(&r.join("ch5"), false, true, order).unwrap()), "ch9");
    }

    #[cfg(windows)]
    #[test]
    fn hidden_and_system_folders_are_not_siblings() {
        use std::os::windows::ffi::OsStrExt;
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        for name in ["a", "b-hidden", "c-system", "d"] {
            touch(&r.join(name), &["1.png"]);
        }
        let set = |name: &str, attributes: u32| {
            let wide: Vec<u16> = r.join(name).as_os_str().encode_wide().chain(Some(0)).collect();
            unsafe extern "system" {
                fn SetFileAttributesW(name: *const u16, attributes: u32) -> i32;
            }
            assert_ne!(unsafe { SetFileAttributesW(wide.as_ptr(), attributes) }, 0);
        };
        set("b-hidden", 0x2);
        set("c-system", 0x4);
        let order = SortOrder::default();
        assert_eq!(dir_name(&neighbour(&r.join("a"), false, true, order).unwrap()), "d");
        assert_eq!(dir_name(&neighbour(&r.join("d"), false, false, order).unwrap()), "a");
        // so the temporary folder can be removed
        set("b-hidden", 0x80);
        set("c-system", 0x80);
    }

    fn zip(path: &Path, entries: &[&str]) {
        let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
        for e in entries {
            w.start_file(*e, zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut w, b"x").unwrap();
        }
        w.finish().unwrap();
    }

    #[test]
    fn archives_go_on_to_the_next_archive_with_images() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        zip(&r.join("vol1.cbz"), &["01.png", "02.png"]);
        zip(&r.join("vol2.cbz"), &["readme.txt"]); // no images
        std::fs::write(r.join("vol3.zip"), b"not a zip at all").unwrap(); // unreadable
        zip(&r.join("vol10.zip"), &["10.png", "2.png"]);
        touch(&r.join("vol5"), &["a.png"]); // a folder: not an archive sibling
        std::fs::write(r.join("vol4.png"), b"x").unwrap(); // an image: not an archive
        let order = SortOrder::default();

        let next = neighbour(&r.join("vol1.cbz"), true, true, order).expect("vol10 follows");
        assert!(next.is_archive());
        assert_eq!(dir_name(&next), "vol10.zip");
        let pages: Vec<_> = next.files().iter().map(|p| p.file_name().unwrap().to_owned()).collect();
        assert_eq!(pages, ["2.png", "10.png"], "pages in natural order");
        assert!(neighbour(&r.join("vol10.zip"), true, true, order).is_none());
        let prev = neighbour(&r.join("vol10.zip"), true, false, order).unwrap();
        assert_eq!(dir_name(&prev), "vol1.cbz");
        assert!(neighbour(&r.join("vol1.cbz"), true, false, order).is_none());
    }

    /// A path may spell a name in another case than the disk (typed on a command line): the
    /// folder or archive is still itself, not its own neighbour.
    #[cfg(windows)]
    #[test]
    fn a_name_in_another_case_is_still_the_current_one() {
        let root = tempfile::tempdir().unwrap();
        let r = root.path();
        touch(&r.join("ch1"), &["a.png"]);
        touch(&r.join("ch2"), &["b.png"]);
        touch(&r.join("CH3"), &["c.png"]);
        let order = SortOrder::default();
        // upper case on the command line, lower case on disk: the raw tie-break put "ch1" after "CH1"
        assert_eq!(dir_name(&neighbour(&r.join("CH1"), false, true, order).unwrap()), "ch2");
        assert!(neighbour(&r.join("CH1"), false, false, order).is_none());
        // lower case on the command line, upper case on disk: "CH3" sorted before "ch3"
        assert_eq!(dir_name(&neighbour(&r.join("ch3"), false, false, order).unwrap()), "ch2");
        assert!(neighbour(&r.join("ch3"), false, true, order).is_none());

        zip(&r.join("vol1.cbz"), &["01.png"]);
        zip(&r.join("VOL2.CBZ"), &["01.png"]);
        assert_eq!(dir_name(&neighbour(&r.join("VOL1.CBZ"), true, true, order).unwrap()), "VOL2.CBZ");
        assert!(neighbour(&r.join("VOL1.CBZ"), true, false, order).is_none());
        assert_eq!(dir_name(&neighbour(&r.join("vol2.cbz"), true, false, order).unwrap()), "vol1.cbz");
        assert!(neighbour(&r.join("vol2.cbz"), true, true, order).is_none());
    }

    #[test]
    fn a_folder_without_a_parent_has_no_siblings() {
        let root = std::path::absolute("/").unwrap();
        assert!(neighbour(&root, false, true, SortOrder::default()).is_none());
    }
}
