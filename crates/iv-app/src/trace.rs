//! `--trace`: timestamps of startup and loading milestones, measured from process start.

use std::cell::RefCell;
use std::collections::HashSet;
use std::time::Instant;

pub struct Trace {
    start: Instant,
    enabled: bool,
    once: RefCell<HashSet<&'static str>>,
}

impl Trace {
    pub fn new(start: Instant, enabled: bool) -> Self {
        Self { start, enabled, once: RefCell::new(HashSet::new()) }
    }

    pub fn mark(&self, what: &str) {
        if self.enabled {
            eprintln!("[trace] {:9.1} ms  {what}", self.start.elapsed().as_secs_f64() * 1000.0);
        }
    }

    /// Like [`Trace::mark`], but only the first time for a given label; empty labels are ignored.
    pub fn mark_once(&self, what: &'static str) {
        if !what.is_empty() && self.once.borrow_mut().insert(what) {
            self.mark(what);
        }
    }
}
