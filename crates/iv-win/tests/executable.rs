//! What the built `image-viewer.exe` carries, read from the file itself: the DLLs it needs at
//! start (only ones every Windows 10/11 has, so the portable zip runs on a clean machine) and its
//! application manifest.

#![cfg(all(windows, target_env = "msvc"))]

const EXE: &str = env!("CARGO_BIN_EXE_image-viewer");

fn u16_at(b: &[u8], at: usize) -> usize {
    u16::from_le_bytes(b[at..at + 2].try_into().unwrap()) as usize
}

fn u32_at(b: &[u8], at: usize) -> usize {
    u32::from_le_bytes(b[at..at + 4].try_into().unwrap()) as usize
}

/// A 64-bit PE image as laid out in its file.
struct Pe<'a> {
    bytes: &'a [u8],
    /// (virtual address, virtual size, file offset) of each section
    sections: Vec<(usize, usize, usize)>,
    /// offset of the optional header's data directory
    directories: usize,
}

impl<'a> Pe<'a> {
    fn parse(bytes: &'a [u8]) -> Self {
        assert_eq!(&bytes[..2], b"MZ", "not an executable");
        let pe = u32_at(bytes, 0x3c);
        assert_eq!(&bytes[pe..pe + 4], b"PE\0\0");
        let count = u16_at(bytes, pe + 6);
        let optional = pe + 24;
        assert_eq!(u16_at(bytes, optional), 0x20b, "PE32+ (x64)");
        let table = optional + u16_at(bytes, pe + 20);
        let sections = (0..count)
            .map(|i| {
                let s = table + i * 40;
                (u32_at(bytes, s + 12), u32_at(bytes, s + 8).max(u32_at(bytes, s + 16)), u32_at(bytes, s + 20))
            })
            .collect();
        Pe { bytes, sections, directories: optional + 112 }
    }

    fn offset(&self, rva: usize) -> usize {
        let (va, _, raw) = self
            .sections
            .iter()
            .find(|(va, size, _)| (*va..va + size).contains(&rva))
            .unwrap_or_else(|| panic!("RVA {rva:#x} is in no section"));
        raw + (rva - va)
    }

    /// (RVA, size) of data directory `index`.
    fn directory(&self, index: usize) -> (usize, usize) {
        let at = self.directories + index * 8;
        (u32_at(self.bytes, at), u32_at(self.bytes, at + 4))
    }

    fn c_string(&self, rva: usize) -> String {
        let start = self.offset(rva);
        let end = start + self.bytes[start..].iter().position(|&b| b == 0).unwrap();
        String::from_utf8_lossy(&self.bytes[start..end]).into_owned()
    }

    /// DLL names from the import table (directory 1, 20-byte descriptors, name at +12) and the
    /// delay-load table (directory 13, 32-byte descriptors, name at +4).
    fn imported_dlls(&self) -> Vec<String> {
        let mut dlls = self.loaded_at_start();
        dlls.extend(self.delay_loaded());
        dlls
    }

    /// DLL names in the import table: loaded with the program.
    fn loaded_at_start(&self) -> Vec<String> {
        self.dll_table(1, 20, 12)
    }

    /// DLL names in the delay-load table: loaded at their first call.
    fn delay_loaded(&self) -> Vec<String> {
        self.dll_table(13, 32, 4)
    }

    fn dll_table(&self, index: usize, step: usize, name_at: usize) -> Vec<String> {
        let mut dlls = Vec::new();
        let (rva, _) = self.directory(index);
        if rva == 0 {
            return dlls;
        }
        let mut at = self.offset(rva);
        while self.bytes[at..at + step].iter().any(|&b| b != 0) {
            dlls.push(self.c_string(u32_at(self.bytes, at + name_at)));
            at += step;
        }
        dlls
    }

    /// The RT_MANIFEST resource (type 24), first name, first language.
    fn manifest(&self) -> Option<String> {
        let (rva, _) = self.directory(2);
        if rva == 0 {
            return None;
        }
        let root = self.offset(rva);
        // entries of the directory at `dir` (offset from the resource section's start)
        let entries = |dir: usize| {
            let at = root + dir;
            let n = u16_at(self.bytes, at + 12) + u16_at(self.bytes, at + 14);
            (0..n).map(move |i| (u32_at(self.bytes, at + 16 + i * 8), u32_at(self.bytes, at + 20 + i * 8)))
        };
        const SUBDIR: usize = 0x8000_0000;
        let (_, types) = entries(0).find(|&(id, _)| id == 24)?;
        let (_, names) = entries(types & !SUBDIR).next()?;
        let (_, leaf) = entries(names & !SUBDIR).next()?;
        let data = root + leaf;
        let (start, len) = (self.offset(u32_at(self.bytes, data)), u32_at(self.bytes, data + 4));
        Some(String::from_utf8_lossy(&self.bytes[start..start + len]).into_owned())
    }
}

#[test]
fn needs_no_c_runtime_dll() {
    let bytes = std::fs::read(EXE).unwrap();
    let dlls = Pe::parse(&bytes).imported_dlls();
    assert!(dlls.iter().any(|d| d.eq_ignore_ascii_case("KERNEL32.dll")), "import table read: {dlls:?}");
    // the Visual C++ runtime is not part of Windows; .cargo/config.toml links it statically
    let runtime = ["vcruntime", "msvcp", "msvcr", "ucrtbase", "api-ms-win-crt-", "concrt"];
    let bad: Vec<_> = dlls.iter().filter(|d| runtime.iter().any(|r| d.to_ascii_lowercase().starts_with(r))).collect();
    assert!(bad.is_empty(), "C runtime DLLs imported: {bad:?} (all imports: {dlls:?})");
}

/// WinHTTP serves only the check for updates the user asks for, so it is not loaded at start.
#[test]
fn winhttp_is_loaded_only_when_used() {
    let bytes = std::fs::read(EXE).unwrap();
    let pe = Pe::parse(&bytes);
    let is_winhttp = |d: &String| d.eq_ignore_ascii_case("winhttp.dll");
    assert!(pe.delay_loaded().iter().any(is_winhttp), "delay-loaded: {:?}", pe.delay_loaded());
    assert!(!pe.loaded_at_start().iter().any(is_winhttp), "loaded at start: {:?}", pe.loaded_at_start());
}

#[test]
fn manifest_declares_windows_10_controls_6_and_per_monitor_dpi() {
    let bytes = std::fs::read(EXE).unwrap();
    let manifest = Pe::parse(&bytes).manifest().expect("an embedded manifest");
    // the linker rewrites the merged XML (namespace prefixes such as `ms_compatibility:`, closing
    // tags), so look for the parts that carry meaning
    let supported_os = manifest.find("supportedOS ").expect("a supportedOS element");
    assert!(
        manifest[supported_os..].starts_with(r#"supportedOS xmlns:ms_compatibility="urn:schemas-microsoft-com:compatibility.v1" Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}""#)
            || manifest[supported_os..].starts_with(r#"supportedOS Id="{8e0f7a12-bfb3-4fe8-b9a5-48fd50a15a9a}""#),
        "Windows 10/11 is the supported OS:\n{manifest}"
    );
    for wanted in [
        r#"name="Microsoft.Windows.Common-Controls" version="6.0.0.0""#,
        ">PerMonitorV2</dpiAwareness>",
        // the linker's own part survives the merge
        r#"requestedExecutionLevel level="asInvoker""#,
    ] {
        assert!(manifest.contains(wanted), "{wanted} missing from the manifest:\n{manifest}");
    }
}
