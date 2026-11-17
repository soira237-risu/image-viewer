//! Turning panics in third-party code into ordinary errors.
//!
//! Decoders, encoders and archive readers run on untrusted files; a malformed one can make a
//! dependency panic. [`catch`] confines such a panic to the one operation: it becomes
//! [`Error::Panicked`] and the thread that ran it (a decode worker, the save thread, a batch
//! conversion) carries on.
//!
//! A panic hook still runs for a caught panic, before the unwinding reaches [`catch`]. A hook
//! that reports crashes asks [`is_caught`] to tell a contained failure from a fatal one.

use std::any::Any;
use std::cell::Cell;
use std::panic::{AssertUnwindSafe, catch_unwind};

use crate::{Error, Result};

thread_local! {
    /// How many [`catch`] calls are active on this thread.
    static CATCHING: Cell<u32> = const { Cell::new(0) };
}

/// Restores the depth when [`catch`] returns or unwinds.
struct Depth;

impl Depth {
    fn enter() -> Self {
        CATCHING.with(|c| c.set(c.get() + 1));
        Depth
    }
}

impl Drop for Depth {
    fn drop(&mut self) {
        CATCHING.with(|c| c.set(c.get().saturating_sub(1)));
    }
}

/// Whether a panic raised on this thread right now would be caught by [`catch`]. Meant for
/// panic hooks: such a panic is reported as a failed operation, not as a crash.
pub fn is_caught() -> bool {
    CATCHING.with(|c| c.get() > 0)
}

/// The message a panic was raised with (`panic!("...")` gives a `&str` or a `String`).
pub fn message(payload: &(dyn Any + Send)) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "unknown panic".to_string()
    }
}

/// Runs `f`; a panic inside it becomes [`Error::Panicked`] naming `what` (e.g. a codec id).
///
/// `f` must not leave shared state half-updated when it panics: everything it touches is either
/// its own or dropped with it (decoders keep their state on the stack). Allocation failures and
/// stack overflows abort the process and are not caught.
pub fn catch<T>(what: &'static str, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let depth = Depth::enter();
    let result = catch_unwind(AssertUnwindSafe(f));
    drop(depth);
    result.unwrap_or_else(|payload| Err(Error::Panicked { what, message: message(payload.as_ref()) }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_panic_becomes_an_error_and_the_thread_goes_on() {
        assert!(!is_caught());
        let r: Result<u32> = catch("test-codec", || {
            assert!(is_caught());
            panic!("bad huffman table {}", 7)
        });
        match r {
            Err(Error::Panicked { what, message }) => {
                assert_eq!(what, "test-codec");
                assert_eq!(message, "bad huffman table 7");
            }
            other => panic!("{other:?}"),
        }
        assert!(!is_caught(), "depth restored after the unwind");
        let r: Result<u32> = catch("x", || panic!("static message"));
        assert!(r.unwrap_err().to_string().contains("static message"));
    }

    #[test]
    fn results_pass_through_and_nesting_keeps_count() {
        assert_eq!(catch("a", || catch("b", || Ok(5))).unwrap(), 5);
        assert!(matches!(catch::<()>("a", || Err(Error::Cancelled)), Err(Error::Cancelled)));
        assert!(!is_caught());
    }
}
