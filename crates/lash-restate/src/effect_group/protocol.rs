//! The effect-group index's stored state decode (ADR 0106 §3, FIG-3814).
//!
//! The record Restate retains for each group lives under the stamped
//! object-state envelope in [`crate::object_state`]: a `format` stamp that
//! decode dispatches on — the current format directly, a stamp one format
//! behind through the family's N-1 upcaster hook. Any other stamp — a newer
//! deployment's, or an unstamped record that predates the envelope — is
//! refused before the handler acts, with a typed terminal error Restate does
//! not retry.

use super::*;

/// The version of the Restate effect-group index protocol this build speaks.
///
/// 2: the close and the retirement release their cancel-decided wait
/// children's waits, and admission answers such a child `CancelDecided`
/// (FIG-3630).
/// 3: a rank read says whether it is the caller's own await, and the index
/// refuses a caller's read of a group closed to it (FIG-3676).
/// 4: a recorded settlement ends the child's cancel wait as `Settled`, which
/// the child's dispatch invocation reads as no cancel (FIG-3709).
pub const EFFECT_GROUP_INDEX_PROTOCOL_VERSION: u32 = 4;

/// The stored format the group index's retained record stamps into its
/// object-state envelope. Bump it when the record's stored shape changes;
/// the previous format reads through the N-1 upcaster slot in
/// [`EFFECT_GROUP_STATE_FORMATS`].
pub const EFFECT_GROUP_STATE_FORMAT_VERSION: u16 = 1;

/// The group index's stored-format table: the current stamp, plus the N-1
/// upcaster hooks (empty while the first stamped layout is the baseline).
pub(crate) const EFFECT_GROUP_STATE_FORMATS: StoredValueFormats = StoredValueFormats {
    what: "effect group",
    current: EFFECT_GROUP_STATE_FORMAT_VERSION,
    upcast_n1: &[],
};

/// Every index handler's first read: the group's retained record, refused
/// before the handler acts when it carries a stamp this build does not read.
pub(super) async fn load_index(
    ctx: &ObjectContext<'_>,
) -> Result<Option<EffectGroupIndexRecord>, TerminalError> {
    object_state::get_stamped(ctx, INDEX_STATE_KEY, &EFFECT_GROUP_STATE_FORMATS).await
}

pub(super) async fn load_index_shared(
    ctx: &SharedObjectContext<'_>,
) -> Result<Option<EffectGroupIndexRecord>, TerminalError> {
    object_state::get_stamped_shared(ctx, INDEX_STATE_KEY, &EFFECT_GROUP_STATE_FORMATS).await
}

/// Decode a group's retained index state, dispatching on its stored-format
/// stamp before the record is decoded, so state whose shape has moved is
/// refused by stamp, never by an accident of decoding.
#[cfg(test)]
pub(crate) fn decode_index_state(
    group_key: &str,
    state: serde_json::Value,
) -> Result<EffectGroupIndexRecord, TerminalError> {
    object_state::decode_stamped_value(group_key, state, &EFFECT_GROUP_STATE_FORMATS)
}

/// The typed refusal of index state written under another protocol version.
pub(crate) fn protocol_retired_error(
    group_key: &str,
    stamped: Option<u64>,
) -> RuntimeEffectControllerError {
    let found = stamped.map_or_else(
        || "no protocol version".to_string(),
        |v| format!("protocol version {v}"),
    );
    RuntimeEffectControllerError::new(
        RuntimeErrorCode::EngineEffectGroupProtocolRetired,
        format!(
            "effect group {group_key} index state carries {found}; this deployment speaks \
             effect-group protocol version {EFFECT_GROUP_INDEX_PROTOCOL_VERSION} and refuses it \
             before any effect. The group's state must be recreated under this version."
        ),
    )
}

/// The typed protocol refusal an index handler's terminal error carries, if
/// that is what `message` is.
pub(crate) fn protocol_refusal_in(message: &str) -> Option<RuntimeEffectControllerError> {
    serde_json::from_str::<RuntimeEffectControllerError>(message)
        .ok()
        .filter(|error| error.code == RuntimeErrorCode::EngineEffectGroupProtocolRetired)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_state(format: Option<u16>) -> serde_json::Value {
        let body = serde_json::to_value(EffectGroupIndexRecord {
            protocol_version: EFFECT_GROUP_INDEX_PROTOCOL_VERSION,
            shape_digest: "shape-digest".to_owned(),
            lifecycle: EffectGroupLifecycle::Retired {
                cleanup: EffectGroupCleanup::Complete,
            },
        })
        .expect("serialize an index record");
        match format {
            Some(format) => serde_json::json!({ "format": format, "body": body }),
            None => body,
        }
    }

    #[test]
    fn index_state_of_the_current_format_decodes() {
        let record = decode_index_state(
            "group",
            record_state(Some(EFFECT_GROUP_STATE_FORMAT_VERSION)),
        )
        .expect("current-format state decodes");
        assert_eq!(record.shape_digest, "shape-digest");
    }

    #[test]
    fn index_state_of_another_or_no_format_is_refused_typed() {
        for stale in [
            record_state(None),
            record_state(Some(EFFECT_GROUP_STATE_FORMAT_VERSION - 1)),
            record_state(Some(EFFECT_GROUP_STATE_FORMAT_VERSION + 1)),
        ] {
            let refusal = decode_index_state("group", stale).expect_err("stale state is refused");
            let typed = crate::object_state::stored_format_error_in(refusal.message())
                .expect("the refusal carries the typed stored-format error");
            assert_eq!(
                typed.code,
                RuntimeErrorCode::EngineObjectStateFormatUnsupported
            );
            assert!(
                typed.code.is_terminal(),
                "Restate must not retry the refusal"
            );
        }
    }
}
