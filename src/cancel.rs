use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Notify;

#[derive(Debug)]
struct CancelledError;

impl std::fmt::Display for CancelledError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("cancelled")
    }
}

impl std::error::Error for CancelledError {}

pub fn cancelled_error() -> anyhow::Error {
    anyhow::Error::new(CancelledError)
}

pub fn is_cancelled_error(error: &anyhow::Error) -> bool {
    error.downcast_ref::<CancelledError>().is_some()
}

#[derive(Debug, Clone, Default)]
pub struct CancellationToken {
    inner: Arc<Inner>,
}

#[derive(Debug, Default)]
struct Inner {
    cancelled: AtomicBool,
    notify: Notify,
}

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        if !self.inner.cancelled.swap(true, Ordering::SeqCst) {
            self.inner.notify.notify_waiters();
        }
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    pub async fn cancelled(&self) {
        if self.is_cancelled() {
            return;
        }
        self.inner.notify.notified().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancelled_turns_remain_recognizable_through_error_context() {
        let error = cancelled_error().context("turn request failed");

        assert!(is_cancelled_error(&error));
    }
}
