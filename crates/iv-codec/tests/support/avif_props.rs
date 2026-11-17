//! Adds HEIF item properties (`irot`, `imir`, `colr`) to an AVIF, as phone cameras and libavif
//! write them. The AVIF encoder here (`ravif`/`avif-serialize`) writes none, so the tests take
//! its output and rebuild the `meta` box: the new property boxes go at the end of `ipco`, their
//! indices after the primary item's existing ones in `ipma`, and every `iloc` offset past the
//! grown `meta` moves along.
//!
//! Shared by the `iv-codec` and `iv-pipeline` tests (`#[path]`), so not every item is used by both.
#![allow(dead_code)]

pub fn bx(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut b = u32::try_from(8 + payload.len()).unwrap().to_be_bytes().to_vec();
    b.extend_from_slice(kind);
    b.extend_from_slice(payload);
    b
}

/// Rotation by `angle` quarter turns anticlockwise.
pub fn irot(angle: u8) -> Vec<u8> {
    bx(b"irot", &[angle])
}

/// Mirror: axis 0 exchanges top and bottom, 1 left and right.
pub fn imir(axis: u8) -> Vec<u8> {
    bx(b"imir", &[axis])
}

pub fn colr_icc(icc: &[u8]) -> Vec<u8> {
    bx(b"colr", &[b"prof".as_slice(), icc].concat())
}

/// `colr` with code points (`nclx`): BT.709 primaries, sRGB transfer, BT.601 matrix, full range,
/// which is what the encoder puts in the AV1 sequence header. libavif writes one of these beside
/// every ICC profile.
pub fn colr_nclx() -> Vec<u8> {
    bx(b"colr", &[b"nclx".as_slice(), &[0, 1, 0, 13, 0, 6, 0x80]].concat())
}

/// Top-level or child boxes of `data` as (type, whole box), 32-bit sizes only.
fn children(data: &[u8]) -> Vec<([u8; 4], &[u8])> {
    let mut out = Vec::new();
    let mut rest = data;
    while !rest.is_empty() {
        let size = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
        assert!(size >= 8 && size <= rest.len(), "test helper handles only 32-bit box sizes");
        out.push((rest[4..8].try_into().unwrap(), &rest[..size]));
        rest = &rest[size..];
    }
    out
}

fn uint(b: &[u8]) -> u64 {
    b.iter().fold(0, |v, &x| v << 8 | x as u64)
}

fn put_uint(b: &mut [u8], v: u64) {
    let n = b.len();
    for (i, x) in b.iter_mut().enumerate() {
        *x = (v >> (8 * (n - 1 - i))) as u8;
    }
    assert_eq!(uint(b), v, "offset fits its field");
}

/// `ipma` rewritten with `extra` (indices, essential bit included) after `item`'s associations;
/// always written with 16-bit indices (flag 1), so any index fits.
fn extended_ipma(ipma: &[u8], item: u32, extra: &[u16]) -> Vec<u8> {
    let (version, flags) = (ipma[8], uint(&ipma[9..12]));
    let mut r = &ipma[12..];
    let mut take = |n: usize| {
        let (a, b) = r.split_at(n);
        r = b;
        uint(a)
    };
    let id_len = if version == 0 { 2 } else { 4 };
    let index_len = if flags & 1 != 0 { 2 } else { 1 };
    let count = take(4);
    let mut body = (count as u32).to_be_bytes().to_vec();
    let mut found = false;
    for _ in 0..count {
        let id = take(id_len);
        let mut indices: Vec<u16> = (0..take(1))
            .map(|_| {
                let v = take(index_len) as u16;
                // a 7-bit index with its essential bit, widened to the 15-bit layout
                if index_len == 1 { (v & 0x80) << 8 | (v & 0x7f) } else { v }
            })
            .collect();
        if id == item as u64 {
            indices.extend_from_slice(extra);
            found = true;
        }
        body.extend_from_slice(&id.to_be_bytes()[8 - id_len..]);
        body.push(u8::try_from(indices.len()).unwrap());
        indices.iter().for_each(|i| body.extend_from_slice(&i.to_be_bytes()));
    }
    assert!(found, "the primary item has an ipma entry");
    let mut payload = vec![version, 0, 0, (flags | 1) as u8];
    payload.extend_from_slice(&body);
    bx(b"ipma", &payload)
}

/// `iloc` with every file offset moved by `delta`.
fn shifted_iloc(iloc: &[u8], delta: u64) -> Vec<u8> {
    let mut out = iloc.to_vec();
    let version = out[8];
    let mut at = 12;
    let (offset_size, length_size) = ((out[at] >> 4) as usize, (out[at] & 15) as usize);
    let (base_size, index_size) = ((out[at + 1] >> 4) as usize, (out[at + 1] & 15) as usize);
    at += 2;
    let count = if version < 2 {
        at += 2;
        uint(&out[at - 2..at])
    } else {
        at += 4;
        uint(&out[at - 4..at])
    };
    for _ in 0..count {
        at += if version < 2 { 2 } else { 4 };
        let method = if version >= 1 {
            at += 2;
            uint(&out[at - 2..at]) & 15
        } else {
            0
        };
        assert_eq!(method, 0, "file offsets only");
        at += 2; // data_reference_index
        let base = at..at + base_size;
        at += base_size;
        let extents = uint(&out[at..at + 2]);
        at += 2;
        let mut offsets = Vec::new();
        for _ in 0..extents {
            if version >= 1 {
                at += index_size;
            }
            offsets.push(at..at + offset_size);
            at += offset_size + length_size;
        }
        if base_size > 0 {
            let v = uint(&out[base.clone()]);
            put_uint(&mut out[base], v + delta);
        } else {
            for o in offsets {
                assert!(!o.is_empty(), "an extent without an offset cannot move");
                let v = uint(&out[o.clone()]);
                put_uint(&mut out[o], v + delta);
            }
        }
    }
    out
}

/// `avif` with `props` (whole boxes) added to the primary item's properties, after the ones it
/// has, in the order given. `irot` and `imir` are marked essential, as HEIF requires.
pub fn with_properties(avif: &[u8], props: &[Vec<u8>]) -> Vec<u8> {
    let top = children(avif);
    let meta_at = top.iter().position(|(k, _)| k == b"meta").expect("a meta box");
    let meta = top[meta_at].1;
    let meta_end: usize = top[..=meta_at].iter().map(|(_, b)| b.len()).sum();
    assert!(top[meta_at + 1..].iter().any(|(k, _)| k == b"mdat"), "the image data follows meta");
    let parts = children(&meta[12..]);
    let pitm = parts.iter().find(|(k, _)| k == b"pitm").expect("pitm").1;
    let primary = if pitm[8] == 0 { uint(&pitm[12..14]) } else { uint(&pitm[12..16]) } as u32;

    let iprp = parts.iter().find(|(k, _)| k == b"iprp").expect("iprp").1;
    let iprp_parts = children(&iprp[8..]);
    let ipco = iprp_parts.iter().find(|(k, _)| k == b"ipco").expect("ipco").1;
    let existing = children(&ipco[8..]).len();
    let extra: Vec<u16> = props
        .iter()
        .enumerate()
        .map(|(i, p)| {
            let essential = if matches!(&p[4..8], b"irot" | b"imir") { 0x8000 } else { 0 };
            essential | u16::try_from(existing + 1 + i).unwrap()
        })
        .collect();
    let new_iprp_payload: Vec<u8> = iprp_parts
        .iter()
        .flat_map(|(k, b)| match k {
            b"ipco" => bx(b"ipco", &[&ipco[8..], &props.concat()].concat()),
            b"ipma" => extended_ipma(b, primary, &extra),
            _ => b.to_vec(),
        })
        .collect();
    let new_iprp = bx(b"iprp", &new_iprp_payload);
    let delta = (new_iprp.len() - iprp.len()) as u64;

    let mut meta_payload = meta[8..12].to_vec();
    for (k, b) in &parts {
        match k {
            b"iprp" => meta_payload.extend_from_slice(&new_iprp),
            b"iloc" => meta_payload.extend_from_slice(&shifted_iloc(b, delta)),
            _ => meta_payload.extend_from_slice(b),
        }
    }
    let new_meta = bx(b"meta", &meta_payload);
    assert_eq!(new_meta.len() as u64, meta.len() as u64 + delta);
    [&avif[..meta_end - meta.len()], &new_meta, &avif[meta_end..]].concat()
}
