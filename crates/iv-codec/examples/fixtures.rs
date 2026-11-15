//! Writes visual check images into a directory:
//! - `rotated-6.jpg`: a JPEG whose pixels are stored sideways with EXIF orientation 6 (it must
//!   display upright, with an arrow pointing up)
//! - `alpha.png`: a horizontal alpha ramp over a red/blue split (left edge fully transparent)
//!
//! ```text
//! cargo run -p iv-codec --example fixtures -- testdata/local/fixtures
//! ```

use std::io::Cursor;
use std::path::Path;

use image::{DynamicImage, ImageFormat, Rgb, RgbImage, Rgba, RgbaImage};

/// An upright arrow (pointing up) on white, 300x400.
fn upright_arrow() -> RgbImage {
    RgbImage::from_fn(300, 400, |x, y| {
        let (cx, x, y) = (150i32, x as i32, y as i32);
        let head = y < 150 && (x - cx).abs() < (y - 20).max(0); // triangle
        let shaft = (150..380).contains(&y) && (x - cx).abs() < 30;
        if head || shaft { Rgb([200, 30, 30]) } else { Rgb([245, 245, 245]) }
    })
}

/// Rotates 90° counter-clockwise: what a camera stores when EXIF 6 says "rotate 90° CW to view".
fn rotate_ccw(img: &RgbImage) -> RgbImage {
    let (w, h) = img.dimensions();
    RgbImage::from_fn(h, w, |x, y| *img.get_pixel(w - 1 - y, x))
}

fn exif_app1(orientation: u16) -> Vec<u8> {
    let mut tiff = b"II\x2A\x00".to_vec();
    tiff.extend_from_slice(&8u32.to_le_bytes());
    tiff.extend_from_slice(&1u16.to_le_bytes());
    tiff.extend_from_slice(&0x0112u16.to_le_bytes());
    tiff.extend_from_slice(&3u16.to_le_bytes());
    tiff.extend_from_slice(&1u32.to_le_bytes());
    tiff.extend_from_slice(&orientation.to_le_bytes());
    tiff.extend_from_slice(&[0, 0, 0, 0, 0, 0]); // value padding + no next IFD
    let mut seg = vec![0xFF, 0xE1];
    seg.extend_from_slice(&((2 + 6 + tiff.len()) as u16).to_be_bytes());
    seg.extend_from_slice(b"Exif\0\0");
    seg.extend_from_slice(&tiff);
    seg
}

fn main() {
    let out = std::env::args().nth(1).expect("usage: fixtures <output dir>");
    let out = Path::new(&out);
    std::fs::create_dir_all(out).unwrap();

    let stored = rotate_ccw(&upright_arrow());
    let mut jpeg = Cursor::new(Vec::new());
    DynamicImage::ImageRgb8(stored).write_to(&mut jpeg, ImageFormat::Jpeg).unwrap();
    let jpeg = jpeg.into_inner();
    let mut with_exif = jpeg[..2].to_vec();
    with_exif.extend_from_slice(&exif_app1(6));
    with_exif.extend_from_slice(&jpeg[2..]);
    std::fs::write(out.join("rotated-6.jpg"), with_exif).unwrap();

    let alpha = RgbaImage::from_fn(400, 200, |x, y| {
        let c = if y < 100 { [220, 40, 40] } else { [40, 60, 220] };
        Rgba([c[0], c[1], c[2], (x * 255 / 399) as u8])
    });
    alpha.save(out.join("alpha.png")).unwrap();
    println!("wrote {}", out.display());
}
