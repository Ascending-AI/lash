use lash_core::AttachmentRef;

use lash_rlm_types::RlmExecutedCall;

use lash_rlm_types::CellOutcome;

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub(crate) struct RlmReasoningPart {
    pub(crate) text: String,
    pub(crate) replay: Option<lash_core::llm::types::ProviderReasoningReplay>,
}

#[derive(Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct RlmDriverState {
    #[serde(default)]
    pub(crate) reasoning: Vec<RlmReasoningPart>,
    pub(crate) assistant_parts: Vec<lash_core::Part>,
    pub(crate) images: Vec<AttachmentRef>,
    #[serde(default)]
    pub(crate) calls: Vec<RlmExecutedCall>,
    #[serde(default)]
    pub(crate) calls_omitted: usize,
    /// One entry per `print` from the executed lashlang block (plus any
    /// raw stdout-style emission). Replaces the old split between a
    /// concatenated `combined_output: String` and a sibling
    /// `observations: Vec<String>` — the two carried the same content.
    pub(crate) output: Vec<lash_rlm_types::RlmPrint>,
    pub(crate) output_archive: Option<lash_core::RetainedOutput>,
    /// The tagged outcome preserves null terminal values. Ambiguous parked
    /// states with the old optional pair must be recreated before 1.0.
    pub(crate) outcome: CellOutcome<lash_core::CellFailure>,
    pub(crate) code: String,
}

/// Schema version of the RLM driver state parked in the protocol
/// driver-state slot in TurnCheckpoint and PendingFollowOn.suspended_cell.
///
/// version_guard(
///     shapes(cover(RlmDriverState, Envelope)),
/// )
#[cfg(not(feature = "synthetic-next"))]
/// version_surface = "migrate"
/// format_manifest = "RlmDriverState"
pub const RLM_DRIVER_STATE_VERSION: u32 = 2;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the surface one version on
/// with version 2's shape; its registered lift reads what N wrote.
#[cfg(feature = "synthetic-next")]
/// version_surface = "migrate"
/// format_manifest = "RlmDriverState"
pub const RLM_DRIVER_STATE_VERSION: u32 = 3;

#[derive(serde::Serialize, serde::Deserialize)]
struct Envelope {
    schema_version: u32,
    state: RlmDriverState,
}

#[expect(
    clippy::expect_used,
    reason = "the envelope carries a u32 schema version and crate-owned driver state, so serde_json encoding cannot fail"
)]
pub(crate) fn rlm_driver_state(
    state: RlmDriverState,
    schema_version: u32,
) -> lash_core::ProtocolDriverState {
    lash_core::ProtocolDriverState::new(
        crate::plugin::RLM_PROTOCOL_PLUGIN_ID,
        serde_json::to_value(Envelope {
            schema_version,
            state,
        })
        .expect("RLM driver state must serialize"),
    )
}

/// `fleet_recorded_version` is the version the fleet's writers emit for this
/// surface — `F`'s recorded version, which the driver's `WriterFormats` table
/// reports (FIG-3796). The decode admits that version and every version the
/// surface's `RecordUpcaster` chain lifts to the newest (FIG-3802); an
/// admitted older payload climbs to the newest through the chain, and
/// anything else is refused as unsupported.
pub(crate) fn decode_rlm_driver_state(
    state: lash_core::ProtocolDriverState,
    fleet_recorded_version: u32,
) -> Result<RlmDriverState, String> {
    if state.plugin_id != crate::plugin::RLM_PROTOCOL_PLUGIN_ID {
        return Err(format!(
            "driver state belongs to plugin `{}`, expected `{}`",
            state.plugin_id,
            crate::plugin::RLM_PROTOCOL_PLUGIN_ID
        ));
    }
    let mut payload = state.payload;
    let actual = payload
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| "RLM driver state carries no u32 schema_version".to_string())?;
    let surface = lash_core::surface_format!(RLM_DRIVER_STATE_VERSION);
    if actual != fleet_recorded_version
        && !lash_core::store::upcast_chain_covers(surface, actual, RLM_DRIVER_STATE_VERSION)
    {
        return Err(format!(
            "unsupported RLM driver state version {actual}, expected {RLM_DRIVER_STATE_VERSION}"
        ));
    }
    if actual != RLM_DRIVER_STATE_VERSION {
        lash_core::store::upcast_json_record(
            "RLM driver state",
            surface,
            actual,
            RLM_DRIVER_STATE_VERSION,
            &mut payload,
        )
        .map_err(|error| error.to_string())?;
    }
    let envelope: Envelope = serde_json::from_value(payload)
        .map_err(|error| format!("invalid RLM driver state: {error}"))?;
    Ok(envelope.state)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn both_channels_refuse_unstamped_and_foreign_driver_state() {
        for parts in [
            Vec::new(),
            vec![lash_core::Part::text(
                "prose".into(),
                "cell prose".into(),
                None,
            )],
        ] {
            let mut encoded = rlm_driver_state(
                RlmDriverState {
                    assistant_parts: parts,
                    ..Default::default()
                },
                RLM_DRIVER_STATE_VERSION,
            );
            assert!(decode_rlm_driver_state(encoded.clone(), RLM_DRIVER_STATE_VERSION).is_ok());
            encoded.payload["schema_version"] = serde_json::json!(u32::MAX);
            assert!(decode_rlm_driver_state(encoded.clone(), RLM_DRIVER_STATE_VERSION).is_err());
            encoded
                .payload
                .as_object_mut()
                .expect("envelope")
                .remove("schema_version");
            assert!(decode_rlm_driver_state(encoded, RLM_DRIVER_STATE_VERSION).is_err());
        }
    }
}
