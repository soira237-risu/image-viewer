//! Draws one photo into a render target.

use iv_core::{ImageBuf, Orientation};

use crate::orient::{UvTransform, uv_transform};
use crate::upload;
use crate::view::ScreenRect;
use crate::view::ViewState;

/// sRGB texture: the sampler linearises, so mipmaps and filtering average light, not codes.
const TEXTURE_FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

/// Tone adjustments the shader previews per image.
pub const MAX_ADJUSTMENTS: usize = 4;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct Params {
    dest: [f32; 4],
    viewport: [f32; 4],
    uv_row0: [f32; 4],
    uv_row1: [f32; 4],
    checker_a: [f32; 4],
    checker_b: [f32; 4],
    /// Selection in screen pixels (x, y, w, h); width 0 = none. Outside it is dimmed.
    selection: [f32; 4],
    /// Tone adjustments (brightness, contrast, gamma, saturation), applied in order.
    adjust: [[f32; 4]; 4],
    /// x: number of adjustments in use.
    misc: [f32; 4],
}

/// Appearance settings.
#[derive(Clone, Copy, Debug)]
pub struct Style {
    /// Clear colour around the image (linear RGB).
    pub background: [f64; 3],
    /// Checkerboard behind transparent pixels (linear RGB).
    pub checker_a: [f32; 3],
    pub checker_b: [f32; 3],
    pub checker_size: f32,
    /// Above this zoom, pixels are drawn as hard squares instead of interpolated.
    pub nearest_above_zoom: f64,
}

impl Default for Style {
    fn default() -> Self {
        Self {
            background: [0.010, 0.010, 0.011],
            checker_a: [0.18, 0.18, 0.18],
            checker_b: [0.30, 0.30, 0.30],
            checker_size: 8.0,
            nearest_above_zoom: 3.0,
        }
    }
}

/// What [`ImageRenderer::set_image`] did with the pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct UploadInfo {
    pub texture_width: u32,
    pub texture_height: u32,
    pub mip_levels: u32,
    /// The image exceeded the GPU texture limit and was reduced.
    pub reduced: bool,
}

struct GpuImage {
    bind_group: wgpu::BindGroup,
    uniform: wgpu::Buffer,
    /// Tone adjustments previewed on this image (at most [`MAX_ADJUSTMENTS`]).
    adjust: Vec<[f32; 4]>,
    /// Shown UV → texture UV: the EXIF orientation, or an edit geometry set later.
    uv: UvTransform,
    has_alpha: bool,
    // kept alive for the bind group
    _texture: wgpu::Texture,
}

pub struct ImageRenderer {
    pipeline: wgpu::RenderPipeline,
    layout: wgpu::BindGroupLayout,
    sampler_linear: wgpu::Sampler,
    sampler_nearest: wgpu::Sampler,
    mip_pipeline: wgpu::RenderPipeline,
    mip_layout: wgpu::BindGroupLayout,
    mip_sampler: wgpu::Sampler,
    max_texture_side: u32,
    image: Option<GpuImage>,
    /// Two-page view: the page beside `image`.
    second: Option<GpuImage>,
    /// Share of the combined width taken by `image` when a second image is shown.
    split: f32,
    second_on_left: bool,
    selection: Option<ScreenRect>,
    pub style: Style,
}

fn sampler(device: &wgpu::Device, filter: wgpu::FilterMode, label: &str) -> wgpu::Sampler {
    device.create_sampler(&wgpu::SamplerDescriptor {
        label: Some(label),
        address_mode_u: wgpu::AddressMode::ClampToEdge,
        address_mode_v: wgpu::AddressMode::ClampToEdge,
        mag_filter: filter,
        min_filter: wgpu::FilterMode::Linear,
        mipmap_filter: wgpu::MipmapFilterMode::Linear,
        ..Default::default()
    })
}

fn texture_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Texture {
            sample_type: wgpu::TextureSampleType::Float { filterable: true },
            view_dimension: wgpu::TextureViewDimension::D2,
            multisampled: false,
        },
        count: None,
    }
}

fn sampler_entry(binding: u32) -> wgpu::BindGroupLayoutEntry {
    wgpu::BindGroupLayoutEntry {
        binding,
        visibility: wgpu::ShaderStages::FRAGMENT,
        ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::Filtering),
        count: None,
    }
}

fn pipeline(
    device: &wgpu::Device,
    label: &str,
    source: &str,
    layout: &wgpu::BindGroupLayout,
    target: wgpu::TextureFormat,
    topology: wgpu::PrimitiveTopology,
) -> wgpu::RenderPipeline {
    let module = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some(label),
        source: wgpu::ShaderSource::Wgsl(source.into()),
    });
    let pl = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some(label),
        bind_group_layouts: &[Some(layout)],
        immediate_size: 0,
    });
    device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
        label: Some(label),
        layout: Some(&pl),
        vertex: wgpu::VertexState {
            module: &module,
            entry_point: Some("vs_main"),
            compilation_options: Default::default(),
            buffers: &[],
        },
        primitive: wgpu::PrimitiveState { topology, ..Default::default() },
        depth_stencil: None,
        multisample: wgpu::MultisampleState::default(),
        fragment: Some(wgpu::FragmentState {
            module: &module,
            entry_point: Some("fs_main"),
            compilation_options: Default::default(),
            targets: &[Some(wgpu::ColorTargetState {
                format: target,
                blend: None,
                write_mask: wgpu::ColorWrites::ALL,
            })],
        }),
        multiview_mask: None,
        cache: None,
    })
}

impl ImageRenderer {
    /// `target_format` is the format of the views passed to [`ImageRenderer::render`]; it should
    /// be an sRGB format so linear shader output is encoded for display.
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("image layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX_FRAGMENT,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                texture_entry(1),
                sampler_entry(2),
                sampler_entry(3),
            ],
        });
        let mip_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("mip layout"),
            entries: &[texture_entry(0), sampler_entry(1)],
        });
        Self {
            pipeline: pipeline(
                device,
                "image",
                include_str!("shaders/image.wgsl"),
                &layout,
                target_format,
                wgpu::PrimitiveTopology::TriangleStrip,
            ),
            mip_pipeline: pipeline(
                device,
                "mip",
                include_str!("shaders/mip.wgsl"),
                &mip_layout,
                TEXTURE_FORMAT,
                wgpu::PrimitiveTopology::TriangleList,
            ),
            layout,
            sampler_linear: sampler(device, wgpu::FilterMode::Linear, "linear"),
            sampler_nearest: sampler(device, wgpu::FilterMode::Nearest, "nearest"),
            mip_sampler: sampler(device, wgpu::FilterMode::Linear, "mip"),
            mip_layout,
            max_texture_side: device.limits().max_texture_dimension_2d,
            image: None,
            second: None,
            split: 1.0,
            second_on_left: false,
            selection: None,
            style: Style::default(),
        }
    }

    /// Shows a second image beside the main one (two-page view). `primary_fraction` is the share
    /// of the combined width taken by the main image; `second_on_left` puts the second image on
    /// the left (right-to-left reading).
    #[allow(clippy::too_many_arguments)]
    pub fn set_second_image(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        img: &ImageBuf,
        orientation: Orientation,
        has_alpha: bool,
        primary_fraction: f32,
        second_on_left: bool,
    ) -> UploadInfo {
        let (gpu, info) = self.upload(device, queue, img, orientation, has_alpha);
        self.second = Some(gpu);
        self.split = primary_fraction.clamp(0.0, 1.0);
        self.second_on_left = second_on_left;
        info
    }

    pub fn clear_second(&mut self) {
        self.second = None;
        self.split = 1.0;
    }

    pub fn has_second(&self) -> bool {
        self.second.is_some()
    }

    /// Replaces how the current image is mapped onto its rectangle: an edit geometry (crop,
    /// rotation) shown without touching pixels. Reset by the next `set_image`.
    pub fn set_uv_transform(&mut self, uv: UvTransform) {
        if let Some(img) = &mut self.image {
            img.uv = uv;
        }
    }

    /// Previews tone adjustments on the main image: each is (brightness, contrast, gamma,
    /// saturation) as in `iv_ops::Adjust`, applied in order. Only the first
    /// [`MAX_ADJUSTMENTS`] are shown. Reset by the next `set_image`.
    pub fn set_adjustments(&mut self, adjust: &[[f32; 4]]) {
        if let Some(img) = &mut self.image {
            img.adjust = adjust.iter().take(MAX_ADJUSTMENTS).copied().collect();
        }
    }

    /// Dims everything outside `rect` (screen pixels), or nothing with `None`.
    pub fn set_selection(&mut self, rect: Option<ScreenRect>) {
        self.selection = rect.filter(|r| r.width >= 1.0 && r.height >= 1.0);
    }

    pub fn has_image(&self) -> bool {
        self.image.is_some()
    }

    pub fn clear_image(&mut self) {
        self.image = None;
        self.clear_second();
    }

    /// Uploads `img` (stored layout) as the main image and builds its mip chain. The previous main
    /// image is released; a second image (two-page view) stays until cleared.
    pub fn set_image(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        img: &ImageBuf,
        orientation: Orientation,
        has_alpha: bool,
    ) -> UploadInfo {
        let (gpu, info) = self.upload(device, queue, img, orientation, has_alpha);
        self.image = Some(gpu);
        info
    }

    fn upload(
        &self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        img: &ImageBuf,
        orientation: Orientation,
        has_alpha: bool,
    ) -> (GpuImage, UploadInfo) {
        let pixels = upload::prepare(img, self.max_texture_side);
        let (w, h) = (pixels.width, pixels.height);
        let mip_levels = 32 - w.max(h).leading_zeros();
        let size = wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 };
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("photo"),
            size,
            mip_level_count: mip_levels,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: TEXTURE_FORMAT,
            usage: wgpu::TextureUsages::TEXTURE_BINDING
                | wgpu::TextureUsages::COPY_DST
                | wgpu::TextureUsages::RENDER_ATTACHMENT,
            view_formats: &[],
        });
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &pixels.rgba8,
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w * 4), rows_per_image: Some(h) },
            size,
        );
        self.build_mips(device, queue, &texture, mip_levels);

        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
        // each image has its own parameters: two images drawn in one pass must not share them
        let uniform = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("image params"),
            size: std::mem::size_of::<Params>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("photo"),
            layout: &self.layout,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: uniform.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&self.sampler_linear) },
                wgpu::BindGroupEntry { binding: 3, resource: wgpu::BindingResource::Sampler(&self.sampler_nearest) },
            ],
        });
        let gpu = GpuImage {
            bind_group,
            uniform,
            adjust: Vec::new(),
            uv: uv_transform(orientation),
            has_alpha,
            _texture: texture,
        };
        let info = UploadInfo {
            texture_width: w,
            texture_height: h,
            mip_levels,
            reduced: (w, h) != (img.width(), img.height()),
        };
        (gpu, info)
    }

    fn build_mips(&self, device: &wgpu::Device, queue: &wgpu::Queue, texture: &wgpu::Texture, levels: u32) {
        if levels < 2 {
            return;
        }
        let level_view = |level: u32| {
            texture.create_view(&wgpu::TextureViewDescriptor {
                base_mip_level: level,
                mip_level_count: Some(1),
                ..Default::default()
            })
        };
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("mips") });
        for level in 1..levels {
            let src = level_view(level - 1);
            let dst = level_view(level);
            let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
                label: Some("mip"),
                layout: &self.mip_layout,
                entries: &[
                    wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(&src) },
                    wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&self.mip_sampler) },
                ],
            });
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("mip"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dst,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            pass.set_pipeline(&self.mip_pipeline);
            pass.set_bind_group(0, &bind_group, &[]);
            pass.draw(0..3, 0..1);
        }
        queue.submit([encoder.finish()]);
    }

    /// Records a pass that clears `target` and draws the current image per `view`.
    /// `viewport` is the target size in pixels.
    pub fn render(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        viewport: (u32, u32),
        view: Option<&ViewState>,
    ) {
        let [r, g, b] = self.style.background;
        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("image"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color { r, g, b, a: 1.0 }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        let (Some(image), Some(view)) = (&self.image, view) else { return };

        // the view spans both pages; split it between them
        let total = view.dest_rect();
        let (main_rect, second_rect) = match &self.second {
            None => (total, None),
            Some(_) => {
                let main_w = total.width * self.split as f64;
                let second_w = total.width - main_w;
                let (main_x, second_x) =
                    if self.second_on_left { (total.x + second_w, total.x) } else { (total.x, total.x + main_w) };
                (
                    ScreenRect { x: main_x, width: main_w, ..total },
                    Some(ScreenRect { x: second_x, width: second_w, ..total }),
                )
            }
        };
        pass.set_pipeline(&self.pipeline);
        self.draw_image(queue, &mut pass, image, main_rect, viewport, view.zoom());
        if let (Some(second), Some(rect)) = (&self.second, second_rect) {
            self.draw_image(queue, &mut pass, second, rect, viewport, view.zoom());
        }
    }

    fn draw_image(
        &self,
        queue: &wgpu::Queue,
        pass: &mut wgpu::RenderPass<'_>,
        image: &GpuImage,
        rect: ScreenRect,
        viewport: (u32, u32),
        zoom: f64,
    ) {
        let t = image.uv;
        let mut flags = 0u32;
        if image.has_alpha {
            flags |= 1;
        }
        if zoom > self.style.nearest_above_zoom {
            flags |= 2;
        }
        let s = &self.style;
        let params = Params {
            dest: [rect.x as f32, rect.y as f32, rect.width as f32, rect.height as f32],
            viewport: [viewport.0.max(1) as f32, viewport.1.max(1) as f32, s.checker_size, flags as f32],
            uv_row0: [t[0][0], t[0][1], t[0][2], 0.0],
            uv_row1: [t[1][0], t[1][1], t[1][2], 0.0],
            checker_a: [s.checker_a[0], s.checker_a[1], s.checker_a[2], 1.0],
            checker_b: [s.checker_b[0], s.checker_b[1], s.checker_b[2], 1.0],
            selection: self.selection.map_or([0.0; 4], |r| [r.x as f32, r.y as f32, r.width as f32, r.height as f32]),
            adjust: std::array::from_fn(|i| image.adjust.get(i).copied().unwrap_or([0.0, 0.0, 1.0, 0.0])),
            misc: [image.adjust.len() as f32, 0.0, 0.0, 0.0],
        };
        queue.write_buffer(&image.uniform, 0, bytemuck::bytes_of(&params));
        pass.set_bind_group(0, &image.bind_group, &[]);
        pass.draw(0..4, 0..1);
    }
}
