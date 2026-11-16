//! In-place edits of raw EXIF blocks (TIFF structure) and of the EXIF segment inside a JPEG,
//! without re-encoding anything.

/// Byte order and IFD0 location of a TIFF block.
struct Tiff {
    little: bool,
    ifd0: usize,
}

fn header(raw: &[u8]) -> Option<Tiff> {
    let little = match raw.get(..4)? {
        b"II\x2A\x00" => true,
        b"MM\x00\x2A" => false,
        _ => return None,
    };
    let t = Tiff { little, ifd0: 0 };
    let ifd0 = t.u32(raw, 4)? as usize;
    Some(Tiff { ifd0, ..t })
}

impl Tiff {
    fn u16(&self, raw: &[u8], at: usize) -> Option<u16> {
        let b: [u8; 2] = raw.get(at..at + 2)?.try_into().ok()?;
        Some(if self.little { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    }
    fn u32(&self, raw: &[u8], at: usize) -> Option<u32> {
        let b: [u8; 4] = raw.get(at..at + 4)?.try_into().ok()?;
        Some(if self.little { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }
    fn put_u16(&self, raw: &mut [u8], at: usize, v: u16) {
        let b = if self.little { v.to_le_bytes() } else { v.to_be_bytes() };
        raw[at..at + 2].copy_from_slice(&b);
    }
    /// Offset of the 4-byte "next IFD" pointer that follows IFD0's entries.
    fn next_ifd_pointer(&self, raw: &[u8]) -> Option<usize> {
        let count = self.u16(raw, self.ifd0)? as usize;
        let at = self.ifd0 + 2 + count * 12;
        (at + 4 <= raw.len()).then_some(at)
    }
}

/// Sets IFD0's orientation tag. Returns `false` (and leaves `raw` alone) when the block is not
/// valid TIFF or has no orientation tag to overwrite.
pub fn set_orientation(raw: &mut [u8], value: u16) -> bool {
    let Some(t) = header(raw) else { return false };
    let Some(count) = t.u16(raw, t.ifd0) else { return false };
    for i in 0..count as usize {
        let entry = t.ifd0 + 2 + i * 12;
        let (Some(tag), Some(kind)) = (t.u16(raw, entry), t.u16(raw, entry + 2)) else { return false };
        // tag 0x0112, type SHORT: the value sits in the first two bytes of the value field
        if tag == 0x0112 && kind == 3 && entry + 10 <= raw.len() {
            t.put_u16(raw, entry + 8, value);
            return true;
        }
    }
    false
}

/// Unlinks IFD1 (the embedded thumbnail), which goes stale once the pixels are edited.
pub fn drop_thumbnail(raw: &mut [u8]) -> bool {
    let Some(t) = header(raw) else { return false };
    let Some(at) = t.next_ifd_pointer(raw) else { return false };
    raw[at..at + 4].copy_from_slice(&[0; 4]);
    true
}

/// A minimal EXIF block holding only an orientation tag.
pub fn minimal_with_orientation(value: u16) -> Vec<u8> {
    let mut t = b"II\x2A\x00".to_vec();
    t.extend_from_slice(&8u32.to_le_bytes());
    t.extend_from_slice(&1u16.to_le_bytes());
    t.extend_from_slice(&0x0112u16.to_le_bytes());
    t.extend_from_slice(&3u16.to_le_bytes());
    t.extend_from_slice(&1u32.to_le_bytes());
    t.extend_from_slice(&value.to_le_bytes());
    t.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    t
}

/// Rewrites a JPEG's EXIF orientation without touching the compressed image data: patches the
/// existing tag, or inserts an EXIF segment when the file has none. Returns `None` when the file
/// has EXIF but no orientation tag (adding a tag means rebuilding the IFD; callers re-encode).
pub fn jpeg_with_orientation(jpeg: &[u8], value: u16) -> Option<Vec<u8>> {
    if !jpeg.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut pos = 2;
    // walk the marker segments before the image data
    while pos + 4 <= jpeg.len() && jpeg[pos] == 0xFF {
        let marker = jpeg[pos + 1];
        if marker == 0xDA || marker == 0xD9 {
            break; // start of scan / end: no EXIF segment before the image data
        }
        let len = u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]) as usize;
        let body = pos + 4;
        let end = pos + 2 + len;
        if end > jpeg.len() || len < 2 {
            return None;
        }
        if marker == 0xE1 && jpeg[body..end].starts_with(b"Exif\0\0") {
            let mut out = jpeg.to_vec();
            return set_orientation(&mut out[body + 6..end], value).then_some(out);
        }
        pos = end;
    }
    // no EXIF: insert one after SOI (and after a JFIF APP0, which must come first)
    let mut insert_at = 2;
    if jpeg.len() > 6 && jpeg[2] == 0xFF && jpeg[3] == 0xE0 {
        insert_at = 4 + u16::from_be_bytes([jpeg[4], jpeg[5]]) as usize;
    }
    let tiff = minimal_with_orientation(value);
    let mut segment = vec![0xFF, 0xE1];
    segment.extend_from_slice(&((2 + 6 + tiff.len()) as u16).to_be_bytes());
    segment.extend_from_slice(b"Exif\0\0");
    segment.extend_from_slice(&tiff);
    let mut out = jpeg[..insert_at].to_vec();
    out.extend(segment);
    out.extend_from_slice(&jpeg[insert_at..]);
    Some(out)
}

/// A JPEG with everything that is not needed to decode it removed, the compressed image data
/// copied unchanged: EXIF (location, camera serial, thumbnail), XMP, ICC profile, IPTC and other
/// application segments, comments, and anything after the end-of-image marker (appended preview
/// or depth images, motion-photo video). Kept: the JFIF and Adobe segments (they say how the
/// colour components are stored) and every table, frame and scan. `None` when the file is not
/// a complete JPEG.
pub fn jpeg_without_metadata(jpeg: &[u8]) -> Option<Vec<u8>> {
    if !jpeg.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut out = vec![0xFF, 0xD8];
    let mut pos = 2;
    loop {
        if pos + 2 > jpeg.len() || jpeg[pos] != 0xFF {
            return None;
        }
        let marker = jpeg[pos + 1];
        match marker {
            // fill byte before a marker
            0xFF => {
                pos += 1;
                continue;
            }
            0xD9 => {
                out.extend_from_slice(&[0xFF, 0xD9]);
                return Some(out);
            }
            // markers without a length
            0x01 | 0xD0..=0xD7 => {
                out.extend_from_slice(&jpeg[pos..pos + 2]);
                pos += 2;
                continue;
            }
            _ => {}
        }
        if pos + 4 > jpeg.len() {
            return None;
        }
        let len = u16::from_be_bytes([jpeg[pos + 2], jpeg[pos + 3]]) as usize;
        let end = pos + 2 + len;
        if len < 2 || end > jpeg.len() {
            return None;
        }
        let body = &jpeg[pos + 4..end];
        let keep = match marker {
            0xE0 => body.starts_with(b"JFIF\0"),
            0xEE => body.starts_with(b"Adobe"),
            // other application segments and comments
            0xE1..=0xEF | 0xFE => false,
            _ => true,
        };
        if keep {
            out.extend_from_slice(&jpeg[pos..end]);
        }
        pos = end;
        if marker == 0xDA {
            // entropy-coded data runs to the next marker: a 0xFF that is not a stuffed zero, a
            // restart marker or fill
            let start = pos;
            while pos + 1 < jpeg.len() && !(jpeg[pos] == 0xFF && !matches!(jpeg[pos + 1], 0x00 | 0xD0..=0xD7 | 0xFF)) {
                pos += 1;
            }
            if pos + 1 >= jpeg.len() {
                return None;
            }
            out.extend_from_slice(&jpeg[start..pos]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read_orientation(raw: &[u8]) -> Option<u32> {
        let exif = exif::Reader::new().read_raw(raw.to_vec()).ok()?;
        exif.get_field(exif::Tag::Orientation, exif::In::PRIMARY)?.value.get_uint(0)
    }

    /// A marker segment: marker, length, body.
    fn segment(marker: u8, body: &[u8]) -> Vec<u8> {
        let mut s = vec![0xFF, marker];
        s.extend_from_slice(&((body.len() + 2) as u16).to_be_bytes());
        s.extend_from_slice(body);
        s
    }

    #[test]
    fn stripping_keeps_only_what_decoding_needs() {
        let jfif = segment(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        let adobe = segment(0xEE, b"Adobe\0\x64\0\0\0\0\x01");
        let tables = segment(0xDB, &[0; 65]);
        let frame = segment(0xC2, &[8, 0, 1, 0, 1, 1, 1, 0x11, 0]);
        let scan = segment(0xDA, &[1, 1, 0, 0, 63, 0]);
        // stuffed 0xFF, a restart marker and fill bytes inside the compressed data
        let data: &[u8] = &[0x12, 0xFF, 0x00, 0x34, 0xFF, 0xD0, 0x56, 0xFF, 0xFF];
        let mut jpeg = vec![0xFF, 0xD8];
        for part in [
            jfif.clone(),
            segment(0xE1, b"Exif\0\0GPS secret"),
            segment(0xE1, b"http://ns.adobe.com/xap/1.0/\0xmp secret"),
            segment(0xE2, b"ICC_PROFILE\0\x01\x01icc"),
            segment(0xED, b"Photoshop 3.0\0iptc secret"),
            adobe.clone(),
            tables.clone(),
            frame.clone(),
            scan.clone(),
            data.to_vec(),
            // a comment between two progressive scans
            segment(0xFE, b"comment secret"),
            scan.clone(),
            data.to_vec(),
            vec![0xFF, 0xD9],
            b"appended secret image".to_vec(),
        ] {
            jpeg.extend(part);
        }
        let want: Vec<u8> = [
            vec![0xFF, 0xD8],
            jfif,
            adobe,
            tables,
            frame,
            scan.clone(),
            data.to_vec(),
            scan,
            data.to_vec(),
            vec![0xFF, 0xD9],
        ]
        .concat();
        assert_eq!(jpeg_without_metadata(&jpeg), Some(want));
        // a cut-off file, or none at all, is left to re-encoding
        assert_eq!(jpeg_without_metadata(&jpeg[..jpeg.len() - 30]), None);
        assert_eq!(jpeg_without_metadata(b"GIF89a"), None);
    }

    #[test]
    fn patches_orientation_in_both_byte_orders() {
        let mut le = minimal_with_orientation(6);
        assert!(set_orientation(&mut le, 3));
        assert_eq!(read_orientation(&le), Some(3));

        // the same block, big-endian
        let mut be =
            b"MM\x00\x2A\x00\x00\x00\x08\x00\x01\x01\x12\x00\x03\x00\x00\x00\x01\x00\x06\x00\x00\x00\x00\x00\x00"
                .to_vec();
        assert_eq!(read_orientation(&be), Some(6));
        assert!(set_orientation(&mut be, 8));
        assert_eq!(read_orientation(&be), Some(8));
    }

    #[test]
    fn refuses_blocks_without_the_tag_or_not_tiff() {
        let mut no_tag = b"II\x2A\x00\x08\x00\x00\x00\x00\x00\x00\x00\x00\x00".to_vec();
        assert!(!set_orientation(&mut no_tag, 1));
        assert!(!set_orientation(&mut b"garbage!".to_vec(), 1));
    }

    #[test]
    fn drop_thumbnail_clears_the_next_ifd_pointer() {
        let mut raw = minimal_with_orientation(1);
        let at = 8 + 2 + 12;
        raw[at..at + 4].copy_from_slice(&26u32.to_le_bytes());
        assert!(drop_thumbnail(&mut raw));
        assert_eq!(&raw[at..at + 4], &[0, 0, 0, 0]);
    }

    #[test]
    fn jpeg_without_exif_gets_a_segment_after_jfif() {
        // SOI, APP0 (JFIF, length 16), then SOS-ish tail standing in for image data
        let mut jpeg = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10];
        jpeg.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        jpeg.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x02, 0x12, 0x34, 0xFF, 0xD9]);
        let out = jpeg_with_orientation(&jpeg, 6).unwrap();
        assert_eq!(&out[..20], &jpeg[..20], "SOI + JFIF untouched");
        assert_eq!(&out[20..22], &[0xFF, 0xE1], "EXIF segment follows JFIF");
        assert!(out.ends_with(&[0xFF, 0xDA, 0x00, 0x02, 0x12, 0x34, 0xFF, 0xD9]), "image data byte-identical");
        // and patching that file again changes the value in place
        let again = jpeg_with_orientation(&out, 3).unwrap();
        assert_eq!(again.len(), out.len());
    }
}
