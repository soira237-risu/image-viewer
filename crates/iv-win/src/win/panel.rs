//! The edit panel (E): a small tool window of standard controls beside the main window. It
//! reports what the user did through [`super::post_ui`]; the app applies it and calls [`sync`]
//! to show the result.

use std::cell::{Cell, RefCell};

use iv_ops::{Adjust, Op};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{COLOR_BTNFACE, HBRUSH};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemServices::SS_RIGHT;
use windows::Win32::UI::Controls::{TBS_NOTICKS, TRACKBAR_CLASSW};
use windows::Win32::UI::HiDpi::{AdjustWindowRectExForDpi, GetDpiForWindow};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{PCWSTR, w};

use super::controls::{self, Builder};
use super::{UiEvent, post_ui, wide};
use crate::i18n::{Strings, fill};
use crate::panel::{
    PanelAction, PanelContext, PanelState, can_apply, field_value, gamma_from_position, gamma_to_position,
    unit_from_position, unit_to_position,
};

const ID_ROTATE_LEFT: i32 = 100;
const ID_ROTATE_RIGHT: i32 = 101;
const ID_FLIP_H: i32 = 102;
const ID_FLIP_V: i32 = 103;
const ID_RESET: i32 = 104;
const ID_WIDTH: i32 = 105;
const ID_HEIGHT: i32 = 106;
const ID_KEEP_ASPECT: i32 = 107;
const ID_HALF: i32 = 108;
const ID_APPLY: i32 = 109;
const ID_UNDO: i32 = 110;
const ID_REDO: i32 = 111;
const ID_SAVE: i32 = 112;
const ID_SAVE_AS: i32 = 113;
/// Esc in a window driven by `IsDialogMessageW`.
const ID_CANCEL: i32 = 2;

/// Client size in 96-DPI units.
const WIDTH: i32 = 260;
const HEIGHT: i32 = 556;

/// A label in the current language, so controls can be relabelled when it changes.
type Text = fn(&Strings) -> &'static str;

struct Panel {
    hwnd: HWND,
    /// brightness, contrast, saturation, gamma
    sliders: [HWND; 4],
    values: [HWND; 4],
    width: HWND,
    height: HWND,
    keep_aspect: HWND,
    apply: HWND,
    half: HWND,
    undo: HWND,
    redo: HWND,
    save: HWND,
    quality: HWND,
    quality_value: HWND,
    edit_buttons: [HWND; 5],
    /// Controls showing translated text.
    texts: Vec<(HWND, Text)>,
    /// Every control, for laying the panel out again on a DPI change.
    layout: controls::Layout,
}

thread_local! {
    static PANEL: RefCell<Option<Panel>> = const { RefCell::new(None) };
    /// Set while the app moves controls, so the notifications that causes are ignored.
    static SYNCING: Cell<bool> = const { Cell::new(false) };
}

pub fn hwnd() -> Option<HWND> {
    PANEL.with(|p| p.borrow().as_ref().map(|p| p.hwnd))
}

fn create(owner: HWND, s: &Strings) -> windows::core::Result<Panel> {
    unsafe {
        let instance = GetModuleHandleW(None).expect("module handle");
        let class = WNDCLASSW {
            lpfnWndProc: Some(wndproc),
            hInstance: instance.into(),
            lpszClassName: w!("iv-edit-panel"),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hbrBackground: HBRUSH((COLOR_BTNFACE.0 + 1) as usize as *mut _),
            ..Default::default()
        };
        RegisterClassW(&class);
        let dpi = GetDpiForWindow(owner).max(96);
        let style = WS_POPUP | WS_CAPTION | WS_SYSMENU;
        let ex = WS_EX_TOOLWINDOW | WS_EX_CONTROLPARENT;
        let mut r = RECT { left: 0, top: 0, right: WIDTH * dpi as i32 / 96, bottom: HEIGHT * dpi as i32 / 96 };
        let _ = AdjustWindowRectExForDpi(&mut r, style, false, ex, dpi);
        let mut owner_rect = RECT::default();
        let _ = GetWindowRect(owner, &mut owner_rect);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let title = wide(s.panel_title);
        let hwnd = CreateWindowExW(
            ex,
            w!("iv-edit-panel"),
            PCWSTR(title.as_ptr()),
            style,
            owner_rect.right - w - 24 * dpi as i32 / 96,
            owner_rect.top + 64 * dpi as i32 / 96,
            w,
            h,
            Some(owner),
            None,
            Some(instance.into()),
            None,
        )?;

        let b = Builder::new(hwnd, dpi);
        let texts = RefCell::new(Vec::<(HWND, Text)>::new());
        let button = |text: Text, x, y, w, id| -> windows::core::Result<HWND> {
            let h = b.add(w!("BUTTON"), text(s), (BS_PUSHBUTTON | WS_TABSTOP.0 as i32) as u32, x, y, w, 26, id)?;
            texts.borrow_mut().push((h, text));
            Ok(h)
        };
        let label = |text: Text, x, y, w| -> windows::core::Result<HWND> {
            let h = b.add(w!("STATIC"), text(s), 0, x, y, w, 18, 0)?;
            texts.borrow_mut().push((h, text));
            Ok(h)
        };
        let edit_buttons = [
            button(|s| s.rotate_ccw, 10, 10, 115, ID_ROTATE_LEFT)?,
            button(|s| s.rotate_cw, 135, 10, 115, ID_ROTATE_RIGHT)?,
            button(|s| s.flip_h, 10, 40, 115, ID_FLIP_H)?,
            button(|s| s.flip_v, 135, 40, 115, ID_FLIP_V)?,
            button(|s| s.reset_adjustments, 10, 280, 240, ID_RESET)?,
        ];
        let names: [Text; 4] = [|s| s.brightness, |s| s.contrast, |s| s.saturation, |s| s.gamma];
        let mut sliders = [HWND::default(); 4];
        let mut values = [HWND::default(); 4];
        for (i, name) in names.into_iter().enumerate() {
            let y = 76 + 50 * i as i32;
            label(name, 12, y, 150)?;
            values[i] = b.add(w!("STATIC"), "", SS_RIGHT.0, 190, y, 58, 18, 0)?;
            sliders[i] = b.add(TRACKBAR_CLASSW, "", TBS_NOTICKS | WS_TABSTOP.0, 4, y + 18, 252, 28, 200 + i as i32)?;
            controls::trackbar_init(sliders[i], -100, 100, 10);
        }
        label(|s| s.resize, 12, 318, 150)?;
        let edit_style = (ES_NUMBER | ES_AUTOHSCROLL | WS_TABSTOP.0 as i32 | WS_BORDER.0 as i32) as u32;
        let width = b.add(w!("EDIT"), "", edit_style, 10, 338, 90, 24, ID_WIDTH)?;
        b.add(w!("STATIC"), "×", 0, 108, 341, 14, 18, 0)?;
        let height = b.add(w!("EDIT"), "", edit_style, 124, 338, 90, 24, ID_HEIGHT)?;
        let check_style = (BS_AUTOCHECKBOX | WS_TABSTOP.0 as i32) as u32;
        let keep_aspect = b.add(w!("BUTTON"), s.keep_aspect, check_style, 10, 368, 240, 22, ID_KEEP_ASPECT)?;
        texts.borrow_mut().push((keep_aspect, |s| s.keep_aspect));
        let half =
            b.add(w!("BUTTON"), "50%", (BS_PUSHBUTTON | WS_TABSTOP.0 as i32) as u32, 10, 394, 115, 26, ID_HALF)?;
        let apply = button(|s| s.apply, 135, 394, 115, ID_APPLY)?;
        let undo = button(|s| s.undo, 10, 430, 115, ID_UNDO)?;
        let redo = button(|s| s.redo, 135, 430, 115, ID_REDO)?;
        label(|s| s.save_quality, 12, 466, 150)?;
        let quality_value = b.add(w!("STATIC"), "", SS_RIGHT.0, 190, 466, 58, 18, 0)?;
        let quality = b.add(TRACKBAR_CLASSW, "", TBS_NOTICKS | WS_TABSTOP.0, 4, 484, 252, 28, 300)?;
        controls::trackbar_init(quality, 1, 100, 5);
        let save = button(|s| s.save, 10, 520, 115, ID_SAVE)?;
        button(|s| s.save_as, 135, 520, 115, ID_SAVE_AS)?;
        let texts = texts.into_inner();
        Ok(Panel {
            hwnd,
            sliders,
            values,
            width,
            height,
            keep_aspect,
            apply,
            half,
            undo,
            redo,
            save,
            quality,
            quality_value,
            edit_buttons,
            texts,
            layout: b.finish(),
        })
    }
}

/// Shows the panel (creating it on first use). With `activate` false it does not take focus.
pub fn show(owner: HWND, activate: bool, s: &Strings) -> Result<(), String> {
    let exists = PANEL.with(|p| p.borrow().is_some());
    if !exists {
        let panel = create(owner, s).map_err(|e| fill(s.panel_error, &[("error", &e.to_string())]))?;
        PANEL.with(|p| *p.borrow_mut() = Some(panel));
    }
    if let Some(h) = hwnd() {
        unsafe {
            let _ = ShowWindow(h, if activate { SW_SHOW } else { SW_SHOWNOACTIVATE });
        }
    }
    Ok(())
}

/// Relabels the panel (if it exists) in another language.
pub fn set_language(s: &Strings) {
    PANEL.with(|p| {
        let p = p.borrow();
        let Some(p) = p.as_ref() else { return };
        controls::set_text(p.hwnd, s.panel_title);
        for (h, text) in &p.texts {
            controls::set_text(*h, text(s));
        }
    });
}

pub fn hide() {
    if let Some(h) = hwnd() {
        unsafe {
            let _ = ShowWindow(h, SW_HIDE);
        }
    }
}

fn adjust_text(i: usize, a: &Adjust) -> String {
    match i {
        0 => format!("{:+.2}", a.brightness),
        1 => format!("{:+.2}", a.contrast),
        2 => format!("{:+.2}", a.saturation),
        _ => format!("{:.2}", a.gamma),
    }
}

/// Shows `st` and `cx` in the controls without reporting anything back.
pub fn sync(st: &PanelState, cx: &PanelContext) {
    PANEL.with(|p| {
        let p = p.borrow();
        let Some(p) = p.as_ref() else { return };
        SYNCING.with(|s| s.set(true));
        let a = st.adjust;
        let positions = [
            unit_to_position(a.brightness),
            unit_to_position(a.contrast),
            unit_to_position(a.saturation),
            gamma_to_position(a.gamma),
        ];
        for i in 0..4 {
            controls::trackbar_set(p.sliders[i], positions[i]);
            controls::set_text(p.values[i], &adjust_text(i, &a));
        }
        let has_image = cx.size.is_some();
        for h in p.edit_buttons.iter().chain(&p.sliders).chain([&p.width, &p.height, &p.keep_aspect, &p.half]) {
            controls::enable(*h, has_image);
        }
        let (w, h) = st.resize;
        let shown = |v: u32| if has_image && v > 0 { v.to_string() } else { String::new() };
        // rewriting a field the user is typing in would move the caret: only when its value
        // differs (an empty, zero or out-of-range field already stands for 0)
        for (field, v) in [(p.width, w), (p.height, h)] {
            let text = controls::text(field);
            if field_value(&text) != v || !has_image && !text.is_empty() {
                controls::set_text(field, &shown(v));
            }
        }
        controls::set_checked(p.keep_aspect, st.keep_aspect);
        controls::enable(p.apply, can_apply(st.resize, cx.size));
        controls::enable(p.undo, cx.can_undo);
        controls::enable(p.redo, cx.can_redo);
        controls::enable(p.save, cx.edited);
        controls::trackbar_set(p.quality, i32::from(st.quality));
        controls::set_text(p.quality_value, &st.quality.to_string());
        SYNCING.with(|s| s.set(false));
    });
}

fn report(a: PanelAction) {
    post_ui(UiEvent::Panel(a));
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let syncing = SYNCING.with(Cell::get);
    match msg {
        WM_COMMAND if !syncing => {
            let (id, code) = ((wp.0 & 0xffff) as i32, ((wp.0 >> 16) & 0xffff) as u32);
            let action = match (id, code) {
                (ID_ROTATE_LEFT, BN_CLICKED) => Some(PanelAction::Edit(Op::rotate_ccw())),
                (ID_ROTATE_RIGHT, BN_CLICKED) => Some(PanelAction::Edit(Op::rotate_cw())),
                (ID_FLIP_H, BN_CLICKED) => Some(PanelAction::Edit(Op::flip_h())),
                (ID_FLIP_V, BN_CLICKED) => Some(PanelAction::Edit(Op::flip_v())),
                (ID_RESET, BN_CLICKED) => Some(PanelAction::Adjust(Adjust::default())),
                (ID_HALF, BN_CLICKED) => Some(PanelAction::ResizeHalf),
                (ID_APPLY, BN_CLICKED) => Some(PanelAction::ResizeApply),
                (ID_UNDO, BN_CLICKED) => Some(PanelAction::Undo),
                (ID_REDO, BN_CLICKED) => Some(PanelAction::Redo),
                (ID_SAVE, BN_CLICKED) => Some(PanelAction::Save),
                (ID_SAVE_AS, BN_CLICKED) => Some(PanelAction::SaveAs),
                (ID_KEEP_ASPECT, BN_CLICKED) => {
                    let on = PANEL.with(|p| p.borrow().as_ref().is_some_and(|p| controls::checked(p.keep_aspect)));
                    Some(PanelAction::KeepAspect(on))
                }
                // always reported, an empty or invalid field as 0: the app must not keep the
                // last valid number while the field shows something else
                (ID_WIDTH | ID_HEIGHT, EN_CHANGE) => {
                    let field = PANEL.with(|p| {
                        p.borrow().as_ref().map(|p| controls::text(if id == ID_WIDTH { p.width } else { p.height }))
                    });
                    field.map(|t| field_value(&t)).map(|v| {
                        if id == ID_WIDTH { PanelAction::ResizeWidth(v) } else { PanelAction::ResizeHeight(v) }
                    })
                }
                (ID_CANCEL, _) => Some(PanelAction::Closed),
                _ => None,
            };
            if let Some(a) = action {
                report(a);
            }
            LRESULT(0)
        }
        WM_HSCROLL if !syncing => {
            let bar = HWND(lp.0 as *mut _);
            PANEL.with(|p| {
                let p = p.borrow();
                let Some(p) = p.as_ref() else { return };
                if p.sliders.contains(&bar) {
                    let pos = p.sliders.map(controls::trackbar_get);
                    let a = Adjust {
                        brightness: unit_from_position(pos[0]),
                        contrast: unit_from_position(pos[1]),
                        saturation: unit_from_position(pos[2]),
                        gamma: gamma_from_position(pos[3]),
                    };
                    for (i, value) in p.values.iter().enumerate() {
                        controls::set_text(*value, &adjust_text(i, &a));
                    }
                    report(PanelAction::Adjust(a));
                } else if bar == p.quality {
                    let q = controls::trackbar_get(p.quality).clamp(1, 100) as u8;
                    controls::set_text(p.quality_value, &q.to_string());
                    report(PanelAction::Quality(q));
                }
            });
            LRESULT(0)
        }
        WM_CLOSE => {
            report(PanelAction::Closed);
            LRESULT(0)
        }
        WM_DPICHANGED => {
            PANEL.with(|p| {
                if let Some(p) = p.borrow().as_ref() {
                    SYNCING.with(|s| s.set(true));
                    p.layout.dpi_changed(hwnd, wp, lp);
                    SYNCING.with(|s| s.set(false));
                }
            });
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wp, lp) },
    }
}
