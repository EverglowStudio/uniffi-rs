//! Explicit resource close contracts reuse the unified async gates and counters.
use super::{Arc, Ordering, UnifiedError, UnifiedGate, UnifiedTracker};

#[derive(uniffi::Object)]
pub struct UnifiedLifecycle {
    tracker: Arc<UnifiedTracker>,
    cleanup: Arc<UnifiedGate>,
    fail: bool,
}

#[uniffi::export]
impl UnifiedLifecycle {
    #[uniffi::constructor]
    pub fn new(tracker: Arc<UnifiedTracker>, cleanup: Arc<UnifiedGate>, fail: bool) -> Arc<Self> {
        Arc::new(Self {
            tracker,
            cleanup,
            fail,
        })
    }

    pub async fn ping(&self) -> u32 {
        7
    }

    pub fn sync_ping(&self) -> Result<u32, UnifiedError> {
        Ok(5)
    }

    pub fn plain_ping(&self) -> u32 {
        3
    }

    pub async fn pending(&self, body: Arc<UnifiedGate>) -> u32 {
        self.tracker.polls.fetch_add(1, Ordering::SeqCst);
        body.wait().await;
        9
    }

    pub async fn close(&self) -> Result<(), UnifiedError> {
        self.tracker.started.fetch_add(1, Ordering::SeqCst);
        self.cleanup.wait().await;
        self.tracker.cleaned.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            Err(UnifiedError::Closed)
        } else {
            Ok(())
        }
    }
}

impl Drop for UnifiedLifecycle {
    fn drop(&mut self) {
        self.tracker.dropped.fetch_add(1, Ordering::SeqCst);
    }
}
