//! Background loading: probe → preview → full decode, each step reported as soon as it is done.
//! Starting a new load cancels the previous one; results carry a generation number so the UI can
//! drop anything that arrives late.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use iv_codec::{Decoded, Registry};
use iv_core::{CancelToken, Error, ImageBuf, ImageInfo, Source};

/// Longest side asked of a preview.
const PREVIEW_TARGET: u32 = 1920;

#[derive(Debug)]
pub enum LoadEvent {
    Probed { generation: u64, path: PathBuf, info: ImageInfo },
    Preview { generation: u64, image: ImageBuf },
    Decoded { generation: u64, decoded: Decoded, elapsed: Duration },
    Failed { generation: u64, path: PathBuf, error: String },
}

pub struct Loader {
    registry: Arc<Registry>,
    generation: u64,
    cancel: Option<CancelToken>,
}

impl Loader {
    pub fn new(registry: Arc<Registry>) -> Self {
        Self { registry, generation: 0, cancel: None }
    }

    pub fn current_generation(&self) -> u64 {
        self.generation
    }

    /// Starts loading `path` on a worker thread; `send` delivers each event (it must be
    /// callable from that thread). Returns the generation of this load.
    pub fn load(&mut self, path: PathBuf, send: impl Fn(LoadEvent) + Send + 'static) -> u64 {
        if let Some(previous) = self.cancel.take() {
            previous.cancel();
        }
        self.generation += 1;
        let generation = self.generation;
        let cancel = CancelToken::new();
        self.cancel = Some(cancel.clone());
        let registry = self.registry.clone();

        std::thread::Builder::new()
            .name(format!("load-{generation}"))
            .spawn(move || run(&registry, generation, path, &cancel, &send))
            .expect("spawn loader thread");
        generation
    }
}

fn run(registry: &Registry, generation: u64, path: PathBuf, cancel: &CancelToken, send: &dyn Fn(LoadEvent)) {
    let started = Instant::now();
    let src = Source::from_path(&path);
    let fail = |error: Error| {
        if !matches!(error, Error::Cancelled) {
            send(LoadEvent::Failed { generation, path: path.clone(), error: error.to_string() });
        }
    };

    match registry.probe(&src) {
        Ok(info) => send(LoadEvent::Probed { generation, path: path.clone(), info }),
        Err(e) => return fail(e),
    }
    if cancel.is_cancelled() {
        return;
    }
    // a missing or broken preview is not an error: the full decode follows
    if let Ok(Some(image)) = registry.preview(&src, PREVIEW_TARGET, cancel) {
        send(LoadEvent::Preview { generation, image });
    }
    match registry.decode(&src, cancel) {
        Ok(decoded) => send(LoadEvent::Decoded { generation, decoded, elapsed: started.elapsed() }),
        Err(e) => fail(e),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    fn png(dir: &std::path::Path, name: &str) -> PathBuf {
        let p = dir.join(name);
        image::RgbaImage::from_pixel(1, 1, image::Rgba([1, 2, 3, 4])).save(&p).unwrap();
        p
    }

    fn collect(loader: &mut Loader, path: PathBuf) -> Vec<LoadEvent> {
        let (tx, rx) = mpsc::channel();
        loader.load(path, move |e| tx.send(e).unwrap());
        rx.iter().collect() // the sender drops when the worker finishes
    }

    #[test]
    fn reports_probe_then_decode() {
        let dir = std::env::temp_dir().join(format!("iv-loader-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut loader = Loader::new(Arc::new(iv_codec::default_registry()));
        let events = collect(&mut loader, png(&dir, "one.png"));
        assert!(matches!(events.first(), Some(LoadEvent::Probed { generation: 1, .. })), "{events:?}");
        match events.last() {
            Some(LoadEvent::Decoded { generation: 1, decoded, .. }) => assert_eq!(decoded.image.width(), 1),
            other => panic!("expected Decoded, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_files_fail_with_a_message() {
        let mut loader = Loader::new(Arc::new(iv_codec::default_registry()));
        let events = collect(&mut loader, PathBuf::from("Z:/no/such/file.png"));
        assert!(matches!(events.as_slice(), [LoadEvent::Failed { generation: 1, .. }]), "{events:?}");
    }

    #[test]
    fn a_new_load_cancels_the_previous_one_and_bumps_the_generation() {
        let mut loader = Loader::new(Arc::new(iv_codec::default_registry()));
        let (tx, _rx) = mpsc::channel::<LoadEvent>();
        loader.load(PathBuf::from("Z:/a.png"), move |e| {
            let _ = tx.send(e);
        });
        let first = loader.cancel.clone().unwrap();
        loader.load(PathBuf::from("Z:/b.png"), |_| {});
        assert!(first.is_cancelled());
        assert_eq!(loader.current_generation(), 2);
    }
}
