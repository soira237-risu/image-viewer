//! Renders offscreen on a real (or software) GPU and reads the pixels back.

use iv_core::{ImageBuf, Orientation, PixelFormat};
use iv_render::{Gpu, ImageRenderer, ViewState, wgpu};

const W: u32 = 64; // 64 * 4 bytes = 256, the required row alignment for readback
const H: u32 = 48;
const TARGET: wgpu::TextureFormat = wgpu::TextureFormat::Rgba8UnormSrgb;

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

fn gpu() -> Gpu {
    pollster::block_on(Gpu::headless()).expect("a GPU or software adapter")
}

fn image(w: u32, h: u32, pixels: &[[u8; 4]]) -> ImageBuf {
    ImageBuf::new(w, h, PixelFormat::Rgba8, pixels.concat()).unwrap()
}

/// Draws and returns the target as tightly packed RGBA8 rows.
fn draw(gpu: &Gpu, renderer: &ImageRenderer, view: &ViewState) -> Vec<u8> {
    let target = gpu.device.create_texture(&wgpu::TextureDescriptor {
        label: None,
        size: wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: TARGET,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let readback = gpu.device.create_buffer(&wgpu::BufferDescriptor {
        label: None,
        size: (W * H * 4) as u64,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    let target_view = target.create_view(&Default::default());
    let mut enc = gpu.device.create_command_encoder(&Default::default());
    renderer.render(&gpu.queue, &mut enc, &target_view, (W, H), Some(view));
    enc.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &target,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback,
            layout: wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(W * 4), rows_per_image: Some(H) },
        },
        wgpu::Extent3d { width: W, height: H, depth_or_array_layers: 1 },
    );
    gpu.queue.submit([enc.finish()]);
    readback.map_async(wgpu::MapMode::Read, .., |r| r.expect("map"));
    gpu.device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
    let data = readback.get_mapped_range(..).expect("mapped").to_vec();
    readback.unmap();
    data
}

fn px(buf: &[u8], x: u32, y: u32) -> [u8; 4] {
    let i = ((y * W + x) * 4) as usize;
    buf[i..i + 4].try_into().unwrap()
}

fn near(a: [u8; 4], b: [u8; 4], tol: i32) -> bool {
    a.iter().zip(b).all(|(x, y)| (*x as i32 - y as i32).abs() <= tol)
}

/// 2x2 image zoomed to 48x48 (nearest), centred in the 64x48 target: quadrants at
/// x 8..32 | 32..56, y 0..24 | 24..48.
fn quadrants(gpu: &Gpu, orientation: Orientation) -> [[u8; 4]; 4] {
    let mut r = ImageRenderer::new(&gpu.device, TARGET);
    r.set_image(&gpu.device, &gpu.queue, &image(2, 2, &[RED, GREEN, BLUE, WHITE]), orientation, false);
    let mut view = ViewState::new((2, 2), (W, H));
    view.set_zoom_at(24.0, (32.0, 24.0));
    let out = draw(gpu, &r, &view);
    [px(&out, 20, 12), px(&out, 44, 12), px(&out, 20, 36), px(&out, 44, 36)]
}

#[test]
fn draws_pixels_in_place_with_background_around() {
    let gpu = gpu();
    assert_eq!(quadrants(&gpu, Orientation::Normal), [RED, GREEN, BLUE, WHITE]);

    // left of the image is the clear colour, not image content
    let mut r = ImageRenderer::new(&gpu.device, TARGET);
    r.set_image(&gpu.device, &gpu.queue, &image(2, 2, &[RED, GREEN, BLUE, WHITE]), Orientation::Normal, false);
    let mut view = ViewState::new((2, 2), (W, H));
    view.set_zoom_at(24.0, (32.0, 24.0));
    let out = draw(&gpu, &r, &view);
    let bg = px(&out, 2, 24);
    assert!(bg[0] < 40 && bg[1] < 40 && bg[2] < 40, "background {bg:?}");
}

#[test]
fn orientation_is_applied_on_the_gpu() {
    let gpu = gpu();
    // stored [[R,G],[B,W]] rotated 90° clockwise displays [[B,R],[W,G]]
    assert_eq!(quadrants(&gpu, Orientation::Rotate90), [BLUE, RED, WHITE, GREEN]);
    assert_eq!(quadrants(&gpu, Orientation::Rotate180), [WHITE, BLUE, GREEN, RED]);
    assert_eq!(quadrants(&gpu, Orientation::FlipH), [GREEN, RED, WHITE, BLUE]);
    assert_eq!(quadrants(&gpu, Orientation::Rotate270), [GREEN, WHITE, RED, BLUE]);
}

#[test]
fn transparency_shows_the_checkerboard() {
    let gpu = gpu();
    let mut r = ImageRenderer::new(&gpu.device, TARGET);
    r.set_image(&gpu.device, &gpu.queue, &image(1, 1, &[[255, 0, 0, 0]]), Orientation::Normal, true);
    let mut view = ViewState::new((1, 1), (W, H));
    view.set_zoom_at(48.0, (32.0, 24.0));
    let out = draw(&gpu, &r, &view);
    // two neighbouring 8px cells inside the image differ, and neither is red
    let a = px(&out, 12, 4);
    let b = px(&out, 20, 4);
    assert_ne!(a, b, "checker cells alternate");
    for c in [a, b] {
        assert!(c[0] == c[1] && c[1] == c[2], "grey checker, got {c:?}");
    }
}

#[test]
fn zooming_out_averages_through_mipmaps() {
    let gpu = gpu();
    // 256x256, one white column in every 8, shown at exactly 1/8 scale. A bilinear tap without
    // mips lands between two black columns (0); the area average is 1/8 linear white.
    let stripes: Vec<[u8; 4]> =
        (0..256 * 256).map(|i| if (i % 256) % 8 == 0 { WHITE } else { [0, 0, 0, 255] }).collect();
    let mut r = ImageRenderer::new(&gpu.device, TARGET);
    let info = r.set_image(&gpu.device, &gpu.queue, &image(256, 256, &stripes), Orientation::Normal, false);
    assert_eq!(info.mip_levels, 9);
    let mut view = ViewState::new((256, 256), (W, H));
    view.set_zoom_at(1.0 / 8.0, (32.0, 24.0));
    let out = draw(&gpu, &r, &view);
    // 0.125 linear is sRGB ≈ 99 (an sRGB-space average would give 32)
    let c = px(&out, 32, 24);
    assert!(near(c, [99, 99, 99, 255], 6), "got {c:?}");
}

#[test]
fn sixteen_bit_images_upload() {
    let gpu = gpu();
    let px16: [u16; 16] = [65535, 0, 0, 65535, 0, 65535, 0, 65535, 0, 0, 65535, 65535, 65535, 65535, 65535, 65535];
    let img = ImageBuf::new(2, 2, PixelFormat::Rgba16, bytemuck::cast_slice(&px16).to_vec()).unwrap();
    let mut r = ImageRenderer::new(&gpu.device, TARGET);
    r.set_image(&gpu.device, &gpu.queue, &img, Orientation::Normal, false);
    let mut view = ViewState::new((2, 2), (W, H));
    view.set_zoom_at(24.0, (32.0, 24.0));
    let out = draw(&gpu, &r, &view);
    assert_eq!([px(&out, 20, 12), px(&out, 44, 12), px(&out, 20, 36), px(&out, 44, 36)], [RED, GREEN, BLUE, WHITE]);
}

#[test]
fn uv_override_shows_an_edit_without_new_pixels() {
    let gpu = gpu();
    let mut r = ImageRenderer::new(&gpu.device, TARGET);
    r.set_image(&gpu.device, &gpu.queue, &image(2, 2, &[RED, GREEN, BLUE, WHITE]), Orientation::Normal, false);
    // crop to the right column (green over white), shown 1x2 scaled up
    r.set_uv_transform([[0.5, 0.0, 0.5], [0.0, 1.0, 0.0]]);
    let mut view = ViewState::new((1, 2), (W, H));
    view.set_zoom_at(24.0, (32.0, 24.0));
    let out = draw(&gpu, &r, &view);
    assert_eq!(px(&out, 32, 12), GREEN);
    assert_eq!(px(&out, 32, 36), WHITE);
}

#[test]
fn selection_dims_the_outside() {
    let gpu = gpu();
    let mut r = ImageRenderer::new(&gpu.device, TARGET);
    r.set_image(&gpu.device, &gpu.queue, &image(1, 1, &[WHITE]), Orientation::Normal, false);
    let mut view = ViewState::new((1, 1), (W, H));
    view.set_zoom_at(48.0, (32.0, 24.0));
    r.set_selection(Some(iv_render::ScreenRect { x: 8.0, y: 0.0, width: 24.0, height: 48.0 }));
    let out = draw(&gpu, &r, &view);
    assert_eq!(px(&out, 20, 24), WHITE, "inside the selection");
    let dim = px(&out, 44, 24);
    assert!(dim[0] < 200 && dim[0] == dim[1] && dim[3] == 255, "outside is darkened: {dim:?}");
    r.set_selection(None);
    assert_eq!(px(&draw(&gpu, &r, &view), 44, 24), WHITE);
}
