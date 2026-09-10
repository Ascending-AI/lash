use super::*;

#[async_trait::async_trait]
impl SessionCommitStore for Store {
    async fn read_session_state_version(&self) -> Result<u32, StoreError> {
        let Some(session_id) = self.resolve_session_id_for_read().await? else {
            return Ok(lash_core::store::OLDEST_SUPPORTED_SESSION_STATE_VERSION);
        };
        self.conn
            .call(move |conn| Ok(read_session_state_version_conn(conn, &session_id)))
            .await
            .map_err(sqlite_error)?
    }

    async fn admit_session_state(
        &self,
        lease: &SessionExecutionLeaseAuthority,
    ) -> Result<lash_core::store::SessionStateAdmission, StoreError> {
        let lease = lease.clone();
        let now = self.clock.timestamp_ms();
        self.conn
            .write_flow(move |tx| {
                let outcome = (|| {
                    ensure_session_execution_lease_conn(tx, &lease.session_id, &lease, now)?;
                    let version = read_session_state_version_conn(tx, &lease.session_id)?;
                    Ok(lash_core::store::SessionStateAdmission {
                        session_id: lease.session_id.clone(),
                        version,
                        lease_fencing_token: lease.fencing_token,
                    })
                })();
                Ok(match outcome {
                    Ok(admission) => TxOutcome::Commit(Ok(admission)),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn load_session(&self) -> Result<Option<PersistedSessionRead>, StoreError> {
        let Some(session_id) = self.resolve_session_id_for_read().await? else {
            return Ok(None);
        };
        let warning_session_id = session_id.clone();
        let (outcome, corrupt_failure_receipts) = self
            .conn
            .call(move |conn| {
                let tx = conn.transaction()?;
                let outcome: Result<Option<SessionLoadWithWarnings>, StoreError> = (|| {
                    read_session_state_version_conn(&tx, &session_id)?;
                    let Some(meta) = try_load_session_head_meta_from_conn(&tx, &session_id)? else {
                        return Ok(None);
                    };
                    let leaf_node_id = meta.leaf_node_id.clone();
                    let mut graph = Self::load_active_path_session_graph_from_conn(
                        &tx,
                        &session_id,
                        leaf_node_id.clone(),
                    )?;
                    if !graph.nodes.is_empty() {
                        graph.set_leaf_node_id(leaf_node_id);
                    }
                    let checkpoint = match meta.checkpoint_ref.as_ref() {
                        Some(blob_ref) => {
                            Some(Self::get_checkpoint_conn(&tx, blob_ref)?.ok_or_else(|| {
                                StoreError::CheckpointComponentMissing {
                                    key: "manifest".to_string(),
                                    blob_ref: blob_ref.clone(),
                                }
                            })?)
                        }
                        None => None,
                    };
                    let failure_settlements = load_turn_failure_settlements_conn(&tx, &session_id)?;
                    Ok(Some(SessionLoadWithWarnings {
                        read: PersistedSessionRead {
                            session_id: meta.session_id,
                            head_revision: meta.head_revision,
                            config: meta.config,
                            current_frame_node_id: meta.current_frame_node_id,
                            graph,
                            checkpoint_ref: meta.checkpoint_ref,
                            checkpoint,
                            token_ledger: lash_core::store::merge_token_ledger_entries_checked(
                                Self::load_usage_deltas_conn(&tx, &session_id)?,
                            )?,
                            turn_failure_settlements: failure_settlements.settlements,
                        },
                        corrupt_failure_receipts: failure_settlements.corrupt_receipts,
                    }))
                })(
                );
                tx.commit()?;
                let (read, corrupt_failure_receipts) = match outcome {
                    Ok(Some(loaded)) => (Ok(Some(loaded.read)), loaded.corrupt_failure_receipts),
                    Ok(None) => (Ok(None), Vec::new()),
                    Err(error) => (Err(error), Vec::new()),
                };
                Ok((read, corrupt_failure_receipts))
            })
            .await
            .map_err(sqlite_error)?;
        for corrupt in corrupt_failure_receipts {
            tracing::warn!(
                target: "lash_sqlite_store::persistence",
                session_id = warning_session_id.as_str(),
                turn_id = corrupt.turn_id.as_str(),
                error = corrupt.error,
                "skipping corrupt runtime turn receipt while loading failure evidence"
            );
        }
        outcome
    }

    async fn load_session_head_meta(&self) -> Result<Option<SessionHeadMeta>, StoreError> {
        self.read_session_state_version().await?;
        Store::load_session_head_meta(self).await
    }

    /// FIG-653: session-relative history reads enforce graph membership, not authorization.
    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<lash_core::SessionNodeRecord>, StoreError> {
        let session_id = self.selected_session_id()?;
        let node_id = node_id.to_string();
        let row: Option<(String, Option<String>, String)> = self
            .conn
            .call(move |conn| {
                let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Deferred)?;
                let outcome = (|| {
                    let candidate = tx
                        .query_row(
                            "SELECT node.node_id, node.parent_node_id, node.node_json,
                                node.session_id, node.generation
                         FROM graph_nodes AS node
                         WHERE node.node_id = ?1 AND node.tombstoned = 0
                           AND (
                               node.session_id = ?2
                               OR EXISTS (
                                   SELECT 1 FROM fork_lineage AS lineage
                                   WHERE lineage.session_id = ?2
                                     AND lineage.ancestor_session_id = node.session_id
                                     AND node.generation <= lineage.fork_generation
                               )
                           )",
                            params![node_id, session_id.as_str()],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, Option<String>>(1)?,
                                    row.get::<_, String>(2)?,
                                    row.get::<_, String>(3)?,
                                    row.get::<_, i64>(4)?,
                                ))
                            },
                        )
                        .optional()?;
                    let Some((
                        candidate_id,
                        parent_node_id,
                        node_json,
                        owner,
                        candidate_generation,
                    )) = candidate
                    else {
                        return Ok(None);
                    };
                    if owner != session_id {
                        let mut stmt = tx.prepare(
                            "WITH readable_sessions(session_id, generation_ceiling) AS (
                                 SELECT ?1, NULL
                                 UNION ALL
                                 SELECT lineage.ancestor_session_id, lineage.fork_generation
                                 FROM fork_lineage AS lineage
                                 WHERE lineage.session_id = ?1
                             )
                             SELECT head.leaf_node_id, head_node.generation, head_node.tombstoned,
                                    node.node_id, node.parent_node_id,
                                    node.generation, node.tombstoned
                             FROM session_head AS head
                             LEFT JOIN graph_nodes AS head_node
                               ON head_node.node_id = head.leaf_node_id
                             LEFT JOIN readable_sessions AS readable ON TRUE
                             LEFT JOIN graph_nodes AS node
                               ON node.session_id = readable.session_id
                              AND node.generation BETWEEN ?2 AND head_node.generation
                              AND (
                                  readable.generation_ceiling IS NULL
                                  OR node.generation <= readable.generation_ceiling
                              )
                             WHERE head.session_id = ?1",
                        )?;
                        let rows = stmt
                            .query_map(params![session_id.as_str(), candidate_generation], |row| {
                                Ok((
                                    row.get::<_, Option<String>>(0)?,
                                    row.get::<_, Option<i64>>(1)?,
                                    row.get::<_, Option<i64>>(2)?,
                                    row.get::<_, Option<String>>(3)?,
                                    row.get::<_, Option<String>>(4)?,
                                    row.get::<_, Option<i64>>(5)?,
                                    row.get::<_, Option<i64>>(6)?,
                                ))
                            })?
                            .collect::<Result<Vec<_>, _>>()?;
                        let Some(first) = rows.first() else {
                            return Ok(None);
                        };
                        let Some(head_id) = first.0.clone() else {
                            return Ok(None);
                        };
                        let (Some(head_generation), Some(head_tombstoned)) = (first.1, first.2)
                        else {
                            return Err(sqlite_conversion_error(stored_data_corrupt(
                                "SessionGraph",
                                "head leaf is missing",
                            )));
                        };
                        if head_tombstoned != 0 {
                            return Err(sqlite_conversion_error(stored_data_corrupt(
                                "SessionGraph",
                                "head leaf is tombstoned",
                            )));
                        }
                        if candidate_generation < 0 || candidate_generation > head_generation {
                            return Ok(None);
                        }
                        let mut range = std::collections::HashMap::new();
                        for row in rows {
                            if let (
                                Some(node_id),
                                parent_node_id,
                                Some(generation),
                                Some(tombstoned),
                            ) = (row.3, row.4, row.5, row.6)
                            {
                                range.insert(node_id, (parent_node_id, generation, tombstoned));
                            }
                        }
                        let mut current_id = head_id;
                        let mut current_generation = head_generation;
                        loop {
                            let Some((parent_id, generation, tombstoned)) = range.get(&current_id)
                            else {
                                return Err(sqlite_conversion_error(stored_data_corrupt(
                                    "SessionGraph",
                                    "readable generation range omits an edge-path node",
                                )));
                            };
                            if *tombstoned != 0 || *generation != current_generation {
                                return Err(sqlite_conversion_error(stored_data_corrupt(
                                    "SessionGraph",
                                    "parent edge crosses a tombstone or generation gap",
                                )));
                            }
                            if current_generation == candidate_generation {
                                if current_id != candidate_id {
                                    return Ok(None);
                                }
                                break;
                            }
                            current_id = parent_id.clone().ok_or_else(|| {
                                sqlite_conversion_error(stored_data_corrupt(
                                    "SessionGraph",
                                    "parent edge ended before the candidate generation",
                                ))
                            })?;
                            current_generation -= 1;
                        }
                    }
                    Ok(Some((candidate_id, parent_node_id, node_json)))
                })()?;
                tx.commit()?;
                Ok(outcome)
            })
            .await
            .map_err(sqlite_error)?;
        row.map(|(node_id, parent_node_id, node_json)| {
            lash_core::SessionNodeRecord::decode_storage_body(node_id, parent_node_id, &node_json)
                .map_err(|error| stored_data_corrupt("SessionGraph node", error))
        })
        .transpose()
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        let planner = lash_core::store::RuntimeCommitPlanner::prepare(commit)?;
        self.bind_session(&planner.commit().session_id)?;
        let blob_profile = self.options.blob_profile;
        let now = self.clock.timestamp_ms();
        let enqueue_nonce_start = self.commit_count.fetch_add(
            planner.commit().enqueued_queue_batches.len() as u64,
            AtomicOrdering::Relaxed,
        );
        let result = self
            .conn
            .write_flow(move |tx| {
                let outcome: Result<RuntimeCommitReceipt, StoreError> = (|| {
                    let commit = planner.commit();
                    ensure_session_not_deleted_conn(tx, &commit.session_id)?;
                    if let Some(fence) = commit.session_execution_lease_fence.as_ref() {
                        ensure_session_execution_lease_conn(tx, &commit.session_id, fence, now)?;
                    }
                    let existing =
                        try_load_session_head_meta_from_conn(tx, &commit.session_id)?;
                    planner.validate_session_binding(
                        existing.as_ref().map(|meta| &meta.session_id),
                    )?;
                    crate::session_meta::write_session_meta(
                        tx,
                        &SessionMeta {
                            session_id: commit.session_id.clone(),
                            relation: lash_core::SessionRelation::Root,
                            pending_observer_intents: Vec::new(),
                        },
                        crate::session_meta::SessionMetaWrite::Insert,
                        now,
                    )?;
                    planner.validate_node_derivation()?;
                    {
                        let prior: Option<(
                            String,
                            String,
                            Option<String>,
                            Option<i64>,
                            Option<i64>,
                        )> = tx
                            .query_row(
                                "SELECT turn_commit_hash, result_json,
                                        request_identity_hash, identity_encoding_version,
                                        requested_node_count
                                 FROM runtime_turn_commits
                                 WHERE session_id = ?1 AND turn_id = ?2",
                                params![commit.session_id.as_str(), planner.operation_key()],
                                |row| {
                                    Ok((
                                        row.get(0)?,
                                        row.get(1)?,
                                        row.get(2)?,
                                        row.get(3)?,
                                        row.get(4)?,
                                    ))
                                },
                            )
                            .optional()
                            .map_err(sqlite_error)?;
                        if let Some((
                            stored_hash,
                            result_json,
                            stored_identity,
                            stored_version,
                            stored_requested_node_count,
                        )) = prior
                        {
                            // One codec owns the unit-shape and integer-range checks.
                            // The ancestor stays write-only because the request hash binds it.
                            let append_request_identity =
                                lash_core::store_backend_support::decode_append_request_identity(
                                    &commit.turn_commit.operation.key,
                                    stored_identity,
                                    stored_version,
                                    stored_requested_node_count,
                                )?;
                            let result = serde_json::from_str(&result_json).map_err(|err| {
                                StoreError::Backend(format!(
                                    "failed to decode runtime turn commit result: {err}"
                                ))
                            })?;
                            let prior = lash_core::store::RuntimeCommitReceiptRecord {
                                turn_commit_hash: stored_hash,
                                result,
                                append_request_identity,
                            };
                            if let Some(replay) = planner.decide_receipt(Some(prior))? {
                                if let Some(completion) =
                                    replay.release_session_execution_lease()
                                {
                                    let _release_was_current =
                                        release_session_execution_lease_conn(tx, completion)?;
                                    // FIG-884: ancillary stale release must
                                    // never veto a replayed commit.
                                }
                                return Ok(replay.into_result());
                            }
                        }
                    }
                    let actual_revision = existing.as_ref().map_or(0, |meta| meta.head_revision);
                    let old_leaf_node_id = existing
                        .as_ref()
                        .and_then(|head| head.leaf_node_id.clone());
                    let parent_node_facts = old_leaf_node_id
                        .as_deref()
                        .map(|leaf_node_id| {
                            tx.query_row(
                                "SELECT generation, frame_node_id FROM graph_nodes
                                 WHERE node_id = ?1 AND tombstoned = 0",
                                params![leaf_node_id],
                                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
                            )
                            .optional()
                            .map_err(sqlite_error)?
                            .map(|(generation, frame_node_id)| {
                                Ok(lash_core::store::ParentNodeFacts {
                                    node_id: leaf_node_id.to_string(),
                                    generation: u64::try_from(generation).map_err(|_| {
                                        stored_data_corrupt(
                                            "SessionGraph node",
                                            format!("negative generation {generation}"),
                                        )
                                    })?,
                                    frame_node_id,
                                })
                            })
                            .transpose()
                        })
                        .transpose()?
                        .flatten();
                    let requested_ancestor_is_active = match (
                        requested_append_ancestor(&commit.turn_commit),
                        parent_node_facts.as_ref(),
                    ) {
                        (None, _) => true,
                        (Some(_), None) => false,
                        (Some(required), Some(parent)) => tx
                            .query_row(
                                "SELECT 1 FROM graph_nodes AS node
                                 WHERE node.node_id = ?1
                                   AND node.tombstoned = 0
                                   AND node.generation <= ?3
                                   AND (
                                       node.session_id = ?2
                                       OR EXISTS (
                                           SELECT 1 FROM fork_lineage AS lineage
                                           WHERE lineage.session_id = ?2
                                             AND lineage.ancestor_session_id = node.session_id
                                             AND node.generation <= lineage.fork_generation
                                       )
                                   )",
                                params![required, commit.session_id.as_str(), i64::try_from(parent.generation).map_err(|_| {
                                    StoreError::Backend("parent generation does not fit SQLite INTEGER".to_string())
                                })?],
                                |_| Ok(()),
                            )
                            .optional()
                            .map_err(sqlite_error)?
                            .is_some(),
                    };
                    let mut occupied_node_ids = std::collections::HashSet::new();
                    for node in &commit.graph.nodes {
                        let occupied = tx
                                .query_row(
                                    "SELECT 1 FROM graph_nodes WHERE node_id = ?1 LIMIT 1",
                                    params![node.node_id],
                                    |_| Ok(()),
                                )
                                .optional()
                            .map_err(sqlite_error)?
                            .is_some();
                        if occupied {
                            occupied_node_ids.insert(node.node_id.clone());
                        }
                    }
                    let selected_leaf_is_live = match commit.graph.leaf_node_id() {
                        Some(leaf_node_id) => tx
                            .query_row(
                                "SELECT 1 FROM graph_nodes
                                 WHERE node_id = ?1 AND tombstoned = 0
                                 LIMIT 1",
                                params![leaf_node_id],
                                |_| Ok(()),
                            )
                            .optional()
                            .map_err(sqlite_error)?
                            .is_some(),
                        None => false,
                    };
                    let has_live_nodes = tx
                        .query_row(
                            "SELECT 1 FROM graph_nodes
                             WHERE session_id = ?1 AND tombstoned = 0
                             LIMIT 1",
                            params![commit.session_id.as_str()],
                            |_| Ok(()),
                        )
                        .optional()
                        .map_err(sqlite_error)?
                        .is_some();
                    let published_leaf = match old_leaf_node_id {
                        None => lash_core::store::PublishedLeafFacts::Absent,
                        Some(node_id) => match parent_node_facts {
                            Some(parent) => lash_core::store::PublishedLeafFacts::Live(parent),
                            None => lash_core::store::PublishedLeafFacts::Retired { node_id },
                        },
                    };
                    let plan = planner.plan(lash_core::store::FreshRuntimeCommitFacts {
                        actual_head_revision: actual_revision,
                        published_leaf,
                        requested_ancestor_is_active,
                        occupied_node_ids,
                        selected_leaf_is_live,
                        has_live_nodes,
                    })?;
                    let sql_head_revision = sql_monotonic_counter_value(
                        "session_head_revision",
                        plan.actual_head_revision(),
                        plan.next_head_revision(),
                    )?;
                    for completed in &commit.completed_queue_claims {
                        ensure_queued_work_completion_conn(tx, completed)?;
                    }
                    for completed in &commit.completed_turn_input_claims {
                        for input_id in &completed.input_ids {
                            let observed = tx
                                .query_row(
                                    "SELECT claim_id, claim_token, claim_session_lease_generation, state
                                     FROM pending_turn_inputs
                                     WHERE session_id = ?1 AND input_id = ?2",
                                    params![completed.session_id.as_str(), input_id.as_str()],
                                    |row| {
                                        Ok((
                                            row.get::<_, Option<String>>(0)?,
                                            row.get::<_, Option<String>>(1)?,
                                            row.get::<_, i64>(2)?,
                                            row.get::<_, String>(3)?,
                                        ))
                                    },
                                )
                                .optional()
                                .map_err(sqlite_error)?;
                            let observed = observed
                                .map(|(claim_id, claim_token, generation, state)| {
                                    Ok((
                                        claim_id,
                                        claim_token,
                                        u64::try_from(generation).map_err(|_| {
                                            stored_data_corrupt(
                                                "PendingTurnInput",
                                                format!(
                                                    "claim_session_lease_generation must be non-negative, got {generation}"
                                                ),
                                            )
                                        })?,
                                        state,
                                    ))
                                })
                                .transpose()?;
                            // One predicate, two regimes: the claim fields only
                            // strengthen it (ADR 0069 section 5). Claimed
                            // settlement requires the row to still carry this
                            // claim; unclaimed settlement requires it to still
                            // be unclaimed and unsettled.
                            let owns_row = match completed.claim.as_ref() {
                                Some(claim) => observed.as_ref().is_some_and(
                                    |(claim_id, claim_token, _, _)| {
                                        claim_id.as_deref() == Some(claim.claim_id.as_str())
                                            && claim_token.as_deref()
                                                == Some(claim.lease_token.as_str())
                                    },
                                ),
                                None => observed.as_ref().is_some_and(|(claim_id, _, _, state)| {
                                    claim_id.is_none()
                                        && unclaimed_turn_input_is_settleable(state)
                                }),
                            };
                            if !owns_row {
                                return Err(match completed.claim.as_ref() {
                                    Some(claim) => StoreError::TurnInputClaimSuperseded {
                                        session_id: completed.session_id.clone(),
                                        claim_id: claim.claim_id.clone(),
                                        row_id: Some(input_id.clone().into_boxed_str()),
                                        superseding_claim_id: observed
                                            .as_ref()
                                            .and_then(|(claim_id, _, _, _)| claim_id.clone())
                                            .map(String::into_boxed_str),
                                        superseding_session_lease_generation: observed
                                            .as_ref()
                                            .and_then(|(claim_id, _, generation, _)| {
                                                claim_id.as_ref().map(|_| Box::new(*generation))
                                            }),
                                    },
                                    None => StoreError::UnclaimedTurnInputSettlementSuperseded {
                                        session_id: completed.session_id.clone(),
                                        input_id: input_id.clone(),
                                        observed_state: observed
                                            .as_ref()
                                            .map(|(_, _, _, state)| {
                                                state.clone().into_boxed_str()
                                            }),
                                        superseding_claim_id: observed
                                            .as_ref()
                                            .and_then(|(claim_id, _, _, _)| claim_id.clone())
                                            .map(String::into_boxed_str),
                                    },
                                });
                            }
                        }
                    }

                    let stored_checkpoint =
                        Self::put_checkpoint_conn(tx, &commit.checkpoint, blob_profile)?;

                    if !commit.usage_deltas.is_empty() {
                        let mut stmt = tx
                            .prepare(
                                "INSERT OR IGNORE INTO usage_deltas (
                                    session_id, operation_storage_key, entry_ordinal, payload_encoding_version, payload_hash, source, model, input_tokens, output_tokens, cache_read_input_tokens, cache_write_input_tokens, reasoning_output_tokens, usage_disposition_json
                                ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                            )
                            .map_err(sqlite_error)?;
                        for entry in &commit.usage_deltas {
                            let entry_ordinal = i64::try_from(entry.identity.entry_ordinal)
                                .map_err(|_| {
                                    StoreError::Backend(
                                        "usage delta ordinal does not fit SQLite INTEGER"
                                            .to_string(),
                                    )
                                })?;
                            stmt.execute(params![
                                commit.session_id.as_str(),
                                entry.identity.operation_storage_key,
                                entry_ordinal,
                                i64::from(entry.identity.payload_encoding_version),
                                entry.identity.payload_hash,
                                entry.entry.source,
                                entry.entry.model,
                                entry.entry.usage.input_tokens,
                                entry.entry.usage.output_tokens,
                                entry.entry.usage.cache_read_input_tokens,
                                entry.entry.usage.cache_write_input_tokens,
                                entry.entry.usage.reasoning_output_tokens,
                                crate::blobs::encode_usage_disposition(
                                    &entry.entry.usage_disposition,
                                )?,
                            ])
                            .map_err(sqlite_error)?;
                        }
                    }

                    for (node, facts) in commit
                        .graph
                        .nodes
                        .iter()
                        .zip(plan.planned_node_facts())
                    {
                        let node_json = node.encode_storage_body().map_err(|err| {
                            StoreError::Backend(format!(
                                "failed to encode graph node body: {err}"
                            ))
                        })?;
                        tx.execute(
                            "INSERT INTO graph_nodes
                             (session_id, node_id, parent_node_id, generation, frame_node_id, node_json)
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                            params![
                                commit.session_id.as_str(),
                                node.node_id.as_str(),
                                node.parent_node_id,
                                i64::try_from(facts.generation).map_err(|_| StoreError::Backend(
                                    "node generation does not fit SQLite INTEGER".to_string()
                                ))?,
                                facts.frame_node_id.as_str(),
                                node_json
                            ],
                        )
                        .map_err(|error| {
                            sqlite_graph_node_insert_error(
                                error,
                                &commit.session_id,
                                facts.generation,
                                &node.node_id,
                            )
                        })?;
                    }
                    let meta = plan.head_meta(stored_checkpoint.checkpoint_ref.clone());
                    tx.execute(
                        "INSERT OR REPLACE INTO session_head
                         (session_id, head_json, head_revision, leaf_node_id, checkpoint_ref)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            meta.session_id.as_str(),
                            encode_json(&meta.payload())?,
                            sql_head_revision,
                            meta.leaf_node_id,
                            meta.checkpoint_ref.as_ref().map(BlobRef::as_str),
                        ],
                    )
                    .map_err(sqlite_error)?;
                    tx.execute(
                        "UPDATE session_meta SET last_commit_at_ms = ?2 WHERE session_id = ?1",
                        params![commit.session_id.as_str(), crate::clamp_epoch_ms(now)],
                    )
                    .map_err(sqlite_error)?;
                    if plan.head_changed()
                        && let Some(old_leaf_node_id) = plan.old_leaf_node_id()
                    {
                        retire_unreachable_ancestry_conn(tx, old_leaf_node_id)?;
                    }
                    for completed in &commit.completed_queue_claims {
                        for batch_id in &completed.batch_ids {
                            tx.execute(
                                "INSERT INTO wake_redelivery_fences (
                                    session_id, process_id, allocation_floor
                                 )
                                 SELECT batch.session_id,
                                        json_extract(item.payload_json, '$.wake.process_id'),
                                        json_extract(item.payload_json, '$.wake.sequence')
                                 FROM queued_work_batches AS batch
                                 JOIN queued_work_items AS item
                                   ON item.batch_id = batch.batch_id
                                 WHERE batch.session_id = ?1
                                   AND batch.batch_id = ?2
                                   AND batch.claim_id = ?3
                                   AND batch.claim_token = ?4
                                   AND json_extract(item.payload_json, '$.type') = 'process_wake'
                                 ON CONFLICT(session_id, process_id) DO UPDATE SET
                                   allocation_floor = MAX(
                                       wake_redelivery_fences.allocation_floor,
                                       excluded.allocation_floor
                                   )",
                                params![
                                    completed.session_id.as_str(),
                                    batch_id,
                                    completed.claim_id,
                                    completed.lease_token
                                ],
                            )
                            .map_err(sqlite_error)?;
                            tx.execute(
                                "DELETE FROM queued_work_batches
                                 WHERE session_id = ?1
                                   AND batch_id = ?2
                                   AND claim_id = ?3
                                   AND claim_token = ?4",
                                params![
                                    completed.session_id.as_str(),
                                    batch_id,
                                    completed.claim_id,
                                    completed.lease_token
                                ],
                            )
                            .map_err(sqlite_error)?;
                        }
                    }
                    for completed in &commit.completed_turn_input_claims {
                        for input_id in &completed.input_ids {
                            // One conditional write for both settlement
                            // regimes: the claim fields are an optional
                            // predicate strengthener, and either way exactly
                            // one row must change (ADR 0069 section 5).
                            let settled = match completed.claim.as_ref() {
                                Some(claim) => tx.execute(
                                    &format!(
                                        "UPDATE pending_turn_inputs
                                         SET {TURN_INPUT_SETTLEMENT_ASSIGNMENTS}
                                         WHERE session_id = ?1
                                           AND input_id = ?2
                                           AND claim_id = ?4
                                           AND claim_token = ?5"
                                    ),
                                    params![
                                        completed.session_id.as_str(),
                                        input_id,
                                        lash_core::TurnInputState::Completed.as_str(),
                                        claim.claim_id,
                                        claim.lease_token,
                                    ],
                                ),
                                None => tx.execute(
                                    &format!(
                                        "UPDATE pending_turn_inputs
                                         SET {TURN_INPUT_SETTLEMENT_ASSIGNMENTS}
                                         WHERE session_id = ?1
                                           AND input_id = ?2
                                           AND claim_id IS NULL
                                           AND state NOT IN ({terminal_states})",
                                        terminal_states =
                                            unclaimed_turn_input_terminal_states_sql()
                                    ),
                                    params![
                                        completed.session_id.as_str(),
                                        input_id,
                                        lash_core::TurnInputState::Completed.as_str(),
                                    ],
                                ),
                            }
                            .map_err(sqlite_error)?;
                            if settled != 1 {
                                return Err(match completed.claim.as_ref() {
                                    Some(claim) => StoreError::TurnInputClaimSuperseded {
                                        session_id: completed.session_id.clone(),
                                        claim_id: claim.claim_id.clone(),
                                        row_id: Some(input_id.clone().into_boxed_str()),
                                        superseding_claim_id: None,
                                        superseding_session_lease_generation: None,
                                    },
                                    None => StoreError::UnclaimedTurnInputSettlementSuperseded {
                                        session_id: completed.session_id.clone(),
                                        input_id: input_id.clone(),
                                        observed_state: None,
                                        superseding_claim_id: None,
                                    },
                                });
                            }
                        }
                    }
                    let mut turn_cancel_input_outcome = lash_core::TurnCancelInputOutcome::default();
                    if let Some(turn_id) = commit.interrupted_turn_input_turn_id.as_ref() {
                        let cancellation = commit.interrupted_turn_input_cancellation.as_ref();
                        let disposition = cancellation.map_or(
                            lash_core::TurnCancelDisposition::Defer,
                            |evidence| evidence.undelivered,
                        );
                        if let Some(evidence) = cancellation {
                            reconcile_turn_cancel_winner_conn(
                                tx,
                                &commit.session_id,
                                turn_id,
                                evidence,
                            )?;
                        }
                        let input_ids = {
                            let mut stmt = tx
                                .prepare(
                                    "SELECT input_id, ingress_json, input_json
                                     FROM pending_turn_inputs
                                     WHERE session_id = ?1 AND state = ?2 ORDER BY enqueue_seq ASC",
                                )
                                .map_err(sqlite_error)?;
                            let rows = stmt
                                .query_map(
                                    params![
                                        commit.session_id.as_str(),
                                        lash_core::TurnInputState::PendingActive.as_str()
                                    ],
                                    |row| {
                                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, String>(2)?))
                                    },
                                )
                                .map_err(sqlite_error)?;
                            let mut input_ids = Vec::new();
                            for row in rows {
                                let (input_id, ingress_json, input_json) = row.map_err(sqlite_error)?;
                                let ingress = decode_turn_input_ingress(ingress_json)?;
                                if ingress
                                    .active_turn_id()
                                    .is_some_and(|active| active == turn_id)
                                {
                                    input_ids.push((input_id, decode_stored_json(&input_json, "turn input")?));
                                }
                            }
                            input_ids
                        };
                        let next_turn_ingress =
                            encode_json(&lash_core::TurnInputIngress::NextTurn)?;
                        let mut stmt = tx
                            .prepare(
                                "UPDATE pending_turn_inputs
                                 SET state = ?3,
                                     ingress_json = COALESCE(?4, ingress_json),
                                     claim_id = NULL,
                                     claim_owner_id = NULL,
                                     claim_owner_incarnation_id = NULL,
                                     claim_token = NULL,
                                     claim_session_lease_generation = 0
                                 WHERE session_id = ?1 AND input_id = ?2",
                            )
                            .map_err(sqlite_error)?;
                        for (input_id, payload) in input_ids {
                            stmt.execute(params![
                                commit.session_id.as_str(),
                                input_id,
                                match disposition {
                                    lash_core::TurnCancelDisposition::Defer => lash_core::TurnInputState::DeferredNextTurn.as_str(),
                                    lash_core::TurnCancelDisposition::Drop => lash_core::TurnInputState::Cancelled.as_str(),
                                },
                                match disposition {
                                    lash_core::TurnCancelDisposition::Defer => Some(next_turn_ingress.as_str()),
                                    lash_core::TurnCancelDisposition::Drop => None,
                                }
                            ])
                            .map_err(sqlite_error)?;
                            let affected = lash_core::TurnCancelAffectedInput { input_id, payload, disposition };
                            if cancellation.is_some() {
                                append_turn_cancel_outcome_conn(tx, &commit.session_id, turn_id, affected.clone())?;
                                turn_cancel_input_outcome.affected_inputs.push(affected);
                            }
                        }
                    }
                    crate::attachments::commit_attachment_refs_conn(
                        tx, &commit.session_id, &commit.committed_attachment_ids, now as i64,
                    )?;
                    if let Some(turn_id) = commit.turn_commit.operation.turn_id() {
                        tx.execute(
                            "UPDATE attachment_manifest
                                 SET committed_at_ms = COALESCE(committed_at_ms, ?1)
                                 WHERE session_id = ?2
                                   AND owner_kind = ?4
                                   AND owner_id = ?3
                                   AND committed_at_ms IS NULL",
                            params![
                                now as i64,
                                commit.session_id.as_str(),
                                turn_id.as_str(),
                                AttachmentOwnerKind::Turn.as_str()
                            ],
                        )
                        .map_err(sqlite_error)?;
                    }
                    let mut enqueued_queue_batches = Vec::new();
                    for (index, batch) in commit.enqueued_queue_batches.iter().enumerate() {
                        let enqueue_nonce = enqueue_nonce_start
                            .checked_add(index as u64)
                            .ok_or(StoreError::MonotonicCounterOverflow {
                                counter: "queued_work_enqueue_sequence",
                                current: enqueue_nonce_start,
                            })?;
                        enqueued_queue_batches.push(enqueue_queued_work_conn(
                            tx,
                            batch,
                            now,
                            enqueue_nonce,
                        )?);
                    }
                    let mut result = plan.result(
                        stored_checkpoint.checkpoint_ref,
                        stored_checkpoint.manifest,
                        enqueued_queue_batches,
                    );
                    result.turn_cancel_input_outcome = turn_cancel_input_outcome;
                    {
                        let receipt = plan.receipt_write(&result);
                        let result_json = encode_json(receipt.result)?;
                        let identity = append_identity_columns(receipt.append_request_identity);
                        tx.execute(
                            "INSERT INTO runtime_turn_commits (
                                session_id, turn_id, turn_commit_hash, result_json, committed_at_ms,
                                request_identity_hash, requested_node_count, identity_encoding_version
                             )
                             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                            params![
                                receipt.session_id.as_str(),
                                receipt.operation_key,
                                receipt.turn_commit_hash,
                                result_json,
                                now as i64,
                                identity.0,
                                identity.1,
                                identity.2,
                            ],
                        )
                        .map_err(sqlite_error)?;
                        if commit.turn_commit.operation.key == "session-command" {
                            for batch_id in commit
                                .completed_queue_claims
                                .iter()
                                .flat_map(|completion| &completion.batch_ids)
                            {
                                let marker = lash_core::store_backend_support::session_command_batch_completion_key(
                                    &commit.session_id,
                                    batch_id,
                                )?;
                                tx.execute(
                                    "INSERT INTO runtime_turn_commits (
                                        session_id, turn_id, turn_commit_hash, result_json,
                                        committed_at_ms, request_identity_hash,
                                        requested_node_count, identity_encoding_version
                                     ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, NULL, NULL)",
                                    params![
                                        commit.session_id.as_str(),
                                        marker,
                                        receipt.turn_commit_hash,
                                        result_json,
                                        now as i64,
                                    ],
                                )
                                .map_err(sqlite_error)?;
                            }
                        }
                    }
                    if let Some(completion) = commit.release_session_execution_lease.as_ref() {
                        let _release_was_current =
                            release_session_execution_lease_conn(tx, completion)?;
                        // FIG-884: head CAS is commit authority; release is ancillary.
                    }
                    Ok(result)
                })();
                // Roll back on a `StoreError` so a failure after the first
                // write (e.g. a head-revision conflict surfaced mid-commit, or a
                // backend write error) does not leave the partial transaction
                // committed, while still carrying the typed error to the caller.
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(Ok(value))),
                    Err(err) => Ok(TxOutcome::Rollback(Err(err))),
                }
            })
            .await
            .map_err(sqlite_error)??;
        Ok(result)
    }

    async fn admit_and_bind_session(
        &self,
        binding: &lash_core::SessionBinding,
    ) -> Result<lash_core::SessionAdmission, StoreError> {
        binding.validate()?;
        self.bind_session(&binding.session_id)?;
        let session_id = binding.session_id.clone();
        let created_at_ms = self.clock.timestamp_ms();
        let meta = SessionMeta {
            session_id: session_id.clone(),
            relation: binding.relation.clone(),
            pending_observer_intents: Vec::new(),
        };
        self.conn
            .write_flow(move |tx| {
                let outcome: Result<lash_core::SessionAdmission, StoreError> = (|| {
                    ensure_session_not_deleted_conn(tx, &session_id)?;
                    let inserted = crate::session_meta::write_session_meta(
                        tx,
                        &meta,
                        crate::session_meta::SessionMetaWrite::Insert,
                        created_at_ms,
                    )?;
                    Ok(if inserted {
                        lash_core::SessionAdmission::Created
                    } else {
                        lash_core::SessionAdmission::Rebound
                    })
                })();
                Ok(match outcome {
                    Ok(admission) => TxOutcome::Commit(Ok(admission)),
                    Err(err) => TxOutcome::Rollback(Err(err)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError> {
        Store::save_session_meta(self, meta).await
    }

    async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError> {
        Store::load_session_meta(self).await
    }
}
