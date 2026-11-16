//! Images inside ZIP / CBZ and 7z / CB7 archives, addressed by virtual paths: the archive's path followed by
//! the entry name, e.g. `C:\comics\vol1.cbz\pages/001.jpg`. Everything keyed by path (cache,
//! prefetch, navigation, Save As) works on entries unchanged; only reading goes through here.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock};

use iv_core::{CancelToken, Error, FormatId, Result, Source};

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

/// Every archive extension this module opens (lowercase, without the dot).
pub fn extensions() -> impl Iterator<Item = &'static str> {
    ZIP.iter().chain(SEVEN_Z).copied()
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
/// block every entry can only be reached by decompressing everything before it in that block,
/// so reading pages one by one would cost O(pages²); instead each pass keeps every image it
/// walks past (measured on a 4-page solid 7z: 2.0 s for all pages without this). Entries are
/// shared with the sources made from them, never copied. Over [`SEVEN_Z_CACHE_BYTES`] the
/// least recently used entries go first.
#[derive(Default)]
struct SevenZCache {
    archive: Option<ArchiveId>,
    /// Name → (bytes, last use).
    entries: HashMap<String, (Arc<[u8]>, u64)>,
    bytes: usize,
    tick: u64,
}

impl SevenZCache {
    /// Empties the cache unless it holds entries of `id`.
    fn select(&mut self, id: &ArchiveId) {
        if self.archive.as_ref() != Some(id) {
            *self = SevenZCache { archive: Some(id.clone()), ..Default::default() };
        }
    }

    fn get(&mut self, name: &str) -> Option<Arc<[u8]>> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(name).map(|(bytes, used)| {
            *used = tick;
            bytes.clone()
        })
    }

    fn insert(&mut self, name: String, bytes: Arc<[u8]>) {
        if bytes.len() > SEVEN_Z_CACHE_BYTES {
            return;
        }
        if let Some((old, _)) = self.entries.remove(&name) {
            self.bytes -= old.len();
        }
        while self.bytes + bytes.len() > SEVEN_Z_CACHE_BYTES {
            let Some(oldest) = self.entries.iter().min_by_key(|(_, (_, used))| *used).map(|(n, _)| n.clone()) else {
                break;
            };
            if let Some((old, _)) = self.entries.remove(&oldest) {
                self.bytes -= old.len();
            }
        }
        self.tick += 1;
        self.bytes += bytes.len();
        self.entries.insert(name, (bytes, self.tick));
    }
}

/// Upper bound for cached encoded entries.
const SEVEN_Z_CACHE_BYTES: usize = 128 * 1024 * 1024;
/// Image entries of the same solid block decompressed past the requested one.
const SEVEN_Z_READ_AHEAD: usize = 6;

fn seven_z_cache() -> MutexGuard<'static, SevenZCache> {
    static CACHE: OnceLock<Mutex<SevenZCache>> = OnceLock::new();
    CACHE.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner())
}

/// Held while a 7z block is decompressed: the foreground and prefetch workers asking for pages
/// of the same block wait for the one pass and then find their page in the cache, instead of
/// each decompressing the block from its start at the same time.
fn seven_z_walk() -> MutexGuard<'static, ()> {
    static WALK: Mutex<()> = Mutex::new(());
    WALK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Frees the encoded 7z pages kept for quick page turns, e.g. once the viewer has left the
/// archive. Reading an entry again just decompresses it again.
pub fn release_cache() {
    *seven_z_cache() = SevenZCache::default();
}

/// Largest 7z header (the file list), in KiB, read and decompressed. The header is the archive's
/// own claim and a few kilobytes of LZMA can expand to gigabytes before it is parsed, so it is
/// held to this. Parsing it then needs at most about 100 bytes per header byte (the crate bounds
/// every count by the header's length), so a hostile header costs some 1.6 GB at worst, briefly;
/// real ones take 50 to 100 bytes per file, so this still lists 150 000 files or more. It also
/// bounds the dictionary used to decompress the header (16 MiB; 7-Zip uses 1 MiB, the
/// `sevenz-rust2` writer 8 MiB).
const SEVEN_Z_HEADER_KIB: usize = 16 * 1024;

/// Largest dictionary, in KiB, a 7z block may ask for: as large as the largest entry read
/// ([`MAX_ENTRY_BYTES`]) plus the decoder's own state. A block declaring more is refused before
/// anything is decompressed (7-Zip's ultra preset uses 64 MiB).
const SEVEN_Z_DICTIONARY_KIB: usize = (MAX_ENTRY_BYTES / 1024) as usize + 1024;

/// A 7z error as text, the memory limits worded for people.
fn seven_z_error(e: sevenz_rust2::Error) -> String {
    match e {
        sevenz_rust2::Error::MaxMemLimited { max_kb, actaul_kb } => {
            format!("needs {actaul_kb} KiB of memory, over the {max_kb} KiB limit")
        }
        e => e.to_string(),
    }
}

/// The index (header) of a 7z archive, read within [`SEVEN_Z_HEADER_KIB`].
fn seven_z_index(file: &mut BufReader<File>) -> std::result::Result<sevenz_rust2::Archive, sevenz_rust2::Error> {
    sevenz_rust2::Archive::read_with_memory_limit(file, &sevenz_rust2::Password::empty(), SEVEN_Z_HEADER_KIB)
}

fn cached_7z(id: &ArchiveId, entry: &str) -> Option<Arc<[u8]>> {
    let mut cache = seven_z_cache();
    cache.select(id);
    cache.get(entry)
}

/// One entry of a 7z archive. Only the compression block holding it is decompressed: in a
/// non-solid archive that is the entry alone; in a solid block, every entry before it (kept,
/// see [`SevenZCache`]) and up to [`SEVEN_Z_READ_AHEAD`] images after it. Stops early, with
/// [`Error::Cancelled`], once `cancel` is set.
fn seven_z_read(archive: &Path, entry: &str, limit: u64, cancel: &CancelToken) -> Result<Arc<[u8]>> {
    let id = ArchiveId::of(archive)?;
    if let Some(hit) = cached_7z(&id, entry) {
        return Ok(hit);
    }
    let _walk = seven_z_walk();
    // another worker may have decompressed it while this one waited
    if let Some(hit) = cached_7z(&id, entry) {
        return Ok(hit);
    }
    cancel.check()?;
    let fail = |e: sevenz_rust2::Error| Error::decode("7z", format!("{entry}: {}", seven_z_error(e)));
    let password = sevenz_rust2::Password::empty();
    let mut file = BufReader::new(File::open(archive)?);
    let index = seven_z_index(&mut file).map_err(fail)?;
    let missing = || Error::decode("7z", format!("{entry}: not in the archive"));
    let target = index
        .files
        .iter()
        .position(|f| f.has_stream && !f.is_directory() && clean_name(f.name()).as_deref() == Some(entry))
        .ok_or_else(missing)?;
    let block = index.stream_map.file_block_index[target].ok_or_else(missing)?;
    let target = &index.files[target];

    let mut found = None;
    let mut after_target = 0;
    let mut cancelled = false;
    // One thread: the multithreaded LZMA2 reader decompresses each run of chunks between
    // dictionary resets whole into memory, on several threads at once and ahead of what is read,
    // which no entry limit bounds (one run can be the whole block). Read on one thread, memory
    // stays at the dictionary, which is held to SEVEN_Z_DICTIONARY_KIB.
    sevenz_rust2::BlockDecoder::new_with_memory_limit(1, SEVEN_Z_DICTIONARY_KIB, block, &index, &password, &mut file)
        .for_each_entries(&mut |e, data| {
            if cancel.is_cancelled() {
                cancelled = true;
                return Ok(false);
            }
            let is_target = std::ptr::eq(e, target);
            let Some(name) = clean_name(e.name()) else { return Ok(true) };
            if !is_target && (e.is_directory() || !is_image_entry(&name)) {
                return Ok(true);
            }
            // pages kept from an earlier pass are not copied again (the decoder skips their data)
            let kept = if is_target { None } else { cached_7z(&id, &name) };
            let bytes = match kept {
                Some(bytes) => bytes,
                None => {
                    let bytes: Arc<[u8]> = match read_entry(data, e.size(), limit) {
                        Ok(bytes) => bytes.into(),
                        Err(err) if is_target => return Err(std::io::Error::other(format!("{name}: {err}")).into()),
                        // an oversized page passed on the way is not kept, and does not stop
                        // the walk to the page that was asked for (the decoder skips its data)
                        Err(_) => return Ok(true),
                    };
                    let mut cache = seven_z_cache();
                    cache.select(&id);
                    cache.insert(name, bytes.clone());
                    bytes
                }
            };
            if is_target {
                found = Some(bytes);
            } else if found.is_some() {
                after_target += 1;
            }
            // go a little past the target (the next pages are about to be asked for), then stop:
            // the rest of the block is not decompressed
            Ok(found.is_none() || after_target < SEVEN_Z_READ_AHEAD)
        })
        .map_err(fail)?;
    match found {
        Some(bytes) => Ok(bytes),
        None if cancelled => Err(Error::Cancelled),
        None => Err(missing()),
    }
}

/// Entry names of a 7z archive with `/` separators, unsafe names left out. Directories skipped.
fn seven_z_names(archive: &Path) -> Result<Vec<String>> {
    let index =
        seven_z_index(&mut BufReader::new(File::open(archive)?)).map_err(|e| Error::decode("7z", seven_z_error(e)))?;
    Ok(index.files.iter().filter(|f| !f.is_directory() && f.has_stream).filter_map(|f| clean_name(f.name())).collect())
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
    source_for_cancellable(path, &CancelToken::never())
}

/// [`source_for`], giving up with [`Error::Cancelled`] once `cancel` is set while a 7z block is
/// being decompressed (the decoding pipeline's page the user has already left).
pub fn source_for_cancellable(path: &Path, cancel: &CancelToken) -> Result<Source> {
    let Some((archive, entry)) = split(path) else { return Ok(Source::from_path(path)) };
    let bytes: Arc<[u8]> = match kind(&archive) {
        Some(Kind::Zip) => zip_read(&archive, &entry, MAX_ENTRY_BYTES)?.into(),
        Some(Kind::SevenZ) => seven_z_read(&archive, &entry, MAX_ENTRY_BYTES, cancel)?,
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

    fn fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../testdata/archive").join(name)
    }

    /// The 7z cache is one per process: these tests take turns with it.
    fn exclusive() -> MutexGuard<'static, ()> {
        static TURN: Mutex<()> = Mutex::new(());
        let turn = TURN.lock().unwrap_or_else(|e| e.into_inner());
        release_cache();
        turn
    }

    fn cached_names() -> Vec<String> {
        let cache = seven_z_cache();
        let mut names: Vec<String> = cache.entries.keys().cloned().collect();
        names.sort();
        assert_eq!(cache.bytes, cache.entries.values().map(|(b, _)| b.len()).sum::<usize>(), "byte count");
        names
    }

    fn page(archive: &Path, n: u32) -> PathBuf {
        archive.join(format!("p{n:02}.png"))
    }

    /// `testdata/archive/book.cb7` holds p10.png ("ten"), p2.png ("two") and readme.txt ("no"),
    /// written by `sevenz_rust2::ArchiveWriter`. It and the others here are committed rather
    /// than written by the test so the 7z writer (the crate's `compress` feature) stays out of
    /// every build: a test-only feature would compile this crate and everything above it twice.
    #[test]
    fn seven_z_pages_list_and_read() {
        let _turn = exclusive();
        let cb7 = fixture("book.cb7");
        assert!(is_archive(&cb7));
        let pages = list_images(&cb7).unwrap();
        let names: Vec<_> = pages.iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
        assert_eq!(names, ["p2.png", "p10.png"]);
        assert_eq!(&*source_for(&pages[1]).unwrap().read_all().unwrap(), b"ten");
        assert!(source_for(&cb7.join("missing.png")).is_err());
    }

    /// `pages.cb7`: p01.png … p10.png ("page 01" …), one compression block each (7-Zip, -ms=off).
    #[test]
    fn a_non_solid_archive_decompresses_only_the_page_asked_for() {
        let _turn = exclusive();
        let cb7 = fixture("pages.cb7");
        assert_eq!(&*source_for(&page(&cb7, 3)).unwrap().read_all().unwrap(), b"page 03");
        assert_eq!(cached_names(), ["p03.png"], "no other block was decompressed");
        assert_eq!(&*source_for(&page(&cb7, 10)).unwrap().read_all().unwrap(), b"page 10");
        assert_eq!(cached_names(), ["p03.png", "p10.png"]);
    }

    /// `solid.cb7`: the same ten pages in one solid block (7-Zip, -ms=on).
    #[test]
    fn a_solid_block_is_read_up_to_a_few_pages_past_the_one_asked_for() {
        let _turn = exclusive();
        let cb7 = fixture("solid.cb7");
        assert_eq!(&*source_for(&page(&cb7, 2)).unwrap().read_all().unwrap(), b"page 02");
        let want: Vec<String> = (1..=2 + SEVEN_Z_READ_AHEAD as u32).map(|n| format!("p{n:02}.png")).collect();
        assert_eq!(cached_names(), want, "the pages before it and the read-ahead, not the rest of the block");
        // a page in the read-ahead is a hit; the next pass keeps going from where it is needed
        assert_eq!(&*source_for(&page(&cb7, 5)).unwrap().read_all().unwrap(), b"page 05");
        assert_eq!(&*source_for(&page(&cb7, 10)).unwrap().read_all().unwrap(), b"page 10");
        assert_eq!(cached_names().len(), 10);
        // reading it all again finds everything kept, counted once
        for n in 1..=10 {
            assert_eq!(&*source_for(&page(&cb7, n)).unwrap().read_all().unwrap(), format!("page {n:02}").as_bytes());
        }
        assert_eq!(cached_names().len(), 10);
    }

    #[test]
    fn a_cancelled_read_stops_and_says_so() {
        let _turn = exclusive();
        let cb7 = fixture("solid.cb7");
        let cancel = CancelToken::new();
        cancel.cancel();
        let err = source_for_cancellable(&page(&cb7, 9), &cancel).expect_err("cancelled");
        assert!(matches!(err, Error::Cancelled), "{err}");
        assert!(cached_names().is_empty(), "nothing decompressed");
    }

    /// `header-bomb.cb7`: 9.6 KB whose LZMA-compressed header (the stream of 64 MiB of zeros
    /// made by 7-Zip, `-m0=LZMA:d=1m`) declares and really decompresses to 64 MiB. Built by hand:
    /// start header, that packed stream, and an encoded header naming one LZMA folder of
    /// 64 MiB. Unbounded, opening it would decompress all of that before parsing a byte.
    #[test]
    fn a_7z_header_that_decompresses_past_the_limit_is_refused() {
        let _turn = exclusive();
        let cb7 = fixture("header-bomb.cb7");
        let err = list_images(&cb7).expect_err("over the header limit").to_string();
        assert!(err.contains(&format!("over the {SEVEN_Z_HEADER_KIB} KiB limit")), "{err}");
        let err = source_for(&cb7.join("p.png")).expect_err("over the header limit").to_string();
        assert!(err.contains("limit"), "{err}");
    }

    /// `huge-dictionary.cb7`: two pages (7-Zip, LZMA2, `-mhc=off`) whose LZMA2 dictionary
    /// property was raised to 4 GiB, with the header checksums fixed. The data still decodes
    /// with a small dictionary, so only the limit can refuse it.
    #[test]
    fn a_7z_block_asking_for_a_dictionary_past_the_limit_is_refused() {
        let _turn = exclusive();
        let cb7 = fixture("huge-dictionary.cb7");
        assert_eq!(page_names(&cb7), ["p1.png", "p2.png"], "the header itself is fine");
        let err = source_for(&cb7.join("p1.png")).expect_err("over the dictionary limit").to_string();
        assert!(err.contains(&format!("over the {SEVEN_Z_DICTIONARY_KIB} KiB limit")), "{err}");
    }

    #[test]
    fn a_7z_header_at_the_limit_parses_into_a_bounded_index() {
        // every file of the index costs one entry per header byte at worst
        let worst = SEVEN_Z_HEADER_KIB as u64 * 1024 * std::mem::size_of::<sevenz_rust2::ArchiveEntry>() as u64;
        assert!(worst <= 1700 * 1000 * 1000, "{worst} bytes");
    }

    fn page_names(archive: &Path) -> Vec<String> {
        list_images(archive).unwrap().iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect()
    }

    #[test]
    fn the_cache_drops_the_least_recently_used_pages_first() {
        let mut cache = SevenZCache::default();
        let big = |n: usize| -> Arc<[u8]> { vec![0u8; n].into() };
        let third = SEVEN_Z_CACHE_BYTES / 3;
        cache.insert("a".into(), big(third));
        cache.insert("b".into(), big(third));
        cache.insert("c".into(), big(third));
        assert!(cache.get("a").is_some(), "a is now the most recently used");
        cache.insert("d".into(), big(third));
        let mut names: Vec<&str> = cache.entries.keys().map(String::as_str).collect();
        names.sort();
        assert_eq!(names, ["a", "c", "d"]);
        cache.insert("d".into(), big(10));
        assert_eq!(cache.bytes, 2 * third + 10, "a replaced entry is not counted twice");
    }
}
