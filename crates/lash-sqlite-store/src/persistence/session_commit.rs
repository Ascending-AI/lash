mod graph_nodes;

use super::*;
use crate::session_sql::session_sql;
use graph_nodes::{insert_graph_nodes_conn, occupied_node_ids_conn};
use lash_core_execution::FleetFormatStore;

/// Apply a commit's frame transition: carry out of `ended` into the
/// successor, then end every frame in `left` (the frames the commit leaves,
/// `ended` among them) gated on the transition's execution.
fn commit_frame_transition_tx(
    tx: &rusqlite::Connection,
    transition: &lash_core_execution::store::FrameTransition,
    left: &[lash_core_execution::FrameNodeId],
    now_ms: u64,
) -> Result<(), StoreError> {
    use lash_core_execution::ArtifactReferrer;
    let source = ArtifactReferrer::FrameEnvironment(transition.ended.clone());
    let successor = ArtifactReferrer::FrameEnvironment(transition.successor.clone());
    if crate::artifact_store::artifact_fenced_tx(tx, &successor).map_err(sqlite_error)? {
        return Err(StoreError::ArtifactReferrerEnded {
            referrer: successor,
        });
    }
    let sql = crate::artifact_store::artifact_sql();
    let mut carries = transition.carries.clone();
    for carry in &transition.carries {
        if carry.store != lash_core_execution::ArtifactStoreId::ProcessDefinition {
            continue;
        }
        let id = lash_core_execution::ProcessDefinitionId::parse(&carry.artifact_ref)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        let blob: String = tx
            .query_row(
                sql.refs.select_blob_ref.sql(),
                params!["process_definition", carry.artifact_ref],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;
        let bytes = SqliteStore::get_blob_conn(tx, &lash_core_execution::store::BlobRef(blob))?
            .ok_or_else(|| {
                StoreError::Backend("carried definition has no descriptor bytes".into())
            })?;
        let draft = lash_core_execution::ProcessDefinitionDraft::from_store_bytes(&id, &bytes)
            .map_err(|e| StoreError::Backend(e.to_string()))?;
        carries.extend(draft.artifacts().iter().cloned());
    }
    carries.sort();
    carries.dedup();
    for carry in &carries {
        let Some(namespace) = crate::artifact_store::store_namespace(&carry.store) else {
            continue;
        };
        let exists: bool = tx
            .query_row(
                sql.edges.select_edge_exists.sql(),
                params![
                    namespace,
                    carry.artifact_ref,
                    source.kind().as_str(),
                    source.canonical_id()
                ],
                |row| row.get(0),
            )
            .map_err(sqlite_error)?;
        if !exists {
            return Err(StoreError::ArtifactCarryMissing {
                artifact_ref: carry.artifact_ref.clone(),
                to: successor,
            });
        }
        crate::conn::cached_execute(
            tx,
            sql.edges.insert_edge.sql(),
            params![
                namespace,
                carry.artifact_ref,
                successor.kind().as_str(),
                successor.canonical_id()
            ],
        )
        .map_err(sqlite_error)?;
    }
    end_frames_tx(
        tx,
        transition.ended.session_id(),
        left,
        Some(&transition.gate),
        now_ms,
    )
}

/// End each frame in `left`: fence it and upsert its `Ended` cleanup with no
/// carries, gated on `gate` (ADR 0113 §3.1).
fn end_frames_tx(
    tx: &rusqlite::Connection,
    session_id: &SessionId,
    left: &[lash_core_execution::FrameNodeId],
    gate: Option<&lash_sansio::EffectJournalIdentity>,
    now_ms: u64,
) -> Result<(), StoreError> {
    for frame in left {
        let referrer = lash_core_execution::ArtifactReferrer::FrameEnvironment(
            lash_core_execution::FrameEnvironmentId::new(session_id.clone(), frame.clone()),
        );
        crate::artifact_store::fence_artifact_referrer_tx(tx, &referrer, now_ms)
            .map_err(sqlite_error)?;
        crate::obligation_ledger::arm_cleanup_tx(
            tx,
            &lash_core_execution::ArtifactCleanup::ended(referrer, Vec::new(), gate.cloned()),
            now_ms,
        )?;
    }
    Ok(())
}

#[async_trait::async_trait]
impl SessionCommitStore for SqliteStore {
    async fn committed_turn_exists(
        &self,
        session_id: &SessionId,
        turn_id: &lash_core_execution::TurnId,
    ) -> Result<bool, StoreError> {
        let session_id = session_id.clone();
        let key = lash_core_execution::store_backend_support::turn_commit_receipt_storage_key(
            &session_id,
            turn_id,
        )?;
        self.read_connection()
            .call(move |conn| {
                let exists: bool = conn.query_row(
                    session_sql().turn_commits.exists_for_turn.sql(),
                    params![session_id.as_str(), key],
                    |row| row.get(0),
                )?;
                Ok(exists)
            })
            .await
            .map_err(sqlite_error)
    }

    async fn read_session_state_version(&self, session_id: &SessionId) -> Result<u32, StoreError> {
        let session_id = session_id.clone();
        let fleet = self.fleet_format();
        self.read_connection()
            .call(move |conn| Ok(read_session_state_version_conn(conn, &session_id, fleet)))
            .await
            .map_err(sqlite_error)?
    }

    async fn admit_session_state(
        &self,
        session_id: &SessionId,
    ) -> Result<lash_core_execution::store::SessionStateAdmission, StoreError> {
        let session_id = session_id.clone();
        let fleet = self.fleet_format();
        self.read_connection()
            .call(move |conn| {
                Ok(
                    read_session_state_version_conn(conn, &session_id, fleet).map(|version| {
                        lash_core_execution::store::SessionStateAdmission {
                            session_id: session_id.clone(),
                            version,
                        }
                    }),
                )
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn retain_admission_base(
        &self,
        session_id: &SessionId,
        base: &lash_core_execution::store::SessionHeadRef,
    ) -> Result<(), StoreError> {
        let session_id = session_id.clone();
        let checkpoint_ref = base.checkpoint.clone();
        self.conn
            .write_flow(move |tx| {
                let outcome = crate::session_meta::retain_admission_base_conn(
                    tx,
                    &session_id,
                    checkpoint_ref.as_ref(),
                );
                Ok(match outcome {
                    Ok(()) => TxOutcome::Commit(Ok(())),
                    Err(error) => TxOutcome::Rollback(Err(error)),
                })
            })
            .await
            .map_err(sqlite_error)?
    }

    async fn load_session_head_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionHeadMeta>, StoreError> {
        self.read_session_state_version(session_id).await?;
        SqliteStore::load_session_head_meta(self, session_id).await
    }

    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitReceipt, StoreError> {
        lash_core_execution::store::validate_session_id(&commit.session_id)?;
        // The commit is planned before `BEGIN` under the handle's last
        // observed `F`. When the fence reads another writable epoch the
        // transaction writes nothing and the commit is planned again under
        // the new one, once (ADR 0115 §2.3); `F` moves at most once per
        // release, so a second move inside one commit is contention.
        let mut planned_under = self.fleet_format();
        let mut planner =
            lash_core_execution::store::RuntimeCommitPlanner::prepare(commit, planned_under)?;
        let mut replanned = false;
        let blob_profile = self.options.blob_profile;
        let now = self.clock.timestamp_ms();
        loop {
            match self
                .commit_attempt(planner, planned_under, blob_profile, now)
                .await?
            {
                CommitAttempt::Done(result) => return *result,
                CommitAttempt::FleetMoved(returned) if !replanned => {
                    replanned = true;
                    planned_under = self.fleet_format();
                    planner = lash_core_execution::store::RuntimeCommitPlanner::prepare(
                        returned.commit().clone(),
                        planned_under,
                    )?;
                }
                CommitAttempt::FleetMoved(_) => return Err(StoreError::Contended),
            }
        }
    }

    async fn settle_observer_intents(
        &self,
        session_id: &SessionId,
        remaining: Vec<lash_core_execution::facade_support::SessionObserverIntent>,
    ) -> Result<(), StoreError> {
        SqliteStore::settle_observer_intents(self, session_id, remaining).await
    }

    async fn load_session_meta(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError> {
        SqliteStore::load_session_meta(self, session_id).await
    }

    async fn load_session_meta_for_commit(
        &self,
        session_id: &SessionId,
    ) -> Result<Option<SessionMeta>, StoreError> {
        let session_id = session_id.clone();
        self.conn
            .call(move |conn| {
                Ok((|| {
                    ensure_session_not_deleted_conn(conn, &session_id)?;
                    crate::session_meta::load_session_meta(conn, Some(&session_id))
                })())
            })
            .await
            .map_err(sqlite_error)?
    }
}

#[cfg(test)]
mod artifact_frame_transition_tests;

/// One attempt of a planned runtime commit.
enum CommitAttempt {
    /// The transaction ran: committed, or rolled back with a typed refusal.
    Done(Box<Result<RuntimeCommitReceipt, StoreError>>),
    /// The fence read an epoch other than the one the plan was made under;
    /// nothing was written, and the planner comes back to be planned again.
    FleetMoved(Box<lash_core_execution::store::RuntimeCommitPlanner>),
}

/// A turn commit's stored receipt row, as the idempotent replay reads it.
type PriorReceiptRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    Option<i64>,
    Option<i64>,
);

impl SqliteStore {
    /// One transaction of a planned runtime commit, fenced: it answers
    /// `FleetMoved` with the planner, having written nothing, when the fence
    /// reads an epoch other than `planned_under`.
    async fn commit_attempt(
        &self,
        planner: lash_core_execution::store::RuntimeCommitPlanner,
        planned_under: lash_core_execution::FleetFormat,
        blob_profile: crate::BuiltinBlobProfile,
        now: u64,
    ) -> Result<CommitAttempt, StoreError> {
        self.conn
            .write_flow(move |tx| {
                if tx.fleet().version() != planned_under.version() {
                    return Ok(TxOutcome::Rollback(CommitAttempt::FleetMoved(Box::new(
                        planner,
                    ))));
                }
                let outcome = apply_runtime_commit_conn(
                    tx,
                    &planner,
                    lash_core_execution::store::HeadWriter::Store,
                    blob_profile,
                    now,
                );
                // Roll back on a `StoreError` so a failure after the first
                // write (e.g. a head-revision conflict surfaced mid-commit, or a
                // backend write error) does not leave the partial transaction
                // committed, while still carrying the typed error to the caller.
                match outcome {
                    Ok(value) => Ok(TxOutcome::Commit(CommitAttempt::Done(Box::new(Ok(value))))),
                    Err(err) => Ok(TxOutcome::Rollback(CommitAttempt::Done(Box::new(Err(err))))),
                }
            })
            .await
            .map_err(sqlite_error)
    }
}

/// Apply `planner`'s runtime commit inside the open write transaction `tx`:
/// the receipt replay, the head compare-and-set and every write of the
/// commit, or a typed refusal with the transaction left for the caller to
/// roll back. The runtime store's own commit and the session actor's head
/// commits (in its fenced owner transaction) both apply a commit here,
/// `writer` naming which.
pub(crate) fn apply_runtime_commit_conn(
    tx: &crate::conn::FencedTx<'_>,
    planner: &lash_core_execution::store::RuntimeCommitPlanner,
    writer: lash_core_execution::store::HeadWriter,
    blob_profile: crate::BuiltinBlobProfile,
    now: u64,
) -> Result<RuntimeCommitReceipt, StoreError> {
    let fleet = tx.fleet();
    let commit = planner.commit();
    // The commit's plugin state and config namespaces are
    // admitted against the fleet record's writer ranges
    // before anything of the commit is written (FIG-4746).
    tx.admit_plugin_writers(planner.plugin_publication())
        .map_err(sqlite_error)?;
    ensure_session_not_deleted_conn(tx, &commit.session_id)?;
    let admitted = tx
        .query_row(
            session_sql().meta_sqlite.exists_materialized.sql(),
            params![commit.session_id.as_str()],
            |_| Ok(()),
        )
        .optional()
        .map_err(sqlite_error)?
        .is_some();
    if !admitted {
        return Err(StoreError::SessionNotFound {
            session_id: commit.session_id.clone(),
        });
    }
    let existing = try_load_session_head_meta_from_conn(tx, &commit.session_id, fleet)?;
    planner.validate_node_derivation()?;
    {
        let prior: Option<PriorReceiptRow> = tx
            .query_row(
                session_sql().turn_commits.select_receipt.sql(),
                params![commit.session_id.as_str(), planner.operation_key()],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()
            .map_err(sqlite_error)?;
        if let Some((
            stored_hash,
            result_json,
            stored_outcome,
            stored_identity,
            stored_version,
            stored_requested_node_count,
        )) = prior
        {
            // One codec owns the unit-shape and integer-range checks.
            // The ancestor stays write-only because the request hash binds it.
            let append_request_identity =
                lash_core_execution::store_backend_support::decode_append_request_identity(
                    &commit.turn_commit.operation.key,
                    stored_identity,
                    stored_version,
                    stored_requested_node_count,
                )?;
            let result = lash_core_execution::store::decode_runtime_commit_receipt_for_fleet(
                &commit.session_id,
                planner.operation_key(),
                &result_json,
                fleet,
            )?;
            lash_core_execution::store::validate_turn_commit_outcome_code(
                &result,
                stored_outcome.as_deref(),
            )?;
            let prior = lash_core_execution::store::RuntimeCommitReceiptRecord {
                turn_commit_hash: stored_hash,
                result,
                append_request_identity,
            };
            let replay = planner.decide_receipt(Some(prior))?;
            if let Some(replay) = replay {
                return Ok(replay.into_result());
            }
        }
    }
    let actual_revision = existing.as_ref().map_or(0, |meta| meta.head_revision);
    let old_leaf_node_id = existing.as_ref().and_then(|head| head.leaf_node_id.clone());
    let parent_leaf = old_leaf_node_id
        .as_deref()
        .map(|leaf_node_id| {
            tx.query_row(
                session_sql().graph_sqlite.select_parent_facts.sql(),
                params![leaf_node_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()
            .map_err(sqlite_error)?
            .map(
                |(generation, frame_node_id, owner)| -> Result<_, StoreError> {
                    let generation = u64::try_from(generation).map_err(|_| {
                        stored_data_corrupt(
                            "SessionGraph node",
                            format!("negative generation {generation}"),
                        )
                    })?;
                    Ok((
                        lash_core_execution::store::ParentNodeFacts {
                            node_id: leaf_node_id.to_string().try_into()?,
                            generation,
                            frame_node_id: frame_node_id.try_into()?,
                        },
                        lash_core_execution::store_backend_support::PathNode {
                            node_id: leaf_node_id.to_string().try_into()?,
                            owner_session_id: owner.try_into()?,
                            generation,
                        },
                    ))
                },
            )
            .transpose()
        })
        .transpose()?
        .flatten();
    let (parent_node_facts, parent_path_node) = parent_leaf.unzip();
    // The ceilings select the requested ancestor; the head
    // leaf's parent edges decide whether it is active
    // (ADR 0057, edge authority).
    let requested_ancestor_is_active = match (
        requested_append_ancestor(&commit.turn_commit),
        parent_node_facts.as_ref(),
    ) {
        (None, _) => true,
        (Some(_), None) => false,
        (Some(required), Some(parent)) => match tx
            .query_row(
                session_sql().graph_sqlite.select_readable_ancestor.sql(),
                params![
                    required,
                    commit.session_id.as_str(),
                    i64::try_from(parent.generation).map_err(|_| {
                        StoreError::Backend(
                            "parent generation does not fit SQLite INTEGER".to_string(),
                        )
                    })?
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()
            .map_err(sqlite_error)?
        {
            None => false,
            Some((owner, generation)) => crate::history::head_reaches(
                tx,
                parent_path_node.clone(),
                lash_core_execution::store_backend_support::PathNode {
                    node_id: required.to_string().try_into()?,
                    owner_session_id: owner.try_into()?,
                    generation: u64::try_from(generation).map_err(|_| {
                        stored_data_corrupt(
                            "SessionGraph node",
                            format!("negative generation {generation}"),
                        )
                    })?,
                },
            )?,
        },
    };
    let occupied_node_ids = occupied_node_ids_conn(tx, commit.graph.nodes())?;
    let published_leaf = match old_leaf_node_id {
        None => lash_core_execution::store::PublishedLeafFacts::Absent,
        Some(node_id) => match parent_node_facts {
            Some(parent) => lash_core_execution::store::PublishedLeafFacts::Live(parent),
            None => lash_core_execution::store::PublishedLeafFacts::Retired { node_id },
        },
    };
    let plan = planner.plan(lash_core_execution::store::FreshRuntimeCommitFacts {
        actual_head_revision: actual_revision,
        published_leaf,
        requested_ancestor_is_active,
        occupied_node_ids,
    })?;
    // The bound turn owns the head (FIG-4202): a write
    // outside every run is refused while a run or an open
    // command owns it. A replayed receipt above answered its
    // first outcome already, and the plan's own refusals (a
    // moved head) answer first.
    if lash_core_execution::store::head_write_needs_ownership(
        commit,
        writer,
        existing.as_ref().is_some_and(|head| !head.is_created()),
    ) {
        let facts = crate::session_runs::head_ownership_facts_conn(tx, &commit.session_id)?;
        lash_core_execution::store::require_unowned_head(&commit.session_id, facts)?;
    }
    let sql_head_revision = sql_monotonic_counter_value(
        "session_head_revision",
        plan.actual_head_revision(),
        plan.next_head_revision(),
    )?;
    let stored_checkpoint =
        SqliteStore::put_checkpoint_conn(tx, &commit.checkpoint, blob_profile, fleet)?;

    insert_graph_nodes_conn(tx, &commit.session_id, commit.graph.nodes(), &plan)?;
    let meta = plan.head_meta(stored_checkpoint.checkpoint_ref.clone());
    // Divergence ruling (FIG-3381): SQLite carries no CAS
    // predicate on its head upsert and needs none. `existing`
    // was read inside this `BEGIN IMMEDIATE` transaction,
    // which is SQLite's database-wide single-writer lock, so
    // no revision can move between that read and this write.
    // The invariant is asserted rather than assumed: if the
    // head read is ever moved out of the write transaction,
    // this refuses the publication instead of publishing over
    // a revision nobody held.
    lash_core_execution::store_backend_support::require_single_writer_head_publication(
        &commit.session_id,
        crate::SQLITE_BACKEND,
        !tx.is_autocommit(),
    )?;
    // Read the published revision again, inside the same
    // write transaction, and let the shared verdict decide.
    // This is SQLite's equivalent of PostgreSQL's
    // `SELECT head_revision … FOR UPDATE`: under
    // `BEGIN IMMEDIATE` it must still be the revision the plan
    // was built on, and if the earlier read is ever moved out
    // of this transaction it will not be.
    let published_revision = tx
        .query_row(
            session_sql().head.select_revision.sql(),
            params![commit.session_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(sqlite_error)?
        .map(|revision| u64_from_sql("SessionHeadMeta", "head_revision", revision))
        .transpose()
        .map_err(sqlite_error)?
        .unwrap_or(0);
    match lash_core_execution::store_backend_support::head_publication_verdict(
        plan.actual_head_revision(),
        published_revision,
    ) {
        lash_core_execution::store_backend_support::HeadPublicationVerdict::Publish => {}
        lash_core_execution::store_backend_support::HeadPublicationVerdict::HeadMoved {
            observed_head_revision,
            ..
        } => {
            return Err(StoreError::HeadRevisionConflict {
                expected: plan.actual_head_revision(),
                actual: observed_head_revision,
            });
        }
    }
    let left = lash_core_execution::store::frames_left_by_commit(
        existing
            .as_ref()
            .and_then(|head| head.current_frame_node_id.as_ref()),
        &commit.graph,
        meta.current_frame_node_id.as_ref(),
    );
    if let Some(transition) = &commit.frame_transition {
        if transition.ended.session_id() != commit.session_id
            || transition.successor.session_id() != commit.session_id
            || !left.contains(transition.ended.frame_node_id())
            || meta.current_frame_node_id.as_ref() != Some(transition.successor.frame_node_id())
        {
            return Err(StoreError::Backend(
                "frame transition does not match the committed head".into(),
            ));
        }
        commit_frame_transition_tx(tx, transition, &left, now)?;
    } else {
        end_frames_tx(tx, &commit.session_id, &left, None, now)?;
    }
    let head_json = encode_json(&meta.payload())?;
    // The published head is a retained revision from this
    // transaction on. Recording it reads no pin: a pin
    // resolves to it by query whenever something asks.
    crate::revisions::record_revision_conn(
        tx,
        &commit.session_id,
        sql_head_revision,
        meta.leaf_node_id.as_deref(),
        meta.checkpoint_ref.as_ref().map(BlobRef::as_str),
        &head_json,
    )?;
    let changed = crate::conn::cached_execute(
        tx,
        session_sql().head_sqlite.upsert_cas.sql(),
        params![
            meta.session_id.as_str(),
            sql_head_revision,
            plan.actual_head_revision() as i64,
        ],
    )
    .map_err(sqlite_error)?;
    if changed != 1 {
        return Err(StoreError::HeadRevisionConflict {
            expected: plan.actual_head_revision(),
            actual: published_revision,
        });
    }
    let retention = tx
        .prepare_cached(session_sql().meta.touch_last_commit.sql())
        .map_err(sqlite_error)?
        .query_row(
            params![commit.session_id.as_str(), crate::clamp_epoch_ms(now)],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .optional()
        .map_err(sqlite_error)?
        .map(|(kind, last_turns)| lash_core_execution::Retention::from_stored(&kind, last_turns))
        .transpose()?;
    if plan.head_changed()
        && let Some(old_leaf_node_id) = plan.old_leaf_node_id()
    {
        retire_unreachable_ancestry_conn(tx, old_leaf_node_id)?;
    }
    let turn_cancel_input_outcome =
        super::ingress_settlement::settle_commit_ingress_conn(tx, commit, now)?;
    let claim = lash_core_execution::ReferrerClaim::unguarded(
        lash_core_execution::ArtifactReferrer::Session(commit.session_id.clone()),
    )
    .map_err(|error| error.into_store_error("attachment session referrer"))?;
    crate::attachments::acquire_attachment_refs_conn(
        tx,
        &claim,
        &commit.committed_attachment_ids,
        now,
    )?;
    crate::session_runs::write_commit_run_terminal_conn(
        tx,
        commit,
        plan.next_head_revision(),
        now,
    )?;
    // `until_gc` releases nothing here and reads no pin. The
    // other policies release what this publication moved out
    // of their window, once the run's terminal names it. A session with
    // no metadata row recorded no policy and releases nothing either.
    if retention.is_some_and(lash_core_execution::Retention::releases_at_commit) {
        crate::revisions::release_unretained_conn(tx, false, Some(&commit.session_id))?;
    }
    let mut result = plan.result(
        stored_checkpoint.checkpoint_ref,
        stored_checkpoint.manifest,
        now,
        tx.query_row(
            crate::turn_ingress::turn_ingress_sql()
                .family
                .has_admissible_work
                .sql(),
            [commit.session_id.as_str()],
            |row| row.get::<_, bool>(0),
        )
        .map_err(sqlite_error)?,
    );
    result.turn_cancel_input_outcome = turn_cancel_input_outcome;
    {
        let receipt = plan.receipt_write(&result);
        let result_json = encode_json(receipt.result)?;
        let identity = append_identity_columns(receipt.append_request_identity);
        crate::conn::cached_execute(
            tx,
            session_sql().turn_commits.insert.sql(),
            params![
                receipt.session_id.as_str(),
                receipt.operation_key,
                receipt.turn_commit_hash,
                result_json,
                receipt
                    .result
                    .outcome
                    .as_ref()
                    .map(|outcome| outcome.as_str()),
                now as i64,
                identity.0,
                identity.1,
                identity.2,
                !result.failure_evidence.is_empty(),
                crate::catalog::catalog_reads::next_turn_change_sequence(tx)?,
                sql_head_revision,
            ],
        )
        .map_err(sqlite_error)?;
    }

    Ok(result)
}
