//! The application icon (`res/app.ico`) and the resources `build.rs` makes from it.
//!
//! The icon is drawn here, by code: a blue rounded tile with a sun and two white mountains on dark ground,
//! rendered at every size Windows asks for (anti-aliased by 8×8 supersampling, so small sizes
//! stay clean). The committed `.ico` must be exactly this drawing; after changing it, write the
//! file again with
//!
//! ```text
//! IV_WRITE_ICON=1 cargo test -p iv-win app_icon
//! ```

#[path = "../res/winres.rs"]
mod winres;

use std::io::Cursor;

/// Sizes in the `.ico`: the small and large icon at every common scale, and 256 for Explorer's
/// big views.
const SIZES: [u32; 8] = [16, 20, 24, 32, 40, 48, 64, 256];
const ICO: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/res/app.ico");

/// Straight-alpha colour of the drawing at `(x, y)` in icon units (0..1 across the icon).
fn shade(x: f64, y: f64) -> [f64; 4] {
    // the tile: a rounded square with a little margin, as Windows icons have
    let (margin, radius) = (1.0 / 16.0, 0.19);
    let (cx, cy, half) = (0.5, 0.5, 0.5 - margin);
    let qx = (x - cx).abs() - (half - radius);
    let qy = (y - cy).abs() - (half - radius);
    let outside = qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - radius;
    if outside > 0.0 {
        return [0.0; 4];
    }
    // tile-local coordinates
    let (u, v) = ((x - margin) / (1.0 - 2.0 * margin), (y - margin) / (1.0 - 2.0 * margin));
    let mix = |a: [f64; 3], b: [f64; 3], t: f64| [0, 1, 2].map(|i| a[i] + (b[i] - a[i]) * t);
    // sky: light blue at the top left to deep blue at the bottom right
    let mut c = mix([0.36, 0.67, 1.0], [0.13, 0.33, 0.85], ((u + v) / 2.0).clamp(0.0, 1.0));
    let in_triangle = |p: [(f64, f64); 3]| {
        let edge = |(ax, ay): (f64, f64), (bx, by): (f64, f64)| (bx - ax) * (v - ay) - (by - ay) * (u - ax);
        let (d0, d1, d2) = (edge(p[0], p[1]), edge(p[1], p[2]), edge(p[2], p[0]));
        (d0 >= 0.0 && d1 >= 0.0 && d2 >= 0.0) || (d0 <= 0.0 && d1 <= 0.0 && d2 <= 0.0)
    };
    if (u - 0.70).hypot(v - 0.30) <= 0.115 {
        c = [1.0, 0.84, 0.29]; // sun
    }
    if in_triangle([(0.40, 1.05), (0.70, 0.50), (1.05, 1.05)]) {
        c = mix(c, [1.0; 3], 0.62); // far mountain, in haze
    }
    if in_triangle([(-0.05, 1.05), (0.33, 0.40), (0.76, 1.05)]) {
        c = [1.0; 3]; // near mountain
    }
    if v > 0.80 {
        // the ground the mountains stand on keeps the tile's outline whole on light backgrounds
        c = mix([0.11, 0.29, 0.76], [0.07, 0.20, 0.62], u.clamp(0.0, 1.0));
    }
    [c[0], c[1], c[2], 1.0]
}

/// The icon at `size`×`size`, straight-alpha RGBA.
fn render(size: u32) -> Vec<u8> {
    const SS: u32 = 8;
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut acc = [0.0f64; 4]; // premultiplied
            for sy in 0..SS {
                for sx in 0..SS {
                    let x = (f64::from(px * SS + sx) + 0.5) / f64::from(size * SS);
                    let y = (f64::from(py * SS + sy) + 0.5) / f64::from(size * SS);
                    let [r, g, b, a] = shade(x, y);
                    acc = [acc[0] + r * a, acc[1] + g * a, acc[2] + b * a, acc[3] + a];
                }
            }
            let n = f64::from(SS * SS);
            let a = acc[3] / n;
            let straight = |c: f64| if a > 0.0 { c / n / a } else { 0.0 };
            let byte = |v: f64| (v.clamp(0.0, 1.0) * 255.0).round() as u8;
            out.extend_from_slice(&[byte(straight(acc[0])), byte(straight(acc[1])), byte(straight(acc[2])), byte(a)]);
        }
    }
    out
}

fn encode_png(size: u32, rgba: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, size, size);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.set_compression(png::Compression::High);
    let mut w = enc.write_header().expect("PNG header");
    w.write_image_data(rgba).expect("PNG data");
    w.finish().expect("PNG end");
    out
}

fn decode_png(data: &[u8]) -> (u32, u32, Vec<u8>) {
    let mut reader = png::Decoder::new(Cursor::new(data)).read_info().expect("PNG in the icon");
    let mut buf = vec![0; reader.output_buffer_size().expect("size")];
    let info = reader.next_frame(&mut buf).expect("PNG pixels");
    assert_eq!((info.color_type, info.bit_depth), (png::ColorType::Rgba, png::BitDepth::Eight));
    buf.truncate(info.buffer_size());
    (info.width, info.height, buf)
}

/// The `.ico` file for the drawing: one PNG image per size (Windows reads PNG icon images at
/// every size, and they are a fraction of the size of bitmaps).
fn ico_file() -> Vec<u8> {
    let images: Vec<winres::IcoImage> = SIZES
        .iter()
        .map(|&s| winres::IcoImage {
            width: (s % 256) as u8,
            height: (s % 256) as u8,
            colors: 0,
            planes: 1,
            bit_count: 32,
            data: encode_png(s, &render(s)),
        })
        .collect();
    winres::ico_file(&images)
}

fn u16_at(b: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([b[at], b[at + 1]]))
}

fn u32_at(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize
}

#[test]
fn committed_icon_is_the_drawing() {
    if std::env::var_os("IV_WRITE_ICON").is_some() {
        std::fs::write(ICO, ico_file()).expect("write res/app.ico");
    }
    let ico = std::fs::read(ICO).expect("res/app.ico");
    let images = winres::parse_ico(&ico).unwrap();
    let sizes: Vec<u32> = images.iter().map(|i| if i.width == 0 { 256 } else { u32::from(i.width) }).collect();
    assert_eq!(sizes, SIZES);
    for (img, size) in images.iter().zip(SIZES) {
        assert_eq!((img.width, img.height, img.planes, img.bit_count), ((size % 256) as u8, (size % 256) as u8, 1, 32));
        let (w, h, pixels) = decode_png(&img.data);
        assert_eq!((w, h), (size, size));
        assert!(pixels == render(size), "res/app.ico at {size}px differs from the drawing; see the module docs");
    }
    // small enough to embed in every build
    assert!(ico.len() < 40 * 1024, "{} bytes", ico.len());
}

#[test]
fn the_drawing_is_a_tile_with_transparent_corners() {
    let px = |img: &[u8], size: u32, x: u32, y: u32| -> [u8; 4] {
        let i = ((y * size + x) * 4) as usize;
        img[i..i + 4].try_into().unwrap()
    };
    let s = 64;
    let img = render(s);
    assert_eq!(px(&img, s, 0, 0)[3], 0, "corner outside the tile");
    assert_eq!(px(&img, s, 3, 3)[3], 0, "rounded corner");
    let sky = px(&img, s, 12, 12);
    assert_eq!(sky[3], 255);
    assert!(sky[2] > 200 && sky[0] < 140, "blue sky: {sky:?}");
    assert_eq!(px(&img, s, 20, 43), [255, 255, 255, 255], "near mountain");
    let ground = px(&img, s, 32, 56);
    assert!(ground[3] == 255 && ground[2] > 150 && ground[0] < 40, "ground: {ground:?}");
    let sun = px(&img, s, 44, 21);
    assert!(sun[0] > 240 && sun[1] > 190 && sun[2] < 120, "yellow sun: {sun:?}");
}

/// The resource file `build.rs` writes: an empty first entry, one icon resource per image, the
/// icon group pointing at them, and version information whose blocks are consistent.
#[test]
fn resource_file_holds_the_icons_and_a_consistent_version() {
    let ico = std::fs::read(ICO).expect("res/app.ico");
    let version = winres::version_info([1, 2, 3, 0], &[("FileDescription", "image-viewer"), ("FileVersion", "1.2.3")]);
    let res = winres::resource_file(&ico, &version).unwrap();
    // walk the entries: (type, id, data)
    let mut entries = Vec::new();
    let mut at = 0;
    while at < res.len() {
        let (size, header) = (u32_at(&res, at), u32_at(&res, at + 4));
        assert_eq!(header, 32);
        assert_eq!((u16_at(&res, at + 8), u16_at(&res, at + 12)), (0xffff, 0xffff), "numeric type and id");
        let (kind, id) = (u16_at(&res, at + 10) as u16, u16_at(&res, at + 14) as u16);
        entries.push((kind, id, res[at + 32..at + 32 + size].to_vec()));
        at = (at + 32 + size).div_ceil(4) * 4;
    }
    assert_eq!(at, res.len());
    assert_eq!((entries[0].0, entries[0].1, entries[0].2.len()), (0, 0, 0), "the empty first resource");
    let images = winres::parse_ico(&ico).unwrap();
    for (i, img) in images.iter().enumerate() {
        assert_eq!(entries[1 + i], (winres::RT_ICON, 1 + i as u16, img.data.clone()));
    }
    let (kind, id, group) = &entries[1 + images.len()];
    assert_eq!((*kind, *id), (winres::RT_GROUP_ICON, winres::ICON_GROUP_ID));
    assert_eq!(u16_at(group, 4), images.len());
    for (i, img) in images.iter().enumerate() {
        let e = 6 + i * 14;
        assert_eq!(group[e], img.width);
        assert_eq!(u32_at(group, e + 8), img.data.len());
        assert_eq!(u16_at(group, e + 12), 1 + i, "points at icon resource {}", 1 + i);
    }
    let (kind, _, info) = entries.last().unwrap();
    assert_eq!(*kind, winres::RT_VERSION);
    assert_eq!(info, &version);

    // every block's length covers exactly its key, value and children
    fn walk(b: &[u8], depth: usize, keys: &mut Vec<(usize, String)>) {
        let len = u16_at(b, 0);
        assert!(len <= b.len());
        let key: Vec<u16> = (6..).step_by(2).map(|i| u16_at(b, i) as u16).take_while(|&c| c != 0).collect();
        let key = String::from_utf16(&key).unwrap();
        let (value_len, kind) = (u16_at(b, 2), u16_at(b, 4));
        let value_bytes = if kind == 1 { value_len * 2 } else { value_len };
        let mut at = (6 + (key.encode_utf16().count() + 1) * 2).div_ceil(4) * 4 + value_bytes;
        keys.push((depth, key));
        while at.div_ceil(4) * 4 < len {
            at = at.div_ceil(4) * 4;
            let child = u16_at(b, at);
            walk(&b[at..at + child], depth + 1, keys);
            at += child;
        }
        assert_eq!(at, len, "block {:?} ends where its length says", keys.last());
    }
    let mut keys = Vec::new();
    walk(&version, 0, &mut keys);
    let names: Vec<&str> = keys.iter().map(|(_, k)| k.as_str()).collect();
    assert_eq!(
        names,
        [
            "VS_VERSION_INFO",
            "StringFileInfo",
            "040904B0",
            "FileDescription",
            "FileVersion",
            "VarFileInfo",
            "Translation"
        ]
    );
    // the fixed part: signature, then version 1.2.3.0 as file and product version
    let fixed = (6 + 16 * 2usize).div_ceil(4) * 4;
    assert_eq!(u32_at(&version, fixed), 0xfeef_04bd);
    assert_eq!((u32_at(&version, fixed + 8), u32_at(&version, fixed + 12)), (0x0001_0002, 0x0003_0000));
    assert_eq!((u32_at(&version, fixed + 16), u32_at(&version, fixed + 20)), (0x0001_0002, 0x0003_0000));
}
