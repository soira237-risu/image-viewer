//! The window: input → commands, pipeline events → textures, redraws on demand.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use iv_codec::Decoded;
use iv_codec::encode::Encoders;
use iv_core::{FormatId, ImageBuf, ImageInfo};
use iv_ops::{EditStack, Geometry, Op};
use iv_pipeline::export::{ExportOutcome, export};
use iv_pipeline::{FolderList, Pipeline, PipelineConfig, PipelineEvent, prefetch_order, step};
use iv_render::{Gpu, ImageRenderer, ScreenRect, ViewState, ZoomMode, wgpu};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::StartCause;
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::commands::Command;
use crate::edit::{drag_rect, in_place_format, save_request, selection_to_crop};
use crate::keymap::{Chord, Keymap, canonical_key};
use crate::panel::{self, PanelAction, PanelContext, PanelState};
use crate::settings::Settings;
use crate::trace::Trace;

const ZOOM_STEP: f64 = 1.25;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

#[derive(Debug)]
pub enum AppEvent {
    Pipeline(PipelineEvent),
    Folder { for_path: PathBuf, result: Result<FolderList, String> },
    Saved { source: PathBuf, dest: PathBuf, result: Result<ExportOutcome, String> },
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

/// What is on the GPU right now. It can lag behind [`Current`]: the previous image stays on
/// screen until the next one has something to show, so navigation never flashes black.
struct Display {
    path: PathBuf,
    kind: Shown,
    view: ViewState,
    playback: Option<Playback>,
    /// Two-page view: the page shown beside `path`.
    second: Option<PathBuf>,
}

/// An animation being played on screen.
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

    /// Advances to the frame that should be on screen at `now`, skipping frames when late.
    /// Returns whether the frame changed.
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

struct Surface {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    gpu: Gpu,
    renderer: ImageRenderer,
    /// format the renderer targets (the sRGB view of the swapchain)
    view_format: wgpu::TextureFormat,
    egui: EguiParts,
}

/// The edit panel's UI toolkit, drawn over the image.
struct EguiParts {
    ctx: egui::Context,
    state: egui_winit::State,
    renderer: egui_wgpu::Renderer,
}

pub struct App {
    proxy: EventLoopProxy<AppEvent>,
    pipeline: Pipeline,
    keymap: Keymap,
    trace: Trace,
    surface: Option<Surface>,
    current: Option<Current>,
    display: Option<Display>,
    folder: Option<FolderList>,
    index: Option<usize>,
    /// Last navigation direction (+1 / −1), 0 right after opening a file.
    direction: i32,
    /// A navigation key is being held: flip without prefetching.
    quick_browsing: bool,
    error: Option<String>,
    modifiers: ModifiersState,
    cursor: PhysicalPosition<f64>,
    drag_from: Option<PhysicalPosition<f64>>,
    last_click: Option<(Instant, PhysicalPosition<f64>)>,
    first_frame_logged: bool,
    gpu_init: Option<GpuInit>,
    /// Open without taking keyboard focus (`--no-activate`, for automated checks that must not
    /// capture keys meant for another window).
    pub no_activate: bool,
    /// Edits to the current image (cleared when another image becomes current).
    edits: EditStack,
    /// Shift+drag selection: start corner while dragging, finished rectangle afterwards.
    select_from: Option<PhysicalPosition<f64>>,
    selection: Option<ScreenRect>,
    saving: bool,
    encoders: Encoders,
    /// Two-page view: `Some(right_to_left)` when on.
    spread: Option<bool>,
    panel: PanelState,
    settings: Settings,
    settings_path: Option<PathBuf>,
    settings_open: bool,
}

/// GPU device creation started at launch, before the window exists.
pub type GpuInit = std::thread::JoinHandle<Result<Gpu, String>>;

/// Starts creating the GPU device on a background thread.
pub fn start_gpu_init() -> GpuInit {
    std::thread::Builder::new()
        .name("gpu-init".into())
        .spawn(|| pollster::block_on(Gpu::first_available(&iv_render::preferred_backends())))
        .expect("spawn gpu-init thread")
}

/// Turns a winit key into the keymap's canonical name.
pub fn key_name(key: &Key) -> Option<String> {
    match key {
        Key::Named(named) => {
            let name = match named {
                NamedKey::ArrowLeft => "Left",
                NamedKey::ArrowRight => "Right",
                NamedKey::ArrowUp => "Up",
                NamedKey::ArrowDown => "Down",
                NamedKey::Escape => "Esc",
                NamedKey::Enter => "Enter",
                NamedKey::Space => "Space",
                NamedKey::Tab => "Tab",
                NamedKey::Backspace => "Backspace",
                NamedKey::Delete => "Delete",
                NamedKey::Insert => "Insert",
                NamedKey::Home => "Home",
                NamedKey::End => "End",
                NamedKey::PageUp => "PageUp",
                NamedKey::PageDown => "PageDown",
                other => return canonical_key(&format!("{other:?}")), // F1..F24
            };
            Some(name.to_string())
        }
        Key::Character(s) => canonical_key(s),
        _ => None,
    }
}

fn file_name(p: &Path) -> String {
    p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
}

impl App {
    pub fn new(
        proxy: EventLoopProxy<AppEvent>,
        keymap: Keymap,
        trace: Trace,
        gpu_init: Option<GpuInit>,
        settings: Settings,
        settings_path: Option<PathBuf>,
    ) -> Self {
        let sink = proxy.clone();
        let config =
            PipelineConfig { budget_bytes: settings.cache_mib as u64 * 1024 * 1024, ..PipelineConfig::default() };
        let pipeline = Pipeline::new(Arc::new(iv_codec::default_registry()), config, move |e| {
            // the window may already be closing; nothing to do then
            let _ = sink.send_event(AppEvent::Pipeline(e));
        });
        Self {
            proxy,
            pipeline,
            keymap,
            trace,
            surface: None,
            current: None,
            display: None,
            folder: None,
            index: None,
            direction: 0,
            quick_browsing: false,
            error: None,
            modifiers: ModifiersState::empty(),
            cursor: PhysicalPosition::new(0.0, 0.0),
            drag_from: None,
            last_click: None,
            first_frame_logged: false,
            gpu_init,
            no_activate: false,
            edits: EditStack::new(),
            select_from: None,
            selection: None,
            saving: false,
            encoders: Encoders::default(),
            spread: None,
            panel: PanelState { quality: settings.save_quality, ..PanelState::default() },
            settings,
            settings_path,
            settings_open: false,
        }
    }

    /// Opens a file and lists its folder in the background for navigation.
    pub fn open(&mut self, path: PathBuf) {
        if !self.confirm_discard() {
            return;
        }
        self.trace.mark(&format!("open {}", path.display()));
        self.folder = None;
        self.index = None;
        self.direction = 0;

        // an archive opens on its first page; its listing is the "folder"
        if iv_pipeline::archive::is_archive(&path) && path.is_file() {
            match FolderList::for_file(&path) {
                Ok(list) if !list.is_empty() => {
                    let first = list.files()[0].clone();
                    self.trace.mark(&format!("archive listed: {} images", list.len()));
                    self.folder = Some(list);
                    self.index = Some(0);
                    self.request(first);
                }
                Ok(_) => {
                    self.error = Some(format!("{}: no images inside", file_name(&path)));
                    self.update_title();
                }
                Err(e) => {
                    self.error = Some(format!("{}: {e}", file_name(&path)));
                    self.update_title();
                }
            }
            return;
        }
        self.request(path.clone());

        let proxy = self.proxy.clone();
        std::thread::Builder::new()
            .name("folder-scan".into())
            .spawn(move || {
                let result = FolderList::for_file(&path).map_err(|e| e.to_string());
                let _ = proxy.send_event(AppEvent::Folder { for_path: path, result });
            })
            .expect("spawn folder scan");
    }

    /// Paths to prefetch around the current index, nearest first.
    fn prefetch_paths(&self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = match (&self.folder, self.index) {
            (Some(folder), Some(i)) if !self.quick_browsing => {
                // two pages are shown at a time, so look twice as far
                let n = self.settings.prefetch_count;
                let count = if self.spread.is_some() { n * 2 } else { n };
                prefetch_order(i, folder.len(), self.direction, count, self.settings.wrap)
                    .into_iter()
                    .filter_map(|j| folder.get(j).map(Path::to_path_buf))
                    .collect()
            }
            _ => Vec::new(),
        };
        // the facing page is needed right away
        if let Some(partner) = self.partner() {
            paths.retain(|p| *p != partner);
            paths.insert(0, partner);
        }
        paths
    }

    /// After the two-page setting changed: prefetch the facing page and show it (or remove it).
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
            self.sync_selection();
        }
        if let (Some(cur), Some(decoded)) = (self.current.as_mut(), hit) {
            if cur.full.is_none() {
                cur.info = Some(decoded.info.clone());
                cur.full = Some(decoded);
            }
        }
        self.sync_texture();
        self.update_title();
    }

    fn navigate(&mut self, cmd: Command, repeat: bool) {
        let (Some(folder), Some(index)) = (&self.folder, self.index) else { return };
        let len = folder.len();
        let target = match cmd {
            Command::NavNext => step(index, len, if self.spread.is_some() { 2 } else { 1 }, self.settings.wrap),
            Command::NavPrev => step(index, len, if self.spread.is_some() { -2 } else { -1 }, self.settings.wrap),
            Command::NavFirst => (index != 0).then_some(0),
            Command::NavLast => (index + 1 != len).then(|| len - 1),
            _ => None,
        };
        let Some(target) = target else { return };
        let Some(path) = folder.get(target).map(Path::to_path_buf) else { return };
        if !self.confirm_discard() {
            return;
        }
        self.direction = match cmd {
            Command::NavPrev | Command::NavFirst => -1,
            _ => 1,
        };
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

    fn viewport(&self) -> (u32, u32) {
        self.surface.as_ref().map_or((1280, 800), |s| (s.config.width, s.config.height))
    }

    fn create_surface(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let attrs = Window::default_attributes()
            .with_title("image-viewer")
            .with_inner_size(PhysicalSize::new(1280, 800))
            .with_active(!self.no_activate);
        let window = Arc::new(event_loop.create_window(attrs).map_err(|e| e.to_string())?);
        self.trace.mark("window created");

        // the device was started on another thread at launch; use it if it can present here
        let early = self.gpu_init.take().map(|h| h.join().unwrap_or_else(|_| Err("gpu thread panicked".into())));
        let (gpu, surface) = match early {
            Some(Ok(gpu)) => {
                let surface = gpu.instance.create_surface(window.clone()).map_err(|e| e.to_string())?;
                if gpu.adapter.is_surface_supported(&surface) {
                    (gpu, surface)
                } else {
                    self.trace.mark("early adapter cannot present; retrying with the surface");
                    drop(surface);
                    Self::gpu_for_window(&window)?
                }
            }
            Some(Err(e)) => {
                self.trace.mark(&format!("early gpu init failed ({e}); retrying with the surface"));
                Self::gpu_for_window(&window)?
            }
            None => Self::gpu_for_window(&window)?,
        };
        let info = gpu.adapter.get_info();
        self.trace.mark(&format!("gpu ready: {} ({:?})", info.name, info.backend));

        let size = window.inner_size();
        let mut config = surface
            .get_default_config(&gpu.adapter, size.width.max(1), size.height.max(1))
            .ok_or("surface is not supported by the adapter")?;
        let caps = surface.get_capabilities(&gpu.adapter);
        // render through an sRGB view so linear shader output is encoded for display
        config.format = caps.formats.iter().copied().find(|f| f.is_srgb()).unwrap_or(config.format);
        let view_format = config.format.add_srgb_suffix();
        if view_format != config.format {
            config.view_formats.push(view_format);
        }
        config.present_mode = wgpu::PresentMode::AutoVsync;
        surface.configure(&gpu.device, &config);
        self.trace.mark("surface configured");

        let mut renderer = ImageRenderer::new(&gpu.device, view_format);
        apply_style(&mut renderer, &self.settings);
        self.trace.mark("pipelines created");
        let ctx = egui::Context::default();
        let state = egui_winit::State::new(
            ctx.clone(),
            egui::ViewportId::ROOT,
            &*window,
            Some(window.scale_factor() as f32),
            None,
            Some(gpu.max_texture_side() as usize),
        );
        let egui_renderer = egui_wgpu::Renderer::new(&gpu.device, view_format, egui_wgpu::RendererOptions::default());
        let egui = EguiParts { ctx, state, renderer: egui_renderer };
        self.surface = Some(Surface { window, surface, config, gpu, renderer, view_format, egui });
        Ok(())
    }

    /// Slow path: any backend, adapter chosen for this window's surface.
    fn gpu_for_window(window: &Arc<Window>) -> Result<(Gpu, wgpu::Surface<'static>), String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance.create_surface(window.clone()).map_err(|e| e.to_string())?;
        let gpu = pollster::block_on(Gpu::new(instance, Some(&surface)))?;
        Ok((gpu, surface))
    }

    /// Uploads the best image for the current request that is not on the GPU yet.
    /// Uploads the best image for the current request, then re-applies the edits to it.
    fn sync_texture(&mut self) {
        self.sync_texture_inner();
        if !self.edits.is_empty() {
            self.apply_edits_to_display();
        }
        self.sync_second();
    }

    /// Two-page view: puts the facing page beside the current one once it is decoded, or
    /// removes it when there is none (single-page view, last page).
    fn sync_second(&mut self) {
        let want = self.partner();
        let current = self.current.as_ref().and_then(|c| Some((c.path.clone(), c.info.as_ref()?.display_size())));
        let (Some(s), Some(d)) = (self.surface.as_mut(), self.display.as_mut()) else { return };
        let Some((_, main_size)) = current.filter(|(p, _)| *p == d.path) else { return };
        match want {
            None => {
                if d.second.take().is_some() {
                    s.renderer.clear_second();
                    d.view.set_image_size(main_size);
                    s.window.request_redraw();
                }
            }
            Some(partner) if d.second.as_ref() != Some(&partner) => {
                let Some(page) = self.pipeline.cached(&partner) else { return };
                let (total, fraction) = spread_layout(main_size, page.info.display_size());
                let rtl = self.spread.unwrap_or(true);
                s.renderer.set_second_image(
                    &s.gpu.device,
                    &s.gpu.queue,
                    &page.image,
                    page.info.orientation,
                    page.info.has_alpha,
                    fraction,
                    rtl,
                );
                d.second = Some(partner);
                d.view.set_image_size(total);
                s.window.request_redraw();
            }
            Some(_) => {}
        }
    }

    fn sync_texture_inner(&mut self) {
        let viewport = self.viewport();
        let (Some(s), Some(cur)) = (self.surface.as_mut(), self.current.as_mut()) else { return };
        let same_image = self.display.as_ref().is_some_and(|d| d.path == cur.path);
        let shown = self.display.as_ref().filter(|_| same_image).map(|d| d.kind);

        let (image, info, kind) = match (&cur.full, &cur.preview, &cur.info) {
            (Some(full), _, _) if shown != Some(Shown::Full) => (&full.image, &full.info, Shown::Full),
            (None, Some(preview), Some(info)) if shown.is_none() => (preview, info, Shown::Preview),
            _ => return,
        };
        let t = Instant::now();
        let up = s.renderer.set_image(&s.gpu.device, &s.gpu.queue, image, info.orientation, info.has_alpha);
        self.trace.mark(&format!(
            "{} uploaded {}x{} ({} mips{}) in {:.1} ms",
            if kind == Shown::Full { "image" } else { "preview" },
            up.texture_width,
            up.texture_height,
            up.mip_levels,
            if up.reduced { ", reduced to GPU limit" } else { "" },
            t.elapsed().as_secs_f64() * 1000.0
        ));
        match &mut self.display {
            // the full image replacing its own preview keeps the user's zoom
            Some(d) if same_image => {
                d.kind = kind;
                d.view.set_image_size(info.display_size());
            }
            _ => {
                self.display = Some(Display {
                    path: cur.path.clone(),
                    kind,
                    view: ViewState::new(info.display_size(), viewport),
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
        }
        s.window.request_redraw();
    }

    /// Puts an image that is no longer current on screen (quick browsing), unless the current one
    /// is already showing.
    fn show_passing(&mut self, path: PathBuf, decoded: &Decoded) {
        let viewport = self.viewport();
        let Some(s) = self.surface.as_mut() else { return };
        let current_shown = match (&self.current, &self.display) {
            (Some(c), Some(d)) => d.path == c.path,
            _ => false,
        };
        if current_shown {
            return;
        }
        s.renderer.set_image(
            &s.gpu.device,
            &s.gpu.queue,
            &decoded.image,
            decoded.info.orientation,
            decoded.info.has_alpha,
        );
        self.trace.mark(&format!("passing image shown: {}", file_name(&path)));
        self.display = Some(Display {
            path,
            kind: Shown::Full,
            view: ViewState::new(decoded.info.display_size(), viewport),
            playback: None,
            second: None,
        });
        s.window.request_redraw();
    }

    fn update_title(&self) {
        let Some(s) = &self.surface else { return };
        let position = match (&self.folder, self.index) {
            (Some(f), Some(i)) if self.partner().is_some() => format!("[{}-{}/{}] ", i + 1, i + 2, f.len()),
            (Some(f), Some(i)) => format!("[{}/{}] ", i + 1, f.len()),
            _ => String::new(),
        };
        let title = match (&self.current, &self.error) {
            (_, Some(err)) => format!("{position}image-viewer — {err}"),
            (Some(cur), None) => {
                let name = file_name(&cur.path);
                let Some(info) = &cur.info else { return s.window.set_title(&format!("{position}{name} — loading")) };
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
                    (Some(_), Some(t)) => {
                        format!("{} · {} · {:.0} ms", info.format, info.codec, t.as_secs_f64() * 1000.0)
                    }
                    (Some(_), None) => format!("{} · {} · cached", info.format, info.codec),
                    (None, _) if cur.preview.is_some() => format!("{} · preview", info.format),
                    (None, _) => format!("{} · loading", info.format),
                };
                let edited = if self.edits.is_empty() { "" } else { "*" };
                let saving = if self.saving { " · saving…" } else { "" };
                format!("{position}{name}{edited} — {w}×{h}{zoom} — {state}{saving}")
            }
            (None, None) => "image-viewer — Ctrl+O to open, or drop a file".to_string(),
        };
        s.window.set_title(&title);
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
                if let Some(s) = &mut self.surface {
                    s.renderer.clear_image();
                    s.window.request_redraw();
                }
            }
            // while a key is held, an image the user flipped past is shown as soon as it is
            // decoded, so the screen moves with the key instead of freezing until release
            PipelineEvent::Ready { path, decoded, .. } if self.quick_browsing => self.show_passing(path, &decoded),
            // prefetch results live in the pipeline's cache until asked for
            _ => {}
        }
        self.sync_texture();
        self.update_title();
    }

    fn handle_folder(&mut self, for_path: PathBuf, result: Result<FolderList, String>) {
        // a newer open superseded this scan
        if self.current.as_ref().map(|c| &c.path) != Some(&for_path) {
            return;
        }
        match result {
            Ok(folder) => {
                self.trace.mark(&format!("folder listed: {} images", folder.len()));
                self.index = folder.position(&for_path);
                self.folder = Some(folder);
                // now that neighbours are known, start prefetching them
                self.request(for_path);
            }
            Err(e) => eprintln!("cannot list folder of {}: {e}", for_path.display()),
        }
    }

    fn run_command(&mut self, cmd: Command, repeat: bool, event_loop: &ActiveEventLoop) {
        let center = {
            let (w, h) = self.viewport();
            (w as f64 / 2.0, h as f64 / 2.0)
        };
        match cmd {
            Command::FileOpen => {
                let mut exts: Vec<&str> = FormatId::ALL.iter().flat_map(|f| f.extensions().iter().copied()).collect();
                exts.extend(["zip", "cbz"]);
                if let Some(path) = rfd::FileDialog::new().add_filter("Images", &exts).pick_file() {
                    self.open(path);
                }
            }
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
            Command::AppSettings => {
                self.settings_open = !self.settings_open;
                if let Some(s) = &self.surface {
                    s.window.request_redraw();
                }
            }
            Command::ViewToggleEditPanel => {
                self.panel.open = !self.panel.open;
                if let Some(s) = &self.surface {
                    s.window.request_redraw();
                }
            }
            Command::ViewToggleSpread => {
                self.spread = if self.spread.is_some() { None } else { Some(self.settings.spread_right_to_left) };
                self.trace.mark(&format!("two-page view: {:?}", self.spread));
                self.refresh_spread();
            }
            Command::ViewToggleDirection => {
                if let Some(rtl) = self.spread {
                    self.spread = Some(!rtl);
                    // re-place the facing page on the other side
                    if let Some(d) = &mut self.display {
                        d.second = None;
                    }
                    self.refresh_spread();
                }
            }
            Command::ViewToggleFullscreen => {
                if let Some(s) = &self.surface {
                    let next = if s.window.fullscreen().is_some() { None } else { Some(Fullscreen::Borderless(None)) };
                    s.window.set_fullscreen(next);
                }
            }
            Command::Escape => {
                if self.selection.take().is_some() {
                    self.sync_selection();
                } else if let Some(s) = self.surface.as_ref().filter(|s| s.window.fullscreen().is_some()) {
                    s.window.set_fullscreen(None);
                } else {
                    self.quit(event_loop);
                }
            }
            Command::AppQuit => self.quit(event_loop),
        }
    }

    fn quit(&mut self, event_loop: &ActiveEventLoop) {
        if self.confirm_discard() {
            event_loop.exit();
        }
    }

    fn with_view(&mut self, f: impl FnOnce(&mut ViewState)) {
        if let Some(d) = &mut self.display {
            f(&mut d.view);
            self.update_title();
            if let Some(s) = &self.surface {
                s.window.request_redraw();
            }
        }
    }

    fn redraw(&mut self) {
        let Some(s) = &mut self.surface else { return };
        let frame = match s.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(f) => f,
            wgpu::CurrentSurfaceTexture::Suboptimal(f) => {
                // present this one, reconfigure for the next
                s.surface.configure(&s.gpu.device, &s.config);
                f
            }
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                s.surface.configure(&s.gpu.device, &s.config);
                s.window.request_redraw();
                return;
            }
            // minimised / timed out: try again on the next redraw request
            _ => return,
        };
        let target = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor { format: Some(s.view_format), ..Default::default() });
        let mut encoder = s.gpu.device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("frame") });
        let view = self.display.as_ref().map(|d| &d.view);
        s.renderer.render(&s.gpu.queue, &mut encoder, &target, (s.config.width, s.config.height), view);
        let mut panel_actions = Vec::new();
        let mut ui_changed = false;
        if self.panel.open || self.settings_open {
            let cx = self.panel_context();
            let s = self.surface.as_mut().expect("checked above");
            let before = (self.settings.clone(), self.panel.quality);
            let mut save_settings = false;
            panel_actions = draw_ui(
                s,
                &mut UiState {
                    panel: &mut self.panel,
                    settings: &mut self.settings,
                    settings_open: &mut self.settings_open,
                },
                &cx,
                &mut save_settings,
                &mut encoder,
                &target,
            );
            ui_changed = before != (self.settings.clone(), self.panel.quality) || save_settings;
            if save_settings {
                self.save_settings();
            }
        }
        let s = self.surface.as_mut().expect("checked above");
        s.gpu.queue.submit([encoder.finish()]);
        s.window.pre_present_notify();
        s.gpu.queue.present(frame);
        if ui_changed {
            self.settings_changed();
        }
        if !panel_actions.is_empty() {
            self.apply_panel_actions(panel_actions);
        }

        if !self.first_frame_logged {
            self.first_frame_logged = true;
            self.trace.mark("first frame presented");
        }
        // time from asking for an image to its full-resolution frame being on screen
        if let (Some(cur), Some(d)) = (&mut self.current, &self.display) {
            if d.path == cur.path && d.kind == Shown::Full && !cur.shown_logged {
                cur.shown_logged = true;
                let stats = self.pipeline.stats();
                self.trace.mark(&format!(
                    "shown {} {:.1} ms after request ({}) | cache {} images, {:.0} MiB of {:.0} MiB{}",
                    file_name(&cur.path),
                    cur.requested_at.elapsed().as_secs_f64() * 1000.0,
                    if cur.load_time.is_some() { "decoded" } else { "cache hit" },
                    stats.cached,
                    stats.used_bytes as f64 / 1048576.0,
                    self.pipeline.budget_bytes() as f64 / 1048576.0,
                    if self.quick_browsing { " | quick browsing" } else { "" }
                ));
            }
        }
    }

    fn on_key(&mut self, event: &KeyEvent, event_loop: &ActiveEventLoop) {
        let Some(name) = key_name(&event.logical_key) else { return };
        let m = self.modifiers;
        let chord = Chord::new(name, m.control_key(), m.alt_key(), m.shift_key());
        let Some(cmd) = self.keymap.lookup(&chord) else { return };
        match event.state {
            ElementState::Pressed => self.run_command(cmd, event.repeat, event_loop),
            ElementState::Released if cmd.is_navigation() => self.end_quick_browsing(),
            ElementState::Released => {}
        }
    }

    fn on_mouse_button(&mut self, state: ElementState, button: MouseButton) {
        if button != MouseButton::Left {
            return;
        }
        match state {
            // Shift+drag selects a region (for cropping) instead of panning
            ElementState::Pressed if self.modifiers.shift_key() => {
                self.select_from = Some(self.cursor);
                self.selection = None;
                self.sync_selection();
            }
            ElementState::Pressed => {
                let now = Instant::now();
                let pos = self.cursor;
                let double = self.last_click.is_some_and(|(t, p)| {
                    now - t < DOUBLE_CLICK && (p.x - pos.x).abs() < 4.0 && (p.y - pos.y).abs() < 4.0
                });
                if double {
                    self.last_click = None;
                    self.with_view(
                        |v| {
                            if v.mode() == ZoomMode::Fit { v.actual_size_at((pos.x, pos.y)) } else { v.fit() }
                        },
                    );
                } else {
                    self.last_click = Some((now, pos));
                    self.drag_from = Some(pos);
                }
            }
            ElementState::Released => {
                self.drag_from = None;
                self.select_from = None;
            }
        }
    }
}

/// Editing and saving.
impl App {
    /// Geometry of the current image with its edits, once its size is known.
    fn geometry(&self) -> Option<Geometry> {
        let info = self.current.as_ref()?.info.as_ref()?;
        Some(Geometry::new((info.width, info.height), info.orientation, self.edits.active()))
    }

    /// Texture mapping for the current edits, `None` when there are none (the plain orientation
    /// mapping set with the texture is then right).
    fn edit_uv(&self) -> Option<iv_render::orient::UvTransform> {
        if self.edits.is_empty() {
            return None;
        }
        self.geometry().map(|g| g.display_to_stored_uv().map(|r| r.map(|v| v as f32)))
    }

    /// Shows the current edits: texture mapping and view size follow the edit geometry.
    fn apply_edits_to_display(&mut self) {
        let Some(g) = self.geometry() else { return };
        let uv = g.display_to_stored_uv().map(|r| r.map(|v| v as f32));
        let (Some(s), Some(d), Some(cur)) = (self.surface.as_mut(), self.display.as_mut(), self.current.as_ref())
        else {
            return;
        };
        if d.path != cur.path {
            return;
        }
        s.renderer.set_uv_transform(uv);
        s.renderer.set_adjustments(&crate::edit::preview_adjustments(&g));
        if d.view.image_size() != (g.size.0 as f64, g.size.1 as f64) {
            self.panel.resize = (0, 0);
        }
        d.view.set_image_size(g.size);
        s.window.request_redraw();
    }

    /// Settings or the panel's quality changed: keep both in step and apply what is live.
    fn settings_changed(&mut self) {
        if self.panel.quality != self.settings.save_quality {
            // whichever moved last wins; the panel slider is the more direct control
            self.settings.save_quality = self.panel.quality;
        }
        self.settings = self.settings.clone().clamped();
        self.panel.quality = self.settings.save_quality;
        if let Some(s) = &mut self.surface {
            apply_style(&mut s.renderer, &self.settings);
            s.window.request_redraw();
        }
    }

    fn save_settings(&mut self) {
        let Some(path) = self.settings_path.clone() else { return };
        match self.settings.save(&path) {
            Ok(()) => self.trace.mark(&format!("settings saved to {}", path.display())),
            Err(e) => {
                self.error = Some(format!("settings: {e}"));
                self.update_title();
            }
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

    fn apply_panel_actions(&mut self, actions: Vec<PanelAction>) {
        for action in actions {
            match action {
                PanelAction::Adjust(a) => {
                    if self.spread.is_some() {
                        self.error = Some("editing works in single-page view (press D)".into());
                    } else if self.current.as_ref().is_some_and(|c| c.info.is_some()) {
                        self.edits.push_or_replace(Op::Adjust(a), |o| matches!(o, Op::Adjust(_)));
                        self.after_edit_change();
                    }
                }
                PanelAction::Edit(op) => self.edit(op),
                PanelAction::Undo => {
                    if self.edits.undo() {
                        self.after_edit_change();
                    }
                }
                PanelAction::Redo => {
                    if self.edits.redo() {
                        self.after_edit_change();
                    }
                }
                PanelAction::Save => self.save_in_place(),
                PanelAction::SaveAs => self.save_as(),
            }
        }
        self.update_title();
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
        self.sync_selection();
        self.apply_edits_to_display();
        self.update_title();
    }

    fn edit(&mut self, op: Op) {
        if self.spread.is_some() {
            self.error = Some("editing works in single-page view (press D)".into());
            self.update_title();
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

    fn sync_selection(&mut self) {
        if let Some(s) = &mut self.surface {
            s.renderer.set_selection(self.selection);
            s.window.request_redraw();
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
        let Some(cur) = &self.current else { return };
        let mut dialog = rfd::FileDialog::new()
            .set_file_name(cur.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default());
        if let Some(dir) = cur.path.parent() {
            dialog = dialog.set_directory(dir);
        }
        for enc in self.encoders.all() {
            let f = enc.info().format;
            dialog = dialog.add_filter(f.name(), f.extensions());
        }
        if let Some(dest) = dialog.save_file() {
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
            Err(e) => {
                self.error = Some(e);
                self.update_title();
                return;
            }
        };
        self.saving = true;
        self.update_title();
        let (proxy, encoders) = (self.proxy.clone(), self.encoders.clone());
        std::thread::Builder::new()
            .name("save".into())
            .spawn(move || {
                let result = export(&iv_codec::default_registry(), &encoders, &req).map_err(|e| e.to_string());
                let _ = proxy.send_event(AppEvent::Saved { source: req.source, dest: req.dest, result });
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
                    // show the file as it now is on disk: keep the old frame up until the new
                    // decode arrives, but mark it stale so that decode replaces it
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
                self.error = Some(format!("save failed: {e}"));
            }
        }
        self.update_title();
    }

    /// Before the current image is replaced or the app closes: asks to save unsaved edits.
    /// `true` means go ahead.
    fn confirm_discard(&mut self) -> bool {
        if self.edits.is_empty() {
            return true;
        }
        let name = self.current.as_ref().map(|c| file_name(&c.path)).unwrap_or_default();
        let answer = rfd::MessageDialog::new()
            .set_level(rfd::MessageLevel::Warning)
            .set_title("image-viewer")
            .set_description(format!("Save changes to {name}?"))
            .set_buttons(rfd::MessageButtons::YesNoCancel)
            .show();
        match answer {
            rfd::MessageDialogResult::Yes => {
                let Some(cur) = &self.current else { return true };
                let format_ok = cur.info.as_ref().and_then(|i| in_place_format(&self.encoders, i.format)).is_some();
                if !format_ok {
                    self.error = Some("this format cannot be written; use Save As".into());
                    self.update_title();
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
                        self.error = Some(format!("save failed: {e}"));
                        self.update_title();
                        false
                    }
                }
            }
            rfd::MessageDialogResult::No => {
                self.edits.clear();
                true
            }
            _ => false,
        }
    }
}

impl App {
    /// Shows the animation frame that is due, if any.
    fn tick_animation(&mut self) {
        let edit_uv = self.edit_uv();
        let (Some(s), Some(d)) = (self.surface.as_mut(), self.display.as_mut()) else { return };
        let Some(p) = d.playback.as_mut() else { return };
        if !p.advance(Instant::now()) {
            return;
        }
        let Some(anim) = p.decoded.animation.as_ref() else { return };
        let info = &p.decoded.info;
        s.renderer.set_image(
            &s.gpu.device,
            &s.gpu.queue,
            &anim.frames[p.frame].image,
            info.orientation,
            info.has_alpha,
        );
        if let Some(uv) = edit_uv {
            s.renderer.set_uv_transform(uv);
        }
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
        s.window.request_redraw();
    }
}

impl ApplicationHandler<AppEvent> for App {
    fn new_events(&mut self, _event_loop: &ActiveEventLoop, cause: StartCause) {
        if let StartCause::ResumeTimeReached { .. } = cause {
            self.tick_animation();
        }
    }

    /// Sleeps until the next animation frame is due, or until an event when nothing animates.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match self.display.as_ref().and_then(|d| d.playback.as_ref()) {
            Some(p) => event_loop.set_control_flow(ControlFlow::WaitUntil(p.next_at)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }

    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.surface.is_some() {
            return;
        }
        if let Err(e) = self.create_surface(event_loop) {
            eprintln!("cannot start the renderer: {e}");
            event_loop.exit();
            return;
        }
        // anything that finished decoding while the GPU was starting goes up now
        self.sync_texture();
        self.update_title();
        if let Some(s) = &self.surface {
            s.window.request_redraw();
        }
    }

    fn user_event(&mut self, _event_loop: &ActiveEventLoop, event: AppEvent) {
        match event {
            AppEvent::Pipeline(e) => self.handle_pipeline(e),
            AppEvent::Folder { for_path, result } => self.handle_folder(for_path, result),
            AppEvent::Saved { source, dest, result } => self.handle_saved(source, dest, result),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // the edit panel and settings window get first look at input; what they use (clicks on
        // them, typing in their fields) does not also reach the viewer
        if self.panel.open || self.settings_open {
            if let Some(s) = &mut self.surface {
                let response = s.egui.state.on_window_event(&s.window, &event);
                if response.repaint {
                    s.window.request_redraw();
                }
                // a focused panel widget (slider, field, button) owns the keyboard: arrow keys must move
                // the slider, not also step to the next image
                let panel_has_focus = s.egui.ctx.memory(|m| m.focused().is_some());
                if response.consumed || (panel_has_focus && matches!(event, WindowEvent::KeyboardInput { .. })) {
                    return;
                }
            }
        }
        match event {
            WindowEvent::CloseRequested => self.quit(event_loop),
            WindowEvent::Resized(size) => {
                if let Some(s) = &mut self.surface {
                    s.config.width = size.width.max(1);
                    s.config.height = size.height.max(1);
                    s.surface.configure(&s.gpu.device, &s.config);
                    s.window.request_redraw();
                }
                let vp = self.viewport();
                if let Some(d) = &mut self.display {
                    d.view.set_viewport(vp);
                }
                self.update_title();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.on_key(&event, event_loop),
            WindowEvent::CursorMoved { position, .. } => {
                if let Some(from) = self.select_from {
                    self.selection = Some(drag_rect((from.x, from.y), (position.x, position.y)));
                    self.sync_selection();
                }
                if let Some(from) = self.drag_from {
                    let (dx, dy) = (position.x - from.x, position.y - from.y);
                    self.drag_from = Some(position);
                    self.with_view(|v| v.pan_by(dx, dy));
                }
                self.cursor = position;
            }
            WindowEvent::MouseInput { state, button, .. } => self.on_mouse_button(state, button),
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as f64,
                    MouseScrollDelta::PixelDelta(p) => p.y / 120.0,
                };
                let anchor = (self.cursor.x, self.cursor.y);
                self.with_view(|v| v.zoom_by_at(ZOOM_STEP.powf(lines), anchor));
            }
            WindowEvent::DroppedFile(path) => self.open(path),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn winit_keys_map_to_keymap_names() {
        assert_eq!(key_name(&Key::Named(NamedKey::ArrowRight)).as_deref(), Some("Right"));
        assert_eq!(key_name(&Key::Named(NamedKey::Escape)).as_deref(), Some("Esc"));
        assert_eq!(key_name(&Key::Named(NamedKey::F11)).as_deref(), Some("F11"));
        assert_eq!(key_name(&Key::Character("o".into())).as_deref(), Some("O"));
        assert_eq!(key_name(&Key::Character("+".into())).as_deref(), Some("+"));
        assert_eq!(key_name(&Key::Named(NamedKey::Shift)), None);
    }

    #[test]
    fn default_bindings_reach_commands_from_winit_keys() {
        let map = Keymap::defaults();
        let chord = |key: Key, ctrl, shift| Chord::new(key_name(&key).unwrap(), ctrl, false, shift);
        assert_eq!(map.lookup(&chord(Key::Character("o".into()), true, false)), Some(Command::FileOpen));
        // Shift+= delivers "+"
        assert_eq!(map.lookup(&chord(Key::Character("+".into()), false, true)), Some(Command::ViewZoomIn));
        assert_eq!(map.lookup(&chord(Key::Named(NamedKey::ArrowRight), false, false)), Some(Command::NavNext));
        assert_eq!(map.lookup(&chord(Key::Named(NamedKey::Space), false, false)), Some(Command::NavNext));
        assert_eq!(map.lookup(&chord(Key::Named(NamedKey::ArrowLeft), false, false)), Some(Command::NavPrev));
        assert_eq!(map.lookup(&chord(Key::Named(NamedKey::Home), false, false)), Some(Command::NavFirst));
    }
}

#[cfg(test)]
mod playback_tests {
    use super::*;
    use iv_codec::{Animation, AnimationFrame};
    use iv_core::{ImageBuf, ImageInfo, Orientation, PixelFormat};

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
}

/// Two pages scaled to a common height: the combined size, and the share of its width taken by
/// the first page.
pub fn spread_layout(main: (u32, u32), second: (u32, u32)) -> ((u32, u32), f32) {
    let h = main.1.max(second.1).max(1) as f64;
    let main_w = main.0 as f64 * h / main.1.max(1) as f64;
    let second_w = second.0 as f64 * h / second.1.max(1) as f64;
    (((main_w + second_w).round() as u32, h as u32), (main_w / (main_w + second_w)) as f32)
}

#[cfg(test)]
mod spread_tests {
    use super::spread_layout;

    #[test]
    fn pages_share_a_height() {
        assert_eq!(spread_layout((800, 1200), (800, 1200)), ((1600, 1200), 0.5));
        // a half-height page is scaled up to match
        let ((w, h), f) = spread_layout((800, 1200), (400, 600));
        assert_eq!((w, h), (1600, 1200));
        assert!((f - 0.5).abs() < 1e-6);
        // a double-width spread page takes two thirds
        let (_, f) = spread_layout((1600, 1200), (800, 1200));
        assert!((f - 2.0 / 3.0).abs() < 1e-6);
    }
}

/// What the egui frame edits.
struct UiState<'a> {
    panel: &'a mut PanelState,
    settings: &'a mut Settings,
    settings_open: &'a mut bool,
}

/// Runs one egui frame (edit panel and/or settings window) and draws it over the image.
fn draw_ui(
    s: &mut Surface,
    ui_state: &mut UiState<'_>,
    cx: &PanelContext,
    save_settings: &mut bool,
    encoder: &mut wgpu::CommandEncoder,
    target: &wgpu::TextureView,
) -> Vec<PanelAction> {
    let e = &mut s.egui;
    let raw = e.state.take_egui_input(&s.window);
    let mut actions = Vec::new();
    let full = e.ctx.run_ui(raw, |ui| {
        if ui_state.panel.open {
            actions = panel::show(ui, ui_state.panel, cx);
        }
        if *ui_state.settings_open {
            *save_settings |= crate::settings::show(ui.ctx(), ui_state.settings_open, ui_state.settings);
        }
    });
    e.state.handle_platform_output(&s.window, full.platform_output);
    let ppp = full.pixels_per_point;
    let jobs = e.ctx.tessellate(full.shapes, ppp);
    let screen =
        egui_wgpu::ScreenDescriptor { size_in_pixels: [s.config.width, s.config.height], pixels_per_point: ppp };
    for (id, deltas) in &full.textures_delta.set {
        for delta in deltas {
            e.renderer.update_texture(&s.gpu.device, &s.gpu.queue, *id, delta);
        }
    }
    let callbacks = e.renderer.update_buffers(&s.gpu.device, &s.gpu.queue, encoder, &jobs, &screen);
    s.gpu.queue.submit(callbacks);
    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("edit panel"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        e.renderer.render(&mut pass.forget_lifetime(), &jobs, &screen);
    }
    for id in &full.textures_delta.free {
        e.renderer.free_texture(id);
    }
    // egui animates hover and focus; keep drawing while it asks for it
    if full.viewport_output.get(&egui::ViewportId::ROOT).is_some_and(|v| v.repaint_delay.is_zero()) {
        s.window.request_redraw();
    }
    actions
}

/// Renderer appearance from the settings.
fn apply_style(renderer: &mut ImageRenderer, settings: &Settings) {
    renderer.style.background = settings.background_linear();
    renderer.style.nearest_above_zoom = settings.nearest_above_zoom as f64;
}
