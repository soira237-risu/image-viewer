//! Registering the viewer's Explorer thumbnail provider (`crates/iv-thumb`, the DLL next to the
//! program) for the current user: which registry values that takes, and what undoing it
//! removes. `win::shell` only reads, writes and deletes them.
//!
//! The provider is offered only for extensions that no thumbnail handler claims yet — neither
//! Windows' own (JPEG, PNG and every format a WIC codec covers, through the extension or its
//! perceived type) nor another program's. So it never takes over a picture Windows already
//! draws, and only the formats that would otherwise show an icon get one.

use std::path::{Path, PathBuf};

use iv_core::FormatId;

use crate::assoc::{Removal, Value};

/// The provider's file, next to `image-viewer.exe`.
pub const DLL_NAME: &str = "image_viewer_thumbs.dll";
/// The shell extension slot of thumbnail handlers (`IThumbnailProvider`).
pub const THUMBNAIL_HANDLER: &str = "{e357fccd-a995-4576-b01f-234630154e96}";
/// The name the class is registered with.
const CLASS_NAME: &str = "image-viewer thumbnails";
/// Archives that hold comics: their first page is their picture. Plain `.zip` and `.7z` are left
/// alone: they hold anything, and an image found inside is rarely what they are.
const COMIC_ARCHIVES: [&str; 2] = ["cbz", "cb7"];

const CLASSES: &str = r"Software\Classes";

/// The provider's class id, `{…}`.
pub fn clsid() -> String {
    iv_pipeline::thumbnail::provider_clsid_text()
}

/// The provider's class key, under `HKEY_CURRENT_USER`.
pub fn class_key() -> String {
    format!(r"{CLASSES}\CLSID\{}", clsid())
}

/// Where the class names its DLL, under `HKEY_CURRENT_USER`.
pub fn server_key() -> String {
    format!(r"{}\InprocServer32", class_key())
}

/// The thumbnail handler key of `ext` (no dot), relative to a classes root.
pub fn handler_key(ext: &str) -> String {
    format!(r".{ext}\ShellEx\{THUMBNAIL_HANDLER}")
}

/// The provider that goes with the program at `exe`.
pub fn dll_for(exe: &Path) -> PathBuf {
    exe.with_file_name(DLL_NAME)
}

/// Formats the viewer reads only through a Windows codec (WIC): where Windows has no thumbnail
/// handler for them it has no codec either, so the provider could not draw them.
const THROUGH_WINDOWS: [FormatId; 2] = [FormatId::Heif, FormatId::Raw];

/// Every extension the provider can draw: the image formats the viewer decodes itself and the
/// comic archives (lowercase, without the dot, no repeats).
pub fn candidates() -> Vec<String> {
    let images =
        FormatId::ALL.iter().filter(|f| !THROUGH_WINDOWS.contains(f)).flat_map(|f| f.extensions().iter().copied());
    unique(images.chain(COMIC_ARCHIVES))
}

/// Every extension whose handler entry unregistering looks at: all the viewer opens, wider than
/// [`candidates`], so an entry written by a version that drew more formats is removed too.
pub fn cleanup_scope() -> Vec<String> {
    let images = FormatId::ALL.iter().flat_map(|f| f.extensions().iter().copied());
    unique(images.chain(iv_pipeline::archive::extensions()).chain(COMIC_ARCHIVES))
}

/// Lowercase, in order, without repeats.
fn unique<'a>(exts: impl Iterator<Item = &'a str>) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for ext in exts {
        let ext = ext.to_ascii_lowercase();
        if !out.contains(&ext) {
            out.push(ext);
        }
    }
    out
}

/// The candidates no other thumbnail handler claims: `taken(ext)` says whether one does
/// (looked up with the viewer's own entries removed first).
pub fn chosen(taken: impl Fn(&str) -> bool) -> Vec<String> {
    candidates().into_iter().filter(|e| !taken(e)).collect()
}

/// Whether a handler value names the provider (class ids are compared without case).
pub fn is_ours(handler: &str) -> bool {
    handler.trim().eq_ignore_ascii_case(&clsid())
}

/// Everything registering writes for the provider at `dll`, drawing thumbnails of `exts`.
/// The class runs in a surrogate process: `DisableProcessIsolation` is never written.
pub fn registration(dll: &Path, exts: &[String]) -> Vec<Value> {
    let value = |key: String, name: &str, data: String| Value { key, name: name.to_string(), data };
    let mut out = vec![
        value(class_key(), "", CLASS_NAME.to_string()),
        value(server_key(), "", dll.display().to_string()),
        value(server_key(), "ThreadingModel", "Apartment".to_string()),
    ];
    out.extend(exts.iter().map(|e| value(format!(r"{CLASSES}\{}", handler_key(e)), "", clsid())));
    out
}

/// What unregistering removes: the class, and the handler entries of `ours` (the extensions
/// whose entry names the provider; another program's entry is never touched). Keys left empty
/// are pruned, deepest first.
pub fn removal(ours: &[String]) -> Removal {
    let mut r = Removal { trees: vec![class_key()], ..Default::default() };
    for ext in ours {
        let handler = format!(r"{CLASSES}\{}", handler_key(ext));
        r.values.push((handler.clone(), String::new()));
        r.prune_if_empty.push(handler);
        r.prune_if_empty.push(format!(r"{CLASSES}\.{ext}\ShellEx"));
        r.prune_if_empty.push(format!(r"{CLASSES}\.{ext}"));
    }
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    fn find<'a>(values: &'a [Value], key: &str, name: &str) -> Option<&'a str> {
        values.iter().find(|v| v.key == key && v.name == name).map(|v| v.data.as_str())
    }

    #[test]
    fn candidates_are_the_formats_the_viewer_decodes_itself_and_comic_archives() {
        let c = candidates();
        for e in ["jpg", "tga", "exr", "qoi", "jxl", "avif", "cbz", "cb7"] {
            assert!(c.iter().any(|x| x == e), "{e}");
        }
        for e in ["zip", "7z", "txt", "heic", "dng", "cr3", "r3d"] {
            assert!(!c.iter().any(|x| x == e), "{e}");
        }
        let unique: std::collections::HashSet<_> = c.iter().collect();
        assert_eq!(unique.len(), c.len());
        assert!(c.iter().all(|e| !e.starts_with('.') && *e == e.to_ascii_lowercase()));
        // unregistering looks wider: every candidate, and the formats left out since
        let scope = cleanup_scope();
        assert!(c.iter().all(|e| scope.contains(e)));
        for e in ["heic", "dng", "r3d", "zip", "7z"] {
            assert!(scope.iter().any(|x| x == e), "{e}");
        }
    }

    #[test]
    fn extensions_with_a_handler_are_left_to_it() {
        let windows_draws = ["jpg", "jpeg", "png", "heic", "qoi"];
        let c = chosen(|e| windows_draws.contains(&e));
        assert!(c.iter().any(|e| e == "tga") && c.iter().any(|e| e == "cbz"));
        for e in windows_draws {
            assert!(!c.iter().any(|x| x == e), "{e} taken over");
        }
        assert!(chosen(|_| true).is_empty());
    }

    #[test]
    fn registration_names_the_dll_and_each_extension() {
        let dll = Path::new(r"C:\Tools\image viewer\image_viewer_thumbs.dll");
        let exts = vec!["tga".to_string(), "cbz".to_string()];
        let v = registration(dll, &exts);
        let class = r"Software\Classes\CLSID\{d8db828f-95c2-42e9-aeb2-8024b33517f1}";
        assert_eq!(class_key(), class);
        assert_eq!(find(&v, class, ""), Some(CLASS_NAME));
        let server = format!(r"{class}\InprocServer32");
        assert_eq!(find(&v, &server, ""), Some(r"C:\Tools\image viewer\image_viewer_thumbs.dll"));
        assert_eq!(find(&v, &server, "ThreadingModel"), Some("Apartment"));
        let tga = r"Software\Classes\.tga\ShellEx\{e357fccd-a995-4576-b01f-234630154e96}";
        assert_eq!(find(&v, tga, ""), Some(clsid().as_str()));
        assert!(find(&v, r"Software\Classes\.cbz\ShellEx\{e357fccd-a995-4576-b01f-234630154e96}", "").is_some());
        assert!(!v.iter().any(|x| x.name == "DisableProcessIsolation"), "runs out of Explorer's process");
        assert!(v.iter().all(|x| x.key.starts_with(r"Software\Classes\")), "only the user's own classes");
        assert_eq!(dll_for(Path::new(r"D:\iv\image-viewer.exe")), Path::new(r"D:\iv\image_viewer_thumbs.dll"));
        assert!(
            is_ours(" {D8DB828F-95C2-42E9-AEB2-8024B33517F1} ") && !is_ours("{c7657c4a-9f68-40fa-a4df-96bc08eb3551}")
        );
    }

    #[test]
    fn removal_undoes_what_registration_wrote() {
        let exts = vec!["tga".to_string(), "exr".to_string()];
        let written = registration(Path::new(r"D:\x\image_viewer_thumbs.dll"), &exts);
        let r = removal(&exts);
        for v in &written {
            let in_tree = r.trees.iter().any(|t| v.key == *t || v.key.starts_with(&format!(r"{t}\")));
            let by_name = r.values.iter().any(|(k, n)| *k == v.key && *n == v.name);
            assert!(in_tree || by_name, "left behind: {v:?}");
        }
        // extension keys are shared with other programs: emptied ones are pruned, never deleted whole
        assert!(!r.trees.iter().any(|t| t.contains(r"\.tga")));
        let at = |k: &str| r.prune_if_empty.iter().position(|x| x == k).unwrap();
        let handler = r"Software\Classes\.tga\ShellEx\{e357fccd-a995-4576-b01f-234630154e96}";
        assert!(at(handler) < at(r"Software\Classes\.tga\ShellEx"));
        assert!(at(r"Software\Classes\.tga\ShellEx") < at(r"Software\Classes\.tga"));
    }
}
