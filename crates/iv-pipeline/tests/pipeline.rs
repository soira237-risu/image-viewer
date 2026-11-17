//! The decoding service against real files.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
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

/// Holds every decode (or, with `in_probe`, every header probe) of files whose name starts with
/// `prefix` until `open` is set (ignoring cancellation, like a decoder in the middle of a long
/// pixel decode), then fails so the next decoder (image-rs) reads the file for real.
struct Gate {
    info: iv_codec::DecoderInfo,
    prefix: &'static str,
    open: Arc<AtomicBool>,
    /// Decodes waiting at the gate.
    waiting: Arc<AtomicUsize>,
    /// Names of the files that reached the gate.
    held: Arc<Mutex<Vec<String>>>,
    in_probe: bool,
}

impl Gate {
    fn hold(&self, src: &Source) {
        self.held.lock().unwrap().push(src.display_name().to_string());
        self.waiting.fetch_add(1, Ordering::AcqRel);
        while !self.open.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.waiting.fetch_sub(1, Ordering::AcqRel);
    }
}

impl iv_codec::Decoder for Gate {
    fn info(&self) -> &iv_codec::DecoderInfo {
        &self.info
    }
    fn claims(&self, src: &Source, _: FormatId) -> iv_core::Result<bool> {
        Ok(src.display_name().starts_with(self.prefix))
    }
    fn probe(&self, src: &Source, _: FormatId) -> iv_core::Result<ImageInfo> {
        if self.in_probe {
            self.hold(src);
        }
        Err(iv_core::Error::decode("gate", "probe by the next decoder"))
    }
    fn decode(&self, src: &Source, _: FormatId, _: &CancelToken) -> iv_core::Result<iv_codec::Decoded> {
        if !self.in_probe {
            self.hold(src);
        }
        Err(iv_core::Error::decode("gate", "opened"))
    }
}

fn gated_pipeline(prefix: &'static str, budget: u64, workers: usize) -> (Pipeline, Receiver<PipelineEvent>, Gated) {
    gated_pipeline_at(prefix, false, budget, workers)
}

/// [`gated_pipeline`] holding the header probe instead of the decode when `in_probe` is set.
fn gated_pipeline_at(
    prefix: &'static str,
    in_probe: bool,
    budget: u64,
    workers: usize,
) -> (Pipeline, Receiver<PipelineEvent>, Gated) {
    let gated = Gated::default();
    let mut reg = iv_codec::default_registry();
    reg.register(Arc::new(Gate {
        info: iv_codec::DecoderInfo { id: "gate", name: "gate", formats: &[FormatId::Png], priority: 1000 },
        prefix,
        open: gated.open.clone(),
        waiting: gated.waiting.clone(),
        held: gated.held.clone(),
        in_probe,
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
fn a_held_key_keeps_every_image_it_passes_while_decoding_one_at_a_time() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 1);
    let passed: Vec<PathBuf> = (0..3)
        .map(|i| {
            let p = dir.path().join(format!("quick{i}.png"));
            std::fs::copy(&files[0], &p).unwrap();
            p
        })
        .collect();
    // free prefetch workers that could take the newer requests beside the slow decode
    let (p, rx, gate) = gated_pipeline("quick", u64::MAX, 3);

    p.request_quick(&passed[0]);
    gate.wait_for(1);
    // key repeats while the first image is still decoding
    p.request_quick(&passed[1]);
    p.request_quick(&passed[2]);
    p.request_quick(&files[0]);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(gate.waiting.load(Ordering::Acquire), 1, "one quick-browse image decodes at a time");
    assert_eq!(p.stats().in_flight, 1, "{:?}", p.stats());

    gate.open();
    // the image decoding when the key moved on is still shown, then the newest one
    ready_for(&rx, &passed[0]);
    ready_for(&rx, &files[0]);
    settle(&p);
    assert!(p.cached(&passed[0]).is_some(), "the passed image was kept");
    let more = readies(&rx);
    assert!(!more.contains(&passed[1]) && !more.contains(&passed[2]), "skipped requests are not decoded: {more:?}");
}

#[test]
fn quick_browsing_does_not_wait_for_an_abandoned_decode() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 2);
    let stuck = dir.path().join("hold.png");
    std::fs::copy(&files[0], &stuck).unwrap();
    let (p, rx, gate) = gated_pipeline("hold", u64::MAX, 1);

    p.request(&stuck, &[]);
    gate.wait_for(1);
    // a single key press leaves `stuck` (cancelled, still in its decoder), then the key is held
    p.request(&files[0], &[]);
    ready_for(&rx, &files[0]);
    p.request_quick(&files[1]);
    ready_for(&rx, &files[1]);
    gate.open();
    settle(&p);
    assert!(p.cached(&stuck).is_none(), "the abandoned decode stays abandoned");
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
    held: Arc<Mutex<Vec<String>>>,
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

/// Reads the whole file of names starting with `prefix` first, then holds the decode until
/// `open` is set, then decodes the bytes it read: like a decoder that has the old bytes in
/// memory while the file is written over.
struct Snapshot {
    info: iv_codec::DecoderInfo,
    prefix: &'static str,
    open: Arc<AtomicBool>,
    waiting: Arc<AtomicUsize>,
}

impl iv_codec::Decoder for Snapshot {
    fn info(&self) -> &iv_codec::DecoderInfo {
        &self.info
    }
    fn claims(&self, src: &Source, _: FormatId) -> iv_core::Result<bool> {
        Ok(src.display_name().starts_with(self.prefix))
    }
    fn probe(&self, _: &Source, _: FormatId) -> iv_core::Result<ImageInfo> {
        Err(iv_core::Error::decode("snapshot", "probe by the next decoder"))
    }
    fn decode(&self, src: &Source, _: FormatId, _: &CancelToken) -> iv_core::Result<iv_codec::Decoded> {
        let bytes = src.read_all()?;
        self.waiting.fetch_add(1, Ordering::AcqRel);
        while !self.open.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        self.waiting.fetch_sub(1, Ordering::AcqRel);
        let rgba = image::load_from_memory(&bytes).map_err(|e| iv_core::Error::decode("snapshot", e))?.to_rgba8();
        let (width, height) = rgba.dimensions();
        let info = ImageInfo {
            format: FormatId::Png,
            codec: "snapshot",
            width,
            height,
            frame_count: Some(1),
            has_alpha: false,
            bits_per_channel: 8,
            orientation: iv_core::Orientation::Normal,
            icc_profile: None,
            exif: None,
        };
        let image = iv_core::ImageBuf::new(width, height, iv_core::PixelFormat::Rgba8, rgba.into_raw())?;
        Ok(iv_codec::Decoded::still(info, image))
    }
}

#[test]
fn a_file_saved_over_while_it_decodes_is_decoded_again() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("edited.png");
    let old = image::Rgba([10, 0, 0, 255]);
    let new = image::Rgba([200, 0, 0, 255]);
    image::RgbaImage::from_pixel(SIDE, SIDE, old).save(&file).unwrap();
    let gate = Gated::default();
    let mut reg = iv_codec::default_registry();
    reg.register(Arc::new(Snapshot {
        info: iv_codec::DecoderInfo { id: "snapshot", name: "snapshot", formats: &[FormatId::Png], priority: 1000 },
        prefix: "edited",
        open: gate.open.clone(),
        waiting: gate.waiting.clone(),
    }));
    let (p, rx) = pipeline_with(reg, u64::MAX, 1, |_| {});
    // declared after the pipeline, so a failing assertion opens the gate before the pipeline
    // waits for its workers
    let gate = gate;

    // the decode has the old bytes in hand when a save in place writes the file
    p.request(&file, &[]);
    gate.wait_for(1);
    image::RgbaImage::from_pixel(SIDE, SIDE, new).save(&file).unwrap();
    p.invalidate(&file);
    // the viewer asks for the saved file again: a fresh decode starts beside the stale one
    p.request(&file, &[]);
    gate.wait_for(2);
    gate.open();
    let shown = ready_for(&rx, &file);
    assert_eq!(&shown.image.data()[..4], &new.0, "the saved pixels are announced, not the old ones");
    settle(&p);
    // the stale decode is done once it has given back its reservation
    let deadline = Instant::now() + Duration::from_secs(10);
    while p.stats().reserved_bytes != 0 {
        assert!(Instant::now() < deadline, "{:?}", p.stats());
        std::thread::sleep(Duration::from_millis(1));
    }
    std::thread::sleep(Duration::from_millis(50));
    assert!(readies(&rx).is_empty(), "the stale decode is not announced afterwards either");
    let cached = p.cached(&file).expect("the new decode is cached");
    assert_eq!(&cached.image.data()[..4], &new.0, "the cache holds the saved pixels");
}

/// Counts the decodes of files whose name starts with `prefix`, then lets the next decoder
/// (image-rs) decode them for real.
struct Counter {
    info: iv_codec::DecoderInfo,
    prefix: &'static str,
    decodes: Arc<AtomicUsize>,
}

impl iv_codec::Decoder for Counter {
    fn info(&self) -> &iv_codec::DecoderInfo {
        &self.info
    }
    fn claims(&self, src: &Source, _: FormatId) -> iv_core::Result<bool> {
        Ok(src.display_name().starts_with(self.prefix))
    }
    fn probe(&self, _: &Source, _: FormatId) -> iv_core::Result<ImageInfo> {
        Err(iv_core::Error::decode("counter", "probe by the next decoder"))
    }
    fn decode(&self, _: &Source, _: FormatId, _: &CancelToken) -> iv_core::Result<iv_codec::Decoded> {
        self.decodes.fetch_add(1, Ordering::AcqRel);
        Err(iv_core::Error::decode("counter", "counted"))
    }
}

#[test]
fn prefetching_never_decodes_what_the_budget_cannot_keep() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 14);
    let decodes = Arc::new(AtomicUsize::new(0));
    let mut reg = iv_codec::default_registry();
    reg.register(Arc::new(Counter {
        info: iv_codec::DecoderInfo { id: "counter", name: "counter", formats: &[FormatId::Png], priority: 1000 },
        prefix: "img",
        decodes: decodes.clone(),
    }));
    // the current image and two neighbours fit; the plan asks for four neighbours
    let budget = IMAGE_BYTES * 7 / 2;
    let (p, rx) = pipeline_with(reg, budget, 3, |_| {});
    let plan = |i: usize| -> Vec<PathBuf> { files[i + 1..i + 5].to_vec() };

    p.request(&files[0], &plan(0));
    ready_for(&rx, &files[0]);
    settle(&p);
    for i in 1..=8 {
        let before = decodes.load(Ordering::Acquire);
        assert!(p.request(&files[i], &plan(i)).is_some(), "step {i}: the next image was prefetched");
        settle(&p);
        assert_eq!(decodes.load(Ordering::Acquire) - before, 1, "step {i}: only the one new neighbour that fits");
        assert!(p.cached(&files[i + 1]).is_some() && p.cached(&files[i + 2]).is_some(), "step {i}");
        assert!(p.cached(&files[i + 3]).is_none(), "step {i}: one too many for the budget");
        assert!(p.stats().used_bytes <= budget, "step {i}: {:?}", p.stats());
    }
}

#[test]
fn the_cache_keeps_display_pixels_and_real_transparency() {
    let dir = tempfile::tempdir().unwrap();
    let deep = dir.path().join("deep.png");
    image::ImageBuffer::<image::Rgba<u16>, _>::from_fn(5, 3, |x, _| image::Rgba([x as u16 * 10000, 257, 65535, 65535]))
        .save(&deep)
        .unwrap();
    let opaque_rgba = dir.path().join("opaque.png");
    image::RgbaImage::from_pixel(4, 4, image::Rgba([1, 2, 3, 255])).save(&opaque_rgba).unwrap();
    let see_through = dir.path().join("alpha.png");
    image::RgbaImage::from_fn(4, 4, |x, _| image::Rgba([1, 2, 3, if x == 3 { 0 } else { 255 }]))
        .save(&see_through)
        .unwrap();
    let (p, rx) = pipeline(u64::MAX, 1);

    p.request(&deep, &[]);
    let d = ready_for(&rx, &deep);
    assert_eq!(d.image.format(), iv_core::PixelFormat::Rgba8, "8-bit for the screen");
    assert_eq!(d.memory_size(), 5 * 3 * 4, "the budget counts the 8-bit pixels");
    assert_eq!(d.info.bits_per_channel, 16, "the file's own depth is still reported");
    assert_eq!(&d.image.data()[4..8], &[39, 1, 255, 255], "rounded from 16 bits");

    p.request(&opaque_rgba, &[]);
    assert!(!ready_for(&rx, &opaque_rgba).info.has_alpha, "an RGBA file without transparency");
    p.request(&see_through, &[]);
    assert!(ready_for(&rx, &see_through).info.has_alpha);
}

/// Waits for the `Probed` event of `path`, failing on its `Ready` or `Failed` first.
fn probed_for(rx: &Receiver<PipelineEvent>, path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("an event") {
            PipelineEvent::Probed { path: p, .. } if p == path => return,
            PipelineEvent::Ready { path: p, .. } | PipelineEvent::Failed { path: p, .. } if p == path => {
                panic!("{} finished without being probed", p.display())
            }
            _ => {}
        }
    }
}

#[test]
fn an_image_too_big_to_prefetch_loads_when_the_user_moves_onto_it_while_it_is_prefetched() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 1);
    let big = dir.path().join("held-big.png");
    image::RgbaImage::new(SIDE * 2, SIDE * 2).save(&big).unwrap();
    // four images' worth: over half of this budget, so not kept as a neighbour
    let (p, rx, gate) = gated_pipeline_at("held", true, IMAGE_BYTES * 6, 1);

    p.request(&files[0], std::slice::from_ref(&big));
    ready_for(&rx, &files[0]);
    // its prefetch is still reading the header (an archive page being inflated, say) when the
    // user steps onto it: no second job starts for it
    gate.wait_for(1);
    p.request(&big, std::slice::from_ref(&files[0]));
    gate.open();
    probed_for(&rx, &big);
    let d = ready_for(&rx, &big);
    assert_eq!(d.image.width(), SIDE * 2);
    assert!(p.cached(&big).is_some(), "kept as the image on screen");
}

#[test]
fn an_image_waiting_for_room_to_prefetch_loads_at_once_when_the_user_moves_onto_it() {
    let dir = tempfile::tempdir().unwrap();
    let files = write_pngs(dir.path(), 1);
    let slow: Vec<PathBuf> = (0..3)
        .map(|i| {
            let p = dir.path().join(format!("slow{i}.png"));
            std::fs::copy(&files[0], &p).unwrap();
            p
        })
        .collect();
    // room for two neighbours being decoded at once, not three
    let budget = IMAGE_BYTES * 5 / 2;
    let (p, rx, gate) = gated_pipeline("slow", budget, 3);
    p.request(&files[0], &slow);
    ready_for(&rx, &files[0]);
    gate.wait_for(2);
    let deadline = Instant::now() + Duration::from_secs(10);
    while p.stats()
        != (iv_pipeline::Stats {
            cached: 1,
            used_bytes: IMAGE_BYTES,
            queued: 0,
            in_flight: 3,
            reserved_bytes: 2 * IMAGE_BYTES,
        })
    {
        assert!(Instant::now() < deadline, "{:?}", p.stats());
        std::thread::sleep(Duration::from_millis(1));
    }
    // the third neighbour waits for room in the budget; the user steps onto it
    let held = gate.held.lock().unwrap().clone();
    let waiting = slow.iter().find(|s| !held.contains(&s.file_name().unwrap().to_string_lossy().into_owned())).unwrap();
    let others: Vec<PathBuf> = slow.iter().filter(|s| *s != waiting).cloned().collect();
    p.request(waiting, &others);
    // it is announced and decoded now, beside the two neighbours still decoding
    probed_for(&rx, waiting);
    gate.wait_for(3);
    gate.open();
    ready_for(&rx, waiting);
    settle(&p);
}

/// CRC-32 (IEEE) of `bytes`, for hand-made PNG chunks.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            crc = if crc & 1 != 0 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
        }
    }
    !crc
}

/// A PNG whose header declares `width` x `height`, with an empty image data chunk.
fn png_header_only(width: u32, height: u32) -> Vec<u8> {
    let chunk = |kind: &[u8], data: &[u8]| {
        let mut c = (data.len() as u32).to_be_bytes().to_vec();
        c.extend_from_slice(kind);
        c.extend_from_slice(data);
        c.extend_from_slice(&crc32(&c[4..]).to_be_bytes());
        c
    };
    let mut ihdr = width.to_be_bytes().to_vec();
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit RGBA
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend(chunk(b"IHDR", &ihdr));
    // an empty zlib stream
    png.extend(chunk(b"IDAT", &[0x78, 0x9C, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01]));
    png.extend(chunk(b"IEND", &[]));
    png
}

#[test]
fn an_image_too_large_to_open_says_so() {
    let dir = tempfile::tempdir().unwrap();
    // 6.4 GB of RGBA: more than one image buffer may take
    let huge = dir.path().join("huge.png");
    std::fs::write(&huge, png_header_only(40_000, 40_000)).unwrap();
    let (p, rx) = pipeline(u64::MAX, 1);
    p.request(&huge, &[]);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())).expect("an event") {
            PipelineEvent::Failed { path, kind, error } if path == huge => {
                assert_eq!(kind, iv_pipeline::FailureKind::TooLarge { width: 40_000, height: 40_000 }, "{error}");
                break;
            }
            PipelineEvent::Ready { .. } => panic!("decoded"),
            _ => {}
        }
    }
}
