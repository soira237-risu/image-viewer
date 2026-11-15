//! The window: input → commands, load events → textures, redraws on demand.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iv_codec::Decoded;
use iv_core::{FormatId, ImageBuf, ImageInfo};
use iv_render::{Gpu, ImageRenderer, ViewState, ZoomMode, wgpu};
use winit::application::ApplicationHandler;
use winit::dpi::{PhysicalPosition, PhysicalSize};
use winit::event::{ElementState, KeyEvent, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, EventLoopProxy};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Fullscreen, Window, WindowId};

use crate::commands::Command;
use crate::keymap::{Chord, Keymap, canonical_key};
use crate::loader::{LoadEvent, Loader};
use crate::trace::Trace;

const ZOOM_STEP: f64 = 1.25;
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

#[derive(Debug)]
pub enum AppEvent {
    Load(LoadEvent),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Shown {
    Nothing,
    Preview,
    Full,
}

/// The photo being shown and what has arrived for it so far.
struct Current {
    path: PathBuf,
    info: ImageInfo,
    preview: Option<ImageBuf>,
    full: Option<Decoded>,
    load_time: Option<Duration>,
    shown: Shown,
    view: ViewState,
}

struct Surface {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    config: wgpu::SurfaceConfiguration,
    gpu: Gpu,
    renderer: ImageRenderer,
    /// format the renderer targets (the sRGB view of the swapchain)
    view_format: wgpu::TextureFormat,
}

pub struct App {
    proxy: EventLoopProxy<AppEvent>,
    loader: Loader,
    keymap: Keymap,
    trace: Trace,
    surface: Option<Surface>,
    current: Option<Current>,
    error: Option<String>,
    modifiers: ModifiersState,
    cursor: PhysicalPosition<f64>,
    drag_from: Option<PhysicalPosition<f64>>,
    last_click: Option<(Instant, PhysicalPosition<f64>)>,
    first_frame_logged: bool,
    gpu_init: Option<GpuInit>,
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

impl App {
    pub fn new(
        proxy: EventLoopProxy<AppEvent>,
        loader: Loader,
        keymap: Keymap,
        trace: Trace,
        gpu_init: Option<GpuInit>,
    ) -> Self {
        Self {
            gpu_init,
            proxy,
            loader,
            keymap,
            trace,
            surface: None,
            current: None,
            error: None,
            modifiers: ModifiersState::empty(),
            cursor: PhysicalPosition::new(0.0, 0.0),
            drag_from: None,
            last_click: None,
            first_frame_logged: false,
        }
    }

    pub fn open(&mut self, path: PathBuf) {
        let proxy = self.proxy.clone();
        self.trace.mark(&format!("load start {}", path.display()));
        self.loader.load(path, move |e| {
            // the window may already be closing; nothing to do then
            let _ = proxy.send_event(AppEvent::Load(e));
        });
    }

    fn viewport(&self) -> (u32, u32) {
        self.surface.as_ref().map_or((1280, 800), |s| (s.config.width, s.config.height))
    }

    fn create_surface(&mut self, event_loop: &ActiveEventLoop) -> Result<(), String> {
        let attrs =
            Window::default_attributes().with_title("image-viewer").with_inner_size(PhysicalSize::new(1280, 800));
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

        let renderer = ImageRenderer::new(&gpu.device, view_format);
        self.trace.mark("pipelines created");
        self.surface = Some(Surface { window, surface, config, gpu, renderer, view_format });
        Ok(())
    }

    /// Slow path: any backend, adapter chosen for this window's surface.
    fn gpu_for_window(window: &Arc<Window>) -> Result<(Gpu, wgpu::Surface<'static>), String> {
        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let surface = instance.create_surface(window.clone()).map_err(|e| e.to_string())?;
        let gpu = pollster::block_on(Gpu::new(instance, Some(&surface)))?;
        Ok((gpu, surface))
    }

    /// Uploads the best image that has arrived but is not on the GPU yet.
    fn sync_texture(&mut self) {
        let (Some(s), Some(cur)) = (self.surface.as_mut(), self.current.as_mut()) else { return };
        let orientation = cur.info.orientation;
        if let Some(full) = &cur.full {
            if cur.shown != Shown::Full {
                let t = Instant::now();
                let up =
                    s.renderer.set_image(&s.gpu.device, &s.gpu.queue, &full.image, orientation, full.info.has_alpha);
                cur.shown = Shown::Full;
                cur.preview = None;
                self.trace.mark(&format!(
                    "full image uploaded {}x{} ({} mips{}) in {:.1} ms",
                    up.texture_width,
                    up.texture_height,
                    up.mip_levels,
                    if up.reduced { ", reduced to GPU limit" } else { "" },
                    t.elapsed().as_secs_f64() * 1000.0
                ));
                s.window.request_redraw();
            }
        } else if let Some(preview) = &cur.preview {
            if cur.shown == Shown::Nothing {
                s.renderer.set_image(&s.gpu.device, &s.gpu.queue, preview, orientation, cur.info.has_alpha);
                cur.shown = Shown::Preview;
                self.trace.mark(&format!("preview uploaded {}x{}", preview.width(), preview.height()));
                s.window.request_redraw();
            }
        }
    }

    fn update_title(&self) {
        let Some(s) = &self.surface else { return };
        let title = match (&self.current, &self.error) {
            (_, Some(err)) => format!("image-viewer — {err}"),
            (Some(cur), None) => {
                let name =
                    cur.path.file_name().map_or_else(|| cur.path.display().to_string(), |n| n.to_string_lossy().into());
                let (w, h) = cur.info.display_size();
                let zoom = (cur.view.zoom() * 100.0).round();
                let state = match (cur.shown, cur.load_time) {
                    (Shown::Full, Some(t)) => {
                        format!("{} · {} · {:.0} ms", cur.info.format, cur.info.codec, t.as_secs_f64() * 1000.0)
                    }
                    (Shown::Preview, _) => format!("{} · preview", cur.info.format),
                    _ => format!("{} · loading", cur.info.format),
                };
                format!("{name} — {w}×{h} — {zoom}% — {state}")
            }
            (None, None) => "image-viewer — Ctrl+O to open, or drop a file".to_string(),
        };
        s.window.set_title(&title);
    }

    fn handle_load(&mut self, event: LoadEvent) {
        let current_gen = self.loader.current_generation();
        match event {
            LoadEvent::Probed { generation, path, info } if generation == current_gen => {
                self.trace.mark(&format!("probed {}x{} {}", info.width, info.height, info.format));
                let view = ViewState::new(info.display_size(), self.viewport());
                self.error = None;
                self.current = Some(Current {
                    path,
                    info,
                    preview: None,
                    full: None,
                    load_time: None,
                    shown: Shown::Nothing,
                    view,
                });
                if let Some(s) = &mut self.surface {
                    s.renderer.clear_image();
                    s.window.request_redraw();
                }
            }
            LoadEvent::Preview { generation, image } if generation == current_gen => {
                self.trace.mark(&format!("preview decoded {}x{}", image.width(), image.height()));
                if let Some(cur) = &mut self.current {
                    cur.preview = Some(image);
                }
            }
            LoadEvent::Decoded { generation, decoded, elapsed } if generation == current_gen => {
                self.trace.mark(&format!(
                    "decoded by {} ({:.1} ms since load start)",
                    decoded.info.codec,
                    elapsed.as_secs_f64() * 1000.0
                ));
                if let Some(cur) = &mut self.current {
                    cur.info = decoded.info.clone();
                    cur.view.set_image_size(cur.info.display_size());
                    cur.load_time = Some(elapsed);
                    cur.full = Some(decoded);
                }
            }
            LoadEvent::Failed { generation, path, error } if generation == current_gen => {
                eprintln!("{}: {error}", path.display());
                self.error = Some(format!(
                    "{}: {error}",
                    path.file_name().map_or_else(String::new, |n| n.to_string_lossy().into())
                ));
                self.current = None;
                if let Some(s) = &mut self.surface {
                    s.renderer.clear_image();
                    s.window.request_redraw();
                }
            }
            // a superseded load finished late
            _ => {}
        }
        self.sync_texture();
        self.update_title();
    }

    fn run_command(&mut self, cmd: Command, event_loop: &ActiveEventLoop) {
        let center = {
            let (w, h) = self.viewport();
            (w as f64 / 2.0, h as f64 / 2.0)
        };
        match cmd {
            Command::FileOpen => {
                let exts: Vec<&str> = FormatId::ALL.iter().flat_map(|f| f.extensions().iter().copied()).collect();
                if let Some(path) = rfd::FileDialog::new().add_filter("Images", &exts).pick_file() {
                    self.open(path);
                }
            }
            Command::ViewFit => self.with_view(|v| v.fit()),
            Command::ViewActualSize => self.with_view(|v| v.actual_size_at(center)),
            Command::ViewZoomIn => self.with_view(|v| v.zoom_by(ZOOM_STEP)),
            Command::ViewZoomOut => self.with_view(|v| v.zoom_by(1.0 / ZOOM_STEP)),
            Command::ViewToggleFullscreen => {
                if let Some(s) = &self.surface {
                    let next = if s.window.fullscreen().is_some() { None } else { Some(Fullscreen::Borderless(None)) };
                    s.window.set_fullscreen(next);
                }
            }
            Command::Escape => match &self.surface {
                Some(s) if s.window.fullscreen().is_some() => s.window.set_fullscreen(None),
                _ => event_loop.exit(),
            },
            Command::AppQuit => event_loop.exit(),
        }
    }

    fn with_view(&mut self, f: impl FnOnce(&mut ViewState)) {
        if let Some(cur) = &mut self.current {
            f(&mut cur.view);
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
        let view = self.current.as_ref().filter(|c| c.shown != Shown::Nothing).map(|c| &c.view);
        s.renderer.render(&s.gpu.queue, &mut encoder, &target, (s.config.width, s.config.height), view);
        s.gpu.queue.submit([encoder.finish()]);
        s.window.pre_present_notify();
        s.gpu.queue.present(frame);

        if !self.first_frame_logged {
            self.first_frame_logged = true;
            self.trace.mark("first frame presented");
        }
        if let Some(cur) = &self.current {
            self.trace.mark_once(match cur.shown {
                Shown::Preview => "first preview frame presented",
                Shown::Full => "first full-image frame presented",
                Shown::Nothing => "",
            });
        }
    }

    fn on_key(&mut self, event: &KeyEvent, event_loop: &ActiveEventLoop) {
        if event.state != ElementState::Pressed {
            return;
        }
        let Some(name) = key_name(&event.logical_key) else { return };
        let m = self.modifiers;
        let chord = Chord::new(name, m.control_key(), m.alt_key(), m.shift_key());
        if let Some(cmd) = self.keymap.lookup(&chord) {
            self.run_command(cmd, event_loop);
        }
    }

    fn on_mouse_button(&mut self, state: ElementState, button: MouseButton) {
        if button != MouseButton::Left {
            return;
        }
        match state {
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
            ElementState::Released => self.drag_from = None,
        }
    }
}

impl ApplicationHandler<AppEvent> for App {
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
            AppEvent::Load(e) => self.handle_load(e),
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => event_loop.exit(),
            WindowEvent::Resized(size) => {
                if let Some(s) = &mut self.surface {
                    s.config.width = size.width.max(1);
                    s.config.height = size.height.max(1);
                    s.surface.configure(&s.gpu.device, &s.config);
                    s.window.request_redraw();
                }
                let vp = self.viewport();
                if let Some(cur) = &mut self.current {
                    cur.view.set_viewport(vp);
                }
                self.update_title();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::ModifiersChanged(m) => self.modifiers = m.state(),
            WindowEvent::KeyboardInput { event, .. } => self.on_key(&event, event_loop),
            WindowEvent::CursorMoved { position, .. } => {
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
    fn ctrl_o_from_winit_hits_the_default_binding() {
        let chord = Chord::new(key_name(&Key::Character("o".into())).unwrap(), true, false, false);
        assert_eq!(Keymap::defaults().lookup(&chord), Some(Command::FileOpen));
        // Shift+= delivers "+"
        let plus = Chord::new(key_name(&Key::Character("+".into())).unwrap(), false, false, true);
        assert_eq!(Keymap::defaults().lookup(&plus), Some(Command::ViewZoomIn));
    }
}
