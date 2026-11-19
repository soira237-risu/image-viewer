//! The Windows shell around the current file: the clipboard, the Recycle Bin, Explorer, Settings
//! and the per-user "Open with" and Explorer thumbnail registrations. The data each needs is
//! built (and tested) in [`crate::clipboard`], [`crate::assoc`] and [`crate::thumbs`]; this
//! module only hands it to Windows.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, GlobalFree, HANDLE, HGLOBAL, HWND, WIN32_ERROR,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CLASSES_ROOT, HKEY_CURRENT_USER, KEY_READ, REG_SZ, RRF_RT_REG_SZ, RegCloseKey, RegDeleteKeyValueW,
    RegDeleteKeyW, RegDeleteTreeW, RegGetValueW, RegOpenKeyExW, RegQueryInfoKeyW, RegSetKeyValueW,
};
use windows::Win32::UI::Shell::{
    FO_DELETE, FOF_ALLOWUNDO, FOF_NOCONFIRMATION, FOF_SILENT, FOF_WANTNUKEWARNING, ILCreateFromPathW, ILFree,
    SHCNE_ASSOCCHANGED, SHCNF_IDLIST, SHChangeNotify, SHFILEOPSTRUCTW, SHFileOperationW, SHOpenFolderAndSelectItems,
    ShellExecuteW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
use windows::core::{PCWSTR, w};

use super::wide;
use crate::assoc::{self, Check, Removal, Value};
use crate::clipboard;
use crate::i18n::Strings;
use crate::thumbs;

/// One clipboard format and its bytes.
pub enum Clip {
    Standard(u32, Vec<u8>),
    /// A format registered by name (`"PNG"`).
    Named(&'static str, Vec<u8>),
}

/// Copies `bytes` into memory the clipboard can own.
fn global_copy(bytes: &[u8]) -> Result<HGLOBAL, String> {
    unsafe {
        let mem = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1)).map_err(|e| e.message())?;
        let p = GlobalLock(mem);
        if p.is_null() {
            let _ = GlobalFree(Some(mem));
            return Err("GlobalLock failed".into());
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), p.cast::<u8>(), bytes.len());
        let _ = GlobalUnlock(mem);
        Ok(mem)
    }
}

/// Replaces the clipboard's contents with `items`, all offered at once (a program pasting picks
/// the format it understands best). Another program may hold the clipboard for a moment, so
/// opening it is retried briefly.
pub fn set_clipboard(owner: HWND, items: Vec<Clip>) -> Result<(), String> {
    unsafe {
        let mut opened = OpenClipboard(Some(owner));
        for _ in 0..10 {
            if opened.is_ok() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
            opened = OpenClipboard(Some(owner));
        }
        opened.map_err(|e| e.message())?;
        let result = (|| {
            EmptyClipboard().map_err(|e| e.message())?;
            for item in items {
                let (format, bytes) = match item {
                    Clip::Standard(f, b) => (f, b),
                    Clip::Named(name, b) => {
                        let n = wide(name);
                        match RegisterClipboardFormatW(PCWSTR(n.as_ptr())) {
                            0 => return Err(format!("cannot register the {name} clipboard format")),
                            f => (f, b),
                        }
                    }
                };
                let mem = global_copy(&bytes)?;
                drop(bytes);
                // on success the clipboard owns the memory; on failure it is still ours
                if let Err(e) = SetClipboardData(format, Some(HANDLE(mem.0))) {
                    let _ = GlobalFree(Some(mem));
                    return Err(e.message());
                }
            }
            Ok(())
        })();
        let _ = CloseClipboard();
        result
    }
}

/// The current file on the clipboard as a file: pasting in Explorer copies it, pasting in a
/// chat attaches it.
pub fn copy_file(owner: HWND, path: &Path) -> Result<(), String> {
    set_clipboard(
        owner,
        vec![
            Clip::Standard(clipboard::CF_HDROP, clipboard::file_drop(&[path])),
            Clip::Named(clipboard::DROP_EFFECT_FORMAT, clipboard::DROPEFFECT_COPY.to_le_bytes().to_vec()),
        ],
    )
}

pub fn copy_text(owner: HWND, text: &str) -> Result<(), String> {
    set_clipboard(owner, vec![Clip::Standard(clipboard::CF_UNICODETEXT, clipboard::text(text))])
}

/// Moves `path` to the Recycle Bin. Asking the user is the caller's job; the shell still warns
/// before deleting for good a file it cannot recycle (on a network drive, or too large for the
/// bin), and explains failures itself. Succeeds only if the file is really gone.
pub fn recycle(owner: HWND, path: &Path) -> Result<(), String> {
    let from = clipboard::path_list(path);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: owner,
        wFunc: FO_DELETE,
        pFrom: PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_WANTNUKEWARNING).0 as u16,
        ..Default::default()
    };
    let code = unsafe { SHFileOperationW(&mut op) };
    if path.symlink_metadata().is_err() {
        Ok(())
    } else if op.fAnyOperationsAborted.as_bool() {
        Err("cancelled".into())
    } else {
        Err(format!("shell error 0x{code:x}"))
    }
}

/// Opens the folder holding `path` in Explorer with `path` selected.
pub fn show_in_folder(path: &Path) -> Result<(), String> {
    let p = wide(&path.to_string_lossy());
    unsafe {
        let pidl = ILCreateFromPathW(PCWSTR(p.as_ptr()));
        if pidl.is_null() {
            return Err(format!("{}: not found", path.display()));
        }
        // no items: the folder of the item `pidl` names is opened with that item selected
        let result = SHOpenFolderAndSelectItems(pidl, None, 0).map_err(|e| e.message());
        ILFree(Some(pidl));
        result
    }
}

/// Opens a URI (`ms-settings:…`) with the program Windows has for it.
pub fn open_uri(owner: HWND, uri: &str) -> Result<(), String> {
    let u = wide(uri);
    let r = unsafe { ShellExecuteW(Some(owner), w!("open"), PCWSTR(u.as_ptr()), None, None, SW_SHOWNORMAL) };
    // ShellExecute reports success as a value above 32
    if r.0 as isize > 32 { Ok(()) } else { Err(format!("ShellExecute error {}", r.0 as isize)) }
}

/// Settings > Default apps, on the viewer's own page where Windows supports that.
pub const DEFAULT_APPS_URI: &str = "ms-settings:defaultapps?registeredAppUser=image-viewer";

fn check(e: WIN32_ERROR, what: &str) -> Result<(), String> {
    if e == ERROR_SUCCESS || e == ERROR_FILE_NOT_FOUND {
        Ok(())
    } else {
        Err(format!("{what}: {}", windows::core::Error::from(e.to_hresult()).message()))
    }
}

/// `key` under the test root `root` (empty for the real location).
fn under(root: &str, key: &str) -> Vec<u16> {
    if root.is_empty() { wide(key) } else { wide(&format!(r"{root}\{key}")) }
}

/// Writes `values` under `HKEY_CURRENT_USER\<root>` (keys are created as needed).
pub fn write_values(root: &str, values: &[Value]) -> Result<(), String> {
    for v in values {
        let key = under(root, &v.key);
        let name = wide(&v.name);
        let data = wide(&v.data);
        let bytes = (data.len() * 2) as u32;
        let e = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                REG_SZ.0,
                Some(data.as_ptr().cast()),
                bytes,
            )
        };
        if e != ERROR_SUCCESS {
            return check(e, &format!(r"HKCU\{}", String::from_utf16_lossy(&key[..key.len() - 1])));
        }
    }
    Ok(())
}

/// Whether `HKEY_CURRENT_USER\<key>` exists with no values and no subkeys.
fn is_empty_key(key: &[u16]) -> bool {
    unsafe {
        let mut h = HKEY::default();
        if RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), None, KEY_READ, &mut h) != ERROR_SUCCESS {
            return false;
        }
        let (mut subkeys, mut values) = (0u32, 0u32);
        let e = RegQueryInfoKeyW(
            h,
            None,
            None,
            None,
            Some(&mut subkeys),
            None,
            None,
            Some(&mut values),
            None,
            None,
            None,
            None,
        );
        let _ = RegCloseKey(h);
        e == ERROR_SUCCESS && subkeys == 0 && values == 0
    }
}

/// Deletes what `r` lists under `HKEY_CURRENT_USER\<root>`; what is already gone is fine.
pub fn remove(root: &str, r: &Removal) -> Result<(), String> {
    for tree in &r.trees {
        let key = under(root, tree);
        check(unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr())) }, tree)?;
    }
    for (k, name) in &r.values {
        let key = under(root, k);
        let n = wide(name);
        check(unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), PCWSTR(n.as_ptr())) }, k)?;
    }
    for k in &r.prune_if_empty {
        let key = under(root, k);
        if is_empty_key(&key) {
            check(unsafe { RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr())) }, k)?;
        }
    }
    Ok(())
}

fn texts(t: &Strings) -> assoc::Texts<'static> {
    assoc::Texts { image_type: t.image_type_name, archive_type: t.archive_type_name, description: t.app_description }
}

/// Lists the running program in "Open with" and Default apps for the current user, with type
/// names in the language of `t`.
pub fn register(t: &Strings) -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let result = {
        let _writing = writing();
        write_values("", &assoc::registration(&exe, &texts(t)))
    };
    notify_associations_changed();
    result
}

/// The string value `name` of `HKEY_CURRENT_USER\<root>\<key>`, if there is one.
pub fn read_value(root: &str, key: &str, name: &str) -> Option<String> {
    read_from(HKEY_CURRENT_USER, &under(root, key), name)
}

/// The string value `name` of `<hive>\<key>` (`key` NUL-terminated), if there is one.
fn read_from(hive: HKEY, key: &[u16], name: &str) -> Option<String> {
    let name = wide(name);
    let mut buf = vec![0u16; 512];
    // the value may grow between asking its size and reading it: try again a few times
    for _ in 0..4 {
        let mut bytes = (buf.len() * 2) as u32;
        let e = unsafe {
            RegGetValueW(
                hive,
                PCWSTR(key.as_ptr()),
                PCWSTR(name.as_ptr()),
                RRF_RT_REG_SZ,
                None,
                Some(buf.as_mut_ptr().cast()),
                Some(&mut bytes),
            )
        };
        if e == ERROR_MORE_DATA {
            buf = vec![0u16; (bytes as usize).div_ceil(2) + 1];
            continue;
        }
        let len = (bytes as usize / 2).min(buf.len());
        return (e == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf[..len.saturating_sub(1)]));
    }
    None
}

/// The program the registration under `HKEY_CURRENT_USER\<root>` runs, if there is one.
pub fn registered_exe(root: &str) -> Option<PathBuf> {
    read_value(root, assoc::IMAGE_COMMAND_KEY, "").and_then(|c| assoc::command_exe(&c))
}

/// Where the registration under `root` points, for the running program `current`. Looks at the
/// file system (the registered program may be on a slow or missing drive), so the window's
/// thread does not call it at startup.
pub fn registration_check(root: &str, current: &Path) -> Check {
    let registered = registered_exe(root);
    let found = registered.as_deref().map_or(assoc::Found::Unknown, |r| assoc::look_up(r, current));
    assoc::check(registered.as_deref(), current, found)
}

/// Held while the registration is written or removed, so a startup repair and the Register or
/// Unregister button do not interleave their writes.
fn writing() -> std::sync::MutexGuard<'static, ()> {
    static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    WRITING.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// At startup: when the registration points at a program that is gone, registers the running
/// program `current` instead (in the language of `t`), dropping the old program's own "Open
/// with" entry if it had another file name. Anything else is left as it is. Returns what was
/// found, and how repairing went when it was needed.
pub fn repair_registration(root: &str, current: &Path, t: &Strings) -> (Check, Option<Result<(), String>>) {
    apply_repair(root, current, registration_check(root, current), t)
}

/// The writing half of [`repair_registration`], for what `found` says.
fn apply_repair(root: &str, current: &Path, found: Check, t: &Strings) -> (Check, Option<Result<(), String>>) {
    let Check::Repair { old } = &found else { return (found, None) };
    let writing = writing();
    // the file system was asked without the lock (it can be slow): if Register or Unregister
    // changed the registration meanwhile, that choice stands
    if !registered_exe(root).is_some_and(|r| assoc::same_path(&r, old)) {
        drop(writing);
        return (registration_check(root, current), None);
    }
    let mut result = write_values(root, &assoc::registration(current, &texts(t)));
    let old_name = assoc::exe_name(old);
    if result.is_ok() && !old_name.eq_ignore_ascii_case(&assoc::exe_name(current)) {
        let key = assoc::application_key(&old_name);
        let command = read_value(root, &format!(r"{key}\shell\open\command"), "");
        // only if that entry is the old copy's own, not another program's of the same name
        if command.and_then(|c| assoc::command_exe(&c)).is_some_and(|c| assoc::same_path(&c, old)) {
            result = remove(root, &Removal { trees: vec![key], ..Default::default() });
        }
    }
    drop(writing);
    // the real registration changed: Explorer reads it again
    if root.is_empty() {
        notify_associations_changed();
    }
    (found, Some(result))
}

/// Removes what [`register`] wrote.
pub fn unregister() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let result = {
        let _writing = writing();
        remove("", &assoc::removal(&assoc::exe_name(&exe)))
    };
    notify_associations_changed();
    result
}

/// Whether a thumbnail handler other than the viewer's claims `ext`, where Explorer looks for
/// one: the user's chosen program and the extension's own file type, the extension, Windows'
/// entries for it, and those for its perceived type (`image`: every format a WIC codec draws).
/// Read from the merged classes view, so machine-wide entries count too.
fn thumbnail_handler_taken(ext: &str) -> bool {
    let classes = |key: &str| read_from(HKEY_CLASSES_ROOT, &wide(key), "");
    let handler = |prefix: &str| classes(&format!(r"{prefix}\ShellEx\{}", thumbs::THUMBNAIL_HANDLER));
    let user_choice = read_value(
        "",
        &format!(r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.{ext}\UserChoice"),
        "ProgId",
    );
    let prog_id = classes(&format!(".{ext}"));
    let perceived = read_from(HKEY_CLASSES_ROOT, &wide(&format!(".{ext}")), "PerceivedType");
    let mut places = vec![format!(".{ext}"), format!(r"SystemFileAssociations\.{ext}")];
    places.extend([user_choice, prog_id].into_iter().flatten().filter(|p| !p.trim().is_empty()));
    places.extend(perceived.filter(|p| !p.trim().is_empty()).map(|p| format!(r"SystemFileAssociations\{p}")));
    places.iter().filter_map(|p| handler(p)).any(|h| !h.trim().is_empty() && !thumbs::is_ours(&h))
}

/// The extensions whose thumbnail entry under `HKEY_CURRENT_USER\<root>` names the provider, among
/// every extension the viewer opens (see [`thumbs::cleanup_scope`]).
fn our_thumbnail_exts(root: &str) -> Vec<String> {
    thumbs::cleanup_scope()
        .into_iter()
        .filter(|e| {
            let key = format!(r"Software\Classes\{}", thumbs::handler_key(e));
            read_value(root, &key, "").is_some_and(|v| thumbs::is_ours(&v))
        })
        .collect()
}

/// Registers the provider at `dll` under `HKEY_CURRENT_USER\<root>` for every candidate that
/// `taken` says no other handler claims, after removing its old entries (so a format that has
/// gained a handler of its own since is given back). Returns how many extensions it took.
fn register_thumbnails_at(root: &str, dll: &Path, taken: impl Fn(&str) -> bool) -> Result<usize, String> {
    let _writing = writing();
    remove(root, &thumbs::removal(&our_thumbnail_exts(root)))?;
    let exts = thumbs::chosen(taken);
    if exts.is_empty() {
        return Ok(0);
    }
    write_values(root, &thumbs::registration(dll, &exts))?;
    Ok(exts.len())
}

fn unregister_thumbnails_at(root: &str) -> Result<(), String> {
    let _writing = writing();
    remove(root, &thumbs::removal(&our_thumbnail_exts(root)))
}

/// Draws Explorer thumbnails of the formats that have none with the DLL next to the running
/// program, for the current user. Returns how many extensions that covers (none: nothing was
/// written). A missing DLL is reported with its path.
pub fn register_thumbnails() -> Result<usize, ThumbnailError> {
    let exe = std::env::current_exe().map_err(|e| ThumbnailError::Failed(e.to_string()))?;
    let dll = thumbs::dll_for(&exe);
    if !dll.is_file() {
        return Err(ThumbnailError::DllMissing(dll));
    }
    let result = register_thumbnails_at("", &dll, thumbnail_handler_taken).map_err(ThumbnailError::Failed);
    notify_associations_changed();
    result
}

/// Removes what [`register_thumbnails`] wrote.
pub fn unregister_thumbnails() -> Result<(), String> {
    let result = unregister_thumbnails_at("");
    notify_associations_changed();
    result
}

/// Why registering the thumbnail provider failed.
#[derive(Debug)]
pub enum ThumbnailError {
    DllMissing(PathBuf),
    Failed(String),
}

/// Registers (`on`) or unregisters the thumbnail provider, for the settings window's buttons and
/// the command line: what happened, in the language of `t`, or why it failed.
pub fn change_thumbnails(on: bool, t: &Strings) -> Result<String, String> {
    use crate::i18n::fill;
    let result = if on {
        register_thumbnails().map(|n| match n {
            0 => t.thumbnails_none_needed.to_string(),
            n => fill(t.thumbnails_registered, &[("count", &n.to_string())]),
        })
    } else {
        unregister_thumbnails().map(|()| t.thumbnails_unregistered.to_string()).map_err(ThumbnailError::Failed)
    };
    result.map_err(|e| match e {
        ThumbnailError::DllMissing(dll) => fill(t.thumbnails_dll_missing, &[("path", &dll.display().to_string())]),
        ThumbnailError::Failed(e) => fill(t.thumbnails_failed, &[("error", &e)]),
    })
}

/// The provider's DLL that the registration under `HKEY_CURRENT_USER\<root>` names.
pub fn registered_thumbnail_dll(root: &str) -> Option<PathBuf> {
    read_value(root, &thumbs::server_key(), "").filter(|p| !p.trim().is_empty()).map(PathBuf::from)
}

/// Where the thumbnail registration under `root` points, for the DLL `current` of the running
/// program. Looks at the file system, like [`registration_check`].
pub fn thumbnails_check(root: &str, current: &Path) -> Check {
    let registered = registered_thumbnail_dll(root);
    let found = registered.as_deref().map_or(assoc::Found::Unknown, |r| assoc::look_up(r, current));
    assoc::check(registered.as_deref(), current, found)
}

/// At startup: when the thumbnail registration names a DLL that is gone and the running
/// program has its own, points it there (the extensions stay as they are). Anything else is
/// left alone, like [`repair_registration`].
pub fn repair_thumbnails(root: &str, current: &Path) -> (Check, Option<Result<(), String>>) {
    let found = thumbnails_check(root, current);
    let Check::Repair { old } = &found else { return (found, None) };
    if !current.is_file() {
        return (found, None);
    }
    let writing = writing();
    if !registered_thumbnail_dll(root).is_some_and(|r| assoc::same_path(&r, old)) {
        drop(writing);
        return (thumbnails_check(root, current), None);
    }
    let server = Value { key: thumbs::server_key(), name: String::new(), data: current.display().to_string() };
    let result = write_values(root, &[server]);
    drop(writing);
    if root.is_empty() {
        notify_associations_changed();
    }
    (found, Some(result))
}

/// Tells Explorer to read file associations again.
fn notify_associations_changed() {
    unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED, SHCNF_IDLIST, None, None) };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch key under `HKEY_CURRENT_USER\Software`, deleted when dropped.
    struct Scratch(String);

    impl Scratch {
        fn new(name: &str) -> Self {
            let root = format!(r"Software\image-viewer-test-{name}-{}", std::process::id());
            let _ = remove("", &Removal { trees: vec![root.clone()], ..Default::default() });
            Scratch(root)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = remove("", &Removal { trees: vec![self.0.clone()], ..Default::default() });
        }
    }

    fn read(root: &str, key: &str, name: &str) -> Option<String> {
        read_value(root, key, name)
    }

    fn exists(root: &str, key: &str) -> bool {
        let key = under(root, key);
        let mut h = HKEY::default();
        let ok = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, PCWSTR(key.as_ptr()), None, KEY_READ, &mut h) };
        if ok == ERROR_SUCCESS {
            unsafe {
                let _ = RegCloseKey(h);
            }
        }
        ok == ERROR_SUCCESS
    }

    /// Registering and unregistering in a scratch key: everything written can be read back,
    /// and unregistering removes it while values of other programs in shared keys stay.
    #[test]
    fn register_then_unregister_leaves_other_programs_alone() {
        let scratch = Scratch::new("assoc");
        let root = scratch.0.as_str();
        // another program's entries in keys the viewer shares
        let others = [
            Value { key: r"Software\Classes\.jpg".into(), name: String::new(), data: "OtherApp.Jpeg".into() },
            Value {
                key: r"Software\Classes\.jpg\OpenWithProgids".into(),
                name: "OtherApp.Jpeg".into(),
                data: "".into(),
            },
            Value { key: r"Software\RegisteredApplications".into(), name: "OtherApp".into(), data: "x".into() },
        ];
        write_values(root, &others).unwrap();

        let exe = Path::new(r"C:\Program Files\iv\image-viewer.exe");
        let texts = assoc::Texts { image_type: "Image", archive_type: "Archive", description: "test" };
        let values = assoc::registration(exe, &texts);
        write_values(root, &values).unwrap();
        for v in &values {
            assert_eq!(read(root, &v.key, &v.name).as_deref(), Some(v.data.as_str()), "{v:?}");
        }

        remove(root, &assoc::removal(&assoc::exe_name(exe))).unwrap();
        for v in &values {
            assert_eq!(read(root, &v.key, &v.name), None, "still there: {v:?}");
        }
        for v in &others {
            assert_eq!(read(root, &v.key, &v.name).as_deref(), Some(v.data.as_str()), "removed: {v:?}");
        }
        // keys only the viewer used are gone; shared ones it emptied are pruned
        assert!(!exists(root, r"Software\Classes\Applications\image-viewer.exe"));
        assert!(!exists(root, r"Software\Classes\ImageViewer.Image"));
        assert!(!exists(root, r"Software\image-viewer"));
        assert!(!exists(root, r"Software\Classes\.png"), "emptied extension keys are pruned");
        assert!(exists(root, r"Software\Classes\.jpg\OpenWithProgids"), "still used by the other program");
        // unregistering twice is fine
        remove(root, &assoc::removal(&assoc::exe_name(exe))).unwrap();
    }

    const TEXTS: assoc::Texts = assoc::Texts { image_type: "Image", archive_type: "Archive", description: "test" };

    /// A registration in a scratch key pointing at `exe`.
    fn registered_at(name: &str, exe: &Path) -> Scratch {
        let scratch = Scratch::new(name);
        write_values(&scratch.0, &assoc::registration(exe, &TEXTS)).unwrap();
        scratch
    }

    #[test]
    fn a_registration_whose_program_is_gone_moves_to_the_running_one() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("new").join("image-viewer.exe");
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::write(&current, b"MZ").unwrap();
        let old = dir.path().join("old").join("image-viewer.exe");
        let scratch = registered_at("repair", &old);
        let root = scratch.0.as_str();
        assert_eq!(registered_exe(root).as_deref(), Some(old.as_path()));

        let (found, result) = repair_registration(root, &current, &crate::i18n::EN);
        assert_eq!(found, Check::Repair { old: old.clone() });
        assert_eq!(result, Some(Ok(())));
        // every value now names the running program, in the language given
        for v in assoc::registration(&current, &texts(&crate::i18n::EN)) {
            assert_eq!(read(root, &v.key, &v.name).as_deref(), Some(v.data.as_str()), "{v:?}");
        }
        assert_eq!(registration_check(root, &current), Check::Here);
        // a second start finds nothing to do
        assert_eq!(repair_registration(root, &current, &crate::i18n::EN), (Check::Here, None));
    }

    #[test]
    fn a_renamed_program_drops_the_old_names_entry() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("image-viewer.exe");
        std::fs::write(&current, b"MZ").unwrap();
        let old = dir.path().join("gone").join("viewer-0.1.exe");
        let scratch = registered_at("rename", &old);
        let root = scratch.0.as_str();
        let (_, result) = repair_registration(root, &current, &crate::i18n::KO);
        assert_eq!(result, Some(Ok(())));
        assert!(!exists(root, r"Software\Classes\Applications\viewer-0.1.exe"), "old entry left behind");
        assert!(exists(root, r"Software\Classes\Applications\image-viewer.exe"));
        assert_eq!(registered_exe(root).as_deref(), Some(current.as_path()));

        // an entry of that name that runs some other program is not the old copy's to remove
        let scratch = registered_at("rename-other", &old);
        let root = scratch.0.as_str();
        let foreign = Value {
            key: r"Software\Classes\Applications\viewer-0.1.exe\shell\open\command".into(),
            name: String::new(),
            data: r#""D:\Other\viewer-0.1.exe" "%1""#.into(),
        };
        write_values(root, std::slice::from_ref(&foreign)).unwrap();
        assert_eq!(repair_registration(root, &current, &crate::i18n::KO).1, Some(Ok(())));
        assert_eq!(read(root, &foreign.key, "").as_deref(), Some(foreign.data.as_str()));
    }

    #[test]
    fn another_existing_copy_and_no_registration_are_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("a").join("image-viewer.exe");
        let other = dir.path().join("b").join("image-viewer.exe");
        for f in [&current, &other] {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, b"MZ").unwrap();
        }
        let scratch = registered_at("other", &other);
        let root = scratch.0.as_str();
        let before: Vec<_> = assoc::registration(&other, &TEXTS);
        assert_eq!(repair_registration(root, &current, &crate::i18n::EN), (Check::OtherCopy(other.clone()), None));
        for v in &before {
            assert_eq!(read(root, &v.key, &v.name).as_deref(), Some(v.data.as_str()), "changed: {v:?}");
        }

        let empty = Scratch::new("none");
        assert_eq!(repair_registration(&empty.0, &current, &crate::i18n::EN), (Check::NotRegistered, None));
        assert!(!exists(&empty.0, r"Software\Classes"), "nothing written when nothing was registered");
    }

    #[test]
    fn a_copy_on_a_drive_that_does_not_answer_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("image-viewer.exe");
        std::fs::write(&current, b"MZ").unwrap();
        // a drive letter with nothing behind it, like an unplugged USB stick
        let Some(drive) = (b'D'..=b'Z').rev().map(|l| format!(r"{}:\", l as char)).find(|d| !Path::new(d).exists())
        else {
            return;
        };
        let away = Path::new(&drive).join("iv").join("image-viewer.exe");
        let scratch = registered_at("unplugged", &away);
        let root = scratch.0.as_str();
        assert_eq!(repair_registration(root, &current, &crate::i18n::EN), (Check::OtherCopy(away.clone()), None));
        assert_eq!(registered_exe(root).as_deref(), Some(away.as_path()), "moved off a drive that may come back");
    }

    #[test]
    fn a_registration_changed_while_looking_is_not_overwritten() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("new").join("image-viewer.exe");
        let other = dir.path().join("other").join("image-viewer.exe");
        for f in [&current, &other] {
            std::fs::create_dir_all(f.parent().unwrap()).unwrap();
            std::fs::write(f, b"MZ").unwrap();
        }
        let old = dir.path().join("old").join("image-viewer.exe");
        let stale = Check::Repair { old: old.clone() };

        // another copy registered itself after the look found the old one gone
        let scratch = registered_at("moved-meanwhile", &old);
        let root = scratch.0.as_str();
        write_values(root, &assoc::registration(&other, &TEXTS)).unwrap();
        assert_eq!(
            apply_repair(root, &current, stale.clone(), &crate::i18n::EN),
            (Check::OtherCopy(other.clone()), None)
        );
        assert_eq!(registered_exe(root).as_deref(), Some(other.as_path()));

        // or the user unregistered meanwhile: nothing comes back
        let scratch = registered_at("removed-meanwhile", &old);
        let root = scratch.0.as_str();
        remove(root, &assoc::removal(&assoc::exe_name(&old))).unwrap();
        assert_eq!(apply_repair(root, &current, stale, &crate::i18n::EN), (Check::NotRegistered, None));
        assert!(!exists(root, r"Software\Classes\ImageViewer.Image"), "written again after unregistering");
    }

    #[test]
    fn long_values_are_read_whole() {
        let scratch = Scratch::new("long");
        let long = format!(r#""C:\{}\image-viewer.exe" "%1""#, "x".repeat(2000));
        let v = Value { key: assoc::IMAGE_COMMAND_KEY.into(), name: String::new(), data: long.clone() };
        write_values(&scratch.0, &[v]).unwrap();
        assert_eq!(read_value(&scratch.0, assoc::IMAGE_COMMAND_KEY, "").as_deref(), Some(long.as_str()));
        assert_eq!(read_value(&scratch.0, assoc::IMAGE_COMMAND_KEY, "missing"), None);
    }

    fn handler(root: &str, ext: &str) -> Option<String> {
        read(root, &format!(r"Software\Classes\{}", thumbs::handler_key(ext)), "")
    }

    /// Thumbnails in a scratch key: only the formats no other handler claims are taken, another
    /// program's handler is never overwritten or removed, and unregistering leaves nothing of
    /// the viewer's behind.
    #[test]
    fn thumbnails_take_only_free_formats_and_leave_others_alone() {
        let scratch = Scratch::new("thumbs");
        let root = scratch.0.as_str();
        let other = "{11111111-2222-3333-4444-555555555555}";
        let foreign = Value {
            key: format!(r"Software\Classes\{}", thumbs::handler_key("exr")),
            name: String::new(),
            data: other.into(),
        };
        write_values(root, std::slice::from_ref(&foreign)).unwrap();
        let dll = Path::new(r"C:\Tools\iv\image_viewer_thumbs.dll");
        let taken = |e: &str| ["jpg", "jpeg", "png", "exr"].contains(&e);
        let n = register_thumbnails_at(root, dll, taken).unwrap();
        assert_eq!(n, thumbs::chosen(taken).len());
        assert_eq!(registered_thumbnail_dll(root).as_deref(), Some(dll));
        assert_eq!(read(root, &thumbs::server_key(), "ThreadingModel").as_deref(), Some("Apartment"));
        assert_eq!(handler(root, "tga").as_deref(), Some(thumbs::clsid().as_str()));
        assert_eq!(handler(root, "cbz").as_deref(), Some(thumbs::clsid().as_str()));
        assert_eq!(handler(root, "jpg"), None, "Windows draws it");
        assert_eq!(handler(root, "exr").as_deref(), Some(other), "another program's handler kept");

        // registering again after TGA gained a handler of its own gives it back
        let taken_now = |e: &str| taken(e) || e == "tga";
        assert_eq!(register_thumbnails_at(root, dll, taken_now).unwrap(), n - 1);
        assert_eq!(handler(root, "tga"), None);
        assert!(!exists(root, r"Software\Classes\.tga"), "its emptied keys are pruned");

        unregister_thumbnails_at(root).unwrap();
        assert_eq!(registered_thumbnail_dll(root), None);
        assert!(!exists(root, &thumbs::class_key()));
        for e in thumbs::candidates().iter().filter(|e| *e != "exr") {
            assert!(!exists(root, &format!(r"Software\Classes\.{e}")), ".{e} left behind");
        }
        assert_eq!(handler(root, "exr").as_deref(), Some(other), "another program's handler kept");
        unregister_thumbnails_at(root).unwrap();

        // nothing free: nothing written, not even the class
        assert_eq!(register_thumbnails_at(root, dll, |_| true).unwrap(), 0);
        assert!(!exists(root, &thumbs::class_key()));
    }

    /// Entries a build that drew more formats wrote (RAW and HEIF were candidates at first) are
    /// still the viewer's to remove, by registering again or unregistering.
    #[test]
    fn entries_outside_todays_candidates_are_removed_too() {
        let scratch = Scratch::new("thumbs-older");
        let root = scratch.0.as_str();
        let dll = Path::new(r"C:\Tools\iv\image_viewer_thumbs.dll");
        let older = ["dng", "r3d", "heic"].map(String::from);
        assert!(older.iter().all(|e| !thumbs::candidates().contains(e)));
        write_values(root, &thumbs::registration(dll, &older)).unwrap();
        register_thumbnails_at(root, dll, |_| false).unwrap();
        for e in &older {
            assert_eq!(handler(root, e), None, ".{e} left by registering again");
        }
        write_values(root, &thumbs::registration(dll, &older)).unwrap();
        unregister_thumbnails_at(root).unwrap();
        for e in &older {
            assert!(!exists(root, &format!(r"Software\Classes\.{e}")), ".{e} left by unregistering");
        }
    }

    #[test]
    fn a_thumbnail_dll_that_is_gone_moves_to_the_running_copy() {
        let dir = tempfile::tempdir().unwrap();
        let current = dir.path().join("new").join(thumbs::DLL_NAME);
        std::fs::create_dir_all(current.parent().unwrap()).unwrap();
        std::fs::write(&current, b"MZ").unwrap();
        let old = dir.path().join("old").join(thumbs::DLL_NAME);
        let scratch = Scratch::new("thumbs-repair");
        let root = scratch.0.as_str();
        assert_eq!(thumbnails_check(root, &current), Check::NotRegistered);
        assert_eq!(repair_thumbnails(root, &current), (Check::NotRegistered, None));
        write_values(root, &thumbs::registration(&old, &["tga".to_string()])).unwrap();

        // the running copy has no DLL of its own: nothing to point at
        let missing = dir.path().join("bare").join(thumbs::DLL_NAME);
        assert_eq!(repair_thumbnails(root, &missing), (Check::Repair { old: old.clone() }, None));
        assert_eq!(registered_thumbnail_dll(root).as_deref(), Some(old.as_path()));

        assert_eq!(repair_thumbnails(root, &current), (Check::Repair { old: old.clone() }, Some(Ok(()))));
        assert_eq!(registered_thumbnail_dll(root).as_deref(), Some(current.as_path()));
        assert_eq!(handler(root, "tga").as_deref(), Some(thumbs::clsid().as_str()), "extensions kept");
        assert_eq!(repair_thumbnails(root, &current), (Check::Here, None));

        // another copy's DLL that still exists is left alone
        let other = dir.path().join("other").join(thumbs::DLL_NAME);
        std::fs::create_dir_all(other.parent().unwrap()).unwrap();
        std::fs::write(&other, b"MZ").unwrap();
        write_values(root, &thumbs::registration(&other, &[])).unwrap();
        assert_eq!(repair_thumbnails(root, &current), (Check::OtherCopy(other.clone()), None));
    }

    /// Windows draws JPEG and PNG thumbnails itself (through the `image` perceived type) on
    /// every install; an extension nobody registered has no handler.
    #[test]
    fn windows_own_thumbnail_handlers_are_found() {
        assert!(thumbnail_handler_taken("jpg"));
        assert!(thumbnail_handler_taken("png"));
        assert!(!thumbnail_handler_taken("iv-no-such-extension"));
        // what registering would take on this machine (read only)
        let chosen = thumbs::chosen(thumbnail_handler_taken);
        println!("would draw thumbnails of: {chosen:?}");
        assert!(!chosen.iter().any(|e| e == "jpg" || e == "png"));
    }
}
