//! Images inside ZIP / CBZ and 7z / CB7 archives, addressed by virtual paths: the archive's path followed by
//! the entry name, e.g. `C:\comics\vol1.cbz\pages/001.jpg`. Everything keyed by path (cache,
//! prefetch, navigation, Save As) works on entries unchanged; only reading goes through here.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

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

/// Largest archive entry read into memory. Sizes in archive headers are whatever the file says,
/// and a few kilobytes of deflate or LZMA can expand to gigabytes: both the declared size and
/// what is actually read are held to this.
pub const MAX_ENTRY_BYTES: u64 = 1 << 30;

/// Reads one entry of at most `limit` bytes. `declared` is the size the archive's header gives;
/// it only sizes the first allocation (up to 64 MiB), it is never trusted beyond that.
fn read_entry(r: &mut dyn Read, declared: u64, limit: u64) -> std::io::Result<Vec<u8>> {
    let too_big =
        |bytes: u64| std::io::Error::other(format!("{bytes} bytes, over the {limit}-byte limit for one entry"));
    if declared > limit {
        return Err(too_big(declared));
    }
    let mut bytes = Vec::with_capacity(declared.min(64 << 20) as usize);
    r.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(too_big(bytes.len() as u64));
    }
    Ok(bytes)
}

/// An entry name with `/` separators (ZIPs written by some Windows tools use `\`), or `None`
/// when the name cannot be a page of the archive's virtual folder: absolute (`/x`, `C:/x`), with
/// `.`, `..` or empty components, or with a `:` (a drive or an NTFS stream). As virtual paths
/// such names would point outside the archive or fail to round-trip through [`split`].
fn clean_name(raw: &str) -> Option<String> {
    let name = raw.replace('\\', "/");
    if name.starts_with('/') || name.contains(':') || name.contains('\0') {
        return None;
    }
    let body = name.strip_suffix('/').unwrap_or(&name);
    if body.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
        return None;
    }
    Some(name)
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

/// Which file on disk the cached entries came from: a replaced or edited archive is read afresh.
#[derive(Clone, PartialEq, Eq)]
struct ArchiveId {
    path: PathBuf,
    len: u64,
    modified: Option<std::time::SystemTime>,
}

impl ArchiveId {
    fn of(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path)?;
        Ok(Self { path: path.to_path_buf(), len: meta.len(), modified: meta.modified().ok() })
    }
}

/// Encoded bytes of 7z entries already extracted, for the archive being browsed. In a solid
/// archive every entry can only be reached by decompressing everything before it, so reading
/// pages one by one would cost O(pages²); instead each pass keeps every image it walks past
/// (measured on a 4-page solid 7z: 2.0 s for all pages without this). Entries are shared with
/// the sources made from them, never copied.
#[derive(Default)]
struct SevenZCache {
    archive: Option<ArchiveId>,
    entries: HashMap<String, Arc<[u8]>>,
    bytes: usize,
}

impl SevenZCache {
    /// Empties the cache unless it holds entries of `id`.
    fn select(&mut self, id: &ArchiveId) {
        if self.archive.as_ref() != Some(id) {
            *self = SevenZCache { archive: Some(id.clone()), ..Default::default() };
        }
    }

    fn insert(&mut self, name: String, bytes: Arc<[u8]>) {
        if bytes.len() > SEVEN_Z_CACHE_BYTES {
            return;
        }
        if self.bytes + bytes.len() > SEVEN_Z_CACHE_BYTES {
            self.entries.clear();
            self.bytes = 0;
        }
        self.bytes += bytes.len();
        if let Some(old) = self.entries.insert(name, bytes) {
            self.bytes -= old.len();
        }
    }
}

/// Upper bound for cached encoded entries; over it the cache starts afresh.
const SEVEN_Z_CACHE_BYTES: usize = 128 * 1024 * 1024;
/// Image entries decompressed past the requested one.
const SEVEN_Z_READ_AHEAD: usize = 6;

fn seven_z_cache() -> MutexGuard<'static, SevenZCache> {
    static CACHE: OnceLock<Mutex<SevenZCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// Frees the encoded 7z pages kept for quick page turns, e.g. once the viewer has left the
/// archive. Reading an entry again just decompresses it again.
pub fn release_cache() {
    *seven_z_cache() = SevenZCache::default();
}

/// One entry of a 7z archive, decompressing (and caching) every image entry on the way to it.
fn seven_z_read(archive: &Path, entry: &str, limit: u64) -> Result<Arc<[u8]>> {
    let id = ArchiveId::of(archive)?;
    {
        let mut cache = seven_z_cache();
        cache.select(&id);
        if let Some(hit) = cache.entries.get(entry) {
            return Ok(hit.clone());
        }
    }
    let mut reader = sevenz_rust2::ArchiveReader::open(archive, sevenz_rust2::Password::empty())
        .map_err(|e| Error::decode("7z", e))?;
    let mut found = None;
    let mut after_target = 0;
    reader
        .for_each_entries(|e, data| {
            let Some(name) = clean_name(e.name()) else { return Ok(true) };
            if e.is_directory() || !is_image_entry(&name) {
                return Ok(true);
            }
            let bytes: Arc<[u8]> = read_entry(data, e.size(), limit)
                .map_err(|err| std::io::Error::other(format!("{name}: {err}")))?
                .into();
            {
                let mut cache = seven_z_cache();
                cache.select(&id);
                cache.insert(name.clone(), bytes.clone());
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
    found.ok_or_else(|| Error::decode("7z", format!("{entry}: not in the archive")))
}

/// Entry names of a 7z archive with `/` separators, unsafe names left out. Directories skipped.
fn seven_z_names(archive: &Path) -> Result<Vec<String>> {
    let reader = sevenz_rust2::ArchiveReader::open(archive, sevenz_rust2::Password::empty())
        .map_err(|e| Error::decode("7z", e))?;
    Ok(reader
        .archive()
        .files
        .iter()
        .filter(|f| !f.is_directory() && f.has_stream)
        .filter_map(|f| clean_name(f.name()))
        .collect())
}

/// One entry of a ZIP archive by its cleaned name (see [`clean_name`]).
fn zip_read(archive: &Path, entry: &str, limit: u64) -> Result<Vec<u8>> {
    let archive_len = std::fs::metadata(archive)?.len();
    let mut zip = open_zip(archive)?;
    // stored names usually match as they are; ones written with `\` are found by their clean
    // form (`file_names` lists entries in index order)
    let index = zip
        .index_for_name(entry)
        .or_else(|| zip.file_names().position(|n| n.ok().and_then(|n| clean_name(&n)).as_deref() == Some(entry)))
        .ok_or_else(|| Error::decode("zip", format!("{entry}: not in the archive")))?;
    let mut file = zip.by_index(index).map_err(|e| Error::decode("zip", format!("{entry}: {e}")))?;
    if file.compressed_size() > archive_len {
        return Err(Error::decode("zip", format!("{entry}: compressed size is larger than the archive")));
    }
    let declared = file.size();
    read_entry(&mut file, declared, limit).map_err(|e| Error::decode("zip", format!("{entry}: {e}")))
}

/// The image entries of `archive` as virtual paths, in natural order of their full names
/// (so `ch2/01.jpg` comes before `ch10/01.jpg`). Entries whose names could point outside the
/// archive are left out.
pub fn list_images(archive: &Path) -> Result<Vec<PathBuf>> {
    let mut names: Vec<String> = match kind(archive) {
        Some(Kind::Zip) => {
            open_zip(archive)?.file_names().filter_map(|n| n.ok().and_then(|n| clean_name(&n))).collect()
        }
        Some(Kind::SevenZ) => seven_z_names(archive)?,
        None => return Err(Error::UnsupportedFormat),
    };
    names.retain(|n| is_image_entry(n));
    names.sort_by(|a, b| natural_cmp(a, b));
    names.dedup();
    Ok(names.iter().map(|n| entry_path(archive, n)).collect())
}

/// A [`Source`] for a real file or an archive entry (read into memory, at most
/// [`MAX_ENTRY_BYTES`]).
pub fn source_for(path: &Path) -> Result<Source> {
    let Some((archive, entry)) = split(path) else { return Ok(Source::from_path(path)) };
    let bytes: Arc<[u8]> = match kind(&archive) {
        Some(Kind::Zip) => zip_read(&archive, &entry, MAX_ENTRY_BYTES)?.into(),
        Some(Kind::SevenZ) => seven_z_read(&archive, &entry, MAX_ENTRY_BYTES)?,
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

    fn page_names(archive: &Path) -> Vec<String> {
        list_images(archive)
            .unwrap()
            .iter()
            .map(|p| p.strip_prefix(archive).unwrap().to_string_lossy().replace('\\', "/"))
            .collect()
    }

    #[test]
    fn backslash_names_open_and_escaping_names_are_left_out() {
        let dir = tempfile::tempdir().unwrap();
        // a real file next to the archive that a `../` entry would otherwise reach
        std::fs::write(dir.path().join("outside.png"), b"outside").unwrap();
        let cbz = dir.path().join("book.cbz");
        make_zip(
            &cbz,
            &[
                ("ch1\\01.png", b"one"),
                ("../outside.png", b"evil"),
                ("ch1/../../x.png", b"evil"),
                ("/abs.png", b"evil"),
                ("C:/Windows/x.png", b"evil"),
                ("a:stream.png", b"evil"),
                ("ch1/./02.png", b"evil"),
                ("ch1//03.png", b"evil"),
            ],
        );
        assert_eq!(page_names(&cbz), ["ch1/01.png"]);
        let page = &list_images(&cbz).unwrap()[0];
        assert_eq!(&*source_for(page).unwrap().read_all().unwrap(), b"one");
    }

    #[test]
    fn clean_names() {
        assert_eq!(clean_name("a\\b\\c.jpg").as_deref(), Some("a/b/c.jpg"));
        assert_eq!(clean_name("dir/").as_deref(), Some("dir/"));
        for bad in ["..\\x.jpg", "a/../x.jpg", "\\\\host\\share\\x.jpg", "D:x.jpg", ".", "", "a/./b.jpg"] {
            assert_eq!(clean_name(bad), None, "{bad}");
        }
    }

    /// Byte offsets of every `sig` in `bytes`.
    fn find_all(bytes: &[u8], sig: u32) -> Vec<usize> {
        let sig = sig.to_le_bytes();
        (0..bytes.len().saturating_sub(4)).filter(|&i| bytes[i..i + 4] == sig).collect()
    }

    #[test]
    fn declared_sizes_past_the_limit_are_refused_before_allocating() {
        let dir = tempfile::tempdir().unwrap();
        let cbz = dir.path().join("lie.cbz");
        let mut w = zip::ZipWriter::new(File::create(&cbz).unwrap());
        let stored = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Stored);
        w.start_file("p.png", stored).unwrap();
        w.write_all(b"abc").unwrap();
        w.finish().unwrap();
        // claim almost 4 GiB uncompressed in the local and the central header
        let mut bytes = std::fs::read(&cbz).unwrap();
        let huge = 0xFFFF_FFF0u32.to_le_bytes();
        for at in find_all(&bytes, 0x0403_4b50) {
            bytes[at + 22..at + 26].copy_from_slice(&huge);
        }
        for at in find_all(&bytes, 0x0201_4b50) {
            bytes[at + 24..at + 28].copy_from_slice(&huge);
        }
        std::fs::write(&cbz, bytes).unwrap();
        let page = entry_path(&cbz, "p.png");
        let err = source_for(&page).expect_err("a 4 GiB claim is refused").to_string();
        assert!(err.contains("limit"), "{err}");
    }

    #[test]
    fn decompression_is_cut_off_at_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let cbz = dir.path().join("bomb.cbz");
        // 4 MiB of zeros deflate to a few kilobytes
        make_zip(&cbz, &[("p.png", &vec![0u8; 4 << 20])]);
        assert!(std::fs::metadata(&cbz).unwrap().len() < 64 * 1024);
        let err = zip_read(&cbz, "p.png", 1 << 20).expect_err("over the limit").to_string();
        assert!(err.contains("limit"), "{err}");
        assert_eq!(zip_read(&cbz, "p.png", 4 << 20).unwrap().len(), 4 << 20, "exactly at the limit is fine");

        let mut short: &[u8] = &[1, 2, 3];
        assert!(read_entry(&mut short, 3, 3).is_ok());
        let mut long: &[u8] = &[1, 2, 3, 4];
        assert!(read_entry(&mut long, 1, 3).is_err(), "a header that understates the size is no way around it");
    }
}

#[cfg(test)]
mod seven_z_tests {
    use super::*;

    /// `testdata/archive/book.cb7` holds p10.png ("ten"), p2.png ("two") and readme.txt ("no"),
    /// written by `sevenz_rust2::ArchiveWriter`. It is committed rather than written by the test so
    /// the 7z writer (the crate's `compress` feature) stays out of every build: a test-only
    /// feature would compile this crate and everything above it twice.
    #[test]
    fn seven_z_pages_list_and_read() {
        let cb7 = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/archive/book.cb7");
        assert!(is_archive(&cb7));
        let pages = list_images(&cb7).unwrap();
        let names: Vec<_> = pages.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["p2.png", "p10.png"]);
        assert_eq!(&*source_for(&pages[1]).unwrap().read_all().unwrap(), b"ten");
        assert!(source_for(&cb7.join("missing.png")).is_err());
    }
}
