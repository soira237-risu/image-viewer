//! Adds ICC / EXIF to a WebP produced by libwebp's simple API, by rewriting it into the extended
//! (VP8X) container: VP8X, ICCP, the image chunks, EXIF.

use crate::encode::Metadata;

fn chunk(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut c = tag.to_vec();
    c.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    c.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        c.push(0);
    }
    c
}

type Chunk<'a> = ([u8; 4], &'a [u8]);

/// The chunks of a WebP file: (tag, payload).
fn chunks(webp: &[u8]) -> Result<Vec<Chunk<'_>>, String> {
    if webp.len() < 12 || &webp[..4] != b"RIFF" || &webp[8..12] != b"WEBP" {
        return Err("not a WebP file".into());
    }
    let mut out = Vec::new();
    let mut pos = 12;
    while pos + 8 <= webp.len() {
        let tag: [u8; 4] = webp[pos..pos + 4].try_into().unwrap();
        let len = u32::from_le_bytes(webp[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let start = pos + 8;
        let end = start.checked_add(len).filter(|e| *e <= webp.len()).ok_or("truncated WebP chunk")?;
        out.push((tag, &webp[start..end]));
        pos = end + (len & 1);
    }
    Ok(out)
}

pub fn with_metadata(
    simple: &[u8],
    width: u32,
    height: u32,
    has_alpha: bool,
    meta: &Metadata,
) -> Result<Vec<u8>, String> {
    if meta.icc.is_none() && meta.exif.is_none() {
        return Ok(simple.to_vec());
    }
    let image_chunks: Vec<_> = chunks(simple)?.into_iter().filter(|(t, _)| t != b"VP8X").collect();
    let mut flags = 0u8;
    if meta.icc.is_some() {
        flags |= 0x20;
    }
    if has_alpha {
        flags |= 0x10;
    }
    if meta.exif.is_some() {
        flags |= 0x08;
    }
    let le24 = |v: u32| [v as u8, (v >> 8) as u8, (v >> 16) as u8];
    let mut vp8x = vec![flags, 0, 0, 0];
    vp8x.extend_from_slice(&le24(width - 1));
    vp8x.extend_from_slice(&le24(height - 1));

    let mut body = b"WEBP".to_vec();
    body.extend(chunk(b"VP8X", &vp8x));
    if let Some(icc) = &meta.icc {
        body.extend(chunk(b"ICCP", icc));
    }
    for (tag, payload) in image_chunks {
        body.extend(chunk(&tag, payload));
    }
    if let Some(exif) = &meta.exif {
        body.extend(chunk(b"EXIF", exif));
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend(body);
    Ok(out)
}
