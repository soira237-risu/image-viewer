//! The viewer's state and behaviour: what is current, what is on screen, navigation, editing
//! and saving. The window procedure (`window.rs`) feeds it messages; pipeline and worker threads
//! feed it [`AppEvent`]s through a channel.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::{Duration, Instant};

use iv_codec::Decoded;
use iv_codec::encode::Encoders;
use iv_core::{FormatId, ImageBuf, ImageInfo, Orientation};
use iv_ops::{EditStack, Geometry, Op, Rect};
use iv_pipeline::export::{ExportOutcome, ExportRequest, export};
use iv_pipeline::{
    AtEnd, FolderList, Move, Pipeline, PipelineConfig, PipelineEvent, SortKey, SortOrder, entry_index, prefetch_order,
    siblings, step_or_cross,
};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{InvalidateRect, ScreenToClient};
use windows::Win32::System::SystemServices::{MK_CONTROL, MK_SHIFT};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyState, ReleaseCapture, SetCapture, VIRTUAL_KEY, VK_CONTROL, VK_MENU, VK_SHIFT,
};
use windows::Win32::UI::Shell::{DragFinish, DragQueryFileW, HDROP};
use windows::Win32::UI::WindowsAndMessaging::*;

use super::dialogs::{self, Answer, Filter};
use super::shell::{self, Clip};
use super::{UiEvent, help, keys, menu, panel, session, settings_window, wake, wide};
use crate::clipboard;
use crate::commands::Command;
use crate::diag;
use crate::edit::{
    InPlace, extension_mismatch, frames, in_place, save_request, save_request_as, selection_on_screen, selection_rect,
};
use crate::i18n::{Strings, fill};
use crate::keymap::{Chord, Keymap};
use crate::panel::{PanelAction, PanelContext, PanelState, typed_resize};
use crate::raster::{self, Frame, Page, Picture, Style};
use crate::settings::Settings;
use crate::trace::Trace;
use crate::view::{Fit, ScreenRect, ViewState, ZoomMode};
use crate::wheel::{WheelSteps, WheelUse, wheel_use};

const ZOOM_STEP: f64 = 1.25;
const ANIMATION_TIMER: usize = 1;
/// Clears a notice from the title bar.
const NOTICE_TIMER: usize = 2;
const NOTICE_TIME_MS: u32 = 6000;
/// One scroll step (an arrow key, a wheel notch) as a share of the window's height.
const SCROLL_STEP: f64 = 0.125;

#[derive(Debug)]
pub enum AppEvent {
    Pipeline(PipelineEvent),
    Folder {
        for_path: PathBuf,
        result: Result<FolderList, String>,
    },
    Saved {
        source: PathBuf,
        dest: PathBuf,
        result: Result<ExportOutcome, String>,
    },
    /// The folder or archive beside the current one, looked for by [`App::find_neighbour`]
    /// (search number `seq`): `None` when there is none with images that way.
    Neighbour {
        seq: u64,
        forward: bool,
        result: Option<FolderList>,
        elapsed: Duration,
    },
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
    /// Edits that were just saved into the file this display shows: the old pixels stay up,
    /// drawn with those edits (as the saved file looks), until the file is decoded again.
    saved_edits: Option<Vec<Op>>,
}

struct Second {
    path: PathBuf,
    picture: Picture,
    stored: (u32, u32),
    orientation: Orientation,
    /// Display (oriented) size of the page.
    size: (u32, u32),
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

/// A save running on its own thread. Until its result arrives nothing may change what it
/// writes or race it: edits, another save and renaming are refused, and moving to another
/// image or quitting waits for it ([`AfterSave`]).
struct SaveJob {
    source: PathBuf,
    /// The edits being written; they leave the edit stack only if it still holds exactly these
    /// when the save succeeds.
    ops: Vec<Op>,
}

/// What the user asked for while a save was running (or by answering Yes to "save changes?"):
/// done once the save has succeeded, dropped if it failed (the edits are still unsaved).
#[derive(Clone, Debug, PartialEq)]
enum AfterSave {
    Navigate(Command),
    Open(PathBuf),
    Quit,
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
    /// The search for the next or previous folder (or archive) that is running, by number, and
    /// whether it looks forward; a result for any other number (superseded by opening or moving
    /// elsewhere, or by a press the other way) is dropped.
    crossing: Option<(u64, bool)>,
    crossings_started: u64,
    /// Why the current file (or folder) cannot be shown; stays until another one is opened.
    error: Option<String>,
    /// A one-off message (a refused edit, a failed save): shown after the title, gone after a
    /// few seconds or at the next command.
    notice: Option<String>,
    /// What the title bar says now, so the same text is not set again (panning, prefetch).
    last_title: RefCell<String>,
    cursor: (f64, f64),
    drag_from: Option<(f64, f64)>,
    /// Shift+drag in progress: where it started, in image pixels.
    select_from: Option<(f64, f64)>,
    /// The selection in the shown image's pixels: it stays on the same part of the image while
    /// the view zooms or pans or the window resizes, and it is exactly what Crop keeps.
    selection: Option<Rect>,
    /// Wheel deltas toward the next whole notch, for wheel commands.
    wheel: WheelSteps,
    first_frame_logged: bool,
    no_activate: bool,
    edits: EditStack,
    saving: Option<SaveJob>,
    /// Waiting for the running save to succeed.
    after_save: Option<AfterSave>,
    encoders: Encoders,
    /// Two-page view: `Some(right_to_left)` when on.
    spread: Option<bool>,
    panel: PanelState,
    settings: Settings,
    settings_path: Option<PathBuf>,
    settings_open: bool,
    help_open: bool,
    style: Style,
    fullscreen: Option<Restore>,
    /// A key handled as WM_SYSKEYDOWN: swallow the WM_SYSCHAR that follows (else: a beep).
    swallow_syschar: bool,
}

fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

/// The images around `path` in `order`: its folder, or the archive it is or is in (in natural
/// order). Listing an archive parses it, so a reader crashing on a malformed one is an error
/// here like any other.
fn list_folder(path: &Path, order: SortOrder) -> Result<FolderList, String> {
    iv_core::panic::catch("archive reader", || FolderList::for_file_sorted(path, order).map_err(iv_core::Error::Io))
        .map_err(|e| match e {
            iv_core::Error::Io(io) => io.to_string(),
            other => other.to_string(),
        })
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
        session::set_placement_file(settings_path.clone());
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
            crossing: None,
            crossings_started: 0,
            error: None,
            notice: None,
            last_title: RefCell::new(String::new()),
            cursor: (0.0, 0.0),
            drag_from: None,
            select_from: None,
            selection: None,
            wheel: WheelSteps::default(),
            first_frame_logged: false,
            no_activate,
            edits: EditStack::new(),
            saving: None,
            after_save: None,
            encoders: Encoders::default(),
            spread: None,
            panel: PanelState { quality: settings.save_quality, ..PanelState::default() },
            settings,
            settings_path,
            settings_open: false,
            help_open: false,
            style,
            fullscreen: None,
            swallow_syschar: false,
        }
    }

    pub fn trace(&self, what: &str) {
        self.trace.mark(what);
    }

    /// Problems with the settings or keymap file, found before the window existed. They stay
    /// in the title bar until the first command (no timer runs before the window exists).
    pub fn startup_warnings(&mut self, settings: Option<String>, keymap: &[String]) {
        let t = self.t();
        let mut parts: Vec<String> = settings.iter().map(|e| fill(t.settings_unreadable, &[("error", e)])).collect();
        parts.extend(keymap.iter().map(|e| fill(t.keymap_warning, &[("error", e)])));
        if !parts.is_empty() {
            self.notice = Some(parts.join(" · "));
        }
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
            if let Some((w, h)) = super::take_pending_size() {
                self.set_viewport(w, h);
            } else if let Ok(e) = self.rx.try_recv() {
                match e {
                    AppEvent::Pipeline(e) => self.handle_pipeline(e),
                    AppEvent::Folder { for_path, result } => self.handle_folder(for_path, result),
                    AppEvent::Saved { source, dest, result } => self.handle_saved(source, dest, result),
                    AppEvent::Neighbour { seq, forward, result, elapsed } => {
                        self.handle_neighbour(seq, forward, result, elapsed)
                    }
                }
            } else if let Some(e) = super::take_ui() {
                self.handle_ui(e);
            } else {
                break;
            }
        }
    }

    /// The window's client area is now `w`×`h`.
    pub fn set_viewport(&mut self, w: u32, h: u32) {
        let vp = (w.max(1), h.max(1));
        if vp == self.viewport {
            return;
        }
        self.viewport = vp;
        if let Some(d) = &mut self.display {
            d.view.set_viewport(vp);
        }
        self.trace.mark(&format!(
            "viewport {}x{}{}",
            vp.0,
            vp.1,
            self.display.as_ref().map_or(String::new(), |d| format!(", image at {:?}", d.view.dest_rect()))
        ));
        self.update_title();
        self.invalidate();
    }

    /// Applies a client size that is waiting (from a `WM_SIZE` that arrived while the app was
    /// busy), before anything is drawn for the old size.
    pub fn apply_pending_size(&mut self) {
        if let Some((w, h)) = super::take_pending_size() {
            self.set_viewport(w, h);
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

    /// Opens a file (or an archive's or a folder's first image) and lists its folder in the
    /// background.
    pub fn open(&mut self, path: PathBuf) {
        if !self.confirm_discard(AfterSave::Open(path.clone())) {
            return;
        }
        self.trace.mark(&format!("open {}", path.display()));
        self.folder = None;
        self.index = None;
        self.direction = 0;
        self.crossing = None;

        // an archive or a folder: browse its images from the first
        let order = self.settings.sort_order();
        let listing = if iv_pipeline::archive::is_archive(&path) && path.is_file() {
            Some(list_folder(&path, order))
        } else if path.is_dir() {
            Some(FolderList::scan_sorted(&path, order).map_err(|e| e.to_string()))
        } else {
            None
        };
        // encoded pages kept for quick page turns belong to the archive being left
        if iv_pipeline::archive::split(&path).is_none() && !iv_pipeline::archive::is_archive(&path) {
            iv_pipeline::archive::release_cache();
        }
        if let Some(listing) = listing {
            let name = file_name(&path);
            match listing {
                Ok(list) if !list.is_empty() => {
                    let first = list.files()[0].clone();
                    self.trace.mark(&format!("listed {}: {} images", path.display(), list.len()));
                    self.folder = Some(list);
                    self.index = Some(0);
                    self.request(first);
                }
                Ok(_) => self.show_open_error(fill(self.t().no_images_inside, &[("name", &name)])),
                Err(e) => self.show_open_error(fill(self.t().cannot_open, &[("name", &name), ("error", &e)])),
            }
            return;
        }
        self.request(path.clone());
        self.scan_folder(path);
    }

    /// Lists the folder of `path` in the background; [`Self::handle_folder`] takes the result if
    /// `path` is still the current image by then.
    fn scan_folder(&self, path: PathBuf) {
        let tx = self.tx.clone();
        let order = self.settings.sort_order();
        std::thread::Builder::new()
            .name("folder-scan".into())
            .spawn(move || {
                // listing an archive parses it: a reader crashing on a malformed one fails the listing
                let result = list_folder(&path, order);
                send(&tx, AppEvent::Folder { for_path: path, result });
            })
            .expect("spawn folder scan");
    }

    /// The UI's words in the chosen language.
    fn t(&self) -> &'static Strings {
        self.settings.language.strings()
    }

    /// Nothing could be opened (an empty or unreadable folder or archive): the previous image
    /// is no longer current, and the title says why until something else is opened.
    fn show_open_error(&mut self, e: String) {
        self.current = None;
        self.display = None;
        self.edits.clear();
        self.selection = None;
        self.error = Some(e);
        self.after_image_change();
        self.update_title();
    }

    /// A one-off message after the title; it goes away after a few seconds or at the next
    /// command, and never hides the file name, size or unsaved-edit mark.
    fn set_notice(&mut self, message: String) {
        self.notice = Some(message);
        if let Some(h) = self.hwnd {
            unsafe {
                SetTimer(Some(h), NOTICE_TIMER, NOTICE_TIME_MS, None);
            }
        }
        self.update_title();
    }

    fn clear_notice(&mut self) {
        if self.notice.take().is_some() {
            if let Some(h) = self.hwnd {
                unsafe {
                    let _ = KillTimer(Some(h), NOTICE_TIMER);
                }
            }
            self.update_title();
        }
    }

    /// Paths to prefetch around the current index, nearest first.
    fn prefetch_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = match (&self.folder, self.index) {
            (Some(folder), Some(i)) if !self.quick_browsing => {
                let n = self.settings.prefetch_count;
                let count = if self.spread.is_some() { n * 2 } else { n };
                // never across into another folder: that one is not known until stepping there
                let wrap = self.settings.at_end == AtEnd::Wrap;
                prefetch_order(i, folder.len(), self.direction, count, wrap)
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
            self.select_from = None;
            self.after_image_change();
        }
        if let (Some(cur), Some(decoded)) = (self.current.as_mut(), hit)
            && cur.full.is_none()
        {
            cur.info = Some(decoded.info.clone());
            cur.full = Some(decoded);
        }
        self.sync_display();
        self.update_title();
    }

    fn navigate(&mut self, cmd: Command, repeat: bool) {
        let (Some(folder), Some(index)) = (&self.folder, self.index) else { return };
        let len = folder.len();
        let stride = if self.spread.is_some() { 2 } else { 1 };
        let at_end = self.settings.at_end;
        let step = match cmd {
            Command::NavNext => step_or_cross(index, len, stride, at_end, repeat),
            Command::NavPrev => step_or_cross(index, len, -stride, at_end, repeat),
            Command::NavFirst if index != 0 => Move::To(0),
            Command::NavLast if index + 1 != len => Move::To(len - 1),
            _ => Move::Stay,
        };
        let target = match step {
            Move::To(target) => target,
            Move::Cross { forward } => return self.find_neighbour(forward),
            Move::Stay => return,
        };
        let Some(path) = folder.get(target).map(Path::to_path_buf) else { return };
        if !self.confirm_discard(AfterSave::Navigate(cmd)) {
            return;
        }
        // moved on within this folder: a folder search started before is no longer wanted
        self.crossing = None;
        self.direction = if matches!(cmd, Command::NavPrev | Command::NavFirst) { -1 } else { 1 };
        self.index = Some(target);
        if repeat {
            self.quick_browsing = true;
        }
        self.request(path);
    }

    /// Stepped past the end of the list (`forward`) or before its start with "go on to the next
    /// folder" set: looks for the sibling folder or archive with images on a thread of its own
    /// (it may list many folders or open many archives). [`Self::handle_neighbour`] takes the
    /// answer. Another press the same way while a search runs is ignored; a press the other way
    /// starts a search that way, and the first one's answer is dropped.
    fn find_neighbour(&mut self, forward: bool) {
        let Some(folder) = &self.folder else { return };
        if self.crossing.is_some_and(|(_, way)| way == forward) {
            return;
        }
        self.crossings_started += 1;
        let seq = self.crossings_started;
        self.crossing = Some((seq, forward));
        let (current, archive, order) = (folder.dir().to_path_buf(), folder.is_archive(), self.settings.sort_order());
        self.trace.mark(&format!(
            "looking for the {} {} beside {}",
            if forward { "next" } else { "previous" },
            if archive { "archive" } else { "folder" },
            current.display()
        ));
        let tx = self.tx.clone();
        let spawned = std::thread::Builder::new().name("neighbour-search".into()).spawn(move || {
            let started = Instant::now();
            let result = siblings::neighbour(&current, archive, forward, order);
            send(&tx, AppEvent::Neighbour { seq, forward, result, elapsed: started.elapsed() });
        });
        if let Err(e) = spawned {
            // no thread to be had (out of memory): stay, and the next press tries again
            self.crossing = None;
            self.trace.mark(&format!("neighbour search not started: {e}"));
        }
    }

    /// The answer to [`Self::find_neighbour`]: shows the first image of the next folder (or the
    /// last of the previous one) and names it, or says there is none and stays.
    fn handle_neighbour(&mut self, seq: u64, forward: bool, result: Option<FolderList>, elapsed: Duration) {
        if self.crossing.map(|(running, _)| running) != Some(seq) {
            return; // the user opened or moved to something else meanwhile, or turned back
        }
        self.crossing = None;
        let archive = self.folder.as_ref().is_some_and(FolderList::is_archive);
        self.trace.mark(&format!(
            "neighbour search done in {:.1} ms: {}",
            elapsed.as_secs_f64() * 1000.0,
            result
                .as_ref()
                .map_or_else(|| "none".to_string(), |l| format!("{} ({} images)", l.dir().display(), l.len()))
        ));
        let Some(list) = result else {
            let t = self.t();
            let notice = match (archive, forward) {
                (false, true) => t.no_next_folder,
                (false, false) => t.no_previous_folder,
                (true, true) => t.no_next_archive,
                (true, false) => t.no_previous_archive,
            };
            return self.set_notice(notice.into());
        };
        // unsaved edits are asked about only now that there is somewhere to go
        let cmd = if forward { Command::NavNext } else { Command::NavPrev };
        if !self.confirm_discard(AfterSave::Navigate(cmd)) {
            return;
        }
        self.enter_list(list, forward);
    }

    /// Makes `list` (a sibling folder or archive) the one being browsed, entered going
    /// `forward` (at its first image) or back (at its last), and names it in a notice.
    fn enter_list(&mut self, list: FolderList, forward: bool) {
        let stride = if self.spread.is_some() { 2 } else { 1 };
        let index = entry_index(list.len(), forward, stride);
        let Some(path) = list.get(index).map(Path::to_path_buf) else { return };
        let name = file_name(list.dir());
        let entered = if list.is_archive() { self.t().entered_archive } else { self.t().entered_folder };
        self.trace.mark(&format!("entered {} at [{}/{}]", list.dir().display(), index + 1, list.len()));
        // encoded pages kept for quick page turns belong to the archive being left
        iv_pipeline::archive::release_cache();
        self.folder = Some(list);
        self.index = Some(index);
        self.direction = if forward { 1 } else { -1 };
        self.request(path);
        self.set_notice(fill(entered, &[("name", &name)]));
    }

    /// The sort setting changed: the open folder is sorted again and the image on screen stays
    /// (the images around it, and so the prefetch, are new).
    fn resort(&mut self) {
        let order = self.settings.sort_order();
        let Some(folder) = self.folder.as_mut() else { return };
        if folder.is_archive() || folder.order() == order {
            return;
        }
        folder.set_order(order);
        let Some(path) = self.current.as_ref().map(|c| c.path.clone()) else { return };
        self.index = folder.position(&path);
        self.trace.mark(&format!("folder sorted by {order:?}: current at {:?}", self.index.map(|i| i + 1)));
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
        let open = self.settings.zoom_on_open;
        let Some(cur) = self.current.as_mut() else { return };
        let same_image = self.display.as_ref().is_some_and(|d| d.path == cur.path);
        let shown = self.display.as_ref().filter(|_| same_image).map(|d| d.kind);

        let (image, info, kind) = match (&cur.full, &cur.preview, &cur.info) {
            (Some(full), _, _) if shown != Some(Shown::Full) => (&full.image, &full.info, Shown::Full),
            (None, Some(preview), Some(info)) if shown.is_none() => (preview, info, Shown::Preview),
            _ => {
                // the full image is up already, e.g. shown while flipping past it with a key
                // held (that display does not play): start its animation now that it is current
                if let (Some(d), Some(full)) = (self.display.as_mut().filter(|_| same_image), &cur.full)
                    && d.kind == Shown::Full
                    && d.playback.is_none()
                    && full.animation.is_some()
                {
                    d.playback = Playback::start(full);
                    self.trace.mark("animation started on the image already shown");
                    self.schedule_animation();
                }
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
                // the file as saved has arrived: its pixels already include the edits
                d.saved_edits = None;
                if d.second.is_none() {
                    d.view.set_image_size(size);
                }
            }
            _ => {
                // a selection drawn on the image that was up is in that image's pixels
                self.selection = None;
                self.select_from = None;
                self.display = Some(Display {
                    path: cur.path.clone(),
                    kind,
                    view: ViewState::opened(size, viewport, open),
                    picture,
                    stored,
                    orientation: info.orientation,
                    playback: None,
                    second: None,
                    saved_edits: None,
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
        let Some(main_size) = self.main_size() else { return };
        let Some(cur) = &self.current else { return };
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
                let size = page.info.display_size();
                let (total, fraction) = spread_layout(main_size, size);
                d.second = Some(Second {
                    path: partner,
                    picture: Picture::new(&page.image, page.info.has_alpha),
                    stored: (page.info.width, page.info.height),
                    orientation: page.info.orientation,
                    size,
                    fraction,
                });
                d.view.set_image_size(total);
                self.invalidate();
            }
            Some(_) => {}
        }
    }

    /// Display size of the current image as drawn: with its edits, if any.
    fn main_size(&self) -> Option<(u32, u32)> {
        let info = self.current.as_ref()?.info.as_ref()?;
        if self.edits.is_empty() { Some(info.display_size()) } else { self.geometry().map(|g| g.size) }
    }

    /// Two-page view: the edited size of the current page changed (an undo or redo while two
    /// pages are shown), so the pages share the width anew.
    fn relayout_second(&mut self) {
        let Some(main_size) = self.main_size() else { return };
        let Some(cur) = &self.current else { return };
        let Some(d) = self.display.as_mut().filter(|d| d.path == cur.path) else { return };
        let Some(s) = d.second.as_mut() else { return };
        let (total, fraction) = spread_layout(main_size, s.size);
        s.fraction = fraction;
        d.view.set_image_size(total);
        self.invalidate();
    }

    /// Shows an image that is no longer current (quick browsing), unless the current one is
    /// already on screen.
    fn show_passing(&mut self, path: PathBuf, decoded: &Decoded) {
        if self.current_shown() {
            return;
        }
        // a selection belongs to the image it was drawn on
        self.selection = None;
        self.select_from = None;
        self.trace.mark(&format!("passing image shown: {}", file_name(&path)));
        let info = &decoded.info;
        self.display = Some(Display {
            path,
            kind: Shown::Full,
            view: ViewState::opened(info.display_size(), self.viewport, self.settings.zoom_on_open),
            picture: Picture::new(&decoded.image, info.has_alpha),
            stored: (info.width, info.height),
            orientation: info.orientation,
            playback: None,
            second: None,
            saved_edits: None,
        });
        self.invalidate();
    }

    /// Whether the screen shows the current image. It does not while the next image loads: the
    /// previous one stays up until the new one has something to show.
    fn current_shown(&self) -> bool {
        matches!((&self.current, &self.display), (Some(c), Some(d)) if d.path == c.path)
    }

    /// Whether a command on the current file acts on what the user sees: the image on screen, or
    /// a file that cannot be shown at all (the title bar names it). Otherwise says so in a notice.
    fn file_in_view(&mut self, path: &Path) -> bool {
        let unreadable = self.error.is_some() && self.display.is_none();
        if self.current_shown() || unreadable {
            return true;
        }
        self.set_notice(fill(self.t().not_shown_yet, &[("name", &file_name(path))]));
        false
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
                        let saving = if self.saving.is_some() { format!(" · {}", t.saving) } else { String::new() };
                        let mismatch = if extension_mismatch(&cur.path, info.format) {
                            format!(" · {}", t.extension_mismatch)
                        } else {
                            String::new()
                        };
                        format!("{position}{name}{edited} — {w}×{h}{zoom} — {state}{mismatch}{saving}")
                    }
                }
            }
            (None, None) => format!("image-viewer — {}", t.title_empty),
        };
        let title = match &self.notice {
            Some(notice) => format!("{title} — {notice}"),
            None => title,
        };
        // setting the same text again still repaints the caption; panning would do it per move
        if *self.last_title.borrow() == title {
            return;
        }
        let w = wide(&title);
        unsafe {
            let _ = SetWindowTextW(hwnd, windows::core::PCWSTR(w.as_ptr()));
        }
        *self.last_title.borrow_mut() = title;
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
            PipelineEvent::Failed { path, error, kind } if is_current(&path, &self.current) => {
                diag::log(&format!("{}: {error}", path.display()));
                self.error = Some(self.t().open_failed(&file_name(&path), kind, &error));
                // nothing truthful to show for this file: not even its embedded preview, which
                // would look like a healthy (blurry) image
                self.display = None;
                self.current.as_mut().expect("checked").preview = None;
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
                let mut folder = folder;
                // the sort setting may have changed while the folder was being listed
                folder.set_order(self.settings.sort_order());
                self.index = folder.position(&for_path);
                self.folder = Some(folder);
                self.request(for_path);
            }
            Err(e) => {
                // the image is shown, but without its folder there is no previous or next one
                diag::log(&format!("cannot list folder of {}: {e}", for_path.display()));
                let name = file_name(&for_path);
                self.set_notice(fill(self.t().folder_list_failed, &[("name", &name), ("error", &e)]));
            }
        }
    }

    fn center(&self) -> (f64, f64) {
        (self.viewport.0 as f64 / 2.0, self.viewport.1 as f64 / 2.0)
    }

    pub fn run_command(&mut self, cmd: Command, repeat: bool) {
        let center = self.center();
        // a notice is about the last thing done; the next command makes it stale
        self.clear_notice();
        match cmd {
            Command::FileOpen => self.open_dialog(),
            Command::FileSave => self.save_in_place(None),
            Command::FileSaveAs => self.save_as(None),
            Command::FileFixExtension => self.fix_extension(),
            Command::FileDelete => self.delete_current(),
            Command::FileCopyFile => self.copy_file(),
            Command::FileCopyPath => self.copy_path(),
            Command::FileShowInFolder => self.show_in_folder(),
            Command::EditCopyImage => self.copy_image(),
            Command::AppAbout => self.about(),
            Command::EditRotateCw => self.edit(Op::rotate_cw()),
            Command::EditRotateCcw => self.edit(Op::rotate_ccw()),
            Command::EditFlipH => self.edit(Op::flip_h()),
            Command::EditFlipV => self.edit(Op::flip_v()),
            Command::EditCrop => self.crop_to_selection(),
            Command::EditUndo => {
                if !self.busy_saving() && self.edits.undo() {
                    self.after_edit_change();
                }
            }
            Command::EditRedo => {
                if !self.busy_saving() && self.edits.redo() {
                    self.after_edit_change();
                }
            }
            Command::NavNext | Command::NavPrev | Command::NavFirst | Command::NavLast => self.navigate(cmd, repeat),
            Command::ViewFit => self.with_view(|v| v.fit()),
            Command::ViewActualSize => self.with_view(|v| v.actual_size_at(center)),
            Command::ViewZoomIn => self.with_view(|v| v.zoom_by(ZOOM_STEP)),
            Command::ViewZoomOut => self.with_view(|v| v.zoom_by(1.0 / ZOOM_STEP)),
            Command::ViewScrollUp | Command::ViewScrollDown => {
                let step = f64::from(self.viewport.1) * SCROLL_STEP;
                let dy = if cmd == Command::ViewScrollDown { step } else { -step };
                self.with_view(|v| {
                    v.scroll_by(dy);
                });
            }
            Command::AppSettings => self.toggle_settings(),
            Command::AppHelp => self.toggle_help(),
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
        let go = self.confirm_discard(AfterSave::Quit);
        // a close asked for again while that question was up has had its answer
        session::take_deferred_close();
        if go && let Some(h) = self.hwnd {
            self.remember_placement(h);
            unsafe {
                let _ = DestroyWindow(h);
            }
        }
    }

    /// Writes where the window is to the window state file next to the settings file (never
    /// the settings file itself), so the next start opens it there: see
    /// [`session::save_placement`].
    fn remember_placement(&self, hwnd: HWND) {
        match session::save_placement(hwnd) {
            Some(Ok(saved)) => self.trace.mark(&format!("window placement saved: {saved:?}")),
            Some(Err(e)) => diag::log(&format!("window placement not saved: {e}")),
            None => {}
        }
    }

    /// Tells Windows what ending the session now would lose (see [`session`]): a running save,
    /// which logoff waits for, or unsaved edits, which hold an ordinary logoff or shutdown so the
    /// user can go back and save them.
    pub fn publish_session(&self) {
        let Some(hwnd) = self.hwnd else { return };
        let t = self.t();
        let (stake, reason) = match (&self.saving, &self.current) {
            (Some(job), _) => (session::Stake::Saving, fill(t.shutdown_saving, &[("name", &file_name(&job.source))])),
            (None, Some(cur)) if !self.edits.is_empty() => {
                (session::Stake::Unsaved, fill(t.shutdown_unsaved, &[("name", &file_name(&cur.path))]))
            }
            _ => (session::Stake::Nothing, String::new()),
        };
        session::publish(hwnd, stake, reason);
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
        patterns.extend(iv_pipeline::archive::extensions().map(|e| format!("*.{e}")));
        let filters = [
            Filter { name: self.t().filter_images.into(), patterns: patterns.join(";") },
            Filter { name: self.t().filter_all.into(), patterns: "*.*".into() },
        ];
        if let Some(path) = dialogs::open_file(hwnd, &filters) {
            self.open(path);
        }
    }

    /// Full screen at start (the `startup_window` setting).
    pub fn start_fullscreen(&mut self) {
        if self.fullscreen.is_none() {
            self.trace.mark("full screen at start");
            self.toggle_fullscreen();
        }
    }

    fn toggle_fullscreen(&mut self) {
        let Some(hwnd) = self.hwnd else { return };
        unsafe {
            match self.fullscreen.take() {
                Some(r) => {
                    session::set_fullscreen_placement(None);
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
                    session::set_fullscreen_placement(Some(placement));
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
                    self.set_notice(e);
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
                    self.set_notice(e);
                }
            }
            _ => settings_window::hide(),
        }
    }

    fn help_rows(&self) -> Vec<crate::help::Row> {
        crate::help::rows(&self.keymap, self.t(), self.settings.wheel_navigates)
    }

    fn toggle_help(&mut self) {
        self.help_open = !self.help_open;
        self.trace.mark(&format!("help: {}", if self.help_open { "open" } else { "closed" }));
        match (self.help_open, self.hwnd) {
            (true, Some(h)) => {
                if let Err(e) = help::show(h, self.t(), &self.help_rows(), !self.no_activate) {
                    self.help_open = false;
                    self.set_notice(e);
                }
            }
            _ => help::hide(),
        }
    }

    fn handle_ui(&mut self, e: UiEvent) {
        match e {
            UiEvent::Panel(a) => self.apply_panel_action(a),
            UiEvent::SettingsChanged(s) => {
                let language_changed = s.language != self.settings.language;
                let help_changed = language_changed || s.wheel_navigates != self.settings.wheel_navigates;
                let zoom_changed = s.zoom_on_open != self.settings.zoom_on_open;
                let sort_changed = s.sort_order() != self.settings.sort_order();
                self.settings = s.clamped();
                if zoom_changed {
                    // the image on screen shows the new way at once
                    let open = self.settings.zoom_on_open;
                    self.with_view(|v| v.set_open(open));
                }
                if sort_changed {
                    self.resort();
                }
                if language_changed {
                    let t = self.t();
                    panel::set_language(t);
                    settings_window::set_language(t);
                    self.update_title();
                }
                if help_changed {
                    help::refresh(self.t(), &self.help_rows());
                }
                self.panel.quality = self.settings.save_quality;
                self.settings_changed();
            }
            UiEvent::SettingsSave => self.save_settings(),
            UiEvent::SettingsClosed => {
                self.settings_open = false;
                settings_window::hide();
            }
            UiEvent::HelpClosed => {
                self.help_open = false;
                help::hide();
            }
            UiEvent::Register(on) => {
                let t = self.t();
                let result = if on { shell::register(t) } else { shell::unregister() };
                self.trace
                    .mark(&format!("open-with registration {}: {result:?}", if on { "added" } else { "removed" }));
                match result {
                    Ok(()) => self.set_notice(if on { t.registered } else { t.unregistered }.into()),
                    Err(e) => {
                        diag::log(&format!("open-with registration: {e}"));
                        self.set_notice(fill(t.register_failed, &[("error", &e)]));
                    }
                }
            }
            UiEvent::OpenDefaultApps => {
                let Some(hwnd) = self.hwnd else { return };
                if let Err(e) = shell::open_uri(hwnd, shell::DEFAULT_APPS_URI) {
                    self.set_notice(fill(self.t().default_apps_failed, &[("error", &e)]));
                }
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
            Ok(None) => self.trace.mark(&format!("settings saved to {}", path.display())),
            // the file it replaced could not be read (the warning at startup): say where it went
            Ok(Some(kept)) => {
                self.trace.mark(&format!("settings saved to {}; old file kept as {}", path.display(), kept.display()));
                self.set_notice(fill(self.t().settings_backed_up, &[("name", &file_name(&kept))]));
            }
            Err(e) => self.set_notice(fill(self.t().settings_error, &[("error", &e)])),
        }
    }

    fn panel_context(&self) -> PanelContext {
        let shown = self.current.as_ref().is_some_and(|c| self.display.as_ref().is_some_and(|d| d.path == c.path));
        PanelContext {
            size: if shown { self.geometry().map(|g| g.size) } else { None },
            can_undo: self.edits.can_undo(),
            can_redo: self.edits.can_redo(),
            // Save is greyed out while the edits are being saved
            edited: !self.edits.is_empty() && self.saving.is_none(),
        }
    }

    fn sync_panel(&mut self) {
        if !self.panel.open {
            return;
        }
        let cx = self.panel_context();
        if let Some(size) = cx.size
            && self.panel.resize == (0, 0)
        {
            self.panel.resize = size;
        }
        panel::sync(&self.panel, &cx);
    }

    fn apply_panel_action(&mut self, action: PanelAction) {
        let size = self.panel_context().size;
        self.clear_notice();
        match action {
            // the slider springs back (sync_panel below) to the adjustment being saved
            PanelAction::Adjust(_) if self.busy_saving() => {}
            PanelAction::Adjust(a) => {
                self.panel.adjust = a;
                if self.spread.is_some() {
                    self.set_notice(self.t().edit_single_page.into());
                } else if self.current.as_ref().is_some_and(|c| c.info.is_some()) {
                    self.edits.push_or_replace(Op::Adjust(a), |o| matches!(o, Op::Adjust(_)));
                    self.after_edit_change();
                }
            }
            PanelAction::Edit(op) => self.edit(op),
            PanelAction::Undo => self.run_command(Command::EditUndo, false),
            PanelAction::Redo => self.run_command(Command::EditRedo, false),
            PanelAction::Save => self.save_in_place(None),
            PanelAction::SaveAs => self.save_as(None),
            PanelAction::Quality(q) => {
                self.panel.quality = q;
                self.settings.save_quality = q;
                // the settings window edits a copy of the settings: keep its quality in step
                settings_window::set_quality(q);
            }
            PanelAction::ResizeWidth(w) => {
                self.panel.resize = typed_resize(self.panel.resize, self.panel.keep_aspect, size, true, w);
            }
            PanelAction::ResizeHeight(h) => {
                self.panel.resize = typed_resize(self.panel.resize, self.panel.keep_aspect, size, false, h);
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

    /// The view follows the edited size (in two-page view, the pages' shared layout does).
    fn apply_edits_to_view(&mut self) {
        let Some(g) = self.geometry() else { return };
        let (Some(d), Some(cur)) = (self.display.as_mut(), self.current.as_ref()) else { return };
        if d.path != cur.path {
            return;
        }
        if d.second.is_some() {
            self.relayout_second();
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
                && d.second.is_none()
            {
                d.view.set_image_size(info.display_size());
            }
            self.panel.resize = self.geometry().map_or((0, 0), |g| g.size);
        }
        self.apply_edits_to_view();
        self.sync_panel();
        self.update_title();
        self.invalidate();
    }

    fn edit(&mut self, op: Op) {
        if self.busy_saving() {
            return;
        }
        if self.spread.is_some() {
            self.set_notice(self.t().edit_single_page.into());
            return;
        }
        if self.current.as_ref().is_some_and(|c| c.info.is_some()) {
            self.edits.push(op);
            self.trace.mark(&format!("edit: {op:?}"));
            self.after_edit_change();
        }
    }

    fn crop_to_selection(&mut self) {
        // the selection is in the pixels of the image on screen, which must be the current one
        if let Some(r) = self.selection.filter(|_| self.current_shown()) {
            self.edit(Op::Crop(r));
        }
    }

    /// A save is running: says so and returns `true`, so the caller leaves alone what the save
    /// is writing.
    fn busy_saving(&mut self) -> bool {
        if self.saving.is_some() {
            self.set_notice(self.t().save_busy.into());
        }
        self.saving.is_some()
    }

    /// Saves the edits over the current file in the format of its content (a WebP named `.jpg`
    /// stays WebP). An animation, a page inside an archive or a format nothing can write goes
    /// to Save As instead, with the reason in the title bar. While only the header has been
    /// read and it cannot tell a still image from an animation, nothing is saved and nothing
    /// waiting on the save (`then`) happens; the title bar says to try again. `then` runs once
    /// the save has succeeded.
    fn save_in_place(&mut self, then: Option<AfterSave>) {
        if self.busy_saving() {
            return;
        }
        let Some(cur) = &self.current else { return };
        let Some(info) = &cur.info else { return };
        if self.edits.is_empty() {
            return;
        }
        let target = in_place(&self.encoders, &cur.path, info.format, frames(info, cur.full.as_deref()))
            .keeping_depth(info.bits_per_channel);
        self.trace.mark(&format!("save in place: {target:?}"));
        match target {
            InPlace::NotLoaded => self.set_notice(self.t().save_not_loaded.into()),
            InPlace::Write(format) => {
                let req = save_request_as(
                    &self.encoders,
                    &cur.path,
                    self.edits.active(),
                    cur.path.clone(),
                    format,
                    self.panel.quality,
                );
                match req {
                    Ok(req) => self.start_save(req, then),
                    Err(e) => self.set_notice(e.message(self.t())),
                }
            }
            refused => {
                if let Some(reason) = refused.reason(self.t()) {
                    self.set_notice(reason.into());
                }
                self.save_as(then);
            }
        }
    }

    /// Asks for a file name and saves there; the format follows the name's extension.
    fn save_as(&mut self, then: Option<AfterSave>) {
        if self.busy_saving() {
            return;
        }
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
        let Some(dest) = dialogs::save_file(hwnd, &filters, dir.as_deref(), &stem, index, ext) else { return };
        let Some(cur) = &self.current else { return };
        match save_request(&self.encoders, &cur.path, self.edits.active(), dest, self.panel.quality) {
            Ok(req) => self.start_save(req, then),
            Err(e) => self.set_notice(e.message(self.t())),
        }
    }

    /// Exports on a worker thread; the result arrives as [`AppEvent::Saved`]. Until then the
    /// save is isolated: see [`SaveJob`].
    fn start_save(&mut self, req: ExportRequest, then: Option<AfterSave>) {
        if self.saving.is_some() {
            return;
        }
        let ops = req.ops.clone();
        let source = req.source.clone();
        let (tx, encoders) = (self.tx.clone(), self.encoders.clone());
        let thread = std::thread::Builder::new()
            .name("save".into())
            .spawn(move || {
                // an encoder crashing must still report back, or the viewer would wait for this save forever
                let result = iv_core::panic::catch("save", || export(&iv_codec::default_registry(), &encoders, &req))
                    .map_err(|e| e.to_string());
                send(&tx, AppEvent::Saved { source: req.source, dest: req.dest, result });
            })
            .expect("spawn save thread");
        self.trace.mark(&format!("saving {}", source.display()));
        // kept where logoff can wait for it, even while a dialog has the app (see `session`)
        session::set_save_thread(thread);
        self.saving = Some(SaveJob { source, ops });
        self.after_save = then;
        self.sync_panel();
        self.update_title();
    }

    fn handle_saved(&mut self, source: PathBuf, dest: PathBuf, result: Result<ExportOutcome, String>) {
        let Some(job) = self.saving.take() else { return };
        // the thread has sent its last word: this returns at once
        session::finish_save();
        let then = self.after_save.take();
        match result {
            Ok(outcome) => {
                if then.is_some() {
                    // "will continue when the save finishes": it is happening now
                    self.clear_notice();
                }
                self.trace.mark(&format!("saved {} ({outcome:?})", dest.display()));
                self.pipeline.invalidate(&dest);
                // the edits are in the file now, unless the image or its edits changed since the
                // save started: those are kept as unsaved edits
                let unchanged = self.current.as_ref().is_some_and(|c| c.path == job.source)
                    && self.edits.active() == job.ops.as_slice();
                let saved_edits = self.edits.active().to_vec();
                if unchanged {
                    self.edits.clear();
                }
                if let ExportOutcome::Encoded { dropped_frames, .. } = outcome
                    && dropped_frames > 0
                {
                    self.set_notice(fill(self.t().first_frame_only, &[("count", &dropped_frames.to_string())]));
                }
                if dest == source {
                    if unchanged {
                        // keep the old frame up until the new decode arrives, marked stale, and
                        // drawn with the edits it was saved with: without them the old pixels
                        // would be stretched into the edited (rotated, cropped) size
                        if let Some(d) = self.display.as_mut().filter(|d| d.path == source) {
                            d.kind = Shown::Preview;
                            d.saved_edits = Some(saved_edits);
                        }
                        self.current = None;
                        self.request(source);
                    }
                } else if then.is_none() && unchanged {
                    self.open(dest);
                }
                if let Some(then) = then {
                    self.trace.mark(&format!("after the save: {then:?}"));
                    match then {
                        AfterSave::Navigate(cmd) => self.navigate(cmd, false),
                        AfterSave::Open(path) => self.open(path),
                        AfterSave::Quit => self.quit(),
                    }
                }
            }
            Err(e) => {
                // what was waiting for the save does not happen: the edits are still unsaved
                diag::log(&format!("save failed: {e}"));
                let message = fill(self.t().save_failed, &[("error", &e)]);
                self.set_notice(message.clone());
                if let Some(h) = self.hwnd {
                    dialogs::show_error(h, &message);
                }
            }
        }
        self.sync_panel();
        self.update_title();
    }

    /// Before the current image is replaced or the app closes (`then` says which): asks to
    /// save unsaved edits. `true` means go ahead now. On Yes the save runs like Save (in the
    /// background, or through Save As when the file cannot take it) and `then` follows once it
    /// has succeeded. While a save is running, `then` waits for it instead.
    fn confirm_discard(&mut self, then: AfterSave) -> bool {
        if self.saving.is_some() {
            // the latest request wins, except that a pending close is never dropped for a
            // navigation asked for after it
            if !matches!(self.after_save, Some(AfterSave::Quit)) {
                self.after_save = Some(then);
            }
            self.set_notice(self.t().continue_after_save.into());
            return false;
        }
        if self.edits.is_empty() {
            return true;
        }
        let Some(hwnd) = self.hwnd else { return true };
        let name = self.current.as_ref().map(|c| file_name(&c.path)).unwrap_or_default();
        match dialogs::ask_yes_no_cancel(hwnd, &fill(self.t().ask_save_changes, &[("name", &name)])) {
            Answer::Yes => {
                self.save_in_place(Some(then));
                false
            }
            Answer::No => {
                self.edits.clear();
                true
            }
            Answer::Cancel => false,
        }
    }

    /// Renames the current file so its extension matches its content (a WebP named `.jpg`
    /// becomes `.webp`). Never overwrites a file; edits and the view stay as they are.
    fn fix_extension(&mut self) {
        if self.busy_saving() {
            return;
        }
        let Some(cur) = &self.current else { return };
        let old = cur.path.clone();
        if iv_pipeline::archive::split(&old).is_some() {
            return self.set_notice(self.t().archive_page_rename.into());
        }
        if !self.file_in_view(&old) {
            return;
        }
        let t = self.t();
        match iv_pipeline::export::fix_extension(&iv_codec::default_registry(), &old) {
            Ok(None) => self.set_notice(t.extension_ok.into()),
            Ok(Some(new)) => {
                self.trace.mark(&format!("renamed {} -> {}", old.display(), new.display()));
                self.renamed(&old, &new);
                self.set_notice(fill(t.extension_fixed, &[("name", &file_name(&new))]));
            }
            Err(iv_core::Error::Io(e)) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let target = old.with_extension(
                    self.current.as_ref().and_then(|c| c.info.as_ref()).map_or("", |i| i.format.canonical_extension()),
                );
                self.set_notice(fill(t.extension_exists, &[("name", &file_name(&target))]));
            }
            Err(e) => self.set_notice(fill(t.extension_fix_failed, &[("error", &e.to_string())])),
        }
    }

    /// The current file is now called `new`: everything that knew it by `old` follows.
    fn renamed(&mut self, old: &Path, new: &Path) {
        self.pipeline.invalidate(old);
        let followed = self.folder.as_mut().and_then(|folder| folder.rename(old, new));
        // without a list that followed (the folder scan, for the old name, has not come back
        // yet and will be dropped as stale, or the list does not know the file), list the
        // folder again under the new name rather than keep an entry for a file that is gone
        let rescan = followed.is_none();
        if rescan {
            self.folder = None;
        }
        self.index = followed;
        if let Some(d) = self.display.as_mut().filter(|d| d.path == old) {
            d.path = new.to_path_buf();
        }
        let Some(cur) = self.current.as_mut() else { return };
        cur.path = new.to_path_buf();
        if cur.full.is_none() {
            // still loading under the old name: ask again under the new one (edits stay)
            self.request(new.to_path_buf());
        }
        if rescan {
            self.scan_folder(new.to_path_buf());
        }
        self.update_title();
    }
}

/// The file in the shell: Recycle Bin, clipboard, Explorer, About.
impl App {
    /// The current file as a full path: the shell resolves relative paths against whatever its
    /// current folder is, which need not be the one the path was given in.
    fn current_file(&self) -> Option<PathBuf> {
        let path = &self.current.as_ref()?.path;
        Some(std::path::absolute(path).unwrap_or_else(|_| path.clone()))
    }

    /// Moves the current file to the Recycle Bin, asking first (always when it has unsaved
    /// edits, else as the settings say), and shows the image that followed it in its folder.
    fn delete_current(&mut self) {
        if self.busy_saving() {
            return;
        }
        let (Some(hwnd), Some(path)) = (self.hwnd, self.current_file()) else { return };
        if iv_pipeline::archive::split(&path).is_some() {
            return self.set_notice(self.t().archive_page_delete.into());
        }
        let t = self.t();
        let name = file_name(&path);
        // only the file the user is looking at: while the next image loads, the previous one
        // stays on screen (and after a delete, the deleted one does). A file that cannot be shown
        // at all is named in the title bar and may go.
        let unreadable = self.error.is_some() && self.display.is_none();
        if !self.current_shown() && !unreadable {
            return self.set_notice(fill(t.delete_not_shown, &[("name", &name)]));
        }
        let edited = !self.edits.is_empty();
        if self.settings.confirm_delete || edited {
            let mut question = fill(t.delete_confirm, &[("name", &name)]);
            if edited {
                question = format!("{question}\n\n{}", t.delete_unsaved);
            }
            if !dialogs::ask_yes_no(hwnd, &question) {
                return;
            }
        }
        // where it sorted, for finding the image that followed it once it is gone
        let key = SortKey::read(&path);
        match shell::recycle(hwnd, &path) {
            Ok(()) => {
                self.trace.mark(&format!("moved to the Recycle Bin: {}", path.display()));
                self.deleted(&path, &key);
                self.set_notice(fill(t.deleted, &[("name", &name)]));
            }
            Err(e) => {
                diag::log(&format!("{}: not deleted: {e}", path.display()));
                self.set_notice(fill(t.delete_failed, &[("name", &name)]));
            }
        }
    }

    /// The current file is gone: it leaves the folder list and the cache, its edits are
    /// dropped, and the image that followed it (or the new last one) becomes current. An emptied
    /// folder leaves the window empty.
    fn deleted(&mut self, path: &Path, key: &SortKey) {
        let deleted = self.current.as_ref().map_or_else(|| path.to_path_buf(), |c| c.path.clone());
        self.crossing = None;
        self.pipeline.invalidate(&deleted);
        self.edits.clear();
        self.selection = None;
        // the image after the deleted one, or the new last one when it was last (then read-ahead
        // runs backwards, towards the images still ahead of the user)
        let mut went_back = false;
        let next = match self.folder.as_mut() {
            Some(folder) => folder.remove(&deleted).or_else(|| folder.index_after_key(key)).map(|i| {
                let last = folder.len().saturating_sub(1);
                went_back = i > last;
                i.min(last)
            }),
            None => {
                // the folder listing has not come back yet (it was for the deleted file and
                // will be dropped as stale): list the folder now
                let dir = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
                match FolderList::scan_sorted(dir, self.settings.sort_order()) {
                    Ok(list) => {
                        let at = list.index_after_key(key);
                        self.folder = Some(list);
                        at
                    }
                    Err(e) => {
                        diag::log(&format!("cannot list {} after deleting: {e}", dir.display()));
                        None
                    }
                }
            }
        };
        let next_path = next.and_then(|i| Some((i, self.folder.as_ref()?.get(i)?.to_path_buf())));
        // whatever is shown now belongs to the deleted file
        self.current = None;
        match next_path {
            Some((i, next)) => {
                self.index = Some(i);
                self.direction = if went_back { -1 } else { 1 };
                self.request(next);
            }
            None => {
                self.folder = None;
                self.index = None;
                self.display = None;
                self.error = None;
                self.after_image_change();
                self.update_title();
            }
        }
    }

    /// The image as it is shown, with its edits (and the frame on screen, for an animation), on
    /// the clipboard: as a plain bitmap for every program and, when it has transparency, as a
    /// bitmap with alpha and as PNG for the programs that keep it.
    fn copy_image(&mut self) {
        let Some(hwnd) = self.hwnd else { return };
        let Some(cur) = &self.current else { return };
        let Some(full) = cur.full.clone() else { return self.set_notice(self.t().copy_not_loaded.into()) };
        let frame = self
            .display
            .as_ref()
            .filter(|d| d.path == cur.path)
            .and_then(|d| d.playback.as_ref())
            .and_then(|p| Some(&full.animation.as_ref()?.frames.get(p.frame)?.image));
        let image = frame.unwrap_or(&full.image);
        let g = Geometry::new((image.width(), image.height()), full.info.orientation, self.edits.active());
        let result = iv_ops::apply::render(image, &g).map_err(|e| e.to_string()).and_then(|edited| {
            let (w, h) = (edited.width(), edited.height());
            let rgba = edited.to_rgba8();
            let mut items = vec![Clip::Standard(clipboard::CF_DIB, clipboard::dib(&rgba, w, h, [255; 3]))];
            if !edited.is_opaque() {
                items.push(Clip::Standard(clipboard::CF_DIBV5, clipboard::dibv5(&rgba, w, h)));
                if let Some(png) = self.encoders.for_format(FormatId::Png) {
                    let prepared = iv_codec::encode::prepare(&edited, png.info(), [255; 3]);
                    match png.encode(&prepared, &Default::default(), &Default::default()) {
                        Ok(bytes) => items.push(Clip::Named(clipboard::PNG_FORMAT, bytes)),
                        Err(e) => diag::log(&format!("clipboard PNG not made: {e}")),
                    }
                }
            }
            drop(rgba);
            drop(edited);
            shell::set_clipboard(hwnd, items).map(|()| (w, h))
        });
        let t = self.t();
        match result {
            Ok((w, h)) => {
                self.trace.mark(&format!("copied the image {w}x{h}"));
                self.set_notice(fill(t.copied_image, &[("width", &w.to_string()), ("height", &h.to_string())]));
            }
            Err(e) => self.set_notice(fill(t.clipboard_failed, &[("error", &e)])),
        }
    }

    /// The current file itself on the clipboard (pasting in Explorer copies it).
    fn copy_file(&mut self) {
        let (Some(hwnd), Some(path)) = (self.hwnd, self.current_file()) else { return };
        let t = self.t();
        if iv_pipeline::archive::split(&path).is_some() {
            return self.set_notice(t.archive_page_copy_file.into());
        }
        if !self.file_in_view(&path) {
            return;
        }
        match shell::copy_file(hwnd, &path) {
            Ok(()) => self.set_notice(t.copied_file.into()),
            Err(e) => self.set_notice(fill(t.clipboard_failed, &[("error", &e)])),
        }
    }

    /// The current file's full path as text. A page inside an archive has the path the viewer
    /// itself opens: the archive's path followed by the page's name.
    fn copy_path(&mut self) {
        let (Some(hwnd), Some(path)) = (self.hwnd, self.current_file()) else { return };
        if !self.file_in_view(&path) {
            return;
        }
        let t = self.t();
        match shell::copy_text(hwnd, &path.to_string_lossy()) {
            Ok(()) => self.set_notice(t.copied_path.into()),
            Err(e) => self.set_notice(fill(t.clipboard_failed, &[("error", &e)])),
        }
    }

    /// Explorer, in the current file's folder with the file selected (for a page inside an
    /// archive: the archive).
    fn show_in_folder(&mut self) {
        let Some(path) = self.current_file() else { return };
        let target = iv_pipeline::archive::split(&path).map_or(path, |(archive, _)| archive);
        if let Err(e) = shell::show_in_folder(&target) {
            self.set_notice(fill(self.t().show_in_folder_failed, &[("error", &e)]));
        }
    }

    /// Name, version, and where the settings and the diagnostic log are kept.
    fn about(&mut self) {
        let Some(hwnd) = self.hwnd else { return };
        let folder = |d: Option<&Path>| d.map_or_else(|| "-".to_string(), |d| d.display().to_string());
        let config = folder(self.settings_path.as_deref().and_then(Path::parent));
        let logs = folder(diag::default_dir().as_deref());
        let text =
            fill(self.t().about_text, &[("version", env!("CARGO_PKG_VERSION")), ("config", &config), ("logs", &logs)]);
        dialogs::show_info(hwnd, &text);
    }
}

impl Drop for App {
    /// A save the user asked for always finishes, even when the window closed under it (the
    /// process would otherwise end and kill the save thread, leaving the file unsaved). Logoff
    /// and shutdown end the process without this: `WM_ENDSESSION` waits for the save instead.
    fn drop(&mut self) {
        if self.saving.is_some() {
            session::finish_save();
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
        // redraws that change only the selection (a Shift+drag), the tones (edit panel sliders)
        // or the background (settings) reuse the resampled image; otherwise nothing is kept
        frame.keep_resampled(self.select_from.is_some() || self.panel.open || self.settings_open);
        let t = Instant::now();
        let editing = self.current.as_ref().zip(self.display.as_ref()).is_some_and(|(c, d)| c.path == d.path);
        let live: &[Op] = if editing { self.edits.active() } else { &[] };
        match &self.display {
            None => raster::render(frame, &[], None, &self.style),
            Some(d) => {
                let main_g = match &d.saved_edits {
                    // the old pixels of a file just saved in place: the saved edits, then any
                    // made since (they apply to the saved file, which is those pixels edited)
                    Some(saved) => {
                        let ops: Vec<Op> = saved.iter().chain(live).cloned().collect();
                        Geometry::new(d.stored, d.orientation, &ops)
                    }
                    None => Geometry::new(d.stored, d.orientation, live),
                };
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
                        // a whole number of screen pixels, so both pages start on whole pixels too
                        let main_w = (dest.width * s.fraction).round();
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
                let selection = self.selection.map(|r| selection_on_screen(&d.view, r));
                raster::render(frame, &pages, selection, &self.style);
            }
        }
        if !self.first_frame_logged {
            self.first_frame_logged = true;
            self.trace.mark(&format!("first frame drawn in {:.1} ms", t.elapsed().as_secs_f64() * 1000.0));
        }
        if let (Some(cur), Some(d)) = (&mut self.current, &self.display)
            && d.path == cur.path
            && d.kind == Shown::Full
            && !cur.shown_logged
        {
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

    fn chord(&self, vk: VIRTUAL_KEY, lp: LPARAM) -> Option<Chord> {
        let shift = key_down(VK_SHIFT);
        let scan = ((lp.0 >> 16) & 0xff) as u32;
        let extended = lp.0 & (1 << 24) != 0;
        let vk = keys::real_key(vk, scan, extended);
        let name = keys::key_name(vk, scan, shift)?;
        Some(Chord::new(name, key_down(VK_CONTROL), key_down(VK_MENU), shift))
    }

    fn on_key_down(&mut self, wp: WPARAM, lp: LPARAM) -> bool {
        let Some(chord) = self.chord(VIRTUAL_KEY(wp.0 as u16), lp) else { return false };
        let Some(cmd) = self.keymap.lookup(&chord) else { return false };
        // bit 30: the key was already down (auto-repeat)
        let repeat = lp.0 & (1 << 30) != 0;
        // a held key runs a one-shot command (Delete, Esc, a toggle) once, but the key is
        // still bound: swallowed, not passed on to Windows
        if !repeat || cmd.repeats() {
            self.run_command(cmd, repeat);
        }
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
            can_navigate: self.index.is_some()
                && self
                    .folder
                    .as_ref()
                    .is_some_and(|f| f.len() > 1 || self.settings.at_end == AtEnd::NextFolder && !f.is_empty()),
            has_path: self.current.is_some(),
            on_disk: self.current.as_ref().is_some_and(|c| iv_pipeline::archive::split(&c.path).is_none()),
            loaded: self.current.as_ref().is_some_and(|c| c.full.is_some()),
        }
    }

    /// Ctrl and Shift from a mouse message's key flags, Alt from the keyboard state.
    fn mouse_chord(&self, key: &str, wp: WPARAM) -> Chord {
        let flags = wp.0 as u32;
        Chord::new(key, flags & MK_CONTROL.0 != 0, key_down(VK_MENU), flags & MK_SHIFT.0 != 0)
    }

    fn on_wheel(&mut self, hwnd: HWND, wp: WPARAM, lp: LPARAM) {
        let delta = i32::from((wp.0 >> 16) as u16 as i16);
        let up = delta > 0;
        let chord = self.mouse_chord(if up { "WheelUp" } else { "WheelDown" }, wp);
        // a picture fitted to the width and taller than the window scrolls under the wheel
        let scroll = self
            .display
            .as_ref()
            .map(|d| &d.view)
            .filter(|v| v.mode() == ZoomMode::Fit(Fit::Width) && v.scrolls_y())
            .map(|v| v.can_scroll(!up));
        let navigates = self.settings.wheel_navigates;
        match wheel_use(&self.keymap, up, chord.ctrl, chord.alt, chord.shift, navigates, scroll) {
            WheelUse::Scroll => {
                let dy = -f64::from(delta) / f64::from(crate::wheel::NOTCH) * f64::from(self.viewport.1) * SCROLL_STEP;
                self.with_view(|v| {
                    v.scroll_by(dy);
                });
            }
            WheelUse::Command(cmd) => {
                // one command per whole notch, so a precision wheel does not race through files
                if self.wheel.push(delta, Instant::now()) != 0 {
                    self.run_command(cmd, false);
                }
            }
            WheelUse::Zoom => {
                let (sx, sy) = super::point_from_lparam(lp);
                let mut pt = POINT { x: sx, y: sy };
                unsafe {
                    let _ = ScreenToClient(hwnd, &mut pt);
                }
                let anchor = (pt.x as f64, pt.y as f64);
                self.with_view(|v| {
                    v.zoom_by_at(ZOOM_STEP.powf(f64::from(delta) / f64::from(crate::wheel::NOTCH)), anchor)
                });
            }
        }
    }

    /// One window message; `None` lets Windows handle it.
    pub fn on_message(&mut self, hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        let point = || {
            let (x, y) = super::point_from_lparam(lp);
            (x as f64, y as f64)
        };
        match msg {
            // WM_SIZE is handled by the window procedure (see `super::set_pending_size`)
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
                    // only on the current image: one still loading would be cropped to a
                    // rectangle drawn on the image before it
                    self.select_from =
                        self.display.as_ref().filter(|_| self.current_shown()).map(|d| d.view.screen_to_image(p));
                    self.selection = None;
                    self.invalidate();
                } else {
                    self.drag_from = Some(p);
                }
                Some(LRESULT(0))
            }
            WM_LBUTTONDBLCLK => {
                let p = point();
                self.with_view(|v| if v.is_fitted() { v.actual_size_at(p) } else { v.fit() });
                Some(LRESULT(0))
            }
            WM_MOUSEMOVE => {
                let p = point();
                if let (Some(from), Some(d)) =
                    (self.select_from, self.display.as_ref().filter(|_| self.current_shown()))
                {
                    let to = d.view.screen_to_image(p);
                    let (w, h) = d.view.image_size();
                    let selection = selection_rect(from, to, (w, h));
                    if selection != self.selection {
                        self.selection = selection;
                        self.invalidate();
                    }
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
                self.on_wheel(hwnd, wp, lp);
                Some(LRESULT(0))
            }
            // the side buttons (back / forward): bound in the keymap as Mouse4 / Mouse5
            WM_XBUTTONDOWN | WM_XBUTTONDBLCLK => {
                let key = match (wp.0 >> 16) & 0xffff {
                    1 => "Mouse4", // XBUTTON1
                    _ => "Mouse5",
                };
                let cmd = self.keymap.lookup(&self.mouse_chord(key, wp))?;
                self.run_command(cmd, false);
                // Windows asks for TRUE from a handled X button message
                Some(LRESULT(1))
            }
            WM_XBUTTONUP => {
                // a bound button's release must not also become a "browser back" app command
                let key = if (wp.0 >> 16) & 0xffff == 1 { "Mouse4" } else { "Mouse5" };
                self.keymap.lookup(&self.mouse_chord(key, wp)).map(|_| LRESULT(1))
            }
            WM_KILLFOCUS => {
                // the key-up of a held navigation key goes to the window that took the focus
                self.end_quick_browsing();
                None
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
            WM_TIMER if wp.0 == NOTICE_TIMER => {
                self.clear_notice();
                Some(LRESULT(0))
            }
            // WM_CLOSE, WM_QUERYENDSESSION and WM_ENDSESSION are handled by the window procedure
            // (see `session`): they must be answered while a modal dialog has the app too
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
