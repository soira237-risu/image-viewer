//! `iv` — command-line front end to the image pipeline.
//!
//! ```text
//! iv probe <file|dir>...              format, decoder, size, orientation; flags wrong extensions
//! iv bench [--runs N] <file|dir>...   per-file probe / preview / decode timings and a per-format summary
//! iv formats                          formats and the decoders registered for them
//! iv convert --to <format> ...         convert / batch-convert, with optional rotate, flip, downscale
//! iv fix-ext [--dry-run] <file|dir>... rename files whose extension does not match their content
//! iv --version                        the version
//! ```

mod bench;
mod convert;
mod files;

use std::process::ExitCode;

use iv_codec::default_registry;

fn usage() -> ExitCode {
    eprintln!(
        "usage:
  iv probe <file|dir>...
  iv bench [--runs N] <file|dir>...
  iv formats
  {}
  iv fix-ext [--dry-run] <file|dir>...
  iv --version",
        convert::CONVERT_USAGE
    );
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some((cmd, rest)) = args.split_first() else { return usage() };
    match cmd.as_str() {
        "probe" if !rest.is_empty() => probe(rest),
        "bench" => match bench::Options::parse(rest) {
            Some(opts) => bench::run(&opts),
            None => usage(),
        },
        "formats" => formats(),
        "convert" => convert::convert(rest),
        "fix-ext" => convert::fix_ext(rest),
        "--version" | "-V" => {
            println!("iv {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        _ => usage(),
    }
}

fn probe(inputs: &[String]) -> ExitCode {
    let reg = default_registry();
    let paths = files::collect(inputs);
    let mut failed = 0;
    for path in &paths {
        // decoder panics are contained by the registry; an archive reader's here
        let line = iv_core::panic::catch("archive reader", || iv_pipeline::archive::source_for(path))
            .and_then(|src| reg.identify(&src).and_then(|id| reg.probe(&src).map(|info| (id, info))));
        match line {
            Ok((id, info)) => {
                let frames = info.frame_count.map_or("?".to_string(), |n| n.to_string());
                let icc = if info.icc_profile.is_some() { " icc" } else { "" };
                let mismatch = if id.extension_mismatch() {
                    format!(
                        "  [extension says {}, content is {} → .{}]",
                        id.extension_format.unwrap(),
                        id.format,
                        id.format.canonical_extension()
                    )
                } else {
                    String::new()
                };
                println!(
                    "{}\t{}\t{}\t{}x{}\t{}bit{}\t{:?}\tframes={}{icc}{mismatch}",
                    path.display(),
                    info.format,
                    info.codec,
                    info.width,
                    info.height,
                    info.bits_per_channel,
                    if info.has_alpha { "+alpha" } else { "" },
                    info.orientation,
                    frames,
                );
            }
            Err(e) => {
                failed += 1;
                println!("{}\tERROR\t{e}", path.display());
            }
        }
    }
    if failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

fn formats() -> ExitCode {
    let reg = default_registry();
    for f in iv_core::FormatId::ALL {
        let decoders: Vec<_> =
            reg.decoders_for(f).map(|d| format!("{} (p{})", d.info().id, d.info().priority)).collect();
        let list = if decoders.is_empty() { "-".to_string() } else { decoders.join(", ") };
        println!("{:<14} .{:<6} {list}", f.name(), f.canonical_extension());
    }
    ExitCode::SUCCESS
}
