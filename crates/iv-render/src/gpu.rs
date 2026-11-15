//! Device acquisition shared by the window and headless (test) paths.

/// The wgpu objects every renderer needs.
pub struct Gpu {
    pub instance: wgpu::Instance,
    pub adapter: wgpu::Adapter,
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
}

/// Backends to try for a windowed app: `WGPU_BACKEND` when set, otherwise the platform's
/// preferred one first. On Windows, DX12 reached the first full frame faster than Vulkan in
/// measurement (median 326 ms vs 380 ms over 5 runs each, RX 9060).
pub fn preferred_backends() -> Vec<wgpu::Backends> {
    if let Some(env) = wgpu::Backends::from_env() {
        return vec![env];
    }
    if cfg!(windows) { vec![wgpu::Backends::DX12, wgpu::Backends::all()] } else { vec![wgpu::Backends::all()] }
}

/// A new instance limited to `backends`.
pub fn instance(backends: wgpu::Backends) -> wgpu::Instance {
    let mut desc = wgpu::InstanceDescriptor::new_without_display_handle_from_env();
    desc.backends = backends;
    wgpu::Instance::new(desc)
}

impl Gpu {
    /// Picks a high-performance adapter able to present to `surface` (if given), falling back to a
    /// software adapter (WARP on Windows) when no hardware adapter is available.
    pub async fn new(instance: wgpu::Instance, surface: Option<&wgpu::Surface<'_>>) -> Result<Self, String> {
        let mut options = wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::HighPerformance,
            force_fallback_adapter: false,
            compatible_surface: surface,
            ..Default::default()
        };
        let adapter = match instance.request_adapter(&options).await {
            Ok(a) => a,
            Err(first) => {
                options.force_fallback_adapter = true;
                instance
                    .request_adapter(&options)
                    .await
                    .map_err(|e| format!("no GPU adapter: {first}; fallback: {e}"))?
            }
        };
        let (device, queue) = adapter
            .request_device(&wgpu::DeviceDescriptor {
                label: Some("iv device"),
                // the adapter's own limits: large photos need its full texture size
                required_limits: adapter.limits(),
                ..Default::default()
            })
            .await
            .map_err(|e| format!("request_device: {e}"))?;
        Ok(Self { instance, adapter, device, queue })
    }

    /// Tries each backend set in order, without a surface (so it can run before the window
    /// exists). The caller checks [`wgpu::Adapter::is_surface_supported`] once it has a surface.
    pub async fn first_available(backends: &[wgpu::Backends]) -> Result<Self, String> {
        let mut errors = Vec::new();
        for &b in backends {
            match Self::new(instance(b), None).await {
                Ok(gpu) => return Ok(gpu),
                Err(e) => errors.push(format!("{b:?}: {e}")),
            }
        }
        Err(errors.join("; "))
    }

    /// Headless device for tests and offscreen rendering.
    pub async fn headless() -> Result<Self, String> {
        Self::new(wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env()), None).await
    }

    pub fn max_texture_side(&self) -> u32 {
        self.device.limits().max_texture_dimension_2d
    }
}
