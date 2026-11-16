//! The UI's words in each language. Every string is a field of [`Strings`], so a language that
//! misses one does not compile. Templates use `{name}`-style placeholders filled by [`fill`].

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use iv_pipeline::FailureKind;

use crate::commands::Command;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Language {
    #[default]
    Korean,
    English,
}

impl Language {
    pub const ALL: [Language; 2] = [Language::Korean, Language::English];

    /// The code stored in `settings.toml`.
    pub fn code(self) -> &'static str {
        match self {
            Language::Korean => "ko",
            Language::English => "en",
        }
    }

    pub fn from_code(code: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|l| l.code().eq_ignore_ascii_case(code.trim()))
    }

    /// The language's name in itself, for the language picker.
    pub fn native_name(self) -> &'static str {
        match self {
            Language::Korean => "한국어",
            Language::English => "English",
        }
    }

    pub fn strings(self) -> &'static Strings {
        match self {
            Language::Korean => &KO,
            Language::English => &EN,
        }
    }
}

impl Serialize for Language {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(self.code())
    }
}

/// An unknown code falls back to the default language rather than discarding the whole
/// settings file.
impl<'de> Deserialize<'de> for Language {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let code = String::deserialize(d)?;
        Ok(Language::from_code(&code).unwrap_or_default())
    }
}

/// Replaces `{key}` in `template` with each value.
pub fn fill(template: &str, args: &[(&str, &str)]) -> String {
    args.iter().fold(template.to_string(), |s, (k, v)| s.replace(&format!("{{{k}}}"), v))
}

macro_rules! strings {
    ($($field:ident),* $(,)?) => {
        pub struct Strings {
            $(pub $field: &'static str,)*
        }

        impl Strings {
            /// Every field with its name, for checking the tables.
            pub fn all(&self) -> Vec<(&'static str, &'static str)> {
                vec![$((stringify!($field), self.$field)),*]
            }
        }
    };
}

strings! {
    // commands (menu items, panel buttons)
    open, save, save_as, fix_extension, rotate_cw, rotate_ccw, flip_h, flip_v, crop, undo, redo,
    next, previous, first, last, fit, actual_size, zoom_in, zoom_out, fullscreen,
    two_pages, right_to_left, edit_panel, settings, help, escape, quit,
    // edit panel
    panel_title, reset_adjustments, brightness, contrast, saturation, gamma, resize, keep_aspect,
    apply, save_quality,
    // settings window
    prefetch_count, cache_memory, wrap, spread_rtl, sharp_pixels, save_quality_range, background,
    choose, cache_note, defaults, language, wheel_navigates,
    // help window: column titles, and mouse actions that are not in the keymap
    help_action, help_keys, pan, select_area, fit_or_actual, zoom_at_cursor, all_commands_menu,
    open_by_drop, mouse_drag, mouse_shift_drag, mouse_double_click, mouse_wheel, mouse_ctrl_wheel,
    mouse_right_click, mouse_drop,
    // dialogs and the title bar
    filter_images, filter_all, ask_save_changes, save_failed, title_empty, loading, preview,
    cached, saving, no_images_inside, edit_single_page, format_not_writable, settings_error,
    panel_error, settings_window_error, help_window_error, cannot_open, unsupported_format, too_large,
    file_not_found, unknown_file_type, cannot_write_format,
    // saving safely
    archive_page_save_as, animated_save_as, save_not_loaded, first_frame_only, save_busy, continue_after_save,
    // fixing a wrong extension
    extension_mismatch, extension_fixed, extension_ok, extension_exists, extension_fix_failed,
    archive_page_rename,
    // startup: settings and keymap files that could not be used
    settings_unreadable, keymap_warning, settings_backed_up,
    // failures without a console to report them: the folder listing, the window, a crash
    folder_list_failed, window_failed, crash_title, crash_message,
}

impl Strings {
    /// The label of a command.
    pub fn command(&self, cmd: Command) -> &'static str {
        match cmd {
            Command::FileOpen => self.open,
            Command::FileSave => self.save,
            Command::FileSaveAs => self.save_as,
            Command::FileFixExtension => self.fix_extension,
            Command::EditRotateCw => self.rotate_cw,
            Command::EditRotateCcw => self.rotate_ccw,
            Command::EditFlipH => self.flip_h,
            Command::EditFlipV => self.flip_v,
            Command::EditCrop => self.crop,
            Command::EditUndo => self.undo,
            Command::EditRedo => self.redo,
            Command::NavNext => self.next,
            Command::NavPrev => self.previous,
            Command::NavFirst => self.first,
            Command::NavLast => self.last,
            Command::ViewFit => self.fit,
            Command::ViewActualSize => self.actual_size,
            Command::ViewZoomIn => self.zoom_in,
            Command::ViewZoomOut => self.zoom_out,
            Command::ViewToggleFullscreen => self.fullscreen,
            Command::ViewToggleEditPanel => self.edit_panel,
            Command::ViewToggleSpread => self.two_pages,
            Command::ViewToggleDirection => self.right_to_left,
            Command::AppSettings => self.settings,
            Command::AppHelp => self.help,
            Command::Escape => self.escape,
            Command::AppQuit => self.quit,
        }
    }

    /// Why `name` cannot be shown, in this language. Common causes get a full sentence; for the
    /// rest the decoder's own (English) text is kept as the detail in brackets.
    pub fn open_failed(&self, name: &str, kind: FailureKind, detail: &str) -> String {
        match kind {
            FailureKind::Unsupported => fill(self.unsupported_format, &[("name", name)]),
            FailureKind::NotFound => fill(self.file_not_found, &[("name", name)]),
            FailureKind::TooLarge { width, height } => {
                fill(self.too_large, &[("name", name), ("width", &width.to_string()), ("height", &height.to_string())])
            }
            FailureKind::Other => fill(self.cannot_open, &[("name", name), ("error", detail)]),
        }
    }
}

pub static KO: Strings = Strings {
    open: "열기…",
    save: "저장",
    save_as: "다른 이름으로 저장…",
    fix_extension: "확장자 바로잡기",
    rotate_cw: "오른쪽으로 회전",
    rotate_ccw: "왼쪽으로 회전",
    flip_h: "좌우 뒤집기",
    flip_v: "상하 뒤집기",
    crop: "선택 영역으로 자르기",
    undo: "실행 취소",
    redo: "다시 실행",
    next: "다음",
    previous: "이전",
    first: "처음",
    last: "마지막",
    fit: "창에 맞춤",
    actual_size: "원래 크기",
    zoom_in: "확대",
    zoom_out: "축소",
    fullscreen: "전체 화면",
    two_pages: "두 쪽 보기",
    right_to_left: "오른쪽에서 왼쪽으로 읽기",
    edit_panel: "편집 패널",
    settings: "설정",
    help: "단축키 도움말",
    escape: "선택 해제 / 전체 화면 해제 / 끝내기",
    quit: "끝내기",
    panel_title: "편집",
    reset_adjustments: "보정 초기화",
    brightness: "밝기",
    contrast: "대비",
    saturation: "채도",
    gamma: "감마",
    resize: "크기 변경",
    keep_aspect: "비율 유지",
    apply: "적용",
    save_quality: "저장 품질",
    prefetch_count: "미리 읽을 장수",
    cache_memory: "캐시 메모리 (MiB)",
    wrap: "끝에서 처음으로 이어서 넘기기",
    spread_rtl: "두 쪽 보기를 오른쪽에서 왼쪽으로 읽기",
    sharp_pixels: "픽셀을 선명하게 보일 배율 (×)",
    save_quality_range: "저장 품질 (1–100)",
    background: "배경색",
    choose: "선택…",
    cache_note: "캐시 메모리는 다시 시작한 뒤 적용됩니다.",
    defaults: "기본값",
    language: "언어",
    wheel_navigates: "휠로 사진 넘기기 (Ctrl+휠은 확대·축소)",
    help_action: "동작",
    help_keys: "키·마우스",
    pan: "이동",
    select_area: "영역 선택",
    fit_or_actual: "창에 맞춤 ↔ 100%",
    zoom_at_cursor: "커서 기준 확대·축소",
    all_commands_menu: "모든 명령 메뉴",
    open_by_drop: "파일이나 폴더 열기",
    mouse_drag: "드래그",
    mouse_shift_drag: "Shift+드래그",
    mouse_double_click: "더블클릭",
    mouse_wheel: "휠",
    mouse_ctrl_wheel: "Ctrl+휠",
    mouse_right_click: "우클릭",
    mouse_drop: "끌어다 놓기",
    filter_images: "이미지와 압축파일",
    filter_all: "모든 파일",
    ask_save_changes: "{name}의 변경 사항을 저장할까요?",
    save_failed: "저장하지 못했습니다: {error}",
    title_empty: "Ctrl+O로 열거나 파일을 끌어다 놓으세요",
    loading: "불러오는 중",
    preview: "미리보기",
    cached: "캐시",
    saving: "저장 중…",
    no_images_inside: "{name}: 이미지가 없습니다",
    edit_single_page: "편집은 한 쪽 보기에서만 됩니다 (D 키)",
    format_not_writable: "이 형식으로는 저장할 수 없어 다른 이름으로 저장합니다",
    settings_error: "설정: {error}",
    panel_error: "편집 패널을 만들 수 없습니다: {error}",
    settings_window_error: "설정 창을 만들 수 없습니다: {error}",
    help_window_error: "도움말 창을 만들 수 없습니다: {error}",
    cannot_open: "{name}: 열 수 없습니다 ({error})",
    unsupported_format: "{name}: 지원하지 않는 형식입니다",
    too_large: "{name}: 이미지가 너무 커서 열 수 없습니다 ({width}×{height})",
    file_not_found: "{name}: 파일이 없습니다",
    unknown_file_type: "알 수 없는 파일 형식입니다: .{ext}",
    cannot_write_format: "{format} 형식으로는 저장할 수 없습니다",
    archive_page_save_as: "압축파일 안의 페이지는 그 자리에 저장할 수 없어 다른 이름으로 저장합니다",
    animated_save_as: "움직이는 이미지는 첫 프레임만 저장되므로 원본을 지키려고 다른 이름으로 저장합니다",
    save_not_loaded: "아직 이미지를 다 읽지 않아 몇 프레임인지 모릅니다. 다 읽은 뒤에 다시 저장하세요",
    first_frame_only: "첫 프레임만 저장했습니다 (나머지 {count}프레임은 저장하지 않음)",
    save_busy: "저장하는 중입니다. 끝난 뒤에 다시 하세요",
    continue_after_save: "저장이 끝나면 이어서 진행합니다",
    extension_mismatch: "확장자가 내용과 다름",
    extension_fixed: "이름을 {name}(으)로 바꿨습니다",
    extension_ok: "확장자가 이미 내용과 맞습니다",
    extension_exists: "{name} 파일이 이미 있어 이름을 바꾸지 않았습니다",
    extension_fix_failed: "확장자를 바꾸지 못했습니다: {error}",
    archive_page_rename: "압축파일 안의 페이지는 이름을 바꿀 수 없습니다",
    settings_unreadable: "설정 파일을 읽지 못해 기본값을 씁니다 ({error})",
    keymap_warning: "키맵: {error}",
    settings_backed_up: "설정을 저장했습니다. 읽지 못한 예전 파일은 {name}(으)로 보관했습니다",
    folder_list_failed: "{name}이(가) 있는 폴더를 읽지 못해 이전·다음 이미지로 넘길 수 없습니다 ({error})",
    window_failed: "창을 만들지 못했습니다: {error}",
    crash_title: "이미지 뷰어 오류",
    crash_message: "문제가 생겨 이미지 뷰어를 닫습니다.\n\n자세한 내용을 다음 파일에 남겼습니다:\n{path}",
};

pub static EN: Strings = Strings {
    open: "Open…",
    save: "Save",
    save_as: "Save as…",
    fix_extension: "Fix extension",
    rotate_cw: "Rotate right",
    rotate_ccw: "Rotate left",
    flip_h: "Flip horizontally",
    flip_v: "Flip vertically",
    crop: "Crop to selection",
    undo: "Undo",
    redo: "Redo",
    next: "Next",
    previous: "Previous",
    first: "First",
    last: "Last",
    fit: "Fit to window",
    actual_size: "Actual size",
    zoom_in: "Zoom in",
    zoom_out: "Zoom out",
    fullscreen: "Full screen",
    two_pages: "Two pages",
    right_to_left: "Right to left",
    edit_panel: "Edit panel",
    settings: "Settings",
    help: "Keyboard and mouse",
    escape: "Clear selection / leave full screen / quit",
    quit: "Quit",
    panel_title: "Edit",
    reset_adjustments: "Reset adjustments",
    brightness: "Brightness",
    contrast: "Contrast",
    saturation: "Saturation",
    gamma: "Gamma",
    resize: "Resize",
    keep_aspect: "Keep aspect ratio",
    apply: "Apply",
    save_quality: "Save quality",
    prefetch_count: "Images decoded ahead",
    cache_memory: "Cache memory (MiB)",
    wrap: "Wrap around at the ends",
    spread_rtl: "Two pages read right to left",
    sharp_pixels: "Sharp pixels above zoom (×)",
    save_quality_range: "Save quality (1–100)",
    background: "Background",
    choose: "Choose…",
    cache_note: "Cache memory applies after a restart.",
    defaults: "Defaults",
    language: "Language",
    wheel_navigates: "Wheel browses images (Ctrl+wheel zooms)",
    help_action: "Action",
    help_keys: "Keys and mouse",
    pan: "Pan",
    select_area: "Select an area",
    fit_or_actual: "Fit ↔ 100%",
    zoom_at_cursor: "Zoom at the cursor",
    all_commands_menu: "Menu of every command",
    open_by_drop: "Open a file or folder",
    mouse_drag: "Drag",
    mouse_shift_drag: "Shift+drag",
    mouse_double_click: "Double-click",
    mouse_wheel: "Wheel",
    mouse_ctrl_wheel: "Ctrl+wheel",
    mouse_right_click: "Right-click",
    mouse_drop: "Drag and drop",
    filter_images: "Images and archives",
    filter_all: "All files",
    ask_save_changes: "Save changes to {name}?",
    save_failed: "Save failed: {error}",
    title_empty: "Ctrl+O to open, or drop a file",
    loading: "loading",
    preview: "preview",
    cached: "cached",
    saving: "saving…",
    no_images_inside: "{name}: no images inside",
    edit_single_page: "editing works in single-page view (press D)",
    format_not_writable: "this format cannot be written; choose a new file",
    settings_error: "settings: {error}",
    panel_error: "cannot create the edit panel: {error}",
    settings_window_error: "cannot create the settings window: {error}",
    help_window_error: "cannot create the help window: {error}",
    cannot_open: "{name}: cannot open ({error})",
    unsupported_format: "{name}: unsupported format",
    too_large: "{name}: too large to open ({width}×{height})",
    file_not_found: "{name}: file not found",
    unknown_file_type: "unknown file type: .{ext}",
    cannot_write_format: "{format} files cannot be written",
    archive_page_save_as: "pages inside an archive cannot be saved in place; choose a new file",
    animated_save_as: "saving an animation keeps only its first frame; choose a new file to keep the original",
    save_not_loaded: "the image is still loading, so its frame count is not known yet; save again once it has loaded",
    first_frame_only: "saved the first frame only ({count} more frames not saved)",
    save_busy: "a save is in progress; try again when it finishes",
    continue_after_save: "will continue when the save finishes",
    extension_mismatch: "extension does not match the content",
    extension_fixed: "renamed to {name}",
    extension_ok: "the extension already matches the content",
    extension_exists: "{name} already exists; nothing was renamed",
    extension_fix_failed: "cannot fix the extension: {error}",
    archive_page_rename: "pages inside an archive cannot be renamed",
    settings_unreadable: "the settings file could not be read; using defaults ({error})",
    keymap_warning: "keymap: {error}",
    settings_backed_up: "settings saved; the unreadable old file was kept as {name}",
    folder_list_failed: "cannot list the folder of {name}, so there is no previous or next image ({error})",
    window_failed: "cannot create the window: {error}",
    crash_title: "image-viewer error",
    crash_message: "image-viewer ran into a problem and has to close.\n\nThe details were written to:\n{path}",
};

#[cfg(test)]
mod tests {
    use super::*;

    fn placeholders(s: &str) -> Vec<&str> {
        let mut out: Vec<&str> =
            s.split('{').skip(1).filter_map(|rest| rest.split_once('}').map(|(name, _)| name)).collect();
        out.sort_unstable();
        out
    }

    #[test]
    fn every_string_is_translated_with_the_same_placeholders() {
        for ((name, ko), (_, en)) in KO.all().into_iter().zip(EN.all()) {
            assert!(!ko.trim().is_empty() && !en.trim().is_empty(), "{name} is empty");
            assert_eq!(placeholders(ko), placeholders(en), "{name}: placeholders differ");
        }
        for cmd in Command::ALL {
            assert!(!KO.command(cmd).is_empty() && !EN.command(cmd).is_empty(), "{cmd:?}");
        }
        // the languages really differ (no copy left untranslated by mistake)
        let same: Vec<_> = KO.all().into_iter().zip(EN.all()).filter(|(k, e)| k.1 == e.1).map(|(k, _)| k.0).collect();
        assert!(same.is_empty(), "identical in both languages: {same:?}");
    }

    #[test]
    fn templates_are_filled() {
        assert_eq!(fill(KO.ask_save_changes, &[("name", "a.jpg")]), "a.jpg의 변경 사항을 저장할까요?");
        assert_eq!(fill(EN.save_failed, &[("error", "disk full")]), "Save failed: disk full");
    }

    #[test]
    fn open_failures_are_worded_per_language() {
        assert_eq!(KO.open_failed("a.jpg", FailureKind::Unsupported, "x"), "a.jpg: 지원하지 않는 형식입니다");
        assert_eq!(EN.open_failed("a.jpg", FailureKind::NotFound, "x"), "a.jpg: file not found");
        let big = FailureKind::TooLarge { width: 90000, height: 80000 };
        assert_eq!(EN.open_failed("b.png", big, "x"), "b.png: too large to open (90000×80000)");
        let other = KO.open_failed("c.heic", FailureKind::Other, "WIC failed to decode: 0x88982F50");
        assert_eq!(other, "c.heic: 열 수 없습니다 (WIC failed to decode: 0x88982F50)");
    }

    #[test]
    fn codes_round_trip_and_unknown_codes_fall_back_to_korean() {
        for l in Language::ALL {
            assert_eq!(Language::from_code(l.code()), Some(l));
        }
        assert_eq!(Language::from_code(" EN "), Some(Language::English));
        assert_eq!(Language::default(), Language::Korean);
        #[derive(serde::Deserialize)]
        struct T {
            language: Language,
        }
        let t: T = toml::from_str("language = \"fr\"").unwrap();
        assert_eq!(t.language, Language::Korean);
    }
}
