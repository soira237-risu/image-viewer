//! Windows' own dialogs: file open/save (`IFileDialog`), questions and messages, and the colour
//! picker.

use std::cell::RefCell;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{COLORREF, HWND};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::UI::Controls::Dialogs::{CC_FULLOPEN, CC_RGBINIT, CHOOSECOLORW, ChooseColorW};
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::{
    FileOpenDialog, FileSaveDialog, IFileOpenDialog, IFileSaveDialog, IShellItem, SHCreateItemFromParsingName,
    SIGDN_FILESYSPATH,
};
use windows::Win32::UI::WindowsAndMessaging::{
    IDNO, IDYES, MB_ICONERROR, MB_ICONINFORMATION, MB_ICONWARNING, MB_OK, MB_YESNO, MB_YESNOCANCEL, MESSAGEBOX_STYLE,
    MessageBoxW,
};
use windows::core::PCWSTR;

use super::wide;

/// A file type filter: display name and patterns such as `"*.jpg;*.jpeg"`.
pub struct Filter {
    pub name: String,
    pub patterns: String,
}

/// Owns the UTF-16 strings a `COMDLG_FILTERSPEC` array points into.
struct Specs {
    _strings: Vec<(Vec<u16>, Vec<u16>)>,
    specs: Vec<COMDLG_FILTERSPEC>,
}

fn specs(filters: &[Filter]) -> Specs {
    let strings: Vec<(Vec<u16>, Vec<u16>)> = filters.iter().map(|f| (wide(&f.name), wide(&f.patterns))).collect();
    let specs = strings
        .iter()
        .map(|(n, p)| COMDLG_FILTERSPEC { pszName: PCWSTR(n.as_ptr()), pszSpec: PCWSTR(p.as_ptr()) })
        .collect();
    Specs { _strings: strings, specs }
}

fn item_path(item: &IShellItem) -> Option<PathBuf> {
    unsafe {
        let p = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

/// The Open dialog. `None` when cancelled.
pub fn open_file(owner: HWND, filters: &[Filter]) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let s = specs(filters);
        dlg.SetFileTypes(&s.specs).ok()?;
        dlg.Show(Some(owner)).ok()?;
        item_path(&dlg.GetResult().ok()?)
    }
}

/// The Save As dialog, starting in `dir` with `file_name` and filter `type_index` (0-based)
/// selected; a name typed without an extension gets the selected type's.
pub fn save_file(
    owner: HWND,
    filters: &[Filter],
    dir: Option<&Path>,
    file_name: &str,
    type_index: usize,
    default_ext: &str,
) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let s = specs(filters);
        dlg.SetFileTypes(&s.specs).ok()?;
        dlg.SetFileTypeIndex(type_index as u32 + 1).ok()?;
        let ext = wide(default_ext);
        dlg.SetDefaultExtension(PCWSTR(ext.as_ptr())).ok()?;
        let name = wide(file_name);
        dlg.SetFileName(PCWSTR(name.as_ptr())).ok()?;
        if let Some(dir) = dir {
            let d = wide(&dir.to_string_lossy());
            if let Ok(folder) = SHCreateItemFromParsingName::<_, _, IShellItem>(PCWSTR(d.as_ptr()), None) {
                let _ = dlg.SetFolder(&folder);
            }
        }
        dlg.Show(Some(owner)).ok()?;
        item_path(&dlg.GetResult().ok()?)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Answer {
    Yes,
    No,
    Cancel,
}

fn message_box(
    owner: HWND,
    text: &str,
    style: MESSAGEBOX_STYLE,
) -> windows::Win32::UI::WindowsAndMessaging::MESSAGEBOX_RESULT {
    let (t, c) = (wide(text), wide("image-viewer"));
    unsafe { MessageBoxW(Some(owner), PCWSTR(t.as_ptr()), PCWSTR(c.as_ptr()), style) }
}

pub fn ask_yes_no_cancel(owner: HWND, text: &str) -> Answer {
    match message_box(owner, text, MB_YESNOCANCEL | MB_ICONWARNING) {
        IDYES => Answer::Yes,
        IDNO => Answer::No,
        _ => Answer::Cancel,
    }
}

/// A yes/no question with Yes as the default button; `true` for Yes.
pub fn ask_yes_no(owner: HWND, text: &str) -> bool {
    message_box(owner, text, MB_YESNO | MB_ICONWARNING) == IDYES
}

pub fn show_error(owner: HWND, text: &str) {
    message_box(owner, text, MB_OK | MB_ICONERROR);
}

pub fn show_info(owner: HWND, text: &str) {
    message_box(owner, text, MB_OK | MB_ICONINFORMATION);
}

/// An error box with no owner window, shown from a thread of its own and waited for: the
/// calling thread's message loop is not run meanwhile. For a crash, where the window's state is
/// mid-panic and must not see another message, and for failures before there is a window.
pub fn show_fatal(title: &str, text: &str) {
    let (t, c) = (wide(text), wide(title));
    let shown = std::thread::Builder::new().name("error-box".into()).spawn(move || unsafe {
        MessageBoxW(None, PCWSTR(t.as_ptr()), PCWSTR(c.as_ptr()), MB_OK | MB_ICONERROR);
    });
    if let Ok(thread) = shown {
        let _ = thread.join();
    }
}

thread_local! {
    static CUSTOM_COLOURS: RefCell<[COLORREF; 16]> = const { RefCell::new([COLORREF(0x00ff_ffff); 16]) };
}

/// The colour picker, starting at `initial` (sRGB). `None` when cancelled.
pub fn choose_color(owner: HWND, initial: [u8; 3]) -> Option<[u8; 3]> {
    CUSTOM_COLOURS.with(|custom| {
        let mut custom = custom.borrow_mut();
        let [r, g, b] = initial.map(u32::from);
        let mut cc = CHOOSECOLORW {
            lStructSize: size_of::<CHOOSECOLORW>() as u32,
            hwndOwner: owner,
            rgbResult: COLORREF(r | g << 8 | b << 16),
            lpCustColors: custom.as_mut_ptr(),
            Flags: CC_RGBINIT | CC_FULLOPEN,
            ..Default::default()
        };
        let ok = unsafe { ChooseColorW(&mut cc) }.as_bool();
        let c = cc.rgbResult.0;
        ok.then_some([c as u8, (c >> 8) as u8, (c >> 16) as u8])
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx};
    use windows::Win32::UI::Shell::FOS_NOCHANGEDIR;

    /// Relative paths (a file named on the command line) stay valid after a file dialog: unlike
    /// the old `GetOpenFileName`, the item dialogs keep the current folder unless told otherwise.
    #[test]
    fn file_dialogs_never_change_the_current_folder() {
        unsafe {
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let open: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).unwrap();
            let save: IFileSaveDialog = CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).unwrap();
            for dlg in [&*open, &*save] {
                assert!(dlg.GetOptions().unwrap().contains(FOS_NOCHANGEDIR));
            }
        }
    }
}
