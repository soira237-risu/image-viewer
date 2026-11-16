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

use crate::i18n::{Language, fill};

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

/// The text of a crash report.
pub fn crash_report(thread: &str, location: &str, message: &str, backtrace: &str) -> String {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    format!(
        "image-viewer {} crashed\r\npid {}, unix time {since_epoch}\r\nthread '{thread}' panicked at {location}:\r\n{message}\r\n\r\nbacktrace:\r\n{}\r\n",
        env!("CARGO_PKG_VERSION"),
        std::process::id(),
        backtrace.replace('\n', "\r\n")
    )
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
        let backtrace = Backtrace::force_capture().to_string();
        if thread.id() != ui_thread {
            hook_log(&format!("thread '{name}' panicked at {location}: {message}\n{backtrace}"));
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
        let report = crash_report(&name, &location, &message, &backtrace);
        let written = dir.as_deref().map(|d| write_crash(d, &report));
        hook_log(&format!("crash: thread '{name}' panicked at {location}: {message}"));
        let t = language.strings();
        let place = match &written {
            Some(Ok(path)) => path.display().to_string(),
            _ => format!("({message} at {location})"),
        };
        show(t.crash_title, &fill(t.crash_message, &[("path", &place)]));
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let first = write_crash(dir.path(), &crash_report("main", "a.rs:1:1", "first", "bt")).unwrap();
        let second = write_crash(dir.path(), &crash_report("main", "b.rs:2:2", "second", "bt")).unwrap();
        assert_eq!(first, second);
        let text = std::fs::read_to_string(second).unwrap();
        assert!(text.contains("second") && text.contains("b.rs:2:2") && !text.contains("first"), "{text}");
    }
}
