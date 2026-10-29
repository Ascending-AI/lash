use lash_rlm_types::RlmProtocolEvent;

/// Version of the RLM payload nested in a session-history protocol event.
/// version_surface = "migrate"
/// format_manifest = "RlmProtocolEvent"
/// version_guard(roots(RlmEventEnvelope))
pub const RLM_PROTOCOL_EVENT_VERSION: u32 = 6;

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct RlmEventEnvelope {
    format: u32,
    #[serde(with = "super::recorded_event")]
    event: RlmProtocolEvent,
}

#[expect(
    clippy::expect_used,
    reason = "the typed RLM history envelope serializes"
)]
pub fn rlm_protocol_event(
    event: RlmProtocolEvent,
    schema_version: u32,
) -> lash_core::ProtocolEvent {
    lash_core::ProtocolEvent::typed(
        crate::plugin::RLM_PROTOCOL_PLUGIN_ID,
        RlmEventEnvelope {
            format: schema_version,
            event,
        },
    )
    .expect("RLM protocol events serialize")
}

#[cfg(test)]
thread_local! {
    pub(crate) static EVENT_DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Read the RLM event a session-history protocol event carries: `None` for
/// another plugin's event, typed corruption for an unstamped, foreign-format or
/// undecodable payload. The inverse of [`rlm_protocol_event`].
///
/// The event is decoded once per retained payload and shared by every
/// reader of it: the transcript, the prompt render and a restore read the
/// same decode, and a rewritten payload decodes afresh (FIG-6443).
pub fn decode_rlm_protocol_event(
    event: &lash_core::ProtocolEvent,
) -> Result<Option<std::sync::Arc<RlmProtocolEvent>>, lash_core::StoredDataCorruption> {
    if event.plugin_id != crate::plugin::RLM_PROTOCOL_PLUGIN_ID {
        return Ok(None);
    }
    let decoded = event
        .payload
        .derived(|payload| DecodedRlmEvent(decode_rlm_payload(payload).map(std::sync::Arc::new)));
    decoded.0.clone().map(Some)
}

/// What one retained RLM payload decodes to, kept with the payload.
struct DecodedRlmEvent(Result<std::sync::Arc<RlmProtocolEvent>, lash_core::StoredDataCorruption>);

fn decode_rlm_payload(
    payload: &serde_json::Value,
) -> Result<RlmProtocolEvent, lash_core::StoredDataCorruption> {
    #[cfg(test)]
    EVENT_DECODES.with(|count| count.set(count.get() + 1));
    let corrupt = |message: String| lash_core::StoredDataCorruption {
        record_kind: "RLM protocol event".into(),
        message,
    };
    #[derive(serde::Deserialize)]
    struct Stamp {
        format: u32,
    }
    let stamp: Stamp =
        serde::Deserialize::deserialize(payload).map_err(|error| corrupt(error.to_string()))?;
    if !lash_core::store::upcast_chain_covers(
        lash_core::surface_format!(RLM_PROTOCOL_EVENT_VERSION),
        stamp.format,
        RLM_PROTOCOL_EVENT_VERSION,
    ) {
        return Err(corrupt(format!(
            "unsupported format {}, expected {RLM_PROTOCOL_EVENT_VERSION}",
            stamp.format
        )));
    }
    let envelope: RlmEventEnvelope =
        serde::Deserialize::deserialize(payload).map_err(|error| corrupt(error.to_string()))?;
    Ok(envelope.event)
}

/// Whether a message is the RLM protocol's own durable output, judged by its
/// typed origin alone: the one classifier every RLM projection, prompt-side
/// and render-side, asks. A host or another plugin writing on the same
/// channel carries its own provenance and is never classified as internal.
pub fn is_rlm_protocol_output(origin: Option<&lash_core::MessageOrigin>) -> bool {
    match origin {
        Some(lash_core::MessageOrigin::Plugin {
            plugin_id,
            transient: false,
        })
        | Some(lash_core::MessageOrigin::TurnOutput {
            source: lash_core::TurnOutputSource::Plugin { plugin_id },
            ..
        }) => plugin_id == crate::plugin::RLM_PROTOCOL_PLUGIN_ID,
        _ => false,
    }
}
