//! Decode timings. Every number printed is measured on the current build; nothing is estimated.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use iv_codec::default_registry;
use iv_core::{CancelToken, FormatId};

use crate::files;

/// Longest side requested from `preview` (a typical full-HD viewport).
const PREVIEW_TARGET: u32 = 1920;

pub struct Options {
    runs: usize,
    inputs: Vec<String>,
}

impl Options {
    pub fn parse(args: &[String]) -> Option<Self> {
        let mut runs = 5;
        let mut inputs = Vec::new();
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if a == "--runs" {
                runs = it.next()?.parse().ok().filter(|n: &usize| *n > 0)?;
            } else {
                inputs.push(a.clone());
            }
        }
        (!inputs.is_empty()).then_some(Self { runs, inputs })
    }
}

struct Row {
    path: PathBuf,
    format: FormatId,
    codec: &'static str,
    megapixels: f64,
    file_bytes: u64,
    probe: Duration,
    preview: Option<(Duration, u32, u32)>,
    decode: Duration,
}

fn median(mut v: Vec<Duration>) -> Duration {
    v.sort();
    v[v.len() / 2]
}

fn ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

/// Runs `f` `runs` times and returns the median duration with the last result.
fn timed<T>(runs: usize, mut f: impl FnMut() -> iv_core::Result<T>) -> iv_core::Result<(Duration, T)> {
    let mut times = Vec::with_capacity(runs);
    let mut last = None;
    for _ in 0..runs {
        let t = Instant::now();
        let v = f()?;
        times.push(t.elapsed());
        last = Some(v);
    }
    Ok((median(times), last.expect("runs > 0")))
}

fn bench_one(path: &PathBuf, runs: usize) -> iv_core::Result<Row> {
    let reg = default_registry();
    let src = iv_pipeline::archive::source_for(path)?;
    let cancel = CancelToken::never();

    // warm the OS file cache so the numbers measure decoding, not the disk
    src.read_all()?;

    let (probe, info) = timed(runs, || reg.probe(&src))?;
    let (preview_time, preview) = timed(runs, || reg.preview(&src, PREVIEW_TARGET, &cancel))?;
    let (decode, decoded) = timed(runs, || reg.decode(&src, &cancel))?;

    Ok(Row {
        path: path.clone(),
        format: info.format,
        codec: decoded.info.codec,
        megapixels: info.width as f64 * info.height as f64 / 1e6,
        file_bytes: src.len()?,
        probe,
        preview: preview.map(|p| (preview_time, p.width(), p.height())),
        decode,
    })
}

pub fn run(opts: &Options) -> ExitCode {
    let paths = files::collect(&opts.inputs);
    if paths.is_empty() {
        eprintln!("no image files found");
        return ExitCode::FAILURE;
    }

    println!("build: {}", if cfg!(debug_assertions) { "debug" } else { "release" });
    println!("runs per measurement: {} (median)\n", opts.runs);
    println!(
        "{:<44} {:>7} {:>11} {:>7} {:>9} {:>10} {:>18} {:>10} {:>8}",
        "file", "format", "codec", "MP", "KiB", "probe ms", "preview ms (size)", "decode ms", "MP/s"
    );

    let mut rows = Vec::new();
    let mut failures = 0;
    for path in &paths {
        match bench_one(path, opts.runs) {
            Ok(r) => {
                let preview = r.preview.map_or("-".to_string(), |(t, w, h)| format!("{:.2} ({w}x{h})", ms(t)));
                println!(
                    "{:<44} {:>7} {:>11} {:>7.2} {:>9.0} {:>10.3} {:>18} {:>10.2} {:>8.1}",
                    short(&r.path),
                    r.format.name(),
                    r.codec,
                    r.megapixels,
                    r.file_bytes as f64 / 1024.0,
                    ms(r.probe),
                    preview,
                    ms(r.decode),
                    r.megapixels / r.decode.as_secs_f64(),
                );
                rows.push(r);
            }
            Err(e) => {
                failures += 1;
                println!("{:<44} ERROR {e}", short(path));
            }
        }
    }

    // per-format summary
    let mut by_format: BTreeMap<FormatId, Vec<&Row>> = BTreeMap::new();
    for r in &rows {
        by_format.entry(r.format).or_default().push(r);
    }
    println!("\n{:<10} {:>6} {:>14} {:>14} {:>12}", "format", "files", "median probe", "median decode", "median MP/s");
    for (format, rs) in by_format {
        let probe = median(rs.iter().map(|r| r.probe).collect());
        let decode = median(rs.iter().map(|r| r.decode).collect());
        let mut mps: Vec<f64> = rs.iter().map(|r| r.megapixels / r.decode.as_secs_f64()).collect();
        mps.sort_by(f64::total_cmp);
        println!(
            "{:<10} {:>6} {:>11.3} ms {:>11.2} ms {:>12.1}",
            format.name(),
            rs.len(),
            ms(probe),
            ms(decode),
            mps[mps.len() / 2]
        );
    }

    if failures > 0 {
        println!("\n{failures} file(s) failed");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

fn short(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    if s.chars().count() <= 44 {
        s
    } else {
        format!("…{}", s.chars().rev().take(43).collect::<Vec<_>>().into_iter().rev().collect::<String>())
    }
}
