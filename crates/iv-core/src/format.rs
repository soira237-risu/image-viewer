use std::fmt;

/// An image container format, independent of which codec handles it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum FormatId {
    Jpeg,
    Png,
    Webp,
    Gif,
    Bmp,
    Ico,
    Tiff,
    Tga,
    Pnm,
    Qoi,
    Dds,
    OpenExr,
    Hdr,
    Avif,
    Heif,
    Jxl,
    Svg,
}

impl FormatId {
    pub const ALL: [FormatId; 17] = [
        FormatId::Jpeg,
        FormatId::Png,
        FormatId::Webp,
        FormatId::Gif,
        FormatId::Bmp,
        FormatId::Ico,
        FormatId::Tiff,
        FormatId::Tga,
        FormatId::Pnm,
        FormatId::Qoi,
        FormatId::Dds,
        FormatId::OpenExr,
        FormatId::Hdr,
        FormatId::Avif,
        FormatId::Heif,
        FormatId::Jxl,
        FormatId::Svg,
    ];

    /// Short human-readable name.
    pub fn name(self) -> &'static str {
        match self {
            FormatId::Jpeg => "JPEG",
            FormatId::Png => "PNG",
            FormatId::Webp => "WebP",
            FormatId::Gif => "GIF",
            FormatId::Bmp => "BMP",
            FormatId::Ico => "ICO",
            FormatId::Tiff => "TIFF",
            FormatId::Tga => "TGA",
            FormatId::Pnm => "PNM",
            FormatId::Qoi => "QOI",
            FormatId::Dds => "DDS",
            FormatId::OpenExr => "OpenEXR",
            FormatId::Hdr => "Radiance HDR",
            FormatId::Avif => "AVIF",
            FormatId::Heif => "HEIF",
            FormatId::Jxl => "JPEG XL",
            FormatId::Svg => "SVG",
        }
    }

    /// File extensions (lowercase, without the dot). The first one is the canonical extension.
    pub fn extensions(self) -> &'static [&'static str] {
        match self {
            FormatId::Jpeg => &["jpg", "jpeg", "jpe", "jfif"],
            FormatId::Png => &["png", "apng"],
            FormatId::Webp => &["webp"],
            FormatId::Gif => &["gif"],
            FormatId::Bmp => &["bmp", "dib"],
            FormatId::Ico => &["ico", "cur"],
            FormatId::Tiff => &["tif", "tiff"],
            FormatId::Tga => &["tga"],
            FormatId::Pnm => &["pnm", "pbm", "pgm", "ppm", "pam"],
            FormatId::Qoi => &["qoi"],
            FormatId::Dds => &["dds"],
            FormatId::OpenExr => &["exr"],
            FormatId::Hdr => &["hdr"],
            FormatId::Avif => &["avif"],
            FormatId::Heif => &["heic", "heif", "hif"],
            FormatId::Jxl => &["jxl"],
            FormatId::Svg => &["svg", "svgz"],
        }
    }

    /// The canonical extension, e.g. `"jpg"`.
    pub fn canonical_extension(self) -> &'static str {
        self.extensions()[0]
    }

    /// Looks a format up by file extension (case-insensitive, with or without a leading dot).
    pub fn from_extension(ext: &str) -> Option<FormatId> {
        let ext = ext.strip_prefix('.').unwrap_or(ext);
        FormatId::ALL.into_iter().find(|f| f.extensions().iter().any(|e| e.eq_ignore_ascii_case(ext)))
    }
}

impl fmt::Display for FormatId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_lookup_is_case_insensitive_and_dot_tolerant() {
        assert_eq!(FormatId::from_extension("JPG"), Some(FormatId::Jpeg));
        assert_eq!(FormatId::from_extension(".jpeg"), Some(FormatId::Jpeg));
        assert_eq!(FormatId::from_extension("HeIc"), Some(FormatId::Heif));
        assert_eq!(FormatId::from_extension("txt"), None);
    }

    #[test]
    fn every_format_has_a_unique_canonical_extension() {
        for f in FormatId::ALL {
            assert_eq!(FormatId::from_extension(f.canonical_extension()), Some(f), "{f}");
        }
    }
}
