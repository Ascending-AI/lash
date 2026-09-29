use crate::admin::SessionConfigPatch;
#[cfg(feature = "rlm")]
use crate::support::SessionSpec;
use crate::support::SessionWorkEngine;
use crate::support::{
    Arc, CancellationToken, EmbedError, LashCore, PluginFactory, ProcessRegistry,
    PromptContribution, PromptLayerSink, PromptSlot, PromptTemplate, ProviderHandle, Result,
    RunActivityCollector, RuntimeSessionState, SessionError, SessionObservationSubscription,
    SessionResume, SessionStoreFactory, StaticPluginFactory, StdMutex, ToolProvider, TurnActivity,
    TurnActivityId, TurnActivitySink, TurnEvent, TurnInput, TurnOutcome, TurnReport, async_trait,
    message_text,
};
use lash_core::ProcessExecutionEnvStore;
use lash_core::facade_support::{
    AgentFrameReasonFacadeOps, RuntimeSessionStateFacadeOps, SessionGraphFacadeOps,
    SessionNodeProjection, ToolStateFacadeOps,
};
use lash_core::{ProcessLifecycle as _, ProcessRegistrar as _};
use lash_sansio::SessionId;
use lash_sansio::sync::MutexExt;
use std::collections::BTreeMap;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{
    LlmContentBlock, LlmRequest, LlmResponse, LlmRole, LlmStreamEvent, ResponseTextMeta,
};
use lash_core::{LlmOutputPart, SessionProcessEventKind, StoreError, ToolDefinitionBindingExt};
use tokio::sync::{Mutex as TokioMutex, oneshot};

fn now_epoch_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before epoch")
        .as_millis() as u64
}

/// Create a session's durable metadata without building a runtime.
///
/// A Durable Session never creates (ADR 0119), so a test that enqueues to a
/// session it has not opened creates it first through the facade's third
/// terminal verb — the same move an in-repo host that relied on
/// enqueue-materialisation now makes.
pub(crate) async fn create_catalog_session(core: &LashCore, session_id: &str) -> Result<()> {
    core.session(session_id).create().await?;
    Ok(())
}

#[derive(Default)]
struct SnapshotStore {
    /// The drive epoch each session drive's seal raises (FIG-3600).
    drive_epochs: lash_core::store::InMemoryDriveEpochs,
    /// Logical roots' terminal evidence and input bindings (FIG-3600 S7).
    roots: lash_core::store::InMemoryRootLedger,
    /// Staged turn capture and sealed stopped partials (ADR 0114).
    captures: lash_core::store::InMemoryTurnCapture,
    root_claim_results:
        std::sync::Mutex<HashMap<(SessionId, lash_core::TurnId), lash_core::store::RootAdmission>>,
    read: std::sync::Mutex<Option<lash_core::store::PersistedSessionRead>>,
    session_meta: std::sync::Mutex<Option<lash_core::SessionMeta>>,
    runtime_turn_commits: std::sync::Mutex<
        std::collections::HashMap<
            (SessionId, String),
            (String, lash_core::store::RuntimeCommitReceipt),
        >,
    >,
    usage_delta_identities:
        std::sync::Mutex<std::collections::HashSet<lash_core::store::RuntimeUsageDeltaIdentity>>,
    /// Accepted-but-unsettled turn inputs, in enqueue order.
    ///
    /// Every turn — direct or queued — is admitted here before it is driven
    /// (ADR 0069), so this double owes the pending lifecycle even though its
    /// tests never enqueue input of their own.
    pending_turn_inputs: std::sync::Mutex<Vec<lash_core::PendingTurnInput>>,
    pending_turn_input_seq: std::sync::Mutex<u64>,
    /// The root each admitted pending input is bound to (FIG-3927). A row is
    /// open until a root admits it and stays listed until its root's commit
    /// settles or releases it.
    admitted_inputs: std::sync::Mutex<HashMap<lash_core::InputId, lash_core::TurnId>>,
}

impl SnapshotStore {
    fn with_state(state: RuntimeSessionState) -> Self {
        let config = lash_core::PersistedSessionConfig::from(&state.policy);
        Self::with_state_and_config(state, config)
    }

    fn with_state_and_config(
        state: RuntimeSessionState,
        config: lash_core::PersistedSessionConfig,
    ) -> Self {
        let turn_state = state.turn_state();
        let session_meta = lash_core::SessionMeta {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: state.session_id.clone(),
            relation: lash_core::SessionRelation::Root,
        };
        let mut components = std::collections::BTreeMap::new();
        if let Some(tool_state) = state.tool_state_snapshot() {
            components.insert(
                lash_core::store::TOOL_STATE_CHECKPOINT_COMPONENT.to_string(),
                lash_core::HydratedCheckpointComponent::changed(
                    rmp_serde::to_vec_named(tool_state).expect("encode test tool state"),
                ),
            );
        }
        if let Some(execution_state) = state.execution_state_snapshot() {
            components.insert(
                lash_core::store::EXECUTION_STATE_CHECKPOINT_COMPONENT.to_string(),
                lash_core::HydratedCheckpointComponent::changed(execution_state.to_vec()),
            );
        }
        Self {
            drive_epochs: Default::default(),
            roots: Default::default(),
            captures: Default::default(),
            root_claim_results: Default::default(),
            read: std::sync::Mutex::new(Some(lash_core::store::PersistedSessionRead {
                session_id: state.session_id,
                head_revision: 7,
                config,
                current_frame_node_id: state.current_frame_node_id,
                pending_follow_on: None,
                graph: state.session_graph,
                checkpoint_ref: None,
                checkpoint: Some(lash_core::store::HydratedSessionCheckpoint {
                    turn_state,
                    components,
                }),
                token_ledger: Vec::new(),
                turn_failure_settlements: Vec::new(),
                turn_commits: Vec::new(),
            })),
            session_meta: std::sync::Mutex::new(Some(session_meta)),
            runtime_turn_commits: std::sync::Mutex::new(std::collections::HashMap::new()),
            usage_delta_identities: std::sync::Mutex::new(std::collections::HashSet::new()),
            pending_turn_inputs: std::sync::Mutex::new(Vec::new()),
            pending_turn_input_seq: std::sync::Mutex::new(0),
            admitted_inputs: std::sync::Mutex::new(HashMap::new()),
        }
    }

    /// Settle the rows `commit` names under its root, and release every row
    /// still bound to the root its terminal ends (FIG-3927).
    fn settle_ingress(&self, commit: &lash_core::store::RuntimeCommit) {
        let mut pending = self.pending_turn_inputs.lock_recover();
        let mut admitted = self.admitted_inputs.lock_recover();
        if let Some(ingress) = commit.ingress.as_ref() {
            let settled = ingress
                .completed_inputs
                .iter()
                .flat_map(|completion| completion.input_ids.iter())
                .chain(ingress.dropped.iter().filter_map(|row| match row {
                    lash_core::store::IngressRowId::Input(input_id) => Some(input_id),
                    lash_core::store::IngressRowId::Batch(_) => None,
                }))
                .cloned()
                .collect::<Vec<_>>();
            pending.retain(|input| !settled.contains(&input.input_id));
            for input_id in &settled {
                admitted.remove(input_id);
            }
            for row in &ingress.released {
                if let lash_core::store::IngressRowId::Input(input_id) = row {
                    admitted.remove(input_id);
                }
            }
        }
        if let Some(terminal) = commit.root_terminal.as_deref() {
            admitted.retain(|_, root| *root != terminal.root);
        }
    }

    fn set_head_provider_id(&self, provider_id: impl Into<String>) {
        let mut read = self.read.lock_recover();
        let Some(read) = read.as_mut() else {
            panic!("snapshot store has no session head");
        };
        let provider_id = provider_id.into();
        read.config.provider_id = provider_id.clone();
        let leaf_node_id = read.graph.leaf_node_id.clone();
        let mut nodes = read.graph.nodes.clone();
        for node in &mut nodes {
            if let lash_core::SessionNodePayload::FrameOpen { assignment, .. } =
                &mut std::sync::Arc::make_mut(node).payload
            {
                assignment.policy.provider_id = provider_id.clone();
            }
        }
        read.graph = lash_core::SessionGraph::from_shared_nodes(nodes, leaf_node_id)
            .expect("snapshot fixture graph is valid");
        read.head_revision += 1;
    }
}

lash_core::impl_noop_attachment_manifest!(SnapshotStore);

lash_core::impl_current_fleet_format!(SnapshotStore);

#[async_trait]
impl lash_core::SessionCommitStore for SnapshotStore {
    async fn raise_pending_follow_on_attempts(
        &self,
        fence: &lash_core::store::DriveFence,
        follow_on_turn_id: &lash_core::TurnId,
    ) -> std::result::Result<lash_core::store::PendingFollowOn, lash_core::store::StoreError> {
        Err(lash_core::store::StoreError::FollowOnNotPending {
            session_id: fence.session().clone(),
            follow_on_turn_id: follow_on_turn_id.clone(),
        })
    }

    async fn admit_and_bind_session(
        &self,
        binding: &lash_core::SessionBinding,
    ) -> std::result::Result<lash_core::SessionAdmission, lash_core::store::StoreError> {
        binding.validate()?;
        let mut meta = self.session_meta.lock_recover();
        if let Some(meta) = meta.as_ref() {
            if meta.session_id != binding.session_id {
                return Err(lash_core::store::StoreError::SessionBindingMismatch {
                    bound_session_id: meta.session_id.clone(),
                    attempted_session_id: binding.session_id.clone(),
                });
            }
            lash_core::store_backend_support::guard_rebind_lineage(
                &binding.session_id,
                &lash_core::SessionLineage::of(&meta.relation),
                &binding.relation,
            )?;
            return Ok(lash_core::SessionAdmission::Rebound);
        }
        *meta = Some(lash_core::SessionMeta {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: binding.session_id.clone(),
            relation: binding.relation.clone(),
        });
        Ok(lash_core::SessionAdmission::Created)
    }

    async fn load_session(
        &self,
    ) -> std::result::Result<
        Option<lash_core::store::PersistedSessionRead>,
        lash_core::store::StoreError,
    > {
        Ok(self.read.lock_recover().clone())
    }

    async fn load_session_head_meta(
        &self,
    ) -> std::result::Result<Option<lash_core::store::SessionHeadMeta>, lash_core::store::StoreError>
    {
        self.read
            .lock_recover()
            .as_ref()
            .map(|read| {
                lash_core::store::SessionHeadMeta::assemble(
                    &read.session_id,
                    lash_core::store::SessionHeadPayload {
                        schema_version: lash_core::store::SESSION_HEAD_META_SCHEMA_VERSION,
                        session_id: read.session_id.clone(),
                        config: read.config.clone(),
                        current_frame_node_id: read.current_frame_node_id.clone(),
                    },
                    read.head_revision,
                    read.checkpoint_ref.clone(),
                    read.graph.leaf_node_id.clone(),
                )
            })
            .transpose()
    }

    async fn load_node(
        &self,
        _node_id: &str,
    ) -> std::result::Result<Option<lash_core::SessionNodeRecord>, lash_core::store::StoreError>
    {
        Ok(None)
    }

    async fn commit_runtime_state(
        &self,
        commit: lash_core::store::RuntimeCommit,
    ) -> std::result::Result<lash_core::store::RuntimeCommitReceipt, lash_core::store::StoreError>
    {
        let turn_commit_hash = commit.turn_commit_hash()?;
        let session_id = commit.session_id.clone();
        let mut read = self.read.lock_recover();
        let realized_node_timestamps = commit
            .graph
            .appended_nodes()
            .map(|node| lash_core::session_graph::RealizedNodeTimestamp {
                node_id: node.node_id.clone(),
                timestamp: node.timestamp.clone(),
            })
            .collect();
        let completed = &commit.turn_commit;
        let operation_key = completed.operation.storage_key()?;
        let key = (session_id.clone(), operation_key.clone());
        if let Some((stored_hash, result)) =
            self.runtime_turn_commits.lock_recover().get(&key).cloned()
        {
            if stored_hash == turn_commit_hash {
                // Contract (`SessionCommitStore::commit_runtime_state`): a
                // replay returns the stored first-attempt receipt with only
                // `receipt_replayed` set transiently. The verified boundary's
                // revision-advance assertion exempts exactly this bit.
                let mut result = result;
                result.receipt_replayed = true;
                return Ok(result);
            }
            return Err(lash_core::store::StoreError::RuntimeTurnCommitConflict {
                session_id,
                operation_key,
            });
        }
        if let lash_core::AppendRequestIdentity::Append {
            requested_ancestor_node_id: Some(required_node_id),
            ..
        } = &commit.turn_commit.append_request_identity
            && !read
                .as_ref()
                .is_some_and(|read| read.graph.active_path_contains(required_node_id))
        {
            return Err(lash_core::store::StoreError::AppendAncestorNotActive {
                required_node_id: required_node_id.to_string().into(),
            });
        }
        {
            let mut session_meta = self.session_meta.lock_recover();
            if session_meta.is_none() {
                *session_meta = Some(lash_core::SessionMeta {
                    owning_process_id: None,
                    pending_observer_intents: Vec::new(),
                    session_id: commit.session_id.clone(),
                    relation: lash_core::SessionRelation::Root,
                });
            }
        }
        let existing_graph = read
            .as_ref()
            .map(|read| read.graph.clone())
            .unwrap_or_default();
        let mut graph = existing_graph;
        graph.apply_append(&commit.graph)?;
        let mut token_ledger = read
            .as_ref()
            .map(|read| read.token_ledger.clone())
            .unwrap_or_default();
        let mut usage_delta_identities = self.usage_delta_identities.lock_recover();
        for delta in &commit.usage_deltas {
            if usage_delta_identities.insert(delta.identity.clone()) {
                if let Some(existing) = token_ledger.iter_mut().find(|entry| {
                    entry.source == delta.entry.source && entry.model == delta.entry.model
                }) {
                    existing.usage.input_tokens = existing
                        .usage
                        .input_tokens
                        .saturating_add(delta.entry.usage.input_tokens);
                    existing.usage.output_tokens = existing
                        .usage
                        .output_tokens
                        .saturating_add(delta.entry.usage.output_tokens);
                    existing.usage.cache_read_input_tokens = existing
                        .usage
                        .cache_read_input_tokens
                        .saturating_add(delta.entry.usage.cache_read_input_tokens);
                    existing.usage.cache_write_input_tokens = existing
                        .usage
                        .cache_write_input_tokens
                        .saturating_add(delta.entry.usage.cache_write_input_tokens);
                    existing.usage.reasoning_output_tokens = existing
                        .usage
                        .reasoning_output_tokens
                        .saturating_add(delta.entry.usage.reasoning_output_tokens);
                } else {
                    token_ledger.push(delta.entry.clone());
                }
            }
        }
        drop(usage_delta_identities);
        // Contract: every fresh commit (including the first turn after a
        // session reopen) must advance the durable head revision; only receipt
        // replay may return a non-advancing receipt.
        let next_head_revision = read.as_ref().map_or(0, |read| read.head_revision) + 1;
        self.captures.commit(&commit)?;
        self.settle_ingress(&commit);
        if let Some(write) = commit.root_terminal.as_deref().cloned() {
            self.roots.write_terminal(write.into_terminal(
                commit.session_id.clone(),
                next_head_revision,
                0,
            ))?;
        }
        *read = Some(lash_core::store::PersistedSessionRead {
            session_id: commit.session_id.clone(),
            head_revision: next_head_revision,
            config: commit.config,
            current_frame_node_id: commit.current_frame_node_id,
            pending_follow_on: None,
            graph,
            checkpoint_ref: Some(lash_core::BlobRef("checkpoint".to_string())),
            checkpoint: Some(commit.checkpoint),
            token_ledger,
            turn_failure_settlements: Vec::new(),
            turn_commits: Vec::new(),
        });
        let result = lash_core::store::RuntimeCommitReceipt {
            schema_version: lash_core::store::RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION,
            head_revision: next_head_revision,
            checkpoint_ref: lash_core::BlobRef("checkpoint".to_string()),
            manifest: lash_core::store::SessionCheckpoint::default(),
            committed_leaf_node_id: commit.graph.leaf_node_id().cloned(),
            realized_node_timestamps,
            committed_usage_delta_identities: commit
                .usage_deltas
                .iter()
                .map(|delta| delta.identity.clone())
                .collect(),
            failure_evidence: commit.failure_evidence.clone(),
            outcome: commit.outcome.clone(),
            pending_follow_on: None,
            turn_input_applications: Vec::new(),
            turn_cancel_input_outcome: Default::default(),
            receipt_replayed: false,
        };
        self.runtime_turn_commits.lock_recover().insert(
            (session_id, completed.operation.storage_key()?),
            (turn_commit_hash, result.clone()),
        );
        Ok(result)
    }

    async fn save_session_meta(
        &self,
        meta: lash_core::SessionMeta,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        *self.session_meta.lock_recover() = Some(meta);
        Ok(())
    }

    async fn load_session_meta(
        &self,
    ) -> std::result::Result<Option<lash_core::SessionMeta>, lash_core::store::StoreError> {
        Ok(self.session_meta.lock_recover().clone())
    }
}

#[async_trait]
impl lash_core::store::DriveEpochStore for SnapshotStore {
    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &lash_core::store::AdmissionId,
        observed_epoch: u64,
        root_start: &lash_core::store::RootStartNonce,
    ) -> std::result::Result<lash_core::store::DriveEpochSeal, lash_core::StoreError> {
        Ok(self
            .drive_epochs
            .seal(session_id, admission, observed_epoch, root_start))
    }

    async fn drive_epoch(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<lash_core::store::StoredDriveEpoch, lash_core::StoreError> {
        Ok(self.drive_epochs.epoch(session_id))
    }
}

#[async_trait]
impl lash_core::StoreMaintenance for SnapshotStore {
    async fn vacuum(&self) -> lash_core::MaintenanceResult<lash_core::VacuumReport> {
        Ok(lash_core::VacuumReport::default())
    }

    async fn gc_unreachable(&self) -> lash_core::MaintenanceResult<lash_core::GcReport> {
        Ok(lash_core::GcReport::default())
    }
}

#[derive(Clone)]
struct ReusableStoreFactory {
    store: Arc<dyn lash_core::RuntimePersistence>,
}

/// A memory backend whose catalog is `catalog`: a test catalog that
/// records or faults the requests it serves.
pub(crate) async fn backend_with_catalog(
    catalog: Arc<dyn lash_core::SessionStoreFactory>,
) -> DecoratedBackend {
    DecoratedBackend::over(double_backend().await).session_store_factory(move |_| catalog)
}

/// A memory backend whose catalog serves `store` for every session id:
/// the fixture for a test that seeds or faults one session's persistence
/// directly. Every other port is the memory backend's.
pub(crate) async fn backend_serving(
    store: Arc<dyn lash_core::RuntimePersistence>,
) -> DecoratedBackend {
    DecoratedBackend::over(double_backend().await)
        .session_store_factory(move |_| Arc::new(ReusableStoreFactory { store }))
}

// The reusable mock store uses a no-op attachment manifest; this fixture
// explicitly owns no attachment roots.
#[async_trait::async_trait]
impl lash_core::AttachmentRootSet for ReusableStoreFactory {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<
        std::collections::BTreeSet<lash_core::AttachmentId>,
        lash_core::StoreError,
    > {
        Ok(std::collections::BTreeSet::new())
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &lash_core::AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<bool, lash_core::StoreError> {
        Ok(false)
    }
}

#[async_trait::async_trait]
impl lash_core::SessionStoreFactory for ReusableStoreFactory {
    // One reusable store backs every id this fixture serves, so a by-id
    // lookup hands back that store.
    async fn open_existing_store_by_id(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<Option<Arc<dyn lash_core::RuntimePersistence>>, lash_core::StoreError>
    {
        Ok(Some(
            Arc::clone(&self.store) as Arc<dyn lash_core::RuntimePersistence>
        ))
    }

    async fn pending_turn_cancel_closure_pins(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<Vec<lash_core::TurnCancelClosureAuthorization>, lash_core::StoreError>
    {
        Ok(Vec::new())
    }

    async fn create_store(
        &self,
        _request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<Arc<dyn lash_core::RuntimePersistence>, lash_core::StoreError> {
        Ok(Arc::clone(&self.store))
    }

    // The single reused store is never dropped and no tombstone is recorded.
    async fn session_was_deleted(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(
        &self,
        _session_id: &SessionId,
    ) -> lash_core::MaintenanceResult<lash_core::SessionBlobReclaimReport> {
        Ok(lash_core::SessionBlobReclaimReport::default())
    }

    // This fixture keeps no countable catalog, so it refuses rather than report zero turns.
    async fn count_unsettled_turns(
        &self,
    ) -> std::result::Result<lash_core::store::UnsettledTurnCounts, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::count_unsettled_turns",
        })
    }

    async fn list_turn_parks(
        &self,
        _query: &lash_core::store::TurnParkQuery,
    ) -> std::result::Result<Vec<lash_core::store::TurnPark>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::list_turn_parks",
        })
    }

    async fn turn_park_feed(
        &self,
        _after: lash_core::store::ParkFeedCursor,
        _limit: std::num::NonZeroUsize,
    ) -> std::result::Result<
        lash_core::store::ParkFeedPage<lash_core::store::TurnParkTarget>,
        lash_core::StoreError,
    > {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::turn_park_feed",
        })
    }

    async fn root_terminal(
        &self,
        _session_id: &lash_core::SessionId,
        _root: &lash_core::TurnId,
    ) -> std::result::Result<Option<lash_core::store::RootTerminal>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::root_terminal",
        })
    }

    async fn compact_turn_park_feed(
        &self,
        _through: lash_core::store::ParkFeedCursor,
    ) -> std::result::Result<(), lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::compact_turn_park_feed",
        })
    }
}

struct BoundSessionStore {
    session_id: SessionId,
    drive_epochs: lash_core::store::InMemoryDriveEpochs,
}

macro_rules! impl_unsupported_capture_store {
    ($store:ty) => {
        #[async_trait]
        impl lash_core::store::TurnCaptureStore for $store {
            async fn open_capture_writer(
                &self,
                _request: &lash_core::store::OpenCaptureWriter,
            ) -> std::result::Result<lash_core::store::CaptureWriterLease, lash_core::StoreError>
            {
                Err(lash_core::StoreError::UnsupportedStoreOperation {
                    operation: "TurnCaptureStore::open_capture_writer",
                })
            }

            async fn append_capture_batch(
                &self,
                _batch: &lash_core::store::CaptureBatch,
            ) -> std::result::Result<lash_core::store::CaptureAck, lash_core::StoreError> {
                Err(lash_core::StoreError::UnsupportedStoreOperation {
                    operation: "TurnCaptureStore::append_capture_batch",
                })
            }

            async fn persist_attempt_reset(
                &self,
                _reset: &lash_core::store::CaptureAttemptReset,
            ) -> std::result::Result<lash_core::store::CaptureWriterLease, lash_core::StoreError>
            {
                Err(lash_core::StoreError::UnsupportedStoreOperation {
                    operation: "TurnCaptureStore::persist_attempt_reset",
                })
            }

            async fn advance_capture_base(
                &self,
                _advance: &lash_core::store::CaptureBaseAdvance,
            ) -> std::result::Result<(), lash_core::StoreError> {
                Err(lash_core::StoreError::UnsupportedStoreOperation {
                    operation: "TurnCaptureStore::advance_capture_base",
                })
            }

            async fn seal_turn_capture(
                &self,
                _request: &lash_core::store::SealTurnCapture,
            ) -> std::result::Result<lash_core::store::SealedCapture, lash_core::StoreError> {
                Err(lash_core::StoreError::UnsupportedStoreOperation {
                    operation: "TurnCaptureStore::seal_turn_capture",
                })
            }

            async fn read_stopped_partial(
                &self,
                _request: &lash_core::store::StoppedPartialReadRequest,
            ) -> std::result::Result<lash_core::store::StoppedPartialRead, lash_core::StoreError>
            {
                Err(lash_core::StoreError::UnsupportedStoreOperation {
                    operation: "TurnCaptureStore::read_stopped_partial",
                })
            }
        }
    };
}

#[async_trait]
impl lash_core::store::TurnCaptureStore for SnapshotStore {
    async fn open_capture_writer(
        &self,
        request: &lash_core::store::OpenCaptureWriter,
    ) -> std::result::Result<lash_core::store::CaptureWriterLease, lash_core::StoreError> {
        self.captures.open_capture_writer(request).await
    }

    async fn append_capture_batch(
        &self,
        batch: &lash_core::store::CaptureBatch,
    ) -> std::result::Result<lash_core::store::CaptureAck, lash_core::StoreError> {
        self.captures.append_capture_batch(batch).await
    }

    async fn persist_attempt_reset(
        &self,
        reset: &lash_core::store::CaptureAttemptReset,
    ) -> std::result::Result<lash_core::store::CaptureWriterLease, lash_core::StoreError> {
        self.captures.persist_attempt_reset(reset).await
    }

    async fn advance_capture_base(
        &self,
        advance: &lash_core::store::CaptureBaseAdvance,
    ) -> std::result::Result<(), lash_core::StoreError> {
        self.captures.advance_capture_base(advance).await
    }

    async fn seal_turn_capture(
        &self,
        request: &lash_core::store::SealTurnCapture,
    ) -> std::result::Result<lash_core::store::SealedCapture, lash_core::StoreError> {
        self.captures.seal_turn_capture(request).await
    }

    async fn read_stopped_partial(
        &self,
        request: &lash_core::store::StoppedPartialReadRequest,
    ) -> std::result::Result<lash_core::store::StoppedPartialRead, lash_core::StoreError> {
        self.captures.read_stopped_partial(request).await
    }
}
impl_unsupported_capture_store!(BoundSessionStore);

lash_core::impl_noop_attachment_manifest!(BoundSessionStore);

lash_core::impl_current_fleet_format!(BoundSessionStore);

#[async_trait]
impl lash_core::SessionCommitStore for BoundSessionStore {
    async fn raise_pending_follow_on_attempts(
        &self,
        fence: &lash_core::store::DriveFence,
        follow_on_turn_id: &lash_core::TurnId,
    ) -> std::result::Result<lash_core::store::PendingFollowOn, lash_core::store::StoreError> {
        Err(lash_core::store::StoreError::FollowOnNotPending {
            session_id: fence.session().clone(),
            follow_on_turn_id: follow_on_turn_id.clone(),
        })
    }

    async fn admit_and_bind_session(
        &self,
        binding: &lash_core::SessionBinding,
    ) -> std::result::Result<lash_core::SessionAdmission, lash_core::store::StoreError> {
        let meta = self
            .load_session_meta()
            .await?
            .expect("bound test store metadata");
        if meta.session_id != binding.session_id {
            return Err(lash_core::store::StoreError::SessionBindingMismatch {
                bound_session_id: meta.session_id,
                attempted_session_id: binding.session_id.clone(),
            });
        }
        lash_core::store_backend_support::guard_rebind_lineage(
            &binding.session_id,
            &lash_core::SessionLineage::of(&meta.relation),
            &binding.relation,
        )?;
        Ok(lash_core::SessionAdmission::Rebound)
    }

    async fn load_session(
        &self,
    ) -> std::result::Result<
        Option<lash_core::store::PersistedSessionRead>,
        lash_core::store::StoreError,
    > {
        Ok(None)
    }

    async fn load_session_head_meta(
        &self,
    ) -> std::result::Result<Option<lash_core::store::SessionHeadMeta>, lash_core::store::StoreError>
    {
        Ok(None)
    }

    async fn load_node(
        &self,
        _node_id: &str,
    ) -> std::result::Result<Option<lash_core::SessionNodeRecord>, lash_core::store::StoreError>
    {
        Ok(None)
    }

    async fn commit_runtime_state(
        &self,
        _commit: lash_core::store::RuntimeCommit,
    ) -> std::result::Result<lash_core::store::RuntimeCommitReceipt, lash_core::store::StoreError>
    {
        unreachable!("test should fail before committing to the reused child store")
    }

    async fn save_session_meta(
        &self,
        _meta: lash_core::SessionMeta,
    ) -> std::result::Result<(), lash_core::store::StoreError> {
        Ok(())
    }

    async fn load_session_meta(
        &self,
    ) -> std::result::Result<Option<lash_core::SessionMeta>, lash_core::store::StoreError> {
        Ok(Some(lash_core::SessionMeta {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: self.session_id.clone(),
            relation: lash_core::SessionRelation::Root,
        }))
    }
}

#[async_trait]
impl lash_core::store::DriveEpochStore for BoundSessionStore {
    async fn seal_drive_epoch(
        &self,
        session_id: &SessionId,
        admission: &lash_core::store::AdmissionId,
        observed_epoch: u64,
        root_start: &lash_core::store::RootStartNonce,
    ) -> std::result::Result<lash_core::store::DriveEpochSeal, lash_core::StoreError> {
        Ok(self
            .drive_epochs
            .seal(session_id, admission, observed_epoch, root_start))
    }

    async fn drive_epoch(
        &self,
        session_id: &SessionId,
    ) -> std::result::Result<lash_core::store::StoredDriveEpoch, lash_core::StoreError> {
        Ok(self.drive_epochs.epoch(session_id))
    }
}

#[async_trait]
impl lash_core::StoreMaintenance for BoundSessionStore {
    async fn vacuum(&self) -> lash_core::MaintenanceResult<lash_core::VacuumReport> {
        Ok(lash_core::VacuumReport::default())
    }

    async fn gc_unreachable(&self) -> lash_core::MaintenanceResult<lash_core::GcReport> {
        Ok(lash_core::GcReport::default())
    }
}

#[derive(Default)]
struct RecordingStoreFactory {
    requests: std::sync::Mutex<Vec<lash_core::SessionStoreCreateRequest>>,
}

impl RecordingStoreFactory {
    fn provider_ids(&self) -> Vec<String> {
        self.requests
            .lock_recover()
            .iter()
            .map(|request| request.policy.recorded_provider_id().to_string())
            .collect()
    }
}

// SnapshotStore has a no-op attachment manifest; this request-recording
// fixture explicitly owns no attachment roots.
#[async_trait::async_trait]
impl lash_core::AttachmentRootSet for RecordingStoreFactory {
    async fn live_attachment_refs(
        &self,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<
        std::collections::BTreeSet<lash_core::AttachmentId>,
        lash_core::StoreError,
    > {
        Ok(std::collections::BTreeSet::new())
    }

    async fn has_live_attachment_ref(
        &self,
        _id: &lash_core::AttachmentId,
        _intent_grace_cutoff_epoch_ms: u64,
    ) -> std::result::Result<bool, lash_core::StoreError> {
        Ok(false)
    }
}

#[async_trait::async_trait]
impl lash_core::SessionStoreFactory for RecordingStoreFactory {
    // This fixture records requests and mints a fresh store per call; it keeps
    // no catalog to look an id up in, and says so rather than reporting every
    // session absent.
    async fn open_existing_store_by_id(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<Option<Arc<dyn lash_core::RuntimePersistence>>, lash_core::StoreError>
    {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::open_existing_store_by_id",
        })
    }

    async fn pending_turn_cancel_closure_pins(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<Vec<lash_core::TurnCancelClosureAuthorization>, lash_core::StoreError>
    {
        Ok(Vec::new())
    }

    async fn create_store(
        &self,
        request: &lash_core::SessionStoreCreateRequest,
    ) -> std::result::Result<Arc<dyn lash_core::RuntimePersistence>, lash_core::StoreError> {
        self.requests.lock_recover().push(request.clone());
        Ok(Arc::new(SnapshotStore::default()))
    }

    // Every create_store hands back a fresh store; nothing is ever tombstoned.
    async fn session_was_deleted(
        &self,
        _session_id: &SessionId,
    ) -> std::result::Result<bool, String> {
        Ok(false)
    }

    async fn delete_session(
        &self,
        _session_id: &SessionId,
    ) -> lash_core::MaintenanceResult<lash_core::SessionBlobReclaimReport> {
        Ok(lash_core::SessionBlobReclaimReport::default())
    }

    // This fixture keeps no countable catalog, so it refuses rather than report zero turns.
    async fn count_unsettled_turns(
        &self,
    ) -> std::result::Result<lash_core::store::UnsettledTurnCounts, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::count_unsettled_turns",
        })
    }

    async fn list_turn_parks(
        &self,
        _query: &lash_core::store::TurnParkQuery,
    ) -> std::result::Result<Vec<lash_core::store::TurnPark>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::list_turn_parks",
        })
    }

    async fn turn_park_feed(
        &self,
        _after: lash_core::store::ParkFeedCursor,
        _limit: std::num::NonZeroUsize,
    ) -> std::result::Result<
        lash_core::store::ParkFeedPage<lash_core::store::TurnParkTarget>,
        lash_core::StoreError,
    > {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::turn_park_feed",
        })
    }

    async fn root_terminal(
        &self,
        _session_id: &lash_core::SessionId,
        _root: &lash_core::TurnId,
    ) -> std::result::Result<Option<lash_core::store::RootTerminal>, lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::root_terminal",
        })
    }

    async fn compact_turn_park_feed(
        &self,
        _through: lash_core::store::ParkFeedCursor,
    ) -> std::result::Result<(), lash_core::StoreError> {
        Err(lash_core::StoreError::UnsupportedStoreOperation {
            operation: "SessionStoreFactory::compact_turn_park_feed",
        })
    }
}

#[derive(Default)]
struct RecordingEvents {
    events: TokioMutex<Vec<TurnActivity>>,
}

impl RecordingEvents {
    async fn snapshot(&self) -> Vec<TurnActivity> {
        self.events.lock().await.clone()
    }
}

#[async_trait]
impl TurnActivitySink for RecordingEvents {
    async fn emit(&self, activity: TurnActivity) {
        self.events.lock().await.push(activity);
    }
}

fn test_activity(correlation_id: &str, event: TurnEvent) -> TurnActivity {
    TurnActivity::new(TurnActivityId::new(correlation_id.to_string()), event)
}

fn assistant_prose(events: &[TurnActivity]) -> String {
    events
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::AssistantProseDelta { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect()
}

struct AppTools;

#[async_trait]
impl ToolProvider for AppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async { lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })) })
            .await
            .into()
    }
}

#[cfg(feature = "rlm")]
struct FailingAppTools;

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for FailingAppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async { lash_core::ToolOutcome::err_fmt("lookup failed but Lashlang recovered") })
            .await
            .into()
    }
}

struct PendingAppTools {
    key_tx: StdMutex<Option<oneshot::Sender<lash_core::AwaitEventKey>>>,
}

impl PendingAppTools {
    fn new(key_tx: oneshot::Sender<lash_core::AwaitEventKey>) -> Self {
        Self {
            key_tx: StdMutex::new(Some(key_tx)),
        }
    }
}

#[async_trait]
impl ToolProvider for PendingAppTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![app_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(app_tool_definition().contract()))
    }

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id == app_tool_definition().id()
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            assert_eq!(call.name(), "app_lookup");
            let key = match call.context.completion_key() {
                Ok(key) => key,
                Err(err) => return lash_core::ToolOutcome::err_fmt(err),
            };
            if let Some(tx) = self.key_tx.lock_recover().take() {
                let _ = tx.send(key);
            }
            lash_core::ToolOutcome::pending(lash_core::PendingCompletion::new())
        })
        .await
        .into()
    }
}

#[cfg(feature = "rlm")]
struct DurableInputTools {
    key_tx:
        StdMutex<Option<oneshot::Sender<std::result::Result<lash_core::AwaitEventKey, String>>>>,
    attempt_count: Arc<AtomicUsize>,
}

#[cfg(feature = "rlm")]
struct RetryingDirectTools;

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for RetryingDirectTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![retrying_direct_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "retrying_direct").then(|| Arc::new(retrying_direct_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        assert_eq!(call.name(), "retrying_direct");
        let model = match call.context.sessions().model().await {
            Ok(model) => model,
            Err(err) => return lash_core::ToolOutcome::err_fmt(err).into(),
        };
        let completion = match call
            .context
            .direct_completions()
            .complete(
                lash_core::facade_support::DirectRequest::text(
                    model.model,
                    format!(
                        "retrying direct completion attempt {}",
                        call.context.attempt_number()
                    ),
                ),
                "retrying_direct",
            )
            .await
        {
            Ok(completion) => completion,
            Err(err) => return lash_core::ToolOutcome::err_fmt(err).into(),
        };
        if call.context.attempt_number() == 1 {
            return lash_core::ToolOutcome::failure(lash_core::ToolFailure::safe_retry(
                lash_core::ToolFailureClass::Execution,
                "retrying_direct_first_attempt",
                "retry the complete atomic attempt",
                Some(0),
            ))
            .into();
        }
        lash_core::ToolOutcome::ok(serde_json::json!(completion.text)).into()
    }
}

#[cfg(feature = "rlm")]
fn retrying_direct_tool_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            "tool:retrying_direct",
            "retrying_direct",
            "Call a direct completion and retry the complete attempt once.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "string" }),
        )
        .with_retry_policy(lash_core::ToolRetryPolicy::safe(2, 0, 0)),
        "retrying_direct",
    )
}

#[cfg(feature = "rlm")]
impl DurableInputTools {
    fn new(key_tx: oneshot::Sender<std::result::Result<lash_core::AwaitEventKey, String>>) -> Self {
        Self {
            key_tx: StdMutex::new(Some(key_tx)),
            attempt_count: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn attempt_count(&self) -> usize {
        self.attempt_count.load(Ordering::SeqCst)
    }

    fn send_key_result(&self, result: std::result::Result<lash_core::AwaitEventKey, String>) {
        if let Some(tx) = self.key_tx.lock_recover().take() {
            let _ = tx.send(result);
        }
    }
}

#[cfg(feature = "rlm")]
#[async_trait]
impl ToolProvider for DurableInputTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![durable_input_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "mock_input_request").then(|| Arc::new(durable_input_tool_definition().contract()))
    }

    fn attempt_may_defer(&self, tool_id: &lash_core::ToolId) -> bool {
        tool_id == durable_input_tool_definition().id()
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            assert_eq!(call.name(), "mock_input_request");
            let question = call
                .args
                .get("question")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("answer")
                .to_string();
            let key = match call.context.completion_key() {
                Ok(key) => key,
                Err(err) => {
                    self.send_key_result(Err(err.to_string()));
                    return lash_core::ToolOutcome::err_fmt(err);
                }
            };
            self.attempt_count.fetch_add(1, Ordering::SeqCst);
            // The attempt body cannot append process events. It declares the
            // announcement instead, and the runtime appends it when the call parks.
            let announcement = lash_core::PendingAnnouncement::new(
                "process.yield",
                serde_json::json!({
                    "type": "work.input_request.opened",
                    "request_id": "request-1",
                    "question": question,
                    "await_key_id": key.key_id,
                }),
                "mock-input-request:request-1",
            );
            self.send_key_result(Ok(key));
            lash_core::ToolOutcome::pending(
                lash_core::PendingCompletion::new().announcing(announcement),
            )
        })
        .await
        .into()
    }
}

#[cfg(feature = "rlm")]
fn durable_input_tool_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            "tool:mock_input_request",
            "mock_input_request",
            "Open a durable input request and wait for the answer.",
            serde_json::json!({
                "type": "object",
                "properties": {
                    "question": { "type": "string" }
                },
                "required": ["question"],
                "additionalProperties": false
            }),
            serde_json::json!({
                "type": "object",
                "properties": {
                    "request_id": { "type": "string" },
                    "answer": {}
                },
                "required": ["request_id", "answer"],
                "additionalProperties": true
            }),
        ),
        "mock_input_request",
    )
}

struct AgentFrameSwitchTools;

#[async_trait]
impl ToolProvider for AgentFrameSwitchTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![agent_frame_switch_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "switch_frame").then(|| Arc::new(agent_frame_switch_tool_definition().contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            assert_eq!(call.name(), "switch_frame");
            let task = call
                .args
                .get("task")
                .and_then(serde_json::Value::as_str)
                .expect("task arg")
                .to_string();
            lash_core::ToolOutcome::ok(serde_json::json!({ "ok": true })).with_control(
                lash_core::ToolControl::SwitchAgentFrame {
                    frame_key: lash_core::FrameKey::from_caller_material("durable-follow-frame")
                        .expect("non-empty caller material"),
                    initial_nodes: Vec::new(),
                    task: Some(task),
                },
            )
        })
        .await
        .into()
    }
}

fn agent_frame_switch_tool_definition() -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        "tool:switch_frame",
        "switch_frame",
        "Switch to a fresh agent frame.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "task": { "type": "string" }
            },
            "required": ["task"],
            "additionalProperties": false
        }),
        serde_json::json!({ "type": "object" }),
    )
}

fn app_tool_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            "tool:app_lookup",
            "app_lookup",
            "Look up app state.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "object" }),
        ),
        "app_lookup",
    )
}

struct LongTextTools;

#[async_trait]
impl ToolProvider for LongTextTools {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![long_text_tool_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "app_lookup").then(|| Arc::new(long_text_tool_definition().contract()))
    }

    async fn execute(&self, _call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            lash_core::ToolOutcome::ok(serde_json::json!("abcdefghijklmnopqrstuvwxyz0123456789"))
        })
        .await
        .into()
    }
}

fn long_text_tool_definition() -> lash_core::ToolDefinition {
    test_tool_definition_with_tool_binding(
        lash_core::ToolDefinition::raw(
            "tool:app_lookup",
            "app_lookup",
            "Look up verbose app state.",
            serde_json::json!({
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }),
            serde_json::json!({ "type": "string" }),
        ),
        "app_lookup",
    )
}

fn test_tool_definition_with_tool_binding(
    definition: lash_core::ToolDefinition,
    name: impl Into<String>,
) -> lash_core::ToolDefinition {
    definition.with_tool_binding(lash_core::ToolBinding::new(["tools"], name))
}

struct SurfacePluginFactory;

impl lash_core::facade_support::PluginFactory for SurfacePluginFactory {
    fn id(&self) -> &'static str {
        "surface_test"
    }

    fn build(
        &self,
        _ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        Ok(Arc::new(SurfacePlugin))
    }
}

struct SurfacePlugin;

impl lash_core::facade_support::SessionPlugin for SurfacePlugin {
    fn id(&self) -> &'static str {
        "surface_test"
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        reg.output().response(Arc::new(|ctx| {
            Box::pin(async move {
                Ok(lash_core::facade_support::AssistantResponseTransform {
                    response: ctx.response,
                    events: vec![lash_core::PluginRuntimeEvent::Status {
                        key: "surface".to_string(),
                        label: "working".to_string(),
                        detail: Some("details".to_string()),
                    }],
                })
            })
        }));
        Ok(())
    }
}

fn mock_provider() -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .requires_streaming(true)
        .complete(|request| async move {
            let user_text = last_user_text(&request);
            let reply = format!("echo: {user_text}");
            if let Some(events) = request.stream_events.as_ref() {
                events.send(LlmStreamEvent::Delta {
                    block: lash_core::llm::types::StreamBlockIdentity::new("text:0", 0),
                    text: reply.clone(),
                });
            }
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: reply,
                    response_meta: None,
                }],
                usage: lash_core::llm::types::LlmUsage {
                    input_tokens: user_text.split_whitespace().count() as i64,
                    output_tokens: 2,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                },
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

/// A second provider whose kind differs from [`mock_provider`], for pinning
/// tests that must name a provider the session did not record.
fn other_kind_provider() -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("other-embed-test")
        .complete(|_request| async move {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: "other".to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

fn tool_roundtrip_provider() -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from([
        LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "call-1".to_string(),
                tool_name: "app_lookup".to_string(),
                input_json: "{}".to_string(),
                replay: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
        LlmResponse {
            parts: vec![LlmOutputPart::Text {
                text: "done".to_string(),
                response_meta: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
    ])));
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move { Ok(responses.lock().await.pop_front().expect("queued response")) }
        })
        .build()
        .into_handle()
}

fn agent_frame_switch_provider() -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from([
        LlmResponse {
            parts: vec![LlmOutputPart::ToolCall {
                call_id: "switch-call".to_string(),
                tool_name: "switch_frame".to_string(),
                input_json: serde_json::json!({
                    "task": "finish in the next frame"
                })
                .to_string(),
                replay: None,
            }],
            response_metadata: Default::default(),
            ..LlmResponse::default()
        },
        text_response("done after frame switch"),
    ])));
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move { Ok(responses.lock().await.pop_front().expect("queued response")) }
        })
        .build()
        .into_handle()
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        response_metadata: Default::default(),
        ..LlmResponse::default()
    }
}

#[cfg(feature = "rlm")]
fn typescript_block(source: &str) -> String {
    format!("<typescript>\n{}\n</typescript>", source.trim())
}

#[cfg(feature = "rlm")]
fn queued_text_provider(texts: Vec<impl Into<String>>) -> ProviderHandle {
    let responses = Arc::new(TokioMutex::new(VecDeque::from(
        texts
            .into_iter()
            .map(|text| {
                let text = text.into();
                LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text,
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                }
            })
            .collect::<Vec<_>>(),
    )));
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(move |_request| {
            let responses = Arc::clone(&responses);
            async move { Ok(responses.lock().await.pop_front().expect("queued response")) }
        })
        .build()
        .into_handle()
}

fn semantic_group_provider() -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("embed-test")
        .complete(|_request| async move {
            Ok(LlmResponse {
                parts: vec![
                    LlmOutputPart::Text {
                        text: "first".to_string(),
                        response_meta: Some(ResponseTextMeta {
                            id: Some("assistant:first".to_string()),
                            status: None,
                            phase: None,
                            ..ResponseTextMeta::default()
                        }),
                    },
                    LlmOutputPart::Text {
                        text: "second".to_string(),
                        response_meta: Some(ResponseTextMeta {
                            id: Some("assistant:second".to_string()),
                            status: None,
                            phase: None,
                            ..ResponseTextMeta::default()
                        }),
                    },
                ],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

fn text_provider(kind: &'static str, _model: &'static str, text: &'static str) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind(kind)
        .complete(move |_request| async move {
            Ok(LlmResponse {
                parts: vec![LlmOutputPart::Text {
                    text: text.to_string(),
                    response_meta: None,
                }],
                response_metadata: Default::default(),
                ..LlmResponse::default()
            })
        })
        .build()
        .into_handle()
}

type SeenModels = Arc<std::sync::Mutex<Vec<(String, lash_core::ReasoningSelection)>>>;

fn recording_text_provider(
    kind: &'static str,
    _model: &'static str,
    _variant: Option<&'static str>,
    text: &'static str,
    seen: SeenModels,
) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind(kind)
        .complete(move |request| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock_recover()
                    .push((request.model, request.model_variant));
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: text.to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

fn last_user_text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .rev()
        .find(|message| message.role == LlmRole::User)
        .map(|message| {
            message
                .blocks
                .iter()
                .filter_map(|block| match block {
                    LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

fn system_text(request: &LlmRequest) -> String {
    request
        .instructions
        .as_deref()
        .unwrap_or_default()
        .to_owned()
}

fn request_text(request: &LlmRequest) -> String {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter_map(|block| match block {
            LlmContentBlock::Text { text, .. } => Some(text.as_ref()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn recording_prompt_provider(seen: Arc<std::sync::Mutex<Vec<String>>>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("prompt-test")
        .complete(move |request| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock_recover().push(system_text(&request));
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "ok".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

#[cfg(feature = "rlm")]
fn recording_request_provider(seen: Arc<std::sync::Mutex<Vec<String>>>) -> ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("request-test")
        .complete(move |request| {
            let seen = Arc::clone(&seen);
            async move {
                seen.lock_recover().push(request_text(&request));
                Ok(text_response(&typescript_block("finish(\"ok\");")))
            }
        })
        .build()
        .into_handle()
}

fn retry_once_provider() -> ProviderHandle {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("retry-test")
        .requires_streaming(true)
        .options(lash_core::facade_support::ProviderOptions {
            reliability: lash_core::provider::ProviderReliability::default()
                .max_attempts(2)
                .base_delay_ms(0)
                .max_delay_ms(0),
            ..lash_core::facade_support::ProviderOptions::default()
        })
        .complete(move |_request| {
            let attempts = Arc::clone(&attempts);
            async move {
                if attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst) == 0 {
                    return Err(LlmTransportError::new("retry me").with_retry_verdict(
                        lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
                    ));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "retried".to_string(),
                        response_meta: None,
                    }],
                    response_metadata: Default::default(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

fn checkpoint_gated_provider(
    entered_tx: oneshot::Sender<()>,
    release_rx: oneshot::Receiver<()>,
) -> ProviderHandle {
    let entered_tx = Arc::new(std::sync::Mutex::new(Some(entered_tx)));
    let release_rx = Arc::new(TokioMutex::new(Some(release_rx)));
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("checkpoint-gated")
        .complete(move |request| {
            let entered_tx = Arc::clone(&entered_tx);
            let release_rx = Arc::clone(&release_rx);
            let calls = Arc::clone(&calls);
            async move {
                let call = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if call == 0 {
                    if let Some(tx) = entered_tx.lock_recover().take() {
                        let _ = tx.send(());
                    }
                    if let Some(rx) = release_rx.lock().await.take() {
                        let _ = rx.await;
                    }
                    Ok(text_response("first"))
                } else {
                    Ok(text_response(&format!(
                        "after {}",
                        last_user_text(&request)
                    )))
                }
            }
        })
        .build()
        .into_handle()
}

pub(crate) async fn standard_core() -> LashCore {
    standard_core_over(double_backend().await)
}

/// A standard core over `backend`.
pub(crate) fn standard_core_over(backend: lash_core::Backend) -> LashCore {
    explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
    ))
    .provider(mock_provider())
    .model(mock_model_spec())
    .build(crate::testing::runtime_lease_owner())
    .expect("standard core")
}

/// Default RLM protocol factory for tests, over `backend`, the substrate its
/// Lashlang artifacts live in.
#[cfg(feature = "rlm")]
fn rlm_factory(backend: &lash_core::Backend) -> lash_protocol_rlm::RlmProtocolPluginFactory {
    lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        backend,
    )
}

/// A [`LashCoreBuilder`] pre-seeded with the default RLM factory.
#[cfg(feature = "rlm")]
async fn rlm_core_builder() -> crate::core::LashCoreBuilder {
    rlm_core_builder_over(double_backend().await)
}

/// [`rlm_core_builder`] over `backend`: the core and its RLM factory share
/// the one backend.
#[cfg(feature = "rlm")]
fn rlm_core_builder_over(backend: lash_core::Backend) -> crate::core::LashCoreBuilder {
    let factory = rlm_factory(&backend);
    LashCore::rlm_builder(backend, crate::TurnBudget::Unbounded, factory)
}

mod scope_support;
use scope_support::{
    delete_bound_session, delete_bound_session_outcome, host_scope, runtime_operation_scope,
    text_message,
};
mod control_admin;
mod core_session_builder;
mod deployment_and_testing_facade;
mod durable_session;
mod harness;
pub(crate) use harness::{
    AcceptedSend as _, DecoratedBackend, core_now_ms, double_backend,
    double_backend_explicit_reconcile, double_backend_over, double_backend_over_explicit_reconcile,
    explicit_ephemeral_facets, explicit_ephemeral_facets_with_budget, held_double, latest_double,
    memory_store_backend, memory_store_set, mock_model_spec, model_spec, output_into_cancelled_by,
    redeploy, restate_double, retry_when_claim_frees, run_async_test_on_stack_budget,
    serve_processes, settle_session_drive, store_backend_with_clock, turn_input_states,
};
mod agent_scenarios;
#[cfg(feature = "rlm")]
mod aggregate_await_comprehension;
#[cfg(feature = "rlm")]
mod aggregate_oracle;
mod commit_superseded;
#[cfg(feature = "rlm")]
mod discovery_execution;
mod failure_settlement;
mod finalize_fault;
mod obligation_relays;
mod plugin_stack;
#[cfg(feature = "rlm")]
mod processes_endstate;
#[cfg(feature = "rlm")]
mod redrive_residue;
#[cfg(feature = "rlm")]
mod rlm_restore_idempotence;
mod send_handle;
mod session_control;
mod session_drive;
#[cfg(feature = "rlm")]
mod stack_budget;
mod standard_compaction_persistence;
mod tool_intent_ingress;
mod tool_restore_report;
mod turn_streaming;
#[cfg(feature = "rlm")]
mod usage_durability;
mod withheld_follow_on;

#[path = "tests/control_intent_doubles.rs"]
mod control_intent_doubles;
#[path = "tests/root_stores.rs"]
mod root_stores;
#[path = "tests/turn_input_stores.rs"]
mod turn_input_stores;
