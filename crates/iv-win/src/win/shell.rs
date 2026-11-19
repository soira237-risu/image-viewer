//! The Windows shell around the current file: the clipboard, the Recycle Bin, Explorer, Settings
//! and the per-user "Open with" registration. The data each needs is built (and tested) in
//! [`crate::clipboard`] and [`crate::assoc`]; this module only hands it to Windows.

use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_MORE_DATA, ERROR_SUCCESS, GlobalFree, HANDLE, HGLOBAL, HWND, WIN32_ERROR,
};
use windows::Win32::System::DataExchange::{
    CloseClipboard, EmptyClipboard, OpenClipboard, RegisterClipboardFormatW, SetClipboardData,
};
use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_READ, REG_SZ, RRF_RT_REG_SZ, RegCloseKey, RegDeleteKeyValueW, RegDeleteKeyW,
    RegDeleteTreeW, RegGetValueW, RegOpenKeyExW, RegQueryInfoKeyW, RegSetKeyValueW,
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
    let key = under(root, key);
    let name = wide(name);
    let mut buf = vec![0u16; 512];
    // the value may grow between asking its size and reading it: try again a few times
    for _ in 0..4 {
        let mut bytes = (buf.len() * 2) as u32;
        let e = unsafe {
            RegGetValueW(
                HKEY_CURRENT_USER,
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
}
