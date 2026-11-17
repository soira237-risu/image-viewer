//! HEIF item properties of an AVIF's primary image that `avif-parse` does not expose: rotation
//! (`irot`), mirroring (`imir`) and an ICC profile (`colr`), read straight from the `meta` box.
//!
//! Every read is bounds-checked and nothing here fails: a property box that cannot be read is
//! skipped, like a broken EXIF orientation tag (`avif-parse` has already accepted the file).

use std::sync::Arc;

use iv_core::Orientation;

/// What the container says about how to show the primary image.
///
/// Read: `irot` and `imir`, composed in their `ipma` order, as HEIF (ISO/IEC 23008-12) applies
/// transformative properties (MIAF fixes that order to clean aperture, rotation, mirror); and
/// the first `colr` that holds an ICC profile (`prof`, or the restricted `rICC`).
///
/// Not read: the clean aperture (`clap`) crop, so the whole coded image is shown. An `nclx`
/// `colr` carries no profile; `avif-decode` takes the YUV matrix and range from the AV1 sequence
/// header instead (BT.601 when unspecified there), and its primaries and transfer are not
/// colour-managed: such an image is shown as if it were sRGB.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct ItemProperties {
    pub orientation: Orientation,
    pub icc: Option<Arc<[u8]>>,
}

impl ItemProperties {
    /// The properties of the item `pitm` names, in the AVIF/HEIF file `file`.
    pub fn of_primary_item(file: &[u8]) -> Self {
        let mut props = ItemProperties::default();
        let Some((_, _, meta)) = boxes(file).find(|(kind, _)| kind == b"meta").and_then(|(_, p)| full_box(p)) else {
            return props;
        };
        let (mut primary, mut ipco, mut ipma) = (None, Vec::new(), Vec::new());
        for (kind, payload) in boxes(meta) {
            match &kind {
                b"pitm" => {
                    primary = full_box(payload).and_then(|(version, _, body)| item_id(&mut Reader(body), version))
                }
                b"iprp" => {
                    for (kind, payload) in boxes(payload) {
                        match &kind {
                            b"ipco" => ipco = boxes(payload).collect(),
                            b"ipma" => ipma.push(payload),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        let Some(primary) = primary else { return props };
        let mut indices = Vec::new();
        for ipma in ipma {
            // the associations read before a truncation still count
            let _ = associations(ipma, primary, &mut indices);
        }
        // indices are 1-based; 0 means "no property"
        for (kind, payload) in indices.into_iter().filter_map(|i| ipco.get(usize::from(i).checked_sub(1)?)) {
            match (kind, payload.first()) {
                (b"irot", Some(&angle)) => props.orientation = props.orientation.then(rotation(angle)),
                (b"imir", Some(&axis)) => props.orientation = props.orientation.then(mirror(axis)),
                (b"colr", _) if props.icc.is_none() => props.icc = icc(payload),
                _ => {}
            }
        }
        props
    }
}

/// `irot`: the low two bits are the anticlockwise rotation in quarter turns ([`Orientation`]
/// names clockwise turns).
fn rotation(angle: u8) -> Orientation {
    match angle & 3 {
        1 => Orientation::Rotate270,
        2 => Orientation::Rotate180,
        3 => Orientation::Rotate90,
        _ => Orientation::Normal,
    }
}

/// `imir` (ISO/IEC 23008-12:2022 6.5.12): axis 0 exchanges the top and bottom (EXIF 4), axis 1
/// the left and right (EXIF 2).
fn mirror(axis: u8) -> Orientation {
    if axis & 1 == 0 { Orientation::FlipV } else { Orientation::FlipH }
}

/// The profile of a `colr` box of type `prof` or `rICC`; `None` for `nclx` and anything else.
fn icc(colr: &[u8]) -> Option<Arc<[u8]>> {
    let (kind, profile) = colr.split_first_chunk::<4>()?;
    (matches!(kind, b"prof" | b"rICC") && !profile.is_empty()).then(|| Arc::from(profile))
}

/// Appends the property indices `ipma` associates with `item`, in order.
fn associations(ipma: &[u8], item: u32, out: &mut Vec<u16>) -> Option<()> {
    let (version, flags, body) = full_box(ipma)?;
    let mut r = Reader(body);
    for _ in 0..r.u32()? {
        let id = item_id(&mut r, version)?;
        for _ in 0..r.u8()? {
            // the top bit marks the property essential
            let index = if flags & 1 != 0 { r.u16()? & 0x7fff } else { u16::from(r.u8()? & 0x7f) };
            if id == item {
                out.push(index);
            }
        }
    }
    Some(())
}

/// A 16-bit item id in version 0 boxes, 32-bit in later ones.
fn item_id(r: &mut Reader, version: u8) -> Option<u32> {
    if version == 0 { r.u16().map(u32::from) } else { r.u32() }
}

/// Bounds-checked big-endian reads.
struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn bytes<const N: usize>(&mut self) -> Option<[u8; N]> {
        let (head, rest) = self.0.split_first_chunk::<N>()?;
        self.0 = rest;
        Some(*head)
    }

    fn u8(&mut self) -> Option<u8> {
        self.bytes::<1>().map(|[b]| b)
    }

    fn u16(&mut self) -> Option<u16> {
        self.bytes().map(u16::from_be_bytes)
    }

    fn u32(&mut self) -> Option<u32> {
        self.bytes().map(u32::from_be_bytes)
    }

    fn u64(&mut self) -> Option<u64> {
        self.bytes().map(u64::from_be_bytes)
    }
}

/// One box at the start of `data`: its type, its payload and what follows it.
fn next_box(data: &[u8]) -> Option<([u8; 4], &[u8], &[u8])> {
    let mut r = Reader(data);
    let size = r.u32()?;
    let kind = r.bytes::<4>()?;
    let size = match size {
        // to the end of the enclosing data
        0 => data.len() as u64,
        1 => r.u64()?,
        n => u64::from(n),
    };
    let header = data.len() - r.0.len();
    let end = usize::try_from(size).ok()?;
    Some((kind, data.get(header..end)?, data.get(end..)?))
}

/// The ISOBMFF boxes laid end to end in `data`, as (type, payload), up to the first that does
/// not fit.
fn boxes(data: &[u8]) -> impl Iterator<Item = ([u8; 4], &[u8])> {
    let mut rest = Some(data);
    std::iter::from_fn(move || {
        let (kind, payload, after) = next_box(rest?).or_else(|| {
            rest = None;
            None
        })?;
        rest = Some(after);
        Some((kind, payload))
    })
}

/// A full box's version, flags and body.
fn full_box(payload: &[u8]) -> Option<(u8, u32, &[u8])> {
    let (&[version, f0, f1, f2], body) = payload.split_first_chunk::<4>()?;
    Some((version, u32::from_be_bytes([0, f0, f1, f2]), body))
}

#[cfg(test)]
mod tests {
    use super::*;
    use Orientation::*;

    fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut b = ((8 + payload.len()) as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(payload);
        b
    }

    fn full(kind: &[u8; 4], version: u8, flags: u32, body: &[u8]) -> Vec<u8> {
        let mut p = vec![version];
        p.extend_from_slice(&flags.to_be_bytes()[1..]);
        p.extend_from_slice(body);
        bx(kind, &p)
    }

    /// Version 0 `ipma` with 7-bit indices; entries are (item, [index with essential bit]).
    fn ipma(entries: &[(u16, &[u8])]) -> Vec<u8> {
        let mut body = (entries.len() as u32).to_be_bytes().to_vec();
        for (item, indices) in entries {
            body.extend_from_slice(&item.to_be_bytes());
            body.push(indices.len() as u8);
            body.extend_from_slice(indices);
        }
        full(b"ipma", 0, 0, &body)
    }

    fn file(primary: u16, props: &[Vec<u8>], ipma: Vec<u8>) -> Vec<u8> {
        let iprp = bx(b"iprp", &[bx(b"ipco", &props.concat()), ipma].concat());
        let meta = full(b"meta", 0, 0, &[full(b"pitm", 0, 0, &primary.to_be_bytes()), iprp].concat());
        [bx(b"ftyp", b"avifmif1"), meta, bx(b"mdat", &[1, 2, 3])].concat()
    }

    fn irot(angle: u8) -> Vec<u8> {
        bx(b"irot", &[angle])
    }

    fn imir(axis: u8) -> Vec<u8> {
        bx(b"imir", &[axis])
    }

    fn orientation(file: &[u8]) -> Orientation {
        ItemProperties::of_primary_item(file).orientation
    }

    #[test]
    fn rotation_is_anticlockwise_and_mirror_axis_0_swaps_top_and_bottom() {
        let ispe = bx(b"ispe", &[0; 12]);
        for (angle, want) in [(0, Normal), (1, Rotate270), (2, Rotate180), (3, Rotate90)] {
            let f = file(1, &[ispe.clone(), irot(angle)], ipma(&[(1, &[1, 0x82])]));
            assert_eq!(orientation(&f), want, "irot {angle}");
        }
        assert_eq!(orientation(&file(1, &[imir(0)], ipma(&[(1, &[0x81])]))), FlipV);
        assert_eq!(orientation(&file(1, &[imir(1)], ipma(&[(1, &[0x81])]))), FlipH);
        // reserved bits are ignored
        assert_eq!(orientation(&file(1, &[irot(0xfd)], ipma(&[(1, &[0x81])]))), Rotate270);
        assert_eq!(orientation(&file(1, &[imir(0xfe)], ipma(&[(1, &[0x81])]))), FlipV);
    }

    /// libavif's mapping to EXIF (`avifImageGetExifOrientationFromIrotImir`): rotation first.
    #[test]
    fn rotation_then_mirror_matches_libavif() {
        let expect = [
            ((1, 0), 5), // 90° anticlockwise, then top and bottom swapped
            ((1, 1), 7), // 90° anticlockwise, then left and right swapped
            ((2, 0), 2),
            ((2, 1), 4),
            ((3, 0), 7),
            ((3, 1), 5),
            ((0, 0), 4),
            ((0, 1), 2),
        ];
        for ((angle, axis), exif) in expect {
            let f = file(1, &[irot(angle), imir(axis)], ipma(&[(1, &[0x81, 0x82])]));
            assert_eq!(orientation(&f).to_exif(), exif, "irot {angle} imir {axis}");
        }
    }

    #[test]
    fn transforms_apply_in_association_order() {
        let props = [irot(1), imir(1)];
        let rotate_first = orientation(&file(1, &props, ipma(&[(1, &[0x81, 0x82])])));
        let mirror_first = orientation(&file(1, &props, ipma(&[(1, &[0x82, 0x81])])));
        assert_eq!(rotate_first, Rotate270.then(FlipH));
        assert_eq!(mirror_first, FlipH.then(Rotate270));
        assert_ne!(rotate_first, mirror_first);
        // two rotations add up
        assert_eq!(orientation(&file(1, &[irot(1), irot(1)], ipma(&[(1, &[0x81, 0x82])]))), Rotate180);
    }

    #[test]
    fn only_the_primary_items_properties_count() {
        let f = file(2, &[irot(1), imir(0)], ipma(&[(1, &[0x81]), (2, &[0x82])]));
        assert_eq!(orientation(&f), FlipV);
        // index 0 is "no property", out-of-range indices are skipped
        assert_eq!(orientation(&file(1, &[irot(3)], ipma(&[(1, &[0x80, 0x85, 0x81])]))), Rotate90);
    }

    #[test]
    fn wide_ids_and_indices_and_large_sizes_are_read() {
        // ipma version 1 (32-bit ids) with flag 1 (15-bit indices); pitm version 1
        let mut body = 1u32.to_be_bytes().to_vec();
        body.extend_from_slice(&70000u32.to_be_bytes());
        body.push(1);
        body.extend_from_slice(&0x8002u16.to_be_bytes());
        let ipma = full(b"ipma", 1, 1, &body);
        let iprp = bx(b"iprp", &[bx(b"ipco", &[bx(b"free", &[]), irot(2)].concat()), ipma].concat());
        let pitm = full(b"pitm", 1, 0, &70000u32.to_be_bytes());
        let meta_payload = [vec![0, 0, 0, 0], pitm, iprp].concat();
        // the meta box with a 64-bit size, then an mdat running to the end of the file (size 0)
        let mut meta = 1u32.to_be_bytes().to_vec();
        meta.extend_from_slice(b"meta");
        meta.extend_from_slice(&(16 + meta_payload.len() as u64).to_be_bytes());
        meta.extend_from_slice(&meta_payload);
        let f = [bx(b"ftyp", b"avif"), meta, 0u32.to_be_bytes().to_vec(), b"mdat".to_vec(), vec![9; 5]].concat();
        assert_eq!(orientation(&f), Rotate180);
    }

    #[test]
    fn icc_comes_from_prof_or_ricc_not_nclx() {
        let nclx = bx(b"colr", &[b"nclx".as_slice(), &[0, 1, 0, 13, 0, 6, 0x80]].concat());
        let prof = bx(b"colr", &[b"prof".as_slice(), b"PROFILE"].concat());
        let f = file(1, &[nclx.clone(), prof], ipma(&[(1, &[1, 2])]));
        assert_eq!(ItemProperties::of_primary_item(&f).icc.as_deref(), Some(b"PROFILE".as_slice()));
        let ricc = bx(b"colr", &[b"rICC".as_slice(), b"R"].concat());
        assert_eq!(
            ItemProperties::of_primary_item(&file(1, &[ricc], ipma(&[(1, &[1])]))).icc.as_deref(),
            Some(&b"R"[..])
        );
        assert_eq!(ItemProperties::of_primary_item(&file(1, &[nclx], ipma(&[(1, &[1])]))).icc, None);
        let empty = bx(b"colr", b"prof");
        assert_eq!(ItemProperties::of_primary_item(&file(1, &[empty], ipma(&[(1, &[1])]))).icc, None);
    }

    /// Every prefix of a file, and every single-byte corruption of its `meta`, reads without a
    /// panic; damage only ever loses properties.
    #[test]
    fn damaged_files_never_panic() {
        let colr = bx(b"colr", &[b"prof".as_slice(), &[7; 40]].concat());
        let f = file(1, &[irot(1), imir(1), colr], ipma(&[(1, &[0x81, 0x82, 3])]));
        let whole = ItemProperties::of_primary_item(&f);
        assert_eq!(whole.orientation, Transverse);
        assert!(whole.icc.is_some());
        for end in 0..f.len() {
            let _ = ItemProperties::of_primary_item(&f[..end]);
        }
        for i in 0..f.len() {
            for v in [0x00, 0x01, 0x7f, 0x80, 0xff] {
                let mut g = f.clone();
                g[i] = v;
                let _ = ItemProperties::of_primary_item(&g);
            }
        }
        assert_eq!(ItemProperties::of_primary_item(&[]), ItemProperties::default());
        assert_eq!(ItemProperties::of_primary_item(b"no boxes here at all"), ItemProperties::default());
    }
}
