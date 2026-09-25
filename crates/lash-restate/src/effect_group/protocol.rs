//! The effect-group index protocol version, and the refusal of index state
//! another version wrote.
//!
//! The `EffectGroupIndex` handlers, their request and response wire types, and
//! the index record Restate retains for each group move together as one
//! protocol. A deployment that changes any of them cannot read what an older
//! one left behind, and an in-flight invocation replaying an older journal
//! would meet a journal mismatch that Restate retries without end. So every
//! index record carries the version that wrote it, and each handler refuses a
//! record stamped with any other version before it acts, with a terminal
//! typed error that Restate does not retry.

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

/// The record field the protocol version is stamped under.
const PROTOCOL_VERSION_FIELD: &str = "protocol_version";

/// Every index handler's first read: the group's retained record, refused
/// before the handler acts when another protocol version wrote it.
pub(super) async fn load_index(
    ctx: &ObjectContext<'_>,
) -> Result<Option<EffectGroupIndexRecord>, TerminalError> {
    ctx.get::<Json<serde_json::Value>>(INDEX_STATE_KEY)
        .await?
        .map(|Json(state)| decode_index_state(ctx.key(), state))
        .transpose()
}

pub(super) async fn load_index_shared(
    ctx: &SharedObjectContext<'_>,
) -> Result<Option<EffectGroupIndexRecord>, TerminalError> {
    ctx.get::<Json<serde_json::Value>>(INDEX_STATE_KEY)
        .await?
        .map(|Json(state)| decode_index_state(ctx.key(), state))
        .transpose()
}

/// Decode a group's retained index state, refusing state that another
/// protocol version wrote, or that predates the stamp.
///
/// The stamp is read from the raw state before the record is decoded, so
/// state whose shape has since moved is refused by version, never by an
/// accident of decoding.
pub(crate) fn decode_index_state(
    group_key: &str,
    state: serde_json::Value,
) -> Result<EffectGroupIndexRecord, TerminalError> {
    let stamped = state
        .get(PROTOCOL_VERSION_FIELD)
        .and_then(serde_json::Value::as_u64);
    if stamped != Some(u64::from(EFFECT_GROUP_INDEX_PROTOCOL_VERSION)) {
        let refusal = protocol_retired_error(group_key, stamped);
        return Err(TerminalError::new(
            serde_json::to_string(&refusal).unwrap_or(refusal.message),
        ));
    }
    serde_json::from_value(state).map_err(|error| {
        TerminalError::new(format!(
            "effect group {group_key} index state stamped with protocol version \
             {EFFECT_GROUP_INDEX_PROTOCOL_VERSION} does not decode: {error}"
        ))
    })
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

    fn record_state(protocol_version: Option<u32>) -> serde_json::Value {
        let mut state = serde_json::to_value(EffectGroupIndexRecord {
            protocol_version: EFFECT_GROUP_INDEX_PROTOCOL_VERSION,
            shape_digest: "shape-digest".to_owned(),
            lifecycle: EffectGroupLifecycle::Retired {
                cleanup: EffectGroupCleanup::Complete,
            },
        })
        .expect("serialize an index record");
        let object = state.as_object_mut().expect("the record is an object");
        match protocol_version {
            Some(version) => {
                object.insert(PROTOCOL_VERSION_FIELD.to_owned(), version.into());
            }
            None => {
                object.remove(PROTOCOL_VERSION_FIELD);
            }
        }
        state
    }

    #[test]
    fn index_state_of_this_protocol_version_decodes() {
        let record = decode_index_state(
            "group",
            record_state(Some(EFFECT_GROUP_INDEX_PROTOCOL_VERSION)),
        )
        .expect("current state decodes");
        assert_eq!(record.protocol_version, EFFECT_GROUP_INDEX_PROTOCOL_VERSION);
    }

    #[test]
    fn index_state_of_another_or_no_protocol_version_is_refused_typed() {
        for stale in [
            record_state(None),
            record_state(Some(EFFECT_GROUP_INDEX_PROTOCOL_VERSION - 1)),
            record_state(Some(EFFECT_GROUP_INDEX_PROTOCOL_VERSION + 1)),
        ] {
            let refusal = decode_index_state("group", stale).expect_err("stale state is refused");
            let typed = protocol_refusal_in(refusal.message())
                .expect("the refusal carries the typed protocol error");
            assert_eq!(
                typed.code,
                RuntimeErrorCode::EngineEffectGroupProtocolRetired
            );
            assert!(
                typed.code.is_terminal(),
                "Restate must not retry the refusal"
            );
        }
    }
}
