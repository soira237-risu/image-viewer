//! A compiled Windows resource file (`.res`) written without the resource compiler: the
//! application icon, taken from an `.ico` file, and the version information. The MSVC linker
//! takes a `.res` file as input directly. Used by `build.rs`, and by the crate's tests to check
//! what it writes.

/// Resource types (`RT_*`).
pub const RT_ICON: u16 = 3;
pub const RT_GROUP_ICON: u16 = 14;
pub const RT_VERSION: u16 = 16;
/// The icon group's id: what `LoadImageW(instance, MAKEINTRESOURCE(1), IMAGE_ICON, …)` loads,
/// and Explorer's icon for the program (the first group).
pub const ICON_GROUP_ID: u16 = 1;
/// English (United States), with Unicode strings: the language the version strings are in.
const LANGUAGE: u16 = 0x0409;
const CODE_PAGE: u16 = 1200;

/// One image of an `.ico` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IcoImage {
    /// 0 means 256.
    pub width: u8,
    pub height: u8,
    pub colors: u8,
    pub planes: u16,
    pub bit_count: u16,
    pub data: Vec<u8>,
}

fn u16_at(b: &[u8], at: usize) -> Result<u16, String> {
    b.get(at..at + 2).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or_else(|| format!("truncated at {at}"))
}

fn u32_at(b: &[u8], at: usize) -> Result<u32, String> {
    b.get(at..at + 4).map(|s| u32::from_le_bytes([s[0], s[1], s[2], s[3]])).ok_or_else(|| format!("truncated at {at}"))
}

/// The images of an `.ico` file.
pub fn parse_ico(ico: &[u8]) -> Result<Vec<IcoImage>, String> {
    if u16_at(ico, 0)? != 0 || u16_at(ico, 2)? != 1 {
        return Err("not an icon file".into());
    }
    let count = usize::from(u16_at(ico, 4)?);
    (0..count)
        .map(|i| {
            let e = 6 + i * 16;
            let head = ico.get(e..e + 4).ok_or("truncated directory")?;
            let size = u32_at(ico, e + 8)? as usize;
            let offset = u32_at(ico, e + 12)? as usize;
            let data = ico.get(offset..offset + size).ok_or_else(|| format!("image {i} lies outside the file"))?;
            Ok(IcoImage {
                width: head[0],
                height: head[1],
                colors: head[2],
                planes: u16_at(ico, e + 4)?,
                bit_count: u16_at(ico, e + 6)?,
                data: data.to_vec(),
            })
        })
        .collect()
}

/// An `.ico` file holding `images` in order.
pub fn ico_file(images: &[IcoImage]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len();
    for img in images {
        out.extend_from_slice(&[img.width, img.height, img.colors, 0]);
        out.extend_from_slice(&img.planes.to_le_bytes());
        out.extend_from_slice(&img.bit_count.to_le_bytes());
        out.extend_from_slice(&(img.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(offset as u32).to_le_bytes());
        offset += img.data.len();
    }
    for img in images {
        out.extend_from_slice(&img.data);
    }
    out
}

/// The icon group: the `.ico` directory with resource ids (`first_id`, `first_id + 1`, …) in
/// place of file offsets.
fn group_icon(images: &[IcoImage], first_id: u16) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&[0, 0, 1, 0]);
    out.extend_from_slice(&(images.len() as u16).to_le_bytes());
    for (i, img) in images.iter().enumerate() {
        out.extend_from_slice(&[img.width, img.height, img.colors, 0]);
        out.extend_from_slice(&img.planes.to_le_bytes());
        out.extend_from_slice(&img.bit_count.to_le_bytes());
        out.extend_from_slice(&(img.data.len() as u32).to_le_bytes());
        out.extend_from_slice(&(first_id + i as u16).to_le_bytes());
    }
    out
}

fn utf16z(s: &str) -> Vec<u8> {
    s.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
}

fn pad4(out: &mut Vec<u8>) {
    while out.len() % 4 != 0 {
        out.push(0);
    }
}

enum NodeValue<'a> {
    None,
    Binary(&'a [u8]),
    Text(&'a str),
}

/// One block of version information: length, value length, type, key, value, then children,
/// each starting on a 32-bit boundary (blocks are placed at 32-bit offsets, so offsets within a
/// block are enough).
fn node(key: &str, value: NodeValue, children: &[Vec<u8>]) -> Vec<u8> {
    let (value_bytes, value_len, kind) = match value {
        NodeValue::None => (Vec::new(), 0u16, 1u16),
        NodeValue::Binary(b) => (b.to_vec(), b.len() as u16, 0),
        // text lengths count UTF-16 units, with the terminating NUL
        NodeValue::Text(t) => {
            let b = utf16z(t);
            let units = (b.len() / 2) as u16;
            (b, units, 1)
        }
    };
    let mut out = vec![0, 0];
    out.extend_from_slice(&value_len.to_le_bytes());
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&utf16z(key));
    pad4(&mut out);
    out.extend_from_slice(&value_bytes);
    for child in children {
        pad4(&mut out);
        out.extend_from_slice(child);
    }
    let len = out.len() as u16;
    out[..2].copy_from_slice(&len.to_le_bytes());
    out
}

/// `VS_VERSIONINFO`: the numeric version (`[major, minor, patch, build]`) for file and product,
/// and the version strings, in US English.
pub fn version_info(version: [u16; 4], strings: &[(&str, &str)]) -> Vec<u8> {
    let [a, b, c, d] = version.map(u32::from);
    let (ms, ls) = (a << 16 | b, c << 16 | d);
    let fixed: Vec<u8> = [
        0xfeef_04bd, // signature
        0x0001_0000, // structure version
        ms,
        ls, // file version
        ms,
        ls,          // product version
        0x3f,        // flags mask
        0,           // flags
        0x0004_0004, // VOS_NT_WINDOWS32
        1,           // VFT_APP
        0,
        0,
        0, // subtype, date
    ]
    .into_iter()
    .flat_map(u32::to_le_bytes)
    .collect();
    let entries: Vec<Vec<u8>> = strings.iter().map(|(k, v)| node(k, NodeValue::Text(v), &[])).collect();
    let table = node(&format!("{LANGUAGE:04X}{CODE_PAGE:04X}"), NodeValue::None, &entries);
    let string_info = node("StringFileInfo", NodeValue::None, &[table]);
    let translation: Vec<u8> = [LANGUAGE, CODE_PAGE].into_iter().flat_map(u16::to_le_bytes).collect();
    let var = node("Translation", NodeValue::Binary(&translation), &[]);
    let var_info = node("VarFileInfo", NodeValue::None, &[var]);
    node("VS_VERSION_INFO", NodeValue::Binary(&fixed), &[string_info, var_info])
}

/// One resource with a numeric type and id, padded to 32 bits.
fn resource(out: &mut Vec<u8>, kind: u16, id: u16, flags: u16, data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes()); // header size
    out.extend_from_slice(&[0xff, 0xff]);
    out.extend_from_slice(&kind.to_le_bytes());
    out.extend_from_slice(&[0xff, 0xff]);
    out.extend_from_slice(&id.to_le_bytes());
    out.extend_from_slice(&0u32.to_le_bytes()); // data version
    out.extend_from_slice(&flags.to_le_bytes());
    out.extend_from_slice(&LANGUAGE.to_le_bytes());
    out.extend_from_slice(&[0; 8]); // version, characteristics
    out.extend_from_slice(data);
    pad4(out);
}

/// The `.res` file: every image of `ico` as an icon resource (ids 1, 2, …), the icon group
/// [`ICON_GROUP_ID`] listing them, and `version` (from [`version_info`]).
pub fn resource_file(ico: &[u8], version: &[u8]) -> Result<Vec<u8>, String> {
    let images = parse_ico(ico)?;
    if images.is_empty() {
        return Err("the icon file holds no images".into());
    }
    // a .res file starts with an empty resource
    let mut out = Vec::new();
    out.extend_from_slice(&0u32.to_le_bytes());
    out.extend_from_slice(&32u32.to_le_bytes());
    out.extend_from_slice(&[0xff, 0xff, 0, 0, 0xff, 0xff, 0, 0]);
    out.extend_from_slice(&[0; 16]);
    // memory flags: MOVEABLE | DISCARDABLE (| PURE for groups and version), as rc.exe writes them
    for (i, img) in images.iter().enumerate() {
        resource(&mut out, RT_ICON, 1 + i as u16, 0x1010, &img.data);
    }
    resource(&mut out, RT_GROUP_ICON, ICON_GROUP_ID, 0x1030, &group_icon(&images, 1));
    resource(&mut out, RT_VERSION, 1, 0x0030, version);
    Ok(out)
}
