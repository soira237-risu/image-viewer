//! The decoding service against real files.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use iv_core::{CancelToken, FormatId, ImageInfo, Source};
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
            PipelineEvent::Failed { path: p, error, .. } if p == path => panic!("{error}"),
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

#[test]
fn failures_say_what_kind_they_are() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("gone.jpg");
    let junk = dir.path().join("junk.dat");
    std::fs::write(&junk, b"definitely not an image of any kind").unwrap();
    let (p, rx) = pipeline(u64::MAX, 1);
    for (path, want) in [(&missing, iv_pipeline::FailureKind::NotFound), (&junk, iv_pipeline::FailureKind::Unsupported)]
    {
        p.request(path, &[]);
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("an event") {
                PipelineEvent::Failed { path: p, kind, error } if p == *path => {
                    assert_eq!(kind, want, "{}: {error}", path.display());
                    break;
                }
                PipelineEvent::Ready { .. } => panic!("{} decoded", path.display()),
                _ => {}
            }
        }
    }
}

/// Holds every decode of files whose name starts with `prefix` until `open` is set (ignoring
/// cancellation, like a decoder in the middle of a long pixel decode), then fails so the next
/// decoder (image-rs) decodes the file for real.
struct Gate {
    info: iv_codec::DecoderInfo,
    prefix: &'static str,
    open: Arc<AtomicBool>,
    /// Decodes waiting at the gate.
    waiting: Arc<AtomicUsize>,
}

impl iv_codec::Decoder for Gate {
    fn info(&self) -> &iv_codec::DecoderInfo {
        &self.info
    }
    fn claims(&self, src: &Source, _: FormatId) -> iv_core::Result<bool> {
        Ok(src.display_name().starts_with(self.prefix))
    }
    fn probe(&self, _: &Source, _: FormatId) -> iv_core::Result<ImageInfo> {
        Err(iv_core::Error::decode("gate", "probe by the next decoder"))
    }
    fn decode(&self, _: &Source, _: FormatId, _: &CancelToken) -> iv_core::Result<iv_codec::Decoded> {
        self.waiting.fetch_add(1, Ordering::AcqRel);
        while !self.open.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.waiting.fetch_sub(1, Ordering::AcqRel);
        Err(iv_core::Error::decode("gate", "opened"))
    }
}

fn gated_pipeline(prefix: &'static str, budget: u64, workers: usize) -> (Pipeline, Receiver<PipelineEvent>, Gated) {
    let gated = Gated::default();
    let mut reg = iv_codec::default_registry();
    reg.register(Arc::new(Gate {
        info: iv_codec::DecoderInfo { id: "gate", name: "gate", formats: &[FormatId::Png], priority: 1000 },
        prefix,
        open: gated.open.clone(),
        waiting: gated.waiting.clone(),
    }));
    let (p, rx) = pipeline_with(reg, budget, workers, |_| {});
    (p, rx, gated)
}

fn pipeline_with(
    reg: iv_codec::Registry,
    budget: u64,
    workers: usize,
    also: impl Fn(&PipelineEvent) + Send + Sync + 'static,
) -> (Pipeline, Receiver<PipelineEvent>) {
    let (tx, rx) = channel();
    let tx = std::sync::Mutex::new(tx);
    let p =
        Pipeline::new(Arc::new(reg), PipelineConfig { prefetch_workers: workers, budget_bytes: budget }, move |e| {
            also(&e);
            let _ = tx.lock().unwrap().send(e);
        });
    (p, rx)
}

fn failed_for(rx: &Receiver<PipelineEvent>, path: &Path) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("an event") {
            PipelineEvent::Failed { path: p, error, .. } if p == path => return error,
            PipelineEvent::Ready { path: p, .. } if p == path => panic!("{} decoded", p.display()),
            _ => {}
        }
    }
}

#[test]
fn a_crashing_decoder_fails_one_image_and_the_workers_carry_on() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 3);
    // a decoder panicking deep inside a third-party crate (here: in the probe, through the sink)
    let boom = files[0].clone();
    let (p, rx) = pipeline_with(iv_codec::default_registry(), u64::MAX, 1, move |e| {
        if let PipelineEvent::Probed { path, .. } = e {
            assert!(*path != boom, "index out of bounds");
        }
    });
    for _ in 0..3 {
        // more crashes than there are workers: each one would have killed its thread
        p.request(&files[0], &[]);
        let error = failed_for(&rx, &files[0]);
        assert!(error.contains("crashed") && error.contains("index out of bounds"), "{error}");
        settle(&p);
    }
    p.request(&files[1], &[]);
    ready_for(&rx, &files[1]);
    p.request(&files[2], &[]);
    ready_for(&rx, &files[2]);
    assert_eq!(p.stats().in_flight, 0, "no bookkeeping left behind by the crashes");
}

#[test]
fn a_new_image_does_not_wait_for_an_abandoned_decode() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 1);
    let stuck = dir.path().join("hold.png");
    std::fs::copy(&files[0], &stuck).unwrap();
    let (p, rx, gate) = gated_pipeline("hold", u64::MAX, 1);

    p.request(&stuck, &[]);
    gate.wait_for(1);
    // Home/End or opening another file: the target is neither cached nor being prefetched
    p.request(&files[0], &[]);
    ready_for(&rx, &files[0]);
    gate.open();
    settle(&p);
}

#[test]
fn going_back_to_an_image_whose_decode_was_cancelled_loads_it() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 1);
    let back = dir.path().join("back.png");
    std::fs::copy(&files[0], &back).unwrap();
    let (p, rx, gate) = gated_pipeline("back", u64::MAX, 1);

    p.request(&back, &[]);
    gate.wait_for(1);
    p.request(&files[0], &[]); // cancels `back`, which keeps decoding
    ready_for(&rx, &files[0]);
    p.request(&back, &[]); // and back again before the cancelled decode has ended
    gate.open();
    ready_for(&rx, &back);
}

#[test]
fn prefetching_stays_within_the_budget_while_decoding() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 1);
    let gated: Vec<PathBuf> = (0..3)
        .map(|i| {
            let p = dir.path().join(format!("slow{i}.png"));
            std::fs::copy(&files[0], &p).unwrap();
            p
        })
        .collect();
    // room for two neighbours being decoded at once, not three
    let budget = IMAGE_BYTES * 5 / 2;
    let (p, rx, gate) = gated_pipeline("slow", budget, 3);
    p.request(&files[0], &gated);
    ready_for(&rx, &files[0]);
    let deadline = Instant::now() + Duration::from_secs(10);
    while p.stats().reserved_bytes != 2 * IMAGE_BYTES {
        assert!(Instant::now() < deadline, "{:?}", p.stats());
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(p.stats().reserved_bytes, 2 * IMAGE_BYTES, "the third waits for room");
    gate.open();
    settle(&p);
    assert_eq!(p.stats().reserved_bytes, 0);
    // the waiting one ran too (settled), and the cache kept what its budget allows, nearest first
    assert!(p.cached(&gated[0]).is_some());
    assert!(p.stats().used_bytes <= budget, "{:?}", p.stats());
}

#[test]
fn images_too_large_to_prefetch_wait_until_they_are_opened() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 2);
    let big = dir.path().join("big.png");
    image::RgbaImage::new(SIDE * 2, SIDE * 2).save(&big).unwrap();
    // a neighbour of four images' worth is over half of this budget, a small one is not
    let (p, rx) = pipeline(IMAGE_BYTES * 6, 2);
    p.request(&files[0], &[big.clone(), files[1].clone()]);
    ready_for(&rx, &files[0]);
    settle(&p);
    assert!(p.cached(&files[1]).is_some());
    assert!(p.cached(&big).is_none(), "not decoded in the background");
    // asked for again with the same neighbours: not retried
    p.request(&files[0], &[big.clone(), files[1].clone()]);
    assert_eq!(p.stats().queued, 0, "{:?}", p.stats());
    // opening it decodes it
    p.request(&big, &[]);
    ready_for(&rx, &big);
}

#[derive(Default)]
struct Gated {
    open: Arc<AtomicBool>,
    waiting: Arc<AtomicUsize>,
}

impl Gated {
    fn open(&self) {
        self.open.store(true, Ordering::Release);
    }

    /// Waits until `n` decodes are held at the gate.
    fn wait_for(&self, n: usize) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while self.waiting.load(Ordering::Acquire) < n {
            assert!(Instant::now() < deadline, "{n} decodes never reached the gate");
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// A failing test still lets the held decodes finish, so dropping the pipeline does not hang.
impl Drop for Gated {
    fn drop(&mut self) {
        self.open();
    }
}
