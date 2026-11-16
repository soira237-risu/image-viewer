//! Times the CPU renderer on synthetic photo-sized images, and what a save does to the pixels.
//!
//! ```text
//! cargo run --release -p iv-win --example raster_bench -- [width height]
//! ```
//!
//! Each redraw is timed the way the viewer redraws: "view changed" redraws resample (the view
//! moves a little between runs), "overlay only" redraws change only what is drawn after
//! resampling (a tone slider, the selection), "new picture" is the first draw of an image.

use std::time::Instant;

use iv_core::{ImageBuf, Orientation, PixelFormat};
use iv_ops::{Adjust, Geometry, Op};
use iv_win::raster::{Frame, Page, Picture, Style, render};
use iv_win::view::{ScreenRect, ViewState};

fn time(label: &str, mut f: impl FnMut()) {
    f(); // warm up (thread pool, page faults)
    let runs = 10;
    let t = Instant::now();
    for _ in 0..runs {
        f();
    }
    println!("{label:<52} {:7.2} ms", t.elapsed().as_secs_f64() * 1000.0 / runs as f64);
}

fn image(w: u32, h: u32, alpha: bool) -> ImageBuf {
    let data: Vec<u8> = (0..h)
        .flat_map(|y| {
            (0..w).flat_map(move |x| {
                let a = if alpha && (x / 64 + y / 64) % 3 == 0 { 128 } else { 255 };
                [(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, a]
            })
        })
        .collect();
    ImageBuf::new(w, h, PixelFormat::Rgba8, data).unwrap()
}

fn main() {
    let mut args = std::env::args().skip(1).map(|a| a.parse::<u32>().expect("a number"));
    let (w, h) = (args.next().unwrap_or(3840), args.next().unwrap_or(2400));
    let pic = Picture::new(&image(w, h, false), false);
    println!("image {w}x{h}");

    for (vw, vh) in [(1280u32, 800u32), (2560, 1440), (3840, 2160)] {
        let mut frame = Frame::new(vw, vh);
        let style = Style::default();
        let mut run = 0u32;
        // `zoom`: None = fit (the window one pixel narrower every other run), else that zoom
        // (panned by a pixel every other run)
        let mut go = |ops: &[Op], zoom: Option<f64>, moves: bool| {
            run += 1;
            // the viewer keeps the resampled image only while the edit panel is open (or a
            // selection is dragged): what a slider move reuses
            frame.keep_resampled(!moves);
            let step = if moves { run % 2 } else { 0 };
            let g = Geometry::new((w, h), Orientation::Normal, ops);
            let mut view = ViewState::new(g.size, (vw - if zoom.is_none() { step } else { 0 }, vh));
            if let Some(z) = zoom {
                view.set_zoom_at(z, (vw as f64 / 2.0, vh as f64 / 2.0));
                view.pan_by(step as f64, step as f64);
            }
            let page =
                Page { picture: &pic, uv: g.display_to_stored_uv(), adjusts: &g.adjusts, dest: view.dest_rect() };
            render(&mut frame, &[page], None, &style);
        };
        time(&format!("{vw}x{vh} fit, view changed"), || go(&[], None, true));
        time(&format!("{vw}x{vh} 100%, panned"), || go(&[], Some(1.0), true));
        time(&format!("{vw}x{vh} 400% (sharp pixels), panned"), || go(&[], Some(4.0), true));
        time(&format!("{vw}x{vh} fit, rotated, view changed"), || go(&[Op::rotate_cw()], None, true));
        let adjust = |b: f32| Op::Adjust(Adjust { brightness: b, contrast: 0.2, gamma: 1.3, saturation: 0.2 });
        let mut b = 0.0;
        time(&format!("{vw}x{vh} fit, adjust slider moved"), || {
            b = 0.1 - b;
            go(&[adjust(b)], None, false)
        });
    }

    let (bw, bh) = (8000u32, 6000u32);
    let (vw, vh) = (1920u32, 1080u32);
    println!("image {bw}x{bh} in {vw}x{vh}");
    for alpha in [false, true] {
        let img = image(bw, bh, alpha);
        let kind = if alpha { "with alpha" } else { "opaque" };
        let g = Geometry::new((bw, bh), Orientation::Normal, &[]);
        let style = Style::default();
        let fit = ViewState::new(g.size, (vw, vh));
        let mut frame = Frame::new(vw, vh);
        time(&format!("{kind}: new picture, fit"), || {
            let pic = Picture::new(&img, alpha);
            let page = Page { picture: &pic, uv: g.display_to_stored_uv(), adjusts: &[], dest: fit.dest_rect() };
            render(&mut frame, &[page], None, &style);
        });
        let pic = Picture::new(&img, alpha);
        let mut draw = |view: &ViewState, selection: Option<ScreenRect>| {
            // kept while a selection is being dragged
            frame.keep_resampled(selection.is_some());
            let page = Page { picture: &pic, uv: g.display_to_stored_uv(), adjusts: &[], dest: view.dest_rect() };
            render(&mut frame, &[page], selection, &style);
        };
        let narrower = ViewState::new(g.size, (vw - 1, vh));
        let mut odd = false;
        time(&format!("{kind}: fit, view changed"), || {
            odd = !odd;
            draw(if odd { &fit } else { &narrower }, None)
        });
        let mut n = 0.0;
        time(&format!("{kind}: fit, selection dragged (overlay only)"), || {
            n += 1.0;
            draw(&fit, Some(ScreenRect { x: 100.0, y: 100.0, width: 200.0 + n, height: 150.0 + n }));
        });
        let mut zoomed = ViewState::new(g.size, (vw, vh));
        zoomed.set_zoom_at(2.0, (vw as f64 / 2.0, vh as f64 / 2.0));
        let mut dx = 1.0;
        time(&format!("{kind}: 200%, panned"), || {
            dx = -dx;
            zoomed.pan_by(dx, 0.0);
            draw(&zoomed, None);
        });
    }

    // what a save does to the pixels before the encoder (6000x4000, as from a phone camera)
    let (sw, sh) = (6000u32, 4000u32);
    println!("image {sw}x{sh}, saving");
    let photo = image(sw, sh, false);
    let adjust = Op::Adjust(Adjust { brightness: 0.1, contrast: 0.2, gamma: 1.3, saturation: 0.2 });
    let encoders = iv_codec::encode::Encoders::default();
    let jpeg = encoders.for_format(iv_core::FormatId::Jpeg).unwrap().info();
    for (label, ops, exif) in [
        ("no edits", vec![], Orientation::Normal),
        ("EXIF-rotated", vec![], Orientation::Rotate90),
        ("rotated + adjusted", vec![Op::rotate_cw(), adjust], Orientation::Normal),
    ] {
        let g = Geometry::new((sw, sh), exif, &ops);
        time(&format!("save {label}: edit + prepare"), || {
            let edited = iv_ops::apply::render(&photo, &g).unwrap();
            std::hint::black_box(iv_codec::encode::prepare(&edited, jpeg, [0, 0, 0]));
        });
    }
}
