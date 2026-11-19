//! Listing the viewer in Windows' "Open with" menu and in Settings > Default apps, for the
//! current user only (`HKEY_CURRENT_USER`: no administrator rights, nothing machine-wide).
//!
//! This module decides which registry values that takes and what undoing it removes; the
//! Windows side (`win::shell`) only writes and deletes them. A program cannot make itself
//! the default viewer on Windows 10 and 11: the user picks it in Default apps, and registering
//! is what makes it appear there.

use std::path::{Path, PathBuf};

use iv_core::FormatId;

/// The name Windows shows for the program, and its entry under `RegisteredApplications`.
pub const APP_NAME: &str = "image-viewer";
/// File types the viewer offers to open: images, and archives of images (comics).
pub const IMAGE_PROG_ID: &str = "ImageViewer.Image";
pub const ARCHIVE_PROG_ID: &str = "ImageViewer.Archive";

const CLASSES: &str = r"Software\Classes";
const APP_KEY: &str = r"Software\image-viewer";
const CAPABILITIES: &str = r"Software\image-viewer\Capabilities";
const REGISTERED_APPLICATIONS: &str = r"Software\RegisteredApplications";

/// One string value to write: under `key` (relative to `HKEY_CURRENT_USER`), the value `name`
/// (empty: the key's default value) set to `data`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Value {
    pub key: String,
    pub name: String,
    pub data: String,
}

/// What unregistering deletes: whole keys that belong to the viewer alone, single values in
/// keys it shares with other programs, and keys to delete only if nothing else is left in them
/// (deepest first).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Removal {
    pub trees: Vec<String>,
    pub values: Vec<(String, String)>,
    pub prune_if_empty: Vec<String>,
}

/// Words that Windows shows: the file type names and the program's description, in the
/// language the user chose.
pub struct Texts<'a> {
    pub image_type: &'a str,
    pub archive_type: &'a str,
    pub description: &'a str,
}

/// Every extension the viewer opens with the file type it is registered as, from the codec
/// registry's formats and the archive reader (lowercase, without the dot, no repeats).
pub fn extensions() -> Vec<(String, &'static str)> {
    let images = FormatId::ALL.iter().flat_map(|f| f.extensions().iter()).map(|e| (*e, IMAGE_PROG_ID));
    let archives = iv_pipeline::archive::extensions().map(|e| (e, ARCHIVE_PROG_ID));
    let mut out: Vec<(String, &'static str)> = Vec::new();
    for (ext, prog_id) in images.chain(archives) {
        let ext = ext.to_ascii_lowercase();
        if !out.iter().any(|(e, _)| *e == ext) {
            out.push((ext, prog_id));
        }
    }
    out
}

fn value(key: impl Into<String>, name: &str, data: impl Into<String>) -> Value {
    Value { key: key.into(), name: name.to_string(), data: data.into() }
}

/// The file name Windows knows the program by (`Applications\<name>`).
pub fn exe_name(exe: &Path) -> String {
    exe.file_name().map_or_else(|| "image-viewer.exe".to_string(), |n| n.to_string_lossy().into_owned())
}

/// Everything registering writes for the program at `exe`.
pub fn registration(exe: &Path, t: &Texts) -> Vec<Value> {
    let exe_text = exe.display().to_string();
    let open = format!("\"{exe_text}\" \"%1\"");
    let icon = format!("\"{exe_text}\",0");
    let app = application_key(&exe_name(exe));
    let exts = extensions();
    let mut out = vec![
        // "Open with" finds the program by its file name; it lists the types it supports
        value(&app, "FriendlyAppName", APP_NAME),
        value(format!(r"{app}\shell\open\command"), "", &open),
    ];
    out.extend(exts.iter().map(|(ext, _)| value(format!(r"{app}\SupportedTypes"), &format!(".{ext}"), "")));
    // the file types: what Explorer shows and runs when the user makes the viewer the default
    for (prog_id, name) in [(IMAGE_PROG_ID, t.image_type), (ARCHIVE_PROG_ID, t.archive_type)] {
        let key = format!(r"{CLASSES}\{prog_id}");
        out.push(value(&key, "", name));
        out.push(value(format!(r"{key}\DefaultIcon"), "", &icon));
        out.push(value(format!(r"{key}\shell\open\command"), "", &open));
    }
    // each extension offers the viewer in its "Open with" list
    out.extend(exts.iter().map(|(ext, prog_id)| value(format!(r"{CLASSES}\.{ext}\OpenWithProgids"), prog_id, "")));
    // Default apps lists registered applications with their capabilities
    out.push(value(CAPABILITIES, "ApplicationName", APP_NAME));
    out.push(value(CAPABILITIES, "ApplicationDescription", t.description));
    out.push(value(CAPABILITIES, "ApplicationIcon", &icon));
    out.extend(
        exts.iter()
            .map(|(ext, prog_id)| value(format!(r"{CAPABILITIES}\FileAssociations"), &format!(".{ext}"), *prog_id)),
    );
    out.push(value(REGISTERED_APPLICATIONS, APP_NAME, CAPABILITIES));
    out
}

/// The command of the viewer's image file type, which every registration writes: the exe it
/// names is where the registration points.
pub const IMAGE_COMMAND_KEY: &str = r"Software\Classes\ImageViewer.Image\shell\open\command";

/// The `"Open with"` key of a program file name.
pub fn application_key(exe_name: &str) -> String {
    format!(r"{CLASSES}\Applications\{exe_name}")
}

/// The program a registered command (`"C:\x\image-viewer.exe" "%1"`) runs; `None` when there
/// is none to be read.
pub fn command_exe(command: &str) -> Option<PathBuf> {
    let c = command.trim();
    let exe = match c.strip_prefix('"') {
        Some(rest) => &rest[..rest.find('"')?],
        // unquoted (written by hand): up to the arguments
        None => {
            let end = [c.find(" \""), c.find(" %")].into_iter().flatten().min().unwrap_or(c.len());
            c[..end].trim()
        }
    };
    (!exe.is_empty()).then(|| PathBuf::from(exe))
}

/// Whether two paths name the same file as Windows sees it: case does not matter, nor which slash.
pub fn same_path(a: &Path, b: &Path) -> bool {
    let norm = |p: &Path| {
        let s = p.to_string_lossy().replace('/', "\\");
        s.strip_prefix(r"\\?\").unwrap_or(&s).trim_end_matches('\\').to_lowercase()
    };
    norm(a) == norm(b)
}

/// Where the "Open with" registration points, judged at startup.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    NotRegistered,
    /// At the running program: nothing to do.
    Here,
    /// At a program that no longer exists (the viewer was moved or unzipped elsewhere and the
    /// old copy deleted): registering again for the running program repairs it.
    Repair {
        old: PathBuf,
    },
    /// At another copy that still exists, or that cannot be looked at now (its drive unplugged,
    /// its share offline, no access): left alone; the settings window shows it, and the
    /// Register button moves it here.
    OtherCopy(PathBuf),
}

/// What the file system says about the registered program.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Found {
    /// The file is there.
    Present,
    /// The file is there and is the running program, reached by another path (a `subst` drive,
    /// a junction, a short 8.3 name).
    SameFile,
    /// Its drive or share answers and the file is not on it: really gone.
    Gone,
    /// No answer that says the file is gone: the drive or share is not reachable, or the file
    /// or a folder on the way cannot be read.
    Unknown,
}

/// What to do about the registration found for `registered` (`None`: there is none), when the
/// running program is `current` and `found` says what the file system knows of `registered`.
/// Only a program known to be gone is repaired.
pub fn check(registered: Option<&Path>, current: &Path, found: Found) -> Check {
    match registered {
        None => Check::NotRegistered,
        Some(r) if same_path(r, current) || found == Found::SameFile => Check::Here,
        Some(r) if found == Found::Gone => Check::Repair { old: r.to_path_buf() },
        Some(r) => Check::OtherCopy(r.to_path_buf()),
    }
}

/// The root a path starts from (`C:\`, `\\server\share\`, `\\?\C:\`); `None` for a path that
/// is not absolute.
pub fn volume_root(p: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut parts = p.components();
    let Some(Component::Prefix(prefix)) = parts.next() else { return None };
    let Some(Component::RootDir) = parts.next() else { return None };
    let mut root = PathBuf::from(prefix.as_os_str());
    root.push(std::path::MAIN_SEPARATOR_STR);
    Some(root)
}

/// Looks `registered` up on disk, for the running program `current`. "Gone" needs a clear
/// answer: the file is not found while the root of its drive or share can be read. May be slow
/// (a sleeping drive, a share that has to time out), so not for the window's thread.
pub fn look_up(registered: &Path, current: &Path) -> Found {
    match std::fs::metadata(registered) {
        Ok(_) => {
            let same = matches!(
                (std::fs::canonicalize(registered), std::fs::canonicalize(current)),
                (Ok(a), Ok(b)) if same_path(&a, &b)
            );
            if same { Found::SameFile } else { Found::Present }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // a missing drive letter or share also says "not found": only a readable root counts
            let root_answers = volume_root(registered).is_some_and(|root| std::fs::read_dir(root).is_ok());
            if root_answers { Found::Gone } else { Found::Unknown }
        }
        Err(_) => Found::Unknown,
    }
}

/// Everything unregistering removes for the program whose file is called `exe_name`. Keys
/// other programs use (`.jpg`, `.jpg\OpenWithProgids`, `RegisteredApplications`) lose only the
/// viewer's values, and are deleted only when that leaves them empty. The user's own choice of
/// default program (`UserChoice`) is never touched.
pub fn removal(exe_name: &str) -> Removal {
    let exts = extensions();
    let mut values: Vec<(String, String)> =
        exts.iter().map(|(ext, prog_id)| (format!(r"{CLASSES}\.{ext}\OpenWithProgids"), prog_id.to_string())).collect();
    values.push((REGISTERED_APPLICATIONS.to_string(), APP_NAME.to_string()));
    let mut prune_if_empty = Vec::new();
    for (ext, _) in &exts {
        prune_if_empty.push(format!(r"{CLASSES}\.{ext}\OpenWithProgids"));
        prune_if_empty.push(format!(r"{CLASSES}\.{ext}"));
    }
    prune_if_empty.push(APP_KEY.to_string());
    Removal {
        trees: vec![
            application_key(exe_name),
            format!(r"{CLASSES}\{IMAGE_PROG_ID}"),
            format!(r"{CLASSES}\{ARCHIVE_PROG_ID}"),
            CAPABILITIES.to_string(),
        ],
        values,
        prune_if_empty,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T: Texts = Texts { image_type: "Image", archive_type: "Archive", description: "viewer" };

    fn find<'a>(values: &'a [Value], key: &str, name: &str) -> Option<&'a str> {
        values.iter().find(|v| v.key == key && v.name == name).map(|v| v.data.as_str())
    }

    #[test]
    fn extensions_come_from_the_codecs_and_the_archive_reader() {
        let exts = extensions();
        let prog = |e: &str| exts.iter().find(|(x, _)| x == e).map(|(_, p)| *p);
        assert_eq!(prog("jpg"), Some(IMAGE_PROG_ID));
        assert_eq!(prog("jxl"), Some(IMAGE_PROG_ID));
        assert_eq!(prog("cbz"), Some(ARCHIVE_PROG_ID));
        assert_eq!(prog("7z"), Some(ARCHIVE_PROG_ID));
        assert_eq!(prog("txt"), None);
        // every format's every extension, once each
        for f in FormatId::ALL {
            for e in f.extensions() {
                assert_eq!(exts.iter().filter(|(x, _)| x == e).count(), 1, "{e}");
            }
        }
        assert!(exts.iter().all(|(e, _)| !e.starts_with('.') && *e == e.to_ascii_lowercase()));
    }

    #[test]
    fn registration_lists_the_program_its_types_and_its_capabilities() {
        let exe = Path::new(r"C:\Tools\image viewer\image-viewer.exe");
        let v = registration(exe, &T);
        let app = r"Software\Classes\Applications\image-viewer.exe";
        let open = r#""C:\Tools\image viewer\image-viewer.exe" "%1""#;
        assert_eq!(find(&v, app, "FriendlyAppName"), Some(APP_NAME));
        assert_eq!(find(&v, &format!(r"{app}\shell\open\command"), ""), Some(open), "paths with spaces are quoted");
        assert_eq!(find(&v, &format!(r"{app}\SupportedTypes"), ".webp"), Some(""));
        assert_eq!(find(&v, r"Software\Classes\ImageViewer.Image", ""), Some("Image"));
        assert_eq!(find(&v, r"Software\Classes\ImageViewer.Archive\shell\open\command", ""), Some(open));
        assert_eq!(
            find(&v, r"Software\Classes\ImageViewer.Image\DefaultIcon", ""),
            Some(r#""C:\Tools\image viewer\image-viewer.exe",0"#)
        );
        assert_eq!(find(&v, r"Software\Classes\.png\OpenWithProgids", IMAGE_PROG_ID), Some(""));
        assert_eq!(find(&v, r"Software\Classes\.cbz\OpenWithProgids", ARCHIVE_PROG_ID), Some(""));
        let caps = r"Software\image-viewer\Capabilities";
        assert_eq!(find(&v, caps, "ApplicationName"), Some(APP_NAME));
        assert_eq!(find(&v, &format!(r"{caps}\FileAssociations"), ".jpeg"), Some(IMAGE_PROG_ID));
        assert_eq!(find(&v, r"Software\RegisteredApplications", APP_NAME), Some(caps));
        // nothing outside the current user's own classes and the program's own key
        for x in &v {
            assert!(
                x.key.starts_with(r"Software\Classes\")
                    || x.key.starts_with(r"Software\image-viewer\")
                    || x.key == r"Software\RegisteredApplications",
                "{x:?}"
            );
            assert!(!x.key.contains("UserChoice"), "{x:?}");
        }
    }

    #[test]
    fn the_registered_program_is_read_from_its_command() {
        let exe = Path::new(r"C:\Tools\image viewer\image-viewer.exe");
        let v = registration(exe, &T);
        let command = find(&v, IMAGE_COMMAND_KEY, "").unwrap();
        assert_eq!(command_exe(command).as_deref(), Some(exe), "what registration writes reads back");
        assert_eq!(command_exe(r#""D:\a b\iv.exe""#).as_deref(), Some(Path::new(r"D:\a b\iv.exe")));
        assert_eq!(command_exe(r"D:\iv\image-viewer.exe %1").as_deref(), Some(Path::new(r"D:\iv\image-viewer.exe")));
        assert_eq!(command_exe(r#"D:\iv\viewer.exe "%1""#).as_deref(), Some(Path::new(r"D:\iv\viewer.exe")));
        assert_eq!(command_exe(r"D:\iv\viewer.exe").as_deref(), Some(Path::new(r"D:\iv\viewer.exe")));
        for none in ["", "   ", r#""unclosed"#, r#""" "%1""#] {
            assert_eq!(command_exe(none), None, "{none:?}");
        }
    }

    #[test]
    fn startup_repairs_only_a_registration_whose_program_is_gone() {
        let here = Path::new(r"C:\Tools\iv-0.2\image-viewer.exe");
        let old = Path::new(r"C:\Tools\iv-0.1\image-viewer.exe");
        assert_eq!(check(None, here, Found::Gone), Check::NotRegistered);
        assert_eq!(check(Some(here), here, Found::Present), Check::Here);
        // the same file however it is spelled, even if the existence check failed
        assert_eq!(check(Some(Path::new(r"c:/tools/IV-0.2/Image-Viewer.EXE")), here, Found::Gone), Check::Here);
        assert_eq!(check(Some(Path::new(r"\\?\C:\Tools\iv-0.2\image-viewer.exe")), here, Found::Present), Check::Here);
        // or reached by another path that the file system says is this file
        assert_eq!(check(Some(Path::new(r"X:\image-viewer.exe")), here, Found::SameFile), Check::Here);
        // the old copy was deleted: repair; it is still there: leave it
        assert_eq!(check(Some(old), here, Found::Gone), Check::Repair { old: old.to_path_buf() });
        assert_eq!(check(Some(old), here, Found::Present), Check::OtherCopy(old.to_path_buf()));
        // no clear answer (drive unplugged, share offline, no access): leave it too
        assert_eq!(check(Some(old), here, Found::Unknown), Check::OtherCopy(old.to_path_buf()));
        // non-ASCII folders compare without case too
        assert!(same_path(Path::new(r"C:\Ärger\a.exe"), Path::new(r"c:\ärger\A.EXE")));
        assert!(!same_path(Path::new(r"C:\a\image-viewer.exe"), Path::new(r"C:\b\image-viewer.exe")));
    }

    #[test]
    fn volume_roots() {
        let root = |p: &str| volume_root(Path::new(p)).map(|r| r.to_string_lossy().into_owned());
        assert_eq!(root(r"C:\Tools\iv\image-viewer.exe").as_deref(), Some(r"C:\"));
        assert_eq!(root(r"\\server\share\iv\image-viewer.exe").as_deref(), Some(r"\\server\share\"));
        assert_eq!(root(r"\\?\D:\iv\image-viewer.exe").as_deref(), Some(r"\\?\D:\"));
        assert_eq!(root(r"\\?\UNC\server\share\x.exe").as_deref(), Some(r"\\?\UNC\server\share\"));
        for none in [r"image-viewer.exe", r"iv\image-viewer.exe", r"C:image-viewer.exe", r"\iv\image-viewer.exe"] {
            assert_eq!(root(none), None, "{none}");
        }
    }

    #[test]
    fn only_a_clear_not_found_counts_as_gone() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("new").join("image-viewer.exe");
        let other = dir.path().join("other").join("image-viewer.exe");
        for f in [&current, &other] {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, b"MZ").unwrap();
        }
        assert_eq!(look_up(&other, &current), Found::Present);
        // the running program by a path that differs as text
        let roundabout = dir.path().join("new").join("..").join("new").join("image-viewer.exe");
        assert!(!same_path(&roundabout, &current));
        assert_eq!(look_up(&roundabout, &current), Found::SameFile);
        assert_eq!(check(Some(&roundabout), &current, look_up(&roundabout, &current)), Check::Here);
        // deleted, with or without its folder, on a drive that answers
        assert_eq!(look_up(&dir.path().join("other").join("gone.exe"), &current), Found::Gone);
        assert_eq!(look_up(&dir.path().join("old").join("image-viewer.exe"), &current), Found::Gone);
        // a relative path, or a drive letter that is not there (an unplugged stick): no answer
        assert_eq!(look_up(Path::new(r"old\image-viewer.exe"), &current), Found::Unknown);
        let missing = (b'D'..=b'Z').rev().map(|l| format!(r"{}:\", l as char)).find(|d| !Path::new(d).exists());
        if let Some(drive) = missing {
            let on_it = Path::new(&drive).join("iv").join("image-viewer.exe");
            assert_eq!(look_up(&on_it, &current), Found::Unknown, "{}", on_it.display());
            assert_eq!(check(Some(&on_it), &current, Found::Unknown), Check::OtherCopy(on_it.clone()));
        }
    }

    #[test]
    fn removal_undoes_exactly_what_registration_wrote() {
        let exe = Path::new(r"D:\x\viewer.exe");
        let written = registration(exe, &T);
        let r = removal(&exe_name(exe));
        assert!(r.trees.contains(&r"Software\Classes\Applications\viewer.exe".to_string()));
        // every written value is gone afterwards: inside a deleted tree, or removed by name
        for v in &written {
            let in_tree = r.trees.iter().any(|t| v.key == *t || v.key.starts_with(&format!(r"{t}\")));
            let by_name = r.values.iter().any(|(k, n)| *k == v.key && *n == v.name);
            assert!(in_tree || by_name, "left behind: {v:?}");
        }
        // shared keys are never deleted whole: only pruned when empty
        for shared in
            [r"Software\Classes\.jpg", r"Software\Classes\.jpg\OpenWithProgids", r"Software\RegisteredApplications"]
        {
            assert!(!r.trees.iter().any(|t| t == shared), "{shared}");
        }
        let child = r.prune_if_empty.iter().position(|k| k == r"Software\Classes\.jpg\OpenWithProgids").unwrap();
        let parent = r.prune_if_empty.iter().position(|k| k == r"Software\Classes\.jpg").unwrap();
        assert!(child < parent, "children are pruned before their parents");
    }
}
