//! Decoding service: one foreground worker for the image the user is looking at, a pool of
//! background workers for neighbours, and a cache bounded by a memory budget.
//!
//! The cache is the only owner of prefetched images (the viewer holds an `Arc` to the one on
//! screen). Every decode result is either inserted into the cache under the budget or dropped,
//! so nothing decoded can escape the accounting.
//!
//! The cache holds display-ready pixels: 8-bit RGBA (16-bit and float decodes are converted on
//! the worker, in parallel, so showing one is not a full-image pass on the UI thread and the
//! budget counts what is actually kept), and `info.has_alpha` only when some pixel is not fully
//! opaque (an opaque RGBA file is drawn without alpha blending). Saving decodes the file again
//! at full precision, so nothing is lost.
//!
//! Prefetching only starts the neighbours that fit in the budget together with the current
//! image (sizes of decoded images are remembered; unknown ones are estimated from the last
//! decode), and a neighbour whose result would only push out nearer images is not kept: no
//! decode is thrown away and repeated on every step through large images.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use iv_codec::{Decoded, Registry};
use iv_core::{CancelToken, Error, FormatId, ImageBuf, ImageInfo, PixelFormat};
use rayon::prelude::*;

/// Longest side asked of a preview.
const PREVIEW_TARGET: u32 = 1920;

#[derive(Clone, Debug)]
pub struct PipelineConfig {
    /// Background (prefetch) worker threads.
    pub prefetch_workers: usize,
    /// Upper bound for decoded pixels held in the cache.
    pub budget_bytes: u64,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
        Self { prefetch_workers: (cores.saturating_sub(2)).clamp(1, 3), budget_bytes: 1024 * 1024 * 1024 }
    }
}

#[derive(Debug)]
pub enum PipelineEvent {
    /// Header of the requested image (sent only for foreground loads).
    Probed { path: PathBuf, info: ImageInfo },
    /// Fast stand-in for the requested image (sent only for foreground loads, when available).
    Preview { path: PathBuf, image: ImageBuf },
    /// A decode finished (foreground or prefetch). `elapsed` covers the whole job.
    Ready { path: PathBuf, decoded: Arc<Decoded>, elapsed: Duration },
    Failed {
        path: PathBuf,
        /// The error as text (English, for logs and as detail).
        error: String,
        /// What kind of failure, so the viewer can word the common ones in its own language.
        kind: FailureKind,
    },
}

/// The broad reason a decode failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FailureKind {
    /// No codec recognised the data.
    Unsupported,
    /// The image needs more memory than one buffer may take.
    TooLarge { width: u32, height: u32 },
    /// The file does not exist (any more).
    NotFound,
    /// The format needs an optional system codec that is not installed.
    CodecMissing { format: FormatId },
    /// Anything else; the error text has the detail.
    Other,
}

impl FailureKind {
    pub fn of(e: &Error) -> Self {
        match e {
            Error::UnsupportedFormat => FailureKind::Unsupported,
            Error::TooLarge { width, height, .. } => FailureKind::TooLarge { width: *width, height: *height },
            Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound => FailureKind::NotFound,
            Error::CodecMissing { format } => FailureKind::CodecMissing { format: *format },
            _ => FailureKind::Other,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Foreground,
    Prefetch,
}

struct Job {
    path: PathBuf,
    kind: Kind,
}

struct Entry {
    decoded: Arc<Decoded>,
    bytes: u64,
    last_used: u64,
}

#[derive(Default)]
struct State {
    cache: HashMap<PathBuf, Entry>,
    used_bytes: u64,
    /// Current image first, then prefetch targets nearest-first.
    plan: Vec<PathBuf>,
    foreground: VecDeque<Job>,
    background: VecDeque<Job>,
    /// Decodes running, with the token of the newest job per path. A cancelled job keeps running
    /// until its decoder next checks the token; asking for its path again starts a new job.
    in_flight: HashMap<PathBuf, CancelToken>,
    /// Running decodes of files written since they started (see [`Pipeline::invalidate`]): they
    /// may have read the old bytes, so whatever they produce is dropped, never cached or
    /// announced.
    stale: Vec<CancelToken>,
    failed: HashSet<PathBuf>,
    /// Images whose decoded size is too large to prefetch (more than half the budget): they are
    /// decoded only when the user opens them.
    too_big_to_prefetch: HashSet<PathBuf>,
    /// Pixel bytes the running decodes are expected to produce (from their headers). Prefetch
    /// decodes wait while starting one would take this over the budget, so neighbours cannot
    /// pile up gigabytes of decoding at once.
    reserved_bytes: u64,
    tick: u64,
    shutdown: bool,
    /// Foreground decodes running, oldest first, with their tokens. A cancelled one may still be
    /// finishing on one worker while the newest runs on another.
    foreground_running: Vec<(PathBuf, CancelToken)>,
    /// Quick browsing: running foreground decodes are allowed to finish and are announced even
    /// if the user has already moved past them, so held keys show images as fast as they decode.
    /// Only one of them runs at a time (see [`State::foreground_may_start`]).
    quick: bool,
    /// Cached size of images decoded before (kept for the plan and the cache only), so
    /// prefetching knows which neighbours fit.
    sizes: HashMap<PathBuf, u64>,
    /// Cached size of the last image decoded: the estimate for neighbours not decoded yet.
    typical_bytes: u64,
}

/// Snapshot for tests, benchmarks and diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stats {
    pub cached: usize,
    pub used_bytes: u64,
    pub queued: usize,
    pub in_flight: usize,
    /// Pixel bytes the running decodes are expected to produce.
    pub reserved_bytes: u64,
}

type Sink = Box<dyn Fn(PipelineEvent) + Send + Sync>;

struct Shared {
    registry: Arc<Registry>,
    config: PipelineConfig,
    state: Mutex<State>,
    wake: Condvar,
    sink: Sink,
}

pub struct Pipeline {
    shared: Arc<Shared>,
    workers: Vec<JoinHandle<()>>,
}

impl State {
    /// Evicts until `used_bytes <= budget`: least-recently-used entries outside the plan first,
    /// then plan entries from the far end. `keep` is never evicted.
    fn enforce_budget(&mut self, budget: u64, keep: Option<&Path>) {
        if self.used_bytes <= budget {
            return;
        }
        let mut outside: Vec<(u64, PathBuf)> = self
            .cache
            .iter()
            .filter(|(p, _)| !self.plan.contains(p) && Some(p.as_path()) != keep)
            .map(|(p, e)| (e.last_used, p.clone()))
            .collect();
        outside.sort();
        let far_plan: Vec<PathBuf> = self
            .plan
            .iter()
            .rev()
            .filter(|p| self.cache.contains_key(*p) && Some(p.as_path()) != keep)
            .cloned()
            .collect();
        for victim in outside.into_iter().map(|(_, p)| p).chain(far_plan) {
            if self.used_bytes <= budget {
                break;
            }
            if let Some(e) = self.cache.remove(&victim) {
                self.used_bytes -= e.bytes;
            }
        }
    }

    /// Whether a queued foreground job may start now. Normally always. While quick browsing, only
    /// once no live (uncancelled) foreground decode is running: that decode is kept and shown when
    /// it ends, and the newest request starts right after it. Starting the next one beside it
    /// would split the CPU between images the user is flipping past and could show them out of
    /// order. A cancelled decode that is still finishing does not hold the next one back.
    fn foreground_may_start(&self) -> bool {
        !self.quick || self.foreground_running.iter().all(|(_, t)| t.is_cancelled())
    }

    /// Expected cached size of `path`: known when it is cached or was decoded before, else the
    /// size of the last decode (0 before the first one: no limit yet).
    fn estimate(&self, path: &Path) -> u64 {
        self.cache.get(path).map(|e| e.bytes).or_else(|| self.sizes.get(path).copied()).unwrap_or(self.typical_bytes)
    }

    /// Whether a prefetched `path` of `bytes` can be kept within `budget` by evicting only
    /// images outside the plan or farther in it than `path` (never nearer ones, never the
    /// current image). Otherwise keeping it would push out an image the user reaches first,
    /// which would then be decoded again.
    fn has_room_for(&self, path: &Path, bytes: u64, budget: u64) -> bool {
        let Some(pos) = self.plan.iter().position(|p| p == path) else { return false };
        let replaced = self.cache.get(path).map_or(0, |e| e.bytes);
        let evictable: u64 = self
            .cache
            .iter()
            .filter(|(p, _)| p.as_path() != path)
            .filter(|(p, _)| self.plan.iter().position(|q| q == *p).is_none_or(|q| q > pos))
            .map(|(_, e)| e.bytes)
            .sum();
        (self.used_bytes - replaced).saturating_add(bytes) <= budget.saturating_add(evictable)
    }

    fn stats(&self) -> Stats {
        Stats {
            cached: self.cache.len(),
            used_bytes: self.used_bytes,
            queued: self.foreground.len() + self.background.len(),
            in_flight: self.in_flight.len(),
            reserved_bytes: self.reserved_bytes,
        }
    }
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        // a worker panicking while holding the lock leaves consistent-enough state for a viewer
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn next_job(&self, kind: Kind) -> Option<(Job, CancelToken)> {
        let mut st = self.lock();
        loop {
            if st.shutdown {
                return None;
            }
            // a prefetch worker takes the image on screen first: the foreground worker may still
            // be finishing a decode the user has already left (decoders only stop between stages)
            let foreground = if st.foreground_may_start() { st.foreground.pop_front() } else { None };
            let job = match kind {
                Kind::Foreground => foreground,
                Kind::Prefetch => foreground.or_else(|| st.background.pop_front()),
            };
            if let Some(job) = job {
                let token = CancelToken::new();
                st.in_flight.insert(job.path.clone(), token.clone());
                if job.kind == Kind::Foreground {
                    st.foreground_running.push((job.path.clone(), token.clone()));
                }
                return Some((job, token));
            }
            st = self.wake.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Counts `bytes` as being decoded. A prefetch first waits until the other running decodes
    /// leave room for it in the budget (one always may run); the image on screen never waits.
    fn reserve(&self, bytes: u64, kind: Kind, cancel: &CancelToken) -> Result<u64, Error> {
        let mut st = self.lock();
        while kind == Kind::Prefetch
            && st.reserved_bytes > 0
            && st.reserved_bytes.saturating_add(bytes) > self.config.budget_bytes
        {
            if cancel.is_cancelled() || st.shutdown {
                return Err(Error::Cancelled);
            }
            // cancellation does not signal the condvar; look again now and then
            st = self.wake.wait_timeout(st, Duration::from_millis(50)).unwrap_or_else(|e| e.into_inner()).0;
        }
        st.reserved_bytes += bytes;
        Ok(bytes)
    }

    fn run(&self, job: Job, cancel: CancelToken) {
        let started = Instant::now();
        let kind = job.kind;
        let mut reserved = 0;
        // Decoders and archive readers run on untrusted files: a panic in one of them fails this
        // image, and the worker, its bookkeeping below and every later image carry on.
        let result = iv_core::panic::catch("image reader", || {
            let src = crate::archive::source_for_cancellable(&job.path, &cancel)?;
            let info = self.registry.probe(&src)?;
            let bytes = expected_bytes(&info);
            // a header read just before the file was written over is not announced
            cancel.check()?;
            if job.kind == Kind::Foreground {
                (self.sink)(PipelineEvent::Probed { path: job.path.clone(), info });
            } else if cached_bytes(&info) > self.config.budget_bytes / 2 {
                return Ok(None);
            }
            reserved = self.reserve(bytes, job.kind, &cancel)?;
            cancel.check()?;
            if job.kind == Kind::Foreground {
                // a broken or missing preview is not an error: the decode follows
                if let Ok(Some(image)) = self.registry.preview(&src, PREVIEW_TARGET, &cancel) {
                    (self.sink)(PipelineEvent::Preview { path: job.path.clone(), image: to_display(image) });
                }
            }
            let decoded = self.registry.decode(&src, &cancel)?;
            drop(src);
            cancel.check()?;
            Ok(Some(display_ready(decoded)))
        });

        let event = {
            let mut st = self.lock();
            st.reserved_bytes -= reserved;
            // a newer job for the same path (asked for again after this one was cancelled) keeps
            // its entry
            if st.in_flight.get(&job.path).is_some_and(|t| t.same(&cancel)) {
                st.in_flight.remove(&job.path);
            }
            if job.kind == Kind::Foreground
                && let Some(i) = st.foreground_running.iter().position(|(_, t)| t.same(&cancel))
            {
                st.foreground_running.remove(i);
            }
            // the file was written while this decode ran: its pixels may be the old ones
            let result = match st.stale.iter().position(|t| t.same(&cancel)) {
                Some(i) => {
                    st.stale.swap_remove(i);
                    Err(Error::Cancelled)
                }
                None => result,
            };
            // a quick-browse frame the user has already passed is still worth showing
            let quick_frame = job.kind == Kind::Foreground && st.quick;
            match result {
                Ok(None) => {
                    st.too_big_to_prefetch.insert(job.path);
                    None
                }
                Ok(Some(decoded)) => {
                    let decoded = Arc::new(decoded);
                    let bytes = decoded.memory_size() as u64;
                    let is_current = st.plan.first() == Some(&job.path);
                    st.sizes.insert(job.path.clone(), bytes);
                    st.typical_bytes = bytes;
                    let keep = match job.kind {
                        Kind::Foreground => st.plan.contains(&job.path) || quick_frame,
                        Kind::Prefetch => st.has_room_for(&job.path, bytes, self.config.budget_bytes),
                    };
                    if keep {
                        st.tick += 1;
                        let last_used = st.tick;
                        if let Some(old) =
                            st.cache.insert(job.path.clone(), Entry { decoded: decoded.clone(), bytes, last_used })
                        {
                            st.used_bytes -= old.bytes;
                        }
                        st.used_bytes += bytes;
                        let current = st.plan.first().cloned();
                        st.enforce_budget(self.config.budget_bytes, current.as_deref());
                    }
                    // a prefetch result nobody can keep is not worth announcing
                    (is_current || quick_frame || st.cache.contains_key(&job.path)).then(|| PipelineEvent::Ready {
                        path: job.path,
                        decoded,
                        elapsed: started.elapsed(),
                    })
                }
                Err(Error::Cancelled) => None,
                Err(e) => {
                    st.failed.insert(job.path.clone());
                    Some(PipelineEvent::Failed { path: job.path, error: e.to_string(), kind: FailureKind::of(&e) })
                }
            }
        };
        // freed budget lets a waiting prefetch start; a finished foreground decode lets the next
        // quick-browse image start
        if reserved > 0 || kind == Kind::Foreground {
            self.wake.notify_all();
        }
        if let Some(e) = event {
            (self.sink)(e);
        }
    }
}

/// Pixel memory a decode of `info` is expected to take while it runs: RGBA at the precision the
/// decoder produces (8, 16 or 32-bit float channels), every frame of a known animation counted.
fn expected_bytes(info: &ImageInfo) -> u64 {
    let pixel = match info.bits_per_channel {
        0..=8 => 4,
        9..=16 => 8,
        _ => 16,
    };
    frames_of(info).saturating_mul(pixel)
}

/// Pixel memory the cache keeps for `info`: 8-bit RGBA (see [`display_ready`]).
fn cached_bytes(info: &ImageInfo) -> u64 {
    frames_of(info).saturating_mul(4)
}

/// Pixels of every frame of `info` (one when the count is unknown).
fn frames_of(info: &ImageInfo) -> u64 {
    let frames = info.frame_count.unwrap_or(1).max(1) as u64;
    (info.width as u64).saturating_mul(info.height as u64).saturating_mul(frames)
}

/// Pixels per parallel work item when converting.
const CHUNK_PIXELS: usize = 64 * 1024;

/// `img` as 8-bit RGBA: shared as it is when it already is, else converted in parallel.
fn to_display(img: ImageBuf) -> ImageBuf {
    if img.format() == PixelFormat::Rgba8 {
        return img;
    }
    ImageBuf::with_samples::<u8>(img.width(), img.height(), PixelFormat::Rgba8, |out| {
        out.par_chunks_mut(CHUNK_PIXELS * 4).enumerate().for_each(|(i, chunk)| img.rgba8_into(i * CHUNK_PIXELS, chunk));
    })
    .expect("an 8-bit copy is smaller than the image")
}

/// Whether every pixel of an RGBA8 image is fully opaque.
fn is_opaque(img: &ImageBuf) -> bool {
    debug_assert_eq!(img.format(), PixelFormat::Rgba8);
    img.data().par_chunks(CHUNK_PIXELS * 4).all(|c| c.as_chunks::<4>().0.iter().all(|p| p[3] == u8::MAX))
}

/// What the cache keeps of a decode: the pixels (every frame) as 8-bit RGBA, and `has_alpha`
/// cleared when no pixel of any frame is transparent.
fn display_ready(mut d: Decoded) -> Decoded {
    match &mut d.animation {
        Some(anim) => {
            for f in &mut anim.frames {
                f.image = to_display(f.image.clone());
            }
            // the first frame is the still image: keep sharing it
            d.image = anim.frames[0].image.clone();
        }
        None => d.image = to_display(d.image.clone()),
    }
    if d.info.has_alpha {
        let opaque = match &d.animation {
            Some(anim) => anim.frames.iter().all(|f| is_opaque(&f.image)),
            None => is_opaque(&d.image),
        };
        d.info.has_alpha = !opaque;
    }
    d
}

impl Pipeline {
    /// Starts the workers. `sink` receives every event, on worker threads.
    pub fn new(
        registry: Arc<Registry>,
        config: PipelineConfig,
        sink: impl Fn(PipelineEvent) + Send + Sync + 'static,
    ) -> Self {
        let shared = Arc::new(Shared {
            registry,
            config: config.clone(),
            state: Mutex::new(State::default()),
            wake: Condvar::new(),
            sink: Box::new(sink),
        });
        let mut workers = Vec::new();
        let kinds =
            std::iter::once(Kind::Foreground).chain(std::iter::repeat_n(Kind::Prefetch, config.prefetch_workers));
        for (i, kind) in kinds.enumerate() {
            let shared = shared.clone();
            let name = if kind == Kind::Foreground { "decode-fg".to_string() } else { format!("decode-bg{i}") };
            workers.push(
                std::thread::Builder::new()
                    .name(name)
                    .spawn(move || {
                        while let Some((job, token)) = shared.next_job(kind) {
                            shared.run(job, token);
                        }
                    })
                    .expect("spawn decode worker"),
            );
        }
        Self { shared, workers }
    }

    /// Makes `current` the image on screen and `prefetch` (nearest first) its neighbours.
    ///
    /// Returns the decoded image immediately on a cache hit; otherwise a foreground load starts
    /// (unless one is already running for that path) and its result arrives as an event. Work
    /// for images no longer in the plan is cancelled.
    pub fn request(&self, current: &Path, prefetch: &[PathBuf]) -> Option<Arc<Decoded>> {
        self.request_inner(current, prefetch, false)
    }

    /// Quick browsing (a navigation key is held): no prefetching, and the image being decoded is
    /// finished and announced instead of cancelled. One image decodes at a time and the newest
    /// request is decoded next, so the screen keeps up with the key at whatever pace decoding
    /// allows.
    pub fn request_quick(&self, current: &Path) -> Option<Arc<Decoded>> {
        self.request_inner(current, &[], true)
    }

    fn request_inner(&self, current: &Path, prefetch: &[PathBuf], quick: bool) -> Option<Arc<Decoded>> {
        let mut st = self.shared.lock();
        st.quick = quick;
        st.plan.clear();
        st.plan.push(current.to_path_buf());
        st.plan.extend(prefetch.iter().filter(|p| p.as_path() != current).cloned());

        // quick browsing keeps every running foreground decode: each is an image flipped past that
        // is shown when it ends (at most one of them is live, see `foreground_may_start`)
        for (path, token) in &st.in_flight {
            let kept = quick && st.foreground_running.iter().any(|(_, t)| t.same(token));
            if !st.plan.contains(path) && !kept {
                token.cancel();
            }
        }
        st.foreground.clear();
        st.background.clear();

        st.tick += 1;
        let tick = st.tick;
        let hit = st.cache.get_mut(current).map(|e| {
            e.last_used = tick;
            e.decoded.clone()
        });
        // a decode of this image that was cancelled when the user left it will not be announced
        let running = |st: &State, p: &Path| st.in_flight.get(p).is_some_and(|t| !t.is_cancelled());
        if hit.is_none() && !running(&st, current) {
            // an explicit request retries a file that failed before
            st.failed.remove(current);
            st.foreground.push_back(Job { path: current.to_path_buf(), kind: Kind::Foreground });
        }
        // remembered sizes are only needed for what may be asked for or evicted next
        let State { sizes, plan, cache, .. } = &mut *st;
        sizes.retain(|p, _| cache.contains_key(p) || plan.contains(p));
        // neighbours nearest first, as far as they fit in the budget beside the current image:
        // one that does not fit would only be decoded to be thrown away (and decoded again on the
        // next step)
        let budget = self.shared.config.budget_bytes;
        let mut planned = st.estimate(current);
        let mut todo: Vec<PathBuf> = Vec::new();
        for p in &st.plan[1..] {
            if st.too_big_to_prefetch.contains(p) || st.failed.contains(p) {
                continue;
            }
            planned = planned.saturating_add(st.estimate(p));
            if planned > budget {
                break;
            }
            if !st.cache.contains_key(p) && !running(&st, p) {
                todo.push(p.clone());
            }
        }
        st.background.extend(todo.into_iter().map(|path| Job { path, kind: Kind::Prefetch }));
        st.enforce_budget(self.shared.config.budget_bytes, Some(current));
        drop(st);
        self.shared.wake.notify_all();
        hit
    }

    /// Forgets everything known about `path`, e.g. after the file was saved over: its cached
    /// decode, and a decode of it still running (which may have read the old bytes) is cancelled
    /// and its result dropped, so the next request decodes the file anew.
    pub fn invalidate(&self, path: &Path) {
        let mut st = self.shared.lock();
        if let Some(e) = st.cache.remove(path) {
            st.used_bytes -= e.bytes;
        }
        if let Some(token) = st.in_flight.remove(path) {
            token.cancel();
            st.stale.push(token);
        }
        st.failed.remove(path);
        st.too_big_to_prefetch.remove(path);
        st.sizes.remove(path);
    }

    pub fn cached(&self, path: &Path) -> Option<Arc<Decoded>> {
        self.shared.lock().cache.get(path).map(|e| e.decoded.clone())
    }

    pub fn stats(&self) -> Stats {
        self.shared.lock().stats()
    }

    /// Paths currently held in the cache.
    pub fn cached_paths(&self) -> Vec<PathBuf> {
        self.shared.lock().cache.keys().cloned().collect()
    }

    pub fn budget_bytes(&self) -> u64 {
        self.shared.config.budget_bytes
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        {
            let mut st = self.shared.lock();
            st.shutdown = true;
            for token in st.in_flight.values() {
                token.cancel();
            }
        }
        self.shared.wake.notify_all();
        for w in self.workers.drain(..) {
            let _ = w.join();
        }
    }
}

#[cfg(test)]
mod failure_kind_tests {
    use super::*;

    #[test]
    fn errors_map_to_kinds() {
        assert_eq!(FailureKind::of(&Error::UnsupportedFormat), FailureKind::Unsupported);
        let big = Error::TooLarge { width: 70000, height: 70000, bytes: 1 << 40 };
        assert_eq!(FailureKind::of(&big), FailureKind::TooLarge { width: 70000, height: 70000 });
        let gone = Error::Io(std::io::Error::from(std::io::ErrorKind::NotFound));
        assert_eq!(FailureKind::of(&gone), FailureKind::NotFound);
        let heif = Error::CodecMissing { format: FormatId::Heif };
        assert_eq!(FailureKind::of(&heif), FailureKind::CodecMissing { format: FormatId::Heif });
        assert_eq!(FailureKind::of(&Error::decode("x", "bad huffman table")), FailureKind::Other);
    }
}
