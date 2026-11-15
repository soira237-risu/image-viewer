//! The UI's words in each language. Every string is a field of [`Strings`], so a language that
//! misses one does not compile. Templates use `{name}`-style placeholders filled by [`fill`].

use serde::{Deserialize, Deserializer, Serialize, Serializer};

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
    open, save, save_as, rotate_cw, rotate_ccw, flip_h, flip_v, crop, undo, redo,
    next, previous, first, last, fit, actual_size, zoom_in, zoom_out, fullscreen,
    two_pages, right_to_left, edit_panel, settings, escape, quit,
    // edit panel
    panel_title, reset_adjustments, brightness, contrast, saturation, gamma, resize, keep_aspect,
    apply, save_quality,
    // settings window
    prefetch_count, cache_memory, wrap, spread_rtl, sharp_pixels, save_quality_range, background,
    choose, cache_note, defaults, language,
    // dialogs and the title bar
    filter_images, filter_all, ask_save_changes, save_failed, title_empty, loading, preview,
    cached, saving, no_images_inside, edit_single_page, format_not_writable, settings_error,
    panel_error, settings_window_error,
}

impl Strings {
    /// The label of a command.
    pub fn command(&self, cmd: Command) -> &'static str {
        match cmd {
            Command::FileOpen => self.open,
            Command::FileSave => self.save,
            Command::FileSaveAs => self.save_as,
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
            Command::Escape => self.escape,
            Command::AppQuit => self.quit,
        }
    }
}

pub static KO: Strings = Strings {
    open: "열기…",
    save: "저장",
    save_as: "다른 이름으로 저장…",
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
    format_not_writable: "이 형식으로는 저장할 수 없습니다. 다른 이름으로 저장을 사용하세요",
    settings_error: "설정: {error}",
    panel_error: "편집 패널을 만들 수 없습니다: {error}",
    settings_window_error: "설정 창을 만들 수 없습니다: {error}",
};

pub static EN: Strings = Strings {
    open: "Open…",
    save: "Save",
    save_as: "Save as…",
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
    format_not_writable: "this format cannot be written; use Save As",
    settings_error: "settings: {error}",
    panel_error: "cannot create the edit panel: {error}",
    settings_window_error: "cannot create the settings window: {error}",
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
