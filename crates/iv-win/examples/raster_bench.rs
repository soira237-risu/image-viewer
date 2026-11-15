//! Times the CPU renderer on a synthetic photo-sized image.
//!
//! ```text
//! cargo run --release -p iv-win --example raster_bench -- [width height]
//! ```

use std::time::Instant;

use iv_core::{ImageBuf, Orientation, PixelFormat};
use iv_ops::{Adjust, Geometry, Op};
use iv_win::raster::{Frame, Page, Picture, Style, render};
use iv_win::view::ViewState;

fn time(label: &str, mut f: impl FnMut()) {
    f(); // warm up (thread pool, page faults)
    let runs = 10;
    let t = Instant::now();
    for _ in 0..runs {
        f();
    }
    println!("{label:<44} {:7.2} ms", t.elapsed().as_secs_f64() * 1000.0 / runs as f64);
}

fn main() {
    let mut args = std::env::args().skip(1).map(|a| a.parse::<u32>().expect("a number"));
    let (w, h) = (args.next().unwrap_or(3840), args.next().unwrap_or(2400));
    let data: Vec<u8> = (0..h)
        .flat_map(|y| (0..w).flat_map(move |x| [(x % 256) as u8, (y % 256) as u8, ((x + y) % 256) as u8, 255]))
        .collect();
    let pic = Picture::new(&ImageBuf::new(w, h, PixelFormat::Rgba8, data).unwrap(), false);
    println!("image {w}x{h}");

    for (vw, vh) in [(1280u32, 800u32), (2560, 1440), (3840, 2160)] {
        let mut frame = Frame::new(vw, vh);
        let style = Style::default();
        let mut go = |ops: &[Op], zoom: Option<f64>| {
            let g = Geometry::new((w, h), Orientation::Normal, ops);
            let mut view = ViewState::new(g.size, (vw, vh));
            if let Some(z) = zoom {
                view.set_zoom_at(z, (vw as f64 / 2.0, vh as f64 / 2.0));
            }
            let page =
                Page { picture: &pic, uv: g.display_to_stored_uv(), adjusts: &g.adjusts, dest: view.dest_rect() };
            render(&mut frame, &[page], None, &style);
        };
        time(&format!("{vw}x{vh} fit"), || go(&[], None));
        time(&format!("{vw}x{vh} 100%"), || go(&[], Some(1.0)));
        time(&format!("{vw}x{vh} 400% (sharp pixels)"), || go(&[], Some(4.0)));
        time(&format!("{vw}x{vh} fit, rotated"), || go(&[Op::rotate_cw()], None));
        let adjust = Op::Adjust(Adjust { brightness: 0.1, contrast: 0.2, gamma: 1.3, saturation: 0.2 });
        time(&format!("{vw}x{vh} fit, adjusted"), || go(&[adjust], None));
    }
}
