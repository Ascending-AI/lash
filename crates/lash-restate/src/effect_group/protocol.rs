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

/// The version of the wire the group's handlers speak: the request and
/// response shapes callers exchange with them (ADR 0106 §3, FIG-3814). The
/// stored record's format is [`EFFECT_GROUP_STATE_FORMAT_VERSION`] — a wire
/// move that does not move the record bumps this alone.
///
/// No request field carries it: the wire's check is the format registry,
/// whose serde-shape guard fails a wire-shape edit that does not bump this
/// constant.
///
/// 2: the close and the retirement release their cancel-decided wait
/// children's waits, and admission answers such a child `CancelDecided`
/// (FIG-3630).
/// 3: a rank read says whether it is the caller's own await, and the index
/// refuses a caller's read of a group closed to it (FIG-3676).
/// 4: a recorded settlement ends the child's cancel wait as `Settled`, which
/// the child's dispatch invocation reads as no cancel (FIG-3709).
/// 5: the open request's shape carries the opener's admitted scope
/// (FIG-3780).
/// 6: the open request declares the group's `dispatch_route`, and the open
/// response's `OpenedFresh`/`ReopenedPreparing` report the recorded route
/// (FIG-3795 S10).
pub const EFFECT_GROUP_WIRE_VERSION: u32 = 6;

/// The version of what the dispatch workflow journals: its
/// `EffectGroupDispatchRequest` input and the `ctx.run` outputs a replay
/// trusts. A replayed journal belongs to the generation that wrote it, so
/// this is a drain surface, not a stamped one.
///
/// 5: the journaled child request's shape carries the opener's admitted
/// scope (FIG-3780).
pub const EFFECT_GROUP_DISPATCH_JOURNAL_VERSION: u32 = 5;

/// The stored format the group index's retained record stamps into its
/// object-state envelope. Bump it when the record's stored shape changes;
/// the previous format reads through the N-1 upcaster slot in
/// [`EFFECT_GROUP_STATE_FORMATS`].
///
/// 2: the record carries `dispatch_route`, the service name the group's
/// dispatch was sent under (FIG-3795 S10).
pub const EFFECT_GROUP_STATE_FORMAT_VERSION: u16 = 2;

/// A format-1 record's body read as format 2: `dispatch_route` was added by
/// FIG-3795, and a format-1 group could only have been dispatched under the
/// stable `EffectGroupDispatch` name — generation lanes did not exist.
fn upcast_group_state_v1(body: serde_json::Value) -> Result<serde_json::Value, TerminalError> {
    let mut object = body.as_object().cloned().ok_or_else(|| {
        TerminalError::new("effect group state of stored format 1 is not a JSON object")
    })?;
    object
        .entry("dispatch_route")
        .or_insert_with(|| serde_json::Value::String("EffectGroupDispatch".to_owned()));
    Ok(serde_json::Value::Object(object))
}

/// The group index's stored-format table: the current stamp, plus the N-1
/// upcaster hooks.
pub(crate) const EFFECT_GROUP_STATE_FORMATS: StoredValueFormats = StoredValueFormats {
    what: "effect group",
    current: EFFECT_GROUP_STATE_FORMAT_VERSION,
    upcast_n1: &[(1, upcast_group_state_v1)],
};

/// Every index handler's first read: the group's retained record, refused
/// before the handler acts when it carries a stamp this build does not read.
pub(super) async fn load_index(
    ctx: &ObjectContext<'_>,
) -> Result<Option<EffectGroupStateRecord>, TerminalError> {
    object_state::get_stamped(ctx, INDEX_STATE_KEY, &EFFECT_GROUP_STATE_FORMATS).await
}

pub(super) async fn load_index_shared(
    ctx: &SharedObjectContext<'_>,
) -> Result<Option<EffectGroupStateRecord>, TerminalError> {
    object_state::get_stamped_shared(ctx, INDEX_STATE_KEY, &EFFECT_GROUP_STATE_FORMATS).await
}

/// Decode a group's retained index state, dispatching on its stored-format
/// stamp before the record is decoded, so state whose shape has moved is
/// refused by stamp, never by an accident of decoding.
#[cfg(test)]
pub(crate) fn decode_index_state(
    group_key: &str,
    state: serde_json::Value,
) -> Result<EffectGroupStateRecord, TerminalError> {
    object_state::decode_stamped_value(group_key, state, &EFFECT_GROUP_STATE_FORMATS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record_state(format: Option<u16>) -> serde_json::Value {
        let body = serde_json::to_value(EffectGroupStateRecord {
            shape_digest: "shape-digest".to_owned(),
            dispatch_route: "EffectGroupDispatch".to_owned(),
            lifecycle: EffectGroupLifecycle::Preparing {
                dispatch: EffectGroupDispatchState::Unadopted,
                live: EffectGroupStateLiveRecord {
                    shape: EffectGroupShape {
                        wake: lash_core::GroupWakePolicy::All,
                        loser_disposition: LoserPolicy::RunToCompletion,
                        replay_keys: vec!["child-0".to_owned()],
                        wait_scope: ExecutionScope::runtime_operation("group"),
                        membership: vec!["{}".to_owned()],
                        opener: lash_core::AdmittedScope::turn("session", "turn"),
                    },
                    next_rank: 0,
                    next_commit_seq: 0,
                    commit_states: BTreeMap::new(),
                    settlements: BTreeMap::new(),
                    settled_positions: BTreeMap::new(),
                },
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
            record_state(Some(EFFECT_GROUP_STATE_FORMAT_VERSION - 2)),
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

    #[test]
    fn index_state_of_the_previous_format_upcasts_with_the_stable_route() {
        // A format-1 record has no `dispatch_route`: it predates FIG-3795,
        // so its dispatch could only have been sent under the stable name.
        let mut body = serde_json::to_value(EffectGroupStateRecord {
            shape_digest: "shape-digest".to_owned(),
            dispatch_route: "EffectGroupDispatch".to_owned(),
            lifecycle: EffectGroupLifecycle::Preparing {
                dispatch: EffectGroupDispatchState::Unadopted,
                live: EffectGroupStateLiveRecord {
                    shape: EffectGroupShape {
                        wake: lash_core::GroupWakePolicy::All,
                        loser_disposition: LoserPolicy::RunToCompletion,
                        replay_keys: vec!["child-0".to_owned()],
                        wait_scope: ExecutionScope::runtime_operation("group"),
                        membership: vec!["{}".to_owned()],
                        opener: lash_core::AdmittedScope::turn("session", "turn"),
                    },
                    next_rank: 0,
                    next_commit_seq: 0,
                    commit_states: BTreeMap::new(),
                    settlements: BTreeMap::new(),
                    settled_positions: BTreeMap::new(),
                },
            },
        })
        .expect("serialize an index record");
        body.as_object_mut()
            .expect("the record is an object")
            .remove("dispatch_route");
        let state = serde_json::json!({
            "format": EFFECT_GROUP_STATE_FORMAT_VERSION - 1,
            "body": body,
        });
        let record = decode_index_state("group", state).expect("format-1 state upcasts");
        assert_eq!(record.dispatch_route, "EffectGroupDispatch");
    }
}
