//! WIRE-SLOTS and the admitted-slot hold in the L3 scenario (ADR 0133 §6,
//! ADR 0135 §6, §7; FIG-5445).
//!
//! Every attempt the scenario's drive answers is first sent through the
//! runtime's own attempt loop ([`send_admitted`]): a recording provider
//! receives the wire it filled, and a counting store double delivers each
//! attachment slot as a fresh signed URL, reading the content the slot names
//! first, so a swept attachment cannot be delivered.
//!
//!
//! One cell ([`Mode::AdapterSlots`], FIG-5515) takes the path a production
//! call does instead of the doubles: the Anthropic adapter lowers the call's
//! template and sends it, and the session's own `RuntimeAttachmentStore`
//! delivers the slots, so the adapter's codec and the store's budget,
//! grouping and validation sit inside the same literals-pinned law.
//!
//! [`send_admitted`]: lash_core::testing::runtime_helpers::send_admitted

use super::*;

use lash_core::provider::{
    AttachmentDeliveryError, Provider, ProviderComponents, ProviderHandle, ProviderOptions,
    SlotDeliveries,
};
use lash_sansio::llm::attachment_delivery::{
    AttachmentPosition, Delivery, DeliveryContext, DeliverySecret, ProviderAccepts,
};
use lash_sansio::llm::types::{
    AttachmentSlot, LiveRequestBody, RequestSegment, ResponseContext, SlotCodec,
};

/// The bytes of the image only the slot modes' model calls name.
const IMAGE: &[u8] = b"l3-slot-image";

/// The image the slot modes put before the turn: its ref, and the upload
/// that held it once put.
#[derive(Clone, Debug)]
pub(super) struct Image {
    reference: lash_core::AttachmentRef,
    upload: lash_core::UploadReferrerId,
}

/// Put [`IMAGE`] as a host does before it sends: through the session's
/// guarded store with no execution bound, so an upload of its own holds it.
/// No input names it: only the calls' templates do.
pub(super) async fn put_image(backend: &Backend) -> Result<Image, String> {
    let store = lash_core::facade_support::RuntimeAttachmentStore::new(
        backend.attachment_store(),
        backend.attachment_referrers(),
        lash_core::RuntimeOwner::Session(session()),
        lash_core::facade_support::AttachmentPolicy::standard(),
    );
    let reference = store
        .put(
            IMAGE.to_vec(),
            lash_core::AttachmentCreateMeta::new(
                lash_core::MediaType::parse("image/png").map_err(|error| error.to_string())?,
                None,
                Some("l3.png".to_owned()),
            ),
        )
        .await
        .map_err(|error| format!("put the image: {error}"))?;
    let held = backend
        .attachment_referrers()
        .attachment_referrers(&reference.id)
        .await
        .map_err(|error| format!("read the image's referrers: {error}"))?;
    let upload = held
        .into_iter()
        .find_map(|referrer| match referrer {
            lash_core::ArtifactReferrer::Upload(upload) => Some(upload),
            _ => None,
        })
        .ok_or("the image is held by no upload")?;
    Ok(Image { reference, upload })
}

/// A slot-mode call's template: `literal` (the scenario's builder
/// generation and request), then the image's slot, URL-only under the
/// canonical codec, then the closing brace.
pub(super) fn template(literal: &str, image: &Image) -> RecordedRequestTemplate {
    let mut builder = RecordedRequestTemplate::builder(route(), true, None);
    builder
        .literal(format!("{literal},\"image\":"))
        .attachment(AttachmentSlot {
            reference: image.reference.clone(),
            position: AttachmentPosition::Message,
            accepts: ProviderAccepts {
                bytes: false,
                url: true,
                provider_file: None,
            },
            codec: SlotCodec {
                name: "lash.canonical".into(),
                revision: 1,
            },
        })
        .literal("}");
    builder.finish().expect("the slot template is valid")
}

/// The template the Anthropic adapter lowers for a call that names `image`:
/// `request`, the scenario's composed request, with the image as a user
/// attachment. Each composition renders other instructions, so no two
/// lowerings are alike.
pub(super) async fn adapter_template(
    request: &LlmRequest,
    image: &Image,
) -> Result<RecordedRequestTemplate, TurnError> {
    let mut request = request.clone();
    request.generation.output_token_cap = std::num::NonZeroUsize::new(1024);
    // The host accepts the image inline, the form the SQLite store delivers.
    request.attachment_acceptance = Arc::new(lash_core::provider::AttachmentCapabilitySnapshot {
        revision: "l3".into(),
        acceptors: vec![lash_core::provider::AttachmentAcceptor {
            provider: "anthropic".into(),
            rules: vec![lash_core::provider::AttachmentAcceptanceRule {
                positions: vec![AttachmentPosition::Message],
                media_types: vec![image.reference.media_type.as_str().to_owned()],
                media_families: Vec::new(),
                forms: lash_sansio::llm::attachment_delivery::DeliveryForms {
                    bytes: true,
                    url: false,
                    provider_file: false,
                },
            }],
        }],
    });
    request
        .messages
        .push(lash_core::llm::types::LlmMessage::new(
            lash_core::llm::types::LlmRole::User,
            vec![lash_core::llm::types::LlmContentBlock::Attachment {
                reference: Box::new(image.reference.clone()),
            }],
        ));
    let template = adapter(Arc::default())
        .lower(&request)
        .await
        .map_err(|error| TurnError::Exec(format!("the adapter lowers the call: {error}")))?;
    if template.slots().count() != 1 {
        return Err(TurnError::Exec(format!(
            "the adapter lowered {} slots for one attachment",
            template.slots().count()
        )));
    }
    Ok(template)
}

/// The Anthropic adapter over a transport that records what it is sent.
fn adapter(transport: Arc<WireTransport>) -> lash_provider_anthropic::AnthropicProvider {
    lash_provider_anthropic::AnthropicProvider::new("l3-key").with_transport(transport)
}

/// The adapter's HTTP transport: it records each request body and answers a
/// complete, empty message, so an attempt is exactly one send.
#[derive(Debug, Default)]
struct WireTransport {
    sent: Mutex<Vec<String>>,
}

#[async_trait::async_trait]
impl lash_llm_transport::LlmHttpTransport for WireTransport {
    async fn send(
        &self,
        request: lash_llm_transport::LlmHttpRequest,
        _response_start_timeout: Option<Duration>,
    ) -> Result<lash_llm_transport::LlmHttpResponse, lash_core::llm::transport::LlmTransportError>
    {
        self.sent
            .lock_recover()
            .push(String::from_utf8_lossy(&request.body).into_owned());
        let events = [
            serde_json::json!({"type": "message_start", "message": {"id": "msg_l3", "usage": {"input_tokens": 1}}}),
            serde_json::json!({"type": "content_block_start", "index": 0, "content_block": {"type": "text", "text": ""}}),
            serde_json::json!({"type": "content_block_delta", "index": 0, "delta": {"type": "text_delta", "text": "ok"}}),
            serde_json::json!({"type": "content_block_stop", "index": 0}),
            serde_json::json!({"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 1}}),
            serde_json::json!({"type": "message_stop"}),
        ];
        Ok(lash_llm_transport::LlmHttpResponse {
            status: 200,
            headers: vec![("content-type".into(), "text/event-stream".into())],
            body: lash_llm_transport::LlmHttpBody::buffered(
                events
                    .iter()
                    .map(|event| format!("data: {event}\n\n"))
                    .collect::<String>(),
            ),
        })
    }
}

/// The session's runtime attachment store as the deliveries, counting each
/// delivery it makes so the scenario's delivery laws read them.
struct RuntimeDeliveries {
    store: lash_core::facade_support::RuntimeAttachmentStore,
    seen: Arc<Mutex<Seen>>,
}

#[async_trait::async_trait]
impl SlotDeliveries for RuntimeDeliveries {
    async fn deliver(
        &self,
        slots: &[&AttachmentSlot],
        ctx: &DeliveryContext,
    ) -> Result<Vec<Arc<Delivery>>, AttachmentDeliveryError> {
        let delivered = match self.store.deliver(slots, ctx).await {
            Ok(delivered) => delivered,
            Err(error) => {
                self.seen
                    .lock_recover()
                    .delivery_failures
                    .push(error.to_string());
                return Err(error);
            }
        };
        let mut seen = self.seen.lock_recover();
        for slot in slots {
            let ordinal = seen.deliveries.len() + 1;
            seen.deliveries
                .push(format!("store:{}#{ordinal}", slot.reference.id));
        }
        Ok(delivered)
    }

    async fn invalidate(
        &self,
        reference: &lash_core::AttachmentRef,
        rejected: &Delivery,
    ) -> Result<(), AttachmentDeliveryError> {
        self.store.invalidate(reference, rejected).await
    }
}

/// One attempt of `admitted` on the production path: the Anthropic adapter
/// sends it, its slots filled by the session's runtime attachment store.
async fn send_adapter_attempt(
    services: &L3Services,
    request: &LlmRequest,
    admitted: &lash_sansio::llm::types::AdmittedSend,
) -> Sent {
    let backend = services.backend();
    let transport = Arc::new(WireTransport::default());
    let mut handle = ProviderHandle::new(ProviderComponents::new(Box::new(adapter(Arc::clone(
        &transport,
    )))));
    let deliveries = RuntimeDeliveries {
        store: lash_core::facade_support::RuntimeAttachmentStore::new(
            backend.attachment_store(),
            backend.attachment_referrers(),
            lash_core::RuntimeOwner::Session(session()),
            lash_core::AttachmentPolicy::standard(),
        ),
        seen: Arc::clone(&services.seen),
    };
    let before = services.seen.lock_recover().deliveries.len();
    let _answer = lash_core::testing::runtime_helpers::send_admitted(
        &mut handle,
        request.clone(),
        admitted,
        &deliveries,
    )
    .await;
    let delivered = services.seen.lock_recover().deliveries[before..].to_vec();
    let wire = transport.sent.lock_recover().pop();
    Sent {
        // The adapter sends the body it was handed: the admitted template.
        template: wire
            .is_some()
            .then(|| RecordedRequestTemplate::clone(&admitted.template)),
        wire: wire.unwrap_or_default(),
        delivered,
    }
}

/// What one attempt sent, as the provider saw it.
pub(super) struct Sent {
    pub(super) wire: String,
    pub(super) template: Option<RecordedRequestTemplate>,
    pub(super) delivered: Vec<String>,
}

/// The scenario's provider: it serves [`route`], records every filled
/// body it is handed and answers nothing of its own (the drive answers the
/// machine).
#[derive(Clone, Debug, Default)]
struct WireRecorder {
    options: ProviderOptions,
    sent: Arc<Mutex<Vec<(String, RecordedRequestTemplate)>>>,
}

#[async_trait::async_trait]
impl Provider for WireRecorder {
    fn kind(&self) -> &'static str {
        "l3"
    }

    fn route_identity(&self, _model: &str) -> ProviderRouteIdentity {
        route()
    }

    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }

    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }

    fn serialize_config(&self) -> serde_json::Value {
        serde_json::json!({})
    }

    async fn send(
        &mut self,
        body: &LiveRequestBody,
        _context: ResponseContext,
    ) -> Result<LlmResponse, lash_core::llm::transport::LlmTransportError> {
        self.sent
            .lock_recover()
            .push((body.wire(), body.template().clone()));
        Ok(LlmResponse::default())
    }

    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

/// The host store double: it delivers each slot as a URL signed for this
/// delivery alone, after reading the content the slot names.
struct SlotStore {
    backend: Backend,
    seen: Arc<Mutex<Seen>>,
}

#[async_trait::async_trait]
impl SlotDeliveries for SlotStore {
    async fn deliver(
        &self,
        slots: &[&AttachmentSlot],
        ctx: &DeliveryContext,
    ) -> Result<Vec<Arc<Delivery>>, AttachmentDeliveryError> {
        let mut delivered = Vec::with_capacity(slots.len());
        for slot in slots {
            let id = &slot.reference.id;
            if let Err(error) = self.backend.attachment_store().get(id, 1 << 20).await {
                self.seen
                    .lock_recover()
                    .delivery_failures
                    .push(format!("{id}: {error}"));
                return Err(AttachmentDeliveryError::Missing { id: id.clone() });
            }
            let url = {
                let mut seen = self.seen.lock_recover();
                let url = format!(
                    "https://store.l3.test/{id}?sig={}",
                    seen.deliveries.len() + 1
                );
                seen.deliveries.push(url.clone());
                url
            };
            delivered.push(Arc::new(Delivery::Url {
                url: DeliverySecret::new(url),
                valid_until_ms: Some(ctx.valid_through_ms),
            }));
        }
        Ok(delivered)
    }

    async fn invalidate(
        &self,
        _reference: &lash_core::AttachmentRef,
        _rejected: &Delivery,
    ) -> Result<(), AttachmentDeliveryError> {
        Ok(())
    }
}

/// Send one attempt of `admitted`, the call `request` was admitted as,
/// through the runtime's attempt loop. Under [`Mode::SlotHold`] the image's upload ends
/// and the store is swept first, so only the call's own hold keeps it.
pub(super) async fn send_attempt(
    services: &L3Services,
    request: &LlmRequest,
    admitted: &lash_sansio::llm::types::AdmittedSend,
) -> Result<Sent, TurnError> {
    if services.mode == Mode::AdapterSlots {
        return Ok(send_adapter_attempt(services, request, admitted).await);
    }
    let backend = services.backend();
    if services.mode == Mode::SlotHold {
        let image = services.image.lock_recover().clone();
        if let Some(image) = image {
            sweep_after_upload(&backend, &image)
                .await
                .map_err(TurnError::Exec)?;
        }
    }
    let recorder = WireRecorder::default();
    let mut handle = ProviderHandle::new(ProviderComponents::new(Box::new(recorder.clone())));
    let store = SlotStore {
        backend,
        seen: Arc::clone(&services.seen),
    };
    let before = services.seen.lock_recover().deliveries.len();
    let _answer = lash_core::testing::runtime_helpers::send_admitted(
        &mut handle,
        request.clone(),
        admitted,
        &store,
    )
    .await;
    let delivered = services.seen.lock_recover().deliveries[before..].to_vec();
    let (wire, template) = recorder
        .sent
        .lock_recover()
        .pop()
        .map_or((String::new(), None), |(wire, template)| {
            (wire, Some(template))
        });
    Ok(Sent {
        wire,
        template,
        delivered,
    })
}

/// End the image's upload and sweep the store, as an operator's expiry and
/// reclamation would while the call is admitted.
async fn sweep_after_upload(backend: &Backend, image: &Image) -> Result<(), String> {
    backend
        .attachment_referrers()
        .end_attachment_referrer(&lash_core::ArtifactReferrer::Upload(image.upload.clone()))
        .await
        .map_err(|error| format!("end the image's upload: {error}"))?;
    sweep(backend).await
}

async fn sweep(backend: &Backend) -> Result<(), String> {
    lash::persistence::reclaim_unreferenced_attachments(
        backend.session_store_factory().as_ref(),
        backend.attachment_store().as_ref(),
        lash_core::AttachmentReclamationPolicy::new(
            0,
            lash_core::EmptyRootSetPolicy::AuthorizeDeleteAll,
        ),
    )
    .await
    .map(|_| ())
    .map_err(|error| format!("sweep attachments: {error}"))
}

/// The one JSON value `slot`'s pinned codec makes of the delivery this
/// attempt was given: the canonical codec's `{"url": …}` of the store
/// double's signed URL, or the Anthropic codec's base64 source of the
/// image's bytes, which the runtime store delivers.
fn encoded(slot: &AttachmentSlot, delivery: &str) -> Option<serde_json::Value> {
    use base64::Engine as _;
    match slot.codec.name.as_ref() {
        "lash.canonical" => Some(serde_json::json!({ "url": delivery })),
        "anthropic.messages.source" => Some(serde_json::json!({
            "type": "base64",
            "media_type": slot.reference.media_type,
            "data": base64::engine::general_purpose::STANDARD.encode(IMAGE),
        })),
        _ => None,
    }
}

/// WIRE-SLOTS for one attempt of a slot-bearing call: `call.sent` is
/// `stored`'s literals in order, each slot filled by exactly one JSON value,
/// what the slot's pinned codec makes ([`encoded`]) of the delivery made for
/// this attempt, which names the slot's content.
pub(super) fn fill_laws(
    ordinal: u32,
    call: &Call,
    stored: &RecordedRequestTemplate,
) -> Vec<String> {
    if stored.slots().next().is_none() {
        return Vec::new();
    }
    let attempt = call.attempt;
    let mut rest = call.sent.as_str();
    let mut delivered = call.delivered.iter();
    for segment in stored.segments() {
        match segment {
            RequestSegment::Literal { text } => match rest.strip_prefix(&**text) {
                Some(tail) => rest = tail,
                None => {
                    return vec![format!(
                        "WIRE-SLOTS: attempt {attempt} of call {ordinal} does not send its \
                         admitted literal {text:?} at {rest:?}"
                    )];
                }
            },
            RequestSegment::Attachment { slot } => {
                let mut values =
                    serde_json::Deserializer::from_str(rest).into_iter::<serde_json::Value>();
                let value = match values.next() {
                    Some(Ok(value)) => value,
                    other => {
                        return vec![format!(
                            "WIRE-SLOTS: attempt {attempt} of call {ordinal} fills a slot with \
                             no JSON value: {other:?}"
                        )];
                    }
                };
                rest = &rest[values.byte_offset()..];
                let Some(url) = delivered.next() else {
                    return vec![format!(
                        "WIRE-SLOTS: attempt {attempt} of call {ordinal} filled a slot no \
                         delivery of its own made"
                    )];
                };
                if Some(&value) != encoded(slot, url).as_ref()
                    || !url.contains(slot.reference.id.as_str())
                {
                    return vec![format!(
                        "WIRE-SLOTS: attempt {attempt} of call {ordinal} filled its slot with \
                         {value} instead of its own delivery {url}"
                    )];
                }
            }
        }
    }
    let mut violations = Vec::new();
    if !rest.is_empty() {
        violations.push(format!(
            "WIRE-SLOTS: attempt {attempt} of call {ordinal} sent {rest:?} past its template"
        ));
    }
    if delivered.next().is_some() {
        violations.push(format!(
            "WIRE-SLOTS: attempt {attempt} of call {ordinal} delivered more than its slots"
        ));
    }
    violations
}

/// Deliveries happen only inside attempts, once per slot of each, each
/// fresh: a completed call replays with none, and a resend never reuses the
/// URL an earlier attempt was given.
pub(super) fn delivery_laws(seen: &Seen, image: &Image) -> Vec<String> {
    let mut violations = Vec::new();
    let by_attempts: usize = seen.calls.iter().map(|call| call.delivered.len()).sum();
    if seen.deliveries.len() != by_attempts {
        violations.push(format!(
            "WIRE-SLOTS: the store made {} deliveries, but the attempts sent {by_attempts}",
            seen.deliveries.len()
        ));
    }
    let attempts = seen.calls.len();
    if by_attempts != attempts {
        violations.push(format!(
            "WIRE-SLOTS: {attempts} attempts of one-slot calls carried {by_attempts} deliveries"
        ));
    }
    let mut unique = seen.deliveries.clone();
    unique.sort();
    unique.dedup();
    if unique.len() != seen.deliveries.len() {
        violations.push(format!(
            "WIRE-SLOTS: a delivery was reused across attempts: {:?}",
            seen.deliveries
        ));
    }
    if let Some(foreign) = seen
        .deliveries
        .iter()
        .find(|url| !url.contains(image.reference.id.as_str()))
    {
        violations.push(format!(
            "WIRE-SLOTS: a delivery {foreign} names other content than the slot's"
        ));
    }
    violations
}

/// PUT/REFERRER, the admitted slot (ADR 0135 §7): every attempt delivered
/// the image though its upload ended and a sweep ran before it, across any
/// takeover; once the turn settled and its execution's edges were released,
/// nothing holds the image and a sweep reclaims it.
pub(super) async fn hold_laws(
    backend: &Backend,
    clock: &Arc<SimClock>,
    seen: &Seen,
    image: &Image,
) -> Vec<String> {
    let mut violations = Vec::new();
    if !seen.delivery_failures.is_empty() {
        violations.push(format!(
            "PUT/REFERRER: an admitted call's attachment could not be delivered: {:?}",
            seen.delivery_failures
        ));
    }
    if let Err(error) = sim::relay_ended_executions(backend, clock, &[&image.reference.id]).await {
        return vec![error];
    }
    let held = match backend
        .attachment_referrers()
        .attachment_referrers(&image.reference.id)
        .await
    {
        Ok(held) => held,
        Err(error) => return vec![format!("read the image's referrers: {error}")],
    };
    if !held.is_empty() {
        violations.push(format!(
            "PUT/REFERRER: the settled turn's call still holds its attachment through {held:?}"
        ));
    }
    if let Err(error) = sweep(backend).await {
        violations.push(error);
    }
    if backend
        .attachment_store()
        .get(&image.reference.id, 1 << 20)
        .await
        .is_ok()
    {
        violations.push(
            "PUT/REFERRER: an attachment only a settled call named survived the sweep".to_owned(),
        );
    }
    violations
}
