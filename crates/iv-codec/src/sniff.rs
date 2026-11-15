//! Content-based format detection. Magic bytes win over the file extension, so a WebP saved
//! as `.jpg` still opens with the right decoder and can be offered an extension fix.

use iv_core::FormatId;

/// Bytes [`sniff`] wants to see. SVG may need a long XML prolog, everything else fits in 32.
pub const SNIFF_LEN: usize = 1024;

/// Detects the format from the leading bytes, or `None` when nothing matches.
///
/// Formats without a reliable signature (TGA) are never reported here; callers fall back to
/// the extension for those.
pub fn sniff(head: &[u8]) -> Option<FormatId> {
    let starts = |sig: &[u8]| head.starts_with(sig);

    if starts(&[0xFF, 0xD8, 0xFF]) {
        return Some(FormatId::Jpeg);
    }
    if starts(b"\x89PNG\r\n\x1a\n") {
        return Some(FormatId::Png);
    }
    if starts(b"GIF87a") || starts(b"GIF89a") {
        return Some(FormatId::Gif);
    }
    if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        return Some(FormatId::Webp);
    }
    if starts(b"qoif") {
        return Some(FormatId::Qoi);
    }
    if starts(b"DDS ") {
        return Some(FormatId::Dds);
    }
    if starts(&[0x76, 0x2F, 0x31, 0x01]) {
        return Some(FormatId::OpenExr);
    }
    if starts(b"#?RADIANCE") || starts(b"#?RGBE") {
        return Some(FormatId::Hdr);
    }
    // JPEG XL: bare codestream, or the ISOBMFF-style container signature box.
    if starts(&[0xFF, 0x0A]) || starts(&[0, 0, 0, 0x0C, b'J', b'X', b'L', b' ', 0x0D, 0x0A, 0x87, 0x0A]) {
        return Some(FormatId::Jxl);
    }
    // camera RAW with their own signatures: Fujifilm RAF, Olympus ORF, Panasonic RW2
    if starts(b"FUJIFILMCCD-RAW") || starts(b"IIRO") || starts(b"IIRS") || starts(b"MMOR") || starts(b"IIU\0") {
        return Some(FormatId::Raw);
    }
    // TIFF, little- and big-endian, classic and BigTIFF. Most camera RAW files (DNG, NEF, CR2,
    // ARW …) are TIFF containers too; `Registry::identify` lets a RAW extension decide those.
    if starts(b"II*\0") || starts(b"MM\0*") || starts(b"II+\0") || starts(b"MM\0+") {
        return Some(FormatId::Tiff);
    }
    if let Some(f) = sniff_isobmff(head) {
        return Some(f);
    }
    if starts(b"BM") && head.len() >= 14 {
        return Some(FormatId::Bmp);
    }
    if is_ico(head) {
        return Some(FormatId::Ico);
    }
    if is_pnm(head) {
        return Some(FormatId::Pnm);
    }
    if is_svg(head) {
        return Some(FormatId::Svg);
    }
    None
}

/// AVIF / HEIF: an `ftyp` box whose major or compatible brands name the format.
fn sniff_isobmff(head: &[u8]) -> Option<FormatId> {
    if head.len() < 16 || &head[4..8] != b"ftyp" {
        return None;
    }
    let box_len = u32::from_be_bytes([head[0], head[1], head[2], head[3]]) as usize;
    let end = box_len.clamp(16, head.len());
    // major brand at 8..12, minor version at 12..16, compatible brands from 16
    let brands = std::iter::once(&head[8..12]).chain(head[16..end].chunks_exact(4));

    let mut heif = false;
    for brand in brands {
        match brand {
            b"avif" | b"avis" => return Some(FormatId::Avif),
            // Canon CR3
            b"crx " => return Some(FormatId::Raw),
            b"heic" | b"heix" | b"hevc" | b"hevx" | b"heim" | b"heis" | b"hevm" | b"hevs" | b"mif1" | b"msf1" => {
                heif = true
            }
            _ => {}
        }
    }
    heif.then_some(FormatId::Heif)
}

/// ICO / CUR: reserved 0, type 1 or 2, at least one image, first entry's reserved byte 0.
fn is_ico(head: &[u8]) -> bool {
    head.len() >= 22
        && head[0] == 0
        && head[1] == 0
        && (head[2] == 1 || head[2] == 2)
        && head[3] == 0
        && u16::from_le_bytes([head[4], head[5]]) > 0
        && head[9] == 0
}

/// Netpbm: `P1`..`P7` followed by whitespace.
fn is_pnm(head: &[u8]) -> bool {
    head.len() >= 3 && head[0] == b'P' && (b'1'..=b'7').contains(&head[1]) && head[2].is_ascii_whitespace()
}

/// SVG: XML text whose root element is `<svg`. Gzipped SVGZ cannot be told apart from any other
/// gzip stream here; the extension decides that case.
fn is_svg(head: &[u8]) -> bool {
    let text = head.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(head);
    let text = String::from_utf8_lossy(text);
    let trimmed = text.trim_start();
    if !trimmed.starts_with('<') {
        return false;
    }
    if trimmed.starts_with("<svg") {
        return true;
    }
    (trimmed.starts_with("<?xml") || trimmed.starts_with("<!--") || trimmed.starts_with("<!DOCTYPE"))
        && trimmed.contains("<svg")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ftyp(major: &[u8; 4], compat: &[&[u8; 4]]) -> Vec<u8> {
        let len = 16 + 4 * compat.len();
        let mut v = (len as u32).to_be_bytes().to_vec();
        v.extend_from_slice(b"ftyp");
        v.extend_from_slice(major);
        v.extend_from_slice(&[0, 0, 0, 0]);
        for c in compat {
            v.extend_from_slice(*c);
        }
        v
    }

    #[test]
    fn detects_signatures() {
        assert_eq!(sniff(&[0xFF, 0xD8, 0xFF, 0xE0]), Some(FormatId::Jpeg));
        assert_eq!(sniff(b"\x89PNG\r\n\x1a\n...."), Some(FormatId::Png));
        assert_eq!(sniff(b"GIF89a......"), Some(FormatId::Gif));
        assert_eq!(sniff(b"RIFF\0\0\0\0WEBPVP8 "), Some(FormatId::Webp));
        assert_eq!(sniff(b"RIFF\0\0\0\0WAVEfmt "), None);
        assert_eq!(sniff(b"qoif...."), Some(FormatId::Qoi));
        assert_eq!(sniff(b"II*\0...."), Some(FormatId::Tiff));
        assert_eq!(sniff(b"MM\0*...."), Some(FormatId::Tiff));
        assert_eq!(sniff(b"#?RADIANCE\n"), Some(FormatId::Hdr));
        assert_eq!(sniff(&[0xFF, 0x0A, 0x00]), Some(FormatId::Jxl));
        assert_eq!(sniff(b"P6\n640 480\n255\n"), Some(FormatId::Pnm));
        assert_eq!(sniff(b"P9\n"), None);
    }

    #[test]
    fn isobmff_brands() {
        assert_eq!(sniff(&ftyp(b"avif", &[b"mif1", b"miaf"])), Some(FormatId::Avif));
        // AVIF listed only as a compatible brand still wins over the generic mif1
        assert_eq!(sniff(&ftyp(b"mif1", &[b"avif"])), Some(FormatId::Avif));
        assert_eq!(sniff(&ftyp(b"heic", &[b"mif1"])), Some(FormatId::Heif));
        assert_eq!(sniff(&ftyp(b"mif1", &[b"heic"])), Some(FormatId::Heif));
        assert_eq!(sniff(&ftyp(b"isom", &[b"mp41"])), None, "an MP4 video is not an image");
        assert_eq!(sniff(&ftyp(b"crx ", &[b"isom"])), Some(FormatId::Raw), "Canon CR3");
    }

    #[test]
    fn raw_signatures() {
        assert_eq!(sniff(b"FUJIFILMCCD-RAW 0201"), Some(FormatId::Raw));
        assert_eq!(sniff(b"IIRO\x08\0\0\0"), Some(FormatId::Raw));
        assert_eq!(sniff(b"IIU\0\x08\0\0\0"), Some(FormatId::Raw));
    }

    #[test]
    fn bmp_and_ico() {
        let mut bmp = b"BM".to_vec();
        bmp.resize(54, 0);
        assert_eq!(sniff(&bmp), Some(FormatId::Bmp));

        let mut ico = vec![0, 0, 1, 0, 1, 0, 16, 16, 0, 0];
        ico.resize(22, 0);
        assert_eq!(sniff(&ico), Some(FormatId::Ico));
        assert_eq!(sniff(&[0u8; 22]), None, "all zeros is not an icon");
    }

    #[test]
    fn svg_text() {
        assert_eq!(sniff(b"<svg xmlns='http://www.w3.org/2000/svg'/>"), Some(FormatId::Svg));
        assert_eq!(sniff(b"\xEF\xBB\xBF  <?xml version='1.0'?>\n<svg/>"), Some(FormatId::Svg));
        assert_eq!(sniff(b"<?xml version='1.0'?><html/>"), None);
        assert_eq!(sniff(b"hello"), None);
    }
}
