//! Row decoders and transaction helpers for [`super`]: the `PgRow` →
//! journal-type mappings the impl methods compose. Split under the
//! production file-size budget; nothing here has its own policy.

use super::*;

pub(super) fn stored_effect_row(
    row: PgRow,
) -> Result<StoredEffectRow, RuntimeEffectControllerError> {
    let corrupt = |field, value| {
        effect_store_message(
            StoreError::StoredDataCorrupt {
                record_kind: "RuntimeEffectReplay",
                message: format!("{field} must be non-negative, got {value}"),
            }
            .to_string(),
        )
    };
    let lease_expires_at_ms = row.get::<i64, _>("lease_expires_at_ms");
    let due_at_ms = row.get::<Option<i64>, _>("due_at_ms");
    let state = effect_replay_driver::EffectRowState::from_columns(
        row.get("status"),
        row.get("outcome_json"),
        row.get("error_json"),
    );
    let commit_state = row
        .get::<Option<String>, _>("commit_state")
        .map(|value| {
            effect_replay_driver::EffectCommitState::from_column(&value).ok_or_else(|| {
                effect_store_message(
                    StoreError::StoredDataCorrupt {
                        record_kind: "RuntimeEffectReplay",
                        message: format!("unrecognized commit_state `{value}`"),
                    }
                    .to_string(),
                )
            })
        })
        .transpose()?;
    Ok(StoredEffectRow {
        envelope_hash: row.get("envelope_hash"),
        envelope_json: row.get("envelope_json"),
        state,
        commit_state,
        drain_input: row.get("drain_input"),
        lease_expires_at_ms: u64::try_from(lease_expires_at_ms)
            .map_err(|_| corrupt("lease_expires_at_ms", lease_expires_at_ms))?,
        due_at_ms: due_at_ms
            .map(|value| u64::try_from(value).map_err(|_| corrupt("due_at_ms", value)))
            .transpose()?,
    })
}

/// Insert a fresh claim, reporting `false` when a concurrent inserter won.
pub(super) async fn insert_claimed_row(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &EffectClaimRequest,
    stamp: &EffectLeaseStamp,
) -> Result<bool, RuntimeEffectControllerError> {
    let inserted = sqlx::query(effect_sql().replay_postgres.insert_claimed.sql())
        .bind(&request.scope_id)
        .bind(request.session_id.as_deref())
        .bind(&request.replay_key)
        .bind(&request.envelope_hash)
        .bind(&request.envelope_json)
        .bind(EffectRowStatus::InProgress.column())
        .bind(&request.owner_id)
        .bind(&request.lease_token)
        .bind(stamp.lease_expires_at_ms as i64)
        .bind(stamp.due_at_ms.map(|value| value as i64))
        .bind(stamp.now_ms as i64)
        .bind(stamp.now_ms as i64)
        .bind(request.group_key.as_deref())
        .execute(&mut **tx)
        .await
        .map_err(effect_store_error)?
        .rows_affected();
    Ok(inserted == 1)
}

pub(super) async fn take_over_expired_lease(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &EffectClaimRequest,
    stamp: &EffectLeaseStamp,
) -> Result<(), RuntimeEffectControllerError> {
    sqlx::query(effect_sql().replay.take_over_lease.sql())
        .bind(&request.scope_id)
        .bind(&request.replay_key)
        .bind(&request.owner_id)
        .bind(&request.lease_token)
        .bind(stamp.lease_expires_at_ms as i64)
        .bind(stamp.due_at_ms.map(|value| value as i64))
        .bind(stamp.now_ms as i64)
        .execute(&mut **tx)
        .await
        .map_err(effect_store_error)?;
    Ok(())
}

pub(super) fn stored_group_settlement(
    row: PgRow,
) -> Result<StoredGroupSettlement, RuntimeEffectControllerError> {
    let sequence = row.get::<i64, _>("settlement_seq");
    let state = effect_replay_driver::EffectRowState::from_columns(
        row.get("status"),
        row.get("outcome_json"),
        row.get("error_json"),
    );
    Ok(StoredGroupSettlement {
        sequence: u64::try_from(sequence).map_err(|_| {
            effect_store_message(
                StoreError::StoredDataCorrupt {
                    record_kind: "RuntimeEffectReplay",
                    message: format!("settlement_seq must be non-negative, got {sequence}"),
                }
                .to_string(),
            )
        })?,
        replay_key: row.get("replay_key"),
        state,
    })
}

/// The durably recorded group row, read back through the same column mapping
/// that wrote it.
pub(super) fn stored_group_record(
    row: PgRow,
) -> Result<EffectGroupRecord, RuntimeEffectControllerError> {
    let expected_children = row.get::<i64, _>("expected_children");
    let created_at_ms = row.get::<i64, _>("created_at_ms");
    Ok(EffectGroupRecord {
        group_key: row.get("group_key"),
        scope_id: row.get("scope_id"),
        session_id: row
            .get::<Option<String>, _>("session_id")
            .map(SessionId::from),
        wake: group_column("wake rule", row.get("wake"))?,
        loser_disposition: group_column("loser disposition", row.get("loser_disposition"))?,
        expected_children: usize::try_from(expected_children).map_err(|_| {
            group_corrupt(format!(
                "expected_children must be non-negative, got {expected_children}"
            ))
        })?,
        lifecycle: group_lifecycle(
            row.get("group_key"),
            row.get::<serde_json::Value, _>("lifecycle"),
        )?,
        created_at_ms: u64::try_from(created_at_ms).map_err(|_| {
            group_corrupt(format!(
                "created_at_ms must be non-negative, got {created_at_ms}"
            ))
        })?,
    })
}

/// A persisted `lifecycle` column read back through the JSON contract
/// [`EffectGroupLifecycle`] defines; a value that does not decode is corrupt
/// data, never defaulted to `live` — that would silently re-open a closing
/// group to new claims.
pub(super) fn group_lifecycle(
    group_key: String,
    value: serde_json::Value,
) -> Result<EffectGroupLifecycle, RuntimeEffectControllerError> {
    serde_json::from_value(value.clone()).map_err(|error| {
        group_corrupt(format!(
            "group `{group_key}` lifecycle `{value}` does not decode: {error}"
        ))
    })
}

/// A persisted group column read back through the same mapping that wrote it,
/// refusing a value no version of this runtime writes.
pub(super) fn group_column<T: EffectGroupColumn>(
    column: &'static str,
    value: String,
) -> Result<T, RuntimeEffectControllerError> {
    EffectGroupColumn::from_column(&value)
        .ok_or_else(|| group_corrupt(format!("unknown effect group {column} `{value}`")))
}

pub(super) fn group_corrupt(message: String) -> RuntimeEffectControllerError {
    effect_store_message(
        StoreError::StoredDataCorrupt {
            record_kind: "RuntimeEffectGroup",
            message,
        }
        .to_string(),
    )
}

/// A corrupt membership row, labeled for the table the row came from.
pub(super) fn group_child_corrupt(message: String) -> RuntimeEffectControllerError {
    effect_store_message(
        StoreError::StoredDataCorrupt {
            record_kind: "RuntimeEffectGroupChild",
            message,
        }
        .to_string(),
    )
}

pub(super) fn unsettled_group_child(
    row: PgRow,
) -> Result<UnsettledGroupChild, RuntimeEffectControllerError> {
    // The LEFT JOIN makes the replay half nullable: `status` is the row's
    // existence bit, so `None` there means an accepted-but-never-claimed
    // child, reported with no replay state at all.
    let state = row.get::<Option<String>, _>("status").map(|status| {
        effect_replay_driver::EffectRowState::from_columns(
            status,
            row.get("outcome_json"),
            row.get("error_json"),
        )
    });
    let position: i64 = row.get("position");
    Ok(UnsettledGroupChild {
        scope_id: row.get("scope_id"),
        position: u64::try_from(position).map_err(|_| {
            group_child_corrupt(format!("position must be non-negative, got {position}"))
        })?,
        replay_key: row.get("replay_key"),
        envelope_json: row.get("envelope_json"),
        state,
        lease_expires_at_ms: row
            .get::<Option<i64>, _>("lease_expires_at_ms")
            .map(|lease_expires_at_ms| {
                u64::try_from(lease_expires_at_ms).map_err(|_| {
                    replay_corrupt(format!(
                        "lease_expires_at_ms must be non-negative, got {lease_expires_at_ms}"
                    ))
                })
            })
            .transpose()?
            .unwrap_or(0),
        commit_state: decode_optional_commit_state(&row)?,
        commit_seq: decode_optional_commit_seq(&row)?,
        command_version: u64_from_sql(
            "RuntimeEffectGroupChild",
            "command_version",
            row.get::<i64, _>("command_version"),
        )? as u16,
    })
}

/// A corrupt replay row, labeled for the table the row came from.
pub(super) fn replay_corrupt(message: String) -> RuntimeEffectControllerError {
    effect_store_message(
        StoreError::StoredDataCorrupt {
            record_kind: "RuntimeEffectReplay",
            message,
        }
        .to_string(),
    )
}

/// `commit_state`, left-join nullable: `NULL` is a
/// never-claimed-or-pending child, not a state.
pub(super) fn decode_optional_commit_state(
    row: &PgRow,
) -> Result<Option<EffectCommitState>, RuntimeEffectControllerError> {
    row.get::<Option<String>, _>("commit_state")
        .map(|word| {
            EffectCommitState::from_column(&word)
                .ok_or_else(|| replay_corrupt(format!("unknown replay commit_state `{word}`")))
        })
        .transpose()
}

/// `commit_seq`, left-join nullable.
pub(super) fn decode_optional_commit_seq(
    row: &PgRow,
) -> Result<Option<u64>, RuntimeEffectControllerError> {
    row.get::<Option<i64>, _>("commit_seq")
        .map(|value| {
            u64::try_from(value).map_err(|_| {
                replay_corrupt(format!("commit_seq must be non-negative, got {value}"))
            })
        })
        .transpose()
}

/// Decodes `commit_state, commit_seq` — the replay row's arbitration
/// projection shared by `select_child_commit_state`,
/// `select_arbitration_by_key`, and the locked variant. On those projections
/// the row exists, so `commit_state` is NOT NULL: a NULL here is decode
/// misuse, not `pending`.
pub(super) fn decode_child_arbitration(
    row: &PgRow,
    group_key: &str,
) -> Result<StoredChildArbitration, RuntimeEffectControllerError> {
    let word: Option<String> = row.get("commit_state");
    let commit_state = word
        .as_deref()
        .and_then(EffectCommitState::from_column)
        .ok_or_else(|| {
            replay_corrupt(format!(
                "replay arbitration read returned commit_state {word:?}; the column is \
                 NOT NULL and CHECK-constrained"
            ))
        })?;
    let commit_seq = decode_optional_commit_seq(row)?;
    if matches!(
        commit_state,
        EffectCommitState::Committed | EffectCommitState::Drained
    ) && commit_seq.is_none()
    {
        return Err(replay_corrupt(format!(
            "commit_state '{commit_state:?}' carries no commit_seq; the column CHECK \
             makes that unwritable"
        )));
    }
    Ok(StoredChildArbitration {
        group_key: group_key.to_string(),
        commit_state,
        commit_seq,
    })
}

/// One child row's commit state, inside a transaction. `None` means the
/// child has no replay row — never claimed — which callers pair with the
/// retained membership their own read already proved.
pub(super) async fn select_child_commit_state(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group_key: &str,
    replay_key: &str,
) -> Result<Option<EffectCommitState>, RuntimeEffectControllerError> {
    let row = sqlx::query(effect_sql().replay.select_child_commit_state.sql())
        .bind(group_key)
        .bind(replay_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(effect_store_error)?;
    Ok(row
        .map(|row| decode_child_arbitration(&row, group_key))
        .transpose()?
        .map(|arbitration| arbitration.commit_state))
}

/// The full arbitration projection for one child row, inside a transaction.
pub(super) async fn select_child_arbitration(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group_key: &str,
    replay_key: &str,
) -> Result<Option<StoredChildArbitration>, RuntimeEffectControllerError> {
    let row = sqlx::query(effect_sql().replay.select_child_commit_state.sql())
        .bind(group_key)
        .bind(replay_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(effect_store_error)?;
    row.map(|row| decode_child_arbitration(&row, group_key))
        .transpose()
}

/// The drain input a committed group child recorded at its §4 commit — the
/// read-back a retried boundary commit answers `AlreadyCommitted` with.
pub(super) async fn read_child_drain_input(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group_key: &str,
    replay_key: &str,
) -> Result<Option<String>, RuntimeEffectControllerError> {
    let row = sqlx::query(effect_sql().replay.select_child_drain.sql())
        .bind(group_key)
        .bind(replay_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(effect_store_error)?;
    Ok(row.and_then(|row| row.get::<Option<String>, _>("drain_input")))
}

/// The `commit_seq` a committed or drained child took — the read-back a
/// losing contestant reports.
pub(super) async fn read_child_commit_seq(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group_key: &str,
    replay_key: &str,
) -> Result<u64, RuntimeEffectControllerError> {
    let arbitration = select_child_arbitration(tx, group_key, replay_key)
        .await?
        .ok_or_else(|| {
            replay_corrupt(format!(
                "child `{replay_key}` of group {group_key} won §4 but has no replay row"
            ))
        })?;
    arbitration.commit_seq.ok_or_else(|| {
        replay_corrupt(format!(
            "child `{replay_key}` of group {group_key} holds commit_state {:?} with no \
             commit_seq",
            arbitration.commit_state
        ))
    })
}

/// §4's admission fence: is the effect a fresh admission is minted under a
/// group child whose cancel disposition already committed?
///
/// Asked inside the claim's transaction under `FOR UPDATE`, so the
/// read waits out an in-flight `decide_cancel` on the minting child's
/// replay row and sees the state that committed — never a pre-commit
/// value that would let the insert outlive the decision it should have lost
/// to. `false` when the request names no minting effect: the fence answers
/// only the question §4 asks.
///
/// The minting reference is minted from a
/// [`GroupChildBinding`](lash_core_execution::GroupChildBinding) — the
/// child's own journaled address — so a named row that is missing or is no
/// group child is journal corruption, not "not a group child": a bound
/// child's own replay row cannot be absent, and silently admitting under it
/// would run nested work outside the fence the binding exists to enforce.
pub(super) async fn minting_child_cancel_decided(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    request: &EffectClaimRequest,
) -> Result<bool, RuntimeEffectControllerError> {
    let Some(minting) = &request.minting_effect else {
        return Ok(false);
    };
    let row = sqlx::query(
        effect_sql()
            .replay_postgres
            .select_arbitration_by_key_locked
            .sql(),
    )
    .bind(minting.scope_id.as_str())
    .bind(minting.replay_key.as_str())
    .fetch_optional(&mut **tx)
    .await
    .map_err(effect_store_error)?;
    let Some(row) = row else {
        return Err(replay_corrupt(format!(
            "a bound group-child admission names minting replay row `{}` under \
             scope `{}`, which does not exist; a bound child's row cannot be \
             missing",
            minting.replay_key, minting.scope_id
        )));
    };
    let Some(group_key) = row.get::<Option<String>, _>("group_key") else {
        return Err(replay_corrupt(format!(
            "a bound group-child admission names minting replay row `{}` under \
             scope `{}`, but that row is no group child; the binding derives \
             from a retained membership and cannot name an ungrouped row",
            minting.replay_key, minting.scope_id
        )));
    };
    Ok(matches!(
        decode_child_arbitration(&row, &group_key)?.commit_state,
        EffectCommitState::CancelDecided
    ))
}

/// Why a fenced `finalize` missed, when a committed cancel decision explains
/// it: `true` iff the row is a group child whose commit state is
/// `cancel_decided`, `false` for an ordinary fence loss.
pub(super) async fn finalize_miss_cancel_decided(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    fence: &EffectLeaseFence,
) -> Result<bool, RuntimeEffectControllerError> {
    let row = sqlx::query(effect_sql().replay.select_arbitration_by_key.sql())
        .bind(&fence.scope_id)
        .bind(&fence.replay_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(effect_store_error)?;
    match row {
        Some(row) => match row.get::<Option<String>, _>("group_key") {
            Some(group_key) => Ok(matches!(
                decode_child_arbitration(&row, &group_key)?.commit_state,
                EffectCommitState::CancelDecided
            )),
            None => Ok(false),
        },
        _ => Ok(false),
    }
}

/// The durably recorded group row, inside a transaction — `None` when the
/// group is gone.
pub(super) async fn select_group_record(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group_key: &str,
) -> Result<Option<EffectGroupRecord>, RuntimeEffectControllerError> {
    let row = sqlx::query(effect_sql().group.select_by_key.sql())
        .bind(group_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(effect_store_error)?;
    row.map(stored_group_record).transpose()
}

/// The rank a child's replay row is seated at, for the idempotent read-backs:
/// the rank the first `decide_cancel`/`discharge_child` wrote, which is the
/// only answer those outcomes may give.
pub(super) async fn read_child_settlement_seq(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group_key: &str,
    replay_key: &str,
) -> Result<u64, RuntimeEffectControllerError> {
    let group = select_group_record(tx, group_key)
        .await?
        .ok_or_else(|| missing_group_row(group_key))?;
    let settlement_seq: i64 =
        sqlx::query_scalar::<_, Option<i64>>(effect_sql().replay.select_settlement_seq.sql())
            .bind(&group.scope_id)
            .bind(replay_key)
            .fetch_optional(&mut **tx)
            .await
            .map_err(effect_store_error)?
            .flatten()
            .ok_or_else(|| {
                effect_store_message(
                    StoreError::StoredDataCorrupt {
                        record_kind: "RuntimeEffectReplay",
                        message: format!(
                            "child `{replay_key}` of group {group_key} was decided or \
                             drained but its replay row holds no settlement rank; the \
                             two are written in one transaction, so the journal is \
                             corrupt"
                        ),
                    }
                    .to_string(),
                )
            })?;
    u64::try_from(settlement_seq).map_err(|_| {
        effect_store_message(
            StoreError::StoredDataCorrupt {
                record_kind: "RuntimeEffectReplay",
                message: format!("settlement_seq must be non-negative, got {settlement_seq}"),
            }
            .to_string(),
        )
    })
}

/// Allocate the group's next settlement rank, inside a transaction:
/// `next_seq`, returned.
///
/// The `pg_notify` rides the same transaction, so it is delivered exactly
/// when the rank it announces commits — and not at all when the write rolls
/// back.
pub(super) async fn bump_group_rank(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group_key: &str,
) -> Result<i64, RuntimeEffectControllerError> {
    let settlement_seq = sqlx::query_scalar(effect_sql().group.bump_next_seq.sql())
        .bind(group_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(effect_store_error)?
        .ok_or_else(|| missing_group_row(group_key))?;
    super::super::group_notify::notify_group_settled(tx, group_key)
        .await
        .map_err(effect_store_error)?;
    Ok(settlement_seq)
}

/// A grouped child whose group row is gone is a corrupt journal, not a silently
/// ungrouped settlement: the rank it should have taken can never be served, so
/// reporting success would hide a group no caller can finish consuming.
pub(super) fn missing_group_row(group_key: &str) -> RuntimeEffectControllerError {
    effect_store_message(
        StoreError::StoredDataCorrupt {
            record_kind: "RuntimeEffectGroup",
            message: format!(
                "grouped effect child finalized against missing group row `{group_key}`; \
                 its settlement rank can never be served"
            ),
        }
        .to_string(),
    )
}

/// Take the scope lock and read the retirement fence for a session-free scope.
/// Session scopes are never scope-retired and take no lock here, exactly as
/// their promise atoms take the session lock instead.
pub(super) async fn fence_session_free_scope(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    session_id: Option<&SessionId>,
    scope_id: &str,
) -> Result<bool, RuntimeEffectControllerError> {
    if session_id.is_some() {
        return Ok(false);
    }
    lock_scope(tx, scope_id).await.map_err(effect_store_error)?;
    scope_is_retired(&mut **tx, scope_id)
        .await
        .map_err(|err| effect_store_message(err.to_string()))
}
