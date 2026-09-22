//! The [`EffectReplayRowStore`] port implementation for PostgreSQL: the
//! claim, group, arbitration and drain methods plus the transaction helpers
//! they share. Split from `postgres/effect_replay.rs` under the production
//! file-size budget; that file keeps the schema statements, the row-store
//! type and the driver/host open paths.

use super::*;

#[async_trait::async_trait]
impl EffectReplayRowStore for PostgresEffectReplayRowStore {
    fn vocabulary(&self) -> EffectReplayVocabulary {
        VOCABULARY
    }

    fn capabilities(&self) -> EffectReplayCapabilities {
        EffectReplayCapabilities {
            completion_keys: CompletionKeys::Issued,
            tool_batch_redrive: ToolBatchRedrive::ChildrenFirst,
        }
    }

    async fn claim(
        &self,
        request: &EffectClaimRequest,
    ) -> Result<EffectClaimObservation, RuntimeEffectControllerError> {
        let mut tx = self.pool.begin().await.map_err(effect_store_error)?;
        // Only session-free scopes can carry a retirement tombstone, so only
        // they take the scope lock retirement writes it under; a session scope
        // has nothing here to race with.
        if fence_session_free_scope(&mut tx, request.session_id.as_ref(), &request.scope_id).await?
        {
            tx.commit().await.map_err(effect_store_error)?;
            return Ok(EffectClaimObservation::ScopeRetired);
        }
        // The server's transaction clock is the authoritative lease instant:
        // one stable value for every comparison and derived expiry below.
        let now_ms = postgres_transaction_epoch_ms(&mut tx)
            .await
            .map_err(|err| effect_store_message(err.to_string()))?;
        let observation = self.claim_in_transaction(&mut tx, request, now_ms).await;
        tx.commit().await.map_err(effect_store_error)?;
        observation
    }

    async fn replay_row_exists(
        &self,
        scope_id: &str,
        replay_key: &str,
    ) -> Result<bool, RuntimeEffectControllerError> {
        sqlx::query_scalar(effect_sql().replay.exists_by_key.sql())
            .bind(scope_id)
            .bind(replay_key)
            .fetch_one(&self.pool)
            .await
            .map_err(effect_store_error)
    }

    /// Writes the terminal and, for a grouped child, takes its final-commit
    /// position at the §4 point — in the normative order (N1), in one
    /// transaction.
    ///
    /// The fenced `UPDATE` runs first and `RETURNING group_key` is what makes
    /// "allocate only on rowcount 1" structural rather than remembered: no row
    /// returned is no bump, and the group bumped is the one the child's own row
    /// records rather than one passed in beside it. The shared lock order —
    /// replay row, then group row — is what makes the commit-position bump
    /// and the commit-state CAS serialize against a concurrent
    /// `decide_cancel` or `discharge_child` without deadlock: each contestant
    /// must pass the child's replay row lock first.
    ///
    /// `UPDATE … SET next_commit_seq = next_commit_seq + 1` on a single row
    /// takes that row's lock and is correct under `READ COMMITTED`: no lost
    /// update, and no read of unfenced sibling state. It is also the group's
    /// serialization point — every sibling's finalize queues behind it — which
    /// ADR 0065 accepts with a pre-identified, backend-local escape (a
    /// per-group sequence generator or a sharded counter) that needs no
    /// contract movement.
    async fn finalize(
        &self,
        fence: &EffectLeaseFence,
        terminal: &EffectTerminal,
    ) -> Result<EffectFinalizeOutcome, RuntimeEffectControllerError> {
        let mut tx = self.pool.begin().await.map_err(effect_store_error)?;
        let claimed: Option<Option<String>> =
            sqlx::query_scalar(effect_sql().replay_postgres.finalize_terminal.sql())
                .bind(&fence.scope_id)
                .bind(&fence.replay_key)
                .bind(&fence.envelope_hash)
                .bind(&fence.owner_id)
                .bind(&fence.lease_token)
                .bind(terminal.status().column())
                .bind(terminal.outcome_json())
                .bind(terminal.error_json())
                .fetch_optional(&mut *tx)
                .await
                .map_err(effect_store_error)?;

        let Some(group_key) = claimed else {
            // The fenced write matched no row. A recorded `cancel_decided`
            // state turns the bare miss into W17's typed refusal; anything
            // else is the ordinary fence loss. Either way the transaction
            // wrote nothing, so it rolls back: reporting an observation
            // commits no writes, and no counter burned. Rolled back explicitly
            // rather than by drop, so the statement that discards the work is
            // the one an implementor reads.
            let outcome = if finalize_miss_cancel_decided(&mut tx, fence).await? {
                EffectFinalizeOutcome::CancelDecided
            } else {
                EffectFinalizeOutcome::FenceMoved
            };
            tx.rollback().await.map_err(effect_store_error)?;
            return Ok(outcome);
        };
        let Some(group_key) = group_key else {
            // No group to contest: the final record still wins the row's own
            // commit state, which `committed` is the honest value of.
            sqlx::query(effect_sql().replay.commit_ungrouped.sql())
                .bind(&fence.scope_id)
                .bind(&fence.replay_key)
                .execute(&mut *tx)
                .await
                .map_err(effect_store_error)?;
            tx.commit().await.map_err(effect_store_error)?;
            return Ok(EffectFinalizeOutcome::Written { commit_seq: None });
        };
        // The group row, then the replay row's commit-state CAS — the shared
        // lock order — so the position the winning CAS writes was allocated
        // under this transaction's group-row lock. A missing group row under
        // its own child is corruption, not a miss.
        let commit_seq: Option<i64> =
            sqlx::query_scalar(effect_sql().group.bump_next_commit_seq.sql())
                .bind(&group_key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(effect_store_error)?;
        let commit_seq = commit_seq.ok_or_else(|| missing_group_row(&group_key))?;
        let won = sqlx::query(effect_sql().replay.commit_child.sql())
            .bind(&fence.scope_id)
            .bind(&fence.replay_key)
            .bind(&group_key)
            .bind(commit_seq)
            .execute(&mut *tx)
            .await
            .map_err(effect_store_error)?
            .rows_affected();
        if won == 0 {
            // The point was taken while the replay row still read
            // `in_progress` to this transaction — only a committed cancel
            // decision explains that (a `committed` state is written only
            // with a terminal, which the fenced update would not have
            // matched). Roll back: the terminal, the bump and the state all
            // go away together.
            let outcome =
                match select_child_commit_state(&mut tx, &group_key, &fence.replay_key).await? {
                    Some(EffectCommitState::CancelDecided) => EffectFinalizeOutcome::CancelDecided,
                    other => {
                        return Err(replay_corrupt(format!(
                            "child `{}` of group {group_key} matched its in-progress fence \
                         but lost the commit-state CAS to {other:?}, which only a committed \
                         cancel decision may cause",
                            fence.replay_key
                        )));
                    }
                };
            tx.rollback().await.map_err(effect_store_error)?;
            return Ok(outcome);
        }
        tx.commit().await.map_err(effect_store_error)?;
        Ok(EffectFinalizeOutcome::Written {
            commit_seq: Some(u64::try_from(commit_seq).map_err(|_| {
                effect_store_message(
                    StoreError::StoredDataCorrupt {
                        record_kind: "RuntimeEffectGroup",
                        message: format!("next_commit_seq must be non-negative, got {commit_seq}"),
                    }
                    .to_string(),
                )
            })?),
        })
    }

    /// Commits the cancel disposition at the §4 point, or reports the state
    /// already there.
    ///
    /// The speculative terminal write runs *before* the commit-state CAS on
    /// purpose, not just for idempotence: the shared lock order is replay row
    /// then group row, and writing first is what keeps a concurrent
    /// `finalize` — which holds the replay row's lock while it reaches for
    /// the group counter — from deadlocking against this path. A CAS that
    /// loses rolls the speculative write and the rank bump back with the
    /// rest.
    async fn decide_cancel(
        &self,
        request: &EffectCancelRequest,
    ) -> Result<EffectCancelOutcome, RuntimeEffectControllerError> {
        let mut tx = self.pool.begin().await.map_err(effect_store_error)?;
        match select_child_commit_state(&mut tx, &request.group_key, &request.replay_key).await? {
            Some(EffectCommitState::Committed) | Some(EffectCommitState::Drained) => {
                let commit_seq =
                    read_child_commit_seq(&mut tx, &request.group_key, &request.replay_key).await?;
                tx.commit().await.map_err(effect_store_error)?;
                return Ok(EffectCancelOutcome::FinalCommitted { commit_seq });
            }
            Some(EffectCommitState::CancelDecided) => {
                let settlement_seq =
                    read_child_settlement_seq(&mut tx, &request.group_key, &request.replay_key)
                        .await?;
                tx.commit().await.map_err(effect_store_error)?;
                return Ok(EffectCancelOutcome::AlreadyDecided { settlement_seq });
            }
            // `pending` is the contestable state; `None` is an
            // accepted-but-never-claimed child — its membership row is the
            // admission, so the replay row gets inserted.
            Some(EffectCommitState::Pending) | None => {}
        }
        // Speculative terminal write under the replay-row lock. The group row
        // supplies scope and session; the request carries the canonical
        // envelope — the form the replay column's readers decode — which the
        // caller hashed from retained membership.
        let group = select_group_record(&mut tx, &request.group_key)
            .await?
            .ok_or_else(|| missing_group_row(&request.group_key))?;
        let error_json = request
            .terminal
            .error_json()
            .ok_or_else(|| {
                effect_store_message(
                    StoreError::StoredDataCorrupt {
                        record_kind: "RuntimeEffectReplay",
                        message: format!(
                            "cancel decision for child `{}` of group {} was asked to \
                             journal a non-failure terminal; the cancelled terminal is \
                             always a failure",
                            request.replay_key, request.group_key
                        ),
                    }
                    .to_string(),
                )
            })?
            .to_string();
        let now_ms = postgres_transaction_epoch_ms(&mut tx)
            .await
            .map_err(|err| effect_store_message(err.to_string()))?;
        let wrote = sqlx::query(effect_sql().replay.write_cancelled.sql())
            .bind(&group.scope_id)
            .bind(group.session_id.as_deref())
            .bind(&request.replay_key)
            .bind(&request.envelope_hash)
            .bind(&request.envelope_json)
            .bind(&error_json)
            .bind(&request.group_key)
            .bind(now_ms as i64)
            .execute(&mut *tx)
            .await
            .map_err(effect_store_error)?
            .rows_affected();
        // Group row, then the replay row's CAS: the rank bump ahead of it
        // keeps the shared lock order, and a losing CAS rolls the bump back
        // with the speculative terminal.
        let settlement_seq = bump_group_rank(&mut tx, &request.group_key).await?;
        let won = sqlx::query(effect_sql().replay.cancel_commit_state.sql())
            .bind(&group.scope_id)
            .bind(&request.replay_key)
            .bind(&request.group_key)
            .execute(&mut *tx)
            .await
            .map_err(effect_store_error)?
            .rows_affected();
        if won == 0 {
            let outcome =
                match select_child_commit_state(&mut tx, &request.group_key, &request.replay_key)
                    .await?
                {
                    Some(EffectCommitState::Committed) | Some(EffectCommitState::Drained) => {
                        EffectCancelOutcome::FinalCommitted {
                            commit_seq: read_child_commit_seq(
                                &mut tx,
                                &request.group_key,
                                &request.replay_key,
                            )
                            .await?,
                        }
                    }
                    Some(EffectCommitState::CancelDecided) => EffectCancelOutcome::AlreadyDecided {
                        settlement_seq: read_child_settlement_seq(
                            &mut tx,
                            &request.group_key,
                            &request.replay_key,
                        )
                        .await?,
                    },
                    other => {
                        return Err(replay_corrupt(format!(
                            "cancel CAS for child `{}` of group {} missed a commit \
                             state that reads back {other:?}",
                            request.replay_key, request.group_key
                        )));
                    }
                };
            tx.rollback().await.map_err(effect_store_error)?;
            return Ok(outcome);
        }
        // The CAS won, so the row read `pending` when the speculative write
        // ran: a terminal replay row then is a committed final with no
        // committed state — corruption the commit-state CAS exists to make
        // impossible.
        if wrote == 0 {
            return Err(replay_corrupt(format!(
                "cancel decision for child `{}` of group {} found the commit state \
                 pending but the replay row already terminal; a terminal child \
                 with no committed state is corruption the CAS exists to make \
                 impossible",
                request.replay_key, request.group_key
            )));
        }
        sqlx::query(effect_sql().replay.set_settlement_seq.sql())
            .bind(&group.scope_id)
            .bind(&request.replay_key)
            .bind(settlement_seq)
            .execute(&mut *tx)
            .await
            .map_err(effect_store_error)?;
        tx.commit().await.map_err(effect_store_error)?;
        Ok(EffectCancelOutcome::Decided {
            settlement_seq: u64::try_from(settlement_seq).map_err(|_| {
                effect_store_message(
                    StoreError::StoredDataCorrupt {
                        record_kind: "RuntimeEffectGroup",
                        message: format!("next_seq must be non-negative, got {settlement_seq}"),
                    }
                    .to_string(),
                )
            })?,
        })
    }

    /// Discharges a committed child's §5 drain: the `drained` state and the
    /// settlement rank in one transaction, behind the commit-order barrier.
    async fn discharge_child(
        &self,
        request: &EffectDischargeRequest,
    ) -> Result<EffectDischargeOutcome, RuntimeEffectControllerError> {
        let mut tx = self.pool.begin().await.map_err(effect_store_error)?;
        // Replay row first — the shared lock order — then group.
        let now_ms = postgres_transaction_epoch_ms(&mut tx)
            .await
            .map_err(|err| effect_store_message(err.to_string()))?;
        let touched = sqlx::query(effect_sql().replay.touch_for_discharge.sql())
            .bind(&request.scope_id)
            .bind(&request.replay_key)
            .bind(&request.group_key)
            .bind(now_ms as i64)
            .execute(&mut *tx)
            .await
            .map_err(effect_store_error)?
            .rows_affected();
        if touched == 0 {
            return Err(replay_corrupt(format!(
                "discharge for child `{}` of group {} found no replay row under \
                 scope `{}` carrying that group key; a committed child always \
                 has one",
                request.replay_key, request.group_key, request.scope_id
            )));
        }
        let Some(arbitration) =
            select_child_arbitration(&mut tx, &request.group_key, &request.replay_key).await?
        else {
            return Err(replay_corrupt(format!(
                "discharge for child `{}` of group {} found no commit state on a \
                 replay row the touch just wrote",
                request.replay_key, request.group_key
            )));
        };
        let commit_seq = match arbitration.commit_state {
            EffectCommitState::Drained => {
                let settlement_seq =
                    read_child_settlement_seq(&mut tx, &request.group_key, &request.replay_key)
                        .await?;
                tx.commit().await.map_err(effect_store_error)?;
                return Ok(EffectDischargeOutcome::AlreadyDischarged { settlement_seq });
            }
            EffectCommitState::Committed => arbitration.commit_seq.ok_or_else(|| {
                replay_corrupt(format!(
                    "discharge for child `{}` of group {} found commit_state \
                     'committed' with no commit_seq; the column CHECK makes that \
                     unwritable",
                    request.replay_key, request.group_key
                ))
            })?,
            other => {
                return Err(replay_corrupt(format!(
                    "discharge for child `{}` of group {} found commit_state {other:?}; \
                     only a committed child has a drain to discharge",
                    request.replay_key, request.group_key
                )));
            }
        };
        let blocked: bool =
            sqlx::query_scalar(effect_sql().replay.has_undrained_lower_commit.sql())
                .bind(&request.group_key)
                .bind(commit_seq as i64)
                .fetch_one(&mut *tx)
                .await
                .map_err(effect_store_error)?;
        if blocked {
            tx.rollback().await.map_err(effect_store_error)?;
            return Ok(EffectDischargeOutcome::Blocked);
        }
        // Group row, then the replay row's settle-and-drain write — the
        // shared lock order — onto the row whose lock the touch already
        // holds. A boundary-committed row carries no terminal yet, so its
        // discharge also seats the projected outcome.
        let settlement_seq = bump_group_rank(&mut tx, &request.group_key).await?;
        let stamped = match request.terminal.as_ref() {
            Some(terminal) => sqlx::query(effect_sql().replay.settle_drained_final.sql())
                .bind(&request.scope_id)
                .bind(&request.replay_key)
                .bind(&request.group_key)
                .bind(settlement_seq)
                .bind(terminal.status().column())
                .bind(terminal.outcome_json())
                .bind(terminal.error_json())
                .bind(now_ms as i64)
                .execute(&mut *tx)
                .await
                .map_err(effect_store_error)?
                .rows_affected(),
            None => sqlx::query(effect_sql().replay.settle_drained.sql())
                .bind(&request.scope_id)
                .bind(&request.replay_key)
                .bind(&request.group_key)
                .bind(settlement_seq)
                .execute(&mut *tx)
                .await
                .map_err(effect_store_error)?
                .rows_affected(),
        };
        if stamped == 0 {
            // A terminal-less discharge missing the settle guard means the row
            // is still mid-drain (`status = 'in_progress'`): the drain is
            // still owed, and the rank bump above must roll back with the
            // rest of the transaction.
            if request.terminal.is_none() {
                tx.rollback().await.map_err(effect_store_error)?;
                return Ok(EffectDischargeOutcome::Blocked);
            }
            return Err(replay_corrupt(format!(
                "discharge write for child `{}` of group {} missed a row read \
                 committed in the same transaction",
                request.replay_key, request.group_key
            )));
        }
        tx.commit().await.map_err(effect_store_error)?;
        Ok(EffectDischargeOutcome::Discharged {
            settlement_seq: u64::try_from(settlement_seq).map_err(|_| {
                effect_store_message(
                    StoreError::StoredDataCorrupt {
                        record_kind: "RuntimeEffectGroup",
                        message: format!("next_seq must be non-negative, got {settlement_seq}"),
                    }
                    .to_string(),
                )
            })?,
        })
    }

    async fn drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, RuntimeEffectControllerError> {
        sqlx::query_scalar(effect_sql().replay.has_undrained_lower_commit.sql())
            .bind(group_key)
            .bind(commit_seq as i64)
            .fetch_one(&self.pool)
            .await
            .map_err(effect_store_error)
    }

    /// Commits one group child's final record at the §4 point — the
    /// final-attempt boundary's durable half: `commit_state`, the allocated
    /// `commit_seq`, and the drain input, under the claim's lease fence.
    async fn commit_group_child(
        &self,
        request: &EffectGroupChildCommitRequest,
    ) -> Result<EffectGroupChildCommitOutcome, RuntimeEffectControllerError> {
        let mut tx = self.pool.begin().await.map_err(effect_store_error)?;
        // The row decides its own group: the request's assertion is checked
        // against what the row carries rather than trusted.
        let row = sqlx::query(
            effect_sql()
                .replay_postgres
                .select_arbitration_by_key_locked
                .sql(),
        )
        .bind(&request.scope_id)
        .bind(&request.replay_key)
        .fetch_optional(&mut *tx)
        .await
        .map_err(effect_store_error)?;
        let Some(row) = row else {
            return Err(replay_corrupt(format!(
                "a §4 commit for replay key `{}` under scope `{}` names a replay row \
                 that does not exist; a claimed child's row cannot be missing",
                request.replay_key, request.scope_id
            )));
        };
        let Some(group_key): Option<String> = row.get("group_key") else {
            tx.commit().await.map_err(effect_store_error)?;
            return Ok(EffectGroupChildCommitOutcome::Ungrouped);
        };
        if let Some(asserted) = &request.group_key
            && *asserted != group_key
        {
            return Err(replay_corrupt(format!(
                "final commit for child `{}` of scope `{}` asserted group {asserted} \
                 but the durable row carries {group_key}; the row is authoritative",
                request.replay_key, request.scope_id
            )));
        }
        let commit_state: Option<String> = row.get("commit_state");
        let commit_state = commit_state
            .as_deref()
            .and_then(EffectCommitState::from_column)
            .ok_or_else(|| {
                replay_corrupt(format!(
                    "child `{}` of group {} read back commit_state {:?}; the column \
                     is NOT NULL and CHECK-constrained",
                    request.replay_key, group_key, commit_state
                ))
            })?;
        let commit_seq = decode_optional_commit_seq(&row)?;
        let drain_input: Option<String> = row.get("drain_input");
        match commit_state {
            EffectCommitState::Committed | EffectCommitState::Drained => {
                let commit_seq = commit_seq.ok_or_else(|| {
                    replay_corrupt(format!(
                        "child `{}` of group {group_key} reads committed but carries \
                         no commit_seq; the column CHECK makes that unwritable",
                        request.replay_key
                    ))
                })?;
                tx.commit().await.map_err(effect_store_error)?;
                return Ok(EffectGroupChildCommitOutcome::AlreadyCommitted {
                    group_key,
                    commit_seq,
                    drain_input,
                });
            }
            EffectCommitState::CancelDecided => {
                // The schema CHECK forbids `commit_seq` on a cancel-decided
                // row; the rank the caller is owed is the settlement rank the
                // decision seated.
                let commit_seq =
                    read_child_settlement_seq(&mut tx, &group_key, &request.replay_key).await?;
                tx.commit().await.map_err(effect_store_error)?;
                return Ok(EffectGroupChildCommitOutcome::CancelDecided {
                    group_key,
                    commit_seq,
                });
            }
            EffectCommitState::Pending => {}
        }
        // Group row, then the replay row's CAS — the shared lock order. The
        // lease guard on the CAS is the claim fence: a row reclaimed under
        // another owner misses, and the re-read below decides whether a
        // winner or a fence loss explains it.
        let now_ms = postgres_transaction_epoch_ms(&mut tx)
            .await
            .map_err(|err| effect_store_message(err.to_string()))?;
        let commit_seq: Option<i64> =
            sqlx::query_scalar(effect_sql().group.bump_next_commit_seq.sql())
                .bind(&group_key)
                .fetch_optional(&mut *tx)
                .await
                .map_err(effect_store_error)?;
        let commit_seq = commit_seq.ok_or_else(|| missing_group_row(&group_key))?;
        let won = sqlx::query(effect_sql().replay.commit_child_final.sql())
            .bind(&request.scope_id)
            .bind(&request.replay_key)
            .bind(&group_key)
            .bind(commit_seq)
            .bind(&request.drain_input)
            .bind(&request.owner_id)
            .bind(now_ms as i64)
            .execute(&mut *tx)
            .await
            .map_err(effect_store_error)?
            .rows_affected();
        if won == 0 {
            // Re-read under the same transaction: a committed cancel decision
            // explains the miss, a committed sibling CAS is idempotent, and a
            // still-pending row means the lease moved — a fence loss the
            // caller surfaces as a conflict.
            let outcome = match select_child_commit_state(&mut tx, &group_key, &request.replay_key)
                .await?
            {
                Some(EffectCommitState::CancelDecided) => {
                    EffectGroupChildCommitOutcome::CancelDecided {
                        group_key: group_key.clone(),
                        commit_seq: read_child_commit_seq(&mut tx, &group_key, &request.replay_key)
                            .await?,
                    }
                }
                Some(EffectCommitState::Committed) | Some(EffectCommitState::Drained) => {
                    EffectGroupChildCommitOutcome::AlreadyCommitted {
                        group_key: group_key.clone(),
                        commit_seq: read_child_commit_seq(&mut tx, &group_key, &request.replay_key)
                            .await?,
                        drain_input: read_child_drain_input(
                            &mut tx,
                            &group_key,
                            &request.replay_key,
                        )
                        .await?,
                    }
                }
                Some(EffectCommitState::Pending) => {
                    return Err(RuntimeEffectControllerError::new(
                        lash_core::RuntimeErrorCode::PostgresEffectReplayLeaseLost,
                        format!(
                            "final commit for child `{}` of group {group_key} lost \
                             its lease fence: the row is still pending but the \
                             claiming owner no longer holds it",
                            request.replay_key
                        ),
                    ));
                }
                None => {
                    return Err(replay_corrupt(format!(
                        "final commit for child `{}` of group {group_key} found the \
                         commit state NULL after its own CAS missed; the column is \
                         NOT NULL",
                        request.replay_key
                    )));
                }
            };
            tx.rollback().await.map_err(effect_store_error)?;
            return Ok(outcome);
        }
        tx.commit().await.map_err(effect_store_error)?;
        Ok(EffectGroupChildCommitOutcome::Committed {
            group_key,
            commit_seq: u64::try_from(commit_seq).map_err(|_| {
                effect_store_message(
                    StoreError::StoredDataCorrupt {
                        record_kind: "RuntimeEffectGroup",
                        message: format!("next_commit_seq must be non-negative, got {commit_seq}"),
                    }
                    .to_string(),
                )
            })?,
        })
    }

    async fn read_group_child_arbitration(
        &self,
        scope_id: &str,
        replay_key: &str,
    ) -> Result<Option<StoredChildArbitration>, RuntimeEffectControllerError> {
        let row = sqlx::query(effect_sql().replay.select_arbitration_by_key.sql())
            .bind(scope_id)
            .bind(replay_key)
            .fetch_optional(&self.pool)
            .await
            .map_err(effect_store_error)?;
        match row {
            // `group_key IS NULL` is "not a group child" — the read answers
            // `None` for it exactly as it does for no row.
            Some(row) => match row.get::<Option<String>, _>("group_key") {
                Some(group_key) => decode_child_arbitration(&row, &group_key).map(Some),
                None => Ok(None),
            },
            _ => Ok(None),
        }
    }

    /// Records the group and reports the row **as it stands durably**, so a
    /// reopen is fenced against what the journal holds rather than against the
    /// opening process's memory.
    ///
    /// One statement, so one transaction, committed before any child of this
    /// group claims (N2) — the read-back rides the same statement through
    /// `RETURNING` for the insert and a second query only when the insert
    /// conflicted, and neither touches a child row. `DO NOTHING` rather than an
    /// upsert: reopening a group must not reset `next_seq`, which would re-seat
    /// recorded children at ranks a caller has already consumed.
    async fn open_group(
        &self,
        record: &EffectGroupRecord,
        membership: &[AcceptedGroupChild],
    ) -> Result<EffectGroupRecord, RuntimeEffectControllerError> {
        // One transaction so the retirement fence and the insert are read and
        // written under the scope lock (N4); it still holds no child-row lock,
        // so the N2 lock order against `finalize` is unchanged.
        let mut tx = self.pool.begin().await.map_err(effect_store_error)?;
        if fence_session_free_scope(&mut tx, record.session_id.as_ref(), &record.scope_id).await? {
            tx.commit().await.map_err(effect_store_error)?;
            return Err(effect_replay_driver::scope_retired(&record.scope_id));
        }
        // Children before the group row, in this transaction (ADR 0065 N2), so
        // the group row's existence implies its complete membership
        // (ADR 0099 §3). The returned row is not read: on a first open it is
        // what was just offered, and on a reopen the conflict path leaves it
        // empty — either way the membership a caller acts on is the one
        // `read_group_membership` reports.
        for child in membership {
            sqlx::query(effect_sql().group_child_postgres.insert_accepted.sql())
                .bind(&record.group_key)
                .bind(child.position as i64)
                .bind(&child.replay_key)
                .bind(&child.envelope_json)
                .bind(i64::from(child.command_version))
                .bind(record.created_at_ms as i64)
                .fetch_optional(&mut *tx)
                .await
                .map_err(effect_store_error)?;
        }
        let inserted = sqlx::query(effect_sql().group_postgres.insert_new.sql())
            .bind(&record.group_key)
            .bind(&record.scope_id)
            .bind(record.session_id.as_deref())
            .bind(record.wake.column())
            .bind(record.loser_disposition.column())
            .bind(record.expected_children as i64)
            .bind(record.created_at_ms as i64)
            .fetch_optional(&mut *tx)
            .await
            .map_err(effect_store_error)?;
        if let Some(row) = inserted {
            tx.commit().await.map_err(effect_store_error)?;
            return stored_group_record(row);
        }
        // The conflict path: some earlier open owns this key, and its row — not
        // the one just refused — is what a reopen must be fenced against.
        let existing = sqlx::query(effect_sql().group.select_by_key.sql())
            .bind(&record.group_key)
            .fetch_optional(&mut *tx)
            .await
            .map_err(effect_store_error)?
            .ok_or_else(|| missing_group_row(&record.group_key))?;
        tx.commit().await.map_err(effect_store_error)?;
        stored_group_record(existing)
    }

    async fn read_group_membership(
        &self,
        group_key: &str,
    ) -> Result<Vec<AcceptedGroupChild>, RuntimeEffectControllerError> {
        let rows = sqlx::query(effect_sql().group_child.select_membership.sql())
            .bind(group_key)
            .fetch_all(&self.pool)
            .await
            .map_err(effect_store_error)?;
        rows.into_iter()
            .map(|row| {
                Ok(AcceptedGroupChild {
                    position: u64_from_sql(
                        "RuntimeEffectGroupChild",
                        "position",
                        row.try_get::<i64, _>("position")
                            .map_err(effect_store_error)?,
                    )? as usize,
                    replay_key: row.try_get("replay_key").map_err(effect_store_error)?,
                    envelope_json: row.try_get("envelope_json").map_err(effect_store_error)?,
                    command_version: u64_from_sql(
                        "RuntimeEffectGroupChild",
                        "command_version",
                        row.try_get::<i64, _>("command_version")
                            .map_err(effect_store_error)?,
                    )? as u16,
                })
            })
            .collect()
    }

    /// Reads the group row without writing one, so a drain reads the declared
    /// disposition instead of inserting a group it was only asking about.
    async fn read_group(
        &self,
        group_key: &str,
    ) -> Result<Option<EffectGroupRecord>, RuntimeEffectControllerError> {
        let row = sqlx::query(effect_sql().group.select_by_key.sql())
            .bind(group_key)
            .fetch_optional(&self.pool)
            .await
            .map_err(effect_store_error)?;
        row.map(stored_group_record).transpose()
    }

    /// Anchored on the retained membership (ADR 0099 §3) and left-joined to
    /// the replay row, so a never-claimed child is reported too — it is
    /// unsettled work, and the drain is exactly who must see it. The
    /// arbitration columns come along so the reader can tell a
    /// committed-but-undrained child (a recovery obligation) from a torn one
    /// (corruption) and from work still in flight.
    async fn read_unsettled_group_children(
        &self,
        group_key: &str,
    ) -> Result<Vec<UnsettledGroupChild>, RuntimeEffectControllerError> {
        let rows = sqlx::query(effect_sql().replay.select_unsettled_children.sql())
            .bind(group_key)
            .fetch_all(&self.pool)
            .await
            .map_err(effect_store_error)?;
        rows.into_iter().map(unsettled_group_child).collect()
    }

    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: usize,
    ) -> Result<Option<StoredGroupSettlement>, RuntimeEffectControllerError> {
        let Some(offset) = rank.checked_sub(1) else {
            return Ok(None);
        };
        let row = sqlx::query(effect_sql().replay.select_settlement_by_rank.sql())
            .bind(group_key)
            .bind(offset as i64)
            .fetch_optional(&self.pool)
            .await
            .map_err(effect_store_error)?;
        row.map(stored_group_settlement).transpose()
    }

    async fn renew(
        &self,
        fence: &EffectLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<bool, RuntimeEffectControllerError> {
        let changed = sqlx::query(effect_sql().replay_postgres.renew_lease.sql())
            .bind(&fence.scope_id)
            .bind(&fence.replay_key)
            .bind(&fence.envelope_hash)
            .bind(&fence.owner_id)
            .bind(&fence.lease_token)
            .bind(lease_ttl_ms as i64)
            .execute(&self.pool)
            .await
            .map_err(effect_store_error)?
            .rows_affected();
        Ok(changed == 1)
    }

    /// Deletes the named children **and their groups in the same transaction**
    /// (N3), so no partially-retired group is ever visible.
    ///
    /// A settlement rank counts a group's recorded children, and it survives
    /// gaps only because allocation is monotonic and therefore appends above a
    /// consumed rank. A deletion *below* a consumed rank would shift ranks even
    /// though allocation never does, which is why the group row and its children
    /// go together or not at all. Both predicates select the same set: a group
    /// and its children are opened under one journal identity.
    ///
    /// The reported count stays the children, which is what this method has
    /// always reported and what a caller prunes against.
    async fn retire_journal(
        &self,
        retirement: &lash_core::EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        let retirement_error = |error: sqlx::Error| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::PostgresEffectJournalRetirement,
                error.to_string(),
            )
        };
        let sql = effect_sql();
        let (children_sql, membership_sql, groups_sql, key, fenced_scope) = match retirement {
            lash_core::EffectJournalRetirement::Session { session_id } => (
                sql.replay.delete_by_session.sql(),
                sql.group_child.delete_by_session.sql(),
                sql.group.delete_by_session.sql(),
                session_id.as_str().to_string(),
                None,
            ),
            #[expect(
                clippy::expect_used,
                reason = "`retired_scope` is `Some` for exactly the two variants this arm matches, and neither carries a session id whose validation could refuse the journal identity"
            )]
            lash_core::EffectJournalRetirement::Process { .. }
            | lash_core::EffectJournalRetirement::RuntimeOperation { .. } => {
                let scope = retirement
                    .retired_scope()
                    .expect("scope-exact retirements name their scope");
                let identity = scope.journal_identity().expect(
                    "process and runtime-operation scopes always form durable journal identities",
                );
                (
                    sql.replay.delete_by_scope.sql(),
                    sql.group_child.delete_by_scope.sql(),
                    sql.group.delete_by_scope.sql(),
                    identity.key().to_string(),
                    Some(scope),
                )
            }
        };
        let mut tx = self.pool.begin().await.map_err(retirement_error)?;
        // Scope-exact retirement (N4): tombstone first, under the scope lock
        // every admission path takes, then the rows; the promise rows go in
        // the same transaction so the fence and the deletions land together.
        if let Some(scope) = fenced_scope.as_ref() {
            lock_scope(&mut tx, &key).await.map_err(retirement_error)?;
            let has_closure_participant = scope_has_turn_cancel_closure_participant(&mut tx, &key)
                .await
                .map_err(retirement_error)?;
            if has_closure_participant {
                tx.rollback().await.map_err(retirement_error)?;
                return Err(effect_replay_driver::scope_not_quiescent(&key));
            }
            // The quiescence proof is read under the same scope lock the
            // fence is written under, so no child can start between the
            // proof and the deletions.
            let scope_json = serde_json::to_string(scope).map_err(|err| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::PostgresEffectJournalRetirement,
                    err.to_string(),
                )
            })?;
            if retirement.gate() == Some(lash_core::EffectRetirementGate::WhenQuiescent)
                && !scope_is_quiescent(&mut tx, &key, &scope_json)
                    .await
                    .map_err(retirement_error)?
            {
                tx.rollback().await.map_err(retirement_error)?;
                return Err(effect_replay_driver::scope_not_quiescent(&key));
            }
            let children = retire_scope_rows_tx(&mut tx, &key, &scope_json)
                .await
                .map_err(retirement_error)?;
            tx.commit().await.map_err(retirement_error)?;
            return Ok(children);
        }
        sqlx::query(sql.journal_postgres.insert_session_scope_fences.sql())
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(retirement_error)?;
        let children = sqlx::query(children_sql)
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(retirement_error)?
            .rows_affected();
        // Membership before the group rows it keys off: the statement selects
        // the retiring groups, so deleting them first would strand every
        // accepted request and leave it naming environment bytes this
        // retirement is about to reclaim (ADR 0099 §3).
        sqlx::query(membership_sql)
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(retirement_error)?;
        sqlx::query(groups_sql)
            .bind(&key)
            .execute(&mut *tx)
            .await
            .map_err(retirement_error)?;
        tx.commit().await.map_err(retirement_error)?;
        Ok(children as usize)
    }

    async fn reinstate_scope(&self, scope_id: &str) -> Result<(), RuntimeError> {
        let retirement_error = |error: sqlx::Error| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::PostgresEffectJournalRetirement,
                error.to_string(),
            )
        };
        let mut tx = self.pool.begin().await.map_err(retirement_error)?;
        lock_scope(&mut tx, scope_id)
            .await
            .map_err(retirement_error)?;
        sqlx::query(effect_sql().fence.delete_by_scope.sql())
            .bind(scope_id)
            .execute(&mut *tx)
            .await
            .map_err(retirement_error)?;
        tx.commit().await.map_err(retirement_error)
    }

    async fn pending_artifact_owner_retirements(
        &self,
    ) -> Result<Vec<lash_core::ExecutionScope>, RuntimeError> {
        let keys: Vec<String> = sqlx::query_scalar(
            effect_sql()
                .fence_postgres
                .select_pending_artifact_cleanup
                .sql(),
        )
        .fetch_all(&self.pool)
        .await
        .map_err(|error| {
            RuntimeError::new(
                lash_core::RuntimeErrorCode::PostgresEffectJournalRetirement,
                error.to_string(),
            )
        })?;
        keys.into_iter()
            .map(|key| {
                lash_core::ExecutionScope::from_journal_key(&key).ok_or_else(|| {
                    RuntimeError::new(
                        lash_core::RuntimeErrorCode::PostgresEffectJournalRetirement,
                        format!("invalid retired effect scope key `{key}`"),
                    )
                })
            })
            .collect()
    }

    async fn complete_artifact_owner_retirement(&self, scope_id: &str) -> Result<(), RuntimeError> {
        sqlx::query(effect_sql().fence_postgres.complete_artifact_cleanup.sql())
            .bind(scope_id)
            .execute(&self.pool)
            .await
            .map_err(|error| {
                RuntimeError::new(
                    lash_core::RuntimeErrorCode::PostgresEffectJournalRetirement,
                    error.to_string(),
                )
            })?;
        Ok(())
    }
}

impl PostgresEffectReplayRowStore {
    /// Read the row under its write lock, ask the transition table, and apply
    /// whatever write it prescribes.
    ///
    /// A fresh claim inserts with `ON CONFLICT DO NOTHING`: `FOR UPDATE` cannot
    /// lock a row that does not exist yet, so a concurrent inserter is detected
    /// by the conflict and the row is re-read under its lock. That re-read is
    /// decided by the same table, so a racing claimant sees `Busy` (or the
    /// terminal) rather than a second claim.
    async fn claim_in_transaction(
        &self,
        tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
        request: &EffectClaimRequest,
        now_ms: u64,
    ) -> Result<EffectClaimObservation, RuntimeEffectControllerError> {
        let row = select_effect_row_for_update(tx, &request.scope_id, &request.replay_key).await?;
        let decision = decide_effect_claim(row.as_ref(), request, now_ms);
        let stamp = match decision {
            EffectClaimDecision::Insert(stamp) => {
                // §4's admission fence, taken before the row exists: a minting
                // parent whose cancel decision committed refuses the new
                // admission. Replays and takeovers never reach this arm — they
                // are the already-admitted commands the decision protects.
                if minting_child_cancel_decided(tx, request).await? {
                    return Ok(EffectClaimObservation::MintingChildCancelled);
                }
                if insert_claimed_row(tx, request, &stamp).await? {
                    return Ok(EffectClaimObservation::Claimed {
                        due_at_ms: stamp.due_at_ms,
                    });
                }
                // A concurrent claimant inserted the row `FOR UPDATE` could not
                // lock because it did not exist yet. Re-read it under its lock
                // and let the same table decide again; the second decision can
                // no longer be `Insert`, so it settles as a takeover or a
                // report — never a second claim of a live lease.
                let Some(conflicted) =
                    select_effect_row_for_update(tx, &request.scope_id, &request.replay_key)
                        .await?
                else {
                    return Ok(EffectClaimObservation::CorruptRow {
                        defect: EffectRowDefect::VanishedUnderClaim,
                    });
                };
                match decide_effect_claim(Some(&conflicted), request, now_ms) {
                    EffectClaimDecision::TakeOver(stamp) => stamp,
                    EffectClaimDecision::Report(observation) => return Ok(observation),
                    EffectClaimDecision::Insert(_) => {
                        debug_assert!(
                            false,
                            "decide_effect_claim must never prescribe an insert for a row it \
                             was given: `Insert` is the no-row arm"
                        );
                        return Ok(EffectClaimObservation::CorruptRow {
                            defect: EffectRowDefect::VanishedUnderClaim,
                        });
                    }
                }
            }
            EffectClaimDecision::TakeOver(stamp) => stamp,
            EffectClaimDecision::Report(observation) => return Ok(observation),
        };
        take_over_expired_lease(tx, request, &stamp).await?;
        Ok(EffectClaimObservation::Claimed {
            due_at_ms: stamp.due_at_ms,
        })
    }
}

async fn select_effect_row_for_update(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    scope_id: &str,
    replay_key: &str,
) -> Result<Option<StoredEffectRow>, RuntimeEffectControllerError> {
    let row = sqlx::query(effect_sql().replay_postgres.select_for_claim.sql())
        .bind(scope_id)
        .bind(replay_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(effect_store_error)?;
    row.map(stored_effect_row).transpose()
}

fn stored_effect_row(row: PgRow) -> Result<StoredEffectRow, RuntimeEffectControllerError> {
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
async fn insert_claimed_row(
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

async fn take_over_expired_lease(
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

fn stored_group_settlement(
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
fn stored_group_record(row: PgRow) -> Result<EffectGroupRecord, RuntimeEffectControllerError> {
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
        created_at_ms: u64::try_from(created_at_ms).map_err(|_| {
            group_corrupt(format!(
                "created_at_ms must be non-negative, got {created_at_ms}"
            ))
        })?,
    })
}

/// A persisted group column read back through the same mapping that wrote it,
/// refusing a value no version of this runtime writes.
fn group_column<T: EffectGroupColumn>(
    column: &'static str,
    value: String,
) -> Result<T, RuntimeEffectControllerError> {
    EffectGroupColumn::from_column(&value)
        .ok_or_else(|| group_corrupt(format!("unknown effect group {column} `{value}`")))
}

fn group_corrupt(message: String) -> RuntimeEffectControllerError {
    effect_store_message(
        StoreError::StoredDataCorrupt {
            record_kind: "RuntimeEffectGroup",
            message,
        }
        .to_string(),
    )
}

/// A corrupt membership row, labeled for the table the row came from.
fn group_child_corrupt(message: String) -> RuntimeEffectControllerError {
    effect_store_message(
        StoreError::StoredDataCorrupt {
            record_kind: "RuntimeEffectGroupChild",
            message,
        }
        .to_string(),
    )
}

fn unsettled_group_child(row: PgRow) -> Result<UnsettledGroupChild, RuntimeEffectControllerError> {
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
fn replay_corrupt(message: String) -> RuntimeEffectControllerError {
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
fn decode_optional_commit_state(
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
fn decode_optional_commit_seq(row: &PgRow) -> Result<Option<u64>, RuntimeEffectControllerError> {
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
fn decode_child_arbitration(
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
async fn select_child_commit_state(
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
async fn select_child_arbitration(
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
async fn read_child_drain_input(
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
async fn read_child_commit_seq(
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
async fn minting_child_cancel_decided(
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
async fn finalize_miss_cancel_decided(
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
async fn select_group_record(
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
async fn read_child_settlement_seq(
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
async fn bump_group_rank(
    tx: &mut sqlx::Transaction<'_, sqlx::Postgres>,
    group_key: &str,
) -> Result<i64, RuntimeEffectControllerError> {
    sqlx::query_scalar(effect_sql().group.bump_next_seq.sql())
        .bind(group_key)
        .fetch_optional(&mut **tx)
        .await
        .map_err(effect_store_error)?
        .ok_or_else(|| missing_group_row(group_key))
}

/// A grouped child whose group row is gone is a corrupt journal, not a silently
/// ungrouped settlement: the rank it should have taken can never be served, so
/// reporting success would hide a group no caller can finish consuming.
fn missing_group_row(group_key: &str) -> RuntimeEffectControllerError {
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
async fn fence_session_free_scope(
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
