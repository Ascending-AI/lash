//! The host's one enricher: it sees every span lash exports before the SDK
//! builds it, producers before their sampling decision, and contributes
//! through lash's bounded writer.

use std::borrow::Cow;

use opentelemetry::{Array, KeyValue, StringValue, Value};

use crate::policy::{
    Contributions, TraceAttributeWriter, TraceOmission, TraceOmissionCounts, TraceValue,
    WriterRemaining,
};
use crate::telemetry::metrics::registry::{AttributeKey, RefinableKey};
use crate::{AttrKey, TraceIds, TracePolicySnapshot, TracePosition, TraceRecord, TraceSpanKind};
use lash_sansio::llm::types::LlmUsage;

/// The host's meaning for lash's spans, added at creation.
///
/// Lash calls it once for every span it exports, producers included, before
/// the SDK builds the span; for a producer, before the host's sampler
/// decides the trace, so the sampler sees what the enricher derived. It
/// runs under panic containment: a panicking enricher leaves the span with
/// lash's attributes only, counted as [`TraceOmission::EnricherFailed`]. It
/// must not block on, or call back into, the core that called it.
pub trait SpanEnricher: Send + Sync + 'static {
    fn enrich(&self, span: &SpanView<'_>, out: &mut SpanEnrichment);
}

/// What the enricher reads about one span.
pub struct SpanView<'a> {
    pub kind: TraceSpanKind,
    pub default_name: &'a str,
    /// The span's stable lash ids, the same on every owner: the host maps
    /// these to its own concepts.
    pub ids: &'a TraceIds,
    /// Complete for every span, producers included.
    pub position: &'a TracePosition,
    /// The closing or whole record. A producer's is built before its
    /// decision, as the record of a call that succeeds; the record sinks see
    /// at settle carries the call's real outcome. On a `lash.model.attempt`
    /// span it is the logical call's closing record, shared by every attempt
    /// of the call; read `attempt` and `usage()` rather than re-deriving
    /// from `record`.
    pub record: &'a TraceRecord,
    /// The sealed provider attempt a `lash.model.attempt` span decorates;
    /// `None` on every other span.
    pub attempt: Option<&'a crate::TraceModelAttempt>,
    /// The logical call's folded usage, exported on the span; see
    /// [`SpanView::usage`].
    pub(super) usage: Option<&'a LlmUsage>,
    /// Lash's attributes on the span, for reading.
    pub attributes: &'a [KeyValue],
    /// The policies in effect on the core.
    pub policy: &'a TracePolicySnapshot,
}

impl SpanView<'_> {
    /// The logical usage lash exported as `gen_ai.usage.*` on this span,
    /// from the same fold as those attributes (summed over attempts that
    /// reported usage); `None` where lash exports no usage counters: every
    /// non-model span, every attempt span (an attempt's own usage is
    /// `attempt.attempt.usage`), a call that reported none, or a sum that
    /// overflowed (the span then carries `lash.usage.overflow`).
    pub fn usage(&self) -> Option<&LlmUsage> {
        self.usage
    }
}

/// The host's contribution to one span: added attributes, replaced semantic
/// defaults and a rename, each charged against what is left of the span's
/// contribution budget before it is buffered.
pub struct SpanEnrichment {
    writer: TraceAttributeWriter,
}

impl SpanEnrichment {
    pub(super) fn new(writer: TraceAttributeWriter) -> Self {
        Self { writer }
    }

    pub(super) fn into_writer(self) -> TraceAttributeWriter {
        self.writer
    }

    /// Replaces the span name: at most `trace_limits.name_bytes`. A rename
    /// always wins.
    pub fn rename(&mut self, name: impl Into<Cow<'static, str>>) -> Result<(), TraceOmission> {
        self.writer.rename(name)
    }

    /// Adds a host attribute. A key the registry declares is refused: a
    /// provenance key with [`TraceOmission::Immutable`], a refinable one with
    /// [`TraceOmission::UseReplace`], and any `lash.` key with
    /// [`TraceOmission::ReservedKey`]. Host keys are not otherwise checked.
    pub fn set(
        &mut self,
        key: impl Into<AttrKey>,
        value: impl Into<TraceValue>,
    ) -> Result<(), TraceOmission> {
        self.writer.set(key, value)
    }

    /// Replaces a semantic default lash or a tool body set, for example
    /// `replace(RefinableKey::GenAiProviderName, "openrouter")`. It wins
    /// last, and changes the span only: metric labels keep lash's values.
    pub fn replace(
        &mut self,
        key: RefinableKey,
        value: impl Into<TraceValue>,
    ) -> Result<(), TraceOmission> {
        self.writer.replace(key, value)
    }

    /// Attributes and bytes left in this span's contribution budget, after
    /// the registry's reserve and every earlier contribution to the span.
    pub fn remaining(&self) -> WriterRemaining {
        self.writer.remaining()
    }
}

/// The OpenTelemetry value of a contributed value. JSON is exported as its
/// serialized text.
pub(super) fn otel_value(value: TraceValue) -> Value {
    match value {
        TraceValue::Str(value) => Value::String(StringValue::from(value)),
        TraceValue::Int(value) => Value::I64(value),
        TraceValue::Float(value) => Value::F64(value),
        TraceValue::Bool(value) => Value::Bool(value),
        TraceValue::StrArray(values) => Value::Array(Array::String(
            values.into_iter().map(StringValue::from).collect(),
        )),
        TraceValue::Json(value) => Value::String(StringValue::from(value.to_string())),
    }
}

/// The contributed value of an OpenTelemetry value: what a host sampler's
/// result attributes are written as.
pub(super) fn trace_value(value: Value) -> TraceValue {
    match value {
        Value::Bool(value) => TraceValue::Bool(value),
        Value::I64(value) => TraceValue::Int(value),
        Value::F64(value) => TraceValue::Float(value),
        Value::String(value) => TraceValue::Str(Cow::Owned(value.to_string())),
        Value::Array(Array::String(values)) => TraceValue::StrArray(
            values
                .into_iter()
                .map(|value| Cow::Owned(value.to_string()))
                .collect(),
        ),
        Value::Array(Array::Bool(values)) => TraceValue::Json(serde_json::json!(values)),
        Value::Array(Array::I64(values)) => TraceValue::Json(serde_json::json!(values)),
        Value::Array(Array::F64(values)) => TraceValue::Json(
            values
                .into_iter()
                .map(|value| {
                    serde_json::Number::from_f64(value).map_or(serde_json::Value::Null, Into::into)
                })
                .collect(),
        ),
        other => TraceValue::Str(Cow::Owned(other.to_string())),
    }
}

/// Assembles a span's attributes: lash's, with the refinable keys the host
/// replaced swapped in place (or set, when lash set none), then the span's
/// omission marker, then the host's added keys in admission order. Returns
/// the span's name override.
pub(super) fn assemble(
    attributes: &mut Vec<KeyValue>,
    contributions: Contributions,
    omissions: &TraceOmissionCounts,
) -> Option<Cow<'static, str>> {
    for (key, value) in contributions.replaced {
        let value = otel_value(value);
        let name = key.key();
        match attributes
            .iter_mut()
            .find(|attribute| attribute.key.as_str() == name)
        {
            Some(attribute) => attribute.value = value,
            None => attributes.push(KeyValue::new(name, value)),
        }
    }
    if !omissions.is_empty() {
        attributes.push(
            AttributeKey::TraceOmitted.value(Value::Array(Array::String(
                omissions
                    .iter()
                    .map(|(reason, _)| StringValue::from(reason.as_str()))
                    .collect(),
            ))),
        );
        attributes.push(AttributeKey::TraceOmittedCount.value(i64::from(omissions.total())));
    }
    attributes.extend(
        contributions
            .added
            .into_iter()
            .map(|(key, value)| KeyValue::new(key.as_str().to_owned(), otel_value(value))),
    );
    contributions.name
}

/// Runs `enricher` over `view` into `writer` under panic containment: a
/// panic discards everything it contributed and counts
/// [`TraceOmission::EnricherFailed`], keeping what earlier sources had.
pub(super) fn enrich(
    enricher: &dyn SpanEnricher,
    view: &SpanView<'_>,
    writer: TraceAttributeWriter,
) -> TraceAttributeWriter {
    let fallback = writer.clone();
    let mut enrichment = SpanEnrichment::new(writer);
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        enricher.enrich(view, &mut enrichment);
    })) {
        Ok(()) => enrichment.into_writer(),
        Err(_) => {
            let mut writer = fallback;
            writer.omit(TraceOmission::EnricherFailed);
            writer
        }
    }
}
