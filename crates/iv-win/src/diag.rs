//! Diagnostics for a program without a console: release builds are started from Explorer, so
//! stderr goes nowhere. Messages go to a log file as well, and a crash leaves a report and a
//! message box instead of a window that silently disappears.
//!
//! Both files live in `%LOCALAPPDATA%\image-viewer` and neither grows from session to session:
//! the log is started afresh by the first session that finds it over [`LOG_LIMIT`] (and one
//! session writes at most [`SESSION_LIMIT`] into it), and `crash.log` holds only the last crash.

use std::backtrace::Backtrace;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::panic::PanicHookInfo;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use crate::i18n::{Language, Strings, fill};

pub const LOG_FILE: &str = "image-viewer.log";
pub const CRASH_FILE: &str = "crash.log";
/// A log at least this large is emptied by the next session that writes to it.
pub const LOG_LIMIT: u64 = 256 * 1024;
/// What one session may add to the log; later messages only go to stderr.
pub const SESSION_LIMIT: u64 = 128 * 1024;

/// The folder for the log and crash files: `%LOCALAPPDATA%\image-viewer`.
pub fn default_dir() -> Option<PathBuf> {
    std::env::var_os("LOCALAPPDATA").map(|d| PathBuf::from(d).join("image-viewer"))
}

/// A session's log file, opened on its first message.
pub struct Log {
    dir: Option<PathBuf>,
    file: Option<File>,
    opened: bool,
    written: u64,
    start: Instant,
}

impl Log {
    pub fn new(dir: Option<PathBuf>) -> Self {
        Self { dir, file: None, opened: false, written: 0, start: Instant::now() }
    }

    pub fn path(&self) -> Option<PathBuf> {
        self.dir.as_ref().map(|d| d.join(LOG_FILE))
    }

    /// Appends one line. Failing to write the log is never an error worth reporting.
    pub fn write(&mut self, message: &str) {
        if !self.opened {
            self.opened = true;
            self.file = self.open();
        }
        let Some(file) = &mut self.file else { return };
        let line = format!(
            "[pid {} +{:.3} s] {}\r\n",
            std::process::id(),
            self.start.elapsed().as_secs_f64(),
            message.replace('\n', "\r\n    ")
        );
        if self.written + line.len() as u64 > SESSION_LIMIT {
            let _ = file.write_all(b"[log limit reached for this session]\r\n");
            self.file = None;
            return;
        }
        self.written += line.len() as u64;
        let _ = file.write_all(line.as_bytes());
    }

    fn open(&self) -> Option<File> {
        let dir = self.dir.as_ref()?;
        std::fs::create_dir_all(dir).ok()?;
        let path = dir.join(LOG_FILE);
        // appending, not replacing: another viewer may be writing to the same log right now
        let too_big = std::fs::metadata(&path).is_ok_and(|m| m.len() >= LOG_LIMIT);
        let mut file = if too_big {
            OpenOptions::new().write(true).create(true).truncate(true).open(&path).ok()?
        } else {
            OpenOptions::new().append(true).create(true).open(&path).ok()?
        };
        let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let header = format!(
            "--- image-viewer {} (pid {}, unix time {since_epoch})\r\n",
            env!("CARGO_PKG_VERSION"),
            std::process::id()
        );
        file.write_all(header.as_bytes()).ok()?;
        Some(file)
    }
}

fn cell() -> &'static Mutex<Log> {
    static LOG: OnceLock<Mutex<Log>> = OnceLock::new();
    LOG.get_or_init(|| Mutex::new(Log::new(default_dir())))
}

fn global() -> MutexGuard<'static, Log> {
    cell().lock().unwrap_or_else(|e| e.into_inner())
}

/// For the panic hook: a panic raised while this thread holds the log must not wait for it.
fn try_global() -> Option<MutexGuard<'static, Log>> {
    match cell().try_lock() {
        Ok(g) => Some(g),
        Err(std::sync::TryLockError::Poisoned(e)) => Some(e.into_inner()),
        Err(std::sync::TryLockError::WouldBlock) => None,
    }
}

fn hook_log(message: &str) {
    if let Some(mut log) = try_global() {
        log.write(message);
    }
}

/// Sets the folder of the log and the crash report (before the first message).
pub fn init(dir: Option<PathBuf>) {
    *global() = Log::new(dir);
}

/// Reports `message` on stderr (seen with a console or `--trace`) and in the log file.
pub fn log(message: &str) {
    eprintln!("{message}");
    global().write(message);
}

/// The text of a crash report. `image` (see [`this_image`]) names the build and where it was
/// loaded, so the frame offsets in `backtrace` can be resolved against the release's `.pdb`.
pub fn crash_report(thread: &str, location: &str, message: &str, image: Option<&Image>, backtrace: &str) -> String {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let image = image.map_or_else(|| "unknown".to_string(), Image::to_string);
    format!(
        "image-viewer {} crashed\r\npid {}, unix time {since_epoch}\r\nimage: {image}\r\nthread '{thread}' panicked at {location}:\r\n{message}\r\n\r\nbacktrace (frame addresses; [module+offset] is relative to the image base):\r\n{}\r\n",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        backtrace.replace('\n', "\r\n")
    )
}

/// The `.pdb` a PE image was linked with (its CodeView `RSDS` record): the symbols file that
/// matches the image has the same file name, GUID and age.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pdb {
    /// File name only (rustc embeds no folder).
    pub name: String,
    pub guid: [u8; 16],
    pub age: u32,
}

impl std::fmt::Display for Pdb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let g = &self.guid;
        let data1 = u32::from_le_bytes([g[0], g[1], g[2], g[3]]);
        let data2 = u16::from_le_bytes([g[4], g[5]]);
        let data3 = u16::from_le_bytes([g[6], g[7]]);
        write!(f, "{} {{{data1:08X}-{data2:04X}-{data3:04X}-{:02X}{:02X}-", self.name, g[8], g[9])?;
        for b in &g[10..] {
            write!(f, "{b:02X}")?;
        }
        write!(f, "}} age {}", self.age)
    }
}

/// A loaded executable as a crash report names it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Image {
    /// The executable's file name, the module part of `[module+offset]`.
    pub name: String,
    /// Where it was loaded (it moves from run to run: ASLR).
    pub base: usize,
    /// Its `SizeOfImage`: addresses in `base..base + size` belong to it.
    pub size: usize,
    /// The PE header's `TimeDateStamp`.
    pub timestamp: u32,
    pub pdb: Option<Pdb>,
}

impl Image {
    /// `address` as an offset into this image, if it lies inside it.
    pub fn offset(&self, address: usize) -> Option<usize> {
        address.checked_sub(self.base).filter(|&o| o < self.size)
    }
}

impl std::fmt::Display for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} at {:#x}, size {:#x}, timestamp {:#010x}, ", self.name, self.base, self.size, self.timestamp)?;
        match &self.pdb {
            Some(pdb) => write!(f, "pdb {pdb}"),
            None => f.write_str("no pdb record"),
        }
    }
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at.checked_add(2)?)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at.checked_add(4)?)?.try_into().ok()?))
}

/// What the PE headers at the start of a loaded image say.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Headers {
    size: usize,
    timestamp: u32,
    /// The debug directory: RVA and size in bytes.
    debug: (usize, usize),
}

const IMAGE_DIRECTORY_ENTRY_DEBUG: usize = 6;
const IMAGE_DEBUG_TYPE_CODEVIEW: u32 = 2;
/// `IMAGE_DEBUG_DIRECTORY`
const DEBUG_ENTRY_SIZE: usize = 28;

fn pe_headers(image: &[u8]) -> Option<Headers> {
    if image.get(..2)? != b"MZ" {
        return None;
    }
    let pe = u32_at(image, 0x3c)? as usize;
    if image.get(pe..pe.checked_add(4)?)? != b"PE\0\0" {
        return None;
    }
    let file = pe + 4;
    let timestamp = u32_at(image, file + 4)?;
    let optional = file + 20;
    // PE32+ (64-bit) or PE32: the data directories start at a different offset
    let (count_at, directories) = match u16_at(image, optional)? {
        0x20b => (optional + 108, optional + 112),
        0x10b => (optional + 92, optional + 96),
        _ => return None,
    };
    let size = u32_at(image, optional + 56)? as usize;
    let debug = if u32_at(image, count_at)? as usize > IMAGE_DIRECTORY_ENTRY_DEBUG {
        let at = directories + IMAGE_DIRECTORY_ENTRY_DEBUG * 8;
        (u32_at(image, at)? as usize, u32_at(image, at + 4)? as usize)
    } else {
        (0, 0)
    };
    Some(Headers { size, timestamp, debug })
}

/// The CodeView record in a loaded image's debug directory (`debug` from [`pe_headers`]).
/// `read(rva, len)` gives that many bytes of the image, or `None` past its end: only the
/// directory and the record are borrowed, never the whole image.
fn codeview<'a>(read: impl Fn(usize, usize) -> Option<&'a [u8]>, (rva, len): (usize, usize)) -> Option<Pdb> {
    let directory = read(rva, len)?;
    directory.as_chunks::<DEBUG_ENTRY_SIZE>().0.iter().find_map(|entry| {
        if u32_at(entry, 12)? != IMAGE_DEBUG_TYPE_CODEVIEW {
            return None;
        }
        let (size, at) = (u32_at(entry, 16)? as usize, u32_at(entry, 20)? as usize);
        let data = read(at, size)?;
        if data.get(..4)? != b"RSDS" {
            return None;
        }
        let guid = data.get(4..20)?.try_into().ok()?;
        let age = u32_at(data, 20)?;
        let path = data.get(24..)?;
        let path = &path[..path.iter().position(|&b| b == 0).unwrap_or(path.len())];
        let path = String::from_utf8_lossy(path);
        let name = path.rsplit(['\\', '/']).next().unwrap_or_default().to_string();
        Some(Pdb { name, guid, age })
    })
}

/// The running executable: its name, load address, size and `.pdb` identity, read from its
/// own PE headers in memory (no Windows API needed).
#[cfg(windows)]
pub fn this_image() -> Option<Image> {
    unsafe extern "C" {
        /// The linker's symbol at the first byte of the image it links: the executable's base.
        static __ImageBase: u8;
    }
    // the image is memory the loader mapped, not a Rust object: address it with exposed provenance
    let base = std::ptr::with_exposed_provenance::<u8>((&raw const __ImageBase).expose_provenance());
    // SAFETY: the first page of a loaded image (its headers) is always mapped and readable
    let headers = pe_headers(unsafe { std::slice::from_raw_parts(base, 4096) })?;
    // only the ranges asked for are borrowed (the debug directory and its CodeView record): a
    // slice over the whole image would alias the writable sections other threads may be writing
    let read = |at: usize, len: usize| {
        let end = at.checked_add(len)?;
        // SAFETY: the whole of a loaded image (SizeOfImage bytes from its base) is mapped, and
        // these two ranges are read-only data that nothing writes
        (end <= headers.size).then(|| unsafe { std::slice::from_raw_parts(base.add(at), len) })
    };
    let name = std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "image-viewer.exe".to_string());
    Some(Image {
        name,
        base: base as usize,
        size: headers.size,
        timestamp: headers.timestamp,
        pdb: codeview(read, headers.debug),
    })
}

#[cfg(not(windows))]
pub fn this_image() -> Option<Image> {
    None
}

/// The address of a frame line of a full-format backtrace (`  3:     0x7ff6… - name`).
fn frame_address(line: &str) -> Option<usize> {
    let (index, rest) = line.trim_start().split_once(':')?;
    if index.is_empty() || !index.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let hex = rest.trim_start().strip_prefix("0x")?;
    let end = hex.find(|c: char| !c.is_ascii_hexdigit()).unwrap_or(hex.len());
    usize::from_str_radix(&hex[..end], 16).ok()
}

/// Adds `[name+offset]` to each frame of `backtrace` (full format) inside `image`. Without the
/// `.pdb` next to the executable (a released build) those frames have no name, and the offset is
/// what the separate symbols file resolves.
pub fn annotate_frames(backtrace: &str, image: &Image) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(backtrace.len() + backtrace.len() / 4);
    for line in backtrace.lines() {
        out.push_str(line);
        if let Some(offset) = frame_address(line).and_then(|a| image.offset(a)) {
            let _ = write!(out, "  [{}+{offset:#x}]", image.name);
        }
        out.push('\n');
    }
    out
}

/// The current thread's backtrace in full format (every frame keeps its address, also the ones
/// without symbols), with the frames of this executable as offsets from its base.
pub fn backtrace(image: Option<&Image>) -> String {
    let full = format!("{:#}", Backtrace::force_capture());
    match image {
        Some(image) => annotate_frames(&full, image),
        None => full,
    }
}

/// The text of the crash message box: where the report was written, or why it was not.
pub fn crash_box_text(
    t: &Strings,
    written: Option<&std::io::Result<PathBuf>>,
    message: &str,
    location: &str,
) -> String {
    let error = match written {
        Some(Ok(path)) => return fill(t.crash_message, &[("path", &path.display().to_string())]),
        Some(Err(e)) => e.to_string(),
        None => t.crash_no_folder.to_string(),
    };
    fill(t.crash_unsaved, &[("error", &error), ("detail", &format!("{message}\n({location})"))])
}

/// Replaces `crash.log` in `dir` with `report`. Returns its path.
pub fn write_crash(dir: &Path, report: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let path = dir.join(CRASH_FILE);
    std::fs::write(&path, report)?;
    Ok(path)
}

fn append_crash(dir: &Path, text: &str) {
    if let Ok(mut f) = OpenOptions::new().append(true).open(dir.join(CRASH_FILE)) {
        let _ = f.write_all(text.as_bytes());
    }
}

/// Installs the panic hook. The thread calling this is the window's thread: a panic there
/// cannot be survived (it unwinds into a window procedure, which aborts the process), so it
/// writes `crash.log` and calls `show(title, text)` to tell the user before the process ends.
/// A panic contained by [`iv_core::panic::catch`] (a decoder crashing on a malformed file) and
/// one on a helper thread are only logged: the viewer carries on.
///
/// `show` runs on the crashing thread; it should not pump that thread's messages (the window's
/// state is mid-panic), e.g. show its message box from a thread of its own.
pub fn install_panic_hook(language: Language, show: impl Fn(&str, &str) + Send + Sync + 'static) {
    let ui_thread = std::thread::current().id();
    let crashed = AtomicBool::new(false);
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info: &PanicHookInfo<'_>| {
        default(info);
        let message = iv_core::panic::message(info.payload());
        let location = info.location().map_or_else(|| "unknown location".to_string(), |l| l.to_string());
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed").to_string();
        if iv_core::panic::is_caught() {
            hook_log(&format!("contained panic on thread '{name}' at {location}: {message}"));
            return;
        }
        let image = this_image();
        let backtrace = backtrace(image.as_ref());
        if thread.id() != ui_thread {
            let image = image.as_ref().map_or_else(|| "unknown".to_string(), Image::to_string);
            hook_log(&format!("thread '{name}' panicked at {location}: {message}\nimage: {image}\n{backtrace}"));
            return;
        }
        let dir = try_global().and_then(|l| l.dir.clone()).or_else(default_dir);
        if crashed.swap(true, Ordering::AcqRel) {
            // the second report of the same crash: the panic reached a window procedure
            if let Some(dir) = &dir {
                append_crash(dir, &format!("\r\nthen at {location}: {message}\r\n"));
            }
            return;
        }
        let report = crash_report(&name, &location, &message, image.as_ref(), &backtrace);
        let written = dir.as_deref().map(|d| write_crash(d, &report));
        hook_log(&format!("crash: thread '{name}' panicked at {location}: {message}"));
        if let Some(Err(e)) = &written {
            hook_log(&format!("crash report not written: {e}"));
        }
        let t = language.strings();
        show(t.crash_title, &crash_box_text(t, written.as_ref(), &message, &location));
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reads ranges of an image held in a buffer, the way `this_image` reads the loaded one.
    fn within<'a>(image: &'a [u8]) -> impl Fn(usize, usize) -> Option<&'a [u8]> {
        move |at, len| image.get(at..at.checked_add(len)?)
    }

    #[test]
    fn the_log_is_appended_to_and_never_grows_past_its_limits() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(LOG_FILE);
        let mut log = Log::new(Some(dir.path().to_path_buf()));
        log.write("first session\nsecond line");
        drop(log);
        let mut log = Log::new(Some(dir.path().to_path_buf()));
        log.write("second session");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("first session\r\n    second line") && text.contains("second session"), "{text}");
        assert_eq!(text.matches("--- image-viewer").count(), 2);

        // one session cannot write more than its share
        for i in 0..100_000 {
            log.write(&format!("message {i}"));
        }
        drop(log);
        let len = std::fs::metadata(&path).unwrap().len();
        assert!(len <= SESSION_LIMIT + 4096, "{len}");

        // a log over the limit is started afresh
        std::fs::write(&path, vec![b'x'; LOG_LIMIT as usize]).unwrap();
        let mut log = Log::new(Some(dir.path().to_path_buf()));
        log.write("fresh");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("xxxx"), "old content dropped: {}", text.len());
        assert!(text.contains("fresh"));
    }

    #[test]
    fn no_folder_means_no_file_and_no_error() {
        let mut log = Log::new(None);
        log.write("nowhere");
        assert!(log.path().is_none());
    }

    #[test]
    fn crash_reports_replace_the_previous_one() {
        let dir = tempfile::tempdir().unwrap();
        let first = write_crash(dir.path(), &crash_report("main", "a.rs:1:1", "first", None, "bt")).unwrap();
        let second = write_crash(dir.path(), &crash_report("main", "b.rs:2:2", "second", None, "bt")).unwrap();
        assert_eq!(first, second);
        let text = std::fs::read_to_string(second).unwrap();
        assert!(text.contains("second") && text.contains("b.rs:2:2") && !text.contains("first"), "{text}");
        assert!(text.contains("image: unknown"), "{text}");
    }

    /// A minimal PE32+ image as the loader maps it: headers, then a debug directory with a
    /// non-CodeView entry before the CodeView one.
    fn synthetic_image(path: &[u8]) -> Vec<u8> {
        let mut image = vec![0u8; 0x2000];
        let put = |image: &mut Vec<u8>, at: usize, bytes: &[u8]| image[at..at + bytes.len()].copy_from_slice(bytes);
        put(&mut image, 0, b"MZ");
        put(&mut image, 0x3c, &0x80u32.to_le_bytes());
        put(&mut image, 0x80, b"PE\0\0");
        put(&mut image, 0x84 + 4, &0x6543_21abu32.to_le_bytes()); // TimeDateStamp
        let optional = 0x84 + 20;
        put(&mut image, optional, &0x20bu16.to_le_bytes());
        put(&mut image, optional + 56, &0x2000u32.to_le_bytes()); // SizeOfImage
        put(&mut image, optional + 108, &16u32.to_le_bytes()); // NumberOfRvaAndSizes
        put(&mut image, optional + 112 + 6 * 8, &0x1000u32.to_le_bytes());
        put(&mut image, optional + 112 + 6 * 8 + 4, &(2 * 28u32).to_le_bytes());
        // entry 0: IMAGE_DEBUG_TYPE_POGO, ignored
        put(&mut image, 0x1000 + 12, &13u32.to_le_bytes());
        // entry 1: CodeView at 0x1100
        let record_len = 24 + path.len() as u32 + 1;
        put(&mut image, 0x1000 + 28 + 12, &2u32.to_le_bytes());
        put(&mut image, 0x1000 + 28 + 16, &record_len.to_le_bytes());
        put(&mut image, 0x1000 + 28 + 20, &0x1100u32.to_le_bytes());
        put(&mut image, 0x1100, b"RSDS");
        put(&mut image, 0x1104, &[0x10, 0x32, 0x54, 0x76, 0x98, 0xba, 0xdc, 0xfe, 1, 2, 3, 4, 5, 6, 7, 8]);
        put(&mut image, 0x1114, &3u32.to_le_bytes());
        put(&mut image, 0x1118, path);
        image
    }

    #[test]
    fn the_pdb_identity_is_read_from_the_pe_headers() {
        let image = synthetic_image(br"C:\build\image_viewer.pdb");
        let headers = pe_headers(&image).unwrap();
        assert_eq!(headers, Headers { size: 0x2000, timestamp: 0x6543_21ab, debug: (0x1000, 56) });
        let pdb = codeview(within(&image), headers.debug).unwrap();
        assert_eq!(pdb.name, "image_viewer.pdb");
        assert_eq!(pdb.age, 3);
        assert_eq!(pdb.to_string(), "image_viewer.pdb {76543210-BA98-FEDC-0102-030405060708} age 3");

        let shown = Image {
            name: "image-viewer.exe".into(),
            base: 0x7ff6_0000_0000,
            size: 0x2000,
            timestamp: 0x6543_21ab,
            pdb: Some(pdb),
        };
        assert_eq!(
            shown.to_string(),
            "image-viewer.exe at 0x7ff600000000, size 0x2000, timestamp 0x654321ab, pdb image_viewer.pdb {76543210-BA98-FEDC-0102-030405060708} age 3"
        );
    }

    #[test]
    fn broken_headers_give_nothing_instead_of_panicking() {
        let image = synthetic_image(b"x.pdb");
        assert_eq!(pe_headers(&image[..0x90]), None, "cut inside the optional header");
        let mut far = image.clone();
        far[0x3c..0x40].copy_from_slice(&0xffff_fff0u32.to_le_bytes());
        assert_eq!(pe_headers(&far), None);
        let mut not_pe = image.clone();
        not_pe[0] = b'X';
        assert_eq!(pe_headers(&not_pe), None);
        // a debug directory or CodeView record pointing past the image
        let headers = pe_headers(&image).unwrap();
        assert_eq!(codeview(within(&image), (0x1ff0, 56)), None);
        assert_eq!(codeview(within(&image[..0x1110]), headers.debug), None);
        assert_eq!(codeview(within(&image), (usize::MAX - 4, 56)), None);
        // no terminating zero: the name runs to the end of the record
        let mut unterminated = image.clone();
        unterminated[0x1118 + 5] = b'!';
        assert_eq!(codeview(within(&unterminated), headers.debug).unwrap().name, "x.pdb!");
    }

    #[test]
    fn frame_addresses_become_offsets_into_the_image() {
        let image =
            Image { name: "image-viewer.exe".into(), base: 0x7ff6_a000_0000, size: 0x10_0000, timestamp: 0, pdb: None };
        let full = "   0:     0x7ff6a0012345 - <unknown>\n   1:     0x7ffd11223344 - BaseThreadInitThunk\n                               at C:\\x.rs:1:2\n  12: 0x7ff6a00fffff - main\n";
        assert_eq!(
            annotate_frames(full, &image),
            "   0:     0x7ff6a0012345 - <unknown>  [image-viewer.exe+0x12345]\n   1:     0x7ffd11223344 - BaseThreadInitThunk\n                               at C:\\x.rs:1:2\n  12: 0x7ff6a00fffff - main  [image-viewer.exe+0xfffff]\n"
        );
        assert_eq!(frame_address("   3:     0x00007ff6a0012345 - f"), Some(0x7ff6_a001_2345));
        assert_eq!(frame_address("   at src/x.rs:3:4"), None);
        assert_eq!(frame_address("note: 0x1234"), None);
        assert_eq!(image.offset(0x7ff6_a010_0000), None, "one past the end");
        assert_eq!(image.offset(0x7ff6_9fff_ffff), None);
    }

    #[cfg(windows)]
    #[test]
    fn this_executable_is_found_with_its_pdb() {
        let image = this_image().unwrap();
        let exe = std::env::current_exe().unwrap();
        assert_eq!(image.name, exe.file_name().unwrap().to_string_lossy());
        // a function of this executable lies inside it
        let here = this_image as fn() -> Option<Image> as usize;
        assert!(image.offset(here).is_some(), "{here:#x} in {image}");
        // the identity read from memory is the one in the file on disk
        let pdb = image.pdb.as_ref().unwrap();
        assert!(pdb.name.ends_with(".pdb"), "{pdb}");
        let file = std::fs::read(&exe).unwrap();
        let mut record = b"RSDS".to_vec();
        record.extend_from_slice(&pdb.guid);
        record.extend_from_slice(&pdb.age.to_le_bytes());
        assert!(file.windows(record.len()).any(|w| w == record), "{pdb} not in {}", exe.display());

        // and a real backtrace gets offsets into it
        let text = backtrace(Some(&image));
        assert!(text.contains(&format!("  [{}+0x", image.name)), "{text}");
    }

    #[test]
    fn the_crash_box_says_a_report_was_written_only_when_it_was() {
        let t = Language::English.strings();
        let written: std::io::Result<PathBuf> = Ok(PathBuf::from(r"C:\logs\crash.log"));
        let text = crash_box_text(t, Some(&written), "boom", "a.rs:1:2");
        assert!(text.contains("written to") && text.contains(r"C:\logs\crash.log"), "{text}");

        let failed: std::io::Result<PathBuf> = Err(std::io::Error::other("disk full"));
        let text = crash_box_text(t, Some(&failed), "boom", "a.rs:1:2");
        assert!(!text.contains("written to") && text.contains("could not be saved: disk full"), "{text}");
        assert!(text.contains("boom\n(a.rs:1:2)"), "{text}");

        let text = crash_box_text(Language::Korean.strings(), None, "boom", "a.rs:1:2");
        assert!(
            text.contains("저장하지 못했습니다") && text.contains("%LOCALAPPDATA%") && text.contains("boom"),
            "{text}"
        );
        assert!(!text.contains("남겼습니다"), "{text}");
    }
}
