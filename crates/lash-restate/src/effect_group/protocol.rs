//! The effect-group index's stored state decode (ADR 0106 §3, FIG-3814).
//!
//! The record Restate retains for each group lives under the stamped
//! object-state envelope in [`crate::object_state`]: a `format` stamp that
//! decode dispatches on — the newest format directly, a stamp one format
//! behind through the family's N-1 upcaster hook. Any other stamp — a newer
//! deployment's, or an unstamped record that predates the envelope — is
//! refused before the handler acts, with a typed terminal error Restate does
//! not retry. Every handler admits the object's `_compat` record through
//! [`EFFECT_GROUP_STATE_FAMILY`] before it reads the record (ADR 0115 §3.2).

use super::*;

/// The version of what the dispatch workflow journals: its
/// `EffectGroupDispatchRequest` input and the `ctx.run` outputs a replay
/// trusts. A replayed journal belongs to the generation that wrote it, so
/// this is a drain surface, not a stamped one.
///
/// 5: the journaled child request's shape carries the opener's admitted
/// scope (FIG-3780).
pub const EFFECT_GROUP_DISPATCH_JOURNAL_VERSION: u32 = 5;

/// The stored format the group index's retained record stamps into its
/// object-state envelope, and the family format of every `EffectGroupIndex`
/// object's `_compat` record (ADR 0115 §3.2). Bump it when the record's
/// stored shape changes; the previous format reads through the N-1 upcaster
/// slot in [`EFFECT_GROUP_STATE_FORMATS`].
///
/// 1 is the 1.0 baseline: the record carries `dispatch_route`, the service
/// name the group's dispatch was sent under (FIG-3795 S10). It was reset in
/// place under the pre-1.0 version freeze (FIG-4048), so no upcaster is
/// registered.
pub const EFFECT_GROUP_STATE_FORMAT_VERSION: u16 = 1;

/// The group index's stored-format table: the family's registered surface
/// and descriptor, plus the N-1 upcaster hooks (none at the baseline).
pub(crate) const EFFECT_GROUP_STATE_FORMATS: StoredValueFormats = StoredValueFormats {
    what: "effect group",
    surface: lash_core::surface_format!(EFFECT_GROUP_STATE_FORMAT_VERSION),
    upcast_n1: &[],
};

/// The object family whose `_compat` record every handler admits first
/// (ADR 0115 §3.2).
pub(crate) const EFFECT_GROUP_STATE_FAMILY: ObjectFamily = ObjectFamily {
    component: lash_core_store::compat::ComponentId::RESTATE_EFFECT_GROUP_STATE,
    formats: &EFFECT_GROUP_STATE_FORMATS,
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
