//! Adapter helpers for lowering a built request tree and encoding its slots.
use crate::llm::transport::{LlmTransportError, ProviderFailureKind, TransportRetryVerdict};
use lash_sansio::AttachmentRef;
use lash_sansio::llm::attachment_delivery::{AttachmentPosition, Delivery};
use lash_sansio::llm::types::{
    AttachmentSlot, GenerationReceipt, LiveRequestBody, LlmRequest, ProviderRouteIdentity,
    RecordedRequestTemplate, SlotCodec, TemplateJson, TransientJson,
};
use serde_json::{Value, json};

pub fn template_error(error: impl std::fmt::Display) -> LlmTransportError {
    LlmTransportError::new(format!("request template unavailable: {error}"))
        .with_kind(ProviderFailureKind::Validation)
        .with_lash_code(lash_sansio::session_model::TurnFailureCode::AdmittedRequestUnavailable)
        .with_retry_verdict(TransportRetryVerdict::Forbidden)
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

/// Lower the tree an adapter's builder emitted: its JSON as literals, and
/// each attachment node as a slot pinned to `codec` and to what the route
/// accepts there, narrowed by the request's host acceptance. An attachment
/// nothing accepts refuses the lowering.
pub fn lower_attachment_json(
    accepts: impl Fn(
        &lash_sansio::MediaType,
        AttachmentPosition,
    ) -> lash_sansio::llm::attachment_delivery::ProviderAccepts,
    request: &LlmRequest,
    route: ProviderRouteIdentity,
    settings: (bool, Option<GenerationReceipt>),
    body: &TemplateJson,
    codec: &str,
) -> Result<RecordedRequestTemplate, LlmTransportError> {
    let (stream, generation) = settings;
    let provider = route.provider.clone();
    let mut builder = RecordedRequestTemplate::builder(route, stream, generation);
    builder.json(body, &mut |reference: &AttachmentRef, position| {
        let accepts = accepts(&reference.media_type, position).narrowed(
            request
                .attachment_acceptance
                .forms(&provider, &reference.media_type, position),
        );
        if accepts.is_empty() {
            return Err(crate::llm::transport::unsupported_attachment_capability(
                &provider,
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
    })?;
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
