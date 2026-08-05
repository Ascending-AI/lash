use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::{Layer, Registry};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum CapturedFieldKind {
    Debug,
    Str,
    U64,
    Bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct CapturedField {
    kind: CapturedFieldKind,
    value: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CapturedEvent {
    pub(crate) level: String,
    pub(crate) target: String,
    fields: BTreeMap<String, CapturedField>,
}

impl CapturedEvent {
    pub(crate) fn field(&self, name: &str) -> &str {
        &self
            .fields
            .get(name)
            .unwrap_or_else(|| panic!("event is missing field `{name}`: {self:?}"))
            .value
    }

    pub(crate) fn field_kind(&self, name: &str) -> CapturedFieldKind {
        self.fields
            .get(name)
            .unwrap_or_else(|| panic!("event is missing field `{name}`: {self:?}"))
            .kind
    }

    pub(crate) fn field_count(&self) -> usize {
        self.fields.len()
    }

    pub(crate) fn contains_field(&self, name: &str) -> bool {
        self.fields.contains_key(name)
    }
}

#[derive(Clone, Default)]
pub(crate) struct EventCapture {
    pub(crate) events: Arc<Mutex<Vec<CapturedEvent>>>,
}

impl EventCapture {
    /// Every captured event whose `event` field names this transition.
    pub(crate) fn named(&self, event: &str) -> Vec<CapturedEvent> {
        self.events
            .lock()
            .expect("lock captured events")
            .iter()
            .filter(|captured| {
                captured
                    .fields
                    .get("event")
                    .map(|field| field.value.as_str())
                    == Some(event)
            })
            .cloned()
            .collect()
    }

    pub(crate) fn exactly_one(&self, event: &str) -> CapturedEvent {
        let matched = self.named(event);
        assert_eq!(
            matched.len(),
            1,
            "expected exactly one `{event}` trace event, captured: {:?}",
            self.events.lock().expect("lock captured events")
        );
        matched.into_iter().next().expect("checked one event")
    }
}

struct FieldVisitor(BTreeMap<String, CapturedField>);

impl FieldVisitor {
    fn record(&mut self, field: &tracing::field::Field, kind: CapturedFieldKind, value: String) {
        self.0
            .insert(field.name().to_string(), CapturedField { kind, value });
    }
}

impl tracing::field::Visit for FieldVisitor {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.record(field, CapturedFieldKind::Debug, format!("{value:?}"));
    }

    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        self.record(field, CapturedFieldKind::Str, value.to_string());
    }

    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.record(field, CapturedFieldKind::U64, value.to_string());
    }

    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.record(field, CapturedFieldKind::Bool, value.to_string());
    }
}

impl<S: tracing::Subscriber> Layer<S> for EventCapture {
    fn on_event(&self, event: &tracing::Event<'_>, _context: Context<'_, S>) {
        let mut visitor = FieldVisitor(BTreeMap::new());
        event.record(&mut visitor);
        self.events
            .lock()
            .expect("lock captured events")
            .push(CapturedEvent {
                level: event.metadata().level().to_string(),
                target: event.metadata().target().to_string(),
                fields: visitor.0,
            });
    }
}

/// Run `body` with a capture layer installed as the thread-local dispatcher.
pub(crate) fn capturing_sync<F, T>(body: F) -> (T, EventCapture)
where
    F: FnOnce() -> T,
{
    let capture = EventCapture::default();
    let subscriber = Registry::default().with(capture.clone());
    let guard = tracing::subscriber::set_default(subscriber);
    let value = body();
    drop(guard);
    (value, capture)
}

/// Run `body` with a capture layer installed as the thread-local dispatcher.
///
/// `#[tokio::test]` builds a current-thread runtime, so tasks polled on this
/// thread emit through the same capture layer.
pub(crate) async fn capturing<F, Fut, T>(body: F) -> (T, EventCapture)
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = T>,
{
    let capture = EventCapture::default();
    let subscriber = Registry::default().with(capture.clone());
    let guard = tracing::subscriber::set_default(subscriber);
    let value = body().await;
    drop(guard);
    (value, capture)
}
