//! Adapter helpers for structural slot binding and the delivery secrecy boundary.
use crate::llm::transport::{LlmTransportError, ProviderFailureKind, TransportRetryVerdict};
use lash_sansio::AttachmentRef;
use lash_sansio::llm::attachment_delivery::{AttachmentPosition, Delivery};
use lash_sansio::llm::types::{
    AttachmentSlot, GenerationReceipt, LiveRequestBody, LlmRequest, ProviderRouteIdentity,
    RecordedRequestTemplate, SlotCodec, TransientJson,
};
use serde_json::{Value, json};

/// Internal JSON tree operand. Only explicitly named wire-part paths are bound
/// by `lower_attachment_json`; arbitrary tool JSON cannot create a slot.
pub fn attachment_operand(reference: &AttachmentRef, position: AttachmentPosition) -> Value {
    json!({"$lash_slot_reference": reference, "$lash_slot_position": position})
}

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

/// Patterns name only native attachment part positions, with `*` for array
/// indexes. The walker never treats a marker elsewhere as an attachment.
pub fn lower_attachment_json(
    accepts: impl Fn(
        &lash_sansio::MediaType,
        AttachmentPosition,
    ) -> lash_sansio::llm::attachment_delivery::ProviderAccepts,
    request: &LlmRequest,
    route: ProviderRouteIdentity,
    settings: (bool, Option<GenerationReceipt>),
    value: &Value,
    codec: &str,
    patterns: &[&str],
) -> Result<RecordedRequestTemplate, LlmTransportError> {
    fn walk(
        value: &Value,
        path: &str,
        patterns: &[&str],
        found: &mut Vec<(String, AttachmentRef, AttachmentPosition)>,
    ) -> Result<(), LlmTransportError> {
        let matches = patterns.iter().any(|pattern| {
            let a: Vec<_> = path.split('/').collect();
            let b: Vec<_> = pattern.split('/').collect();
            a.len() == b.len()
                && a.iter().zip(b).all(|(actual, expected)| {
                    expected == *actual || (expected == "*" && actual.parse::<usize>().is_ok())
                })
        });
        if matches && let Some(reference) = value.get("$lash_slot_reference") {
            let reference = serde_json::from_value(reference.clone()).map_err(template_error)?;
            let position = serde_json::from_value(value["$lash_slot_position"].clone())
                .map_err(template_error)?;
            found.push((path.to_owned(), reference, position));
            return Ok(());
        }
        match value {
            Value::Object(map) => {
                for (key, value) in map {
                    let escaped = key.replace('~', "~0").replace('/', "~1");
                    walk(value, &format!("{path}/{escaped}"), patterns, found)?;
                }
            }
            Value::Array(array) => {
                for (index, value) in array.iter().enumerate() {
                    walk(value, &format!("{path}/{index}"), patterns, found)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let (stream, generation) = settings;
    let mut found = Vec::new();
    walk(value, "", patterns, &mut found)?;
    let mut slots = Vec::new();
    for (path, reference, position) in found {
        let accepts = accepts(&reference.media_type, position).narrowed(
            request
                .attachment_acceptance
                .forms(&route.provider, &reference.media_type, position),
        );
        if accepts.is_empty() {
            return Err(crate::llm::transport::unsupported_attachment_capability(
                &route.provider,
                &reference,
                position,
                &request
                    .attachment_acceptance
                    .acceptors(&reference.media_type, position),
            ));
        }
        slots.push((
            path,
            AttachmentSlot {
                reference,
                position,
                accepts,
                codec: SlotCodec {
                    name: codec.into(),
                    revision: 1,
                },
            },
        ));
    }
    RecordedRequestTemplate::from_json(route, stream, generation, value, &slots)
        .map_err(template_error)
}

pub fn check_slot(
    slot: &AttachmentSlot,
    delivery: &Delivery,
    name: &str,
) -> Result<(), LlmTransportError> {
    if slot.codec.name.as_ref() != name || slot.codec.revision != 1 {
        return Err(template_error(
            "the pinned attachment codec is not implemented",
        ));
    }
    if !slot.accepts.allows(delivery) {
        return Err(template_error("delivery is outside pinned acceptance"));
    }
    Ok(())
}

pub fn canonical_slot(
    slot: &AttachmentSlot,
    delivery: &Delivery,
) -> Result<TransientJson, LlmTransportError> {
    use base64::Engine;
    check_slot(slot, delivery, "lash.canonical")?;
    let value = match delivery {
        Delivery::Bytes(bytes) => {
            json!({"bytes_base64": base64::engine::general_purpose::STANDARD.encode(bytes)})
        }
        Delivery::Url { url, .. } => json!({"url": url.expose()}),
        Delivery::ProviderFile { id, .. } => json!({"provider_file": id.expose()}),
    };
    Ok(TransientJson::new(&value, delivery))
}
