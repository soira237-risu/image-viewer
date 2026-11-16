//! `iv convert` and `iv fix-ext`: format conversion with optional edits, and extension repair.

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use iv_codec::default_registry;
use iv_codec::encode::{EncodeOptions, Encoders};
use iv_core::{FormatId, Orientation, Source};
use iv_ops::Op;
use iv_pipeline::export::{ExportOutcome, ExportRequest, export, fix_extension, with_extension};
use rayon::prelude::*;

use crate::files;

pub const CONVERT_USAGE: &str = "iv convert --to <format> [--out <dir>] [--opt key=value]... \
[--rotate cw|ccw|180] [--flip h|v] [--max <pixels>] [--no-metadata] [--overwrite] <file|dir>...";

struct ConvertArgs {
    format: FormatId,
    out_dir: Option<PathBuf>,
    options: Vec<String>,
    ops: Vec<Op>,
    max_side: Option<u32>,
    keep_metadata: bool,
    overwrite: bool,
    inputs: Vec<String>,
}

fn parse(args: &[String]) -> Result<ConvertArgs, String> {
    let mut a = ConvertArgs {
        format: FormatId::Png,
        out_dir: None,
        options: Vec::new(),
        ops: Vec::new(),
        max_side: None,
        keep_metadata: true,
        overwrite: false,
        inputs: Vec::new(),
    };
    let mut format = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().cloned().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--to" => {
                let v = value("--to")?;
                format = Some(FormatId::from_extension(&v).ok_or_else(|| format!("unknown format '{v}'"))?);
            }
            "--out" => a.out_dir = Some(PathBuf::from(value("--out")?)),
            "--opt" => a.options.push(value("--opt")?),
            "--rotate" => a.ops.push(match value("--rotate")?.as_str() {
                "cw" | "90" => Op::rotate_cw(),
                "ccw" | "270" | "-90" => Op::rotate_ccw(),
                "180" => Op::Orient(Orientation::Rotate180),
                other => return Err(format!("--rotate: '{other}' is not cw, ccw or 180")),
            }),
            "--flip" => a.ops.push(match value("--flip")?.as_str() {
                "h" => Op::flip_h(),
                "v" => Op::flip_v(),
                other => return Err(format!("--flip: '{other}' is not h or v")),
            }),
            "--max" => {
                let v = value("--max")?;
                a.max_side = Some(
                    v.parse()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| format!("--max: '{v}' is not a positive number"))?,
                );
            }
            "--no-metadata" => a.keep_metadata = false,
            "--overwrite" => a.overwrite = true,
            flag if flag.starts_with("--") => return Err(format!("unknown option {flag}")),
            input => a.inputs.push(input.to_string()),
        }
    }
    a.format = format.ok_or("--to is required")?;
    if a.inputs.is_empty() {
        return Err("no input files".into());
    }
    Ok(a)
}

/// Edits for one file: the requested rotations/flips, then a downscale to fit `max_side`.
fn ops_for(path: &Path, a: &ConvertArgs) -> Result<Vec<Op>, String> {
    let mut ops = a.ops.clone();
    if let Some(max) = a.max_side {
        let src = iv_pipeline::archive::source_for(path).map_err(|e| e.to_string())?;
        let info = default_registry().probe(&src).map_err(|e| e.to_string())?;
        let shown = iv_ops::Geometry::new((info.width, info.height), info.orientation, &ops).size;
        let long = shown.0.max(shown.1);
        if long > max {
            let scale = max as f64 / long as f64;
            let w = ((shown.0 as f64 * scale).round() as u32).max(1);
            let h = ((shown.1 as f64 * scale).round() as u32).max(1);
            ops.push(Op::Resize { width: w, height: h });
        }
    }
    Ok(ops)
}

pub fn convert(args: &[String]) -> ExitCode {
    let a = match parse(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}\nusage: {CONVERT_USAGE}");
            return ExitCode::from(2);
        }
    };
    let encoders = Encoders::default();
    let Some(encoder) = encoders.for_format(a.format) else {
        let known: Vec<_> = encoders.all().map(|e| e.info().format.canonical_extension()).collect();
        eprintln!("cannot write {} (writable: {})", a.format, known.join(", "));
        return ExitCode::from(2);
    };
    let options = match EncodeOptions::parse(encoder.options(), &a.options) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    if let Some(dir) = &a.out_dir
        && let Err(e) = std::fs::create_dir_all(dir)
    {
        eprintln!("{}: {e}", dir.display());
        return ExitCode::FAILURE;
    }

    let paths = files::collect(&a.inputs);
    let dests: Vec<PathBuf> = paths
        .iter()
        .map(|path| match &a.out_dir {
            Some(dir) => with_extension(&dir.join(path.file_name().unwrap_or_default()), a.format),
            None => with_extension(path, a.format),
        })
        .collect();
    let earlier = earlier_with_same_dest(&dests);
    let failed = AtomicUsize::new(0);
    let started = Instant::now();
    paths.par_iter().enumerate().for_each(|(i, path)| {
        let dest = dests[i].clone();
        // two inputs for one output (photo.jpg and photo.png -> photo.webp): only the first is
        // converted, so the other is never silently lost or mixed into it
        if let Some(first) = earlier[i] {
            failed.fetch_add(1, Ordering::Relaxed);
            println!("FAIL  {}: same output {} as {}", path.display(), dest.display(), paths[first].display());
            return;
        }
        // converting in place (same name, same format) also replaces the original: ask for it
        if dest.exists() && !a.overwrite {
            println!("skip  {} (exists; --overwrite to replace)", dest.display());
            return;
        }
        let t = Instant::now();
        // a codec crashing on one malformed file fails that file, not the whole batch
        let result = iv_core::panic::catch("conversion", || Ok(ops_for(path, &a)))
            .map_err(|e| e.to_string())
            .and_then(|ops| ops)
            .and_then(|ops| {
                let req = ExportRequest {
                    source: path.clone(),
                    ops,
                    format: a.format,
                    options: options.clone(),
                    dest: dest.clone(),
                    background: [255, 255, 255],
                    keep_metadata: a.keep_metadata,
                };
                iv_core::panic::catch("conversion", || export(&default_registry(), &encoders, &req))
                    .map_err(|e| e.to_string())
            });
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        match result {
            Ok(ExportOutcome::LosslessOrientation) => {
                println!("ok    {} -> {} (lossless EXIF rotation, {ms:.0} ms)", path.display(), dest.display())
            }
            Ok(ExportOutcome::Encoded { bytes, dropped_frames }) => {
                let note = if dropped_frames > 0 {
                    format!(", first frame only: {dropped_frames} frames not saved")
                } else {
                    String::new()
                };
                println!("ok    {} -> {} ({} KiB, {ms:.0} ms{note})", path.display(), dest.display(), bytes / 1024);
            }
            Err(e) => {
                failed.fetch_add(1, Ordering::Relaxed);
                println!("FAIL  {}: {e}", path.display());
            }
        }
    });
    let failed = failed.into_inner();
    println!("{} file(s), {failed} failed, {:.1} s", paths.len(), started.elapsed().as_secs_f64());
    if failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

pub fn fix_ext(args: &[String]) -> ExitCode {
    let dry_run = args.iter().any(|a| a == "--dry-run");
    let inputs: Vec<String> = args.iter().filter(|a| *a != "--dry-run").cloned().collect();
    if inputs.is_empty() {
        eprintln!("usage: iv fix-ext [--dry-run] <file|dir>...");
        return ExitCode::from(2);
    }
    let reg = default_registry();
    let mut failed = 0;
    for path in files::collect(&inputs) {
        if dry_run {
            match reg.identify(&Source::from_path(&path)) {
                Ok(id) if id.extension_mismatch() => {
                    println!("would rename {} -> {}", path.display(), with_extension(&path, id.format).display())
                }
                Ok(_) => {}
                Err(e) => println!("FAIL  {}: {e}", path.display()),
            }
            continue;
        }
        match fix_extension(&reg, &path) {
            Ok(Some(new)) => println!("renamed {} -> {}", path.display(), new.display()),
            Ok(None) => {}
            Err(e) => {
                failed += 1;
                println!("FAIL  {}: {e}", path.display());
            }
        }
    }
    if failed > 0 { ExitCode::FAILURE } else { ExitCode::SUCCESS }
}

/// For each output path, the index of an earlier input that writes the same file (compared
/// case-insensitively on Windows, where `A.webp` and `a.webp` are one file), else `None`.
fn earlier_with_same_dest(dests: &[PathBuf]) -> Vec<Option<usize>> {
    let key = |p: &Path| {
        let s = p.to_string_lossy();
        if cfg!(windows) { s.to_lowercase().replace('/', "\\") } else { s.into_owned() }
    };
    let mut first: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    dests
        .iter()
        .enumerate()
        .map(|(i, d)| {
            first.get(&key(d)).copied().or_else(|| {
                first.insert(key(d), i);
                None
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_that_share_an_output_are_found() {
        let dests: Vec<PathBuf> =
            ["out/photo.webp", "out/other.webp", "out/photo.webp", "out/PHOTO.webp", "out/x.webp"]
                .iter()
                .map(PathBuf::from)
                .collect();
        let expected_upper = if cfg!(windows) { Some(0) } else { None };
        assert_eq!(earlier_with_same_dest(&dests), [None, None, Some(0), expected_upper, None]);
        assert!(earlier_with_same_dest(&[]).is_empty());
    }
}
