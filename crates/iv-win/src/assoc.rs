//! Listing the viewer in Windows' "Open with" menu and in Settings > Default apps, for the
//! current user only (`HKEY_CURRENT_USER`: no administrator rights, nothing machine-wide).
//!
//! This module decides which registry values that takes and what undoing it removes; the
//! Windows side (`win::registry`) only writes and deletes them. A program cannot make itself
//! the default viewer on Windows 10 and 11: the user picks it in Default apps, and registering
//! is what makes it appear there.

use std::path::Path;

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
    let app = format!(r"{CLASSES}\Applications\{}", exe_name(exe));
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
            format!(r"{CLASSES}\Applications\{exe_name}"),
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
