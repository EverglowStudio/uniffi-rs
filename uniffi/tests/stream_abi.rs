use std::{
    collections::VecDeque,
    fmt,
    mem::ManuallyDrop,
    pin::Pin,
    sync::{
        atomic::{AtomicI8, AtomicUsize, Ordering},
        Mutex,
    },
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

impl From<uniffi::UnexpectedUniFFICallbackError> for StreamError {
    fn from(_: uniffi::UnexpectedUniFFICallbackError) -> Self {
        Self::Boom
    }
}

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

struct RunningSumStream {
    events: uniffi::UniFfiInputStream<StreamEvent, StreamError>,
    sum: u32,
    done: bool,
}

impl Stream for RunningSumStream {
    type Item = Result<StreamEvent, StreamError>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }
        match Pin::new(&mut self.events).poll_next(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Some(Ok(event))) => {
                self.sum = self.sum.wrapping_add(event.value);
                Poll::Ready(Some(Ok(StreamEvent { value: self.sum })))
            }
            Poll::Ready(Some(Err(error))) => {
                self.done = true;
                Poll::Ready(Some(Err(error)))
            }
            Poll::Ready(None) => {
                self.done = true;
                Poll::Ready(None)
            }
        }
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

#[uniffi::export]
pub async fn sum_input_events(
    mut events: uniffi::UniFfiInputStream<StreamEvent, StreamError>,
) -> Result<u64, StreamError> {
    let mut sum = 0;
    while let Some(event) = std::future::poll_fn(|cx| Pin::new(&mut events).poll_next(cx)).await {
        sum += u64::from(event?.value);
    }
    Ok(sum)
}

#[uniffi::export]
pub fn running_sum(
    events: uniffi::UniFfiInputStream<StreamEvent, StreamError>,
) -> uniffi::UniFfiStream<StreamEvent, StreamError> {
    Box::pin(RunningSumStream {
        events,
        sum: 0,
        done: false,
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

fn start_running_sum(input_handle: uniffi::Handle) -> uniffi::Handle {
    let mut status = uniffi::RustCallStatus::default();
    let handle = uniffi_uniffi_fn_func_running_sum(input_handle, &mut status);
    assert_eq!(status.code, uniffi::RustCallStatusCode::Success);
    handle
}

fn next_running_sum(handle: uniffi::Handle) -> (uniffi::RustCallStatusCode, Option<StreamEvent>) {
    let future = uniffi_uniffi_fn_func_running_sum_stream_next(handle);
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

fn complete_u64_future(future: uniffi::Handle) -> (uniffi::RustCallStatusCode, u64) {
    let poll_result = AtomicI8::new(-1);
    unsafe {
        ffi_uniffi_rust_future_poll_u64(
            future.clone(),
            capture_poll,
            (&poll_result as *const AtomicI8) as u64,
        );
    }
    assert_eq!(poll_result.load(Ordering::SeqCst), 0);

    let mut status = uniffi::RustCallStatus::default();
    let value = unsafe { ffi_uniffi_rust_future_complete_u64(future.clone(), &mut status) };
    unsafe {
        ffi_uniffi_rust_future_free_u64(future);
    }
    (status.code, value)
}

static INPUT_STREAM_VALUES: uniffi::deps::once_cell::sync::Lazy<
    Mutex<VecDeque<Result<Option<StreamEvent>, StreamError>>>,
> = uniffi::deps::once_cell::sync::Lazy::new(|| Mutex::new(VecDeque::new()));
static INPUT_STREAM_TEST_LOCK: uniffi::deps::once_cell::sync::Lazy<Mutex<()>> =
    uniffi::deps::once_cell::sync::Lazy::new(|| Mutex::new(()));
static INPUT_STREAM_CANCELS: AtomicUsize = AtomicUsize::new(0);

fn set_input_stream_values(
    values: impl IntoIterator<Item = Result<Option<StreamEvent>, StreamError>>,
) {
    let mut guard = INPUT_STREAM_VALUES.lock().unwrap();
    guard.clear();
    guard.extend(values);
    INPUT_STREAM_CANCELS.store(0, Ordering::SeqCst);
}

extern "C" fn input_stream_next(
    _handle: uniffi::Handle,
    callback: uniffi::ForeignFutureCallback<uniffi::RustBuffer>,
    callback_data: u64,
    _dropped_callback: &mut uniffi::ForeignFutureDroppedCallbackStruct,
) {
    let value = INPUT_STREAM_VALUES
        .lock()
        .unwrap()
        .pop_front()
        .unwrap_or(Ok(None));
    match value {
        Ok(value) => callback(
            callback_data,
            uniffi::ForeignFutureResult::from_raw_parts(
                <Option<StreamEvent> as uniffi::Lower<UniFfiTag>>::lower(value),
                uniffi::RustCallStatus::default(),
            ),
        ),
        Err(error) => callback(
            callback_data,
            uniffi::ForeignFutureResult::from_raw_parts(
                uniffi::RustBuffer::default(),
                uniffi::RustCallStatus {
                    code: uniffi::RustCallStatusCode::Error,
                    error_buf: ManuallyDrop::new(
                        <StreamError as uniffi::LowerError<UniFfiTag>>::lower_error(error),
                    ),
                },
            ),
        ),
    }
}

extern "C" fn input_stream_cancel(_handle: uniffi::Handle) {
    INPUT_STREAM_CANCELS.fetch_add(1, Ordering::SeqCst);
}

fn register_input_stream_callbacks() {
    uniffi_uniffi_fn_func_sum_input_events_input_stream_events_init(
        input_stream_next,
        input_stream_cancel,
    );
    uniffi_uniffi_fn_func_running_sum_input_stream_events_init(
        input_stream_next,
        input_stream_cancel,
    );
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

#[test]
fn input_stream_scaffolding_consumes_registered_foreign_callbacks() {
    let _guard = INPUT_STREAM_TEST_LOCK.lock().unwrap();
    register_input_stream_callbacks();
    set_input_stream_values([
        Ok(Some(StreamEvent { value: 2 })),
        Ok(Some(StreamEvent { value: 4 })),
        Ok(None),
    ]);

    let future = uniffi_uniffi_fn_func_sum_input_events(uniffi::Handle::from_raw_unchecked(1));
    assert_eq!(
        complete_u64_future(future),
        (uniffi::RustCallStatusCode::Success, 6)
    );
    assert_eq!(INPUT_STREAM_CANCELS.load(Ordering::SeqCst), 1);
}

#[test]
fn input_stream_scaffolding_lifts_typed_error_from_registered_callbacks() {
    let _guard = INPUT_STREAM_TEST_LOCK.lock().unwrap();
    register_input_stream_callbacks();
    set_input_stream_values([Ok(Some(StreamEvent { value: 2 })), Err(StreamError::Boom)]);

    let future = uniffi_uniffi_fn_func_sum_input_events(uniffi::Handle::from_raw_unchecked(3));
    assert_eq!(
        complete_u64_future(future),
        (uniffi::RustCallStatusCode::Error, 0)
    );
    assert_eq!(INPUT_STREAM_CANCELS.load(Ordering::SeqCst), 1);
}

#[test]
fn bidi_stream_scaffolding_lifts_input_and_returns_output_handle() {
    let _guard = INPUT_STREAM_TEST_LOCK.lock().unwrap();
    register_input_stream_callbacks();
    set_input_stream_values([
        Ok(Some(StreamEvent { value: 1 })),
        Ok(Some(StreamEvent { value: 2 })),
        Ok(Some(StreamEvent { value: 3 })),
        Ok(None),
    ]);

    let handle = start_running_sum(uniffi::Handle::from_raw_unchecked(5));
    assert_eq!(
        next_running_sum(handle.clone()),
        (
            uniffi::RustCallStatusCode::Success,
            Some(StreamEvent { value: 1 })
        )
    );
    assert_eq!(
        next_running_sum(handle.clone()),
        (
            uniffi::RustCallStatusCode::Success,
            Some(StreamEvent { value: 3 })
        )
    );
    assert_eq!(
        next_running_sum(handle.clone()),
        (
            uniffi::RustCallStatusCode::Success,
            Some(StreamEvent { value: 6 })
        )
    );
    assert_eq!(
        next_running_sum(handle),
        (uniffi::RustCallStatusCode::Success, None)
    );
    assert_eq!(INPUT_STREAM_CANCELS.load(Ordering::SeqCst), 1);
}

#[test]
fn bidi_stream_scaffolding_propagates_input_error_to_output_next() {
    let _guard = INPUT_STREAM_TEST_LOCK.lock().unwrap();
    register_input_stream_callbacks();
    set_input_stream_values([Ok(Some(StreamEvent { value: 2 })), Err(StreamError::Boom)]);

    let handle = start_running_sum(uniffi::Handle::from_raw_unchecked(7));
    assert_eq!(
        next_running_sum(handle.clone()),
        (
            uniffi::RustCallStatusCode::Success,
            Some(StreamEvent { value: 2 })
        )
    );
    assert_eq!(
        next_running_sum(handle),
        (uniffi::RustCallStatusCode::Error, None)
    );
    assert_eq!(INPUT_STREAM_CANCELS.load(Ordering::SeqCst), 1);
}

#[test]
fn bidi_stream_cancel_drops_input_stream() {
    let _guard = INPUT_STREAM_TEST_LOCK.lock().unwrap();
    register_input_stream_callbacks();
    set_input_stream_values([
        Ok(Some(StreamEvent { value: 10 })),
        Ok(Some(StreamEvent { value: 20 })),
    ]);

    let handle = start_running_sum(uniffi::Handle::from_raw_unchecked(9));
    assert_eq!(
        next_running_sum(handle.clone()),
        (
            uniffi::RustCallStatusCode::Success,
            Some(StreamEvent { value: 10 })
        )
    );
    uniffi_uniffi_fn_func_running_sum_stream_cancel(handle.clone());
    uniffi_uniffi_fn_func_running_sum_stream_cancel(handle.clone());
    assert_eq!(INPUT_STREAM_CANCELS.load(Ordering::SeqCst), 1);
    assert_eq!(
        next_running_sum(handle),
        (uniffi::RustCallStatusCode::Success, None)
    );
}
