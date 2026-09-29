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
#[cfg(not(feature = "synthetic-next"))]
pub const EFFECT_GROUP_STATE_FORMAT_VERSION: u16 = 1;

/// Phase A's synthetic N+1 (ADR 0115 §6) moves the family to format 2. Its
/// record keeps format 1's shape, so the N-1 upcaster lifts a format-1 body
/// as it is; the stamp is what moves. Until finalize the fleet's writer pin
/// holds its writes at format 1, and after it the synthetic `upgrade`
/// handler rewrites each object.
#[cfg(feature = "synthetic-next")]
pub const EFFECT_GROUP_STATE_FORMAT_VERSION: u16 = 2;

/// The group index's stored-format table: the family's registered surface
/// and descriptor, plus the N-1 upcaster hooks (none at the baseline).
pub(crate) const EFFECT_GROUP_STATE_FORMATS: StoredValueFormats = StoredValueFormats {
    what: "effect group",
    surface: lash_core::surface_format!(EFFECT_GROUP_STATE_FORMAT_VERSION),
    upcast_n1: EFFECT_GROUP_STATE_UPCASTS,
};

#[cfg(not(feature = "synthetic-next"))]
const EFFECT_GROUP_STATE_UPCASTS: &crate::object_state::UpcastTable = &[];

#[cfg(feature = "synthetic-next")]
const EFFECT_GROUP_STATE_UPCASTS: &crate::object_state::UpcastTable = &[(1, upcast_format_1)];

/// The synthetic N+1's N-1 upcaster: format 1's record body is format 2's.
#[cfg(feature = "synthetic-next")]
fn upcast_format_1(body: serde_json::Value) -> Result<serde_json::Value, TerminalError> {
    Ok(body)
}

/// What the synthetic N+1's `upgrade` handler did to one group (ADR 0115
/// §3.2, §6). JSON is tagged by `upgrade`.
#[cfg(feature = "synthetic-next")]
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "upgrade", rename_all = "snake_case")]
pub(crate) enum EffectGroupUpgradeResponse {
    /// The object holds no state: nothing to convert, and nothing written.
    Absent,
    /// The fleet still writes format `writes`: finalize has not moved `F`,
    /// so nothing is rewritten while rollback is still promised.
    NotFinalized { writes: u32 },
    /// The object's `_compat` already names the newest format.
    Current { format: u32 },
    /// Every value was rewritten at `format` and `_compat` raised to it, in
    /// this one exclusive invocation.
    Upgraded { from: u32, format: u32 },
}

/// The synthetic `upgrade` handler of `EffectGroupIndex` (ADR 0115 §3.2):
/// once finalize has moved the fleet to write the newest format, rewrite
/// the group's record at it and raise all three `_compat` fields, so a
/// leftover N handler is refused by the record. An object already current,
/// or a fleet not yet finalized, is answered with nothing changed.
#[cfg(feature = "synthetic-next")]
pub(super) async fn upgrade(
    ctx: &ObjectContext<'_>,
    fleet: lash_core::FleetFormat,
) -> Result<EffectGroupUpgradeResponse, TerminalError> {
    use crate::compat::{COMPAT_KEY, ObjectCompat};
    use restate_sdk::context::{ContextReadState as _, ContextWriteState as _};

    let keys = ctx.get_keys().await?;
    if keys.is_empty() {
        return Ok(EffectGroupUpgradeResponse::Absent);
    }
    let object = object_state::admit_exclusive(ctx, &EFFECT_GROUP_STATE_FAMILY, fleet).await?;
    let newest = EFFECT_GROUP_STATE_FORMATS.newest();
    let writes = fleet.writer_version(EFFECT_GROUP_STATE_FORMATS.surface);
    if writes != newest {
        return Ok(EffectGroupUpgradeResponse::NotFinalized { writes });
    }
    let Some(Json(compat)) = ctx.get::<Json<ObjectCompat>>(COMPAT_KEY).await? else {
        return Err(TerminalError::new(format!(
            "effect group {} was admitted without a `{COMPAT_KEY}` record",
            ctx.key()
        )));
    };
    if compat == ObjectCompat::fresh(newest) {
        return Ok(EffectGroupUpgradeResponse::Current { format: newest });
    }
    if let Some(unknown) = keys.iter().find(|key| {
        object_state::is_value_key(key) && *key != INDEX_STATE_KEY && *key != MEMBERSHIP_STATE_KEY
    }) {
        return Err(TerminalError::new(format!(
            "effect group {} holds `{unknown}`, which the upgrade does not convert",
            ctx.key()
        )));
    }
    if let Some(record) = load_index(ctx).await? {
        super::store_index(ctx, object.writer, record);
    }
    if let Some(membership) = object_state::get_stamped::<EffectGroupMembership>(
        ctx,
        MEMBERSHIP_STATE_KEY,
        &EFFECT_GROUP_STATE_FORMATS,
    )
    .await?
    {
        super::store_membership(ctx, object.writer, membership);
    }
    ctx.set(COMPAT_KEY, Json(ObjectCompat::fresh(newest)));
    Ok(EffectGroupUpgradeResponse::Upgraded {
        from: compat.format,
        format: newest,
    })
}

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

/// The group's accepted membership, read only where children are rebuilt
/// (FIG-4068). A group whose index exists has one: open writes both in the
/// same exclusive invocation, and only a completed retirement clears it.
pub(super) async fn load_membership(
    ctx: &ObjectContext<'_>,
) -> Result<EffectGroupMembership, TerminalError> {
    object_state::get_stamped(ctx, MEMBERSHIP_STATE_KEY, &EFFECT_GROUP_STATE_FORMATS)
        .await?
        .ok_or_else(|| {
            TerminalError::new(format!(
                "effect group {} retains no membership record",
                ctx.key()
            ))
        })
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
