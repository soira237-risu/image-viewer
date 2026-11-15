//! Builds a multi-format benchmark corpus from existing images.
//!
//! ```text
//! cargo run -p iv-codec --release --example make_corpus -- <source dir> <output dir> [count]
//! ```
//!
//! Each of the first `count` (default 5) JPEG/PNG files in `<source dir>` is re-encoded as PNG,
//! 16-bit PNG, lossless WebP, TIFF, QOI and BMP, and the JPEG itself is copied. Output belongs
//! in `testdata/local/`, which git ignores.

use std::path::{Path, PathBuf};

use image::{DynamicImage, ImageFormat};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() < 2 {
        eprintln!("usage: make_corpus <source dir> <output dir> [count]");
        std::process::exit(2);
    }
    let src_dir = Path::new(&args[0]);
    let out_dir = Path::new(&args[1]);
    let count: usize = args.get(2).map_or(5, |c| c.parse().expect("count must be a number"));
    std::fs::create_dir_all(out_dir).expect("create output dir");

    let mut sources: Vec<PathBuf> = std::fs::read_dir(src_dir)
        .expect("read source dir")
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            matches!(
                p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref(),
                Some("jpg" | "jpeg" | "png")
            )
        })
        .collect();
    sources.sort();

    for path in sources.into_iter().take(count) {
        let stem = path.file_stem().unwrap().to_string_lossy().into_owned();
        let img = image::open(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        println!("{} {}x{}", path.display(), img.width(), img.height());

        if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("jpg") || e.eq_ignore_ascii_case("jpeg")) {
            std::fs::copy(&path, out_dir.join(format!("{stem}.jpg"))).expect("copy jpeg");
        }
        let rgb = DynamicImage::ImageRgb8(img.to_rgb8());
        let rgba = DynamicImage::ImageRgba8(img.to_rgba8());
        let outputs: [(&str, ImageFormat, &DynamicImage); 5] = [
            ("png", ImageFormat::Png, &rgb),
            ("webp", ImageFormat::WebP, &rgba),
            ("tiff", ImageFormat::Tiff, &rgb),
            ("qoi", ImageFormat::Qoi, &rgba),
            ("bmp", ImageFormat::Bmp, &rgb),
        ];
        for (ext, format, data) in outputs {
            let out = out_dir.join(format!("{stem}.{ext}"));
            data.save_with_format(&out, format).unwrap_or_else(|e| panic!("{}: {e}", out.display()));
        }
        let deep = DynamicImage::ImageRgb16(img.to_rgb16());
        deep.save_with_format(out_dir.join(format!("{stem}-16bit.png")), ImageFormat::Png).expect("16-bit png");
    }
}
