//! Encodes an image as HEIC with the Windows HEIF encoder (needs the HEIF and HEVC extensions).
//!
//! ```text
//! cargo run -p iv-codec --example heic_fixture -- <input image> <output.heic>
//! ```

#[cfg(windows)]
fn main() {
    use windows::Win32::Foundation::GENERIC_WRITE;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
    use windows::core::HSTRING;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let [input, output] = args.as_slice() else {
        eprintln!("usage: heic_fixture <input image> <output.heic>");
        std::process::exit(2);
    };
    let img = image::open(input).expect("read input").to_rgba8();
    let (w, h) = img.dimensions();
    // WIC encoders take BGRA
    let bgra: Vec<u8> = img.pixels().flat_map(|p| [p[2], p[1], p[0], p[3]]).collect();

    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let f: IWICImagingFactory = CoCreateInstance(&CLSID_WICImagingFactory2, None, CLSCTX_INPROC_SERVER).unwrap();
        let stream = f.CreateStream().unwrap();
        stream.InitializeFromFilename(&HSTRING::from(output.as_str()), GENERIC_WRITE.0).unwrap();
        let encoder = f.CreateEncoder(&GUID_ContainerFormatHeif, std::ptr::null()).expect("HEIF encoder installed");
        encoder.Initialize(&stream, WICBitmapEncoderNoCache).unwrap();
        let mut frame = None;
        encoder.CreateNewFrame(&mut frame, std::ptr::null_mut()).unwrap();
        let frame = frame.unwrap();
        frame.Initialize(None).unwrap();
        frame.SetSize(w, h).unwrap();
        let mut format = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut format).unwrap();
        frame.WritePixels(h, w * 4, &bgra).unwrap();
        frame.Commit().unwrap();
        encoder.Commit().unwrap();
    }
    println!("wrote {output} ({w}x{h})");
}

#[cfg(not(windows))]
fn main() {
    eprintln!("heic_fixture needs Windows");
}
