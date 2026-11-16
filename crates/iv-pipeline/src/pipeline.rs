//! Decoding service: one foreground worker for the image the user is looking at, a pool of
//! background workers for neighbours, and a cache bounded by a memory budget.
//!
//! The cache is the only owner of prefetched images (the viewer holds an `Arc` to the one on
//! screen). Every decode result is either inserted into the cache under the budget or dropped,
//! so nothing decoded can escape the accounting.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use iv_codec::{Decoded, Registry};
use iv_core::{CancelToken, Error, ImageBuf, ImageInfo};

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
    /// Anything else; the error text has the detail.
    Other,
}

impl FailureKind {
    pub fn of(e: &Error) -> Self {
        match e {
            Error::UnsupportedFormat => FailureKind::Unsupported,
            Error::TooLarge { width, height, .. } => FailureKind::TooLarge { width: *width, height: *height },
            Error::Io(io) if io.kind() == std::io::ErrorKind::NotFound => FailureKind::NotFound,
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
    in_flight: HashMap<PathBuf, CancelToken>,
    failed: HashSet<PathBuf>,
    tick: u64,
    shutdown: bool,
    /// Path the foreground worker is decoding right now.
    foreground_running: Option<PathBuf>,
    /// Quick browsing: the running foreground decode is allowed to finish and is announced even
    /// if the user has already moved past it, so held keys show images as fast as they decode.
    quick: bool,
}

/// Snapshot for tests, benchmarks and diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Stats {
    pub cached: usize,
    pub used_bytes: u64,
    pub queued: usize,
    pub in_flight: usize,
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

    fn stats(&self) -> Stats {
        Stats {
            cached: self.cache.len(),
            used_bytes: self.used_bytes,
            queued: self.foreground.len() + self.background.len(),
            in_flight: self.in_flight.len(),
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
            let job = match kind {
                Kind::Foreground => st.foreground.pop_front(),
                Kind::Prefetch => st.background.pop_front(),
            };
            if let Some(job) = job {
                let token = CancelToken::new();
                st.in_flight.insert(job.path.clone(), token.clone());
                if job.kind == Kind::Foreground {
                    st.foreground_running = Some(job.path.clone());
                }
                return Some((job, token));
            }
            st = self.wake.wait(st).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn run(&self, job: Job, cancel: CancelToken) {
        let started = Instant::now();
        let result = (|| {
            let src = crate::archive::source_for(&job.path)?;
            if job.kind == Kind::Foreground {
                let info = self.registry.probe(&src)?;
                (self.sink)(PipelineEvent::Probed { path: job.path.clone(), info });
                cancel.check()?;
                // a broken or missing preview is not an error: the decode follows
                if let Ok(Some(image)) = self.registry.preview(&src, PREVIEW_TARGET, &cancel) {
                    (self.sink)(PipelineEvent::Preview { path: job.path.clone(), image });
                }
            }
            self.registry.decode(&src, &cancel)
        })();

        let event = {
            let mut st = self.lock();
            st.in_flight.remove(&job.path);
            if job.kind == Kind::Foreground {
                st.foreground_running = None;
            }
            // a quick-browse frame the user has already passed is still worth showing
            let quick_frame = job.kind == Kind::Foreground && st.quick;
            match result {
                Ok(decoded) => {
                    let decoded = Arc::new(decoded);
                    let bytes = decoded.memory_size() as u64;
                    let is_current = st.plan.first() == Some(&job.path);
                    if st.plan.contains(&job.path) || quick_frame {
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
        if let Some(e) = event {
            (self.sink)(e);
        }
    }
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

    /// Quick browsing (a navigation key is held): no prefetching, and the image the foreground
    /// worker is decoding is finished and announced instead of cancelled. The newest request is
    /// decoded next, so the screen keeps up with the key at whatever pace decoding allows.
    pub fn request_quick(&self, current: &Path) -> Option<Arc<Decoded>> {
        self.request_inner(current, &[], true)
    }

    fn request_inner(&self, current: &Path, prefetch: &[PathBuf], quick: bool) -> Option<Arc<Decoded>> {
        let mut st = self.shared.lock();
        st.quick = quick;
        st.plan.clear();
        st.plan.push(current.to_path_buf());
        st.plan.extend(prefetch.iter().filter(|p| p.as_path() != current).cloned());

        let keep_running = if quick { st.foreground_running.clone() } else { None };
        for (path, token) in &st.in_flight {
            if !st.plan.contains(path) && Some(path) != keep_running.as_ref() {
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
        if hit.is_none() && !st.in_flight.contains_key(current) {
            // an explicit request retries a file that failed before
            st.failed.remove(current);
            st.foreground.push_back(Job { path: current.to_path_buf(), kind: Kind::Foreground });
        }
        let todo: Vec<PathBuf> = st.plan[1..]
            .iter()
            .filter(|p| !st.cache.contains_key(*p) && !st.in_flight.contains_key(*p) && !st.failed.contains(*p))
            .cloned()
            .collect();
        st.background.extend(todo.into_iter().map(|path| Job { path, kind: Kind::Prefetch }));
        st.enforce_budget(self.shared.config.budget_bytes, Some(current));
        drop(st);
        self.shared.wake.notify_all();
        hit
    }

    /// Forgets a cached decode, e.g. after the file was saved over.
    pub fn invalidate(&self, path: &Path) {
        let mut st = self.shared.lock();
        if let Some(e) = st.cache.remove(path) {
            st.used_bytes -= e.bytes;
        }
        st.failed.remove(path);
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
        assert_eq!(FailureKind::of(&Error::decode("x", "bad huffman table")), FailureKind::Other);
    }
}
