//! Animated GIF, WebP and APNG: every frame composited, delays kept, memory counted.

use std::io::Cursor;
use std::time::Duration;

use image::codecs::gif::GifEncoder;
use image::{Delay, Frame, Rgba, RgbaImage};
use iv_codec::default_registry;
use iv_core::{CancelToken, Source};

const W: u32 = 8;
const H: u32 = 6;

/// Three solid frames: red, green, blue.
fn colours() -> [[u8; 4]; 3] {
    [[255, 0, 0, 255], [0, 255, 0, 255], [0, 0, 255, 255]]
}

fn solid(c: [u8; 4]) -> RgbaImage {
    RgbaImage::from_pixel(W, H, Rgba(c))
}

fn gif(delays_ms: [u32; 3]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = GifEncoder::new(&mut out);
        enc.set_repeat(image::codecs::gif::Repeat::Infinite).unwrap();
        for (c, ms) in colours().into_iter().zip(delays_ms) {
            enc.encode_frame(Frame::from_parts(solid(c), 0, 0, Delay::from_numer_denom_ms(ms, 1))).unwrap();
        }
    }
    out
}

fn apng(delays_ms: [u16; 3]) -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, W, H);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        enc.set_animated(3, 0).unwrap();
        let mut writer = enc.write_header().unwrap();
        for (c, ms) in colours().into_iter().zip(delays_ms) {
            writer.set_frame_delay(ms, 1000).unwrap();
            writer.write_image_data(solid(c).as_raw()).unwrap();
        }
        writer.finish().unwrap();
    }
    out
}

fn le24(v: u32) -> [u8; 3] {
    let b = v.to_le_bytes();
    [b[0], b[1], b[2]]
}

fn chunk(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut c = tag.to_vec();
    c.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    c.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        c.push(0);
    }
    c
}

/// Animated WebP assembled by hand: each frame is a lossless still from `image-webp`, wrapped in
/// an ANMF chunk (WebP container spec), with VP8X + ANIM headers.
fn webp_animation(delays_ms: [u32; 3]) -> Vec<u8> {
    let mut body = b"WEBP".to_vec();
    let mut vp8x = vec![0x02 | 0x10, 0, 0, 0]; // animation + alpha flags
    vp8x.extend_from_slice(&le24(W - 1));
    vp8x.extend_from_slice(&le24(H - 1));
    body.extend(chunk(b"VP8X", &vp8x));
    body.extend(chunk(b"ANIM", &[0, 0, 0, 0, 0, 0])); // background, loop forever
    for (c, ms) in colours().into_iter().zip(delays_ms) {
        let mut still = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(solid(c)).write_to(&mut still, image::ImageFormat::WebP).unwrap();
        let still = still.into_inner();
        let mut anmf = Vec::new();
        anmf.extend_from_slice(&le24(0)); // x / 2
        anmf.extend_from_slice(&le24(0)); // y / 2
        anmf.extend_from_slice(&le24(W - 1));
        anmf.extend_from_slice(&le24(H - 1));
        anmf.extend_from_slice(&le24(ms));
        anmf.push(0b10); // no blending, no disposal
        anmf.extend_from_slice(&still[12..]); // the VP8L chunk, after "RIFF....WEBP"
        body.extend(chunk(b"ANMF", &anmf));
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend(body);
    out
}

fn check(name: &str, bytes: Vec<u8>, expected_ms: [u64; 3]) {
    let d = default_registry().decode(&Source::from_bytes(name, bytes), &CancelToken::never()).unwrap();
    let anim = d.animation.as_ref().unwrap_or_else(|| panic!("{name}: not animated"));
    assert_eq!(anim.frames.len(), 3, "{name}");
    assert_eq!(d.info.frame_count, Some(3), "{name}");
    for (i, (frame, colour)) in anim.frames.iter().zip(colours()).enumerate() {
        assert_eq!((frame.image.width(), frame.image.height()), (W, H), "{name} frame {i}");
        assert_eq!(&frame.image.data()[..4], &colour, "{name} frame {i}");
        assert_eq!(frame.delay, Duration::from_millis(expected_ms[i]), "{name} frame {i}");
    }
    assert_eq!(d.image.data(), anim.frames[0].image.data(), "{name}: first frame is the still image");
    assert_eq!(d.memory_size(), 3 * (W * H * 4) as usize, "{name}: every frame counted once");
}

#[test]
fn animated_gif() {
    // a 0 ms delay plays at 100 ms, as in browsers
    check("a.gif", gif([50, 0, 200]), [50, 100, 200]);
}

#[test]
fn animated_png() {
    check("a.png", apng([40, 80, 120]), [40, 80, 120]);
}

#[test]
fn animated_webp() {
    check("a.webp", webp_animation([30, 60, 90]), [30, 60, 90]);
}

#[test]
fn single_frame_gif_is_a_still_image() {
    let mut out = Vec::new();
    {
        let mut enc = GifEncoder::new(&mut out);
        enc.encode_frame(Frame::new(solid([1, 2, 3, 255]))).unwrap();
    }
    let d = default_registry().decode(&Source::from_bytes("one.gif", out), &CancelToken::never()).unwrap();
    assert!(d.animation.is_none());
    assert_eq!(d.info.frame_count, Some(1));
}
