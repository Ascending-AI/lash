//! The runtime's settled-session persistence contract and shared store types.
use crate::facade_support::SessionGraphFacadeOps;
mod attachment_manifest;
include!("checkpoint.rs");
mod claim_settlement;
mod commit_budget;
mod commit_identity;
mod error;
mod fork_plan;
mod graph_commit;
mod lease_timings;
mod load;
pub mod queued_work;
mod realization;
mod runtime_commit;
mod runtime_commit_plan;
pub(crate) mod session_execution_lease;
#[cfg(any(test, feature = "testing"))]
mod testing;
mod turn_id;
mod usage;
mod work_claim;

pub use crate::session_graph::RealizedNodeTimestamp;
pub use attachment_manifest::{
    AttachmentIntent, AttachmentManifest, AttachmentManifestEntry, AttachmentOwnerKind,
};
pub use commit_budget::{CommitBudget, CommitBudgetLimit};
pub use commit_identity::{
    OperationId, RuntimeCommitReceiptDecision, decide_runtime_commit_receipt,
    derive_history_node_id,
};
pub use error::{SessionExecutionLeaseRenewalInstallMismatch, StoreError};
#[doc(hidden)]
pub use fork_plan::{ForkLineageAncestor, ForkNodeFacts, ForkPlan};
pub use lease_timings::{LeaseTimings, LeaseTimingsError};
pub use load::{load_persisted_session_state, refresh_persisted_session_state};
pub use queued_work::{QueuedWorkClass, SelectedQueuedWorkClaimOutcome};
pub use realization::commit_runtime_state_verified;
pub use runtime_commit::{
    RuntimeCommit, RuntimeCommitResult, RuntimeTurnCommitStamp, RuntimeUsageDelta,
    RuntimeUsageDeltaIdentity,
};
#[doc(hidden)]
pub use runtime_commit_plan::{
    FreshRuntimeCommitFacts, ParentNodeFacts, PlannedNodeFacts, RuntimeCommitPlan,
    RuntimeCommitPlanner, RuntimeCommitReceiptRecord, RuntimeCommitReceiptWrite,
    RuntimeCommitReplay,
};
pub use session_execution_lease::{
    LeaseClaimNonce, LeaseOwnerIdentity, SessionExecutionLease, SessionExecutionLeaseAcquisition,
    SessionExecutionLeaseAuthority, SessionExecutionLeaseClaimOutcome,
    SessionExecutionLeaseDisplacement,
};
#[cfg(any(test, feature = "testing"))]
pub use testing::append_request_commit_with_clock_for_testing;
pub use turn_id::TurnId;
pub use usage::{merge_token_ledger_entries_checked, merge_token_ledger_entry_checked};
pub use work_claim::{WorkClaim, WorkCompletion};

fn default_root_session_id() -> String {
    "root".to_string()
}
pub const SESSION_HEAD_META_SCHEMA_VERSION: u32 = 3;

#[cfg(test)]
mod fig1376_tests;

#[cfg(test)]
mod persisted_state_tests {
    use super::*;

    #[test]
    fn persisted_state_hydrates_provider_id_without_live_provider_rebinding() {
        let state = persisted_session_state_from_head(
            SessionHead {
                session_id: "stored".to_string(),
                head_revision: 7,
                current_frame_node_id: None,
                graph: crate::SessionGraph::default(),
                config: crate::PersistedSessionConfig {
                    provider_id: "stored-provider".to_string(),
                    model: crate::ModelSpec::default(),
                    turn_budget: crate::TurnBudget::Unbounded,
                    prompt: crate::PromptLayer::new(),
                },
                checkpoint_ref: None,
                token_ledger: Vec::new(),
            },
            None,
        )
        .expect("valid persisted state");

        assert_eq!(state.policy.recorded_provider_id(), "stored-provider");
        assert_eq!(state.head_revision, 7);
    }

    #[test]
    fn versioned_json_record_rejects_missing_schema_version() {
        let err = decode_versioned_json_record::<SessionHeadPayload>(
            "{}",
            "SessionHeadMeta",
            SESSION_HEAD_META_SCHEMA_VERSION,
        )
        .expect_err("pre-versioned session head should fail");

        assert!(matches!(
            err,
            StoreError::MissingRecordSchemaVersion {
                record_kind: "SessionHeadMeta",
                expected: SESSION_HEAD_META_SCHEMA_VERSION
            }
        ));
    }

    #[test]
    fn versioned_json_record_rejects_invalid_schema_version() {
        let err = decode_versioned_json_record::<SessionHeadPayload>(
            r#"{"schema_version":"1"}"#,
            "SessionHeadMeta",
            SESSION_HEAD_META_SCHEMA_VERSION,
        )
        .expect_err("invalid session head schema version should fail");

        assert!(matches!(
            err,
            StoreError::InvalidRecordSchemaVersion {
                record_kind: "SessionHeadMeta",
                expected: SESSION_HEAD_META_SCHEMA_VERSION,
                ..
            }
        ));
    }

    #[test]
    fn versioned_json_record_rejects_unsupported_schema_version() {
        let unsupported = SESSION_HEAD_META_SCHEMA_VERSION + 1;
        let err = decode_versioned_json_record::<SessionHeadPayload>(
            &format!(r#"{{"schema_version":{unsupported}}}"#),
            "SessionHeadMeta",
            SESSION_HEAD_META_SCHEMA_VERSION,
        )
        .expect_err("unsupported session head schema version should fail");

        assert!(matches!(
            err,
            StoreError::UnsupportedRecordSchemaVersion {
                record_kind: "SessionHeadMeta",
                actual,
                expected: SESSION_HEAD_META_SCHEMA_VERSION
            } if actual == unsupported
        ));
    }

    #[test]
    fn session_meta_rejects_unknown_durable_fields() {
        let error = serde_json::from_str::<SessionMeta>(
            r#"{
                "session_id":"stored",
                "session_name":"stored",
                "created_at":"2026-08-01T00:00:00Z",
                "model":"example",
                "cwd":"/tmp",
                "relation":{"kind":"root"}
            }"#,
        )
        .expect_err("pre-cutover session metadata must not decode by omission");

        assert!(
            error.to_string().contains("unknown field `session_name`"),
            "strict decode must name the first obsolete field: {error}"
        );
    }

    #[test]
    fn session_meta_rejects_unknown_fields_in_nested_relation() {
        let error = serde_json::from_str::<SessionMeta>(
            r#"{
                "session_id":"stored",
                "relation":{
                    "kind":"child",
                    "parent_session_id":"parent",
                    "legacy":true
                }
            }"#,
        )
        .expect_err("nested durable relation fields must not decode by omission");

        assert!(
            error.to_string().contains("unknown field `legacy`"),
            "strict nested decode must name the obsolete relation field: {error}"
        );
    }

    #[test]
    fn session_meta_rejects_unknown_fields_in_nested_causal_ref() {
        let error = serde_json::from_str::<SessionMeta>(
            r#"{
                "session_id":"stored",
                "relation":{
                    "kind":"child",
                    "parent_session_id":"parent",
                    "caused_by":{
                        "type":"turn",
                        "session_id":"source",
                        "turn_id":"turn",
                        "legacy":true
                    }
                }
            }"#,
        )
        .expect_err("nested durable causal fields must not decode by omission");

        assert!(
            error.to_string().contains("unknown field `legacy`"),
            "strict nested decode must name the obsolete causal field: {error}"
        );
    }

    #[test]
    fn session_meta_rejects_extra_observer_inheritance_variants() {
        let error = serde_json::from_str::<SessionMeta>(
            r#"{
                "session_id":"stored",
                "relation":{
                    "kind":"fork",
                    "source_session_id":"source",
                    "source_node_id":"node",
                    "observer_inheritance":{
                        "only":["process"],
                        "legacy":true
                    }
                }
            }"#,
        )
        .expect_err("externally tagged nested enums must reject extra variants");

        assert!(
            error.to_string().contains("expected map with a single key"),
            "externally tagged enum must reject the second variant key: {error}"
        );
    }
}

#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct SessionMeta {
    pub session_id: String,
    pub relation: crate::SessionRelation,
}

impl SessionMeta {
    /// Returns the parent session id, if any, derived from the canonical
    /// [`SessionRelation`](crate::SessionRelation) field.
    pub fn parent_session_id(&self) -> Option<&str> {
        self.relation.parent_session_id()
    }
}

/// Complete durable identity metadata supplied at session admission.
///
/// Session ids are opaque, non-empty UTF-8 strings. Lash deliberately imposes
/// no additional length or character-set policy; hosts that expose ids in URLs,
/// filenames, or other constrained namespaces own those boundary rules.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SessionBinding {
    pub session_id: String,
    pub relation: crate::SessionRelation,
}

impl SessionBinding {
    /// Builds a root binding for store implementors.
    pub fn root(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            relation: crate::SessionRelation::Root,
        }
    }

    /// Projects the durable binding fields store implementors need from a create request.
    pub fn from_create_request(request: &crate::SessionStoreCreateRequest) -> Self {
        Self {
            session_id: request.session_id.clone(),
            relation: request.relation.clone(),
        }
    }

    /// Rejects an empty session ID before store implementors admit the binding.
    pub fn validate(&self) -> Result<(), StoreError> {
        validate_session_id(&self.session_id)
    }
}

/// Outcome of admitting a session binding to a persistence handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionAdmission {
    /// The admission durably created the session metadata row.
    Created,
    /// The handle was already durably bound to the same live session.
    Rebound,
}

pub fn validate_session_id(session_id: &str) -> Result<(), StoreError> {
    if session_id.is_empty() {
        Err(StoreError::InvalidSessionId {
            reason: "session ids must not be empty",
        })
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub struct BlobRef(pub String);

impl BlobRef {
    /// Exposes the opaque durable blob reference to store implementors for backend round-tripping
    /// without imposing path or URL semantics.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for BlobRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<String> for BlobRef {
    fn from(value: String) -> Self {
        Self(value)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GcReport {
    pub root_count: usize,
    pub retained_blob_count: usize,
    pub deleted_blob_count: usize,
}

/// Result of a `StoreMaintenance::vacuum()` call.
/// `removed_node_count` counts the tombstoned graph-node rows that were
/// physically deleted from the store. `removed_pending_turn_input_tombstone_count`
/// counts terminal pending-input evidence rows pruned by host-scheduled
/// retention. Returned so hosts can emit metrics.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VacuumReport {
    pub removed_node_count: usize,
    pub removed_pending_turn_input_tombstone_count: usize,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionHead {
    #[serde(default = "default_root_session_id")]
    pub session_id: String,
    #[serde(skip)]
    pub head_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_frame_node_id: Option<String>,
    pub graph: crate::SessionGraph,
    pub config: crate::PersistedSessionConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checkpoint_ref: Option<BlobRef>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub token_ledger: Vec<crate::TokenLedgerEntry>,
}

/// JSON-owned fields persisted in a session head's `head_json` column.
///
/// Revision and graph/checkpoint references live in dedicated columns and are
/// deliberately absent from this serializable payload.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct SessionHeadPayload {
    pub schema_version: u32,
    #[serde(default = "default_root_session_id")]
    pub session_id: String,
    pub config: crate::PersistedSessionConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_frame_node_id: Option<String>,
}

/// Fully assembled session-head metadata returned by a store.
///
/// This type is intentionally not serializable. Store implementations decode a
/// [`SessionHeadPayload`] and must supply the three column-owned values through
/// [`Self::assemble`].
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct SessionHeadMeta {
    pub schema_version: u32,
    pub session_id: String,
    pub head_revision: u64,
    pub config: crate::PersistedSessionConfig,
    pub current_frame_node_id: Option<String>,
    pub checkpoint_ref: Option<BlobRef>,
    pub leaf_node_id: Option<String>,
}

impl SessionHeadMeta {
    /// Combine the JSON payload with all dedicated-column values.
    ///
    /// This remains public because external stores assemble rows from their
    /// own columns. Callers conventionally enforce session binding and node
    /// derivation before assembly; the constructor cannot validate those facts
    /// from this projection alone.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    pub fn assemble(
        payload: SessionHeadPayload,
        head_revision: u64,
        checkpoint_ref: Option<BlobRef>,
        leaf_node_id: Option<String>,
    ) -> Self {
        Self {
            schema_version: payload.schema_version,
            session_id: payload.session_id,
            head_revision,
            config: payload.config,
            current_frame_node_id: payload.current_frame_node_id,
            checkpoint_ref,
            leaf_node_id,
        }
    }

    /// Project the exact value that may be serialized into `head_json`.
    pub fn payload(&self) -> SessionHeadPayload {
        SessionHeadPayload {
            schema_version: self.schema_version,
            session_id: self.session_id.clone(),
            config: self.config.clone(),
            current_frame_node_id: self.current_frame_node_id.clone(),
        }
    }
}

fn persisted_session_config_from_state(
    state: &crate::RuntimeSessionState,
) -> crate::PersistedSessionConfig {
    crate::PersistedSessionConfig {
        provider_id: state.policy.recorded_provider_id().to_string(),
        model: state.policy.model.clone(),
        turn_budget: state.policy.turn_budget,
        prompt: state.policy.prompt.clone(),
    }
}

#[derive(Clone, Debug)]
pub struct PersistedSessionRead {
    pub session_id: String,
    pub head_revision: u64,
    pub config: crate::PersistedSessionConfig,
    pub current_frame_node_id: Option<String>,
    pub graph: crate::SessionGraph,
    pub checkpoint_ref: Option<BlobRef>,
    pub checkpoint: Option<HydratedSessionCheckpoint>,
    pub token_ledger: Vec<crate::TokenLedgerEntry>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct GraphAppend {
    pub nodes: Vec<crate::SessionNodeRecord>,
    pub leaf_node_id: Option<String>,
}

impl GraphAppend {
    pub fn leaf_node_id(&self) -> Option<&String> {
        self.leaf_node_id.as_ref()
    }
}

/// Reject a persisted record whose `schema_version` does not match the
/// version this binary supports. Backends call this immediately after
/// deserializing a record from durable storage.
pub fn ensure_supported_schema_version(
    record_kind: &'static str,
    actual: u32,
    expected: u32,
) -> Result<(), StoreError> {
    if actual == expected {
        Ok(())
    } else {
        Err(StoreError::UnsupportedRecordSchemaVersion {
            record_kind,
            actual,
            expected,
        })
    }
}

pub fn ensure_supported_record_schema_version(
    record_kind: &'static str,
    value: &serde_json::Value,
    expected: u32,
) -> Result<(), StoreError> {
    let Some(schema_version) = value.get("schema_version") else {
        return Err(StoreError::MissingRecordSchemaVersion {
            record_kind,
            expected,
        });
    };
    let Some(actual) = schema_version
        .as_u64()
        .and_then(|version| u32::try_from(version).ok())
    else {
        return Err(StoreError::InvalidRecordSchemaVersion {
            record_kind,
            actual: schema_version.to_string(),
            expected,
        });
    };
    ensure_supported_schema_version(record_kind, actual, expected)
}

pub fn decode_versioned_json_record<T>(
    json: &str,
    record_kind: &'static str,
    expected: u32,
) -> Result<T, StoreError>
where
    T: serde::de::DeserializeOwned,
{
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|err| StoreError::Backend(format!("failed to decode {record_kind}: {err}")))?;
    ensure_supported_record_schema_version(record_kind, &value, expected)?;
    serde_json::from_value(value)
        .map_err(|err| StoreError::Backend(format!("failed to decode {record_kind}: {err}")))
}

fn build_persisted_turn_state(state: &crate::RuntimeSessionState) -> crate::PersistedTurnState {
    crate::PersistedTurnState {
        turn_index: state.turn_index,
        token_usage: state.token_usage.clone(),
        last_prompt_usage: state.last_prompt_usage.clone(),
        protocol_turn_options: state.protocol_turn_options.clone(),
    }
}

pub(crate) fn encode_checkpoint_component<T: serde::Serialize>(
    key: &str,
    value: &T,
) -> Result<Vec<u8>, StoreError> {
    rmp_serde::to_vec_named(value).map_err(|error| StoreError::RecordEncodingFailed {
        record_kind: format!("checkpoint component `{key}`"),
        message: error.to_string(),
    })
}

fn build_checkpoint_from_persisted_state(
    state: &crate::RuntimeSessionState,
) -> Result<HydratedSessionCheckpoint, StoreError> {
    state.checkpoint_components.build_checkpoint(
        build_persisted_turn_state(state),
        state.plugin_snapshot_revision,
    )
}

impl RuntimeCommit {
    /// Rejects an invalid or empty operation identity and any session-scoped operation whose
    /// session differs from the commit before store implementors write it.
    pub fn validate_operation_session(&self) -> Result<(), StoreError> {
        let completed = &self.turn_commit;
        completed
            .operation
            .scope
            .validate()
            .map_err(|err| StoreError::Backend(err.to_string()))?;
        if completed.operation.key.trim().is_empty() {
            return Err(StoreError::Backend(
                "commit operation identity requires a non-empty key".to_string(),
            ));
        }
        if completed
            .operation
            .scope
            .session_id()
            .is_some_and(|session_id| session_id != self.session_id)
        {
            return Err(StoreError::RuntimeTurnCommitConflict {
                session_id: self.session_id.clone(),
                turn_id: completed.operation.storage_key()?,
            });
        }
        commit_identity::validate_append_receipt_identity(completed)?;
        self.validate_usage_delta_identities()?;
        Ok(())
    }

    fn validate_usage_delta_identities(&self) -> Result<(), StoreError> {
        let mut seen = std::collections::HashSet::with_capacity(self.usage_deltas.len());
        for delta in &self.usage_deltas {
            if delta.identity.operation_storage_key.trim().is_empty() {
                return Err(StoreError::Backend(
                    "runtime usage delta identity requires a non-empty operation storage key"
                        .to_string(),
                ));
            }
            let expected = RuntimeUsageDeltaIdentity::for_entry(
                delta.identity.operation_storage_key.clone(),
                delta.identity.entry_ordinal,
                &delta.entry,
            );
            if delta.identity.payload_encoding_version != expected.payload_encoding_version
                || delta.identity.payload_hash != expected.payload_hash
            {
                return Err(StoreError::Backend(format!(
                    "runtime usage delta identity payload encoding version or hash does not match canonical entry content ({}, {})",
                    delta.identity.operation_storage_key, delta.identity.entry_ordinal
                )));
            }
            if !seen.insert(&delta.identity) {
                return Err(StoreError::Backend(format!(
                    "runtime commit repeats usage delta identity ({}, {}, {}, {})",
                    delta.identity.operation_storage_key,
                    delta.identity.entry_ordinal,
                    delta.identity.payload_encoding_version,
                    delta.identity.payload_hash
                )));
            }
        }
        Ok(())
    }

    /// Exhaustive append-envelope allowlist. Adding a new commit member forces
    /// this destructure to be reconsidered, while the checks keep append
    /// commits from silently acquiring another unrelated settlement side
    /// effect. Usage is deliberately allowed because it has its own durable
    /// exactly-once identity.
    pub(crate) fn debug_assert_append_envelope_scope(&self) {
        let RuntimeCommit {
            commit_budget: _,
            session_id: _,
            expected_head_revision: _,
            session_execution_lease_fence: _,
            release_session_execution_lease: _,
            config: _,
            current_frame_node_id: _,
            graph: _,
            checkpoint: _,
            usage_deltas: _,
            turn_commit,
            completed_queue_claims,
            completed_turn_input_claims,
            enqueued_queue_batches,
            interrupted_turn_input_turn_id,
            committed_attachment_ids,
        } = self;
        debug_assert!(turn_commit.request_identity_hash.is_some());
        debug_assert!(
            completed_queue_claims.is_empty()
                && completed_turn_input_claims.is_empty()
                && enqueued_queue_batches.is_empty()
                && interrupted_turn_input_turn_id.is_none()
                && committed_attachment_ids.is_empty(),
            "append-session-nodes constructor gained unrelated settlement side effects"
        );
    }

    /// Re-derives every appended node ID by ordinal for store implementors, using frame keys for
    /// frame-open nodes and operation identity for all other nodes.
    pub fn validate_node_derivation(&self) -> Result<(), StoreError> {
        let completed = &self.turn_commit;
        for (ordinal, node) in self.graph.nodes.iter().enumerate() {
            let expected = match &node.payload {
                crate::SessionNodePayload::FrameOpen { frame_key, .. } => {
                    crate::session_graph::frame_node_id(&self.session_id, frame_key)
                }
                _ => {
                    derive_history_node_id(&self.session_id, &completed.operation, ordinal as u64)?
                }
            };
            if node.node_id != expected {
                return Err(StoreError::NodeIdDerivationMismatch {
                    node_id: node.node_id.clone(),
                    expected_node_id: expected,
                });
            }
        }
        Ok(())
    }

    /// Rejects duplicate node IDs within one append batch before store implementors mutate durable
    /// graph state.
    pub fn validate_append_node_ids_unique(&self) -> Result<(), StoreError> {
        let mut seen = std::collections::HashSet::with_capacity(self.graph.nodes.len());
        for node in &self.graph.nodes {
            if !seen.insert(node.node_id.as_str()) {
                return Err(StoreError::NodeIdCollision {
                    node_id: node.node_id.clone(),
                });
            }
        }
        Ok(())
    }

    /// Flattens application evidence from completed turn-input claims in completion and per-claim
    /// order for store implementors returning commit results.
    pub fn turn_input_applications(&self) -> Vec<crate::TurnInputApplication> {
        self.completed_turn_input_claims
            .iter()
            .flat_map(|completion| completion.applications.iter().cloned())
            .collect()
    }

    pub(crate) fn validate_claim_settlement(
        &self,
        originating_queue_claims: &[crate::QueuedWorkCompletion],
        originating_turn_input_claims: &[crate::TurnInputCompletion],
    ) -> Result<(), StoreError> {
        claim_settlement::validate_claim_settlement(
            self,
            originating_queue_claims,
            originating_turn_input_claims,
        )
    }

    /// Computes the canonical semantic commit hash store implementors use to distinguish idempotent
    /// replay from a conflicting operation reuse.
    pub fn turn_commit_hash(&self) -> Result<String, StoreError> {
        commit_identity::turn_commit_hash(self)
    }

    pub(crate) fn persisted_state_with_operation_and_budget(
        state: &mut crate::RuntimeSessionState,
        usage_deltas: &[crate::TokenLedgerEntry],
        operation: OperationId,
        commit_budget: CommitBudget,
    ) -> Result<(Self, Vec<String>), StoreError> {
        let mut graph = state.pending_graph_commit();
        let mapping = graph.derive_node_ids(&state.session_id, &operation)?;
        state
            .session_graph
            .remap_node_ids(&state.session_id, &mapping);
        remap_optional_node_id(&mut state.current_frame_node_id, &mapping);
        state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
        let persisted_node_ids = mapping.iter().map(|(_, derived)| derived.clone()).collect();
        let commit = Self::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            graph,
            usage_deltas,
            operation,
            commit_budget,
        )?;
        Ok((commit, persisted_node_ids))
    }

    pub(crate) fn persisted_state_with_operation_and_staged_usage_and_budget(
        state: &mut crate::RuntimeSessionState,
        usage_deltas: &[RuntimeUsageDelta],
        operation: OperationId,
        commit_budget: CommitBudget,
    ) -> Result<(Self, Vec<String>), StoreError> {
        let mut graph = state.pending_graph_commit();
        let mapping = graph.derive_node_ids(&state.session_id, &operation)?;
        state
            .session_graph
            .remap_node_ids(&state.session_id, &mapping);
        remap_optional_node_id(&mut state.current_frame_node_id, &mapping);
        state.agent_frames = state.session_graph.agent_frame_records(&state.session_id);
        let persisted_node_ids = mapping.iter().map(|(_, derived)| derived.clone()).collect();
        let commit = Self::persisted_state_with_graph_commit_and_staged_usage_and_budget(
            state,
            graph,
            usage_deltas,
            operation,
            commit_budget,
        )?;
        Ok((commit, persisted_node_ids))
    }

    pub(crate) fn persisted_state_with_graph_commit_and_operation_and_budget(
        state: &crate::RuntimeSessionState,
        graph: GraphAppend,
        usage_deltas: &[crate::TokenLedgerEntry],
        operation: OperationId,
        commit_budget: CommitBudget,
    ) -> Result<Self, StoreError> {
        let usage_deltas = RuntimeUsageDelta::for_operation(&operation, usage_deltas)?;
        Self::persisted_state_with_graph_commit_and_staged_usage_and_budget(
            state,
            graph,
            &usage_deltas,
            operation,
            commit_budget,
        )
    }

    pub(crate) fn persisted_state_with_graph_commit_and_staged_usage_and_budget(
        state: &crate::RuntimeSessionState,
        graph: GraphAppend,
        usage_deltas: &[RuntimeUsageDelta],
        operation: OperationId,
        commit_budget: CommitBudget,
    ) -> Result<Self, StoreError> {
        let mut projected_graph = state.session_graph.clone();
        for node in &graph.nodes {
            if projected_graph.find_node(&node.node_id).is_none() {
                projected_graph.extend_node_records(std::iter::once(node.clone()));
            }
        }
        projected_graph.set_leaf_node_id(graph.leaf_node_id().cloned());
        let current_frame_node_id = projected_graph
            .nearest_frame_node_id(projected_graph.leaf_node_id.as_deref())
            .map(ToOwned::to_owned);
        Ok(Self {
            commit_budget,
            session_id: state.session_id.clone(),
            expected_head_revision: state.head_revision,
            session_execution_lease_fence: None,
            release_session_execution_lease: None,
            config: persisted_session_config_from_state(state),
            current_frame_node_id,
            graph,
            checkpoint: build_checkpoint_from_persisted_state(state)?,
            usage_deltas: usage_deltas.to_vec(),
            turn_commit: RuntimeTurnCommitStamp::new(operation),
            completed_queue_claims: Vec::new(),
            completed_turn_input_claims: Vec::new(),
            enqueued_queue_batches: Vec::new(),
            interrupted_turn_input_turn_id: None,
            committed_attachment_ids: Vec::new(),
        })
    }

    /// Derive append-node identities, stamp the operation, and return the
    /// old-to-derived id mapping so callers can remap any resident graph that
    /// supplied the commit.
    pub fn with_operation(
        mut self,
        operation: OperationId,
    ) -> Result<(Self, Vec<(String, String)>), StoreError> {
        let session_id = self.session_id.clone();
        let node_id_mapping = self.graph.derive_node_ids(&session_id, &operation)?;
        remap_optional_node_id(&mut self.current_frame_node_id, &node_id_mapping);
        self.turn_commit = RuntimeTurnCommitStamp::new(operation);
        Ok((self, node_id_mapping))
    }

    /// Adds exact lease-completion evidence for store implementors to release atomically with the
    /// runtime commit rather than in a separate raceable write.
    pub fn releasing_session_execution_lease(
        mut self,
        completion: SessionExecutionLeaseAuthority,
    ) -> Self {
        self.release_session_execution_lease = Some(completion);
        self
    }

    /// Requires the caller's current authority without changing lane ownership.
    pub fn borrowing_session_execution_lease(
        mut self,
        fence: SessionExecutionLeaseAuthority,
    ) -> Self {
        self.session_execution_lease_fence = Some(fence);
        self
    }

    /// Adds one queued-work completion for store implementors to settle atomically with the runtime
    /// commit.
    pub fn completing_queue_claim(
        mut self,
        completed_queue_claim: crate::QueuedWorkCompletion,
    ) -> Self {
        self.completed_queue_claims.push(completed_queue_claim);
        self
    }

    /// Adds queued-work completions in caller order for store implementors to settle atomically
    /// with the runtime commit.
    pub fn completing_queue_claims(
        mut self,
        completed_queue_claims: impl IntoIterator<Item = crate::QueuedWorkCompletion>,
    ) -> Self {
        self.completed_queue_claims.extend(completed_queue_claims);
        self
    }

    /// Adds one turn-input completion for store implementors to settle atomically with the runtime
    /// commit.
    pub fn completing_turn_input_claim(
        mut self,
        completed_turn_input_claim: crate::TurnInputCompletion,
    ) -> Self {
        self.completed_turn_input_claims
            .push(completed_turn_input_claim);
        self
    }

    /// Adds turn-input completions in caller order for store implementors to settle atomically with
    /// the runtime commit.
    pub fn completing_turn_input_claims(
        mut self,
        completed_turn_input_claims: impl IntoIterator<Item = crate::TurnInputCompletion>,
    ) -> Self {
        self.completed_turn_input_claims
            .extend(completed_turn_input_claims);
        self
    }

    /// Marks one interrupted turn so store implementors atomically defer its unsettled active-turn
    /// inputs instead of losing or prematurely completing them.
    pub fn deferring_interrupted_turn_inputs(mut self, turn_id: impl Into<String>) -> Self {
        self.interrupted_turn_input_turn_id = Some(turn_id.into());
        self
    }

    /// Replaces the attachment-ID set that manifest implementors must promote atomically with this
    /// runtime commit; caller order is preserved.
    pub fn with_committed_attachments(
        mut self,
        attachment_ids: impl IntoIterator<Item = crate::AttachmentId>,
    ) -> Self {
        self.committed_attachment_ids = attachment_ids.into_iter().collect();
        self
    }
}

/// Build the exact identity-bearing append commit used by the runtime.
///
/// The perf harness needs this test-gated seam to isolate receipt derivation
/// and real backend publication without adding timing hooks to production. It
/// deliberately stops before the host-owned parts of the production sequence:
/// protocol-plugin mutation/rollback, live plugin-state stamping, the host clock
/// (this hook uses `SystemClock`), staged token-ledger merging, and fresh
/// session-execution-lease acquisition. Keep those divergences visible here when
/// the production append sequence changes.
#[cfg(any(test, feature = "testing"))]
#[doc(hidden)]
pub fn append_request_commit_for_testing(
    state: &mut crate::RuntimeSessionState,
    operation_id: &str,
    nodes: &[crate::SessionAppendNode],
    requested_ancestor_node_id: Option<&str>,
) -> Result<RuntimeCommit, StoreError> {
    append_request_commit_with_clock_for_testing(
        state,
        operation_id,
        nodes,
        requested_ancestor_node_id,
        &crate::SystemClock,
    )
}

fn remap_optional_node_id(node_id: &mut Option<String>, mapping: &[(String, String)]) {
    let Some(current) = node_id.as_mut() else {
        return;
    };
    if let Some((_, derived)) = mapping.iter().find(|(draft, _)| draft == current) {
        *current = derived.clone();
    }
}

fn persisted_session_state_from_head(
    head: SessionHead,
    checkpoint: Option<HydratedSessionCheckpoint>,
) -> Result<crate::RuntimeSessionState, StoreError> {
    let persisted_node_ids = head
        .graph
        .nodes
        .iter()
        .map(|node| node.node_id.clone())
        .collect();
    let graph = head.graph;
    let agent_frames = graph.agent_frame_records(&head.session_id);
    let mut state = crate::RuntimeSessionState {
        session_id: head.session_id,
        policy: crate::SessionPolicy::new(head.config.turn_budget),
        agent_frames,
        current_frame_node_id: head.current_frame_node_id,
        session_graph: graph,
        turn_index: 0,
        token_usage: crate::TokenUsage::default(),
        last_prompt_usage: None,
        protocol_turn_options: crate::ProtocolTurnOptions::default(),
        checkpoint_components: crate::runtime::state::RuntimeCheckpointComponents::unproven(),
        plugin_snapshot_revision: None,
        token_ledger: head.token_ledger,
        checkpoint_ref: head.checkpoint_ref.clone(),
        head_revision: head.head_revision,
        persisted_node_ids,
    };
    state.policy.model = head.config.model.clone();
    state.policy.provider_id = head.config.provider_id.clone();
    state.policy.prompt = head.config.prompt.clone();
    crate::runtime::state::apply_session_checkpoint(&mut state, checkpoint)?;
    Ok(state)
}

#[cfg(any(test, feature = "testing"))]
impl Default for SessionHead {
    fn default() -> Self {
        Self {
            session_id: default_root_session_id(),
            head_revision: 0,
            current_frame_node_id: None,
            graph: crate::SessionGraph::default(),
            config: crate::PersistedSessionConfig::new(crate::TurnBudget::Unbounded),
            checkpoint_ref: None,
            token_ledger: Vec::new(),
        }
    }
}

#[cfg(any(test, feature = "testing"))]
impl Default for SessionHeadPayload {
    fn default() -> Self {
        Self {
            schema_version: SESSION_HEAD_META_SCHEMA_VERSION,
            session_id: default_root_session_id(),
            config: crate::PersistedSessionConfig::new(crate::TurnBudget::Unbounded),
            current_frame_node_id: None,
        }
    }
}

/// Settled-session commit/read capability: the runtime's atomic transaction
/// facade for visible session state.
///
/// This segment owns session graph/head commits, checkpoint hydration and
/// usage, final turn-commit idempotency, session metadata, and the attachment
/// write-ahead manifest. Queued-work and turn-input *completions* also settle
/// here — [`commit_runtime_state`](Self::commit_runtime_state) consumes claims
/// granted by [`QueuedWorkStore`] and [`TurnInputStore`] in the same atomic
/// commit. In-flight nondeterministic work belongs to the active
/// [`EffectHost`](crate::EffectHost), not to the store contract.
///
/// The [`AttachmentManifest`] supertrait is required so the runtime can wrap
/// any persistence backend with a
/// [`SessionAttachmentStore`](crate::SessionAttachmentStore)
/// without dual-trait casting. Backends with no attachment-write story can
/// paste no-op manifest impls via
/// [`impl_noop_attachment_manifest!`](crate::impl_noop_attachment_manifest).
///
/// Checkpoint components have one backend-independent durable shape. When a
/// commit supplies a tool-state, plugin-snapshot, or execution-state body, the
/// backend must store it under a content ref and return that ref in
/// [`RuntimeCommitResult::manifest`]. A later commit may carry the ref without
/// the body to mean "unchanged"; the backend must resolve the existing body
/// when hydrating the checkpoint. A ref-only commit whose component is absent
/// must fail instead of persisting a checkpoint that hydrates to `None`.
#[async_trait::async_trait]
pub trait SessionCommitStore: AttachmentManifest + Send + Sync {
    async fn load_session(&self) -> Result<Option<PersistedSessionRead>, StoreError>;

    /// Read the current session head without hydrating graph, checkpoint, or
    /// usage history.
    ///
    /// Implementations must project this from at most one durable row. Runtime
    /// freshness checks depend on the revision, leaf, and checkpoint reference
    /// all being present in this read. The read must use the same session
    /// resolution and binding semantics as [`SessionCommitStore::load_session`]
    /// so the two projections agree about session presence. `Ok(None)` means
    /// resolution completed and found no readable session; inability to
    /// determine the head must be returned as `Err`, never collapsed to absence.
    async fn load_session_head_meta(&self) -> Result<Option<SessionHeadMeta>, StoreError>;

    async fn load_node(
        &self,
        node_id: &str,
    ) -> Result<Option<crate::SessionNodeRecord>, StoreError>;

    /// Atomically persist one settled runtime commit and its durable receipt.
    ///
    /// A commit carrying [`RuntimeCommit::session_execution_lease_fence`]
    /// borrows a turn driver's held lane without claiming, rotating, renewing,
    /// or releasing it. Implementors must validate that fence inside the write
    /// transaction and before receipt lookup, so stale or expired authority
    /// vetoes even an otherwise replayable operation identity.
    ///
    /// Implementors must look up the `(session_id, operation storage key)`
    /// receipt inside the write transaction before the fresh append ancestor
    /// fence and head-revision compare-and-swap. Existing receipts must be
    /// adjudicated with [`decide_runtime_commit_receipt`]: replay returns the
    /// stored first-attempt [`RuntimeCommitResult`] with only
    /// [`RuntimeCommitResult::receipt_replayed`] set transiently, applies none
    /// of the attempted commit envelope, and may release the attempt's explicit
    /// execution-lease completion. Conflicts and corrupt count cross-checks
    /// mutate nothing.
    ///
    /// Every [`RuntimeUsageDelta`] is published idempotently on `(session_id,
    /// operation_storage_key, entry_ordinal, payload_encoding_version,
    /// payload_hash)`, where the versioned hand-written projection is
    /// documented on [`RuntimeUsageDeltaIdentity`]. A duplicate full identity
    /// is a no-op inside this same transaction. Fresh results list every
    /// identity made durable by the commit in
    /// [`RuntimeCommitResult::committed_usage_delta_identities`]; stored
    /// receipt results retain the original attempt's list so callers do not
    /// clear staged rows that the original transaction never carried.
    ///
    /// A fresh identity-bearing append enforces
    /// [`RuntimeTurnCommitStamp::requested_ancestor_node_id`] against the
    /// transaction's active path, then atomically publishes graph, checkpoint,
    /// usage, queue/input settlements, attachment adoptions, and a receipt whose
    /// stored replay bit is `false`. Receipt lookup, fresh-only ancestor fencing,
    /// commit publication, and receipt insertion are one transaction.
    ///
    /// A queued-work completion that no longer owns a named row must return
    /// [`StoreError::QueuedWorkClaimSuperseded`] with its `row_id`. If another
    /// claim owns the live row, the error also carries its claim ID and lease
    /// generation so recovery preserves peer rows without weakening fencing.
    async fn commit_runtime_state(
        &self,
        commit: RuntimeCommit,
    ) -> Result<RuntimeCommitResult, StoreError>;

    /// Admit `binding.session_id` to this store and bind this handle to it.
    ///
    /// This is the authoritative durable admission seam for pre-opened stores,
    /// managed child sessions, and parked resume. `SessionStoreFactory::create_store`
    /// is a convenience that must produce the same admission decision.
    ///
    /// Implementations must atomically:
    ///
    /// 1. reject an empty id with [`StoreError::InvalidSessionId`];
    /// 2. reject a permanent tombstone with [`StoreError::SessionDeleted`];
    /// 3. reject a handle bound to another id with
    ///    [`StoreError::SessionBindingMismatch`];
    /// 4. create metadata exactly from `binding` when absent, without replacing
    ///    existing metadata, and return [`SessionAdmission::Created`];
    /// 5. leave an already-bound same-id session unchanged and return
    ///    [`SessionAdmission::Rebound`].
    async fn admit_and_bind_session(
        &self,
        binding: &SessionBinding,
    ) -> Result<SessionAdmission, StoreError>;

    async fn save_session_meta(&self, meta: SessionMeta) -> Result<(), StoreError>;
    async fn load_session_meta(&self) -> Result<Option<SessionMeta>, StoreError>;
}

/// Pending turn-input lifecycle capability: durable ingress for model-visible
/// user input.
///
/// Active-turn ingress is claimed only by the matching live turn at a
/// checkpoint. Next-turn ingress is claimed only by idle dispatch. User input
/// must not be represented as generic queued work. Claims granted here are
/// completed atomically by [`SessionCommitStore::commit_runtime_state`].
#[async_trait::async_trait]
pub trait TurnInputStore: Send + Sync {
    /// Persist model-visible user input into the pending turn-input lifecycle.
    async fn enqueue_pending_turn_input(
        &self,
        input: crate::PendingTurnInputDraft,
    ) -> Result<crate::PendingTurnInput, StoreError>;

    /// List pending user inputs available for reconciliation or queue preview.
    ///
    /// This excludes completed/cancelled rows and rows currently held by a live
    /// claim. Expired claims are visible again according to their state.
    async fn list_pending_turn_inputs(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::PendingTurnInput>, StoreError>;

    /// Read canonical input applications from durable turn-commit records.
    ///
    /// Unlike live observation replay, this surface is not retention-window
    /// dependent. Implementations return settled applications in durable
    /// commit order so a host can reconcile admission identity after a gap.
    async fn list_turn_input_applications(
        &self,
        _session_id: &str,
    ) -> Result<Vec<crate::TurnInputApplication>, StoreError> {
        Err(StoreError::Backend(
            "turn input application reconciliation is not implemented by this store".to_string(),
        ))
    }

    /// Cancel an unclaimed pending user input by id.
    ///
    /// Provided convenience: the singular form is exactly
    /// [`cancel_pending_turn_inputs`](Self::cancel_pending_turn_inputs) with a
    /// one-element target list, so backends implement only the plural
    /// primitive.
    async fn cancel_pending_turn_input(
        &self,
        session_id: &str,
        input_id: &str,
    ) -> Result<crate::PendingTurnInputCancelOutcome, StoreError> {
        let target = crate::PendingTurnInputCancelTarget::input_id(input_id);
        let targets = vec![target];
        let mut outcomes = self
            .cancel_pending_turn_inputs(session_id, &targets)
            .await?;
        Ok(outcomes
            .pop()
            .map(|result| result.outcome)
            .unwrap_or(crate::PendingTurnInputCancelOutcome::NotFound))
    }

    /// Atomically cancel a list of pending user inputs by input id or source key.
    async fn cancel_pending_turn_inputs(
        &self,
        session_id: &str,
        targets: &[crate::PendingTurnInputCancelTarget],
    ) -> Result<Vec<crate::PendingTurnInputCancelResult>, StoreError>;

    /// Atomically cancel the same-session runtime-admission suffix from an anchor.
    async fn cancel_pending_turn_input_suffix(
        &self,
        session_id: &str,
        anchor: &crate::PendingTurnInputCancelTarget,
    ) -> Result<crate::PendingTurnInputSuffixCancelOutcome, StoreError>;

    /// Claim active-turn input at a checkpoint for the live turn id.
    ///
    /// The claim pins the caller's live session-execution-lease generation
    /// (`session_execution_lease.fencing_token`) rather than a TTL; it is live
    /// exactly while that generation still holds the session lease (ADR 0029).
    async fn claim_active_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError>;

    /// Claim queued next-turn input at idle.
    async fn claim_next_turn_inputs(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        max_inputs: usize,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>, StoreError>;

    /// Abandon a held pending-turn-input claim so it can be reclaimed.
    async fn abandon_turn_input_claim(
        &self,
        claim: &crate::WorkClaim<crate::runtime::TurnInputClaimData>,
    ) -> Result<(), StoreError>;

    /// Release multiple held pending-turn-input claims in one backend batch.
    async fn abandon_turn_input_claims(
        &self,
        claims: &[crate::WorkClaim<crate::runtime::TurnInputClaimData>],
    ) -> Result<(), StoreError> {
        for claim in claims {
            self.abandon_turn_input_claim(claim).await?;
        }
        Ok(())
    }
}

/// Durable single-writer execution-lane capability, fenced by monotonic
/// fencing tokens.
#[async_trait::async_trait]
pub trait SessionExecutionLeaseStore: Send + Sync {
    /// Try to claim the durable single-writer execution lane for `session_id`.
    /// Returns [`SessionExecutionLeaseClaimOutcome::Busy`] when another owner
    /// holds an unexpired lease. Expired or released leases may be reclaimed
    /// and receive a higher fencing token. A live claim reenters only when its
    /// owner id, boot incarnation, and runtime-minted executor id all match;
    /// reentry rotates the lease token but preserves the fencing generation.
    /// Renewal never rotates either token. Any live mismatch is busy.
    ///
    /// A granted claim must carry
    /// [`SessionExecutionLeaseAcquisition::displaced`] naming the lapsed holder
    /// it took the lane from, read inside the same atomic operation. This is the
    /// only truthful report of a takeover: the displaced runner is frequently
    /// dead or frozen (that is why its lease lapsed), so nothing it would have
    /// logged is guaranteed to happen. A claim that displaced nobody, including
    /// exact owner/incarnation/executor reentry and a reclaim of a released row,
    /// reports `None`.
    /// `executor_id` is the caller's own runtime-open discriminator and must be
    /// supplied: it is identity, and a default that minted one per call would
    /// make reentry unreachable through this method while silently changing the
    /// claimant on every retry. Only the claim nonce - a per-attempt
    /// capability, not identity - is minted here, so a caller that needs one
    /// nonce across an ambiguous-outcome retry uses
    /// [`try_claim_session_execution_lease_with_token`](Self::try_claim_session_execution_lease_with_token)
    /// directly.
    async fn try_claim_session_execution_lease(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        executor_id: &str,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError> {
        let claim_nonce = LeaseClaimNonce::new();
        self.try_claim_session_execution_lease_with_token(
            session_id,
            owner,
            executor_id,
            &claim_nonce,
            lease_ttl_ms,
        )
        .await
    }

    /// Try one retry-safe claim attempt using an opaque claim nonce.
    ///
    /// The caller mints [`LeaseClaimNonce::new`] once for the logical claim and
    /// borrows it again only for an ambiguous-outcome retry. With no value-taking
    /// constructor, stable host identity cannot accidentally be
    /// reused as claim identity. Backends persist the nonce bytes as the lease
    /// token so a retry observes one settled rotation instead of rotating again.
    async fn try_claim_session_execution_lease_with_token(
        &self,
        session_id: &str,
        owner: &LeaseOwnerIdentity,
        executor_id: &str,
        claim_nonce: &LeaseClaimNonce,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLeaseClaimOutcome, StoreError>;

    /// Extend a live session execution lease owned by the caller.
    ///
    /// Backends reject expired authority with [`StoreError::SessionExecutionLeaseExpired`];
    /// stale, released, or superseded owner/token authority uses
    /// [`StoreError::SessionExecutionLeaseRenewalRefused`] with structured decision evidence.
    /// Granted renewals echo the presented session and owner, never rotate either
    /// token, and return expiry at least as late; core refuses install otherwise.
    async fn renew_session_execution_lease(
        &self,
        fence: &SessionExecutionLeaseAuthority,
        lease_ttl_ms: u64,
    ) -> Result<SessionExecutionLease, StoreError>;

    /// Release a session execution lease predicated on its owner and lease token.
    ///
    /// A stale, repeated, released, or superseded completion is refused with
    /// [`StoreError::SessionExecutionLeaseReleaseRefused`] and must not clear a
    /// successor lease. The fencing token remains generation evidence; lock
    /// lifecycle uses owner plus lease token. Named refusals record structured evidence.
    async fn release_session_execution_lease(
        &self,
        completion: &SessionExecutionLeaseAuthority,
    ) -> Result<(), StoreError>;

    /// Read the current session-execution-lease row without claiming it.
    ///
    /// Returns the persisted lease when an owner holds the row, or `None` when
    /// the row is absent, unleased, or released. The returned lease may already
    /// be expired: expiry is a raw fact exposed read-side, mirroring
    /// [`ProcessRegistry::get_process_lease`](crate::ProcessRegistry::get_process_lease),
    /// so callers classify staleness themselves. This never mutates the lease
    /// and never advances a generation. Unknown session ids return `None`.
    ///
    /// This read is diagnostics only. The commit CAS is the single authority on
    /// who may publish (ADR 0029); a backend must never let a caller substitute
    /// this snapshot for the fence it presents on claim, renew, or release.
    async fn get_session_execution_lease(
        &self,
        session_id: &str,
    ) -> Result<Option<SessionExecutionLease>, StoreError>;
}

/// Durable queued-work capability: ingress, ordered claiming, and claim leases
/// for non-input work (process wakes and session commands).
///
/// Claims granted here are completed atomically by
/// [`SessionCommitStore::commit_runtime_state`].
#[async_trait::async_trait]
pub trait QueuedWorkStore: Send + Sync {
    /// Persist a queued-work batch for later claiming.
    async fn enqueue_queued_work(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkBatch, StoreError>;

    /// Persist a queued-work batch and expose whether receiver idempotency
    /// absorbed it. The wake driver uses this for delivery evidence; stores
    /// with no richer receipt may retain the default inserted outcome.
    #[doc(hidden)]
    async fn enqueue_queued_work_with_outcome(
        &self,
        batch: crate::QueuedWorkBatchDraft,
    ) -> Result<crate::QueuedWorkEnqueueOutcome, StoreError> {
        self.enqueue_queued_work(batch)
            .await
            .map(crate::QueuedWorkEnqueueOutcome::Inserted)
    }

    /// Claim a leading ready session-command batch for `owner_id`.
    ///
    /// A command claim is returned only when the earliest ready claimable batch
    /// is classified as [`QueuedWorkClass::SessionCommand`].
    /// Backends derive the class from queued payloads; no schema column is
    /// required.
    async fn claim_leading_ready_session_command(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::QueuedWorkClaimData>>, StoreError>;

    /// Claim the next ready turn-work group for `owner_id`.
    ///
    /// A turn-work claim is returned only when the earliest ready claimable
    /// batch is classified as [`QueuedWorkClass::TurnWork`].
    /// Earlier ready session commands are not skipped and are never
    /// materialized as turn input.
    /// When the head belongs to an interrupted predecessor-generation claim,
    /// the successor reclaims exactly the rows carrying that durable claim id;
    /// later compatible rows wait for a subsequent claim.
    async fn claim_ready_queued_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<Option<crate::WorkClaim<crate::runtime::QueuedWorkClaimData>>, StoreError>;

    /// Claim both ingress families admitted at an active-turn checkpoint.
    ///
    /// Backends must probe durable store state before opening a write
    /// transaction. When either family is pending, both claims are granted in
    /// one write transaction after validating the session-execution fence once.
    #[allow(clippy::too_many_arguments)]
    async fn claim_checkpoint_work(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        turn_id: &crate::TurnId,
        checkpoint: crate::CheckpointKind,
        max_inputs: usize,
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<
        (
            Option<crate::WorkClaim<crate::runtime::TurnInputClaimData>>,
            Option<crate::WorkClaim<crate::runtime::QueuedWorkClaimData>>,
        ),
        StoreError,
    >;

    /// Claim a specific ready batch set selected from the durable queue.
    ///
    /// This is the host-facing counterpart to
    /// [`claim_ready_queued_work`](Self::claim_ready_queued_work): callers that
    /// project queued work into a UI can claim the exact batch ids they
    /// rendered instead of reconstructing authority from local draft state.
    ///
    /// This selection is intentionally allowed to bypass earlier unrelated
    /// ready work. The logical-turn driver uses it to reclaim an atomic outbox
    /// handoff immediately, preserving foreground frame-chain ordering.
    /// Requested ids are interpreted in durable `enqueue_seq` order. A claim
    /// returns their maximal physically contiguous prefix that satisfies the
    /// ordinary key/boundary/budget law; an unrequested physical row is a
    /// barrier, and requested rows after it remain queued.
    async fn claim_ready_queued_work_by_batch_ids(
        &self,
        session_id: &str,
        session_execution_lease: &SessionExecutionLeaseAuthority,
        owner: &LeaseOwnerIdentity,
        boundary: crate::QueuedWorkClaimBoundary,
        batch_ids: &[String],
        policy: crate::QueuedWorkClaimPolicy,
    ) -> Result<crate::SelectedQueuedWorkClaimOutcome, StoreError>;

    /// Release a held queued-work claim without completing it.
    async fn abandon_queued_work_claim(
        &self,
        claim: &crate::WorkClaim<crate::runtime::QueuedWorkClaimData>,
    ) -> Result<(), StoreError>;

    /// Release multiple queued-work claims in one backend batch.
    async fn abandon_queued_work_claims(
        &self,
        claims: &[crate::WorkClaim<crate::runtime::QueuedWorkClaimData>],
    ) -> Result<(), StoreError> {
        for claim in claims {
            self.abandon_queued_work_claim(claim).await?;
        }
        Ok(())
    }

    /// Remove an unclaimed queued-work batch from durable ingress.
    ///
    /// Returns the removed batch when cancellation won the race. Returns `None`
    /// when the batch is missing or currently held by a live claim; callers must
    /// treat that as "already claimed or completed" and must not restore any
    /// stale local draft state.
    async fn cancel_queued_work_batch(
        &self,
        session_id: &str,
        batch_id: &str,
    ) -> Result<Option<crate::QueuedWorkBatch>, StoreError>;

    /// List all queued-work batches for a session, including batches held by a
    /// live claim.
    async fn list_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;

    /// List queued-work batches that are still pending presentation/editing.
    ///
    /// This excludes batches currently held by a live claim. A claim counts as
    /// live only while the session-execution-lease generation it pins still
    /// holds the session lease; batches pinned to a superseded or released
    /// generation are pending again because they can be reclaimed or cancelled.
    ///
    /// This is a distinct required query, not a derivation of
    /// [`list_queued_work`](Self::list_queued_work): the two differ by
    /// claim-state filter, and backends answer each with its own query over
    /// claim rows rather than leaking claim state to callers for client-side
    /// filtering.
    async fn list_pending_queued_work(
        &self,
        session_id: &str,
    ) -> Result<Vec<crate::QueuedWorkBatch>, StoreError>;
}

/// Host-scheduled retention and garbage-collection capability over settled
/// state.
#[async_trait::async_trait]
pub trait StoreMaintenance: Send + Sync {
    /// Physically delete tombstoned graph-node rows and prune terminal
    /// pending-turn-input evidence rows. See [`VacuumReport`].
    async fn vacuum(&self) -> Result<VacuumReport, StoreError>;

    /// Delete blobs no longer reachable from any retained root.
    async fn gc_unreachable(&self) -> Result<GcReport, StoreError>;

    /// Seed the exact session-owned trigger-manifest artifact-ref namespace for
    /// deletion conformance and differential tests.
    ///
    /// Returns `false` when the backend has no artifact-ref namespace on this
    /// store surface (the in-memory runtime store is such a backend).
    #[doc(hidden)]
    async fn seed_session_trigger_manifest_ref_for_testing(
        &self,
        session_id: &str,
    ) -> Result<bool, StoreError>;

    /// Return session-owned artifact-ref identities through this retained store
    /// handle. Values are `(namespace, artifact_ref)` pairs; physical pointer
    /// and body representations are deliberately excluded.
    #[doc(hidden)]
    async fn raw_session_owned_artifact_refs_for_testing(
        &self,
        session_id: &str,
    ) -> Result<Vec<(String, String)>, StoreError>;
}

/// Exact settled-session persistence protocol required by the runtime.
///
/// `Arc<dyn RuntimePersistence>` is *the* runtime storage handle: one object
/// implementing every persistence capability segment —
/// [`SessionCommitStore`] (atomic graph/head commits, reads, metadata, and the
/// attachment write-ahead manifest), [`TurnInputStore`] (pending turn-input
/// lifecycle), [`QueuedWorkStore`] (queued-work ingress and claiming),
/// [`SessionExecutionLeaseStore`] (single-writer execution lane), and
/// [`StoreMaintenance`] (vacuum/GC). The segments share one transactional
/// domain: claims granted by the input and queue segments settle atomically in
/// [`SessionCommitStore::commit_runtime_state`]. In-flight nondeterministic
/// work belongs to the active [`EffectHost`](crate::EffectHost), not to the
/// store contract.
///
/// Blanket-implemented for every type that implements all five segments;
/// backends implement the segment traits and never this trait directly.
pub trait RuntimePersistence:
    SessionCommitStore
    + TurnInputStore
    + SessionExecutionLeaseStore
    + QueuedWorkStore
    + StoreMaintenance
{
}

impl<T> RuntimePersistence for T where
    T: SessionCommitStore
        + TurnInputStore
        + SessionExecutionLeaseStore
        + QueuedWorkStore
        + StoreMaintenance
        + ?Sized
{
}

#[cfg(test)]
mod tests;
