//! The [`EffectReplayRowStore`] port implementation for SQLite: the claim,
//! group, arbitration and drain methods plus the transaction helpers they
//! share. Split from `effect_replay.rs` under the production file-size
//! budget; `effect_replay.rs` keeps the schema statements, the row-store
//! type and the driver/host open paths.

use super::*;

mod tx;

use tx::*;

#[async_trait::async_trait]
impl EffectReplayRowStore for SqliteEffectReplayRowStore {
    fn vocabulary(&self) -> EffectReplayVocabulary {
        VOCABULARY
    }

    /// The process-wide notifier for this backend and subject; see
    /// [`JournalWakeKey`] for which writers it reaches.
    async fn journal_wake(
        &self,
        subject: EffectJournalSubject<'_>,
    ) -> Result<EffectJournalWake, RuntimeEffectControllerError> {
        Ok(EffectJournalWake {
            notify: EffectJournalNotifiers::notifier(&self.wake.identity, subject),
            writers: self.wake.writers,
        })
    }

    async fn claim(
        &self,
        request: &EffectClaimRequest,
    ) -> Result<EffectClaimObservation, RuntimeEffectControllerError> {
        let request = request.clone();
        let clock = Arc::clone(&self.clock);
        let fences = self.fence_locations().await?;
        self.conn
            .write(move |tx| {
                // The retirement fence is read under the same `BEGIN IMMEDIATE`
                // lock retirement writes it under (on every attached file), so
                // a claim can never slip between a scope's tombstone and its
                // row deletions.
                if fences.is_fenced(tx, &request.scope_id)? {
                    return Ok(EffectClaimObservation::ScopeRetired);
                }
                let row = select_effect_row(tx, &request.scope_id, &request.replay_key)?;
                // Queueing and writer admission must not consume the new lease.
                let now_ms = clock.timestamp_ms();
                Ok(match decide_effect_claim(row.as_ref(), &request, now_ms) {
                    EffectClaimDecision::Insert(stamp) => {
                        // §4's admission fence, inside the same `BEGIN
                        // IMMEDIATE` the insert runs under: a minting parent
                        // whose cancel decision committed refuses the new
                        // admission before any row exists for it. Replays and
                        // takeovers never reach this arm — they are the
                        // already-admitted commands the decision protects.
                        if minting_child_cancel_decided(tx, &request)? {
                            return Ok(EffectClaimObservation::MintingChildCancelled);
                        }
                        insert_claimed_row(tx, &request, &stamp)?;
                        EffectClaimObservation::Claimed {
                            due_at_ms: stamp.due_at_ms,
                        }
                    }
                    EffectClaimDecision::TakeOver(stamp) => {
                        take_over_expired_lease(tx, &request, &stamp)?;
                        EffectClaimObservation::Claimed {
                            due_at_ms: stamp.due_at_ms,
                        }
                    }
                    EffectClaimDecision::Report(observation) => observation,
                })
            })
            .await
            .map_err(effect_sqlite_error)
    }

    async fn replay_row_exists(
        &self,
        scope_id: &str,
        replay_key: &str,
    ) -> Result<bool, RuntimeEffectControllerError> {
        let scope_id = scope_id.to_string();
        let replay_key = replay_key.to_string();
        self.conn
            .call(move |connection| {
                connection.query_row(
                    effect_sql(Schema::Main).replay.exists_by_key.sql(),
                    params![scope_id, replay_key],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(effect_sqlite_error)
    }

    async fn recorded_keys_in_range(
        &self,
        scope_id: &str,
        range: &RecordedKeyRange,
    ) -> Result<RecordedKeys, RuntimeEffectControllerError> {
        let scope_id = scope_id.to_string();
        let RecordedKeyRange {
            lower,
            upper,
            group_key_prefix,
        } = range.clone();
        self.conn
            .call(move |connection| {
                let read = |sql: &str, lower: &str, upper: &str| -> rusqlite::Result<Vec<String>> {
                    let mut statement = connection.prepare_cached(sql)?;
                    let rows = statement.query_map(params![scope_id, lower, upper], |row| {
                        row.get::<_, String>(0)
                    })?;
                    rows.collect()
                };
                let sql = effect_sql(Schema::Main);
                let replay_keys = read(sql.replay.select_keys_in_range.sql(), &lower, &upper)?;
                let group_keys = read(
                    sql.group.select_keys_in_range.sql(),
                    &format!("{group_key_prefix}{lower}"),
                    &format!("{group_key_prefix}{upper}"),
                )?
                .into_iter()
                .map(|key| {
                    key.strip_prefix(group_key_prefix.as_str())
                        .map_or_else(|| key.clone(), str::to_string)
                })
                .collect();
                let closing_outcome = connection
                    .query_row(
                        sql.replay.select_completed_outcome_by_key.sql(),
                        params![scope_id, upper],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                Ok(RecordedKeys {
                    replay_keys,
                    group_keys,
                    closing_outcome,
                })
            })
            .await
            .map_err(effect_sqlite_error)
    }

    async fn discard_reexecuted_row(
        &self,
        scope_id: &str,
        replay_key: &str,
    ) -> Result<(), RuntimeEffectControllerError> {
        let (scope, key) = (scope_id.to_string(), replay_key.to_string());
        let deleted = self
            .conn
            .call(move |connection| {
                connection.execute(
                    effect_sql(Schema::Main)
                        .replay
                        .delete_ungrouped_by_key
                        .sql(),
                    params![scope, key],
                )
            })
            .await
            .map_err(effect_sqlite_error)?;
        if deleted > 0 {
            self.announce_row(scope_id, replay_key);
        }
        Ok(())
    }

    /// Writes the terminal and, for a grouped child, contests the group's §4
    /// linearization point — in the normative order (N1, extended by ADR 0099).
    ///
    /// The fenced `UPDATE` runs first and `RETURNING group_key` is what makes
    /// "allocate only on rowcount 1" structural rather than remembered: no row
    /// returned is no state CAS and no counter bump, and the group contested
    /// is the one the child's own row records. On a hit the same transaction
    /// bumps `next_commit_seq` and CAS-wins the replay row's `commit_state`
    /// with the returned position; a CAS loss — only a committed cancel
    /// decision can cause it — rolls the whole transaction back, so the
    /// refused final writes nothing: not the terminal, not the state, not a
    /// burned commit position (W17).
    ///
    /// Settlement rank is deliberately absent here: ADR 0099 §5 allocates it
    /// at discharge ([`discharge_child`](Self::discharge_child)), after the
    /// child's declared intents are durable.
    ///
    /// Everything runs inside one `BEGIN IMMEDIATE` write transaction, so the
    /// decision cannot interleave with a sibling's regardless.
    async fn release_uncommitted_derivation(
        &self,
        fence: &EffectLeaseFence,
    ) -> Result<bool, RuntimeEffectControllerError> {
        let released = fence.clone();
        let clock = Arc::clone(&self.clock);
        let released = self
            .conn
            .write(move |tx| {
                let fence = released;
                let now = clock.timestamp_ms();
                let changed = tx.execute(
                    effect_sql(Schema::Main)
                        .replay_sqlite
                        .release_uncommitted_derivation
                        .sql(),
                    params![
                        fence.scope_id,
                        fence.replay_key,
                        fence.envelope_hash,
                        fence.owner_id,
                        fence.lease_token,
                        now as i64
                    ],
                )?;
                Ok(changed == 1)
            })
            .await
            .map_err(effect_sqlite_error)?;
        if released {
            self.announce_row(&fence.scope_id, &fence.replay_key);
        }
        Ok(released)
    }

    async fn finalize(
        &self,
        fence: &EffectLeaseFence,
        terminal: &EffectTerminal,
    ) -> Result<EffectFinalizeOutcome, RuntimeEffectControllerError> {
        let written = fence.clone();
        let status = terminal.status().column();
        let outcome_json = terminal.outcome_json().map(str::to_string);
        let error_json = terminal.error_json().map(str::to_string);
        let clock = Arc::clone(&self.clock);
        let (settled_group, outcome) = self
            .conn
            .write_flow(move |tx| {
                let fence = written;
                let now = clock.timestamp_ms();
                let claimed: Option<Option<String>> = tx
                    .query_row(
                        effect_sql(Schema::Main)
                            .replay_sqlite
                            .finalize_terminal
                            .sql(),
                        params![
                            fence.scope_id.as_str(),
                            fence.replay_key.as_str(),
                            fence.envelope_hash.as_str(),
                            fence.owner_id.as_str(),
                            fence.lease_token.as_str(),
                            status,
                            outcome_json,
                            error_json,
                            now as i64,
                            now as i64,
                        ],
                        |row| row.get(0),
                    )
                    .optional()?;
                let Some(group_key) = claimed else {
                    // The fenced write matched no row. A recorded
                    // `cancel_decided` state turns the bare miss into W17's
                    // typed refusal; anything else is the ordinary fence
                    // loss. Either way the transaction wrote nothing, so it
                    // rolls back: reporting an observation commits no writes,
                    // and no counter burned.
                    return Ok(TxOutcome::Rollback((
                        None,
                        if finalize_miss_cancel_decided(tx, &fence)? {
                            EffectFinalizeOutcome::CancelDecided
                        } else {
                            EffectFinalizeOutcome::FenceMoved
                        },
                    )));
                };
                let Some(group_key) = group_key else {
                    // No group to contest: the final record still wins the
                    // row's own commit state, which `committed` is the honest
                    // value of — `pending` means "the §4 point is open" and
                    // an ungrouped terminal row's is not.
                    tx.execute(
                        effect_sql(Schema::Main).replay.commit_ungrouped.sql(),
                        params![fence.scope_id.as_str(), fence.replay_key.as_str()],
                    )?;
                    return Ok(TxOutcome::Commit((
                        None,
                        EffectFinalizeOutcome::Written { commit_seq: None },
                    )));
                };
                // The group row, then the replay row's commit-state CAS —
                // the shared lock order — so the position the winning CAS
                // writes was allocated under this transaction's group-row
                // lock. A miss here is a group row gone under its own child:
                // corrupt.
                let commit_seq: i64 = tx
                    .query_row(
                        effect_sql(Schema::Main).group.bump_next_commit_seq.sql(),
                        params![group_key.as_str()],
                        |row| row.get(0),
                    )
                    .optional()?
                    .ok_or_else(|| missing_group_row(&group_key))?;
                let won = tx.execute(
                    effect_sql(Schema::Main).replay.commit_child.sql(),
                    params![
                        fence.scope_id.as_str(),
                        fence.replay_key.as_str(),
                        group_key.as_str(),
                        commit_seq,
                    ],
                )?;
                if won == 0 {
                    // The point was taken while the replay row still read
                    // `in_progress` to this transaction — only a committed
                    // cancel decision explains that (a `committed` state is
                    // written only with a terminal, which the fenced update
                    // would not have matched). Roll back: the terminal, the
                    // bump and the state all go away together.
                    let outcome =
                        match select_child_commit_state(tx, &group_key, &fence.replay_key)? {
                            Some(EffectCommitState::CancelDecided) => {
                                EffectFinalizeOutcome::CancelDecided
                            }
                            other => {
                                return Err(sqlite_conversion_error(stored_data_corrupt(
                                    "RuntimeEffectReplay",
                                    format!(
                                        "child `{}` of group {group_key} matched its \
                                     in-progress fence but lost the commit-state CAS \
                                     to {other:?}, which only a committed cancel \
                                     decision may cause",
                                        fence.replay_key
                                    ),
                                )));
                            }
                        };
                    return Ok(TxOutcome::Rollback((None, outcome)));
                }
                Ok(TxOutcome::Commit((
                    Some(group_key),
                    EffectFinalizeOutcome::Written {
                        commit_seq: Some(u64_from_sql(
                            "RuntimeEffectGroup",
                            "next_commit_seq",
                            commit_seq,
                        )?),
                    },
                )))
            })
            .await
            .map_err(effect_sqlite_error)?;
        if matches!(outcome, EffectFinalizeOutcome::Written { .. }) {
            self.announce_row(&fence.scope_id, &fence.replay_key);
        }
        if let Some(group_key) = settled_group {
            self.announce(EffectJournalSubject::Group {
                group_key: &group_key,
            });
        }
        Ok(outcome)
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
        let request = request.clone();
        let group_key = request.group_key.clone();
        let clock = Arc::clone(&self.clock);
        let replay_key = request.replay_key.clone();
        let (decided_scope, outcome) = self
            .conn
            .write_flow(move |tx| {
                let now = clock.timestamp_ms();
                match select_child_commit_state(tx, &request.group_key, &request.replay_key)? {
                    Some(EffectCommitState::Committed) | Some(EffectCommitState::Drained) => {
                        let commit_seq =
                            read_child_commit_seq(tx, &request.group_key, &request.replay_key)?;
                        return Ok(TxOutcome::Commit((
                            None,
                            EffectCancelOutcome::FinalCommitted { commit_seq },
                        )));
                    }
                    Some(EffectCommitState::CancelDecided) => {
                        let settlement_seq =
                            read_child_settlement_seq(tx, &request.group_key, &request.replay_key)?;
                        return Ok(TxOutcome::Commit((
                            None,
                            EffectCancelOutcome::AlreadyDecided { settlement_seq },
                        )));
                    }
                    // `pending` is the contestable state; `None` is an
                    // accepted-but-never-claimed child — its membership row
                    // is the admission, so the replay row gets inserted.
                    Some(EffectCommitState::Pending) | None => {}
                }
                // Speculative terminal write under the replay-row lock. The
                // group row supplies scope and session; the request carries the
                // canonical envelope — the form the replay column's readers
                // decode — which the caller hashed from retained membership.
                let group = select_group_record(tx, &request.group_key)
                    .optional()?
                    .ok_or_else(|| missing_group_row(&request.group_key))?;
                let error_json = request
                    .terminal
                    .error_json()
                    .map(str::to_string)
                    .ok_or_else(|| {
                        sqlite_conversion_error(stored_data_corrupt(
                            "RuntimeEffectReplay",
                            format!(
                                "cancel decision for child `{}` of group {} was asked \
                                 to journal a non-failure terminal; the cancelled \
                                 terminal is always a failure",
                                request.replay_key, request.group_key
                            ),
                        ))
                    })?;
                let wrote = tx.execute(
                    effect_sql(Schema::Main).replay.write_cancelled.sql(),
                    params![
                        group.scope_id.as_str(),
                        group.session_id.as_deref(),
                        request.replay_key.as_str(),
                        request.envelope_hash.as_str(),
                        request.envelope_json.as_str(),
                        error_json.as_str(),
                        request.group_key.as_str(),
                        now as i64,
                    ],
                )?;
                // Group row, then the replay row's CAS: the rank bump ahead
                // of it keeps the shared lock order, and a losing CAS rolls
                // the bump back with the speculative terminal. The rank rides
                // the CAS itself — the rank-pairing CHECK admits no
                // `cancel_decided` row without a rank, even transiently.
                let settlement_seq = bump_group_rank(tx, &request.group_key)?;
                let won = tx.execute(
                    effect_sql(Schema::Main).replay.cancel_commit_state.sql(),
                    params![
                        group.scope_id.as_str(),
                        request.replay_key.as_str(),
                        request.group_key.as_str(),
                        settlement_seq,
                    ],
                )?;
                if won == 0 {
                    let outcome = match select_child_commit_state(
                        tx,
                        &request.group_key,
                        &request.replay_key,
                    )? {
                        Some(EffectCommitState::Committed) | Some(EffectCommitState::Drained) => {
                            EffectCancelOutcome::FinalCommitted {
                                commit_seq: read_child_commit_seq(
                                    tx,
                                    &request.group_key,
                                    &request.replay_key,
                                )?,
                            }
                        }
                        Some(EffectCommitState::CancelDecided) => {
                            EffectCancelOutcome::AlreadyDecided {
                                settlement_seq: read_child_settlement_seq(
                                    tx,
                                    &request.group_key,
                                    &request.replay_key,
                                )?,
                            }
                        }
                        other => {
                            return Err(sqlite_conversion_error(stored_data_corrupt(
                                "RuntimeEffectReplay",
                                format!(
                                    "cancel CAS for child `{}` of group {} missed a \
                                     commit state that reads back {other:?}",
                                    request.replay_key, request.group_key
                                ),
                            )));
                        }
                    };
                    return Ok(TxOutcome::Rollback((None, outcome)));
                }
                // The CAS won, so the row read `pending` when the speculative
                // write ran: a terminal replay row then is a committed final
                // with no committed state — corruption the commit-state CAS
                // exists to make impossible.
                if wrote == 0 {
                    return Err(sqlite_conversion_error(stored_data_corrupt(
                        "RuntimeEffectReplay",
                        format!(
                            "cancel decision for child `{}` of group {} found the \
                             commit state pending but the replay row already \
                             terminal; a terminal child with no committed state \
                             is corruption the CAS exists to make impossible",
                            request.replay_key, request.group_key
                        ),
                    )));
                }
                // §4, W17: the decision closes the child's completion key in
                // this same transaction, so no resolve lands between them.
                if let Some(fence) = &request.completion_fence {
                    tx.execute(
                        crate::await_event::wait_sql(Schema::Main)
                            .shared
                            .fence_cancel_decided
                            .sql(),
                        params![
                            fence.key_id.as_str(),
                            fence.identity.scope_json.as_str(),
                            fence.identity.wait_json.as_str(),
                            fence.identity.session_id.as_deref(),
                            fence.identity.turn_control,
                            fence.terminal_json,
                            now as i64,
                        ],
                    )?;
                }
                Ok(TxOutcome::Commit((
                    Some(group.scope_id),
                    EffectCancelOutcome::Decided {
                        settlement_seq: u64_from_sql(
                            "RuntimeEffectGroup",
                            "next_seq",
                            settlement_seq,
                        )?,
                    },
                )))
            })
            .await
            .map_err(effect_sqlite_error)?;
        if let Some(scope_id) = decided_scope {
            self.announce_row(&scope_id, &replay_key);
            self.announce(EffectJournalSubject::Group {
                group_key: &group_key,
            });
        }
        Ok(outcome)
    }

    /// Discharges a committed child's §5 drain: the `drained` state and the
    /// settlement rank in one transaction, behind the commit-order barrier.
    async fn discharge_child(
        &self,
        request: &EffectDischargeRequest,
    ) -> Result<EffectDischargeOutcome, RuntimeEffectControllerError> {
        let request = request.clone();
        let (group_key, scope_id, replay_key) = (
            request.group_key.clone(),
            request.scope_id.clone(),
            request.replay_key.clone(),
        );
        let seats_terminal = request.terminal.is_some();
        let clock = Arc::clone(&self.clock);
        let outcome = self
            .conn
            .write_flow(move |tx| {
                let now = clock.timestamp_ms();
                // Replay row first — the shared lock order — then group.
                let touched = tx.execute(
                    effect_sql(Schema::Main).replay.touch_for_discharge.sql(),
                    params![
                        request.scope_id.as_str(),
                        request.replay_key.as_str(),
                        request.group_key.as_str(),
                        now as i64,
                    ],
                )?;
                if touched == 0 {
                    return Err(sqlite_conversion_error(stored_data_corrupt(
                        "RuntimeEffectReplay",
                        format!(
                            "discharge for child `{}` of group {} found no replay \
                             row under scope `{}` carrying that group key; a \
                             committed child always has one",
                            request.replay_key, request.group_key, request.scope_id
                        ),
                    )));
                }
                let Some(arbitration) =
                    select_child_arbitration(tx, &request.group_key, &request.replay_key)?
                else {
                    return Err(sqlite_conversion_error(stored_data_corrupt(
                        "RuntimeEffectReplay",
                        format!(
                            "discharge for child `{}` of group {} found no commit \
                             state on a replay row the touch just wrote",
                            request.replay_key, request.group_key
                        ),
                    )));
                };
                let commit_seq = match arbitration.commit_state {
                    EffectCommitState::Drained => {
                        let settlement_seq =
                            read_child_settlement_seq(tx, &request.group_key, &request.replay_key)?;
                        return Ok(TxOutcome::Commit(
                            EffectDischargeOutcome::AlreadyDischarged { settlement_seq },
                        ));
                    }
                    EffectCommitState::Committed => arbitration.commit_seq.ok_or_else(|| {
                        sqlite_conversion_error(stored_data_corrupt(
                            "RuntimeEffectReplay",
                            format!(
                                "discharge for child `{}` of group {} found \
                                 commit_state 'committed' with no commit_seq; the \
                                 column CHECK makes that unwritable",
                                request.replay_key, request.group_key
                            ),
                        ))
                    })?,
                    other => {
                        return Err(sqlite_conversion_error(stored_data_corrupt(
                            "RuntimeEffectReplay",
                            format!(
                                "discharge for child `{}` of group {} found \
                                 commit_state {:?}; only a committed child has a \
                                 drain to discharge",
                                request.replay_key, request.group_key, other
                            ),
                        )));
                    }
                };
                if tx.query_row(
                    effect_sql(Schema::Main)
                        .replay
                        .has_undrained_lower_commit
                        .sql(),
                    params![request.group_key.as_str(), commit_seq as i64],
                    |row| row.get::<_, bool>(0),
                )? {
                    return Ok(TxOutcome::Rollback(EffectDischargeOutcome::Blocked));
                }
                // Group row, then the replay row's settle-and-drain write —
                // the shared lock order — onto the row whose lock the touch
                // already holds. A boundary-committed row carries no terminal
                // yet, so its discharge also seats the projected outcome.
                let settlement_seq = bump_group_rank(tx, &request.group_key)?;
                let stamped = match request.terminal.as_ref() {
                    Some(terminal) => tx.execute(
                        effect_sql(Schema::Main).replay.settle_drained_final.sql(),
                        params![
                            request.scope_id.as_str(),
                            request.replay_key.as_str(),
                            request.group_key.as_str(),
                            settlement_seq,
                            terminal.status().column(),
                            terminal.outcome_json(),
                            terminal.error_json(),
                            now as i64,
                        ],
                    )?,
                    None => tx.execute(
                        effect_sql(Schema::Main).replay.settle_drained.sql(),
                        params![
                            request.scope_id.as_str(),
                            request.replay_key.as_str(),
                            request.group_key.as_str(),
                            settlement_seq,
                        ],
                    )?,
                };
                if stamped == 0 {
                    // A terminal-less discharge missing the settle guard means
                    // the row is still mid-drain (`status = 'in_progress'`):
                    // the drain is still owed, and the rank bump above must
                    // roll back with the rest of the transaction.
                    if request.terminal.is_none() {
                        return Ok(TxOutcome::Rollback(EffectDischargeOutcome::Blocked));
                    }
                    return Err(sqlite_conversion_error(stored_data_corrupt(
                        "RuntimeEffectReplay",
                        format!(
                            "discharge write for child `{}` of group {} missed a \
                             row read committed in the same transaction",
                            request.replay_key, request.group_key
                        ),
                    )));
                }
                Ok(TxOutcome::Commit(EffectDischargeOutcome::Discharged {
                    settlement_seq: u64_from_sql("RuntimeEffectGroup", "next_seq", settlement_seq)?,
                }))
            })
            .await
            .map_err(effect_sqlite_error)?;
        if matches!(outcome, EffectDischargeOutcome::Discharged { .. }) {
            if seats_terminal {
                self.announce_row(&scope_id, &replay_key);
            }
            self.announce(EffectJournalSubject::Group {
                group_key: &group_key,
            });
        }
        Ok(outcome)
    }

    async fn drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, RuntimeEffectControllerError> {
        let group_key = group_key.to_string();
        self.conn
            .call(move |connection| {
                connection.query_row(
                    effect_sql(Schema::Main)
                        .replay
                        .has_undrained_lower_commit
                        .sql(),
                    params![group_key.as_str(), commit_seq as i64],
                    |row| row.get(0),
                )
            })
            .await
            .map_err(effect_sqlite_error)
    }

    /// Commits one group child's final record at the §4 point — the
    /// final-attempt boundary's durable half: `commit_state`, the allocated
    /// `commit_seq`, and the drain input, under the claim's lease fence.
    async fn commit_group_child(
        &self,
        request: &EffectGroupChildCommitRequest,
    ) -> Result<EffectGroupChildCommitOutcome, RuntimeEffectControllerError> {
        let request = request.clone();
        let clock = Arc::clone(&self.clock);
        self.conn
            .write_flow(
                move |tx| -> rusqlite::Result<
                    TxOutcome<Result<EffectGroupChildCommitOutcome, RuntimeEffectControllerError>>,
                > {
                    let now = clock.timestamp_ms();
                    // The row decides its own group and arbitration state: the
                    // caller's `group_key`, when it carries one, is an assertion
                    // checked against the row rather than a lookup trusted blind.
                    let Some((commit_state, commit_seq, row_group_key)) = tx
                        .query_row(
                            effect_sql(Schema::Main)
                                .replay
                                .select_arbitration_by_key
                                .sql(),
                            params![request.scope_id.as_str(), request.replay_key.as_str(),],
                            |row| {
                                let (commit_state, commit_seq) =
                                    decode_commit_state_columns(row, 0)?;
                                let group_key: Option<String> = row.get(2)?;
                                Ok((commit_state, commit_seq, group_key))
                            },
                        )
                        .optional()?
                    else {
                        // A claimed child's row cannot be missing — its claim is
                        // what wrote it — so a §4 commit naming no row is
                        // corruption, not an ungrouped caller.
                        return Err(sqlite_conversion_error(stored_data_corrupt(
                            "RuntimeEffectReplay",
                            format!(
                                "a §4 commit for replay key `{}` under scope `{}` names \
                                 a replay row that does not exist; a claimed child's row \
                                 cannot be missing",
                                request.replay_key, request.scope_id
                            ),
                        )));
                    };
                    let Some(group_key) = row_group_key else {
                        return Ok(TxOutcome::Commit(Ok(
                            EffectGroupChildCommitOutcome::Ungrouped,
                        )));
                    };
                    if let Some(asserted) = request.group_key.as_deref()
                        && asserted != group_key
                    {
                        return Err(sqlite_conversion_error(stored_data_corrupt(
                            "RuntimeEffectReplay",
                            format!(
                                "final commit for replay key `{}` asserted group {asserted} \
                             but the row belongs to {group_key}; the durable membership \
                             is the authority",
                                request.replay_key
                            ),
                        )));
                    }
                    let drain_input =
                        |tx: &rusqlite::Transaction<'_>| -> rusqlite::Result<Option<String>> {
                            select_child_drain_input(tx, &group_key, &request.replay_key)
                                .map(|(_, input)| input)
                        };
                    match commit_state {
                        Some(EffectCommitState::Committed) | Some(EffectCommitState::Drained) => {
                            let commit_seq = commit_seq.ok_or_else(|| {
                                sqlite_conversion_error(stored_data_corrupt(
                                    "RuntimeEffectReplay",
                                    format!(
                                        "child `{}` of group {} reads committed but \
                                     carries no commit_seq; the column CHECK makes \
                                     that unwritable",
                                        request.replay_key, group_key
                                    ),
                                ))
                            })?;
                            return Ok(TxOutcome::Commit(Ok(
                                EffectGroupChildCommitOutcome::AlreadyCommitted {
                                    drain_input: drain_input(tx)?,
                                    group_key,
                                    commit_seq,
                                },
                            )));
                        }
                        Some(EffectCommitState::CancelDecided) => {
                            // The schema CHECK forbids `commit_seq` on a
                            // cancel-decided row; the rank the caller is owed
                            // is the settlement rank the decision seated.
                            let commit_seq =
                                read_child_settlement_seq(tx, &group_key, &request.replay_key)?;
                            return Ok(TxOutcome::Commit(Ok(
                                EffectGroupChildCommitOutcome::CancelDecided {
                                    group_key,
                                    commit_seq,
                                },
                            )));
                        }
                        Some(EffectCommitState::Pending) => {}
                        None => {
                            return Err(sqlite_conversion_error(stored_data_corrupt(
                                "RuntimeEffectReplay",
                                format!(
                                    "child `{}` of group {} read back NULL commit_state; \
                                 the column is NOT NULL",
                                    request.replay_key, group_key
                                ),
                            )));
                        }
                    }
                    // Group row, then the replay row's CAS — the shared lock
                    // order. The lease guard on the CAS is the claim fence: a row
                    // reclaimed under another owner misses, and the re-read below
                    // decides whether a winner or a fence loss explains it.
                    let commit_seq: i64 = tx
                        .query_row(
                            effect_sql(Schema::Main).group.bump_next_commit_seq.sql(),
                            params![group_key.as_str()],
                            |row| row.get(0),
                        )
                        .optional()?
                        .ok_or_else(|| missing_group_row(&group_key))?;
                    let won = tx.execute(
                        effect_sql(Schema::Main).replay.commit_child_final.sql(),
                        params![
                            request.scope_id.as_str(),
                            request.replay_key.as_str(),
                            group_key.as_str(),
                            commit_seq,
                            request.drain_input.as_str(),
                            request.owner_id.as_str(),
                            now as i64,
                        ],
                    )?;
                    if won == 0 {
                        // Re-read under the same transaction: a committed cancel
                        // decision explains the miss, a committed sibling CAS is
                        // idempotent, and a still-pending row means the lease
                        // moved — a fence loss the caller surfaces as a conflict.
                        let outcome =
                            match select_child_commit_state(tx, &group_key, &request.replay_key)? {
                                Some(EffectCommitState::CancelDecided) => {
                                    EffectGroupChildCommitOutcome::CancelDecided {
                                        commit_seq: read_child_commit_seq(
                                            tx,
                                            &group_key,
                                            &request.replay_key,
                                        )?,
                                        group_key,
                                    }
                                }
                                Some(EffectCommitState::Committed)
                                | Some(EffectCommitState::Drained) => {
                                    EffectGroupChildCommitOutcome::AlreadyCommitted {
                                        drain_input: drain_input(tx)?,
                                        commit_seq: read_child_commit_seq(
                                            tx,
                                            &group_key,
                                            &request.replay_key,
                                        )?,
                                        group_key,
                                    }
                                }
                                Some(EffectCommitState::Pending) => {
                                    return Ok(TxOutcome::Rollback(Err(
                                RuntimeEffectControllerError::new(
                                    lash_core_execution::RuntimeErrorCode::SqliteEffectReplayLeaseLost,
                                    format!(
                                        "final commit for child `{}` of group {group_key} \
                                         lost its lease fence: the row is still pending \
                                         but the claiming owner no longer holds it",
                                        request.replay_key
                                    ),
                                ),
                            )));
                                }
                                None => {
                                    return Err(sqlite_conversion_error(stored_data_corrupt(
                                        "RuntimeEffectReplay",
                                        format!(
                                            "final commit for child `{}` of group {group_key} \
                                     found the commit state NULL after its own CAS \
                                     missed; the column is NOT NULL",
                                            request.replay_key
                                        ),
                                    )));
                                }
                            };
                        return Ok(TxOutcome::Rollback(Ok(outcome)));
                    }
                    Ok(TxOutcome::Commit(Ok(
                        EffectGroupChildCommitOutcome::Committed {
                            group_key,
                            commit_seq: u64_from_sql(
                                "RuntimeEffectGroup",
                                "next_commit_seq",
                                commit_seq,
                            )?,
                        },
                    )))
                },
            )
            .await
            .map_err(effect_sqlite_error)?
    }

    async fn read_group_child_arbitration(
        &self,
        scope_id: &str,
        replay_key: &str,
    ) -> Result<Option<StoredChildArbitration>, RuntimeEffectControllerError> {
        let scope_id = scope_id.to_string();
        let replay_key = replay_key.to_string();
        self.conn
            .call(move |connection| {
                Ok(connection
                    .query_row(
                        effect_sql(Schema::Main)
                            .replay
                            .select_arbitration_by_key
                            .sql(),
                        params![scope_id.as_str(), replay_key.as_str()],
                        |row| {
                            let group_key: Option<String> = row.get(2)?;
                            group_key
                                .as_deref()
                                .map(|group_key| decode_child_arbitration(row, group_key))
                                .transpose()
                        },
                    )
                    .optional()?
                    .flatten())
            })
            .await
            .map_err(effect_sqlite_error)
    }

    /// Records the group and reports the row **as it stands durably**, so a
    /// reopen is fenced against what the journal holds rather than against this
    /// process's memory.
    ///
    /// The insert and the read-back share one `BEGIN IMMEDIATE` transaction,
    /// which touches only the group table and commits before any child of the
    /// group claims (N2).
    async fn open_group(
        &self,
        record: &EffectGroupRecord,
        membership: &[AcceptedGroupChild],
    ) -> Result<EffectGroupRecord, RuntimeEffectControllerError> {
        let record = record.clone();
        let membership = membership.to_vec();
        let scope_id = record.scope_id.clone();
        let fences = self.fence_locations().await?;
        self.conn
            .write(move |tx| {
                if fences.is_fenced(tx, &record.scope_id)? {
                    return Ok(None);
                }
                // Children before the group row, in one transaction (ADR 0065
                // N2). The order is the lock order, and it is also what makes
                // ADR 0099 §3 hold: the group row's existence implies its
                // complete membership, because nothing can observe the group
                // before this transaction commits.
                for child in &membership {
                    tx.execute(
                        effect_sql(Schema::Main)
                            .group_child_sqlite
                            .insert_accepted
                            .sql(),
                        params![
                            record.group_key.as_str(),
                            child.position as i64,
                            child.replay_key.as_str(),
                            child.envelope_json.as_str(),
                            i64::from(child.command_version),
                            record.created_at_ms as i64,
                        ],
                    )?;
                }
                // `DO NOTHING` rather than an upsert: reopening a group must not
                // reset `next_seq`, which would re-seat recorded children at
                // ranks a caller has already consumed.
                tx.execute(
                    effect_sql(Schema::Main).group_sqlite.insert_new.sql(),
                    params![
                        record.group_key.as_str(),
                        record.scope_id.as_str(),
                        record.session_id.as_deref(),
                        record.wake.column(),
                        record.loser_disposition.column(),
                        record.expected_children as i64,
                        record.created_at_ms as i64,
                    ],
                )?;
                select_group_record(tx, &record.group_key).map(Some)
            })
            .await
            .map_err(effect_sqlite_error)?
            .ok_or_else(|| effect_replay_driver::scope_retired(&scope_id))
    }

    async fn read_group_membership(
        &self,
        group_key: &str,
    ) -> Result<Vec<AcceptedGroupChild>, RuntimeEffectControllerError> {
        let group_key = group_key.to_string();
        self.conn
            .call(move |connection| {
                let tx = connection.transaction()?;
                let mut statement =
                    tx.prepare(effect_sql(Schema::Main).group_child.select_membership.sql())?;
                let membership = statement
                    .query_map(params![group_key.as_str()], |row| {
                        Ok(AcceptedGroupChild {
                            position: row.get::<_, i64>(1)? as usize,
                            replay_key: row.get(2)?,
                            envelope_json: row.get(3)?,
                            command_version: row.get::<_, i64>(4)? as u16,
                        })
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                drop(statement);
                tx.commit()?;
                Ok(membership)
            })
            .await
            .map_err(effect_sqlite_error)
    }

    /// Reads the group row without writing one, so a drain reads the declared
    /// disposition instead of inserting a group it was only asking about.
    async fn read_group(
        &self,
        group_key: &str,
    ) -> Result<Option<EffectGroupRecord>, RuntimeEffectControllerError> {
        let group_key = group_key.to_string();
        self.conn
            .call(move |connection| {
                let tx = connection.transaction()?;
                let record = select_group_record(&tx, &group_key).optional()?;
                tx.commit()?;
                Ok(record)
            })
            .await
            .map_err(effect_sqlite_error)
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
        let group_key = group_key.to_string();
        self.conn
            .call(move |connection| {
                let mut statement = connection.prepare(
                    effect_sql(Schema::Main)
                        .replay
                        .select_unsettled_children
                        .sql(),
                )?;
                let rows = statement
                    .query_map(params![group_key.as_str()], |row| {
                        let state = match row.get::<_, Option<String>>(5)? {
                            Some(status) => {
                                Some(effect_replay_driver::EffectRowState::from_columns(
                                    status,
                                    row.get(6)?,
                                    row.get(7)?,
                                ))
                            }
                            None => None,
                        };
                        let (commit_state, commit_seq) = decode_commit_state_columns(row, 9)?;
                        Ok(UnsettledGroupChild {
                            scope_id: row.get(0)?,
                            position: u64_from_sql(
                                "RuntimeEffectGroupChild",
                                "position",
                                row.get(1)?,
                            )?,
                            replay_key: row.get(2)?,
                            envelope_json: row.get(3)?,
                            command_version: row.get::<_, i64>(4)? as u16,
                            state,
                            lease_expires_at_ms: row
                                .get::<_, Option<i64>>(8)?
                                .map(|value| {
                                    u64_from_sql(
                                        "RuntimeEffectReplay",
                                        "lease_expires_at_ms",
                                        value,
                                    )
                                })
                                .transpose()?
                                .unwrap_or(0),
                            commit_state,
                            commit_seq,
                        })
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .await
            .map_err(effect_sqlite_error)
    }

    async fn transition_group_lifecycle(
        &self,
        group_key: &str,
        from: &[EffectGroupLifecyclePhase],
        to: &EffectGroupLifecycle,
    ) -> Result<EffectGroupLifecycle, RuntimeEffectControllerError> {
        let group_key = group_key.to_string();
        let from = from.to_vec();
        let to = *to;
        self.conn
            .write(move |tx| {
                let to_json = serde_json::to_string(&to).map_err(|error| {
                    sqlite_conversion_error(stored_data_corrupt(
                        "RuntimeEffectGroup",
                        format!("effect group lifecycle does not encode: {error}"),
                    ))
                })?;
                let from_json = serde_json::to_string(
                    &from.iter().map(|phase| phase.column()).collect::<Vec<_>>(),
                )
                .map_err(|error| {
                    sqlite_conversion_error(stored_data_corrupt(
                        "RuntimeEffectGroup",
                        format!("effect group lifecycle phases do not encode: {error}"),
                    ))
                })?;
                let written = tx
                    .query_row(
                        effect_sql(Schema::Main)
                            .group_sqlite
                            .transition_lifecycle
                            .sql(),
                        params![group_key.as_str(), to_json.as_str(), from_json.as_str()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                match written {
                    Some(raw) => group_lifecycle_from_sql(&group_key, &raw),
                    // Guard miss: report the lifecycle the competing writer
                    // left durable — or fail on an unknown group key.
                    None => Ok(select_group_record(tx, &group_key)?.lifecycle),
                }
            })
            .await
            .map_err(effect_sqlite_error)
    }

    async fn read_unsettled_groups(
        &self,
        scope_id: &str,
    ) -> Result<Vec<EffectGroupRecord>, RuntimeEffectControllerError> {
        let scope_id = scope_id.to_string();
        self.conn
            .call(move |connection| {
                let mut statement = connection.prepare(
                    effect_sql(Schema::Main)
                        .group_sqlite
                        .select_unsettled_by_scope
                        .sql(),
                )?;
                statement
                    .query_map(params![scope_id.as_str()], group_record_from_row)?
                    .collect()
            })
            .await
            .map_err(effect_sqlite_error)
    }

    async fn read_session_group_lifecycle_pins(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, EffectGroupLifecycle)>, RuntimeEffectControllerError> {
        let session_id = session_id.to_string();
        self.conn
            .call(move |connection| {
                let mut statement = connection.prepare(
                    effect_sql(Schema::Main)
                        .group_sqlite
                        .select_session_pins
                        .sql(),
                )?;
                statement
                    .query_map(params![session_id.as_str()], |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            group_lifecycle_from_sql(
                                &row.get::<_, String>(0)?,
                                &row.get::<_, String>(1)?,
                            )?,
                        ))
                    })?
                    .collect()
            })
            .await
            .map_err(effect_sqlite_error)
    }

    async fn scope_is_quiescent(&self, scope: &ExecutionScope) -> Result<bool, RuntimeError> {
        let read_error = |error: String| {
            RuntimeError::new(
                lash_core_execution::RuntimeErrorCode::SqliteEffectJournalRetirement,
                error,
            )
        };
        let scope_id = scope.journal_identity()?.key().to_string();
        let scope_json =
            serde_json::to_string(scope).map_err(|error| read_error(error.to_string()))?;
        self.conn
            .call(move |connection| {
                let tx = connection.transaction()?;
                let quiescent = scope_is_quiescent(&tx, Schema::Main, &scope_id, &scope_json)?;
                tx.commit()?;
                Ok(quiescent)
            })
            .await
            .map_err(|error| read_error(error.to_string()))
    }

    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: usize,
    ) -> Result<Option<StoredGroupSettlement>, RuntimeEffectControllerError> {
        let Some(offset) = rank.checked_sub(1) else {
            return Ok(None);
        };
        let group_key = group_key.to_string();
        self.conn
            .call(move |connection| {
                connection
                    .query_row(
                        effect_sql(Schema::Main)
                            .replay
                            .select_settlement_by_rank
                            .sql(),
                        params![group_key.as_str(), crate::clamp_sequence_bound(offset)],
                        |row| {
                            let state = effect_replay_driver::EffectRowState::from_columns(
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                            );
                            Ok(StoredGroupSettlement {
                                sequence: u64_from_sql(
                                    "RuntimeEffectReplay",
                                    "settlement_seq",
                                    row.get(0)?,
                                )?,
                                replay_key: row.get(1)?,
                                state,
                            })
                        },
                    )
                    .optional()
            })
            .await
            .map_err(effect_sqlite_error)
    }

    async fn renew(
        &self,
        fence: &EffectLeaseFence,
        lease_ttl_ms: u64,
    ) -> Result<bool, RuntimeEffectControllerError> {
        let fence = fence.clone();
        let clock = Arc::clone(&self.clock);
        self.conn
            .write(move |tx| {
                let now = clock.timestamp_ms();
                let renewed_expires_at = now.saturating_add(lease_ttl_ms);
                let changed = tx.execute(
                    effect_sql(Schema::Main).replay_sqlite.renew_lease.sql(),
                    params![
                        fence.scope_id.as_str(),
                        fence.replay_key.as_str(),
                        fence.envelope_hash.as_str(),
                        fence.owner_id.as_str(),
                        fence.lease_token.as_str(),
                        renewed_expires_at as i64,
                        now as i64,
                        now as i64,
                    ],
                )?;
                Ok(changed == 1)
            })
            .await
            .map_err(effect_sqlite_error)
    }

    /// Retires the rows and then wakes every waiter on this journal: a
    /// retirement deletes rows and whole groups a parked claim or discharge
    /// may be waiting on.
    async fn retire_journal(
        &self,
        retirement: &EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        let retired = self.retire_journal_rows(retirement).await;
        if retired.is_ok() {
            EffectJournalNotifiers::announce_journal(&self.wake.identity);
        }
        retired
    }

    async fn reinstate_scope(&self, scope_id: &str) -> Result<(), RuntimeError> {
        let scope_id = scope_id.to_string();
        let fences = self
            .registry
            .ensure_attached(&self.conn)
            .await
            .map_err(|error| {
                RuntimeError::new(
                    lash_core_execution::RuntimeErrorCode::SqliteEffectJournalRetirement,
                    error.to_string(),
                )
            })?;
        self.conn
            .write(move |tx| fences.lift(tx, &scope_id))
            .await
            .map_err(|error| {
                RuntimeError::new(
                    lash_core_execution::RuntimeErrorCode::SqliteEffectJournalRetirement,
                    error.to_string(),
                )
            })
    }

    async fn pending_artifact_owner_retirements(
        &self,
    ) -> Result<Vec<ExecutionScope>, RuntimeError> {
        self.conn
            .call(|conn| {
                let mut statement = conn.prepare(
                    fence_sql(Schema::Main)
                        .sqlite
                        .select_pending_artifact_cleanup
                        .sql(),
                )?;
                let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
                let mut scopes = Vec::new();
                for row in rows {
                    let key = row?;
                    let scope = ExecutionScope::from_journal_key(&key).ok_or_else(|| {
                        rusqlite::Error::InvalidParameterName(format!(
                            "invalid retired effect scope key `{key}`"
                        ))
                    })?;
                    scopes.push(scope);
                }
                Ok(scopes)
            })
            .await
            .map_err(|error| {
                RuntimeError::new(
                    lash_core_execution::RuntimeErrorCode::SqliteEffectJournalRetirement,
                    error.to_string(),
                )
            })
    }

    async fn complete_artifact_owner_retirement(&self, scope_id: &str) -> Result<(), RuntimeError> {
        let scope_id = scope_id.to_string();
        self.conn
            .write(move |tx| {
                tx.execute(
                    fence_sql(Schema::Main)
                        .sqlite
                        .complete_artifact_cleanup
                        .sql(),
                    params![scope_id],
                )?;
                Ok(())
            })
            .await
            .map_err(|error| {
                RuntimeError::new(
                    lash_core_execution::RuntimeErrorCode::SqliteEffectJournalRetirement,
                    error.to_string(),
                )
            })
    }
}

impl SqliteEffectReplayRowStore {
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
    ///
    /// A scope-exact retirement (N4) additionally deletes the scope's
    /// await-event promise rows and writes its permanent retirement tombstone.
    /// A fence kept in this file is written in the same `BEGIN IMMEDIATE`
    /// transaction as the deletions, so both become visible together. A
    /// process-scope fence kept in the bound registry's file is the one
    /// commit point of the retirement: its insert commits first, under the
    /// quiescence proof read on the same locks, and the journal purge that
    /// follows is an idempotent cleanup a crash may lose — the fenced scope
    /// then admits nothing and the next bind or retention sweep purges its
    /// rows (ADR 0049).
    async fn retire_journal_rows(
        &self,
        retirement: &EffectJournalRetirement,
    ) -> Result<usize, RuntimeError> {
        let retired_scope_key = retirement
            .retired_scope()
            .and_then(|scope| scope.journal_identity().ok())
            .map(|identity| identity.key().to_string());
        let retirement = retirement.clone();
        let now_ms = self.clock.timestamp_ms();
        let retirement_error = |error: rusqlite::Error| {
            RuntimeError::new(
                lash_core_execution::RuntimeErrorCode::SqliteEffectJournalRetirement,
                error.to_string(),
            )
        };
        let scope = match &retirement {
            EffectJournalRetirement::Session { session_id } => {
                let session_id = session_id.clone();
                return self
                    .conn
                    .write(move |tx| {
                        // ADR 0099 §7: a session that still owns a live or
                        // closing group may not be deleted — the group row is
                        // the durable authority closing and its finalization
                        // run against, and deleting it underneath a
                        // finalization in flight would strand the recorded
                        // obligations. Refuse before any write so the pin
                        // check rides the same `BEGIN IMMEDIATE` the deletes
                        // would have.
                        let sql = effect_sql(Schema::Main);
                        let mut pin_statement =
                            tx.prepare(sql.group_sqlite.select_session_pins.sql())?;
                        let pins = pin_statement
                            .query_map(params![session_id.as_str()], |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    group_lifecycle_from_sql(
                                        &row.get::<_, String>(0)?,
                                        &row.get::<_, String>(1)?,
                                    )?,
                                ))
                            })?
                            .collect::<rusqlite::Result<Vec<_>>>()?;
                        if let Some((first_key, _)) = pins.first() {
                            return Ok(Err(RuntimeError::new(
                                lash_core_execution::RuntimeErrorCode::EffectGroupLifecyclePinned,
                                format!(
                                    "session `{session_id}` still owns {} effect group(s) that are live or closing (first: `{first_key}`); session deletion is refused until they settle",
                                    pins.len()
                                ),
                            )));
                        }
                        // Session deletion is the authoritative lifecycle
                        // decision for every exact execution scope it owns.
                        // Preserve those scopes as the same retirement
                        // evidence used by scope-exact retirement so artifact
                        // cleanup remains retryable after journal deletion.
                        tx.execute(
                            sql.journal_sqlite.insert_session_scope_fences.sql(),
                            params![session_id.as_str(), now_ms as i64],
                        )?;
                        let deleted = tx.execute(
                            sql.replay.delete_by_session.sql(),
                            params![session_id.as_str()],
                        )?;
                        // Membership before the group rows it keys off: the
                        // statement selects the session's groups, so deleting
                        // them first would strand every accepted request and
                        // leave it naming environment bytes the retirement is
                        // about to reclaim (ADR 0099 §3).
                        tx.execute(
                            sql.group_child.delete_by_session.sql(),
                            params![session_id.as_str()],
                        )?;
                        tx.execute(
                            sql.group.delete_by_session.sql(),
                            params![session_id.as_str()],
                        )?;
                        Ok(Ok(deleted))
                    })
                    .await
                    .map_err(retirement_error)?;
            }
            #[expect(
                clippy::expect_used,
                reason = "`retired_scope` is `Some` for exactly the process and runtime-operation variants this arm matches"
            )]
            EffectJournalRetirement::Process { .. }
            | EffectJournalRetirement::RuntimeOperation { .. } => retirement
                .retired_scope()
                .expect("scope-exact retirements name their scope"),
        };
        #[expect(
            clippy::expect_used,
            reason = "process and runtime-operation scopes carry no session id to validate, so their journal identity always forms, and `ExecutionScope` is a derived-`Serialize` enum of strings"
        )]
        let (identity, scope_json) = (
            scope.journal_identity().expect(
                "process and runtime-operation scopes always form durable journal identities",
            ),
            serde_json::to_string(&scope).expect("execution scopes serialize infallibly"),
        );
        let scope_id = identity.key().to_string();
        let when_quiescent = retirement.gate() == Some(EffectRetirementGate::WhenQuiescent);
        let fences = self
            .registry
            .ensure_attached(&self.conn)
            .await
            .map_err(retirement_error)?;
        let fence_schema = fences.fence_schema_for(&scope);
        let two_commits = fences.fence_is_in_registry_file(&scope);
        // Commit point: the quiescence proof and the fence insert, under the
        // `BEGIN IMMEDIATE` lock every claim reads the fence under. When the
        // fence shares the journal file, the purge rides the same commit.
        let fenced = {
            let scope_id = scope_id.clone();
            let scope_json = scope_json.clone();
            self.conn
                .write(move |tx| {
                    let closure_pinned =
                        scope_has_turn_cancel_closure_participant(tx, Schema::Main, &scope_id)?;
                    if closure_pinned {
                        return Ok(None);
                    }
                    if when_quiescent
                        && !scope_is_quiescent(tx, Schema::Main, &scope_id, &scope_json)?
                    {
                        return Ok(None);
                    }
                    insert_scope_fence(tx, fence_schema, &scope_id, now_ms)?;
                    if two_commits {
                        return Ok(Some(None));
                    }
                    Ok(Some(Some(delete_scope_rows(
                        tx,
                        Schema::Main,
                        &scope_id,
                        &scope_json,
                    )?)))
                })
                .await
                .map_err(retirement_error)?
        };
        let Some(purged) = fenced else {
            return Err(effect_replay_driver::scope_not_quiescent(
                retired_scope_key.as_deref().unwrap_or_default(),
            ));
        };
        if let Some(deleted) = purged {
            return Ok(deleted);
        }
        // Post-commit cleanup: idempotent, and repeated by the next bind or
        // sweep if this process dies before it lands.
        self.conn
            .write(move |tx| delete_scope_rows(tx, Schema::Main, &scope_id, &scope_json))
            .await
            .map_err(retirement_error)
    }
}
