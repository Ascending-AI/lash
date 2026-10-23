//! Adapter helpers for lowering a built request tree and encoding its slots.
use crate::llm::transport::ProviderFailureKind;
use lash_protocol_types::AttachmentRef;
use lash_protocol_types::{
    llm::attachment_delivery::AttachmentPosition, llm::attachment_delivery::Delivery,
};

use lash_protocol_types::{
    llm::types::AttachmentSlot, llm::types::GenerationReceipt, llm::types::LiveRequestBody,
    llm::types::LlmRequest, llm::types::LoweredItem, llm::types::ProviderRouteIdentity,
    llm::types::RecordedRequestTemplate, llm::types::RequestSegment, llm::types::SegmentCache,
    llm::types::SegmentContext, llm::types::SegmentContextKey, llm::types::SegmentKey,
    llm::types::SlotCodec, llm::types::TemplateJson, llm::types::TransientJson,
};

use lash_transport_failure::{LlmTransportError, TransportRetryVerdict};
use serde_json::{Value, json};

pub fn template_error(error: impl std::fmt::Display) -> LlmTransportError {
    LlmTransportError::new(format!("request template unavailable: {error}"))
        .with_kind(ProviderFailureKind::Validation)
        .with_lash_code(
            lash_protocol_types::session_model::TurnFailureCode::AdmittedRequestUnavailable,
        )
        .with_retry_verdict(TransportRetryVerdict::Forbidden)
}

pub fn retained_template_error(
    error: lash_protocol_types::llm::types::TemplateError,
) -> LlmTransportError {
    template_error(error)
}

/// Mark the slots `body` delivered as provider files rejected, when
/// `error` is a refusal before any output whose decoded error body
/// `names_missing_file`. A body that delivered no provider file marks
/// nothing, whatever the provider said: bytes and URLs have no cached file
/// to forget (ADR 0135 §4).
pub fn reject_missing_provider_files(
    error: LlmTransportError,
    body: &LiveRequestBody,
    names_missing_file: impl FnOnce(u16, &Value) -> bool,
) -> LlmTransportError {
    if error.output_started || error.partial_response.is_some() {
        return error;
    }
    let slots = body.provider_file_slots();
    let definite = !slots.is_empty()
        && error.http_status.is_some_and(|status| {
            error
                .raw
                .as_deref()
                .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
                .is_some_and(|value| names_missing_file(status, &value))
        });
    if definite {
        error.with_rejected_slots(slots)
    } else {
        error
    }
}

/// Whether a provider's own error message says a file it was handed is
/// missing, deleted or expired. Callers pass the message field of a decoded
/// error object whose status and type they have already matched, never a
/// raw body.
pub fn message_names_missing_file(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("file")
        && [
            "not found",
            "no such file",
            "does not exist",
            "may not exist",
            "expired",
            "deleted",
        ]
        .iter()
        .any(|gone| message.contains(gone))
}

/// The slot an adapter pins for each attachment node it writes: pinned to
/// `codec` and to what the route `accepts` there, narrowed by the request's
/// host acceptance. An attachment nothing accepts refuses the lowering.
pub fn slot_pinner<'a>(
    accepts: impl Fn(
        &lash_protocol_types::MediaType,
        AttachmentPosition,
    ) -> lash_protocol_types::llm::attachment_delivery::ProviderAccepts
    + 'a,
    request: &'a LlmRequest,
    provider: &'a str,
    codec: &'a str,
) -> impl FnMut(&AttachmentRef, AttachmentPosition) -> Result<AttachmentSlot, LlmTransportError> + 'a
{
    move |reference: &AttachmentRef, position| {
        let accepts = accepts(&reference.media_type, position).narrowed(
            request
                .attachment_acceptance
                .forms(provider, &reference.media_type, position),
        );
        if accepts.is_empty() {
            return Err(crate::llm::transport::unsupported_attachment_capability(
                provider,
                reference,
                position,
                &request
                    .attachment_acceptance
                    .acceptors(&reference.media_type, position),
            ));
        }
        Ok(AttachmentSlot {
            reference: reference.clone(),
            position,
            accepts,
            codec: SlotCodec {
                name: codec.into(),
                revision: 1,
            },
        })
    }
}

/// The request-level inputs every lowered unit of `request` on `route`
/// depends on besides its messages: the lowering's `namespace` (its name
/// and revision), the route and model, and the host's attachment
/// acceptance its slots are narrowed by. An adapter adds the dialect flags
/// its units read, then finishes the context.
pub fn segment_context(
    namespace: &str,
    request: &LlmRequest,
    route: &ProviderRouteIdentity,
) -> SegmentContext {
    SegmentContext::new(namespace)
        .str(&route.provider)
        .str(&route.endpoint)
        .str(&route.model)
        .str(request.model.wire_model())
        .json(request.attachment_acceptance.as_ref())
}

/// `segments`, one unit's lowered items, as tree nodes in its body.
///
/// # Errors
///
/// [`template_error`] when a segment is not an item.
pub fn lowered_items(segments: &[RequestSegment]) -> Result<Vec<TemplateJson>, LlmTransportError> {
    segments
        .iter()
        .map(|segment| {
            LoweredItem::new(segment.clone())
                .map(TemplateJson::Lowered)
                .map_err(template_error)
        })
        .collect()
}

/// How an adapter's request builder writes each unit of history it lowers
/// (a wire message, or a group of wire items): as the JSON it builds, for a
/// reader of the plain body, or through a [`SegmentCache`], where a unit
/// whose key was lowered before is the segments it was, and only new units
/// are built and pinned.
pub enum UnitWriter<'a> {
    /// Every unit is built and returned as JSON.
    Plain,
    /// Each unit is lowered once per key, its attachments pinned by `pin`.
    Cached {
        cache: &'a SegmentCache,
        context: SegmentContextKey,
        pin: Box<SlotPin<'a>>,
        source_offset: usize,
    },
}

/// Pins one attachment a cached unit carries to the slot it is sent through.
type SlotPin<'a> =
    dyn FnMut(&AttachmentRef, AttachmentPosition) -> Result<AttachmentSlot, LlmTransportError> + 'a;

impl<'a> UnitWriter<'a> {
    /// Write units through `cache` under `context`, each slot pinned by
    /// [`slot_pinner`] for `codec`.
    pub fn cached(
        cache: &'a SegmentCache,
        context: SegmentContextKey,
        accepts: impl Fn(
            &lash_protocol_types::MediaType,
            AttachmentPosition,
        ) -> lash_protocol_types::llm::attachment_delivery::ProviderAccepts
        + 'a,
        request: &'a LlmRequest,
        provider: &'a str,
        codec: &'a str,
    ) -> Self {
        Self::Cached {
            cache,
            context,
            pin: Box::new(slot_pinner(accepts, request, provider, codec)),
            source_offset: 0,
        }
    }

    /// Cache a changed provider group while building its JSON. Its source
    /// range remains attached to each item, including reused cache entries.
    pub fn unit_at<'m>(
        &mut self,
        range: std::ops::Range<usize>,
        key: impl FnOnce(SegmentKey<'m>) -> SegmentKey<'m>,
        build: impl FnOnce() -> Result<Vec<TemplateJson>, LlmTransportError>,
    ) -> Result<Vec<TemplateJson>, LlmTransportError> {
        let Self::Cached {
            cache,
            context,
            pin,
            source_offset,
        } = self
        else {
            return build();
        };
        let global = range.start + *source_offset..range.end + *source_offset;
        let segments = cache.segments(
            key(SegmentKey::new(context)).u64(global.start as u64),
            || {
                build()?
                    .iter()
                    .map(|json| {
                        RequestSegment::item(json, &mut |reference, position| {
                            pin(reference, position)
                        })
                    })
                    .collect::<Result<Vec<_>, LlmTransportError>>()
            },
        )?;
        segments
            .iter()
            .map(|segment| {
                LoweredItem::new(segment.clone().with_source_range(global.clone()))
                    .map(TemplateJson::Lowered)
                    .map_err(template_error)
            })
            .collect()
    }

    pub fn at_source_offset(mut self, offset: usize) -> Self {
        if let Self::Cached { source_offset, .. } = &mut self {
            *source_offset = offset;
        }
        self
    }
    pub fn source_offset(&self) -> usize {
        match self {
            Self::Plain => 0,
            Self::Cached { source_offset, .. } => *source_offset,
        }
    }

    /// The tree nodes of one unit: built by `build`, or reused when `key`
    /// names a unit lowered before. `key` adds to the unit's key what its
    /// wire form reads besides the context: its source messages, by
    /// identity, and its modifiers.
    ///
    /// # Errors
    ///
    /// What `build` or pinning a slot refuses with.
    pub fn unit<'m>(
        &mut self,
        key: impl FnOnce(SegmentKey<'m>) -> SegmentKey<'m>,
        build: impl FnOnce() -> Result<Vec<TemplateJson>, LlmTransportError>,
    ) -> Result<Vec<TemplateJson>, LlmTransportError> {
        match self {
            Self::Plain => build(),
            Self::Cached {
                cache,
                context,
                pin,
                ..
            } => {
                let segments = cache.segments(key(SegmentKey::new(context)), || {
                    build()?
                        .iter()
                        .map(|json| {
                            RequestSegment::item(json, &mut |reference, position| {
                                pin(reference, position)
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()
                })?;
                lowered_items(&segments)
            }
        }
    }
}

/// Lower the tree an adapter's builder emitted: its JSON as literals, each
/// lowered item as the segment it is, and each attachment node as a slot
/// [`slot_pinner`] pins.
pub fn lower_attachment_json(
    accepts: impl Fn(
        &lash_protocol_types::MediaType,
        AttachmentPosition,
    ) -> lash_protocol_types::llm::attachment_delivery::ProviderAccepts,
    request: &LlmRequest,
    route: ProviderRouteIdentity,
    settings: (bool, Option<GenerationReceipt>),
    body: &TemplateJson,
    codec: &str,
) -> Result<RecordedRequestTemplate, LlmTransportError> {
    let (stream, generation) = settings;
    let provider = route.provider.clone();
    let mut builder = RecordedRequestTemplate::builder(route, stream, generation);
    builder.json(body, &mut slot_pinner(accepts, request, &provider, codec))?;
    builder.finish().map_err(template_error)
}

/// Refuse a slot another codec pinned: an adapter encodes only its own.
/// Whether the delivery is one the slot accepts is the fill's check
/// ([`fill_slots`](super::slot_delivery)), made before any codec runs.
pub fn check_codec(slot: &AttachmentSlot, name: &str) -> Result<(), LlmTransportError> {
    if slot.codec.name.as_ref() != name || slot.codec.revision != 1 {
        return Err(template_error(
            "the pinned attachment codec is not implemented",
        ));
    }
    Ok(())
}

pub fn canonical_slot(
    slot: &AttachmentSlot,
    delivery: &Delivery,
) -> Result<TransientJson, LlmTransportError> {
    use base64::Engine;
    check_codec(slot, "lash.canonical")?;
    let value = match delivery {
        Delivery::Bytes(bytes) => {
            json!({"bytes_base64": base64::engine::general_purpose::STANDARD.encode(bytes)})
        }
        Delivery::Url { url, .. } => json!({"url": url.expose()}),
        Delivery::ProviderFile { id, .. } => json!({"provider_file": id.expose()}),
    };
    Ok(TransientJson::new(&value, delivery))
}

/// Recover the canonical codec's transient values from the supplied wire,
/// without asking an attachment backend again. Errors never contain live values.
pub(super) fn canonical_deliveries(
    body: &LiveRequestBody,
    context: &lash_protocol_types::llm::attachment_delivery::DeliveryContext,
    policy: super::AttachmentReadPolicy,
) -> Result<super::CanonicalDeliveries, LlmTransportError> {
    use base64::Engine;
    use lash_protocol_types::llm::attachment_delivery::DeliverySecret;
    use std::sync::Arc;
    #[derive(serde::Deserialize)]
    #[serde(untagged)]
    enum Encoded {
        Bytes(Bytes),
        Url(Url),
        File(File),
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Bytes {
        bytes_base64: String,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Url {
        url: String,
    }
    #[derive(serde::Deserialize)]
    #[serde(deny_unknown_fields)]
    struct File {
        provider_file: String,
    }
    let malformed = || template_error("malformed canonical attachment delivery");
    let mut slots = Vec::new();
    let mut values: Vec<Arc<Delivery>> = Vec::new();
    let mut encodings: Vec<String> = Vec::new();
    for (slot, encoded_value) in body.encoded_slots() {
        check_codec(slot, "lash.canonical")?;
        let spelling = encoded_value.wire_value();
        let encoded: Encoded = serde_json::from_str(spelling).map_err(|_| malformed())?;
        let value = match encoded {
            Encoded::Bytes(bytes) => Delivery::Bytes(
                base64::engine::general_purpose::STANDARD
                    .decode(bytes.bytes_base64)
                    .map_err(|_| malformed())?,
            ),
            Encoded::Url(url) => Delivery::Url {
                url: DeliverySecret::new(url.url),
                valid_until_ms: encoded_value.valid_until_ms(),
            },
            Encoded::File(file) => Delivery::ProviderFile {
                scope: encoded_value
                    .provider_file_scope()
                    .cloned()
                    .ok_or_else(malformed)?,
                id: DeliverySecret::new(file.provider_file),
                valid_until_ms: encoded_value.valid_until_ms(),
                // This forwarding hop did no upload and read no scratch bytes.
                uploaded: false,
            },
        };
        if encoded_value.forms() != value.forms() {
            return Err(malformed());
        }
        // The SlotDeliveries grouping rule also applies to the decoded
        // body: a repeated group cannot contradict its value or validity.
        let accepts = slot
            .accepts
            .narrowed_to_live_scope(context.live_file_scope.as_ref());
        let shared = slots.iter().position(|prior: &AttachmentSlot| {
            prior.reference.id == slot.reference.id
                && prior.reference.media_type == slot.reference.media_type
                && prior
                    .accepts
                    .narrowed_to_live_scope(context.live_file_scope.as_ref())
                    == accepts
        });
        if let Some(index) = shared {
            if slots[index].reference.byte_len != slot.reference.byte_len {
                return Err(super::AttachmentDeliveryError::ContentMismatch {
                    id: slot.reference.id.clone(),
                }
                .into_transport_error());
            }
            let expiry = match values[index].as_ref() {
                Delivery::Bytes(_) => None,
                Delivery::Url { valid_until_ms, .. }
                | Delivery::ProviderFile { valid_until_ms, .. } => *valid_until_ms,
            };
            let scope = match values[index].as_ref() {
                Delivery::ProviderFile { scope, .. } => Some(scope),
                _ => None,
            };
            if encodings[index] != spelling
                || expiry != encoded_value.valid_until_ms()
                || scope != encoded_value.provider_file_scope()
            {
                return Err(malformed());
            }
        }
        values.push(shared.map_or_else(|| Arc::new(value), |index| values[index].clone()));
        slots.push(slot.clone());
        encodings.push(spelling.to_owned());
    }
    let refs: Vec<_> = slots.iter().collect();
    super::slot_delivery::validate_deliveries(&refs, &values, context, policy)
        .map_err(super::AttachmentDeliveryError::into_transport_error)?;
    Ok(super::CanonicalDeliveries {
        slots,
        values,
        policy,
    })
}

/// A provider's immutable item run and its last complete group boundary.
#[derive(Default)]
struct ProviderHistory {
    source: lash_protocol_types::llm::types::RequestMessages,
    items: lash_protocol_types::llm::types::RequestItems,
    stable_source: usize,
    stable_items: usize,
}
impl lash_protocol_types::llm::types::LoweredUnit for ProviderHistory {
    fn retained_bytes(&self) -> usize {
        self.items.literal_len()
    }
}

/// Carry the unchanged provider groups as a persistent run. The builder gets
/// only the changed suffix and its original source ordinal. A configuration
/// change or a context-wide history policy deliberately starts a cold run.
pub async fn retained_groups<F, Fut, G>(
    request: &lash_protocol_types::llm::types::RetainedLlmRequest,
    cache: &SegmentCache,

    namespace: &str,
    configuration: &Value,
    group_start: G,
    build: F,
) -> Result<RecordedRequestTemplate, LlmTransportError>
where
    F: Fn(LlmRequest, usize) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Result<RecordedRequestTemplate, LlmTransportError>> + Send,
    G: Fn(&lash_protocol_types::llm::types::RequestMessages) -> usize + Send + Sync,
{
    use lash_protocol_types::llm::types::{RequestItems, SegmentKey};

    let context = SegmentContext::new(namespace)
        .json(configuration)
        .json(&request.model)
        .json(&request.generation)
        .json(request.attachment_acceptance.as_ref())
        .json(request.tools.as_ref())
        .json(&request.tool_choice)
        .json(&request.output_spec)
        .json(&request.instructions)
        .finish();
    let Some(first) = request.messages().first() else {
        return build(request.materialize(), 0).await;
    };
    let mut fresh_template = None;
    let history = cache
        .carried_async(
            SegmentKey::new(&context).message(first),
            |prior: Option<std::sync::Arc<ProviderHistory>>| async {
                let prior = prior.filter(|prior| {
                    !request.has_history_dependencies()
                        && prior.source.shared_prefix(request.messages()) >= prior.stable_source
                });
                let start = prior.as_ref().map_or(0, |prior| prior.stable_source);

                let mut suffix = request.envelope().clone();
                suffix.messages = request.messages().iter_from(start).cloned().collect();
                let template = build(suffix, start).await?;
                let (_, tail, _) = split_item_run(&template)?;
                let base = match &prior {
                    Some(prior) => prior.items.prefix(prior.stable_items),
                    None => RequestItems::default(),
                };
                let tail: Vec<_> = tail.iter().cloned().collect();
                let items = base.append(&tail).map_err(retained_template_error)?;
                let stable_source = if request.has_history_dependencies() {
                    0
                } else {
                    group_start(request.messages())
                };
                // The first item whose source overlaps the mutable suffix. Ranges
                // are ordered, so this reads logarithmically many retained items.
                let mut lo = 0;
                let mut hi = items.len();
                while lo < hi {
                    let mid = lo + (hi - lo) / 2;
                    if items
                        .get(mid)
                        .and_then(RequestSegment::source_range)
                        .is_some_and(|range| range.end <= stable_source)
                    {
                        lo = mid + 1;
                    } else {
                        hi = mid;
                    }
                }
                let stable_items = lo;
                fresh_template = Some(template);
                Ok::<_, LlmTransportError>(std::sync::Arc::new(ProviderHistory {
                    source: request.messages().clone(),
                    items,
                    stable_source,
                    stable_items,
                }))
            },
        )
        .await?;
    let Some(template) = fresh_template else {
        return Err(template_error(
            "a carried update did not build its envelope",
        ));
    };
    let (before, _, after) = split_item_run(&template)?;
    if history.items.is_empty() {
        return Ok(template);
    }
    let mut out = RecordedRequestTemplate::from_item_run(
        template.route.clone(),
        template.stream(),
        template.generation,
        before,
        history.items.clone(),
        after,
    )
    .map_err(retained_template_error)?;
    out.fetch_horizon = template.fetch_horizon;
    out.wire_features = template.wire_features;
    Ok(out)
}

fn split_item_run(
    template: &RecordedRequestTemplate,
) -> Result<
    (
        String,
        lash_protocol_types::llm::types::RequestItems,
        String,
    ),
    LlmTransportError,
> {
    use lash_protocol_types::llm::types::{RequestItems, SegmentPart};
    let mut before = String::new();
    let mut after = String::new();
    let mut items = Vec::new();
    let mut seen = false;
    for segment in template.segment_iter() {
        if segment.is_item() {
            seen = true;
            items.push(segment.clone());
        } else {
            let target = if seen { &mut after } else { &mut before };
            for part in segment.parts() {
                match part {
                    SegmentPart::Literal(text) => target.push_str(text),
                    SegmentPart::Slot(_) => {
                        return Err(template_error(
                            "a provider envelope contains an attachment outside its item run",
                        ));
                    }
                }
            }
        }
    }
    Ok((
        before,
        RequestItems::default()
            .append(&items)
            .map_err(retained_template_error)?,
        after,
    ))
}
