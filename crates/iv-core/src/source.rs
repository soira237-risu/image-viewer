use std::fs::File;
use std::io::{self, BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::Result;

/// Where image bytes come from. Codecs never touch the filesystem directly, so archive entries,
/// clipboard data and dropped buffers can later go through the same decoders.
#[derive(Clone, Debug)]
pub struct Source {
    kind: SourceKind,
}

#[derive(Clone, Debug)]
enum SourceKind {
    File(PathBuf),
    Memory { name: String, bytes: Arc<[u8]> },
}

impl Source {
    pub fn from_path(path: impl Into<PathBuf>) -> Self {
        Self { kind: SourceKind::File(path.into()) }
    }

    pub fn from_bytes(name: impl Into<String>, bytes: impl Into<Arc<[u8]>>) -> Self {
        Self { kind: SourceKind::Memory { name: name.into(), bytes: bytes.into() } }
    }

    /// The file path, for file-backed sources.
    pub fn path(&self) -> Option<&Path> {
        match &self.kind {
            SourceKind::File(p) => Some(p),
            SourceKind::Memory { .. } => None,
        }
    }

    /// Name shown to the user (file name, or the name given to an in-memory source).
    pub fn display_name(&self) -> String {
        match &self.kind {
            SourceKind::File(p) => {
                p.file_name().map_or_else(|| p.display().to_string(), |n| n.to_string_lossy().into_owned())
            }
            SourceKind::Memory { name, .. } => name.clone(),
        }
    }

    /// Lowercase extension without the dot, if the name has one.
    pub fn extension(&self) -> Option<String> {
        let name = match &self.kind {
            SourceKind::File(p) => return p.extension().map(|e| e.to_string_lossy().to_ascii_lowercase()),
            SourceKind::Memory { name, .. } => name,
        };
        Path::new(name).extension().map(|e| e.to_string_lossy().to_ascii_lowercase())
    }

    /// Reads at most `max` leading bytes (for format sniffing). Shorter files return fewer bytes.
    pub fn read_head(&self, max: usize) -> Result<Vec<u8>> {
        match &self.kind {
            SourceKind::File(p) => {
                let mut buf = Vec::with_capacity(max);
                File::open(p)?.take(max as u64).read_to_end(&mut buf)?;
                Ok(buf)
            }
            SourceKind::Memory { bytes, .. } => Ok(bytes[..bytes.len().min(max)].to_vec()),
        }
    }

    /// A buffered, seekable reader over the content. Header probes use this so they only
    /// touch the bytes they need.
    pub fn reader(&self) -> Result<SourceReader> {
        match &self.kind {
            SourceKind::File(p) => Ok(SourceReader::File(BufReader::with_capacity(64 * 1024, File::open(p)?))),
            SourceKind::Memory { bytes, .. } => Ok(SourceReader::Memory(Cursor::new(bytes.clone()))),
        }
    }

    /// The whole encoded content. File sources are read on every call (the OS page cache makes
    /// repeats cheap); nothing is retained here, so memory accounting stays with the caller.
    pub fn read_all(&self) -> Result<Arc<[u8]>> {
        match &self.kind {
            SourceKind::File(p) => Ok(std::fs::read(p)?.into()),
            SourceKind::Memory { bytes, .. } => Ok(bytes.clone()),
        }
    }

    /// Size of the encoded content in bytes.
    pub fn len(&self) -> Result<u64> {
        match &self.kind {
            SourceKind::File(p) => Ok(std::fs::metadata(p)?.len()),
            SourceKind::Memory { bytes, .. } => Ok(bytes.len() as u64),
        }
    }

    pub fn is_empty(&self) -> Result<bool> {
        Ok(self.len()? == 0)
    }
}

/// Reader returned by [`Source::reader`]; implements `Read`, `BufRead` and `Seek`.
pub enum SourceReader {
    File(BufReader<File>),
    Memory(Cursor<Arc<[u8]>>),
}

impl Read for SourceReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            SourceReader::File(r) => r.read(buf),
            SourceReader::Memory(r) => r.read(buf),
        }
    }
}

impl BufRead for SourceReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        match self {
            SourceReader::File(r) => r.fill_buf(),
            SourceReader::Memory(r) => r.fill_buf(),
        }
    }

    fn consume(&mut self, amt: usize) {
        match self {
            SourceReader::File(r) => r.consume(amt),
            SourceReader::Memory(r) => r.consume(amt),
        }
    }
}

impl Seek for SourceReader {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        match self {
            SourceReader::File(r) => r.seek(pos),
            SourceReader::Memory(r) => r.seek(pos),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reader_reads_and_seeks() {
        let s = Source::from_bytes("x.bin", b"0123456789".to_vec());
        let mut r = s.reader().unwrap();
        r.seek(SeekFrom::Start(4)).unwrap();
        let mut buf = String::new();
        r.read_to_string(&mut buf).unwrap();
        assert_eq!(buf, "456789");
    }

    #[test]
    fn memory_source_head_and_extension() {
        let s = Source::from_bytes("Photo.JPG", vec![1u8, 2, 3, 4]);
        assert_eq!(s.read_head(2).unwrap(), vec![1, 2]);
        assert_eq!(s.read_head(10).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(s.extension().as_deref(), Some("jpg"));
        assert_eq!(s.len().unwrap(), 4);
        assert!(s.path().is_none());
    }

    #[test]
    fn file_source_reads_from_disk() {
        let dir = std::env::temp_dir().join(format!("iv-core-src-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("a.PNG");
        std::fs::write(&path, b"abcdef").unwrap();

        let s = Source::from_path(&path);
        assert_eq!(s.read_head(3).unwrap(), b"abc");
        assert_eq!(&*s.read_all().unwrap(), b"abcdef");
        assert_eq!(s.extension().as_deref(), Some("png"));
        assert_eq!(s.display_name(), "a.PNG");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}
