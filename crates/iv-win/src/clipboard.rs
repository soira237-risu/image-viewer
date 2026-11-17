//! What the viewer puts on the clipboard, as bytes: device-independent bitmaps for the image, a
//! file drop list for the file and UTF-16 text for the path. Building them needs no window, so
//! they are tested here; `win::shell` hands them to the clipboard.

use std::path::Path;

/// Standard clipboard formats (`CF_*` in winuser.h).
pub const CF_DIB: u32 = 8;
pub const CF_UNICODETEXT: u32 = 13;
pub const CF_HDROP: u32 = 15;
pub const CF_DIBV5: u32 = 17;

/// Registered by name: PNG bytes (alpha-aware apps such as Office, browsers and chat clients
/// read it), and the drop effect Explorer uses to tell a copied file from a cut one.
pub const PNG_FORMAT: &str = "PNG";
pub const DROP_EFFECT_FORMAT: &str = "Preferred DropEffect";
/// `DROPEFFECT_COPY`: pasting the file copies it (the viewer's file is never moved).
pub const DROPEFFECT_COPY: u32 = 1;

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;
/// `LCS_sRGB`: the pixels are sRGB (the decoder converted them from any embedded profile).
const LCS_SRGB: u32 = 0x7352_4742;
const LCS_GM_IMAGES: u32 = 4;
const INFO_HEADER: usize = 40;
const V5_HEADER: usize = 124;

fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// `width * height * 4 == rgba.len()`, or a panic: a caller bug, not a user error.
fn check(rgba: &[u8], width: u32, height: u32) {
    assert_eq!(rgba.len(), width as usize * height as usize * 4, "RGBA buffer does not match {width}x{height}");
}

/// The size field of a header: 0 (allowed for these formats) when it would not fit.
fn size_field(bytes: usize) -> u32 {
    u32::try_from(bytes).unwrap_or(0)
}

/// `CF_DIB`: a `BITMAPINFOHEADER` and bottom-up 24-bit BGR rows, each padded to 4 bytes. Pixels
/// that are not opaque are blended over `background`, since programs reading this format ignore
/// alpha.
pub fn dib(rgba: &[u8], width: u32, height: u32, background: [u8; 3]) -> Vec<u8> {
    check(rgba, width, height);
    let row = width as usize * 3;
    let stride = row.div_ceil(4) * 4;
    let image = stride * height as usize;
    let mut out = Vec::with_capacity(INFO_HEADER + image);
    put_u32(&mut out, INFO_HEADER as u32);
    put_u32(&mut out, width);
    put_u32(&mut out, height); // positive: bottom-up
    put_u16(&mut out, 1); // planes
    put_u16(&mut out, 24);
    put_u32(&mut out, BI_RGB);
    put_u32(&mut out, size_field(image));
    out.extend_from_slice(&[0; 16]); // resolution, colours used / important
    let blend = |c: u8, a: u32, bg: u8| ((u32::from(c) * a + u32::from(bg) * (255 - a) + 127) / 255) as u8;
    for y in (0..height as usize).rev() {
        let src = &rgba[y * width as usize * 4..(y + 1) * width as usize * 4];
        for p in src.as_chunks::<4>().0 {
            let a = u32::from(p[3]);
            out.extend_from_slice(&[
                blend(p[2], a, background[2]),
                blend(p[1], a, background[1]),
                blend(p[0], a, background[0]),
            ]);
        }
        out.resize(out.len() + stride - row, 0);
    }
    out
}

/// `CF_DIBV5`: a `BITMAPV5HEADER` (sRGB, bit fields with an alpha mask) and bottom-up 32-bit
/// BGRA rows with straight (not premultiplied) alpha, for programs that keep transparency.
pub fn dibv5(rgba: &[u8], width: u32, height: u32) -> Vec<u8> {
    check(rgba, width, height);
    let image = width as usize * height as usize * 4;
    let mut out = Vec::with_capacity(V5_HEADER + image);
    put_u32(&mut out, V5_HEADER as u32);
    put_u32(&mut out, width);
    put_u32(&mut out, height);
    put_u16(&mut out, 1);
    put_u16(&mut out, 32);
    put_u32(&mut out, BI_BITFIELDS);
    put_u32(&mut out, size_field(image));
    out.extend_from_slice(&[0; 16]);
    for mask in [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0xff00_0000] {
        put_u32(&mut out, mask); // red, green, blue, alpha
    }
    put_u32(&mut out, LCS_SRGB);
    out.extend_from_slice(&[0; 36 + 12]); // endpoints and gamma: unused for sRGB
    put_u32(&mut out, LCS_GM_IMAGES);
    out.extend_from_slice(&[0; 12]); // profile data, profile size, reserved
    debug_assert_eq!(out.len(), V5_HEADER);
    for y in (0..height as usize).rev() {
        let src = &rgba[y * width as usize * 4..(y + 1) * width as usize * 4];
        out.extend(src.as_chunks::<4>().0.iter().flat_map(|p| [p[2], p[1], p[0], p[3]]));
    }
    out
}

/// `CF_HDROP`: a `DROPFILES` header, then each path as NUL-terminated UTF-16, then one more NUL.
pub fn file_drop(paths: &[&Path]) -> Vec<u8> {
    const HEADER: u32 = 20;
    let mut out = Vec::new();
    put_u32(&mut out, HEADER); // offset of the file list
    out.extend_from_slice(&[0; 8]); // drop point
    put_u32(&mut out, 0); // fNC
    put_u32(&mut out, 1); // fWide: UTF-16
    for p in paths {
        out.extend(p.as_os_str().to_string_lossy().encode_utf16().chain([0]).flat_map(u16::to_le_bytes));
    }
    out.extend_from_slice(&[0, 0]);
    out
}

/// `CF_UNICODETEXT`: NUL-terminated UTF-16.
pub fn text(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

/// One path for the shell's file operations, which take a list: the path NUL-terminated, then
/// the NUL that ends the list. A single NUL would run on into whatever memory follows.
pub fn path_list(path: &Path) -> Vec<u16> {
    path.as_os_str().to_string_lossy().encode_utf16().chain([0, 0]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    /// 3x2: red, green (half transparent), blue / white, black, transparent.
    fn sample() -> Vec<u8> {
        vec![255, 0, 0, 255, 0, 255, 0, 128, 0, 0, 255, 255, 255, 255, 255, 255, 0, 0, 0, 255, 9, 9, 9, 0]
    }

    #[test]
    fn dib_is_bottom_up_bgr_with_padded_rows_and_alpha_blended() {
        let d = dib(&sample(), 3, 2, [255, 255, 255]);
        assert_eq!(u32_at(&d, 0), 40);
        assert_eq!((u32_at(&d, 4), u32_at(&d, 8)), (3, 2));
        assert_eq!(&d[12..16], &[1, 0, 24, 0], "1 plane, 24 bits");
        assert_eq!(u32_at(&d, 16), BI_RGB);
        // a row of 3 pixels is 9 bytes, padded to 12
        assert_eq!(u32_at(&d, 20), 24);
        assert_eq!(d.len(), 40 + 24);
        let px = &d[40..];
        // first stored row is the bottom one: white, black, transparent over white
        assert_eq!(&px[0..9], &[255, 255, 255, 0, 0, 0, 255, 255, 255]);
        assert_eq!(&px[9..12], &[0, 0, 0], "padding");
        // top row: red, half-transparent green over white, blue (stored as BGR)
        assert_eq!(&px[12..15], &[0, 0, 255]);
        assert_eq!(&px[15..18], &[127, 255, 127]);
        assert_eq!(&px[18..21], &[255, 0, 0]);
        // another background shows through the same way
        let dark = dib(&sample(), 3, 2, [0, 0, 0]);
        assert_eq!(&dark[40 + 6..40 + 9], &[0, 0, 0]);
    }

    #[test]
    fn dibv5_keeps_straight_alpha_in_an_srgb_header() {
        let d = dibv5(&sample(), 3, 2);
        assert_eq!(u32_at(&d, 0), 124);
        assert_eq!((u32_at(&d, 4), u32_at(&d, 8)), (3, 2));
        assert_eq!(&d[12..16], &[1, 0, 32, 0]);
        assert_eq!(u32_at(&d, 16), BI_BITFIELDS);
        assert_eq!(u32_at(&d, 20), 24);
        let masks: Vec<u32> = (0..4).map(|i| u32_at(&d, 40 + i * 4)).collect();
        assert_eq!(masks, [0x00ff_0000, 0x0000_ff00, 0x0000_00ff, 0xff00_0000]);
        assert_eq!(u32_at(&d, 56), LCS_SRGB);
        assert_eq!(u32_at(&d, 108), LCS_GM_IMAGES);
        assert_eq!(d.len(), 124 + 24);
        let px = &d[124..];
        // bottom row first; the transparent pixel keeps its colour and alpha 0
        assert_eq!(&px[0..4], &[255, 255, 255, 255]);
        assert_eq!(&px[4..8], &[0, 0, 0, 255]);
        assert_eq!(&px[8..12], &[9, 9, 9, 0]);
        // top row: red stored as BGRA
        assert_eq!(&px[12..16], &[0, 0, 255, 255]);
        // top row: half-transparent green is not premultiplied
        assert_eq!(&px[12 + 4..12 + 8], &[0, 255, 0, 128]);
    }

    #[test]
    #[should_panic(expected = "does not match")]
    fn a_short_buffer_is_a_bug() {
        dib(&[0; 8], 3, 2, [0; 3]);
    }

    #[test]
    fn file_drop_lists_wide_paths_ending_in_two_nuls() {
        let d = file_drop(&[Path::new(r"C:\pics\한 장.jpg")]);
        assert_eq!(u32_at(&d, 0), 20);
        assert_eq!(u32_at(&d, 16), 1, "wide characters");
        let units: Vec<u16> = d[20..].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
        let expected: Vec<u16> = r"C:\pics\한 장.jpg".encode_utf16().chain([0, 0]).collect();
        assert_eq!(units, expected);
    }

    #[test]
    fn text_and_path_lists_are_terminated() {
        assert_eq!(text("a한"), [b'a', 0, 0x5c, 0xd5, 0, 0]);
        let list = path_list(Path::new(r"C:\a b\c.png"));
        assert_eq!(&list[list.len() - 2..], &[0, 0]);
        assert_eq!(String::from_utf16(&list[..list.len() - 2]).unwrap(), r"C:\a b\c.png");
    }
}
