/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

//! A real Rust cdylib fixture for foreign-language output-stream lifecycle tests.
//!
//! The exported factories increment `stream_starts` when foreign code enters Rust.
//! Each returned stream records its own polls and classifies its one Drop as either
//! terminal or cancelled.  `pending_operation` provides the corresponding ordinary
//! async cancellation probe.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{Mutex, OnceLock},
    task::{Context, Poll},
};

use uniffi::deps::futures_core::Stream;

/// A typed stream error with data that generated bindings must preserve.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, uniffi::Error)]
pub enum OutputStreamError {
    #[error("detailed output stream error {code}: {message}")]
    Detailed { code: u32, message: String },
}

/// A consistent snapshot of one lifecycle probe's counters.
#[derive(Clone, Debug, Default, PartialEq, Eq, uniffi::Record)]
pub struct ProbeSnapshot {
    pub stream_starts: u64,
    pub stream_next_polls: u64,
    pub stream_terminal_drops: u64,
    pub stream_cancelled_drops: u64,
    pub stream_drops: u64,
    pub future_starts: u64,
    pub future_polls: u64,
    pub future_cancelled_drops: u64,
    pub future_drops: u64,
}

static PROBES: OnceLock<Mutex<HashMap<String, ProbeSnapshot>>> = OnceLock::new();

fn probes() -> &'static Mutex<HashMap<String, ProbeSnapshot>> {
    PROBES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn with_probe(probe_id: &str, update: impl FnOnce(&mut ProbeSnapshot)) {
    let mut probes = probes()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    update(probes.entry(probe_id.to_owned()).or_default());
}

fn increment(counter: &mut u64) {
    *counter = counter.saturating_add(1);
}

fn record_stream_start(probe_id: &str) {
    with_probe(probe_id, |probe| increment(&mut probe.stream_starts));
}

fn record_stream_poll(probe_id: &str) {
    with_probe(probe_id, |probe| increment(&mut probe.stream_next_polls));
}

fn record_stream_drop(probe_id: &str, terminal: bool) {
    with_probe(probe_id, |probe| {
        increment(&mut probe.stream_drops);
        if terminal {
            increment(&mut probe.stream_terminal_drops);
        } else {
            increment(&mut probe.stream_cancelled_drops);
        }
    });
}

fn record_future_start(probe_id: &str) {
    with_probe(probe_id, |probe| increment(&mut probe.future_starts));
}

fn record_future_poll(probe_id: &str) {
    with_probe(probe_id, |probe| increment(&mut probe.future_polls));
}

fn record_future_cancelled_drop(probe_id: &str) {
    with_probe(probe_id, |probe| {
        increment(&mut probe.future_drops);
        increment(&mut probe.future_cancelled_drops);
    });
}

/// Return a value-copy of the counters associated with `probe_id`.
#[uniffi::export]
pub fn probe_snapshot(probe_id: String) -> ProbeSnapshot {
    let probes = probes()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    probes.get(&probe_id).cloned().unwrap_or_default()
}

/// Reset one probe without affecting any other probe id.
#[uniffi::export]
pub fn reset_probe(probe_id: String) {
    let mut probes = probes()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    probes.insert(probe_id, ProbeSnapshot::default());
}

struct ProbedSequence<T> {
    probe_id: String,
    items: VecDeque<Result<T, OutputStreamError>>,
    terminal: bool,
    dropped: bool,
}

impl<T> ProbedSequence<T> {
    fn new(
        probe_id: String,
        items: impl IntoIterator<Item = Result<T, OutputStreamError>>,
    ) -> Self {
        Self {
            probe_id,
            items: items.into_iter().collect(),
            terminal: false,
            dropped: false,
        }
    }
}

// Moving the sequence after it has been pinned is harmless: no field stores a
// pointer into the sequence itself.
impl<T> Unpin for ProbedSequence<T> {}

impl<T> Stream for ProbedSequence<T> {
    type Item = Result<T, OutputStreamError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        record_stream_poll(&this.probe_id);
        match this.items.pop_front() {
            Some(Ok(item)) => Poll::Ready(Some(Ok(item))),
            Some(Err(error)) => {
                this.terminal = true;
                Poll::Ready(Some(Err(error)))
            }
            None => {
                this.terminal = true;
                Poll::Ready(None)
            }
        }
    }
}

impl<T> Drop for ProbedSequence<T> {
    fn drop(&mut self) {
        if !self.dropped {
            self.dropped = true;
            record_stream_drop(&self.probe_id, self.terminal);
        }
    }
}

struct PendingStream {
    probe_id: String,
    dropped: bool,
}

impl Stream for PendingStream {
    type Item = Result<u32, OutputStreamError>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        record_stream_poll(&self.probe_id);
        Poll::Pending
    }
}

impl Drop for PendingStream {
    fn drop(&mut self) {
        if !self.dropped {
            self.dropped = true;
            record_stream_drop(&self.probe_id, false);
        }
    }
}

/// Start a stream which produces `0..count` and then completes normally.
#[uniffi::export]
pub fn counting_stream(
    probe_id: String,
    count: u32,
) -> uniffi::UniFfiStream<u32, OutputStreamError> {
    record_stream_start(&probe_id);
    Box::pin(ProbedSequence::new(probe_id, (0..count).map(Ok)))
}

/// Start a stream whose `None` is a real item rather than end-of-stream.
#[uniffi::export]
pub fn optional_stream(probe_id: String) -> uniffi::UniFfiStream<Option<u32>, OutputStreamError> {
    record_stream_start(&probe_id);
    Box::pin(ProbedSequence::new(
        probe_id,
        [Ok(Some(1)), Ok(None), Ok(Some(2))],
    ))
}

/// Start a stream that emits one item and then a typed error.
#[uniffi::export]
pub fn typed_error_stream(probe_id: String) -> uniffi::UniFfiStream<u32, OutputStreamError> {
    record_stream_start(&probe_id);
    Box::pin(ProbedSequence::new(
        probe_id,
        [
            Ok(7),
            Err(OutputStreamError::Detailed {
                code: 42,
                message: "typed output stream failure".to_owned(),
            }),
        ],
    ))
}

/// Start a stream that remains pending until a consumer cancels and drops it.
#[uniffi::export]
pub fn pending_stream(probe_id: String) -> uniffi::UniFfiStream<u32, OutputStreamError> {
    record_stream_start(&probe_id);
    Box::pin(PendingStream {
        probe_id,
        dropped: false,
    })
}

struct PendingOperation {
    probe_id: String,
    started: bool,
    dropped: bool,
}

impl PendingOperation {
    fn new(probe_id: String) -> Self {
        Self {
            probe_id,
            started: false,
            dropped: false,
        }
    }
}

impl Future for PendingOperation {
    type Output = u32;

    fn poll(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
        if !self.started {
            self.started = true;
            record_future_start(&self.probe_id);
        }
        record_future_poll(&self.probe_id);
        Poll::Pending
    }
}

impl Drop for PendingOperation {
    fn drop(&mut self) {
        if !self.dropped {
            self.dropped = true;
            record_future_cancelled_drop(&self.probe_id);
        }
    }
}

/// Start an ordinary async operation that remains pending until cancellation.
#[uniffi::export]
pub async fn pending_operation(probe_id: String) -> u32 {
    PendingOperation::new(probe_id).await
}

uniffi::setup_scaffolding!();

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> Context<'static> {
        Context::from_waker(std::task::Waker::noop())
    }

    #[test]
    fn counting_stream_records_items_done_and_terminal_drop() {
        let probe_id = "counting_stream_records_items_done_and_terminal_drop".to_owned();
        reset_probe(probe_id.clone());
        let mut stream = counting_stream(probe_id.clone(), 2);
        let mut context = context();

        assert_eq!(probe_snapshot(probe_id.clone()).stream_starts, 1);
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(Ok(0)))
        ));
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(Ok(1)))
        ));
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(None)
        ));
        drop(stream);

        assert_eq!(
            probe_snapshot(probe_id),
            ProbeSnapshot {
                stream_starts: 1,
                stream_next_polls: 3,
                stream_terminal_drops: 1,
                stream_cancelled_drops: 0,
                stream_drops: 1,
                ..ProbeSnapshot::default()
            }
        );
    }

    #[test]
    fn optional_stream_keeps_none_as_an_item() {
        let probe_id = "optional_stream_keeps_none_as_an_item".to_owned();
        reset_probe(probe_id.clone());
        let mut stream = optional_stream(probe_id.clone());
        let mut context = context();

        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(Ok(Some(1))))
        ));
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(Ok(None)))
        ));
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(Ok(Some(2))))
        ));
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(None)
        ));
        drop(stream);

        assert_eq!(
            probe_snapshot(probe_id),
            ProbeSnapshot {
                stream_starts: 1,
                stream_next_polls: 4,
                stream_terminal_drops: 1,
                stream_drops: 1,
                ..ProbeSnapshot::default()
            }
        );
    }

    #[test]
    fn typed_error_stream_preserves_its_payload_and_is_terminal() {
        let probe_id = "typed_error_stream_preserves_its_payload_and_is_terminal".to_owned();
        reset_probe(probe_id.clone());
        let mut stream = typed_error_stream(probe_id.clone());
        let mut context = context();

        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(Ok(7)))
        ));
        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(Err(OutputStreamError::Detailed { code: 42, ref message })))
                if message == "typed output stream failure"
        ));
        drop(stream);

        assert_eq!(
            probe_snapshot(probe_id),
            ProbeSnapshot {
                stream_starts: 1,
                stream_next_polls: 2,
                stream_terminal_drops: 1,
                stream_drops: 1,
                ..ProbeSnapshot::default()
            }
        );
    }

    #[test]
    fn pending_stream_drop_is_cancelled_once() {
        let probe_id = "pending_stream_drop_is_cancelled_once".to_owned();
        reset_probe(probe_id.clone());
        let mut stream = pending_stream(probe_id.clone());
        let mut context = context();

        assert!(matches!(
            stream.as_mut().poll_next(&mut context),
            Poll::Pending
        ));
        drop(stream);

        assert_eq!(
            probe_snapshot(probe_id),
            ProbeSnapshot {
                stream_starts: 1,
                stream_next_polls: 1,
                stream_terminal_drops: 0,
                stream_cancelled_drops: 1,
                stream_drops: 1,
                ..ProbeSnapshot::default()
            }
        );
    }

    #[test]
    fn pending_operation_drop_is_cancelled_once() {
        let probe_id = "pending_operation_drop_is_cancelled_once".to_owned();
        reset_probe(probe_id.clone());
        let mut operation = Box::pin(pending_operation(probe_id.clone()));
        let mut context = context();

        assert!(matches!(
            operation.as_mut().poll(&mut context),
            Poll::Pending
        ));
        drop(operation);

        assert_eq!(
            probe_snapshot(probe_id),
            ProbeSnapshot {
                future_starts: 1,
                future_polls: 1,
                future_cancelled_drops: 1,
                future_drops: 1,
                ..ProbeSnapshot::default()
            }
        );
    }

    #[test]
    fn reset_and_distinct_probe_ids_do_not_cross_talk() {
        let first = "reset_and_distinct_probe_ids_do_not_cross_talk_first".to_owned();
        let second = "reset_and_distinct_probe_ids_do_not_cross_talk_second".to_owned();
        reset_probe(first.clone());
        reset_probe(second.clone());

        let first_stream = counting_stream(first.clone(), 0);
        assert_eq!(probe_snapshot(first.clone()).stream_starts, 1);
        assert_eq!(probe_snapshot(second.clone()), ProbeSnapshot::default());
        drop(first_stream);

        reset_probe(first.clone());
        let mut second_stream = optional_stream(second.clone());
        let mut context = context();
        assert!(matches!(
            second_stream.as_mut().poll_next(&mut context),
            Poll::Ready(Some(Ok(Some(1))))
        ));
        drop(second_stream);

        assert_eq!(probe_snapshot(first), ProbeSnapshot::default());
        assert_eq!(
            probe_snapshot(second),
            ProbeSnapshot {
                stream_starts: 1,
                stream_next_polls: 1,
                stream_cancelled_drops: 1,
                stream_drops: 1,
                ..ProbeSnapshot::default()
            }
        );
    }
}
