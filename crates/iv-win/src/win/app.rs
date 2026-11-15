//! The viewer's state and behaviour: what is current, what is on screen, navigation, editing
//! and saving. The window procedure (`window.rs`) feeds it messages; pipeline and worker threads
//! feed it [`AppEvent`]s through a channel.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use iv_codec::Decoded;
use iv_codec::encode::Encoders;
use iv_core::{FormatId, ImageBuf, ImageInfo, Orientation};
use iv_ops::{EditStack, Geometry, Op};
use iv_pipeline::export::{ExportOutcome, export};
use iv_pipeline::{FolderList, Pipeline, PipelineConfig, PipelineEvent, prefetch_order, step};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{InvalidateRect, ScreenToClient};
use windows::Win32::System::SystemServices::MK_SHIFT;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::Shell::{DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::dialogs::{self, Answer, Filter};
use super::{UiEvent, keys, menu, panel, settings_window, wake, wide};
use crate::commands::Command;
use crate::edit::{drag_rect, in_place_format, save_request, selection_to_crop};
use crate::i18n::{Strings, fill};
use crate::keymap::{Chord, Keymap};
use crate::panel::{PanelAction, PanelContext, PanelState, follow_aspect};
use crate::raster::{self, Frame, Page, Picture, Style};
use crate::settings::Settings;
use crate::trace::Trace;
use crate::view::{ScreenRect, ViewState, ZoomMode};

const ZOOM_STEP: f64 = 1.25;
const ANIMATION_TIMER: usize = 1;

#[derive(Debug)]
pub enum AppEvent {
    Pipeline(PipelineEvent),
    Folder { for_path: PathBuf, result: Result<FolderList, String> },
    Saved { source: PathBuf, dest: PathBuf, result: Result<ExportOutcome, String> },
}

/// Sends `e` to the UI thread and wakes it.
fn send(tx: &Sender<AppEvent>, e: AppEvent) {
    if tx.send(e).is_ok() {
        wake();
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shown {
    Preview,
    Full,
}

/// The image the user asked for and what has arrived for it so far.
struct Current {
    path: PathBuf,
    info: Option<ImageInfo>,
    preview: Option<ImageBuf>,
    full: Option<Arc<Decoded>>,
    /// Decode time for a fresh load; `None` when it came from the cache.
    load_time: Option<Duration>,
    requested_at: Instant,
    shown_logged: bool,
}

impl Current {
    fn new(path: PathBuf) -> Self {
        Self {
            path,
            info: None,
            preview: None,
            full: None,
            load_time: None,
            requested_at: Instant::now(),
            shown_logged: false,
        }
    }
}

/// What is on screen. It can lag behind [`Current`]: the previous image stays up until the next
/// one has something to show, so navigation never flashes the background.
struct Display {
    path: PathBuf,
    kind: Shown,
    view: ViewState,
    picture: Picture,
    /// Stored size and orientation of the image `picture` shows (a preview has fewer pixels
    /// but the same shape).
    stored: (u32, u32),
    orientation: Orientation,
    playback: Option<Playback>,
    /// Two-page view: the page shown beside `path`.
    second: Option<Second>,
}

struct Second {
    path: PathBuf,
    picture: Picture,
    stored: (u32, u32),
    orientation: Orientation,
    /// Share of the combined width taken by the main page.
    fraction: f64,
}

/// An animation being played.
struct Playback {
    decoded: Arc<Decoded>,
    frame: usize,
    next_at: Instant,
    started_at: Instant,
    loop_logged: bool,
}

impl Playback {
    fn start(decoded: &Arc<Decoded>) -> Option<Self> {
        let anim = decoded.animation.as_ref()?;
        let now = Instant::now();
        Some(Self {
            decoded: decoded.clone(),
            frame: 0,
            next_at: now + anim.frames[0].delay,
            started_at: now,
            loop_logged: false,
        })
    }

    /// Advances to the frame due at `now`, skipping frames when late. Returns whether it changed.
    fn advance(&mut self, now: Instant) -> bool {
        let Some(anim) = self.decoded.animation.as_ref() else { return false };
        let before = self.frame;
        while now >= self.next_at {
            self.frame = (self.frame + 1) % anim.frames.len();
            self.next_at += anim.frames[self.frame].delay;
            // after a long stall (minimised, debugger) resync instead of racing through frames
            if now.saturating_duration_since(self.next_at) > Duration::from_secs(1) {
                self.next_at = now + anim.frames[self.frame].delay;
            }
        }
        self.frame != before
    }
}

/// Window placement saved while in full screen.
struct Restore {
    style: isize,
    placement: WINDOWPLACEMENT,
}

pub struct App {
    tx: Sender<AppEvent>,
    rx: Receiver<AppEvent>,
    pipeline: Pipeline,
    keymap: Keymap,
    trace: Trace,
    hwnd: Option<HWND>,
    viewport: (u32, u32),
    /// The frame no longer shows the state; redraw on the next paint.
    dirty: bool,
    current: Option<Current>,
    display: Option<Display>,
    folder: Option<FolderList>,
    index: Option<usize>,
    /// Last navigation direction (+1 / −1), 0 right after opening a file.
    direction: i32,
    /// A navigation key is being held: flip without prefetching.
    quick_browsing: bool,
    error: Option<String>,
    cursor: (f64, f64),
    drag_from: Option<(f64, f64)>,
    select_from: Option<(f64, f64)>,
    selection: Option<ScreenRect>,
    first_frame_logged: bool,
    no_activate: bool,
    edits: EditStack,
    saving: bool,
    encoders: Encoders,
    /// Two-page view: `Some(right_to_left)` when on.
    spread: Option<bool>,
    panel: PanelState,
    settings: Settings,
    settings_path: Option<PathBuf>,
    settings_open: bool,
    style: Style,
    fullscreen: Option<Restore>,
    /// A key handled as WM_SYSKEYDOWN: swallow the WM_SYSCHAR that follows (else: a beep).
    swallow_syschar: bool,
}

fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// Two pages scaled to a common height: the combined size, and the share of its width taken by
/// the first page.
pub fn spread_layout(main: (u32, u32), second: (u32, u32)) -> ((u32, u32), f64) {
    let h = main.1.max(second.1).max(1) as f64;
    let main_w = main.0 as f64 * h / main.1.max(1) as f64;
    let second_w = second.0 as f64 * h / second.1.max(1) as f64;
    (((main_w + second_w).round() as u32, h as u32), main_w / (main_w + second_w))
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetKeyState(i32::from(vk.0)) < 0 }
}

impl App {
    pub fn new(
        keymap: Keymap,
        trace: Trace,
        settings: Settings,
        settings_path: Option<PathBuf>,
        no_activate: bool,
    ) -> Self {
        let (tx, rx) = channel();
        let sink = tx.clone();
        let config =
            PipelineConfig { budget_bytes: settings.cache_mib as u64 * 1024 * 1024, ..PipelineConfig::default() };
        let pipeline = Pipeline::new(Arc::new(iv_codec::default_registry()), config, move |e| {
            send(&sink, AppEvent::Pipeline(e));
        });
        let style = Style { background: settings.background, nearest_above_zoom: settings.nearest_above_zoom as f64 };
        Self {
            tx,
            rx,
            pipeline,
            keymap,
            trace,
            hwnd: None,
            viewport: (1280, 800),
            dirty: true,
            current: None,
            display: None,
            folder: None,
            index: None,
            direction: 0,
            quick_browsing: false,
            error: None,
            cursor: (0.0, 0.0),
            drag_from: None,
            select_from: None,
            selection: None,
            first_frame_logged: false,
            no_activate,
            edits: EditStack::new(),
            saving: false,
            encoders: Encoders::default(),
            spread: None,
            panel: PanelState { quality: settings.save_quality, ..PanelState::default() },
            settings,
            settings_path,
            settings_open: false,
            style,
            fullscreen: None,
            swallow_syschar: false,
        }
    }

    pub fn trace(&self, what: &str) {
        self.trace.mark(what);
    }

    /// The window exists now.
    pub fn attach(&mut self, hwnd: HWND) {
        self.hwnd = Some(hwnd);
        self.update_title();
        self.invalidate();
        // an animation decoded before the window existed had no window to time its frames
        self.schedule_animation();
    }

    /// Handles everything queued by other threads and by the panel / settings windows.
    pub fn drain(&mut self) {
        loop {
            if let Ok(e) = self.rx.try_recv() {
                match e {
                    AppEvent::Pipeline(e) => self.handle_pipeline(e),
                    AppEvent::Folder { for_path, result } => self.handle_folder(for_path, result),
                    AppEvent::Saved { source, dest, result } => self.handle_saved(source, dest, result),
                }
            } else if let Some(e) = super::take_ui() {
                self.handle_ui(e);
            } else {
                break;
            }
        }
    }

    fn invalidate(&mut self) {
        self.dirty = true;
        if let Some(h) = self.hwnd {
            unsafe {
                let _ = InvalidateRect(Some(h), None, false);
            }
        }
    }

    /// Opens a file (or an archive's first page) and lists its folder in the background.
    pub fn open(&mut self, path: PathBuf) {
        if !self.confirm_discard() {
            return;
        }
        self.trace.mark(&format!("open {}", path.display()));
        self.folder = None;
        self.index = None;
        self.direction = 0;

        if iv_pipeline::archive::is_archive(&path) && path.is_file() {
            match FolderList::for_file(&path) {
                Ok(list) if !list.is_empty() => {
                    let first = list.files()[0].clone();
                    self.trace.mark(&format!("archive listed: {} images", list.len()));
                    self.folder = Some(list);
                    self.index = Some(0);
                    self.request(first);
                }
                Ok(_) => self.set_error(fill(self.t().no_images_inside, &[("name", &file_name(&path))])),
                Err(e) => self.set_error(format!("{}: {e}", file_name(&path))),
            }
            return;
        }
        self.request(path.clone());

        let tx = self.tx.clone();
        std::thread::Builder::new()
            .name("folder-scan".into())
            .spawn(move || {
                let result = FolderList::for_file(&path).map_err(|e| e.to_string());
                send(&tx, AppEvent::Folder { for_path: path, result });
            })
            .expect("spawn folder scan");
    }

    /// The UI's words in the chosen language.
    fn t(&self) -> &'static Strings {
        self.settings.language.strings()
    }

    fn set_error(&mut self, e: String) {
        self.error = Some(e);
        self.update_title();
    }

    /// Paths to prefetch around the current index, nearest first.
    fn prefetch_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = match (&self.folder, self.index) {
            (Some(folder), Some(i)) if !self.quick_browsing => {
                let n = self.settings.prefetch_count;
                let count = if self.spread.is_some() { n * 2 } else { n };
                prefetch_order(i, folder.len(), self.direction, count, self.settings.wrap)
                    .into_iter()
                    .filter_map(|j| folder.get(j).map(Path::to_path_buf))
                    .collect()
            }
            _ => Vec::new(),
        };
        if let Some(partner) = self.partner() {
            paths.retain(|p| *p != partner);
            paths.insert(0, partner);
        }
        paths
    }

    fn refresh_spread(&mut self) {
        if let Some(path) = self.current.as_ref().map(|c| c.path.clone()) {
            self.request(path);
        }
        self.sync_second();
        self.update_title();
    }

    /// Two-page view: the page shown beside the current one.
    fn partner(&self) -> Option<PathBuf> {
        self.spread?;
        let (folder, i) = (self.folder.as_ref()?, self.index?);
        folder.get(i + 1).map(Path::to_path_buf)
    }

    /// Asks the pipeline for `path` (and its neighbours) and makes it current.
    fn request(&mut self, path: PathBuf) {
        let hit = if self.quick_browsing {
            self.pipeline.request_quick(&path)
        } else {
            let prefetch = self.prefetch_paths();
            self.pipeline.request(&path, &prefetch)
        };
        if self.current.as_ref().map(|c| &c.path) != Some(&path) {
            self.current = Some(Current::new(path));
            self.error = None;
            self.edits.clear();
            self.selection = None;
            self.after_image_change();
        }
        if let (Some(cur), Some(decoded)) = (self.current.as_mut(), hit) {
            if cur.full.is_none() {
                cur.info = Some(decoded.info.clone());
                cur.full = Some(decoded);
            }
        }
        self.sync_display();
        self.update_title();
    }

    fn navigate(&mut self, cmd: Command, repeat: bool) {
        let (Some(folder), Some(index)) = (&self.folder, self.index) else { return };
        let len = folder.len();
        let stride = if self.spread.is_some() { 2 } else { 1 };
        let target = match cmd {
            Command::NavNext => step(index, len, stride, self.settings.wrap),
            Command::NavPrev => step(index, len, -stride, self.settings.wrap),
            Command::NavFirst => (index != 0).then_some(0),
            Command::NavLast => (index + 1 != len).then(|| len - 1),
            _ => None,
        };
        let Some(target) = target else { return };
        let Some(path) = folder.get(target).map(Path::to_path_buf) else { return };
        if !self.confirm_discard() {
            return;
        }
        self.direction = if matches!(cmd, Command::NavPrev | Command::NavFirst) { -1 } else { 1 };
        self.index = Some(target);
        if repeat {
            self.quick_browsing = true;
        }
        self.request(path);
    }

    /// The navigation key was released: resume prefetching around where the user stopped.
    fn end_quick_browsing(&mut self) {
        if self.quick_browsing {
            self.quick_browsing = false;
            if let Some(path) = self.current.as_ref().map(|c| c.path.clone()) {
                self.request(path);
            }
        }
    }

    /// Puts the best image for the current request on screen if it is not there yet.
    fn sync_display(&mut self) {
        let viewport = self.viewport;
        let Some(cur) = self.current.as_mut() else { return };
        let same_image = self.display.as_ref().is_some_and(|d| d.path == cur.path);
        let shown = self.display.as_ref().filter(|_| same_image).map(|d| d.kind);

        let (image, info, kind) = match (&cur.full, &cur.preview, &cur.info) {
            (Some(full), _, _) if shown != Some(Shown::Full) => (&full.image, &full.info, Shown::Full),
            (None, Some(preview), Some(info)) if shown.is_none() => (preview, info, Shown::Preview),
            _ => {
                self.sync_second();
                return;
            }
        };
        let t = Instant::now();
        let picture = Picture::new(image, info.has_alpha);
        self.trace.mark(&format!(
            "{} ready {}x{} in {:.1} ms",
            if kind == Shown::Full { "image" } else { "preview" },
            image.width(),
            image.height(),
            t.elapsed().as_secs_f64() * 1000.0
        ));
        let stored = (info.width, info.height);
        let size = info.display_size();
        match &mut self.display {
            // the full image replacing its own preview keeps the user's zoom
            Some(d) if same_image => {
                d.kind = kind;
                d.picture = picture;
                d.stored = stored;
                d.orientation = info.orientation;
                if d.second.is_none() {
                    d.view.set_image_size(size);
                }
            }
            _ => {
                self.display = Some(Display {
                    path: cur.path.clone(),
                    kind,
                    view: ViewState::new(size, viewport),
                    picture,
                    stored,
                    orientation: info.orientation,
                    playback: None,
                    second: None,
                })
            }
        }
        if kind == Shown::Full {
            cur.preview = None;
            if let (Some(d), Some(full)) = (&mut self.display, &cur.full) {
                d.playback = Playback::start(full);
            }
            self.schedule_animation();
        }
        if !self.edits.is_empty() {
            self.apply_edits_to_view();
        }
        self.sync_second();
        self.sync_panel();
        self.invalidate();
    }

    /// Two-page view: puts the facing page beside the current one once it is decoded, or
    /// removes it when there is none.
    fn sync_second(&mut self) {
        let want = self.partner();
        let Some(cur) = &self.current else { return };
        let Some(main_size) = cur.info.as_ref().map(ImageInfo::display_size) else { return };
        let Some(d) = self.display.as_mut().filter(|d| d.path == cur.path) else { return };
        match want {
            None => {
                if d.second.take().is_some() {
                    d.view.set_image_size(main_size);
                    self.invalidate();
                }
            }
            Some(partner) if d.second.as_ref().map(|s| &s.path) != Some(&partner) => {
                let Some(page) = self.pipeline.cached(&partner) else { return };
                let (total, fraction) = spread_layout(main_size, page.info.display_size());
                d.second = Some(Second {
                    path: partner,
                    picture: Picture::new(&page.image, page.info.has_alpha),
                    stored: (page.info.width, page.info.height),
                    orientation: page.info.orientation,
                    fraction,
                });
                d.view.set_image_size(total);
                self.invalidate();
            }
            Some(_) => {}
        }
    }

    /// Shows an image that is no longer current (quick browsing), unless the current one is
    /// already on screen.
    fn show_passing(&mut self, path: PathBuf, decoded: &Decoded) {
        let current_shown = matches!((&self.current, &self.display), (Some(c), Some(d)) if d.path == c.path);
        if current_shown {
            return;
        }
        self.trace.mark(&format!("passing image shown: {}", file_name(&path)));
        let info = &decoded.info;
        self.display = Some(Display {
            path,
            kind: Shown::Full,
            view: ViewState::new(info.display_size(), self.viewport),
            picture: Picture::new(&decoded.image, info.has_alpha),
            stored: (info.width, info.height),
            orientation: info.orientation,
            playback: None,
            second: None,
        });
        self.invalidate();
    }

    fn update_title(&self) {
        let Some(hwnd) = self.hwnd else { return };
        let t = self.t();
        let position = match (&self.folder, self.index) {
            (Some(f), Some(i)) if self.partner().is_some() => format!("[{}-{}/{}] ", i + 1, i + 2, f.len()),
            (Some(f), Some(i)) => format!("[{}/{}] ", i + 1, f.len()),
            _ => String::new(),
        };
        let title = match (&self.current, &self.error) {
            (_, Some(err)) => format!("{position}image-viewer — {err}"),
            (Some(cur), None) => {
                let name = file_name(&cur.path);
                match &cur.info {
                    None => format!("{position}{name} — {}", t.loading),
                    Some(info) => {
                        let (w, h) = if self.edits.is_empty() {
                            info.display_size()
                        } else {
                            self.geometry().map_or(info.display_size(), |g| g.size)
                        };
                        let zoom = self
                            .display
                            .as_ref()
                            .filter(|d| d.path == cur.path)
                            .map_or(String::new(), |d| format!(" — {}%", (d.view.zoom() * 100.0).round()));
                        let state = match (&cur.full, cur.load_time) {
                            (Some(_), Some(elapsed)) => {
                                format!("{} · {} · {:.0} ms", info.format, info.codec, elapsed.as_secs_f64() * 1000.0)
                            }
                            (Some(_), None) => format!("{} · {} · {}", info.format, info.codec, t.cached),
                            (None, _) if cur.preview.is_some() => format!("{} · {}", info.format, t.preview),
                            (None, _) => format!("{} · {}", info.format, t.loading),
                        };
                        let edited = if self.edits.is_empty() { "" } else { "*" };
                        let saving = if self.saving { format!(" · {}", t.saving) } else { String::new() };
                        format!("{position}{name}{edited} — {w}×{h}{zoom} — {state}{saving}")
                    }
                }
            }
            (None, None) => format!("image-viewer — {}", t.title_empty),
        };
        let w = wide(&title);
        unsafe {
            let _ = SetWindowTextW(hwnd, windows::core::PCWSTR(w.as_ptr()));
        }
    }

    fn handle_pipeline(&mut self, event: PipelineEvent) {
        let is_current = |p: &Path, cur: &Option<Current>| cur.as_ref().is_some_and(|c| c.path == p);
        match event {
            PipelineEvent::Probed { path, info } if is_current(&path, &self.current) => {
                let cur = self.current.as_mut().expect("checked");
                if cur.info.is_none() {
                    cur.info = Some(info);
                }
            }
            PipelineEvent::Preview { path, image } if is_current(&path, &self.current) => {
                self.trace.mark(&format!("preview decoded {}x{}", image.width(), image.height()));
                self.current.as_mut().expect("checked").preview = Some(image);
            }
            PipelineEvent::Ready { path, decoded, elapsed } if is_current(&path, &self.current) => {
                self.trace.mark(&format!(
                    "decoded {} by {} in {:.1} ms",
                    file_name(&path),
                    decoded.info.codec,
                    elapsed.as_secs_f64() * 1000.0
                ));
                let cur = self.current.as_mut().expect("checked");
                cur.info = Some(decoded.info.clone());
                cur.load_time = Some(elapsed);
                cur.full = Some(decoded);
            }
            PipelineEvent::Failed { path, error } if is_current(&path, &self.current) => {
                eprintln!("{}: {error}", path.display());
                self.error = Some(format!("{}: {error}", file_name(&path)));
                // nothing truthful to show for this file
                self.display = None;
                self.invalidate();
            }
            // while a key is held, an image flipped past is shown as soon as it is decoded, so
            // the screen moves with the key instead of freezing until release
            PipelineEvent::Ready { path, decoded, .. } if self.quick_browsing => self.show_passing(path, &decoded),
            // prefetch results live in the pipeline's cache until asked for
            _ => {}
        }
        self.sync_display();
        self.update_title();
    }

    fn handle_folder(&mut self, for_path: PathBuf, result: Result<FolderList, String>) {
        if self.current.as_ref().map(|c| &c.path) != Some(&for_path) {
            return; // a newer open superseded this scan
        }
        match result {
            Ok(folder) => {
                self.trace.mark(&format!("folder listed: {} images", folder.len()));
                self.index = folder.position(&for_path);
                self.folder = Some(folder);
                self.request(for_path);
            }
            Err(e) => eprintln!("cannot list folder of {}: {e}", for_path.display()),
        }
    }

    fn center(&self) -> (f64, f64) {
        (self.viewport.0 as f64 / 2.0, self.viewport.1 as f64 / 2.0)
    }

    pub fn run_command(&mut self, cmd: Command, repeat: bool) {
        let center = self.center();
        match cmd {
            Command::FileOpen => self.open_dialog(),
            Command::FileSave => self.save_in_place(),
            Command::FileSaveAs => self.save_as(),
            Command::EditRotateCw => self.edit(Op::rotate_cw()),
            Command::EditRotateCcw => self.edit(Op::rotate_ccw()),
            Command::EditFlipH => self.edit(Op::flip_h()),
            Command::EditFlipV => self.edit(Op::flip_v()),
            Command::EditCrop => self.crop_to_selection(),
            Command::EditUndo => {
                if self.edits.undo() {
                    self.after_edit_change();
                }
            }
            Command::EditRedo => {
                if self.edits.redo() {
                    self.after_edit_change();
                }
            }
            Command::NavNext | Command::NavPrev | Command::NavFirst | Command::NavLast => self.navigate(cmd, repeat),
            Command::ViewFit => self.with_view(|v| v.fit()),
            Command::ViewActualSize => self.with_view(|v| v.actual_size_at(center)),
            Command::ViewZoomIn => self.with_view(|v| v.zoom_by(ZOOM_STEP)),
            Command::ViewZoomOut => self.with_view(|v| v.zoom_by(1.0 / ZOOM_STEP)),
            Command::AppSettings => self.toggle_settings(),
            Command::ViewToggleEditPanel => self.toggle_panel(),
            Command::ViewToggleSpread => {
                self.spread = if self.spread.is_some() { None } else { Some(self.settings.spread_right_to_left) };
                self.trace.mark(&format!("two-page view: {:?}", self.spread));
                self.refresh_spread();
                self.invalidate();
            }
            Command::ViewToggleDirection => {
                if let Some(rtl) = self.spread {
                    self.spread = Some(!rtl);
                    self.invalidate();
                }
            }
            Command::ViewToggleFullscreen => self.toggle_fullscreen(),
            Command::Escape => {
                if self.selection.take().is_some() {
                    self.invalidate();
                } else if self.fullscreen.is_some() {
                    self.toggle_fullscreen();
                } else {
                    self.quit();
                }
            }
            Command::AppQuit => self.quit(),
        }
    }

    /// Closes the window unless unsaved edits make the user change their mind.
    pub fn quit(&mut self) {
        if self.confirm_discard() {
            if let Some(h) = self.hwnd {
                unsafe {
                    let _ = DestroyWindow(h);
                }
            }
        }
    }

    fn with_view(&mut self, f: impl FnOnce(&mut ViewState)) {
        if let Some(d) = &mut self.display {
            f(&mut d.view);
            self.update_title();
            self.invalidate();
        }
    }

    fn open_dialog(&mut self) {
        let Some(hwnd) = self.hwnd else { return };
        let mut patterns: Vec<String> =
            FormatId::ALL.iter().flat_map(|f| f.extensions().iter().map(|e| format!("*.{e}"))).collect();
        patterns.extend(["*.zip", "*.cbz", "*.7z", "*.cb7"].map(String::from));
        let filters = [
            Filter { name: self.t().filter_images.into(), patterns: patterns.join(";") },
            Filter { name: self.t().filter_all.into(), patterns: "*.*".into() },
        ];
        if let Some(path) = dialogs::open_file(hwnd, &filters) {
            self.open(path);
        }
    }

    fn toggle_fullscreen(&mut self) {
        let Some(hwnd) = self.hwnd else { return };
        unsafe {
            match self.fullscreen.take() {
                Some(r) => {
                    SetWindowLongPtrW(hwnd, GWL_STYLE, r.style);
                    let _ = SetWindowPlacement(hwnd, &r.placement);
                    let _ = SetWindowPos(
                        hwnd,
                        None,
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
                    );
                }
                None => {
                    let mut placement =
                        WINDOWPLACEMENT { length: size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
                    let _ = GetWindowPlacement(hwnd, &mut placement);
                    let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
                    let monitor = windows::Win32::Graphics::Gdi::MonitorFromWindow(
                        hwnd,
                        windows::Win32::Graphics::Gdi::MONITOR_DEFAULTTONEAREST,
                    );
                    let mut mi = windows::Win32::Graphics::Gdi::MONITORINFO {
                        cbSize: size_of::<windows::Win32::Graphics::Gdi::MONITORINFO>() as u32,
                        ..Default::default()
                    };
                    let _ = windows::Win32::Graphics::Gdi::GetMonitorInfoW(monitor, &mut mi);
                    SetWindowLongPtrW(hwnd, GWL_STYLE, style & !(WS_OVERLAPPEDWINDOW.0 as isize) | WS_POPUP.0 as isize);
                    let r = mi.rcMonitor;
                    let _ = SetWindowPos(
                        hwnd,
                        Some(HWND_TOP),
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
                    );
                    self.fullscreen = Some(Restore { style, placement });
                }
            }
        }
    }

    fn toggle_panel(&mut self) {
        self.panel.open = !self.panel.open;
        self.trace.mark(&format!("edit panel: {}", if self.panel.open { "open" } else { "closed" }));
        match (self.panel.open, self.hwnd) {
            (true, Some(h)) => match panel::show(h, !self.no_activate, self.t()) {
                Ok(()) => self.sync_panel(),
                Err(e) => {
                    self.panel.open = false;
                    self.set_error(e);
                }
            },
            _ => panel::hide(),
        }
    }

    fn toggle_settings(&mut self) {
        self.settings_open = !self.settings_open;
        match (self.settings_open, self.hwnd) {
            (true, Some(h)) => {
                if let Err(e) = settings_window::show(h, &self.settings, !self.no_activate) {
                    self.settings_open = false;
                    self.set_error(e);
                }
            }
            _ => settings_window::hide(),
        }
    }

    fn handle_ui(&mut self, e: UiEvent) {
        match e {
            UiEvent::Panel(a) => self.apply_panel_action(a),
            UiEvent::SettingsChanged(s) => {
                let language_changed = s.language != self.settings.language;
                self.settings = s.clamped();
                if language_changed {
                    let t = self.t();
                    panel::set_language(t);
                    settings_window::set_language(t);
                    self.update_title();
                }
                self.panel.quality = self.settings.save_quality;
                self.settings_changed();
            }
            UiEvent::SettingsSave => self.save_settings(),
            UiEvent::SettingsClosed => {
                self.settings_open = false;
                settings_window::hide();
            }
        }
    }

    fn settings_changed(&mut self) {
        self.style =
            Style { background: self.settings.background, nearest_above_zoom: self.settings.nearest_above_zoom as f64 };
        self.sync_panel();
        self.invalidate();
    }

    fn save_settings(&mut self) {
        let Some(path) = self.settings_path.clone() else { return };
        match self.settings.save(&path) {
            Ok(()) => self.trace.mark(&format!("settings saved to {}", path.display())),
            Err(e) => self.set_error(fill(self.t().settings_error, &[("error", &e)])),
        }
    }

    fn panel_context(&self) -> PanelContext {
        let shown = self.current.as_ref().is_some_and(|c| self.display.as_ref().is_some_and(|d| d.path == c.path));
        PanelContext {
            size: if shown { self.geometry().map(|g| g.size) } else { None },
            can_undo: self.edits.can_undo(),
            can_redo: self.edits.can_redo(),
            edited: !self.edits.is_empty(),
        }
    }

    fn sync_panel(&mut self) {
        if !self.panel.open {
            return;
        }
        let cx = self.panel_context();
        if let Some(size) = cx.size {
            if self.panel.resize == (0, 0) {
                self.panel.resize = size;
            }
        }
        panel::sync(&self.panel, &cx);
    }

    fn apply_panel_action(&mut self, action: PanelAction) {
        let size = self.panel_context().size;
        match action {
            PanelAction::Adjust(a) => {
                self.panel.adjust = a;
                if self.spread.is_some() {
                    self.set_error(self.t().edit_single_page.into());
                } else if self.current.as_ref().is_some_and(|c| c.info.is_some()) {
                    self.edits.push_or_replace(Op::Adjust(a), |o| matches!(o, Op::Adjust(_)));
                    self.after_edit_change();
                }
            }
            PanelAction::Edit(op) => self.edit(op),
            PanelAction::Undo => self.run_command(Command::EditUndo, false),
            PanelAction::Redo => self.run_command(Command::EditRedo, false),
            PanelAction::Save => self.save_in_place(),
            PanelAction::SaveAs => self.save_as(),
            PanelAction::Quality(q) => {
                self.panel.quality = q;
                self.settings.save_quality = q;
            }
            PanelAction::ResizeWidth(w) => {
                self.panel.resize = match (self.panel.keep_aspect, size) {
                    (true, Some(s)) => follow_aspect(true, (w, self.panel.resize.1), s),
                    _ => (w, self.panel.resize.1),
                };
            }
            PanelAction::ResizeHeight(h) => {
                self.panel.resize = match (self.panel.keep_aspect, size) {
                    (true, Some(s)) => follow_aspect(false, (self.panel.resize.0, h), s),
                    _ => (self.panel.resize.0, h),
                };
            }
            PanelAction::ResizeHalf => {
                if let Some(s) = size {
                    self.panel.resize = (s.0.div_ceil(2), s.1.div_ceil(2));
                }
            }
            PanelAction::ResizeApply => {
                let (width, height) = self.panel.resize;
                if width > 0 && height > 0 && size != Some((width, height)) {
                    self.edit(Op::Resize { width, height });
                }
            }
            PanelAction::KeepAspect(on) => self.panel.keep_aspect = on,
            PanelAction::Closed => {
                self.panel.open = false;
                panel::hide();
            }
        }
        self.sync_panel();
        self.update_title();
    }

    /// The file or folder changed under the panel: edits are gone, fields start over.
    fn after_image_change(&mut self) {
        self.panel.adjust = Default::default();
        self.panel.resize = (0, 0);
        self.sync_panel();
        self.invalidate();
    }
}

/// Editing and saving.
impl App {
    /// Geometry of the current image with its edits, once its size is known.
    fn geometry(&self) -> Option<Geometry> {
        let info = self.current.as_ref()?.info.as_ref()?;
        Some(Geometry::new((info.width, info.height), info.orientation, self.edits.active()))
    }

    /// The view follows the edited size.
    fn apply_edits_to_view(&mut self) {
        let Some(g) = self.geometry() else { return };
        let (Some(d), Some(cur)) = (self.display.as_mut(), self.current.as_ref()) else { return };
        if d.path != cur.path || d.second.is_some() {
            return;
        }
        if d.view.image_size() != (g.size.0 as f64, g.size.1 as f64) {
            self.panel.resize = g.size;
        }
        d.view.set_image_size(g.size);
        self.invalidate();
    }

    fn after_edit_change(&mut self) {
        // the sliders show the newest adjustment in effect
        self.panel.adjust = self
            .edits
            .active()
            .iter()
            .rev()
            .find_map(|o| if let Op::Adjust(a) = o { Some(*a) } else { None })
            .unwrap_or_default();
        self.selection = None;
        if self.edits.is_empty() {
            // back to the plain image: the view returns to its unedited size
            if let (Some(d), Some(info)) = (self.display.as_mut(), self.current.as_ref().and_then(|c| c.info.as_ref()))
            {
                if d.second.is_none() {
                    d.view.set_image_size(info.display_size());
                }
            }
            self.panel.resize = self.geometry().map_or((0, 0), |g| g.size);
        }
        self.apply_edits_to_view();
        self.sync_panel();
        self.update_title();
        self.invalidate();
    }

    fn edit(&mut self, op: Op) {
        if self.spread.is_some() {
            self.set_error(self.t().edit_single_page.into());
            return;
        }
        if self.current.as_ref().is_some_and(|c| c.info.is_some()) {
            self.edits.push(op);
            self.trace.mark(&format!("edit: {op:?}"));
            self.after_edit_change();
        }
    }

    fn crop_to_selection(&mut self) {
        let crop = match (&self.display, self.selection) {
            (Some(d), Some(sel)) => selection_to_crop(&d.view, sel),
            _ => None,
        };
        if let Some(r) = crop {
            self.edit(Op::Crop(r));
        }
    }

    /// Saves the edits over the current file in its own format.
    fn save_in_place(&mut self) {
        let Some(cur) = &self.current else { return };
        if self.edits.is_empty() {
            return;
        }
        match cur.info.as_ref().and_then(|i| in_place_format(&self.encoders, i.format)) {
            Some(_) => {
                let dest = cur.path.clone();
                self.start_save(dest);
            }
            // e.g. HEIC or RAW: nothing to write them with
            None => self.save_as(),
        }
    }

    fn save_as(&mut self) {
        let (Some(cur), Some(hwnd)) = (&self.current, self.hwnd) else { return };
        let encoders: Vec<_> = self.encoders.all().map(|e| e.info().format).collect();
        let filters: Vec<Filter> = encoders
            .iter()
            .map(|f| Filter {
                name: f.name().to_string(),
                patterns: f.extensions().iter().map(|e| format!("*.{e}")).collect::<Vec<_>>().join(";"),
            })
            .collect();
        // start on the image's own format when it can be written
        let own = cur.info.as_ref().map(|i| i.format);
        let index = encoders.iter().position(|f| Some(*f) == own).unwrap_or(0);
        let ext = encoders.get(index).and_then(|f| f.extensions().first()).copied().unwrap_or("png");
        let stem = cur.path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
        // archive pages have no folder of their own on disk
        let dir = cur.path.parent().filter(|d| d.is_dir()).map(Path::to_path_buf);
        if let Some(dest) = dialogs::save_file(hwnd, &filters, dir.as_deref(), &stem, index, ext) {
            self.start_save(dest);
        }
    }

    /// Exports on a worker thread; the result arrives as [`AppEvent::Saved`].
    fn start_save(&mut self, dest: PathBuf) {
        let Some(cur) = &self.current else { return };
        if self.saving {
            return;
        }
        let req = match save_request(&self.encoders, &cur.path, self.edits.active(), dest, self.panel.quality) {
            Ok(r) => r,
            Err(e) => return self.set_error(e),
        };
        self.saving = true;
        self.update_title();
        let (tx, encoders) = (self.tx.clone(), self.encoders.clone());
        std::thread::Builder::new()
            .name("save".into())
            .spawn(move || {
                let result = export(&iv_codec::default_registry(), &encoders, &req).map_err(|e| e.to_string());
                send(&tx, AppEvent::Saved { source: req.source, dest: req.dest, result });
            })
            .expect("spawn save thread");
    }

    fn handle_saved(&mut self, source: PathBuf, dest: PathBuf, result: Result<ExportOutcome, String>) {
        self.saving = false;
        match result {
            Ok(outcome) => {
                self.trace.mark(&format!("saved {} ({outcome:?})", dest.display()));
                self.pipeline.invalidate(&dest);
                self.edits.clear();
                if dest == source {
                    // keep the old frame up until the new decode arrives, but mark it stale
                    if let Some(d) = &mut self.display {
                        d.kind = Shown::Preview;
                    }
                    self.current = None;
                    self.request(source);
                } else {
                    self.open(dest);
                }
            }
            Err(e) => {
                eprintln!("save failed: {e}");
                if let Some(h) = self.hwnd {
                    dialogs::show_error(h, &fill(self.t().save_failed, &[("error", &e)]));
                }
                self.error = Some(fill(self.t().save_failed, &[("error", &e)]));
            }
        }
        self.sync_panel();
        self.update_title();
    }

    /// Before the current image is replaced or the app closes: asks to save unsaved edits.
    /// `true` means go ahead.
    fn confirm_discard(&mut self) -> bool {
        if self.edits.is_empty() {
            return true;
        }
        let Some(hwnd) = self.hwnd else { return true };
        let name = self.current.as_ref().map(|c| file_name(&c.path)).unwrap_or_default();
        match dialogs::ask_yes_no_cancel(hwnd, &fill(self.t().ask_save_changes, &[("name", &name)])) {
            Answer::Yes => {
                let Some(cur) = &self.current else { return true };
                let format_ok = cur.info.as_ref().and_then(|i| in_place_format(&self.encoders, i.format)).is_some();
                if !format_ok {
                    self.set_error(self.t().format_not_writable.into());
                    return false;
                }
                let saved =
                    save_request(&self.encoders, &cur.path, self.edits.active(), cur.path.clone(), self.panel.quality)
                        .and_then(|req| {
                            export(&iv_codec::default_registry(), &self.encoders, &req).map_err(|e| e.to_string())
                        });
                match saved {
                    Ok(_) => {
                        let path = cur.path.clone();
                        self.pipeline.invalidate(&path);
                        self.edits.clear();
                        true
                    }
                    Err(e) => {
                        self.set_error(fill(self.t().save_failed, &[("error", &e)]));
                        false
                    }
                }
            }
            Answer::No => {
                self.edits.clear();
                true
            }
            Answer::Cancel => false,
        }
    }
}

/// Animation.
impl App {
    fn schedule_animation(&self) {
        let (Some(h), Some(p)) = (self.hwnd, self.display.as_ref().and_then(|d| d.playback.as_ref())) else { return };
        let ms = p.next_at.saturating_duration_since(Instant::now()).as_millis().clamp(1, u32::MAX as u128) as u32;
        unsafe {
            SetTimer(Some(h), ANIMATION_TIMER, ms, None);
        }
    }

    fn tick_animation(&mut self) {
        if let Some(h) = self.hwnd {
            unsafe {
                let _ = KillTimer(Some(h), ANIMATION_TIMER);
            }
        }
        let Some(d) = self.display.as_mut() else { return };
        let Some(p) = d.playback.as_mut() else { return };
        if p.advance(Instant::now()) {
            let Some(anim) = p.decoded.animation.as_ref() else { return };
            d.picture = Picture::new(&anim.frames[p.frame].image, p.decoded.info.has_alpha);
            if p.frame == 0 && !p.loop_logged {
                p.loop_logged = true;
                let expected: Duration = anim.frames.iter().map(|f| f.delay).sum();
                self.trace.mark(&format!(
                    "animation looped: {} frames in {:.0} ms (delays sum to {:.0} ms)",
                    anim.frames.len(),
                    p.started_at.elapsed().as_secs_f64() * 1000.0,
                    expected.as_secs_f64() * 1000.0
                ));
            }
            self.invalidate();
        }
        self.schedule_animation();
    }
}

/// Drawing and window messages.
impl App {
    /// Redraws the frame if the state changed since the last paint.
    pub fn render_if_dirty(&mut self, frame: &mut Frame) {
        if !self.dirty && (frame.width(), frame.height()) == self.viewport {
            return;
        }
        self.dirty = false;
        frame.resize(self.viewport.0, self.viewport.1);
        let t = Instant::now();
        let editing = self.current.as_ref().zip(self.display.as_ref()).is_some_and(|(c, d)| c.path == d.path);
        let edited = if editing && !self.edits.is_empty() { self.geometry() } else { None };
        match &self.display {
            None => raster::render(frame, &[], None, &self.style),
            Some(d) => {
                let main_g = edited.unwrap_or_else(|| Geometry::new(d.stored, d.orientation, &[]));
                let dest = d.view.dest_rect();
                let mut pages = Vec::with_capacity(2);
                match &d.second {
                    None => pages.push(Page {
                        picture: &d.picture,
                        uv: main_g.display_to_stored_uv(),
                        adjusts: &main_g.adjusts,
                        dest,
                    }),
                    Some(s) => {
                        let main_w = dest.width * s.fraction;
                        let second_w = dest.width - main_w;
                        let rtl = self.spread.unwrap_or(true);
                        let (main_x, second_x) =
                            if rtl { (dest.x + second_w, dest.x) } else { (dest.x, dest.x + main_w) };
                        let second_g = Geometry::new(s.stored, s.orientation, &[]);
                        pages.push(Page {
                            picture: &d.picture,
                            uv: main_g.display_to_stored_uv(),
                            adjusts: &main_g.adjusts,
                            dest: ScreenRect { x: main_x, width: main_w, ..dest },
                        });
                        pages.push(Page {
                            picture: &s.picture,
                            uv: second_g.display_to_stored_uv(),
                            adjusts: &[],
                            dest: ScreenRect { x: second_x, width: second_w, ..dest },
                        });
                    }
                }
                raster::render(frame, &pages, self.selection, &self.style);
            }
        }
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            self.trace.mark(&format!("first frame drawn in {:.1} ms", t.elapsed().as_secs_f64() * 1000.0));
        }
        if let (Some(cur), Some(d)) = (&mut self.current, &self.display) {
            if d.path == cur.path && d.kind == Shown::Full && !cur.shown_logged {
                cur.shown_logged = true;
                let stats = self.pipeline.stats();
                self.trace.mark(&format!(
                    "shown {} {:.1} ms after request ({}; drawn in {:.1} ms) | cache {} images, {:.0} MiB of {:.0} MiB{}",
                    file_name(&cur.path),
                    cur.requested_at.elapsed().as_secs_f64() * 1000.0,
                    if cur.load_time.is_some() { "decoded" } else { "cache hit" },
                    t.elapsed().as_secs_f64() * 1000.0,
                    stats.cached,
                    stats.used_bytes as f64 / 1048576.0,
                    self.pipeline.budget_bytes() as f64 / 1048576.0,
                    if self.quick_browsing { " | quick browsing" } else { "" }
                ));
            }
        }
    }

    fn chord(&self, vk: VIRTUAL_KEY, lp: LPARAM) -> Option<Chord> {
        let shift = key_down(VK_SHIFT);
        let scan = ((lp.0 >> 16) & 0xff) as u32;
        let name = keys::key_name(vk, scan, shift)?;
        Some(Chord::new(name, key_down(VK_CONTROL), key_down(VK_MENU), shift))
    }

    fn on_key_down(&mut self, wp: WPARAM, lp: LPARAM) -> bool {
        let Some(chord) = self.chord(VIRTUAL_KEY(wp.0 as u16), lp) else { return false };
        let Some(cmd) = self.keymap.lookup(&chord) else { return false };
        // bit 30: the key was already down (auto-repeat)
        let repeat = lp.0 & (1 << 30) != 0;
        self.run_command(cmd, repeat);
        true
    }

    fn on_key_up(&mut self, wp: WPARAM, lp: LPARAM) {
        let cmd = self.chord(VIRTUAL_KEY(wp.0 as u16), lp).and_then(|c| self.keymap.lookup(&c));
        if cmd.is_some_and(Command::is_navigation) || self.quick_browsing && cmd.is_none() {
            self.end_quick_browsing();
        }
    }

    fn menu_state(&self) -> menu::MenuState {
        menu::MenuState {
            fullscreen: self.fullscreen.is_some(),
            spread: self.spread,
            panel_open: self.panel.open,
            settings_open: self.settings_open,
            has_image: self.display.is_some(),
            can_undo: self.edits.can_undo(),
            can_redo: self.edits.can_redo(),
            edited: !self.edits.is_empty(),
            has_selection: self.selection.is_some(),
        }
    }

    /// One window message; `None` lets Windows handle it.
    pub fn on_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let point = || {
            let (x, y) = super::point_from_lparam(lp);
            (x as f64, y as f64)
        };
        match msg {
            WM_SIZE => {
                // minimised: keep the last size
                if wp.0 as u32 != SIZE_MINIMIZED {
                    let (w, h) = super::point_from_lparam(lp);
                    self.viewport = (w.max(1) as u32, h.max(1) as u32);
                    let vp = self.viewport;
                    if let Some(d) = &mut self.display {
                        d.view.set_viewport(vp);
                    }
                    self.update_title();
                    self.invalidate();
                }
                Some(LRESULT(0))
            }
            WM_KEYDOWN => {
                self.on_key_down(wp, lp);
                Some(LRESULT(0))
            }
            WM_SYSKEYDOWN => {
                // Alt chords that are bound (Alt+Enter); others (Alt+F4, Alt alone) go to Windows
                self.swallow_syschar = self.on_key_down(wp, lp);
                self.swallow_syschar.then_some(LRESULT(0))
            }
            WM_SYSCHAR if self.swallow_syschar => {
                self.swallow_syschar = false;
                Some(LRESULT(0))
            }
            WM_KEYUP | WM_SYSKEYUP => {
                self.on_key_up(wp, lp);
                None
            }
            WM_LBUTTONDOWN => {
                let p = point();
                unsafe {
                    SetCapture(hwnd);
                }
                if wp.0 as u32 & MK_SHIFT.0 != 0 {
                    self.select_from = Some(p);
                    self.selection = None;
                    self.invalidate();
                } else {
                    self.drag_from = Some(p);
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDBLCLK => {
                let p = point();
                self.with_view(|v| if v.mode() == ZoomMode::Fit { v.actual_size_at(p) } else { v.fit() });
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                let p = point();
                if let Some(from) = self.select_from {
                    self.selection = Some(drag_rect(from, p));
                    self.invalidate();
                }
                if let Some(from) = self.drag_from {
                    self.drag_from = Some(p);
                    self.with_view(|v| v.pan_by(p.0 - from.0, p.1 - from.1));
                }
                self.cursor = p;
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                self.drag_from = None;
                self.select_from = None;
                unsafe {
                    let _ = ReleaseCapture();
                }
                Some(LRESULT(0))
            }
            WM_CAPTURECHANGED => {
                self.drag_from = None;
                self.select_from = None;
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let notches = ((wp.0 >> 16) as u16 as i16) as f64 / 120.0;
                let (sx, sy) = super::point_from_lparam(lp);
                let mut pt = POINT { x: sx, y: sy };
                unsafe {
                    let _ = ScreenToClient(hwnd, &mut pt);
                }
                let anchor = (pt.x as f64, pt.y as f64);
                self.with_view(|v| v.zoom_by_at(ZOOM_STEP.powf(notches), anchor));
                Some(LRESULT(0))
            }
            WM_CONTEXTMENU => {
                let (mut x, mut y) = super::point_from_lparam(lp);
                if (x, y) == (-1, -1) {
                    // from the keyboard: at the window's centre
                    let mut pt = POINT { x: self.viewport.0 as i32 / 2, y: self.viewport.1 as i32 / 2 };
                    unsafe {
                        let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut pt);
                    }
                    (x, y) = (pt.x, pt.y);
                }
                if let Some(cmd) = menu::show(hwnd, (x, y), &self.keymap, self.t(), &self.menu_state()) {
                    self.run_command(cmd, false);
                }
                Some(LRESULT(0))
            }
            WM_DROPFILES => {
                let drop = HDROP(wp.0 as *mut _);
                let mut buf = vec![0u16; 32768];
                let n = unsafe { DragQueryFileW(drop, 0, Some(&mut buf)) } as usize;
                unsafe { DragFinish(drop) };
                if n > 0 {
                    self.open(PathBuf::from(String::from_utf16_lossy(&buf[..n])));
                }
                Some(LRESULT(0))
            }
            WM_TIMER if wp.0 == ANIMATION_TIMER => {
                self.tick_animation();
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                self.quit();
                Some(LRESULT(0))
            }
            super::WM_APP_WAKE => Some(LRESULT(0)), // the caller drains the queues
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iv_codec::{Animation, AnimationFrame};
    use iv_core::PixelFormat;

    fn animated(delays_ms: &[u64]) -> Arc<Decoded> {
        let frame = || ImageBuf::new(1, 1, PixelFormat::Rgba8, vec![0, 0, 0, 255]).unwrap();
        let info = ImageInfo {
            format: FormatId::Gif,
            codec: "test",
            width: 1,
            height: 1,
            frame_count: Some(delays_ms.len() as u32),
            has_alpha: false,
            bits_per_channel: 8,
            orientation: Orientation::Normal,
            icc_profile: None,
            exif: None,
        };
        let frames =
            delays_ms.iter().map(|ms| AnimationFrame { image: frame(), delay: Duration::from_millis(*ms) }).collect();
        Arc::new(Decoded { info, image: frame(), animation: Some(Animation { frames }), color_managed: false })
    }

    #[test]
    fn frames_follow_their_own_delays_and_loop() {
        let mut p = Playback::start(&animated(&[100, 50, 200])).unwrap();
        let t0 = p.started_at;
        assert!(!p.advance(t0 + Duration::from_millis(99)));
        assert!(p.advance(t0 + Duration::from_millis(100)));
        assert_eq!(p.frame, 1);
        assert!(p.advance(t0 + Duration::from_millis(150)));
        assert_eq!(p.frame, 2);
        assert!(p.advance(t0 + Duration::from_millis(350)));
        assert_eq!(p.frame, 0, "loops after the last frame");
    }

    #[test]
    fn late_ticks_skip_frames_instead_of_drifting() {
        let mut p = Playback::start(&animated(&[100, 100, 100])).unwrap();
        let t0 = p.started_at;
        assert!(p.advance(t0 + Duration::from_millis(250)));
        assert_eq!(p.frame, 2, "two frames were due");
        assert_eq!(p.next_at, t0 + Duration::from_millis(300), "schedule keeps its phase");
    }

    #[test]
    fn stills_do_not_play() {
        let d = Arc::new(Decoded::still(animated(&[1]).info.clone(), animated(&[1]).image.clone()));
        assert!(Playback::start(&d).is_none());
    }

    #[test]
    fn pages_share_a_height() {
        assert_eq!(spread_layout((800, 1200), (800, 1200)), ((1600, 1200), 0.5));
        let ((w, h), f) = spread_layout((800, 1200), (400, 600));
        assert_eq!((w, h), (1600, 1200));
        assert!((f - 0.5).abs() < 1e-9);
        let (_, f) = spread_layout((1600, 1200), (800, 1200));
        assert!((f - 2.0 / 3.0).abs() < 1e-9);
    }
}
