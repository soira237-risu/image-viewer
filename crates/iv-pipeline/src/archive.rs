//! Images inside ZIP / CBZ archives, addressed by virtual paths: the archive's path followed by
//! the entry name, e.g. `C:\comics\vol1.cbz\pages/001.jpg`. Everything keyed by path (cache,
//! prefetch, navigation, Save As) works on entries unchanged; only reading goes through here.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use iv_core::{Error, FormatId, Result, Source};

use crate::natord::natural_cmp;

const EXTENSIONS: &[&str] = &["zip", "cbz"];

/// Whether `path` names an archive this module can open (by extension).
pub fn is_archive(path: &Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.iter().any(|x| x.eq_ignore_ascii_case(e)))
}

/// Splits a virtual path into the archive file and the entry name inside it.
pub fn split(path: &Path) -> Option<(PathBuf, String)> {
    if path.is_file() {
        return None;
    }
    let mut archive = path;
    while let Some(parent) = archive.parent() {
        archive = parent;
        if is_archive(archive) && archive.is_file() {
            let rest = path.strip_prefix(archive).ok()?;
            let entry: Vec<String> = rest.components().map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
            return (!entry.is_empty()).then(|| (archive.to_path_buf(), entry.join("/")));
        }
    }
    None
}

/// The virtual path of `entry` inside `archive`.
pub fn entry_path(archive: &Path, entry: &str) -> PathBuf {
    entry.split('/').filter(|s| !s.is_empty()).fold(archive.to_path_buf(), |p, part| p.join(part))
}

fn open(archive: &Path) -> Result<zip::ZipArchive<BufReader<File>>> {
    zip::ZipArchive::new(BufReader::new(File::open(archive)?)).map_err(|e| Error::decode("zip", e))
}

fn is_image_entry(name: &str) -> bool {
    if name.ends_with('/')
        || name.starts_with("__MACOSX/")
        || name.rsplit('/').next().is_some_and(|f| f.starts_with("._"))
    {
        return false;
    }
    Path::new(name).extension().and_then(|e| e.to_str()).and_then(FormatId::from_extension).is_some()
}

/// The image entries of `archive` as virtual paths, in natural order of their full names
/// (so `ch2/01.jpg` comes before `ch10/01.jpg`).
pub fn list_images(archive: &Path) -> Result<Vec<PathBuf>> {
    let zip = open(archive)?;
    let mut names: Vec<String> =
        zip.file_names().filter_map(|n| n.ok()).filter(|n| is_image_entry(n)).map(|n| n.into_owned()).collect();
    names.sort_by(|a, b| natural_cmp(a, b));
    Ok(names.iter().map(|n| entry_path(archive, n)).collect())
}

/// A [`Source`] for a real file or an archive entry (read into memory).
pub fn source_for(path: &Path) -> Result<Source> {
    let Some((archive, entry)) = split(path) else { return Ok(Source::from_path(path)) };
    let mut zip = open(&archive)?;
    let mut file = zip.by_name(&entry).map_err(|e| Error::decode("zip", format!("{entry}: {e}")))?;
    let mut bytes = Vec::with_capacity(file.size() as usize);
    file.read_to_end(&mut bytes)?;
    let name = entry.rsplit('/').next().unwrap_or(&entry).to_string();
    Ok(Source::from_bytes(name, bytes))
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

    fn make_zip(path: &Path, entries: &[(&str, &[u8])]) {
        let mut w = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, data) in entries {
            if name.ends_with('/') {
                w.add_directory(*name, zip::write::SimpleFileOptions::default()).unwrap();
            } else {
                w.start_file(*name, zip::write::SimpleFileOptions::default()).unwrap();
                w.write_all(data).unwrap();
            }
        }
        w.finish().unwrap();
    }

    #[test]
    fn lists_images_naturally_and_reads_entries() {
        let dir = tempfile::tempdir().unwrap();
        let cbz = dir.path().join("Vol 1.cbz");
        make_zip(
            &cbz,
            &[
                ("ch10/", b""),
                ("ch10/01.png", b"ten"),
                ("ch2/10.jpg", b"two-ten"),
                ("ch2/2.jpg", b"two-two"),
                ("notes.txt", b"not an image"),
                ("__MACOSX/ch2/._2.jpg", b"resource fork"),
            ],
        );
        let pages = list_images(&cbz).unwrap();
        let names: Vec<_> =
            pages.iter().map(|p| p.strip_prefix(&cbz).unwrap().to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(names, ["ch2/2.jpg", "ch2/10.jpg", "ch10/01.png"]);

        let (archive, entry) = split(&pages[1]).unwrap();
        assert_eq!(archive, cbz);
        assert_eq!(entry, "ch2/10.jpg");
        let src = source_for(&pages[1]).unwrap();
        assert_eq!(&*src.read_all().unwrap(), b"two-ten");
        assert_eq!(src.extension().as_deref(), Some("jpg"));
    }

    #[test]
    fn real_files_are_not_split() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.png");
        std::fs::write(&f, b"x").unwrap();
        assert!(split(&f).is_none());
        assert!(source_for(&f).unwrap().path().is_some());
        assert!(split(&dir.path().join("missing.zip").join("x.png")).is_none(), "no such archive");
    }
}
