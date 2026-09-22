//! Transaction-scope helpers for [`super`]: row decoders, guarded
//! sub-selects and the group-counter bumps the impl methods compose. Split
//! under the production file-size budget; nothing here has its own policy.

use super::*;

pub(super) fn select_effect_row(
    tx: &rusqlite::Transaction<'_>,
    scope_id: &str,
    replay_key: &str,
) -> rusqlite::Result<Option<StoredEffectRow>> {
    tx.query_row(
        effect_sql(Schema::Main)
            .replay_sqlite
            .select_for_claim
            .sql(),
        params![scope_id, replay_key],
        |row| {
            let state = effect_replay_driver::EffectRowState::from_columns(
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            );
            let commit_state: Option<String> = row.get(7)?;
            let commit_state = commit_state
                .map(|value| {
                    effect_replay_driver::EffectCommitState::from_column(&value).ok_or_else(|| {
                        sqlite_conversion_error(stored_data_corrupt(
                            "RuntimeEffectReplay",
                            format!("unrecognized commit_state `{value}`"),
                        ))
                    })
                })
                .transpose()?;
            Ok(StoredEffectRow {
                envelope_hash: row.get(0)?,
                envelope_json: row.get(1)?,
                state,
                commit_state,
                drain_input: row.get(8)?,
                lease_expires_at_ms: u64_from_sql(
                    "RuntimeEffectReplay",
                    "lease_expires_at_ms",
                    row.get(5)?,
                )?,
                due_at_ms: row
                    .get::<_, Option<i64>>(6)?
                    .map(|value| u64_from_sql("RuntimeEffectReplay", "due_at_ms", value))
                    .transpose()?,
            })
        },
    )
    .optional()
}

pub(super) fn insert_claimed_row(
    tx: &rusqlite::Transaction<'_>,
    request: &EffectClaimRequest,
    stamp: &EffectLeaseStamp,
) -> rusqlite::Result<()> {
    tx.execute(
        effect_sql(Schema::Main).replay_sqlite.insert_claimed.sql(),
        params![
            request.scope_id.as_str(),
            request.session_id.as_deref(),
            request.replay_key.as_str(),
            request.envelope_hash.as_str(),
            request.envelope_json.as_str(),
            EffectRowStatus::InProgress.column(),
            request.owner_id.as_str(),
            request.lease_token.as_str(),
            stamp.lease_expires_at_ms as i64,
            stamp.due_at_ms.map(|value| value as i64),
            stamp.now_ms as i64,
            stamp.now_ms as i64,
            request.group_key.as_deref(),
        ],
    )?;
    Ok(())
}

pub(super) fn take_over_expired_lease(
    tx: &rusqlite::Transaction<'_>,
    request: &EffectClaimRequest,
    stamp: &EffectLeaseStamp,
) -> rusqlite::Result<()> {
    tx.execute(
        effect_sql(Schema::Main).replay.take_over_lease.sql(),
        params![
            request.scope_id.as_str(),
            request.replay_key.as_str(),
            request.owner_id.as_str(),
            request.lease_token.as_str(),
            stamp.lease_expires_at_ms as i64,
            stamp.due_at_ms.map(|value| value as i64),
            stamp.now_ms as i64,
        ],
    )?;
    Ok(())
}

/// Reads back the durably recorded group row.
///
/// A group is written before this reads it, in the same transaction, so a
/// missing row is a substrate fault rather than a race — reported as corrupt
/// rather than papered over with the record the caller passed in, which would
/// make the reopen fence compare a row against itself.
pub(super) fn select_group_record(
    tx: &rusqlite::Transaction<'_>,
    group_key: &str,
) -> rusqlite::Result<EffectGroupRecord> {
    tx.query_row(
        effect_sql(Schema::Main).group.select_by_key.sql(),
        params![group_key],
        group_record_from_row,
    )
}

/// Decodes one `runtime_effect_group` row in
/// [`GroupStatements::select_by_key`](lash_store_sql::effect::group::GroupStatements)
/// column order — the projection `select_closing_by_scope` shares.
pub(super) fn group_record_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<EffectGroupRecord> {
    let lifecycle_json: String = row.get(6)?;
    Ok(EffectGroupRecord {
        group_key: row.get(0)?,
        scope_id: row.get(1)?,
        session_id: row.get::<_, Option<String>>(2)?.map(SessionId::from),
        wake: group_column_from_sql("wake rule", &row.get::<_, String>(3)?)?,
        loser_disposition: group_column_from_sql("loser disposition", &row.get::<_, String>(4)?)?,
        expected_children: usize_from_sql("RuntimeEffectGroup", "expected_children", row.get(5)?)?,
        lifecycle: group_lifecycle_from_sql(&row.get::<_, String>(0)?, &lifecycle_json)?,
        created_at_ms: u64_from_sql("RuntimeEffectGroup", "created_at_ms", row.get(7)?)?,
    })
}

/// A persisted `lifecycle` column read back through the JSON contract
/// [`EffectGroupLifecycle`] defines; a value that does not decode is corrupt
/// data, never defaulted to `live` — that would silently re-open a closing
/// group to new claims.
pub(super) fn group_lifecycle_from_sql(
    group_key: &str,
    value: &str,
) -> rusqlite::Result<EffectGroupLifecycle> {
    serde_json::from_str(value).map_err(|error| {
        sqlite_conversion_error(stored_data_corrupt(
            "RuntimeEffectGroup",
            format!("group `{group_key}` lifecycle `{value}` does not decode: {error}"),
        ))
    })
}

/// A persisted group column read back through the same mapping that wrote it,
/// refusing a value no version of this runtime writes.
pub(super) fn group_column_from_sql<T: EffectGroupColumn>(
    column: &'static str,
    value: &str,
) -> rusqlite::Result<T> {
    EffectGroupColumn::from_column(value).ok_or_else(|| {
        sqlite_conversion_error(stored_data_corrupt(
            "RuntimeEffectGroup",
            format!("unknown effect group {column} `{value}`"),
        ))
    })
}

pub(super) fn usize_from_sql(
    record_kind: &'static str,
    column: &'static str,
    value: i64,
) -> rusqlite::Result<usize> {
    usize::try_from(value).map_err(|_| {
        sqlite_conversion_error(stored_data_corrupt(
            record_kind,
            format!("{column} must be non-negative, got {value}"),
        ))
    })
}

/// A grouped child whose group row is gone is a corrupt journal, not a
/// silently ungrouped settlement: the rank it should have taken can never be
/// served, so reporting success would hide a group no caller can finish
/// consuming.
pub(super) fn missing_group_row(group_key: &str) -> rusqlite::Error {
    sqlite_conversion_error(stored_data_corrupt(
        "RuntimeEffectGroup",
        format!(
            "grouped effect child finalized against missing group row `{group_key}`; \
             its settlement rank can never be served"
        ),
    ))
}

/// Decodes `commit_state, commit_seq` — the replay commit-protocol
/// projection — starting at column `base`, so the wider unsettled-children
/// read can share it. A `NULL` state is a left-joined miss (no replay row),
/// not corruption; a `commit_seq` under any other state is.
pub(super) fn decode_commit_state_columns(
    row: &rusqlite::Row<'_>,
    base: usize,
) -> rusqlite::Result<(Option<EffectCommitState>, Option<u64>)> {
    let word: Option<String> = row.get(base)?;
    let commit_seq: Option<i64> = row.get(base + 1)?;
    let corrupt = |detail: String| {
        sqlite_conversion_error(stored_data_corrupt("RuntimeEffectReplay", detail))
    };
    let commit_state = word
        .as_deref()
        .map(|word| {
            EffectCommitState::from_column(word)
                .ok_or_else(|| corrupt(format!("unknown commit_state `{word}`")))
        })
        .transpose()?;
    let commit_seq = commit_seq
        .map(|value| u64_from_sql("RuntimeEffectReplay", "commit_seq", value))
        .transpose()?;
    if commit_seq.is_some()
        && !matches!(
            commit_state,
            Some(EffectCommitState::Committed | EffectCommitState::Drained)
        )
    {
        return Err(corrupt(format!(
            "replay row carries commit_seq under commit_state {word:?}; the \
             column CHECK makes that unwritable"
        )));
    }
    Ok((commit_state, commit_seq))
}

/// The `query_row` form for a replay row known to exist: `commit_state` is
/// `NOT NULL`, so a `NULL` read is corruption, not a left-join miss.
pub(super) fn decode_child_arbitration(
    row: &rusqlite::Row<'_>,
    group_key: &str,
) -> rusqlite::Result<StoredChildArbitration> {
    let (commit_state, commit_seq) = decode_commit_state_columns(row, 0)?;
    Ok(StoredChildArbitration {
        group_key: group_key.to_string(),
        commit_state: commit_state.ok_or_else(|| {
            sqlite_conversion_error(stored_data_corrupt(
                "RuntimeEffectReplay",
                "replay row read back NULL commit_state; the column is NOT NULL".to_string(),
            ))
        })?,
        commit_seq,
    })
}

/// §4's admission fence: is the effect a fresh admission is minted under a
/// group child whose cancel disposition already committed?
///
/// Answered inside the claim's `BEGIN IMMEDIATE` transaction, so the read
/// serializes against `decide_cancel`'s replay-row write the same way the
/// commit-state CAS does — a claim can never observe a pre-decision parent
/// while its insert survives the decision's commit. `false` when the request
/// names no minting effect: the fence answers only the question §4 asks.
///
/// The minting reference is minted from a `GroupChildBinding` — the child's
/// own journaled address — so a named row that is missing or is no group
/// child is journal corruption, not "not a group child": a bound child's own
/// replay row cannot be absent, and silently admitting under it would run
/// nested work outside the fence the binding exists to enforce.
pub(super) fn minting_child_cancel_decided(
    tx: &rusqlite::Transaction<'_>,
    request: &EffectClaimRequest,
) -> rusqlite::Result<bool> {
    let Some(minting) = &request.minting_effect else {
        return Ok(false);
    };
    let corrupt = |detail: String| {
        sqlite_conversion_error(stored_data_corrupt("RuntimeEffectReplay", detail))
    };
    let arbitration = tx
        .query_row(
            effect_sql(Schema::Main)
                .replay
                .select_arbitration_by_key
                .sql(),
            params![minting.scope_id.as_str(), minting.replay_key.as_str()],
            |row| {
                let group_key: Option<String> = row.get(2)?;
                group_key
                    .as_deref()
                    .map(|group_key| decode_child_arbitration(row, group_key))
                    .transpose()
            },
        )
        .optional()?
        .ok_or_else(|| {
            corrupt(format!(
                "a bound group-child admission names minting replay row `{}` \
                 under scope `{}`, which does not exist; a bound child's row \
                 cannot be missing",
                minting.replay_key, minting.scope_id
            ))
        })?
        .ok_or_else(|| {
            corrupt(format!(
                "a bound group-child admission names minting replay row `{}` \
                 under scope `{}`, but that row is no group child; the binding \
                 derives from a retained membership and cannot name an \
                 ungrouped row",
                minting.replay_key, minting.scope_id
            ))
        })?;
    Ok(matches!(
        arbitration,
        StoredChildArbitration {
            commit_state: EffectCommitState::CancelDecided,
            ..
        }
    ))
}

/// One group child's commit-protocol state, inside a transaction — the
/// replay row addressed by its membership pair.
pub(super) fn select_child_arbitration(
    tx: &rusqlite::Transaction<'_>,
    group_key: &str,
    replay_key: &str,
) -> rusqlite::Result<Option<StoredChildArbitration>> {
    tx.query_row(
        effect_sql(Schema::Main)
            .replay
            .select_child_commit_state
            .sql(),
        params![group_key, replay_key],
        |row| decode_child_arbitration(row, group_key),
    )
    .optional()
}

/// The commit state and recorded drain input of one group child, inside a
/// transaction — what a retried boundary commit needs to answer
/// `AlreadyCommitted` with the winning commit's obligations.
pub(super) fn select_child_drain_input(
    tx: &rusqlite::Transaction<'_>,
    group_key: &str,
    replay_key: &str,
) -> rusqlite::Result<(Option<EffectCommitState>, Option<String>)> {
    tx.query_row(
        effect_sql(Schema::Main).replay.select_child_drain.sql(),
        params![group_key, replay_key],
        |row| {
            let (commit_state, _) = decode_commit_state_columns(row, 0)?;
            let drain_input: Option<String> = row.get(2)?;
            Ok((commit_state, drain_input))
        },
    )
    .optional()
    .map(|row| row.unwrap_or((None, None)))
}

/// The commit state alone: the classification read the arbitration paths
/// take when a guarded write missed.
pub(super) fn select_child_commit_state(
    tx: &rusqlite::Transaction<'_>,
    group_key: &str,
    replay_key: &str,
) -> rusqlite::Result<Option<EffectCommitState>> {
    Ok(select_child_arbitration(tx, group_key, replay_key)?
        .map(|arbitration| arbitration.commit_state))
}

/// The commit position a committed-or-drained child holds, for the
/// `FinalCommitted` refusal — the CHECK pairs the state with the position,
/// so a miss is corruption.
pub(super) fn read_child_commit_seq(
    tx: &rusqlite::Transaction<'_>,
    group_key: &str,
    replay_key: &str,
) -> rusqlite::Result<u64> {
    select_child_arbitration(tx, group_key, replay_key)?
        .and_then(|arbitration| arbitration.commit_seq)
        .ok_or_else(|| {
            sqlite_conversion_error(stored_data_corrupt(
                "RuntimeEffectReplay",
                format!(
                    "child `{replay_key}` of group {group_key} reads committed but \
                     carries no commit_seq; the column CHECK makes that unwritable"
                ),
            ))
        })
}

/// Why a fenced `finalize` missed: `true` when the row's `commit_state`
/// reads `cancel_decided` — the W17 typed refusal — `false` for an ordinary
/// fence loss.
pub(super) fn finalize_miss_cancel_decided(
    tx: &rusqlite::Transaction<'_>,
    fence: &EffectLeaseFence,
) -> rusqlite::Result<bool> {
    let state = tx
        .query_row(
            effect_sql(Schema::Main)
                .replay
                .select_group_commit_state
                .sql(),
            params![fence.scope_id.as_str(), fence.replay_key.as_str()],
            |row| row.get::<_, Option<String>>(1),
        )
        .optional()?
        .flatten();
    Ok(matches!(
        state.as_deref().and_then(EffectCommitState::from_column),
        Some(EffectCommitState::CancelDecided)
    ))
}

/// The rank a child's replay row is seated at, for the idempotent read-backs:
/// the rank the first `decide_cancel`/`discharge_child` wrote, which is the
/// only answer those outcomes may give.
pub(super) fn read_child_settlement_seq(
    tx: &rusqlite::Transaction<'_>,
    group_key: &str,
    replay_key: &str,
) -> rusqlite::Result<u64> {
    let group = select_group_record(tx, group_key)
        .optional()?
        .ok_or_else(|| missing_group_row(group_key))?;
    tx.query_row(
        effect_sql(Schema::Main).replay.select_settlement_seq.sql(),
        params![group.scope_id.as_str(), replay_key],
        |row| row.get::<_, Option<i64>>(0),
    )
    .optional()?
    .flatten()
    .ok_or_else(|| {
        sqlite_conversion_error(stored_data_corrupt(
            "RuntimeEffectReplay",
            format!(
                "child `{replay_key}` of group {group_key} was decided or drained \
                 but its replay row holds no settlement rank; the two are written \
                 in one transaction, so the journal is corrupt"
            ),
        ))
    })
    .and_then(|value| u64_from_sql("RuntimeEffectReplay", "settlement_seq", value))
}

/// Allocate the group's next settlement rank: `next_seq`, returned.
pub(super) fn bump_group_rank(
    tx: &rusqlite::Transaction<'_>,
    group_key: &str,
) -> rusqlite::Result<i64> {
    tx.query_row(
        effect_sql(Schema::Main).group.bump_next_seq.sql(),
        params![group_key],
        |row| row.get(0),
    )
    .optional()?
    .ok_or_else(|| missing_group_row(group_key))
}
