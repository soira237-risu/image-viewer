//! Images inside ZIP / CBZ and 7z / CB7 archives, addressed by virtual paths: the archive's path followed by
//! the entry name, e.g. `C:\comics\vol1.cbz\pages/001.jpg`. Everything keyed by path (cache,
//! prefetch, navigation, Save As) works on entries unchanged; only reading goes through here.

use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use iv_core::{Error, FormatId, Result, Source};

use crate::natord::natural_cmp;

const ZIP: &[&str] = &["zip", "cbz"];
const SEVEN_Z: &[&str] = &["7z", "cb7"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Zip,
    SevenZ,
}

fn kind(path: &Path) -> Option<Kind> {
    let ext = path.extension()?.to_str()?;
    let is = |list: &[&str]| list.iter().any(|x| x.eq_ignore_ascii_case(ext));
    if is(ZIP) {
        Some(Kind::Zip)
    } else if is(SEVEN_Z) {
        Some(Kind::SevenZ)
    } else {
        None
    }
}

/// Whether `path` names an archive this module can open (by extension).
pub fn is_archive(path: &Path) -> bool {
    kind(path).is_some()
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

fn open_zip(archive: &Path) -> Result<zip::ZipArchive<BufReader<File>>> {
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

/// Encoded bytes of 7z entries already extracted, per archive. In a solid archive every entry
/// can only be reached by decompressing everything before it, so reading pages one by one would
/// cost O(pages²); instead each pass keeps every image it walks past (measured on a 4-page solid
/// 7z: 2.0 s for all pages without this).
struct SevenZCache {
    entries: std::collections::HashMap<(PathBuf, String), std::sync::Arc<Vec<u8>>>,
    bytes: usize,
}

/// Upper bound for cached encoded entries; over it the cache starts afresh.
const SEVEN_Z_CACHE_BYTES: usize = 512 * 1024 * 1024;
/// Image entries decompressed past the requested one.
const SEVEN_Z_READ_AHEAD: usize = 6;

fn seven_z_cache() -> &'static std::sync::Mutex<SevenZCache> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<SevenZCache>> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(SevenZCache { entries: Default::default(), bytes: 0 }))
}

/// One entry of a 7z archive, decompressing (and caching) every image entry on the way to it.
fn seven_z_read(archive: &Path, entry: &str) -> Result<Vec<u8>> {
    let key = (archive.to_path_buf(), entry.to_string());
    if let Some(hit) = seven_z_cache().lock().unwrap_or_else(|e| e.into_inner()).entries.get(&key) {
        return Ok(hit.as_ref().clone());
    }
    let mut reader = sevenz_rust2::ArchiveReader::open(archive, sevenz_rust2::Password::empty())
        .map_err(|e| Error::decode("7z", e))?;
    let mut found = None;
    let mut after_target = 0;
    reader
        .for_each_entries(|e, data| {
            let name = e.name().replace('\\', "/");
            if e.is_directory() || !is_image_entry(&name) {
                return Ok(true);
            }
            let mut bytes = Vec::with_capacity(e.size() as usize);
            data.read_to_end(&mut bytes)?;
            let bytes = std::sync::Arc::new(bytes);
            {
                let mut cache = seven_z_cache().lock().unwrap_or_else(|e| e.into_inner());
                if cache.bytes + bytes.len() > SEVEN_Z_CACHE_BYTES {
                    cache.entries.clear();
                    cache.bytes = 0;
                }
                cache.bytes += bytes.len();
                cache.entries.insert((archive.to_path_buf(), name.clone()), bytes.clone());
            }
            if found.is_some() {
                after_target += 1;
            } else if name == entry {
                found = Some(bytes);
            }
            // go a little past the target (the next pages are about to be asked for), but not
            // through a whole book before the first page can show
            Ok(after_target < SEVEN_Z_READ_AHEAD)
        })
        .map_err(|e| Error::decode("7z", format!("{entry}: {e}")))?;
    found.map(|b| b.as_ref().clone()).ok_or_else(|| Error::decode("7z", format!("{entry}: not in the archive")))
}

/// Entry names of a 7z archive: (normalised with `/` separators, as stored). Directories skipped.
fn seven_z_names(archive: &Path) -> Result<Vec<(String, String)>> {
    let reader = sevenz_rust2::ArchiveReader::open(archive, sevenz_rust2::Password::empty())
        .map_err(|e| Error::decode("7z", e))?;
    Ok(reader
        .archive()
        .files
        .iter()
        .filter(|f| !f.is_directory() && f.has_stream)
        .map(|f| (f.name().replace('\\', "/"), f.name().to_string()))
        .collect())
}

/// The image entries of `archive` as virtual paths, in natural order of their full names
/// (so `ch2/01.jpg` comes before `ch10/01.jpg`).
pub fn list_images(archive: &Path) -> Result<Vec<PathBuf>> {
    let mut names: Vec<String> = match kind(archive) {
        Some(Kind::Zip) => open_zip(archive)?.file_names().filter_map(|n| n.ok()).map(|n| n.into_owned()).collect(),
        Some(Kind::SevenZ) => seven_z_names(archive)?.into_iter().map(|(normal, _)| normal).collect(),
        None => return Err(Error::UnsupportedFormat),
    };
    names.retain(|n| is_image_entry(n));
    names.sort_by(|a, b| natural_cmp(a, b));
    Ok(names.iter().map(|n| entry_path(archive, n)).collect())
}

/// A [`Source`] for a real file or an archive entry (read into memory).
pub fn source_for(path: &Path) -> Result<Source> {
    let Some((archive, entry)) = split(path) else { return Ok(Source::from_path(path)) };
    let bytes = match kind(&archive) {
        Some(Kind::Zip) => {
            let mut zip = open_zip(&archive)?;
            let mut file = zip.by_name(&entry).map_err(|e| Error::decode("zip", format!("{entry}: {e}")))?;
            let mut bytes = Vec::with_capacity(file.size() as usize);
            file.read_to_end(&mut bytes)?;
            bytes
        }
        Some(Kind::SevenZ) => seven_z_read(&archive, &entry)?,
        None => return Err(Error::UnsupportedFormat),
    };
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

#[cfg(test)]
mod seven_z_tests {
    use super::*;

    #[test]
    fn seven_z_pages_list_and_read() {
        let dir = tempfile::tempdir().unwrap();
        let cb7 = dir.path().join("book.cb7");
        {
            let mut w = sevenz_rust2::ArchiveWriter::create(&cb7).unwrap();
            for (name, data) in [("p10.png", &b"ten"[..]), ("p2.png", b"two"), ("readme.txt", b"no")] {
                w.push_archive_entry(sevenz_rust2::ArchiveEntry::new_file(name), Some(data)).unwrap();
            }
            w.finish().unwrap();
        }
        assert!(is_archive(&cb7));
        let pages = list_images(&cb7).unwrap();
        let names: Vec<_> = pages.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["p2.png", "p10.png"]);
        assert_eq!(&*source_for(&pages[1]).unwrap().read_all().unwrap(), b"ten");
        assert!(source_for(&cb7.join("missing.png")).is_err());
    }
}
