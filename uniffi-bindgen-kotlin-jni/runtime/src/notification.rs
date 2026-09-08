//! Future wake transport. Native producers retain no JVM references and never
//! attach to the JVM. Only the Java-owned pump blocks on this queue.
use std::{
    collections::VecDeque,
    sync::{Condvar, Mutex},
};
use uniffi::RustFuturePoll;

struct NotificationQueue {
    entries: Mutex<VecDeque<(u64, RustFuturePoll)>>,
    available: Condvar,
}
impl NotificationQueue {
    const fn new() -> Self {
        Self {
            entries: Mutex::new(VecDeque::new()),
            available: Condvar::new(),
        }
    }
    fn push(&self, id: u64, poll: RustFuturePoll) {
        // Kotlin allocates positive, never-reused signed Long IDs before poll.
        assert!(id > 0 && id <= i64::MAX as u64);
        self.entries.lock().unwrap().push_back((id, poll));
        self.available.notify_one();
    }
    fn take(&self, wait: bool) -> i64 {
        let mut entries = self.entries.lock().unwrap();
        loop {
            if let Some((id, poll)) = entries.pop_front() {
                // The sign carries Ready; neither zero nor MIN_VALUE is used.
                return if poll == RustFuturePoll::Ready {
                    -(id as i64)
                } else {
                    id as i64
                };
            }
            if !wait {
                return 0;
            }
            entries = self.available.wait(entries).unwrap();
        }
    }
}
static NOTIFICATIONS: NotificationQueue = NotificationQueue::new();

/// Generated Scheduler callback: ownership of one notification ID is transferred
/// exactly once after Pending. Synchronous Ready never calls this function.
pub extern "C" fn future_notification_enqueue(id: u64, poll: RustFuturePoll) {
    NOTIFICATIONS.push(id, poll);
}

/// Block the Java-owned daemon pump until an ID is available. This has no JNI
/// calls, including on the Rust wake thread; Kotlin owns every continuation.
pub fn future_notification_take(wait: bool) -> i64 {
    NOTIFICATIONS.take(wait)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use uniffi::{RustFutureCallback, Scheduler};

    struct Notify(Arc<NotificationQueue>, u64);
    impl RustFutureCallback for Notify {
        fn invoke(self, poll: RustFuturePoll) {
            self.0.push(self.1, poll);
        }
    }
    #[test]
    fn scheduler_early_and_repeated_wakes_queue_one_notification() {
        let queue = Arc::new(NotificationQueue::new());
        let scheduler = Scheduler::new();
        scheduler.wake();
        scheduler.wake();
        scheduler.store(Notify(queue.clone(), 1));
        for _ in 0..100 {
            scheduler.wake();
        }
        assert_eq!(queue.entries.lock().unwrap().len(), 1);
        assert_eq!(queue.take(true), 1);
        scheduler.store(Notify(queue.clone(), 2));
        assert_eq!(queue.take(true), 2);
        assert!(queue.entries.lock().unwrap().is_empty());
    }
    #[test]
    fn synchronous_ready_never_enqueues() {
        let handle = crate::future_new(async { Ok(7u32) });
        // SAFETY: typed live handle, exactly one serialized poll and free.
        unsafe {
            assert!(crate::future_poll::<u32>(
                handle.clone(),
                future_notification_enqueue,
                1
            ));
            assert_eq!(future_notification_take(false), 0);
            assert_eq!(crate::future_complete::<u32>(handle.clone()).unwrap(), 7);
            crate::future_free::<u32>(handle);
        }
    }
    #[test]
    fn daemon_consumer_receives_both_states_without_jni() {
        let queue = Arc::new(NotificationQueue::new());
        let receiver = queue.clone();
        let thread = std::thread::spawn(move || (receiver.take(true), receiver.take(true)));
        queue.push(1, RustFuturePoll::Wake);
        queue.push(i64::MAX as u64, RustFuturePoll::Ready);
        assert_eq!(thread.join().unwrap(), (1, -i64::MAX));
    }
}
