use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// A cheap, cloneable cancellation flag shared between the UI and engine workers.
///
/// Every long-running engine operation (index build, sort, search, export) takes one of these and
/// checks it on a row/block cadence, so the UI can cancel a multi-TB job instantly.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// Create a fresh, un-cancelled token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Request cancellation. Idempotent.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Relaxed);
    }

    /// Whether cancellation has been requested.
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flips_once_cancelled_and_is_visible_through_clones() {
        let token = CancellationToken::new();
        assert!(!token.is_cancelled());

        let observer = token.clone();
        token.cancel();

        assert!(
            observer.is_cancelled(),
            "cancellation must be visible to clones"
        );
    }
}
