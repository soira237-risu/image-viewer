//! The settings window (F2): standard controls on four tabs, editing a copy of the settings.
//! Every change is reported with the whole new value; the app applies what is live and saves on
//! request.
//!
//! The tab control is only the strip of tab titles: each page's controls are children of the
//! window itself, below the strip, and switching tabs shows one page's controls and hides the
//! others. So the controls sit on the dialog background like everywhere else, and Tab moves
//! only through the visible page (hidden controls are skipped) and the buttons at the bottom.

use std::cell::{Cell, RefCell};

use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_BTNFACE, CreateSolidBrush, DeleteObject, HBRUSH, HGDIOBJ};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemServices::{SS_ENDELLIPSIS, SS_NOPREFIX, SS_PATHELLIPSIS, SS_SUNKEN};
use windows::Win32::UI::Controls::{
    NMHDR, TCIF_TEXT, TCITEMW, TCM_GETCURSEL, TCM_GETITEMRECT, TCM_INSERTITEMW, TCM_SETCURSEL, TCM_SETITEMW,
    TCN_SELCHANGE, WC_TABCONTROLW,
};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetFocus, GetKeyState, SetFocus, VK_CONTROL, VK_NEXT, VK_PRIOR, VK_SHIFT, VK_TAB,
};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, PWSTR, w};

use super::controls::{self, Builder};
use super::{UiEvent, dialogs, post_ui, wide};
use crate::assoc::Check;
use crate::i18n::{Language, Strings};
use crate::settings::{Choice, Settings, StartupWindow};
use crate::update::{self, Outcome, State};
use crate::view::ZoomOnOpen;
use iv_pipeline::{AtEnd, SortBy};

const ID_PREFETCH: i32 = 100;
const ID_CACHE: i32 = 101;
const ID_AT_END: i32 = 102;
const ID_RTL: i32 = 103;
const ID_NEAREST: i32 = 104;
const ID_QUALITY: i32 = 105;
const ID_COLOUR: i32 = 106;
const ID_SAVE: i32 = 107;
const ID_DEFAULTS: i32 = 108;
const ID_LANGUAGE: i32 = 109;
const ID_WHEEL: i32 = 110;
const ID_CONFIRM_DELETE: i32 = 111;
const ID_REGISTER: i32 = 112;
const ID_UNREGISTER: i32 = 113;
const ID_DEFAULT_APPS: i32 = 114;
const ID_ZOOM: i32 = 115;
const ID_SORT: i32 = 116;
const ID_DESCENDING: i32 = 117;
const ID_STARTUP: i32 = 118;
const ID_TABS: i32 = 119;
const ID_CHECK_UPDATE: i32 = 120;
const ID_OPEN_RELEASE: i32 = 121;
const ID_CANCEL: i32 = 2;

/// Client size in 96-DPI units.
const WIDTH: i32 = 440;
const HEIGHT: i32 = 384;
/// Where the pages start, below the strip of tab titles.
const TOP: i32 = 50;
/// On the General page, under the "Open with" buttons: where the registration points.
const REGISTRATION_Y: i32 = TOP + 182;
/// The version, then the band for checking for updates: a row of buttons (26) and a status
/// line (18) with the usual gaps.
const VERSION_Y: i32 = TOP + 208;
const UPDATE_Y: i32 = TOP + 234;
const UPDATE_H: i32 = 52;
/// The Save and Defaults buttons at the bottom of every page.
const BUTTONS_Y: i32 = HEIGHT - 38;
// each line (18 high) clears the next, and the update band ends above the buttons
const _: () =
    assert!(REGISTRATION_Y + 18 + 8 <= VERSION_Y && VERSION_Y + 18 + 8 <= UPDATE_Y && UPDATE_Y + UPDATE_H <= BUTTONS_Y);
/// Where the label and value columns start, and how wide they are.
const LABEL_X: i32 = 16;
const LABEL_W: i32 = 200;
const VALUE_X: i32 = 224;
const VALUE_W: i32 = 200;
/// A check box or note spanning both columns.
const ROW_W: i32 = VALUE_X + VALUE_W - LABEL_X;

/// A label in the current language, so controls can be relabelled when it changes.
type Text = fn(&Strings) -> &'static str;
/// The items of a drop-down list in the current language.
type Items = fn(&Strings) -> Vec<&'static str>;

/// The pages, in tab order.
const GENERAL: usize = 0;
const VIEW: usize = 1;
const NAVIGATION: usize = 2;
const SAVING: usize = 3;
const TABS: [Text; 4] = [|s| s.tab_general, |s| s.tab_view, |s| s.tab_navigation, |s| s.tab_saving];

fn zoom_items(s: &Strings) -> Vec<&'static str> {
    ZoomOnOpen::ALL
        .iter()
        .map(|z| match z {
            ZoomOnOpen::Fit => s.zoom_fit,
            ZoomOnOpen::FitEnlarge => s.zoom_fit_enlarge,
            ZoomOnOpen::FitWidth => s.zoom_fit_width,
            ZoomOnOpen::ActualSize => s.zoom_actual_size,
        })
        .collect()
}

fn sort_items(s: &Strings) -> Vec<&'static str> {
    SortBy::ALL
        .iter()
        .map(|b| match b {
            SortBy::Name => s.sort_name,
            SortBy::Modified => s.sort_modified,
            SortBy::Size => s.sort_size,
        })
        .collect()
}

fn at_end_items(s: &Strings) -> Vec<&'static str> {
    AtEnd::ALL
        .iter()
        .map(|a| match a {
            AtEnd::Wrap => s.at_end_wrap,
            AtEnd::Stop => s.at_end_stop,
            AtEnd::NextFolder => s.at_end_next_folder,
        })
        .collect()
}

fn startup_items(s: &Strings) -> Vec<&'static str> {
    StartupWindow::ALL
        .iter()
        .map(|w| match w {
            StartupWindow::Remember => s.startup_remember,
            StartupWindow::Maximized => s.startup_maximized,
            StartupWindow::Fullscreen => s.startup_fullscreen,
        })
        .collect()
}

/// Language names are the same in every language.
fn language_items(_: &Strings) -> Vec<&'static str> {
    Language::ALL.iter().map(|l| l.native_name()).collect()
}

/// The program's version (from Cargo.toml).
fn version_text(s: &Strings) -> String {
    crate::i18n::fill(s.version, &[("version", env!("CARGO_PKG_VERSION"))])
}

/// Where the "Open with" registration points, as the General page says it.
fn registration_text(c: &Check, s: &Strings) -> String {
    match c {
        Check::NotRegistered => s.registration_none.to_string(),
        Check::Here => s.registration_here.to_string(),
        Check::OtherCopy(p) => crate::i18n::fill(s.registration_other, &[("path", &p.display().to_string())]),
        Check::Repair { old } => crate::i18n::fill(s.registration_missing, &[("path", &old.display().to_string())]),
    }
}

/// The tab that Ctrl+Tab (`back`: Ctrl+Shift+Tab) moves to from `current`, round the ends.
pub fn next_tab(current: usize, back: bool) -> usize {
    let n = TABS.len();
    if back { (current + n - 1) % n } else { (current + 1) % n }
}

struct Window {
    hwnd: HWND,
    tabs: HWND,
    language: HWND,
    startup: HWND,
    wheel: HWND,
    confirm_delete: HWND,
    registration: HWND,
    version: HWND,
    check_update: HWND,
    open_release: HWND,
    update_status: HWND,
    zoom: HWND,
    swatch: HWND,
    nearest: HWND,
    rtl: HWND,
    sort: HWND,
    descending: HWND,
    at_end: HWND,
    prefetch: HWND,
    cache: HWND,
    quality: HWND,
    /// Controls showing translated text.
    texts: Vec<(HWND, Text)>,
    /// Drop-down lists whose items are translated.
    combos: Vec<(HWND, Items)>,
    /// Every control of a page, with its page.
    pages: Vec<(HWND, usize)>,
    /// Every control, for laying the window out again on a DPI change.
    layout: controls::Layout,
}

thread_local! {
    static WINDOW: RefCell<Option<Window>> = const { RefCell::new(None) };
    static VALUE: RefCell<Settings> = RefCell::new(Settings::default());
    static SYNCING: Cell<bool> = const { Cell::new(false) };
    static SWATCH_BRUSH: Cell<isize> = const { Cell::new(0) };
    /// The page shown.
    static PAGE: Cell<usize> = const { Cell::new(GENERAL) };
    /// The last check for updates, and where the registration points (`None`: not known yet),
    /// kept for relabelling in another language.
    static UPDATE: RefCell<State> = const { RefCell::new(State::Idle) };
    static REGISTRATION: RefCell<Option<Check>> = const { RefCell::new(None) };
}

pub fn hwnd() -> Option<HWND> {
    WINDOW.with(|w| w.borrow().as_ref().map(|w| w.hwnd))
}

/// Replaces a drop-down list's items with `items`, selecting `selected`.
fn set_items(combo: HWND, items: &[&str], selected: usize) {
    unsafe {
        SendMessageW(combo, CB_RESETCONTENT, None, None);
        for item in items {
            let text = wide(item);
            SendMessageW(combo, CB_ADDSTRING, None, Some(LPARAM(text.as_ptr() as isize)));
        }
        SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(selected)), None);
    }
}

fn selected(combo: HWND) -> Option<usize> {
    usize::try_from(unsafe { SendMessageW(combo, CB_GETCURSEL, None, None).0 }).ok()
}

fn select(combo: HWND, index: usize) {
    unsafe {
        SendMessageW(combo, CB_SETCURSEL, Some(WPARAM(index)), None);
    }
}

fn set_tab_title(tabs: HWND, index: usize, title: &str, insert: bool) {
    let mut text = wide(title);
    let item = TCITEMW { mask: TCIF_TEXT, pszText: PWSTR(text.as_mut_ptr()), ..Default::default() };
    let msg = if insert { TCM_INSERTITEMW } else { TCM_SETITEMW };
    unsafe {
        SendMessageW(tabs, msg, Some(WPARAM(index)), Some(LPARAM(&item as *const TCITEMW as isize)));
    }
}

/// Makes the tab control exactly as tall as its row of titles (plus the line under it), so
/// none of its empty body shows. The titles' height follows the font and so the DPI.
fn fit_tabs(tabs: HWND) {
    unsafe {
        let mut item = RECT::default();
        SendMessageW(tabs, TCM_GETITEMRECT, Some(WPARAM(0)), Some(LPARAM(&mut item as *mut RECT as isize)));
        let mut r = RECT::default();
        let _ = GetWindowRect(tabs, &mut r);
        if item.bottom > 0 {
            let _ = SetWindowPos(
                tabs,
                None,
                0,
                0,
                r.right - r.left,
                item.bottom + 2,
                SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
}

/// The window (hidden) over `owner`, laid out for `dpi` and labelled in `s`'s language.
fn create(owner: HWND, dpi: u32, s: &Strings) -> windows::core::Result<Window> {
    unsafe {
        let instance = GetModuleHandleW(None).expect("module handle");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("iv-settings"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _),
            ..Default::default()
        };
        RegisterClassW(&class);
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let ex = WS_EX_DLGMODALFRAME | WS_EX_CONTROLPARENT;
        let mut r = RECT { left: 0, top: 0, right: WIDTH * dpi as i32 / 96, bottom: HEIGHT * dpi as i32 / 96 };
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, ex, dpi);
        let mut o = RECT::default();
        let _ = GetWindowRect(owner, &mut o);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let title = wide(s.settings);
        let hwnd = CreateWindowExW(
            ex,
            w!("iv-settings"),
            PCWSTR(title.as_ptr()),
            style,
            (o.left + o.right - w) / 2,
            (o.top + o.bottom - h) / 2,
            w,
            h,
            Some(owner),
            None,
            Some(instance.into()),
            None,
        )?;
        let b = Builder::new(hwnd, dpi);
        let texts = RefCell::new(Vec::<(HWND, Text)>::new());
        let combos = RefCell::new(Vec::<(HWND, Items)>::new());
        let pages = RefCell::new(Vec::<(HWND, usize)>::new());
        let page = Cell::new(GENERAL);
        // every control below is on the page set last, unless it is added with `common`
        let on_page = |h: HWND| {
            pages.borrow_mut().push((h, page.get()));
            h
        };
        let keep = |h: HWND, text: Text| {
            texts.borrow_mut().push((h, text));
            h
        };
        let label = |text: Text, y| -> windows::core::Result<HWND> {
            Ok(on_page(keep(b.add(w!("STATIC"), text(s), 0, LABEL_X, y + 3, LABEL_W, 18, 0)?, text)))
        };
        let note = |text: Text, y| -> windows::core::Result<HWND> {
            Ok(on_page(keep(b.add(w!("STATIC"), text(s), 0, LABEL_X, y, ROW_W, 18, 0)?, text)))
        };
        let field = |y, id, number: bool| -> windows::core::Result<HWND> {
            let mut style = ES_AUTOHSCROLL | WS_TABSTOP.0 as i32 | WS_BORDER.0 as i32;
            if number {
                style |= ES_NUMBER;
            }
            Ok(on_page(b.add(w!("EDIT"), "", style as u32, VALUE_X, y, 100, 24, id)?))
        };
        let check = |text: Text, y, id| -> windows::core::Result<HWND> {
            let style = (BS_AUTOCHECKBOX | WS_TABSTOP.0 as i32) as u32;
            Ok(on_page(keep(b.add(w!("BUTTON"), text(s), style, LABEL_X, y, ROW_W, 22, id)?, text)))
        };
        let button = |text: Text, x, y, w, id| -> windows::core::Result<HWND> {
            let style = (BS_PUSHBUTTON | WS_TABSTOP.0 as i32) as u32;
            Ok(keep(b.add(w!("BUTTON"), text(s), style, x, y, w, 26, id)?, text))
        };
        let combo = |y, id, items: Items| -> windows::core::Result<HWND> {
            let style = (CBS_DROPDOWNLIST | WS_TABSTOP.0 as i32 | WS_VSCROLL.0 as i32) as u32;
            // the height is the open list's: room for every item
            let h = on_page(b.add(w!("COMBOBOX"), "", style, VALUE_X, y, VALUE_W, 160, id)?);
            set_items(h, &items(s), 0);
            combos.borrow_mut().push((h, items));
            Ok(h)
        };

        // the strip of tab titles; the pages lie below it
        let tab_style = WS_TABSTOP.0 | WS_CLIPSIBLINGS.0;
        let tabs = b.add(WC_TABCONTROLW, "", tab_style, 8, 8, WIDTH - 16, 30, ID_TABS)?;
        for (i, title) in TABS.iter().enumerate() {
            set_tab_title(tabs, i, title(s), true);
        }
        let top = TOP;

        page.set(GENERAL);
        label(|s| s.language, top)?;
        let language = combo(top, ID_LANGUAGE, language_items)?;
        label(|s| s.startup_window, top + 32)?;
        let startup = combo(top + 32, ID_STARTUP, startup_items)?;
        let wheel = check(|s| s.wheel_navigates, top + 66, ID_WHEEL)?;
        let confirm_delete = check(|s| s.confirm_delete, top + 92, ID_CONFIRM_DELETE)?;
        // registration acts at once (it is not a setting kept in the file)
        note(|s| s.open_with, top + 128)?;
        on_page(button(|s| s.register, LABEL_X, top + 150, 110, ID_REGISTER)?);
        on_page(button(|s| s.unregister, LABEL_X + 116, top + 150, 110, ID_UNREGISTER)?);
        on_page(button(|s| s.default_apps, LABEL_X + 232, top + 150, ROW_W - 232, ID_DEFAULT_APPS)?);
        // a long path keeps its start and its file name
        let path_style = SS_NOPREFIX.0 | SS_PATHELLIPSIS.0;
        let registration = on_page(b.add(w!("STATIC"), "", path_style, LABEL_X, REGISTRATION_Y, ROW_W, 18, 0)?);
        let version = on_page(b.add(w!("STATIC"), &version_text(s), 0, LABEL_X, VERSION_Y, LABEL_W, 18, 0)?);
        // checking for updates happens only when this button is pressed
        let check_update = on_page(button(|s| s.check_updates, LABEL_X, UPDATE_Y, 150, ID_CHECK_UPDATE)?);
        let open_release = on_page(button(|s| s.open_release_page, LABEL_X + 156, UPDATE_Y, 160, ID_OPEN_RELEASE)?);
        let status_style = SS_NOPREFIX.0 | SS_ENDELLIPSIS.0;
        let update_status =
            on_page(b.add(w!("STATIC"), "", status_style, LABEL_X, UPDATE_Y + UPDATE_H - 18, ROW_W, 18, 0)?);

        page.set(VIEW);
        label(|s| s.zoom_on_open, top)?;
        let zoom = combo(top, ID_ZOOM, zoom_items)?;
        label(|s| s.background, top + 34)?;
        let swatch = on_page(b.add(w!("STATIC"), "", SS_SUNKEN.0, VALUE_X, top + 32, 40, 24, 0)?);
        on_page(button(|s| s.choose, VALUE_X + 48, top + 31, 90, ID_COLOUR)?);
        label(|s| s.sharp_pixels, top + 66)?;
        let nearest = field(top + 66, ID_NEAREST, false)?;
        let rtl = check(|s| s.spread_rtl, top + 98, ID_RTL)?;

        page.set(NAVIGATION);
        label(|s| s.sort_by, top)?;
        let sort = combo(top, ID_SORT, sort_items)?;
        let descending = check(|s| s.sort_descending, top + 30, ID_DESCENDING)?;
        note(|s| s.sort_note, top + 56)?;
        label(|s| s.at_end, top + 86)?;
        let at_end = combo(top + 86, ID_AT_END, at_end_items)?;
        label(|s| s.prefetch_count, top + 120)?;
        let prefetch = field(top + 120, ID_PREFETCH, true)?;
        label(|s| s.cache_memory, top + 152)?;
        let cache = field(top + 152, ID_CACHE, true)?;
        note(|s| s.cache_note, top + 184)?;

        page.set(SAVING);
        label(|s| s.save_quality_range, top)?;
        let quality = field(top, ID_QUALITY, true)?;

        // on every page
        button(|s| s.save, VALUE_X + VALUE_W - 188, BUTTONS_Y, 92, ID_SAVE)?;
        button(|s| s.defaults, VALUE_X + VALUE_W - 92, BUTTONS_Y, 92, ID_DEFAULTS)?;

        fit_tabs(tabs);
        let window = Window {
            hwnd,
            tabs,
            language,
            startup,
            wheel,
            confirm_delete,
            registration,
            version,
            check_update,
            open_release,
            update_status,
            zoom,
            swatch,
            nearest,
            rtl,
            sort,
            descending,
            at_end,
            prefetch,
            cache,
            quality,
            texts: texts.into_inner(),
            combos: combos.into_inner(),
            pages: pages.into_inner(),
            layout: b.finish(),
        };
        Ok(window)
    }
}

/// Shows page `index` (and its tab), hiding the others' controls. Focus on a control being
/// hidden moves to the tab strip, so keys never go to a control that cannot be seen.
fn show_page(index: usize) {
    WINDOW.with(|w| {
        let w = w.borrow();
        let Some(w) = w.as_ref() else { return };
        PAGE.with(|p| p.set(index));
        unsafe {
            if usize::try_from(SendMessageW(w.tabs, TCM_GETCURSEL, None, None).0).ok() != Some(index) {
                SendMessageW(w.tabs, TCM_SETCURSEL, Some(WPARAM(index)), None);
            }
            // the page coming up first, so nothing in between is drawn without its controls
            for (h, page) in &w.pages {
                if *page == index {
                    let _ = ShowWindow(*h, SW_SHOWNA);
                }
            }
            let focus = GetFocus();
            for (h, page) in &w.pages {
                if *page != index {
                    let _ = ShowWindow(*h, SW_HIDE);
                    if focus == *h || IsChild(*h, focus).as_bool() {
                        let _ = SetFocus(Some(w.tabs));
                    }
                }
            }
        }
    });
}

/// Ctrl+Tab and Ctrl+PageDown go to the next tab, Ctrl+Shift+Tab and Ctrl+PageUp to the
/// previous one, wherever the focus is in the window. Called for the window's messages before
/// dialog navigation (which would take Ctrl+Tab as a plain Tab); `true` when it was handled.
pub fn pre_translate(msg: &MSG) -> bool {
    if msg.message != WM_KEYDOWN {
        return false;
    }
    let ctrl = unsafe { GetKeyState(i32::from(VK_CONTROL.0)) } < 0;
    let shift = unsafe { GetKeyState(i32::from(VK_SHIFT.0)) } < 0;
    let back = match msg.wParam.0 as u16 {
        k if k == VK_TAB.0 && ctrl => shift,
        k if k == VK_NEXT.0 && ctrl => false,
        k if k == VK_PRIOR.0 && ctrl => true,
        _ => return false,
    };
    show_page(next_tab(PAGE.with(Cell::get), back));
    true
}

fn set_swatch(colour: [u8; 3]) {
    let [r, g, b] = colour.map(u32::from);
    let brush = unsafe { CreateSolidBrush(COLORREF(r | g << 8 | b << 16)) };
    let old = SWATCH_BRUSH.with(|c| c.replace(brush.0 as isize));
    if old != 0 {
        unsafe {
            let _ = DeleteObject(HGDIOBJ(old as *mut _));
        }
    }
    WINDOW.with(|w| {
        if let Some(w) = w.borrow().as_ref() {
            unsafe {
                let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(w.swatch), None, true);
            }
        }
    });
}

/// Puts `s` into the fields without reporting it back.
fn fill(s: &Settings) {
    WINDOW.with(|w| {
        let w = w.borrow();
        let Some(w) = w.as_ref() else { return };
        SYNCING.with(|c| c.set(true));
        controls::set_text(w.prefetch, &s.prefetch_count.to_string());
        controls::set_text(w.cache, &s.cache_mib.to_string());
        controls::set_checked(w.rtl, s.spread_right_to_left);
        controls::set_checked(w.wheel, s.wheel_navigates);
        controls::set_checked(w.confirm_delete, s.confirm_delete);
        controls::set_checked(w.descending, s.sort_descending);
        controls::set_text(w.nearest, &format!("{}", s.nearest_above_zoom));
        controls::set_text(w.quality, &s.save_quality.to_string());
        select(w.language, Language::ALL.iter().position(|l| *l == s.language).unwrap_or(0));
        select(w.zoom, s.zoom_on_open.index());
        select(w.sort, s.sort.index());
        select(w.at_end, s.at_end.index());
        select(w.startup, s.startup_window.index());
        SYNCING.with(|c| c.set(false));
    });
    set_swatch(s.background);
}

/// Shows the registration and the last check for updates in `s`'s language: the status lines,
/// the Check button (off while a check runs) and the release page button (on only when a newer
/// release was found).
fn show_status(s: &Strings) {
    WINDOW.with(|w| {
        let w = w.borrow();
        let Some(w) = w.as_ref() else { return };
        let registration = REGISTRATION.with(|r| r.borrow().as_ref().map(|c| registration_text(c, s)));
        controls::set_text(w.registration, &registration.unwrap_or_default());
        let state = UPDATE.with(|u| u.borrow().clone());
        controls::set_text(w.update_status, &update::status_text(&state, s));
        let checking = state == State::Checking;
        // a button being turned off must not keep the focus, or the keyboard would go nowhere
        let focus = unsafe { GetFocus() };
        let newer = matches!(state, State::Done(Ok(Outcome::Newer { .. })));
        for (button, on) in [(w.check_update, !checking), (w.open_release, newer)] {
            if !on && focus == button {
                unsafe {
                    let _ = SetFocus(Some(w.tabs));
                }
            }
            controls::enable(button, on);
        }
    });
}

/// The state of the check for updates changed.
pub fn set_update(state: &State, s: &Strings) {
    UPDATE.with(|u| *u.borrow_mut() = state.clone());
    show_status(s);
}

/// Where the "Open with" registration points now.
pub fn set_registration(check: Check, s: &Strings) {
    REGISTRATION.with(|r| *r.borrow_mut() = Some(check));
    show_status(s);
}

/// Opens the window on a copy of `s`.
pub fn show(owner: HWND, s: &Settings, activate: bool) -> Result<(), String> {
    if hwnd().is_none() {
        let t = s.language.strings();
        let dpi = unsafe { GetDpiForWindow(owner) }.max(96);
        let w = create(owner, dpi, t)
            .map_err(|e| crate::i18n::fill(t.settings_window_error, &[("error", &e.to_string())]))?;
        WINDOW.with(|c| *c.borrow_mut() = Some(w));
        show_page(GENERAL);
        show_status(t);
    }
    VALUE.with(|v| *v.borrow_mut() = s.clone());
    fill(s);
    if let Some(h) = hwnd() {
        unsafe {
            let _ = ShowWindow(h, if activate { SW_SHOW } else { SW_SHOWNOACTIVATE });
        }
    }
    Ok(())
}

/// The save quality was changed elsewhere (the edit panel's slider): the window's copy and its
/// field follow, without reporting it back, so its next change or Save does not bring back the
/// old quality.
pub fn set_quality(quality: u8) {
    VALUE.with(|v| v.borrow_mut().save_quality = quality);
    WINDOW.with(|w| {
        let w = w.borrow();
        let Some(w) = w.as_ref() else { return };
        SYNCING.with(|c| c.set(true));
        controls::set_text(w.quality, &quality.to_string());
        SYNCING.with(|c| c.set(false));
    });
}

/// Relabels the window (if it exists) in another language: title, tabs, labels and the items
/// of the drop-down lists (keeping what is chosen).
pub fn set_language(s: &Strings) {
    WINDOW.with(|w| {
        let w = w.borrow();
        let Some(w) = w.as_ref() else { return };
        SYNCING.with(|c| c.set(true));
        controls::set_text(w.hwnd, s.settings);
        for (i, title) in TABS.iter().enumerate() {
            set_tab_title(w.tabs, i, title(s), false);
        }
        for (h, text) in &w.texts {
            controls::set_text(*h, text(s));
        }
        for (h, items) in &w.combos {
            set_items(*h, &items(s), selected(*h).unwrap_or(0));
        }
        controls::set_text(w.version, &version_text(s));
        SYNCING.with(|c| c.set(false));
        unsafe {
            let _ = windows::Win32::Graphics::Gdi::InvalidateRect(Some(w.tabs), None, true);
        }
    });
    show_status(s);
}

pub fn hide() {
    if let Some(h) = hwnd() {
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
}

/// The value chosen in a drop-down list of `T`s, or `prev` when nothing is.
fn chosen<T: Choice>(combo: HWND, prev: T) -> T {
    selected(combo).and_then(|i| T::ALL.get(i)).copied().unwrap_or(prev)
}

/// The fields as settings; a field that does not parse keeps its previous value.
fn read(prev: &Settings) -> Settings {
    WINDOW.with(|w| {
        let w = w.borrow();
        let Some(w) = w.as_ref() else { return prev.clone() };
        let mut s = prev.clone();
        let num = |h: HWND| controls::text(h).trim().parse::<u64>().ok();
        if let Some(v) = num(w.prefetch) {
            s.prefetch_count = v.min(1 << 20) as usize;
        }
        if let Some(v) = num(w.cache) {
            s.cache_mib = v.min(u32::MAX as u64) as u32;
        }
        if let Some(v) = num(w.quality) {
            s.save_quality = v.min(255) as u8;
        }
        if let Ok(v) = controls::text(w.nearest).trim().parse::<f32>() {
            s.nearest_above_zoom = v;
        }
        s.spread_right_to_left = controls::checked(w.rtl);
        s.wheel_navigates = controls::checked(w.wheel);
        s.confirm_delete = controls::checked(w.confirm_delete);
        s.sort_descending = controls::checked(w.descending);
        if let Some(l) = selected(w.language).and_then(|i| Language::ALL.get(i)) {
            s.language = *l;
        }
        s.zoom_on_open = chosen(w.zoom, prev.zoom_on_open);
        s.sort = chosen(w.sort, prev.sort);
        s.at_end = chosen(w.at_end, prev.at_end);
        s.startup_window = chosen(w.startup, prev.startup_window);
        s
    })
}

/// Reports `s`. The app uses it clamped into range, so the window keeps the clamped value too.
fn changed(s: Settings) {
    let s = s.clamped();
    VALUE.with(|v| *v.borrow_mut() = s.clone());
    post_ui(UiEvent::SettingsChanged(s));
}

/// When the user leaves a number field: shows the value in effect if the field says otherwise
/// (a number out of range was clamped, text that is not a number was ignored), so the window
/// never shows a value the app is not using. Not while typing: "1024" passes through "1".
fn refill_field(id: i32) {
    let v = VALUE.with(|v| v.borrow().clone());
    WINDOW.with(|w| {
        let w = w.borrow();
        let Some(w) = w.as_ref() else { return };
        let int =
            |h: HWND, value: u64| (h, controls::text(h).trim().parse::<u64>().ok() == Some(value), value.to_string());
        let (field, shows_value, text) = match id {
            ID_PREFETCH => int(w.prefetch, v.prefetch_count as u64),
            ID_CACHE => int(w.cache, u64::from(v.cache_mib)),
            ID_QUALITY => int(w.quality, u64::from(v.save_quality)),
            ID_NEAREST => {
                let zoom = v.nearest_above_zoom;
                let same = controls::text(w.nearest).trim().parse::<f32>().ok() == Some(zoom);
                (w.nearest, same, format!("{zoom}"))
            }
            _ => return,
        };
        if !shows_value {
            SYNCING.with(|c| c.set(true));
            controls::set_text(field, &text);
            SYNCING.with(|c| c.set(false));
        }
    });
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_COMMAND if !SYNCING.with(Cell::get) => {
            let (id, code) = ((wp.0 & 0xffff) as i32, ((wp.0 >> 16) & 0xffff) as u32);
            let current = VALUE.with(|v| v.borrow().clone());
            match (id, code) {
                (ID_PREFETCH | ID_CACHE | ID_NEAREST | ID_QUALITY, EN_CHANGE)
                | (ID_RTL | ID_WHEEL | ID_CONFIRM_DELETE | ID_DESCENDING, BN_CLICKED)
                | (ID_LANGUAGE | ID_ZOOM | ID_SORT | ID_AT_END | ID_STARTUP, CBN_SELCHANGE) => {
                    changed(read(&current));
                }
                (ID_COLOUR, BN_CLICKED) => {
                    if let Some(c) = dialogs::choose_color(hwnd, current.background) {
                        set_swatch(c);
                        let mut s = current;
                        s.background = c;
                        changed(s);
                    }
                }
                (ID_DEFAULTS, BN_CLICKED) => {
                    let d = Settings::default();
                    fill(&d);
                    changed(d);
                }
                (ID_PREFETCH | ID_CACHE | ID_NEAREST | ID_QUALITY, EN_KILLFOCUS) => refill_field(id),
                (ID_SAVE, BN_CLICKED) => {
                    // what is written is what the window shows
                    fill(&current);
                    post_ui(UiEvent::SettingsSave);
                }
                (ID_REGISTER, BN_CLICKED) => post_ui(UiEvent::Register(true)),
                (ID_UNREGISTER, BN_CLICKED) => post_ui(UiEvent::Register(false)),
                (ID_DEFAULT_APPS, BN_CLICKED) => post_ui(UiEvent::OpenDefaultApps),
                (ID_CHECK_UPDATE, BN_CLICKED) => post_ui(UiEvent::CheckUpdate),
                (ID_OPEN_RELEASE, BN_CLICKED) => post_ui(UiEvent::OpenReleasePage),
                (ID_CANCEL, _) => post_ui(UiEvent::SettingsClosed),
                _ => {}
            }
            LRESULT(0)
        }
        WM_NOTIFY => {
            let header = unsafe { &*(lp.0 as *const NMHDR) };
            if header.idFrom == ID_TABS as usize && header.code == TCN_SELCHANGE {
                let tabs = header.hwndFrom;
                if let Ok(index) = usize::try_from(unsafe { SendMessageW(tabs, TCM_GETCURSEL, None, None).0 }) {
                    show_page(index);
                }
            }
            LRESULT(0)
        }
        WM_CTLCOLORSTATIC => {
            let is_swatch = WINDOW.with(|w| w.borrow().as_ref().is_some_and(|w| w.swatch.0 as isize == lp.0));
            let brush = SWATCH_BRUSH.with(Cell::get);
            if is_swatch && brush != 0 {
                return LRESULT(brush);
            }
            unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
        }
        WM_CLOSE => {
            post_ui(UiEvent::SettingsClosed);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            WINDOW.with(|w| {
                if let Some(w) = w.borrow().as_ref() {
                    SYNCING.with(|c| c.set(true));
                    w.layout.dpi_changed(hwnd, wp, lp);
                    fit_tabs(w.tabs);
                    SYNCING.with(|c| c.set(false));
                }
            });
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::Controls::TCM_GETROWCOUNT;

    #[test]
    fn ctrl_tab_goes_round_the_tabs() {
        assert_eq!(next_tab(GENERAL, false), VIEW);
        assert_eq!(next_tab(VIEW, false), NAVIGATION);
        assert_eq!(next_tab(NAVIGATION, false), SAVING);
        assert_eq!(next_tab(SAVING, false), GENERAL);
        assert_eq!(next_tab(GENERAL, true), SAVING);
        assert_eq!(next_tab(SAVING, true), NAVIGATION);
    }

    /// A control of a laid-out window, in the window's client pixels.
    struct Placed {
        hwnd: HWND,
        class: String,
        style: u32,
        /// `None`: on every page.
        page: Option<usize>,
        rect: RECT,
        /// Its text, or a drop-down list's items.
        texts: Vec<String>,
    }

    fn wide_str(buf: &[u16], len: i32) -> String {
        String::from_utf16_lossy(&buf[..usize::try_from(len).unwrap_or(0).min(buf.len())])
    }

    fn placed(w: &Window) -> Vec<Placed> {
        let mut out = Vec::new();
        unsafe {
            let mut next = GetWindow(w.hwnd, GW_CHILD).ok();
            while let Some(h) = next {
                let mut buf = [0u16; 256];
                let len = GetClassNameW(h, &mut buf);
                let class = wide_str(&buf, len);
                let mut r = RECT::default();
                GetWindowRect(h, &mut r).unwrap();
                let mut corners = [POINT { x: r.left, y: r.top }, POINT { x: r.right, y: r.bottom }];
                for p in &mut corners {
                    assert!(ScreenToClient(w.hwnd, p).as_bool());
                }
                let rect = RECT { left: corners[0].x, top: corners[0].y, right: corners[1].x, bottom: corners[1].y };
                let texts = if class.eq_ignore_ascii_case("ComboBox") {
                    let n = SendMessageW(h, CB_GETCOUNT, None, None).0;
                    (0..n)
                        .map(|i| {
                            let len = SendMessageW(
                                h,
                                CB_GETLBTEXT,
                                Some(WPARAM(i as usize)),
                                Some(LPARAM(buf.as_mut_ptr() as isize)),
                            );
                            wide_str(&buf, len.0 as i32)
                        })
                        .collect()
                } else {
                    let len = GetWindowTextW(h, &mut buf);
                    vec![wide_str(&buf, len)]
                };
                let page = w.pages.iter().find(|(c, _)| *c == h).map(|(_, p)| *p);
                let style = GetWindowLongW(h, GWL_STYLE) as u32;
                out.push(Placed { hwnd: h, class, style, page, rect, texts });
                next = GetWindow(h, GW_HWNDNEXT).ok();
            }
        }
        out
    }

    /// The size of `text` on one line in `control`'s font.
    fn text_size(control: HWND, text: &str) -> (i32, i32) {
        use windows::Win32::Graphics::Gdi::{DT_CALCRECT, DT_SINGLELINE, DrawTextW, GetDC, ReleaseDC, SelectObject};
        unsafe {
            let font = SendMessageW(control, WM_GETFONT, None, None).0;
            let dc = GetDC(Some(control));
            let old = SelectObject(dc, HGDIOBJ(font as *mut _));
            let mut chars: Vec<u16> = text.encode_utf16().collect();
            let mut r = RECT::default();
            DrawTextW(dc, &mut chars, &mut r, DT_CALCRECT | DT_SINGLELINE);
            SelectObject(dc, old);
            ReleaseDC(Some(control), dc);
            (r.right - r.left, r.bottom - r.top)
        }
    }

    /// Every kind of result a check can show, with long but realistic numbers.
    fn status_samples() -> Vec<State> {
        use crate::update::Failure;
        let mut out = vec![
            State::Checking,
            State::Done(Ok(Outcome::UpToDate { latest: "v0.1.0".into() })),
            State::Done(Ok(Outcome::Newer { latest: "v10.20.30-beta.2".into(), page: String::new() })),
        ];
        out.extend(
            [
                Failure::Offline,
                Failure::Timeout,
                Failure::Tls,
                Failure::Network(12030),
                Failure::RateLimited(403),
                Failure::Http(503),
                Failure::TooLarge,
                Failure::Unreadable,
            ]
            .map(|f| State::Done(Err(f))),
        );
        out
    }

    /// The status follows the check: the button is off while it runs, and the release page
    /// button is on only when a newer release was found. Built hidden, never shown.
    #[test]
    fn buttons_follow_the_check() {
        use windows::Win32::UI::Input::KeyboardAndMouse::IsWindowEnabled;
        controls::init_common_controls();
        let w = create(HWND::default(), 96, &crate::i18n::EN).expect("settings window");
        let (check, release, status) = (w.check_update, w.open_release, w.update_status);
        WINDOW.with(|c| *c.borrow_mut() = Some(w));
        let enabled = |h: HWND| unsafe { IsWindowEnabled(h).as_bool() };
        let s = &crate::i18n::EN;
        set_update(&State::Idle, s);
        assert!(enabled(check) && !enabled(release));
        assert_eq!(controls::text(status), "");
        set_update(&State::Checking, s);
        assert!(!enabled(check) && !enabled(release));
        assert_eq!(controls::text(status), "Asking GitHub…");
        let newer = State::Done(Ok(Outcome::Newer { latest: "v9.0.0".into(), page: "https://x".into() }));
        set_update(&newer, s);
        assert!(enabled(check) && enabled(release));
        assert!(controls::text(status).starts_with("New version available: v9.0.0"));
        set_update(&State::Done(Err(crate::update::Failure::Timeout)), s);
        assert!(enabled(check) && !enabled(release));
        assert_eq!(controls::text(status), "Check failed: no answer in time");
        // relabelled with the language
        set_language(&crate::i18n::KO);
        assert_eq!(controls::text(status), "확인하지 못함: 응답 시간 초과");
        set_registration(Check::OtherCopy(r"D:\old\image-viewer.exe".into()), &crate::i18n::KO);
        let reg = WINDOW.with(|c| c.borrow().as_ref().unwrap().registration);
        assert_eq!(controls::text(reg), r"다른 사본에 등록됨: D:\old\image-viewer.exe");
        let hwnd = WINDOW.with(|c| c.borrow_mut().take().unwrap().hwnd);
        unsafe { DestroyWindow(hwnd).unwrap() };
    }

    fn overlap(a: &RECT, b: &RECT) -> bool {
        a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
    }

    /// The window as it is built at 100, 125, 150 and 200 % in both languages (hidden, never
    /// shown): every label, check box, button, list item and tab title fits on one line in its
    /// control, controls on one page do not overlap, everything is inside the window, and the
    /// band for checking for updates is free.
    #[test]
    fn every_page_fits_at_every_dpi_in_both_languages() {
        use windows::Win32::UI::HiDpi::GetSystemMetricsForDpi;
        controls::init_common_controls();
        for dpi in [96, 120, 144, 192] {
            let px = |v: i32| controls::scale(v, dpi);
            for s in [&crate::i18n::KO, &crate::i18n::EN] {
                let w = create(HWND::default(), dpi, s).expect("settings window");
                let at = format!("{} at {dpi} DPI", s.tab_general);
                // the client area the window gets at `dpi` (the test thread is not DPI aware, so
                // the frame Windows actually draws here is the 96-DPI one)
                let client = RECT { left: 0, top: 0, right: px(WIDTH), bottom: px(HEIGHT) };
                let all = placed(&w);
                assert!(all.len() > 30, "{at}: {} controls", all.len());
                let check_w = unsafe { GetSystemMetricsForDpi(SM_CXMENUCHECK, dpi) };
                let arrow_w = unsafe { GetSystemMetricsForDpi(SM_CXVSCROLL, dpi) };
                for c in &all {
                    let r = &c.rect;
                    let (cw, ch) = (r.right - r.left, r.bottom - r.top);
                    assert!(
                        r.left >= 0 && r.top >= 0 && r.right <= client.right && r.bottom <= client.bottom,
                        "{at}: {} {:?} outside the window: {r:?}",
                        c.class,
                        c.texts
                    );
                    // the room a control leaves its text
                    let (room, kind) = match c.class.to_ascii_lowercase().as_str() {
                        "static" => (cw, "label"),
                        "button" if c.style & 0xf == BS_AUTOCHECKBOX as u32 => (cw - check_w - px(6), "check box"),
                        "button" => (cw - px(10), "button"),
                        "combobox" => (cw - arrow_w - px(8), "list item"),
                        _ => continue,
                    };
                    for text in c.texts.iter().filter(|t| !t.is_empty()) {
                        let (tw, th) = text_size(c.hwnd, text);
                        assert!(tw <= room, "{at}: {kind} {text:?} is {tw} px wide in {room} px");
                        assert!(th <= ch, "{at}: {kind} {text:?} is {th} px tall in {ch} px");
                    }
                }
                // the tab titles: one row, inside the strip
                unsafe {
                    assert_eq!(SendMessageW(w.tabs, TCM_GETROWCOUNT, None, None).0, 1, "{at}");
                    let mut last = RECT::default();
                    let n = TABS.len() - 1;
                    SendMessageW(
                        w.tabs,
                        TCM_GETITEMRECT,
                        Some(WPARAM(n)),
                        Some(LPARAM(&mut last as *mut RECT as isize)),
                    );
                    let mut strip = RECT::default();
                    GetClientRect(w.tabs, &mut strip).unwrap();
                    assert!(last.right > 0 && last.right <= strip.right, "{at}: tab titles end at {}", last.right);
                }
                // nothing on a page overlaps another control shown with it
                for (i, a) in all.iter().enumerate() {
                    for b in &all[i + 1..] {
                        let together = a.page.is_none() || b.page.is_none() || a.page == b.page;
                        assert!(
                            !(together && overlap(&a.rect, &b.rect)),
                            "{at}: {:?} {:?} overlaps {:?} {:?}",
                            a.texts,
                            a.rect,
                            b.texts,
                            b.rect
                        );
                    }
                }
                // the band for updates holds the two buttons and the status line, and nothing else
                let band = RECT { left: 0, top: px(UPDATE_Y), right: client.right, bottom: px(UPDATE_Y + UPDATE_H) };
                let update_controls = [w.check_update, w.open_release, w.update_status];
                for c in all.iter().filter(|c| matches!(c.page, None | Some(GENERAL))) {
                    let inside = c.rect.top >= band.top && c.rect.bottom <= band.bottom;
                    if update_controls.contains(&c.hwnd) {
                        assert!(inside, "{at}: {:?} {:?} outside the band {band:?}", c.texts, c.rect);
                    } else {
                        assert!(!overlap(&c.rect, &band), "{at}: {:?} is in the band for updates", c.texts);
                    }
                }
                // every status line the two lines can show fits on one line
                let fits = |control: HWND, text: &str| {
                    let mut r = RECT::default();
                    unsafe { GetClientRect(control, &mut r).unwrap() };
                    let (tw, th) = text_size(control, text);
                    assert!(tw <= r.right && th <= r.bottom, "{at}: {text:?} is {tw}x{th} px in {r:?}");
                };
                for state in status_samples() {
                    fits(w.update_status, &update::status_text(&state, s));
                }
                let short = std::path::PathBuf::from(r"D:\iv\image-viewer.exe");
                for c in
                    [Check::NotRegistered, Check::Here, Check::OtherCopy(short.clone()), Check::Repair { old: short }]
                {
                    fits(w.registration, &registration_text(&c, s));
                }
                unsafe { DestroyWindow(w.hwnd).unwrap() };
            }
        }
    }

    #[test]
    fn every_choice_has_a_label_in_both_languages() {
        for s in [&crate::i18n::KO, &crate::i18n::EN] {
            assert_eq!(zoom_items(s).len(), ZoomOnOpen::ALL.len());
            assert_eq!(sort_items(s).len(), SortBy::ALL.len());
            assert_eq!(at_end_items(s).len(), AtEnd::ALL.len());
            assert_eq!(startup_items(s).len(), StartupWindow::ALL.len());
            for items in [zoom_items(s), sort_items(s), at_end_items(s), startup_items(s)] {
                let unique: std::collections::HashSet<_> = items.iter().collect();
                assert_eq!(unique.len(), items.len(), "{items:?}");
            }
        }
    }
}
