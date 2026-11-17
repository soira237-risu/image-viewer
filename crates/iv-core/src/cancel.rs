use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::{Error, Result};

/// A cheap, cloneable cancellation flag shared between the requester and a worker.
///
/// Decoders call [`CancelToken::check`] between stages; a cancelled job returns
/// [`Error::Cancelled`] instead of finishing work nobody will look at.
#[derive(Clone, Debug, Default)]
pub struct CancelToken(Arc<AtomicBool>);

impl CancelToken {
    pub fn new() -> Self {
        Self::default()
    }

    /// A token that is never cancelled, for callers that do not need cancellation.
    pub fn never() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    /// Returns `Err(Error::Cancelled)` once the token has been cancelled.
    pub fn check(&self) -> Result<()> {
        if self.is_cancelled() { Err(Error::Cancelled) } else { Ok(()) }
    }

    /// Whether both are clones of the same token.
    pub fn same(&self, other: &CancelToken) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clones_share_state() {
        let a = CancelToken::new();
        let b = a.clone();
        assert!(a.check().is_ok());
        b.cancel();
        assert!(a.is_cancelled());
        assert!(matches!(a.check(), Err(Error::Cancelled)));
    }
}
