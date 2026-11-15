//! The decoding service against real files.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use iv_pipeline::{Pipeline, PipelineConfig, PipelineEvent};

const SIDE: u32 = 64;
const IMAGE_BYTES: u64 = (SIDE * SIDE * 4) as u64;

fn write_pngs(dir: &Path, n: usize) -> Vec<PathBuf> {
    (0..n)
        .map(|i| {
            let p = dir.join(format!("img{i}.png"));
            image::RgbaImage::from_pixel(SIDE, SIDE, image::Rgba([i as u8, 0, 0, 255])).save(&p).unwrap();
            p
        })
        .collect()
}

fn pipeline(budget: u64, workers: usize) -> (Pipeline, Receiver<PipelineEvent>) {
    let (tx, rx) = channel();
    let tx = std::sync::Mutex::new(tx);
    let p = Pipeline::new(
        Arc::new(iv_codec::default_registry()),
        PipelineConfig { prefetch_workers: workers, budget_bytes: budget },
        move |e| {
            let _ = tx.lock().unwrap().send(e);
        },
    );
    (p, rx)
}

/// Waits until the pipeline has nothing queued or running.
fn settle(p: &Pipeline) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let s = p.stats();
        if s.queued == 0 && s.in_flight == 0 {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    panic!("pipeline did not settle: {:?}", p.stats());
}

fn ready_for(rx: &Receiver<PipelineEvent>, path: &Path) -> Arc<iv_codec::Decoded> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match rx.recv_timeout(left).expect("event before timeout") {
            PipelineEvent::Ready { path: p, decoded, .. } if p == path => return decoded,
            PipelineEvent::Failed { path: p, error } if p == path => panic!("{error}"),
            _ => {}
        }
    }
}

#[test]
fn foreground_load_reports_probe_then_ready_and_then_hits_the_cache() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 1);
    let (p, rx) = pipeline(u64::MAX, 1);

    assert!(p.request(&files[0], &[]).is_none(), "cold cache");
    let first = rx.recv_timeout(Duration::from_secs(10)).unwrap();
    assert!(matches!(first, PipelineEvent::Probed { ref path, .. } if *path == files[0]), "{first:?}");
    let d = ready_for(&rx, &files[0]);
    assert_eq!(d.image.width(), SIDE);

    let hit = p.request(&files[0], &[]).expect("cache hit");
    assert!(Arc::ptr_eq(&hit, &d), "same decoded image, no re-decode");
}

#[test]
fn neighbours_are_prefetched_and_then_served_instantly() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 5);
    let (p, rx) = pipeline(u64::MAX, 2);

    p.request(&files[0], &files[1..4]);
    ready_for(&rx, &files[0]);
    settle(&p);
    for f in &files[1..4] {
        assert!(p.cached(f).is_some(), "{} prefetched", f.display());
    }
    assert!(p.cached(&files[4]).is_none(), "not in the plan");
    assert!(p.request(&files[2], &[]).is_some(), "navigating to a prefetched image is a hit");
}

#[test]
fn cache_never_exceeds_the_budget_but_keeps_the_current_image() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 8);
    // room for two and a half images
    let budget = IMAGE_BYTES * 5 / 2;
    let (p, rx) = pipeline(budget, 3);

    for i in 0..files.len() {
        let rest: Vec<PathBuf> = files.iter().skip(i + 1).cloned().collect();
        p.request(&files[i], &rest);
        if p.cached(&files[i]).is_none() {
            ready_for(&rx, &files[i]);
        }
        settle(&p);
        let s = p.stats();
        assert!(s.used_bytes <= budget, "step {i}: {s:?}");
        assert!(p.cached(&files[i]).is_some(), "step {i}: current image is kept");
    }

    // a single image larger than the whole budget is still kept while it is current
    let (tiny, rx) = pipeline(IMAGE_BYTES / 2, 1);
    tiny.request(&files[0], &[]);
    ready_for(&rx, &files[0]);
    assert!(tiny.cached(&files[0]).is_some());
}

#[test]
fn moving_on_drops_work_for_images_that_left_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 40);
    let (p, rx) = pipeline(u64::MAX, 1);

    // queue lots of prefetch work, then immediately jump elsewhere with no neighbours
    p.request(&files[0], &files[1..39]);
    let before: Vec<PathBuf> = p.cached_paths();
    p.request(&files[39], &[]);
    ready_for(&rx, &files[39]);
    settle(&p);

    let after = p.cached_paths();
    let unexpected: Vec<_> = after.iter().filter(|f| **f != files[39] && !before.contains(f)).collect();
    // at most the jobs that were already running when the plan changed can have finished
    assert!(unexpected.len() <= 2, "stale prefetches kept: {unexpected:?}");
    assert!(after.len() < 10, "the 38-image prefetch queue was dropped, cached {}", after.len());
}

#[test]
fn failures_are_reported_and_retried_on_explicit_request() {
    let dir = tempfile::tempdir().unwrap();
    let bad = dir.path().join("broken.png");
    std::fs::write(&bad, b"\x89PNG\r\n\x1a\nnot really").unwrap();
    let (p, rx) = pipeline(u64::MAX, 1);

    for attempt in 0..2 {
        p.request(&bad, &[]);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("an event") {
                PipelineEvent::Failed { path, .. } if path == bad => break,
                PipelineEvent::Ready { .. } => panic!("attempt {attempt}: corrupt file decoded"),
                _ => {}
            }
        }
        settle(&p);
    }
}

#[test]
fn dropping_the_pipeline_stops_its_workers() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 30);
    let (p, _rx) = pipeline(u64::MAX, 3);
    p.request(&files[0], &files[1..]);
    let t = Instant::now();
    drop(p);
    assert!(t.elapsed() < Duration::from_secs(2), "drop took {:?}", t.elapsed());
}

/// A noisy image large enough that decoding takes a while (tens of milliseconds).
fn write_slow_png(dir: &Path, name: &str) -> PathBuf {
    let p = dir.join(name);
    let mut seed = 0x2545_f491_u32;
    image::RgbImage::from_fn(2500, 2500, |_, _| {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        image::Rgb([seed as u8, (seed >> 8) as u8, (seed >> 16) as u8])
    })
    .save(&p)
    .unwrap();
    p
}

fn wait_until_running(p: &Pipeline) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while p.stats().in_flight == 0 {
        assert!(Instant::now() < deadline, "decode never started");
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn readies(rx: &Receiver<PipelineEvent>) -> Vec<PathBuf> {
    rx.try_iter()
        .filter_map(|e| match e {
            PipelineEvent::Ready { path, .. } => Some(path),
            _ => None,
        })
        .collect()
}

#[test]
fn quick_browsing_finishes_and_announces_the_image_being_decoded() {
    let dir = tempfile::tempdir().unwrap();
    let slow = write_slow_png(dir.path(), "a-slow.png");
    let files = write_pngs(dir.path(), 3);
    let (p, rx) = pipeline(u64::MAX, 1);

    p.request_quick(&slow);
    wait_until_running(&p);
    // the user has already moved on while `slow` is still decoding
    p.request_quick(&files[1]);
    ready_for(&rx, &files[1]);
    settle(&p);
    assert!(p.cached(&slow).is_some(), "the passed image was finished, not thrown away");
    assert!(p.cached(&files[2]).is_none(), "no prefetching while quick browsing");

    // releasing the key: a normal request resumes prefetching
    p.request(&files[1], &files[2..]);
    settle(&p);
    assert!(p.cached(&files[2]).is_some());
}

#[test]
fn normal_navigation_cancels_the_image_being_decoded() {
    let dir = tempfile::tempdir().unwrap();
    let slow = write_slow_png(dir.path(), "a-slow.png");
    let files = write_pngs(dir.path(), 1);
    let (p, rx) = pipeline(u64::MAX, 1);

    p.request(&slow, &[]);
    wait_until_running(&p);
    p.request(&files[0], &[]);
    ready_for(&rx, &files[0]);
    settle(&p);
    assert!(p.cached(&slow).is_none(), "an image the user left is not kept");
    assert!(!readies(&rx).contains(&slow));
}
