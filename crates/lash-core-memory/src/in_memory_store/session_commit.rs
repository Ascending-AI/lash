//! The in-memory store's `SessionCommitStore` implementation.
//!
//! Split out of `in_memory_store.rs` so the parent file stays inside the
//! production line budget; the parent's items are in scope through `super`.
use super::*;

#[async_trait::async_trait]
impl crate::store::SessionCommitStore for InMemorySessionStore {
    async fn read_session_state_version(&self) -> Result<u32, crate::StoreError> {
        self.read_session_state_version_in_memory()
    }

    async fn committed_turn_exists(
        &self,
        turn_id: &crate::TurnId,
    ) -> Result<bool, crate::StoreError> {
        let Some(session_id) = self.bound_session_id.lock_recover().clone() else {
            return Ok(false);
        };
        let key =
            crate::store_backend_support::turn_commit_receipt_storage_key(&session_id, turn_id)?;
        Ok(self
            .runtime_turn_commits
            .lock_recover()
            .contains_key(&(session_id, key)))
    }

    async fn admit_session_state(
        &self,
        lease: &crate::SessionExecutionLeaseAuthority,
    ) -> Result<crate::store::SessionStateAdmission, crate::StoreError> {
        self.admit_session_state_in_memory(lease)
    }

    async fn load_session(
        &self,
    ) -> Result<Option<crate::store::PersistedSessionRead>, crate::store::StoreError> {
        self.guard_session_payload_in_memory()?;
        #[cfg(any(test, feature = "testing"))]
        self.refuse_injected_counter_defect("session_head_revision")?;
        #[cfg(any(test, feature = "testing"))]
        let load_call = self
            .load_session_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        #[cfg(any(test, feature = "testing"))]
        if self
            .fail_load_session_on_call
            .lock_recover()
            .is_some_and(|call| call == load_call)
        {
            self.fail_load_session_on_call.lock_recover().take();
            return Err(crate::StoreError::Backend(
                "injected load-session failure".to_string(),
            ));
        }
        let _transaction = self.write_transaction.lock_recover();
        let Some(meta) = self.session_head_meta.lock_recover().clone() else {
            return Ok(None);
        };
        let tombstoned = self.tombstoned_node_ids.lock_recover().clone();
        let global_graph = self.global_session_graph.lock_recover().clone();
        let map_graph_corruption =
            |error: crate::StoreError| crate::StoreError::StoredDataCorrupt {
                record_kind: "SessionGraph",
                message: error.to_string(),
            };
        let mut graph =
            crate::SessionGraph::from_nodes(global_graph.nodes.clone(), meta.leaf_node_id.clone())
                .map_err(map_graph_corruption)?
                .try_trim_to_active_path()
                .map_err(map_graph_corruption)?;
        if !tombstoned.is_empty() {
            let leaf_node_id = graph
                .leaf_node_id
                .clone()
                .filter(|leaf| !tombstoned.contains(leaf));
            graph = crate::SessionGraph::from_nodes(
                graph
                    .nodes
                    .iter()
                    .filter(|node| !tombstoned.contains(&node.node_id))
                    .cloned()
                    .collect(),
                leaf_node_id,
            )
            .map_err(map_graph_corruption)?;
        }
        graph
            .validate_resident_integrity()
            .map_err(map_graph_corruption)?;
        let mut turn_failure_settlements = self
            .runtime_turn_commits
            .lock_recover()
            .iter()
            .filter_map(|((owner_session_id, turn_id), record)| {
                (owner_session_id == meta.session_id && !record.result.failure_evidence.is_empty())
                    .then(|| {
                        (
                            record.committed_at_ms,
                            crate::TurnFailureSettlement {
                                turn_id: turn_id.clone(),
                                evidence: record.result.failure_evidence.clone(),
                            },
                        )
                    })
            })
            .collect::<Vec<_>>();
        turn_failure_settlements.sort_by(|(left_at, left), (right_at, right)| {
            left_at
                .cmp(right_at)
                .then_with(|| left.turn_id.cmp(&right.turn_id))
        });
        let turn_failure_settlements = turn_failure_settlements
            .into_iter()
            .map(|(_, settlement)| settlement)
            .collect();
        Ok(Some(crate::store::PersistedSessionRead {
            session_id: meta.session_id,
            head_revision: meta.head_revision,
            config: meta.config,
            current_frame_node_id: meta.current_frame_node_id,
            graph,
            checkpoint_ref: meta.checkpoint_ref,
            checkpoint: self.checkpoint.lock_recover().clone(),
            token_ledger: crate::store::merge_token_ledger_entries_checked(
                self.usage_deltas
                    .lock_recover()
                    .iter()
                    .map(|delta| delta.entry.clone())
                    .collect(),
            )?,
            turn_failure_settlements,
        }))
    }

    async fn load_session_head_meta(
        &self,
    ) -> Result<Option<crate::SessionHeadMeta>, crate::StoreError> {
        self.read_session_state_version().await?;
        #[cfg(any(test, feature = "testing"))]
        self.refuse_injected_counter_defect("session_head_revision")?;
        #[cfg(any(test, feature = "testing"))]
        self.load_session_head_meta_count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        #[cfg(any(test, feature = "testing"))]
        if self
            .fail_next_load_session_head_meta
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            return Err(crate::StoreError::Backend(
                "injected load-session-head failure".to_string(),
            ));
        }
        Ok(self.session_head_meta.lock_recover().clone())
    }

    /// FIG-653: fork-lineage visibility is graph membership, not authorization.
    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<crate::SessionNodeRecord>, crate::store::StoreError> {
        if self.tombstoned_node_ids.lock_recover().contains(node_id) {
            return Ok(None);
        }
        if !self.node_visible_to_bound_session(node_id)? {
            return Ok(None);
        }
        let graph = self.global_session_graph.lock_recover();
        Ok(graph.find_node(node_id).cloned())
    }

    async fn commit_runtime_state(
        &self,
        commit: crate::store::RuntimeCommit,
    ) -> Result<crate::store::RuntimeCommitReceipt, crate::store::StoreError> {
        let planner = crate::store::RuntimeCommitPlanner::prepare(commit)?;
        let commit = planner.commit();
        let session_id = commit.session_id.clone();
        let transaction_now = self.clock.timestamp_ms();
        let _transaction = self.write_transaction.lock_recover();
        #[cfg(any(test, feature = "testing"))]
        self.commit_write_transaction_count
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.ensure_session_not_deleted(&session_id)?;
        turn_cancel_closure::verify_pre_replay_fence(self, commit, transaction_now)?;
        #[cfg(any(test, feature = "testing"))]
        if let Some(error) = self.fail_next_runtime_commit.lock_recover().take() {
            return Err(error);
        }
        #[cfg(any(test, feature = "testing"))]
        if let Some(request) = self
            .inject_turn_cancel_before_next_runtime_commit
            .lock_recover()
            .take()
        {
            debug_assert_eq!(request.address.session_id, commit.session_id);
            let mut requests = self.turn_cancel_requests.lock_recover();
            match requests.get_mut(&request.address.turn_id) {
                Some(stored) if request.mode.is_stronger_than(stored.record.request.mode) => {
                    stored.intent_revision = crate::StoreError::checked_monotonic_increment(
                        "turn_cancel_intent_revision",
                        stored.intent_revision,
                    )?;
                }
                Some(_) => {}
                None => {
                    requests.insert(
                        request.address.turn_id.clone(),
                        InMemoryTurnCancelRequest {
                            record: crate::TurnCancelRequestRecord {
                                request,
                                outcome: None,
                            },
                            intent_revision: 1,
                        },
                    );
                }
            }
        }
        let session_meta_before_commit = self.session_meta.lock_recover().clone();
        planner.validate_node_derivation()?;
        let key = (session_id.clone(), planner.operation_key().to_string());
        if let Some(stored) = self.runtime_turn_commits.lock_recover().get(&key).cloned() {
            let prior = crate::store::RuntimeCommitReceiptRecord {
                turn_commit_hash: stored.turn_commit_hash,
                result: stored.result,
                append_request_identity: stored.append_request_identity,
            };
            #[expect(
                clippy::expect_used,
                reason = "a prior receipt yields a replay or an error"
            )]
            let replay = planner
                .decide_receipt(Some(prior))?
                .expect("an existing receipt must produce replay or an error");
            if let Some(completion) = replay.release_session_execution_lease() {
                let _release_was_current =
                    self.release_session_execution_lease_in_memory(completion, false);
                // FIG-884: ancillary stale release must never veto a replayed commit.
            }
            turn_cancel_closure::consume(self, commit);
            return Ok(replay.into_result());
        }
        turn_cancel_closure::validate_after_receipt_miss(self, commit)?;
        if let (Some(turn_id), Some(observed)) = (
            commit.interrupted_turn_input_turn_id.as_ref(),
            commit.interrupted_turn_cancel_intent.as_ref(),
        ) {
            let requests = self.turn_cancel_requests.lock_recover();
            if turn_input::snapshot(&requests, turn_id) != *observed {
                return Err(crate::StoreError::TurnCancelIntentChanged {
                    session_id: commit.session_id.clone(),
                    turn_id: turn_id.clone(),
                });
            }
        }
        // Receipt replay and the cancellation predicate are adjudicated before
        // even session binding metadata can be materialized by a fresh commit.
        self.ensure_session_metadata_for_commit(commit)?;
        let mut meta = self.session_head_meta.lock_recover();
        let actual = meta.as_ref().map_or(0, |meta| meta.head_revision);
        #[cfg(any(test, feature = "testing"))]
        self.fail_after_first_runtime_commit_mutation_if_requested(
            session_meta_before_commit.clone(),
        )?;
        let hydrated_checkpoint =
            checkpoints::resolve_components(&self.checkpoint_component_blobs, &commit.checkpoint)?;
        let incoming_nodes = commit.graph.nodes();
        let mut global_node_owners = self.global_node_owners.lock_recover();
        let graph = self.global_session_graph.lock_recover();
        let tombstoned = self.tombstoned_node_ids.lock_recover();
        let occupied_node_ids = incoming_nodes
            .iter()
            .filter(|node| {
                global_node_owners.contains_key(&node.node_id)
                    || graph.find_node(&node.node_id).is_some()
                    || tombstoned.contains(&node.node_id)
            })
            .map(|node| node.node_id.clone())
            .collect();
        drop(graph);
        drop(tombstoned);
        let published_leaf = {
            let graph = self.global_session_graph.lock_recover();
            let tombstoned = self.tombstoned_node_ids.lock_recover();
            match meta.as_ref().and_then(|head| head.leaf_node_id.as_ref()) {
                None => crate::store::PublishedLeafFacts::Absent,
                Some(leaf_node_id)
                    if tombstoned.contains(leaf_node_id)
                        || graph.find_node(leaf_node_id).is_none() =>
                {
                    crate::store::PublishedLeafFacts::Retired {
                        node_id: leaf_node_id.clone(),
                    }
                }
                Some(leaf_node_id) => {
                    let resident = self.session_graph.lock_recover();
                    let active_path = resident.active_path_nodes();
                    let generation = active_path.len().checked_sub(1).ok_or_else(|| {
                        crate::StoreError::StoredDataCorrupt {
                            record_kind: "SessionGraph",
                            message: "published leaf has an empty active path".to_string(),
                        }
                    })? as u64;
                    let frame_node_id = resident
                        .nearest_frame_node_id(Some(leaf_node_id))
                        .map(ToOwned::to_owned)
                        .ok_or_else(|| crate::StoreError::MissingFrameOpenAncestor {
                            leaf_node_id: crate::NodeId::from(leaf_node_id),
                        })?;
                    crate::store::PublishedLeafFacts::Live(crate::store::ParentNodeFacts {
                        node_id: crate::NodeId::from(leaf_node_id),
                        generation,
                        frame_node_id,
                    })
                }
            }
        };
        let requested_ancestor_is_active = match &commit.turn_commit.append_request_identity {
            crate::AppendRequestIdentity::Append {
                requested_ancestor_node_id: Some(required),
                ..
            } => self
                .session_graph
                .lock_recover()
                .active_path_contains(required),
            crate::AppendRequestIdentity::PlainCommit
            | crate::AppendRequestIdentity::Append {
                requested_ancestor_node_id: None,
                ..
            }
            | crate::AppendRequestIdentity::SemanticBoundary { .. } => true,
        };
        let plan = planner.plan(crate::store::FreshRuntimeCommitFacts {
            actual_head_revision: actual,
            published_leaf,
            requested_ancestor_is_active,
            occupied_node_ids,
        })?;
        let mut proposed = self.global_session_graph.lock_recover().clone();
        proposed.apply_append(&commit.graph)?;
        if let crate::store::GraphAppend::PreserveHead = &commit.graph {
            // A preserve-head commit still republishes the committing
            // session's leaf as the global leaf; `graph_base_leaf_node_id`
            // carries the resident head fact the removed empty-append
            // `leaf_node_id` field used to.
            match &commit.graph_base_leaf_node_id {
                Some(leaf_node_id) if proposed.find_node(leaf_node_id.as_str()).is_none() => {
                    return Err(crate::StoreError::InvalidGraphLeaf {
                        leaf_node_id: commit.graph_base_leaf_node_id.clone(),
                    });
                }
                leaf_node_id => proposed.data_mut().leaf_node_id = leaf_node_id.clone(),
            }
        }
        let (staged_tombstoned_node_ids, staged_session_heads) = {
            let new_leaf_node_id = commit
                .graph
                .leaf_node_id()
                .or(commit.graph_base_leaf_node_id.as_ref())
                .cloned();
            let mut tombstoned = self.tombstoned_node_ids.lock_recover().clone();
            let mut session_heads = self.global_session_heads.lock_recover().clone();
            session_heads.insert(
                SessionId::from(commit.session_id.clone().to_string()),
                new_leaf_node_id.clone(),
            );
            let anchored_node_ids = self
                .node_anchors
                .lock_recover()
                .keys()
                .cloned()
                .collect::<HashSet<_>>();
            let mut live_child_counts = Self::live_child_counts(&proposed, &tombstoned);
            if plan.head_changed()
                && let Some(old_leaf_node_id) = plan.old_leaf_node_id()
            {
                Self::reclaim_unreachable_ancestry(
                    &proposed,
                    &mut live_child_counts,
                    &mut tombstoned,
                    old_leaf_node_id,
                    &session_heads,
                    &anchored_node_ids,
                );
            }
            (tombstoned, session_heads)
        };
        {
            let queued = self.queued_work.lock_recover();
            for completed in &commit.completed_queue_claims {
                if let Some((row_id, current)) = turn_input::settlement_mismatch(
                    &queued,
                    &completed.batch_ids,
                    &completed.session_id,
                    |entry| {
                        (
                            entry.batch.session_id.as_str(),
                            entry.batch.batch_id.as_str(),
                        )
                    },
                    |entry| {
                        entry.batch.session_id == completed.session_id
                            && entry
                                .claim
                                .owned_by(&completed.claim_id, &completed.lease_token)
                            && completed.batch_ids.contains(&entry.batch.batch_id)
                    },
                ) {
                    return Err(crate::store::StoreError::QueuedWorkClaimSuperseded {
                        session_id: completed.session_id.clone(),
                        claim_id: completed.claim_id.clone(),
                        row_id: row_id.map(|id| id.as_str().to_string().into_boxed_str()),
                        superseding_claim_id: current
                            .and_then(|entry| entry.claim.id())
                            .map(String::into_boxed_str),
                        superseding_session_lease_generation: current
                            .and_then(|entry| entry.claim.diagnostic_generation().map(Box::new)),
                    });
                }
            }
        }
        {
            let pending = self.pending_turn_inputs.lock_recover();
            for completed in &commit.completed_turn_input_claims {
                if let Some((row_id, current)) = turn_input::settlement_mismatch(
                    &pending,
                    &completed.input_ids,
                    &completed.session_id,
                    |entry| {
                        (
                            entry.input.session_id.as_str(),
                            entry.input.input_id.as_str(),
                        )
                    },
                    |entry| turn_input::settlement_matches(entry, completed),
                ) {
                    return Err(match completed.claim.as_ref() {
                        Some(claim) => crate::store::StoreError::TurnInputClaimSuperseded {
                            session_id: completed.session_id.clone(),
                            claim_id: claim.claim_id.clone(),
                            row_id: row_id.map(|id| id.as_str().to_string().into_boxed_str()),
                            superseding_claim_id: current
                                .and_then(|entry| entry.claim.id())
                                .map(String::into_boxed_str),
                            superseding_session_lease_generation: current.and_then(|entry| {
                                entry.claim.diagnostic_generation().map(Box::new)
                            }),
                        },
                        None => crate::store::StoreError::UnclaimedTurnInputSettlementSuperseded {
                            session_id: completed.session_id.clone(),
                            input_id: row_id.cloned().unwrap_or_else(|| {
                                crate::InputId::new(
                                    completed
                                        .input_ids
                                        .iter()
                                        .map(crate::InputId::as_str)
                                        .collect::<Vec<_>>()
                                        .join(","),
                                )
                            }),
                            observed_state: current.map(|entry| {
                                entry.input.state.as_str().to_string().into_boxed_str()
                            }),
                            superseding_claim_id: current
                                .and_then(|entry| entry.claim.id())
                                .map(String::into_boxed_str),
                        },
                    });
                }
            }
        }
        let manifest = hydrated_checkpoint.manifest()?;
        let checkpoint_bytes = rmp_serde::to_vec_named(&manifest).map_err(|error| {
            crate::store::StoreError::RecordEncodingFailed {
                record_kind: "in-memory checkpoint root".to_string(),
                message: error.to_string(),
            }
        })?;
        let checkpoint_ref = crate::BlobRef::for_content(&checkpoint_bytes);
        let (
            staged_queued_work,
            staged_wake_redelivery_fences,
            staged_queued_work_next_seq,
            staged_enqueued_queue_batches,
        ) = {
            let mut queued = self.queued_work.lock_recover().clone();
            let mut fences = self.wake_redelivery_fences.lock_recover().clone();
            let mut next_seq = *self.queued_work_next_seq.lock_recover();
            for completed in &commit.completed_queue_claims {
                for entry in queued.iter().filter(|entry| {
                    entry.batch.session_id == completed.session_id
                        && entry
                            .claim
                            .owned_by(&completed.claim_id, &completed.lease_token)
                        && completed.batch_ids.contains(&entry.batch.batch_id)
                }) {
                    if let Some((process_id, sequence)) =
                        entry
                            .batch
                            .items
                            .iter()
                            .find_map(|item| match &item.payload {
                                crate::QueuedWorkPayload::ProcessWake { wake } => {
                                    Some((wake.process_id.clone(), wake.sequence))
                                }
                                _ => None,
                            })
                    {
                        fences
                            .entry((
                                entry.batch.session_id.clone().to_string(),
                                process_id.to_string(),
                            ))
                            .and_modify(|allocation_floor| {
                                *allocation_floor = (*allocation_floor).max(sequence);
                            })
                            .or_insert(sequence);
                    }
                }
                queued.retain(|entry| {
                    !(entry.batch.session_id == completed.session_id
                        && entry
                            .claim
                            .owned_by(&completed.claim_id, &completed.lease_token)
                        && completed.batch_ids.contains(&entry.batch.batch_id))
                });
            }
            let enqueued = commit
                .enqueued_queue_batches
                .iter()
                .cloned()
                .map(|batch| {
                    Self::enqueue_queued_work_for_state(
                        &mut queued,
                        &fences,
                        &mut next_seq,
                        batch,
                        transaction_now,
                    )
                    .map(crate::QueuedWorkEnqueueOutcome::into_batch)
                })
                .collect::<Result<Vec<_>, _>>()?;
            (queued, fences, next_seq, enqueued)
        };
        let (staged_pending_turn_inputs, staged_turn_cancel_requests, turn_cancel_input_outcome) = {
            let mut pending = self.pending_turn_inputs.lock_recover().clone();
            let mut requests = self.turn_cancel_requests.lock_recover().clone();
            let mut outcome = crate::TurnCancelInputOutcome::default();
            for completed in &commit.completed_turn_input_claims {
                for entry in pending.iter_mut() {
                    if turn_input::settlement_matches(entry, completed) {
                        entry.input.state =
                            crate::TurnInputState::Completed(entry.input.state.ingress());
                        entry.clear_claim();
                    }
                }
            }
            if let Some(turn_id) = commit.interrupted_turn_input_turn_id.as_deref() {
                let cancellation = commit.interrupted_turn_input_cancellation.as_ref();
                let disposition = cancellation
                    .map_or(crate::TurnCancelDisposition::Defer, |evidence| {
                        evidence.undelivered
                    });
                if let Some(evidence) = commit
                    .turn_cancel_closure_settlement
                    .as_ref()
                    .and_then(crate::TurnCancelClosureSettlement::base_cancellation)
                {
                    turn_input::reconcile_authenticated_turn_cancel_winner(
                        &mut requests,
                        &crate::TurnAddress::new(&commit.session_id, turn_id),
                        evidence,
                    )?;
                }
                for entry in pending.iter_mut() {
                    if entry.input.session_id == commit.session_id
                        && matches!(
                            &entry.input.state,
                            crate::TurnInputState::PendingActive(scope)
                                if scope.turn_id.as_str() == turn_id
                        )
                    {
                        let affected = crate::TurnCancelAffectedInput {
                            input_id: entry.input.input_id.clone(),
                            payload: entry.input.input.clone(),
                            disposition,
                        };
                        match disposition {
                            crate::TurnCancelDisposition::Defer => {
                                entry.input.state = crate::TurnInputState::DeferredNextTurn;
                            }
                            crate::TurnCancelDisposition::Drop => {
                                entry.input.state =
                                    crate::TurnInputState::Cancelled(entry.input.state.ingress());
                            }
                        }
                        entry.claim.release();
                        if cancellation.is_some()
                            && let Some(record) = requests.get_mut(turn_id)
                        {
                            record
                                .record
                                .outcome
                                .get_or_insert_with(crate::TurnCancelInputOutcome::default)
                                .affected_inputs
                                .push(affected.clone());
                        }
                        if cancellation.is_some() {
                            outcome.affected_inputs.push(affected);
                        }
                    }
                }
            }
            (pending, requests, outcome)
        };

        // Refuse an armed attachment delete before publishing staged boundary
        // state. The same factory transaction excludes attachment GC.
        self.commit_attachment_refs_in_memory(
            &commit.session_id,
            &commit.committed_attachment_ids,
            transaction_now,
        )?;
        *self.queued_work.lock_recover() = staged_queued_work;
        *self.wake_redelivery_fences.lock_recover() = staged_wake_redelivery_fences;
        *self.queued_work_next_seq.lock_recover() = staged_queued_work_next_seq;
        *self.pending_turn_inputs.lock_recover() = staged_pending_turn_inputs;
        *self.turn_cancel_requests.lock_recover() = staged_turn_cancel_requests;
        let resident_graph = proposed.trim_to_active_path();
        let mut global_graph = self.global_session_graph.lock_recover();
        *global_graph = proposed;
        *self.session_graph.lock_recover() = resident_graph;
        drop(global_graph);
        *self.tombstoned_node_ids.lock_recover() = staged_tombstoned_node_ids;
        *self.global_session_heads.lock_recover() = staged_session_heads;
        for node in incoming_nodes {
            global_node_owners.insert(
                node.node_id.clone(),
                SessionId::from(commit.session_id.clone().to_string()),
            );
        }
        drop(global_node_owners);
        {
            let mut usage_deltas = self.usage_deltas.lock_recover();
            for delta in &commit.usage_deltas {
                if !usage_deltas
                    .iter()
                    .any(|stored| stored.identity == delta.identity)
                {
                    usage_deltas.push(delta.clone());
                }
            }
        }
        // The write-transaction mutex still covers both this leaf publication
        // and the checkpoint-root replacement below. That is the in-memory
        // GC-safety equivalent of avoiding git's loose-object race: readers
        // can observe neither unreachable new leaves nor a root with missing
        // leaves.
        {
            let mut blobs = self.checkpoint_component_blobs.lock_recover();
            let mut roots = HashSet::new();
            for component in hydrated_checkpoint.components.values() {
                if let Some(blob_ref) = component.blob_ref().cloned() {
                    if let Some(body) = component.body_arc() {
                        blobs.insert(blob_ref.clone(), body);
                    }
                    roots.insert(blob_ref);
                }
            }
            // This commit's checkpoint is the session's only live one, so its
            // component edges replace the superseded set wholesale.
            self.checkpoint_blob_roots
                .lock_recover()
                .insert(commit.session_id.clone().clone(), roots);
        }
        *self.checkpoint.lock_recover() = Some(hydrated_checkpoint);
        self.commit_turn_attachment_intents(
            &commit.session_id,
            &commit.turn_commit,
            transaction_now,
        );
        *meta = Some(plan.head_meta(checkpoint_ref.clone()));
        #[expect(
            clippy::expect_used,
            reason = "the head metadata is assigned on the line above"
        )]
        let head_revision = meta
            .as_ref()
            .expect("fresh commit publishes session head metadata")
            .head_revision;
        let durable_relation = session_meta_before_commit.map(|meta| meta.relation);
        self.session_catalog
            .lock_recover()
            .entry(SessionId::from(session_id.clone().to_string()))
            .and_modify(|summary| {
                summary.last_commit_at_ms = Some(transaction_now);
                summary.head_revision = head_revision;
                summary.durable_relation = durable_relation.clone();
            })
            .or_insert_with(|| crate::SessionSummary {
                session_id: session_id.clone(),
                created_at_ms: transaction_now,
                last_commit_at_ms: Some(transaction_now),
                head_revision,
                relation: durable_relation
                    .as_ref()
                    .map(crate::SessionRelationKind::from_relation)
                    .unwrap_or(crate::SessionRelationKind::Root),
                durable_relation: durable_relation.clone(),
                parent_session_id: durable_relation
                    .as_ref()
                    .and_then(crate::SessionRelation::parent_session_id)
                    .map(ToOwned::to_owned)
                    .map(Into::into),
                deleted: false,
            });
        *self.runtime_commit_count.lock_recover() += 1;
        let mut result = plan.result(checkpoint_ref, manifest, staged_enqueued_queue_batches);
        result.turn_cancel_input_outcome = turn_cancel_input_outcome;
        let receipt = plan.receipt_write(&result);
        let stored_receipt = RuntimeTurnCommitRecord {
            turn_commit_hash: receipt.turn_commit_hash.to_string(),
            result: result.clone(),
            committed_at_ms: transaction_now,
            append_request_identity: receipt.append_request_identity.clone(),
        };
        let mut runtime_turn_commits = self.runtime_turn_commits.lock_recover();
        runtime_turn_commits.insert(
            (
                session_id.clone().clone(),
                receipt.operation_key.to_string(),
            ),
            stored_receipt.clone(),
        );
        if commit.turn_commit.operation.key == "session-command" {
            for batch_id in commit
                .completed_queue_claims
                .iter()
                .flat_map(|completion| &completion.batch_ids)
            {
                let marker = crate::store_backend_support::session_command_batch_completion_key(
                    &session_id,
                    batch_id,
                )?;
                runtime_turn_commits.insert(
                    (session_id.clone().clone(), marker),
                    RuntimeTurnCommitRecord {
                        append_request_identity: crate::AppendRequestIdentity::PlainCommit,
                        ..stored_receipt.clone()
                    },
                );
            }
        }
        drop(runtime_turn_commits);
        turn_cancel_closure::consume(self, commit);
        if let Some(completion) = commit.release_session_execution_lease.as_ref() {
            let _release_was_current =
                self.release_session_execution_lease_in_memory(completion, false);
            // FIG-884: head CAS is commit authority; release is ancillary.
        }
        Ok(result)
    }

    async fn admit_and_bind_session(
        &self,
        binding: &crate::SessionBinding,
    ) -> Result<crate::SessionAdmission, crate::StoreError> {
        self.admit_and_bind_session_in_memory(binding)
    }

    async fn save_session_meta(
        &self,
        meta: crate::store::SessionMeta,
    ) -> Result<(), crate::store::StoreError> {
        let _transaction = self.write_transaction.lock_recover();
        self.replace_session_meta(meta.clone())?;
        if let Some(summary) = self
            .session_catalog
            .lock_recover()
            .get_mut(&meta.session_id)
        {
            summary.relation = crate::SessionRelationKind::from_relation(&meta.relation);
            summary.durable_relation = Some(meta.relation.clone());
            summary.parent_session_id = meta
                .relation
                .parent_session_id()
                .map(ToOwned::to_owned)
                .map(Into::into);
        }
        Ok(())
    }

    async fn load_session_meta(
        &self,
    ) -> Result<Option<crate::store::SessionMeta>, crate::store::StoreError> {
        Ok(self.session_meta.lock_recover().clone())
    }
}
