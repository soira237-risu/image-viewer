//! The images of one folder in the chosen order (natural file-name order by default), and
//! stepping through them.

use std::cmp::Ordering;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use iv_core::FormatId;

use crate::natord::natural_cmp;

/// What a folder's images are sorted by. Ties (and [`SortBy::Name`]) use natural file-name
/// order, so the order is always total.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum SortBy {
    /// Natural order of the file names: `img2` before `img10`, like Explorer.
    #[default]
    Name,
    /// The file's last-modified time, oldest first.
    Modified,
    /// The file's size in bytes, smallest first.
    Size,
}

/// How a folder's images are ordered. Archive pages ignore it: they always go in natural name
/// order (entries carry no reliable dates, and a comic's pages are named in reading order).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SortOrder {
    pub by: SortBy,
    /// Reverses the key: Z to A, newest first, largest first. Files with an equal date or size
    /// still go in natural name order (A to Z) among themselves.
    pub descending: bool,
}

/// What stepping past the last image (or before the first) does.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum AtEnd {
    /// Continue at the other end of the same folder.
    #[default]
    Wrap,
    /// Stay at the end.
    Stop,
    /// Go on to the next (or previous) sibling folder, or archive, that has images.
    NextFolder,
}

/// What a file sorts by: its name, and the date and size read when the folder was listed
/// (`None` / 0 when they could not be read).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SortKey {
    pub name: String,
    pub modified: Option<SystemTime>,
    pub size: u64,
}

impl SortKey {
    /// The key of `path` as it is on disk now (name only when it cannot be read).
    pub fn read(path: &Path) -> Self {
        let meta = std::fs::metadata(path).ok();
        Self::new(path, meta.as_ref())
    }

    fn new(path: &Path, meta: Option<&std::fs::Metadata>) -> Self {
        Self {
            name: name_of(path),
            modified: meta.and_then(|m| m.modified().ok()),
            size: meta.map_or(0, std::fs::Metadata::len),
        }
    }

    /// Where `self` sorts relative to `other` in `order`.
    pub fn cmp_in(&self, other: &Self, order: SortOrder) -> Ordering {
        let name = natural_cmp(&self.name, &other.name);
        let key = match order.by {
            SortBy::Name => return if order.descending { name.reverse() } else { name },
            SortBy::Modified => self.modified.cmp(&other.modified),
            SortBy::Size => self.size.cmp(&other.size),
        };
        (if order.descending { key.reverse() } else { key }).then(name)
    }
}

#[derive(Clone, Debug, Default)]
pub struct FolderList {
    dir: PathBuf,
    files: Vec<PathBuf>,
    /// The sort key of each entry of `files`, at the same index.
    keys: Vec<SortKey>,
    order: SortOrder,
    /// `dir` is an archive file and the entries are its pages.
    archive: bool,
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
        Self::scan_sorted(dir, SortOrder::default())
    }

    /// [`Self::scan`] in `order`. Dates and sizes come with the directory listing (no extra
    /// call per file on Windows), so every order costs the same.
    pub fn scan_sorted(dir: &Path, order: SortOrder) -> std::io::Result<Self> {
        let (files, keys) = std::fs::read_dir(dir)?
            .flatten()
            .filter(|e| e.file_type().is_ok_and(|t| t.is_file()))
            .map(|e| (e.path(), e.metadata().ok()))
            .filter(|(p, _)| is_image(p))
            .map(|(p, meta)| {
                let key = SortKey::new(&p, meta.as_ref());
                (p, key)
            })
            .unzip();
        let mut list = Self { dir: dir.to_path_buf(), files, keys, order, archive: false };
        list.sort();
        Ok(list)
    }

    /// The images of the archive file `archive`, in natural order.
    pub fn for_archive(archive: &Path) -> std::io::Result<Self> {
        let files = crate::archive::list_images(archive).map_err(|e| std::io::Error::other(e.to_string()))?;
        let keys = files.iter().map(|f| SortKey { name: name_of(f), ..SortKey::default() }).collect();
        // already in natural order: archive pages are never re-sorted
        Ok(Self { dir: archive.to_path_buf(), files, keys, order: SortOrder::default(), archive: true })
    }

    /// The folder holding `file`, scanned. `file` itself is included even when its extension is
    /// unknown (it was opened explicitly, so the user wants to see it), unless it is a folder:
    /// a folder is never an entry of an image list.
    pub fn for_file(file: &Path) -> std::io::Result<Self> {
        Self::for_file_sorted(file, SortOrder::default())
    }

    /// [`Self::for_file`] in `order` (an archive's pages keep their natural order).
    pub fn for_file_sorted(file: &Path, order: SortOrder) -> std::io::Result<Self> {
        // an archive (or a page inside one) browses the archive's images
        let archive = if crate::archive::is_archive(file) && file.is_file() {
            Some(file.to_path_buf())
        } else {
            crate::archive::split(file).map(|(a, _)| a)
        };
        if let Some(archive) = archive {
            return Self::for_archive(&archive);
        }
        let dir = file.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
        let mut list = Self::scan_sorted(dir, order)?;
        if !file.is_dir() && list.position(file).is_none() {
            list.files.push(file.to_path_buf());
            list.keys.push(SortKey::read(file));
            list.sort();
        }
        Ok(list)
    }

    /// Puts the entries in `self.order`, keeping each key with its file.
    fn sort(&mut self) {
        let order = self.order;
        let mut entries: Vec<(PathBuf, SortKey)> = self.files.drain(..).zip(self.keys.drain(..)).collect();
        entries.sort_by(|a, b| a.1.cmp_in(&b.1, order));
        (self.files, self.keys) = entries.into_iter().unzip();
    }

    /// The folder (or, for an archive's pages, the archive file).
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether the entries are the pages of the archive [`Self::dir`].
    pub fn is_archive(&self) -> bool {
        self.archive
    }

    pub fn order(&self) -> SortOrder {
        self.order
    }

    /// Sorts the list again in `order`. An archive's pages keep their natural order.
    pub fn set_order(&mut self, order: SortOrder) {
        if self.archive || order == self.order {
            return;
        }
        self.order = order;
        self.sort();
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

    /// `old` was renamed to `new` on disk: the list follows, keeping its order. `old` is found
    /// the way [`Self::position`] finds it (so `x.jpg` matches the scanned `.\x.jpg`, and letter
    /// case need not match on Windows). Returns the new entry's index, or `None` when `old` is
    /// not in the list.
    pub fn rename(&mut self, old: &Path, new: &Path) -> Option<usize> {
        let i = self.position(old)?;
        self.files[i] = new.to_path_buf();
        let key = &mut self.keys[i];
        key.name = name_of(new);
        // a rename keeps the date and size; read them again in case the file changed meanwhile
        if !self.archive
            && let Ok(meta) = std::fs::metadata(new)
        {
            *key = SortKey::new(new, Some(&meta));
        }
        self.sort();
        self.files.iter().position(|f| f == new)
    }

    /// `path` is gone from disk (deleted): drops its entry, found the way [`Self::position`]
    /// finds it. Returns the index the entry had, or `None` when `path` is not in the list.
    /// The entries after it move up by one, so that index now names the image that followed.
    pub fn remove(&mut self, path: &Path) -> Option<usize> {
        let i = self.position(path)?;
        self.files.remove(i);
        self.keys.remove(i);
        Some(i)
    }

    /// Where `path` sorts in this list by its name alone: see [`Self::index_after_key`].
    pub fn index_after(&self, path: &Path) -> Option<usize> {
        self.index_after_key(&SortKey { name: name_of(path), ..SortKey::default() })
    }

    /// Where a file with sort key `key` sorts in this list: the index of the first entry that
    /// comes after it in the list's order, else the last entry (it sorts after everything).
    /// `None` for an empty list. For a file that is no longer in the list (with the key it had),
    /// this is the image that followed it.
    pub fn index_after_key(&self, key: &SortKey) -> Option<usize> {
        let last = self.files.len().checked_sub(1)?;
        Some(self.keys.iter().position(|k| k.cmp_in(key, self.order).is_gt()).unwrap_or(last))
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

/// Where a navigation step leads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Move {
    /// To this index of the same list.
    To(usize),
    /// Past the last image (`forward`) or before the first: on to the neighbouring folder or
    /// archive.
    Cross { forward: bool },
    /// Nowhere.
    Stay,
}

/// A step of `delta` from `index` in a list of `len`, with `at_end` deciding what happens at
/// the ends. `|delta|` images are on screen from `index` (two in two-page view). With
/// [`AtEnd::NextFolder`] the step crosses into the next folder when nothing is left after what
/// is on screen (into the previous one from the first image), except while the key is held
/// (`repeat`): auto-repeat stops at the end, and crossing takes a fresh press.
pub fn step_or_cross(index: usize, len: usize, delta: isize, at_end: AtEnd, repeat: bool) -> Move {
    if len == 0 || delta == 0 {
        return Move::Stay;
    }
    if at_end != AtEnd::NextFolder {
        return step(index, len, delta, at_end == AtEnd::Wrap).map_or(Move::Stay, Move::To);
    }
    let forward = delta > 0;
    let stride = delta.unsigned_abs();
    let at_edge = if forward { index + stride >= len } else { index == 0 };
    match (at_edge, repeat) {
        (true, true) => Move::Stay,
        (true, false) => Move::Cross { forward },
        (false, _) if forward => Move::To(index + stride),
        (false, _) => Move::To(index.saturating_sub(stride)),
    }
}

/// The image to show on entering a list of `len` images: the first going forward, the last
/// going back. With `stride` images on screen at a time (two-page view), going back lands on
/// the spread that stepping forward from the first image reaches, so the pages pair up the same
/// whichever way the list was entered.
pub fn entry_index(len: usize, forward: bool, stride: usize) -> usize {
    if forward || len == 0 {
        0
    } else {
        let stride = stride.max(1);
        (len - 1) / stride * stride
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
    fn a_deleted_entry_leaves_the_list_and_its_follower_takes_its_index() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["img1.jpg", "img2.jpg", "img10.png"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let mut list = FolderList::scan(dir.path()).unwrap();
        // found like position() finds it: another spelling of the same file
        let other_spelling = dir.path().join("sub").join("..").join("img2.jpg");
        assert_eq!(list.remove(&other_spelling), Some(1));
        let names: Vec<_> = list.files().iter().map(|p| name_of(p)).collect();
        assert_eq!(names, ["img1.jpg", "img10.png"]);
        assert_eq!(name_of(list.get(1).unwrap()), "img10.png", "the next image moved into its place");
        assert_eq!(list.remove(&dir.path().join("img2.jpg")), None, "already gone");
        assert_eq!(list.remove(&dir.path().join("img10.png")), Some(1));
        assert_eq!(list.remove(&dir.path().join("img1.jpg")), Some(0));
        assert!(list.is_empty());
    }

    #[test]
    fn index_after_finds_the_image_that_followed_a_missing_file() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["img1.jpg", "img3.jpg", "img10.png"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let list = FolderList::scan(dir.path()).unwrap();
        // natural order: img2 sits between img1 and img3, img4 between img3 and img10
        assert_eq!(list.index_after(&dir.path().join("img2.jpg")), Some(1));
        assert_eq!(list.index_after(&dir.path().join("img4.webp")), Some(2));
        assert_eq!(list.index_after(&dir.path().join("img0.jpg")), Some(0));
        // past the end: the last image
        assert_eq!(list.index_after(&dir.path().join("img99.jpg")), Some(2));
        assert_eq!(FolderList::default().index_after(&dir.path().join("a.jpg")), None);
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

    fn names(list: &FolderList) -> Vec<String> {
        list.files().iter().map(|p| name_of(p)).collect()
    }

    /// Files with the given names, sizes and modification times (seconds after an epoch).
    fn files_with(dir: &Path, files: &[(&str, usize, u64)]) {
        let epoch = SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        for (name, size, secs) in files {
            let path = dir.join(name);
            std::fs::write(&path, vec![0u8; *size]).unwrap();
            let f = std::fs::File::options().write(true).open(&path).unwrap();
            f.set_modified(epoch + std::time::Duration::from_secs(*secs)).unwrap();
        }
    }

    #[test]
    fn sorting_by_date_and_size_breaks_ties_by_natural_name() {
        let dir = tempfile::tempdir().unwrap();
        // name, size, modified
        files_with(
            dir.path(),
            &[("img10.jpg", 30, 100), ("img2.png", 10, 300), ("img1.webp", 20, 100), ("b.gif", 10, 200)],
        );
        let sorted =
            |by, descending| names(&FolderList::scan_sorted(dir.path(), SortOrder { by, descending }).unwrap());
        assert_eq!(sorted(SortBy::Name, false), ["b.gif", "img1.webp", "img2.png", "img10.jpg"]);
        assert_eq!(sorted(SortBy::Name, true), ["img10.jpg", "img2.png", "img1.webp", "b.gif"]);
        // img1 and img10 share a date: natural order between them, whichever way the dates go
        assert_eq!(sorted(SortBy::Modified, false), ["img1.webp", "img10.jpg", "b.gif", "img2.png"]);
        assert_eq!(sorted(SortBy::Modified, true), ["img2.png", "b.gif", "img1.webp", "img10.jpg"]);
        // b and img2 share a size
        assert_eq!(sorted(SortBy::Size, false), ["b.gif", "img2.png", "img1.webp", "img10.jpg"]);
        assert_eq!(sorted(SortBy::Size, true), ["img10.jpg", "img1.webp", "b.gif", "img2.png"]);
        // the default is today's natural order
        assert_eq!(names(&FolderList::scan(dir.path()).unwrap()), sorted(SortBy::Name, false));
    }

    #[test]
    fn re_sorting_keeps_every_file_and_its_key() {
        let dir = tempfile::tempdir().unwrap();
        files_with(dir.path(), &[("a.jpg", 3, 30), ("b.jpg", 1, 10), ("c.jpg", 2, 20)]);
        let mut list = FolderList::scan(dir.path()).unwrap();
        assert_eq!(names(&list), ["a.jpg", "b.jpg", "c.jpg"]);
        list.set_order(SortOrder { by: SortBy::Size, descending: false });
        assert_eq!(names(&list), ["b.jpg", "c.jpg", "a.jpg"]);
        assert_eq!(list.position(&dir.path().join("a.jpg")), Some(2));
        list.set_order(SortOrder { by: SortBy::Modified, descending: true });
        assert_eq!(names(&list), ["a.jpg", "c.jpg", "b.jpg"]);
        // a renamed file keeps its date: still first, newest
        std::fs::rename(dir.path().join("a.jpg"), dir.path().join("z.jpg")).unwrap();
        assert_eq!(list.rename(&dir.path().join("a.jpg"), &dir.path().join("z.jpg")), Some(0));
        // the image after a deleted one, by the key it had before it went
        let key = SortKey::read(&dir.path().join("c.jpg"));
        assert_eq!(list.remove(&dir.path().join("c.jpg")), Some(1));
        assert_eq!(list.index_after_key(&key), Some(1), "b.jpg followed c.jpg");
        assert_eq!(names(&list), ["z.jpg", "b.jpg"]);
    }

    #[test]
    fn an_opened_file_with_an_odd_extension_sorts_by_its_own_key() {
        let dir = tempfile::tempdir().unwrap();
        files_with(dir.path(), &[("a.jpg", 5, 10), ("b.dat", 1, 20), ("c.jpg", 9, 0)]);
        let order = SortOrder { by: SortBy::Size, descending: false };
        let list = FolderList::for_file_sorted(&dir.path().join("b.dat"), order).unwrap();
        assert_eq!(names(&list), ["b.dat", "a.jpg", "c.jpg"]);
        assert_eq!(list.order(), order);
        assert!(!list.is_archive());
    }

    #[test]
    fn archive_pages_keep_their_natural_order() {
        let dir = tempfile::tempdir().unwrap();
        let cbz = dir.path().join("book.cbz");
        let mut w = zip::ZipWriter::new(std::fs::File::create(&cbz).unwrap());
        for (name, size) in [("p10.png", 1), ("p2.png", 50), ("p1.png", 9)] {
            w.start_file(name, zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut w, &vec![0u8; size]).unwrap();
        }
        w.finish().unwrap();
        let order = SortOrder { by: SortBy::Size, descending: true };
        let mut list = FolderList::for_file_sorted(&cbz, order).unwrap();
        assert!(list.is_archive());
        assert_eq!(names(&list), ["p1.png", "p2.png", "p10.png"]);
        list.set_order(SortOrder { by: SortBy::Name, descending: true });
        assert_eq!(names(&list), ["p1.png", "p2.png", "p10.png"], "the setting does not apply to archives");
    }

    #[test]
    fn wrap_and_stop_keep_their_old_meaning() {
        for (index, len, delta) in [(0, 5, 1), (4, 5, 1), (0, 5, -1), (3, 4, 2), (2, 5, -10)] {
            for (at_end, wrap) in [(AtEnd::Wrap, true), (AtEnd::Stop, false)] {
                let old = step(index, len, delta, wrap).map_or(Move::Stay, Move::To);
                assert_eq!(step_or_cross(index, len, delta, at_end, false), old);
                assert_eq!(step_or_cross(index, len, delta, at_end, true), old);
            }
        }
    }

    #[test]
    fn next_folder_crosses_only_past_the_ends_and_never_on_auto_repeat() {
        let nf = AtEnd::NextFolder;
        assert_eq!(step_or_cross(0, 3, 1, nf, false), Move::To(1));
        assert_eq!(step_or_cross(2, 3, 1, nf, false), Move::Cross { forward: true });
        assert_eq!(step_or_cross(0, 3, -1, nf, false), Move::Cross { forward: false });
        assert_eq!(step_or_cross(2, 3, -1, nf, false), Move::To(1));
        // a held key stops at the end
        assert_eq!(step_or_cross(2, 3, 1, nf, true), Move::Stay);
        assert_eq!(step_or_cross(0, 3, -1, nf, true), Move::Stay);
        assert_eq!(step_or_cross(1, 3, 1, nf, true), Move::To(2), "but moves within the folder");
        // a folder of one image: both ways cross
        assert_eq!(step_or_cross(0, 1, 1, nf, false), Move::Cross { forward: true });
        assert_eq!(step_or_cross(0, 1, -1, nf, false), Move::Cross { forward: false });
        assert_eq!(step_or_cross(0, 0, 1, nf, false), Move::Stay);

        // two pages: the spread 3-4 of five is not the end, 5 alone (or 3-4 of four) is
        assert_eq!(step_or_cross(2, 5, 2, nf, false), Move::To(4));
        assert_eq!(step_or_cross(4, 5, 2, nf, false), Move::Cross { forward: true });
        assert_eq!(step_or_cross(2, 4, 2, nf, false), Move::Cross { forward: true });
        assert_eq!(step_or_cross(1, 4, -2, nf, false), Move::To(0));
        assert_eq!(step_or_cross(0, 4, -2, nf, false), Move::Cross { forward: false });
        assert_eq!(step_or_cross(2, 4, 2, nf, true), Move::Stay);
    }

    #[test]
    fn entering_a_folder_backwards_lands_on_its_last_spread() {
        assert_eq!(entry_index(7, true, 1), 0);
        assert_eq!(entry_index(7, false, 1), 6);
        // two pages: 1-2, 3-4, 5-6, 7 / 1-2, 3-4
        assert_eq!(entry_index(7, false, 2), 6);
        assert_eq!(entry_index(4, false, 2), 2);
        assert_eq!(entry_index(1, false, 2), 0);
        assert_eq!(entry_index(0, false, 2), 0);
    }
}
