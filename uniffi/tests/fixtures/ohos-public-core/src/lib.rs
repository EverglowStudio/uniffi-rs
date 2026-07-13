#[uniffi::export]
pub fn add(left: u32, right: u32) -> u32 {
    left + right
}

#[derive(Clone, Debug, uniffi::Record)]
pub struct CounterEvent {
    pub value: u32,
}

#[derive(Clone, Debug, uniffi::Enum)]
pub enum CounterSignal {
    Tick { event: CounterEvent },
    Complete,
}

#[uniffi::export(with_foreign)]
pub trait CounterObserver: Send + Sync {
    fn observe(&self, signal: CounterSignal);
}

#[derive(Clone, Debug, thiserror::Error, uniffi::Error)]
pub enum StreamError {
    #[error("stream failed")]
    Failed,
}

#[derive(Clone, Debug)]
pub struct EventId(pub u64);

uniffi::custom_type!(EventId, u64, {
    lower: |value| value.0,
    try_lift: |value| Ok(EventId(value)),
});

#[derive(Clone, Debug, uniffi::Record)]
pub struct EventIdBoundary {
    pub primary: EventId,
    pub values: Vec<EventId>,
}

#[derive(uniffi::Object)]
pub struct CounterObject {
    value: u32,
}

#[uniffi::export]
impl CounterObject {
    #[uniffi::constructor]
    pub fn new(value: u32) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self { value })
    }

    pub fn value(&self) -> u32 {
        self.value
    }
}

#[uniffi::export]
pub fn count_events(count: u32) -> uniffi::UniFfiStream<CounterEvent, StreamError> {
    Box::pin(futures_util::stream::iter(
        (0..count).map(|value| Ok(CounterEvent { value })),
    ))
}

#[uniffi::export]
pub fn failing_events() -> uniffi::UniFfiStream<CounterEvent, StreamError> {
    Box::pin(futures_util::stream::iter([Err(StreamError::Failed)]))
}

#[uniffi::export]
pub fn pending_events() -> uniffi::UniFfiStream<CounterEvent, StreamError> {
    Box::pin(futures_util::stream::pending())
}

#[uniffi::export]
pub fn event_id_boundary(value: EventId) -> EventIdBoundary {
    EventIdBoundary {
        primary: value.clone(),
        values: vec![value, EventId(u64::MAX)],
    }
}

#[uniffi::export]
pub fn optional_id_batches() -> uniffi::UniFfiStream<Option<Vec<EventId>>, StreamError> {
    Box::pin(futures_util::stream::iter([Ok(Some(vec![
        EventId(1),
        EventId(2),
    ]))]))
}

#[uniffi::export]
pub fn count_events_for(
    counter: std::sync::Arc<CounterObject>,
) -> uniffi::UniFfiStream<CounterEvent, StreamError> {
    count_events(counter.value())
}

#[uniffi::export]
pub async fn sum_events(
    mut events: uniffi::UniFfiInputStream<CounterEvent, StreamError>,
) -> Result<u32, StreamError> {
    use futures_util::StreamExt;

    let mut sum = 0;
    while let Some(event) = events.next().await {
        sum += event?.value;
    }
    Ok(sum)
}

#[uniffi::export]
pub async fn sum_ids(
    mut events: uniffi::UniFfiInputStream<EventId, StreamError>,
) -> Result<u64, StreamError> {
    use futures_util::StreamExt;

    let mut sum = 0;
    while let Some(event) = events.next().await {
        sum += event?.0;
    }
    Ok(sum)
}

#[uniffi::export]
pub fn echo_events(
    events: uniffi::UniFfiInputStream<CounterEvent, StreamError>,
) -> uniffi::UniFfiStream<CounterEvent, StreamError> {
    Box::pin(events)
}

uniffi::setup_scaffolding!();
