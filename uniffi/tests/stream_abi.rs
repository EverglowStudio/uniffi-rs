use std::{
    fmt,
    pin::Pin,
    sync::atomic::{AtomicI8, Ordering},
    task::{Context, Poll},
};

use uniffi::deps::futures_core::Stream;

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct StreamEvent {
    pub value: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, uniffi::Error)]
pub enum StreamError {
    Boom,
}

impl fmt::Display for StreamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Boom => write!(f, "boom"),
        }
    }
}

impl std::error::Error for StreamError {}

struct CountStream {
    next: u32,
    end: u32,
}

impl Stream for CountStream {
    type Item = Result<StreamEvent, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.next >= self.end {
            Poll::Ready(None)
        } else {
            let value = self.next;
            self.next += 1;
            Poll::Ready(Some(Ok(StreamEvent { value })))
        }
    }
}

struct ErrorStream {
    next: u32,
}

impl Stream for ErrorStream {
    type Item = Result<StreamEvent, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.next += 1;
        match self.next {
            1 => Poll::Ready(Some(Ok(StreamEvent { value: 7 }))),
            2 => Poll::Ready(Some(Err(StreamError::Boom))),
            _ => Poll::Ready(None),
        }
    }
}

struct PendingStream;

impl Stream for PendingStream {
    type Item = Result<StreamEvent, StreamError>;

    fn poll_next(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Pending
    }
}

#[uniffi::export]
pub fn count_events(
    count: u32,
) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, StreamError>> + Send + 'static>> {
    Box::pin(CountStream {
        next: 0,
        end: count,
    })
}

#[uniffi::export]
pub fn error_after_one(
) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, StreamError>> + Send + 'static>> {
    Box::pin(ErrorStream { next: 0 })
}

#[uniffi::export]
pub fn pending_events(
) -> Pin<Box<dyn Stream<Item = Result<StreamEvent, StreamError>> + Send + 'static>> {
    Box::pin(PendingStream)
}

#[uniffi::export]
pub fn count_events_alias(count: u32) -> uniffi::UniFfiStream<StreamEvent, StreamError> {
    Box::pin(CountStream {
        next: 0,
        end: count,
    })
}

uniffi::setup_scaffolding!();

extern "C" fn capture_poll(data: u64, poll: uniffi::RustFuturePoll) {
    let poll_result = unsafe { &*(data as *const AtomicI8) };
    poll_result.store(poll as i8, Ordering::SeqCst);
}

fn start_count(count: u32) -> uniffi::Handle {
    let mut status = uniffi::RustCallStatus::default();
    let handle = uniffi_uniffi_fn_func_count_events(count, &mut status);
    assert_eq!(status.code, uniffi::RustCallStatusCode::Success);
    handle
}

fn next_count(handle: uniffi::Handle) -> (uniffi::RustCallStatusCode, Option<StreamEvent>) {
    let future = uniffi_uniffi_fn_func_count_events_stream_next(handle);
    complete_next_future(future)
}

fn next_error_after_one(
    handle: uniffi::Handle,
) -> (uniffi::RustCallStatusCode, Option<StreamEvent>) {
    let future = uniffi_uniffi_fn_func_error_after_one_stream_next(handle);
    complete_next_future(future)
}

fn next_alias(handle: uniffi::Handle) -> (uniffi::RustCallStatusCode, Option<StreamEvent>) {
    let future = uniffi_uniffi_fn_func_count_events_alias_stream_next(handle);
    complete_next_future(future)
}

fn complete_next_future(
    future: uniffi::Handle,
) -> (uniffi::RustCallStatusCode, Option<StreamEvent>) {
    let poll_result = AtomicI8::new(-1);
    unsafe {
        ffi_uniffi_rust_future_poll_rust_buffer(
            future.clone(),
            capture_poll,
            (&poll_result as *const AtomicI8) as u64,
        );
    }
    assert_eq!(poll_result.load(Ordering::SeqCst), 0);

    let mut status = uniffi::RustCallStatus::default();
    let buf = unsafe { ffi_uniffi_rust_future_complete_rust_buffer(future.clone(), &mut status) };
    unsafe {
        ffi_uniffi_rust_future_free_rust_buffer(future);
    }
    if status.code == uniffi::RustCallStatusCode::Success {
        (
            status.code,
            <Option<StreamEvent> as uniffi::Lift<UniFfiTag>>::try_lift(buf).unwrap(),
        )
    } else {
        (status.code, None)
    }
}

#[test]
fn stream_next_yields_values_then_done() {
    let handle = start_count(2);
    assert_eq!(
        next_count(handle.clone()),
        (
            uniffi::RustCallStatusCode::Success,
            Some(StreamEvent { value: 0 })
        )
    );
    assert_eq!(
        next_count(handle.clone()),
        (
            uniffi::RustCallStatusCode::Success,
            Some(StreamEvent { value: 1 })
        )
    );
    assert_eq!(
        next_count(handle.clone()),
        (uniffi::RustCallStatusCode::Success, None)
    );
    assert_eq!(
        next_count(handle),
        (uniffi::RustCallStatusCode::Success, None)
    );
}

#[test]
fn stream_alias_yields_values_then_done() {
    let mut status = uniffi::RustCallStatus::default();
    let handle = uniffi_uniffi_fn_func_count_events_alias(1, &mut status);
    assert_eq!(status.code, uniffi::RustCallStatusCode::Success);
    assert_eq!(
        next_alias(handle.clone()),
        (
            uniffi::RustCallStatusCode::Success,
            Some(StreamEvent { value: 0 })
        )
    );
    assert_eq!(
        next_alias(handle),
        (uniffi::RustCallStatusCode::Success, None)
    );
}

#[test]
fn stream_next_lowers_errors_through_fallible_path() {
    let mut status = uniffi::RustCallStatus::default();
    let handle = uniffi_uniffi_fn_func_error_after_one(&mut status);
    assert_eq!(status.code, uniffi::RustCallStatusCode::Success);

    assert_eq!(
        next_error_after_one(handle.clone()),
        (
            uniffi::RustCallStatusCode::Success,
            Some(StreamEvent { value: 7 })
        )
    );
    assert_eq!(
        next_error_after_one(handle.clone()),
        (uniffi::RustCallStatusCode::Error, None)
    );
    assert_eq!(
        next_error_after_one(handle),
        (uniffi::RustCallStatusCode::Success, None)
    );
}

#[test]
fn stream_cancel_is_idempotent_and_next_after_cancel_is_done() {
    let handle = start_count(10);
    uniffi_uniffi_fn_func_count_events_stream_cancel(handle.clone());
    uniffi_uniffi_fn_func_count_events_stream_cancel(handle.clone());
    assert_eq!(
        next_count(handle),
        (uniffi::RustCallStatusCode::Success, None)
    );
}

#[test]
fn stream_concurrent_next_is_rejected() {
    let mut status = uniffi::RustCallStatus::default();
    let handle = uniffi_uniffi_fn_func_pending_events(&mut status);
    assert_eq!(status.code, uniffi::RustCallStatusCode::Success);

    let pending = uniffi_uniffi_fn_func_pending_events_stream_next(handle.clone());
    let rejected = uniffi_uniffi_fn_func_pending_events_stream_next(handle.clone());
    let (status_code, value) = complete_next_future(rejected);
    assert_eq!(status_code, uniffi::RustCallStatusCode::UnexpectedError);
    assert_eq!(value, None);

    unsafe {
        ffi_uniffi_rust_future_cancel_rust_buffer(pending.clone());
        ffi_uniffi_rust_future_free_rust_buffer(pending);
    }
    uniffi_uniffi_fn_func_pending_events_stream_cancel(handle);
}
