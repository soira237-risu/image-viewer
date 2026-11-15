//! `--trace`: timestamps of startup, loading and navigation milestones, from process start.

use std::time::Instant;

pub struct Trace {
    start: Instant,
    enabled: bool,
}

impl Trace {
    pub fn new(start: Instant, enabled: bool) -> Self {
        Self { start, enabled }
    }

    pub fn mark(&self, what: &str) {
        if self.enabled {
            eprintln!("[trace] {:9.1} ms  {what}", self.start.elapsed().as_secs_f64() * 1000.0);
        }
    }
}
