use super::*;

/// Stable caller-selected identity for one durable commit operation.
///
/// The scope names the ingress/effect boundary and `key` distinguishes
/// multiple commits within that scope. Both values are part of persisted
/// identity and must be reproduced byte-for-byte on retry.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct OperationId {
    pub scope: crate::ExecutionScope,
    pub key: String,
}

impl OperationId {
    pub fn new(scope: crate::ExecutionScope, key: impl Into<String>) -> Self {
        Self {
            scope,
            key: key.into(),
        }
    }

    pub fn turn(
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
        key: impl Into<String>,
    ) -> Self {
        Self::new(crate::ExecutionScope::turn(session_id, turn_id), key)
    }

    pub fn storage_key(&self) -> Result<String, StoreError> {
        let value = serde_json::to_value(self).map_err(|err| {
            StoreError::Backend(format!(
                "failed to serialize commit operation identity: {err}"
            ))
        })?;
        crate::stable_hash::stable_json_string(&value).map_err(|err| {
            StoreError::Backend(format!("failed to encode commit operation identity: {err}"))
        })
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.scope.turn_id()
    }
}

#[derive(serde::Serialize)]
struct RuntimeCommitIntent<'a> {
    session_id: &'a str,
    config: &'a crate::PersistedSessionConfig,
    current_frame_node_id: Option<&'a str>,
    graph: GraphCommitIntent<'a>,
    checkpoint: CheckpointIntent<'a>,
    usage_deltas: &'a [crate::TokenLedgerEntry],
    completed_queue_batches: Vec<CompletedQueueIntent<'a>>,
    completed_turn_inputs: Vec<CompletedTurnInputIntent<'a>>,
    enqueued_queue_batches: Vec<QueuedBatchIntent<'a>>,
    interrupted_turn_input_turn_id: Option<&'a str>,
    committed_attachment_ids: &'a [crate::AttachmentId],
}

/// Explicit allowlist for durable commit intent.
///
/// Topology, semantic payloads, source keys, turn state, config, settlement
/// targets, and attachment identities are included. Transport authority,
/// store-assigned facts, clock-derived values, claim/lease/fencing authority,
/// and host/plugin snapshot bytes are excluded. Divergence confined to those
/// excluded fields is invisible by design.
impl<'a> From<&'a RuntimeCommit> for RuntimeCommitIntent<'a> {
    fn from(commit: &'a RuntimeCommit) -> Self {
        Self {
            session_id: &commit.session_id,
            config: &commit.config,
            current_frame_node_id: commit.current_frame_node_id.as_deref(),
            graph: GraphCommitIntent::from(&commit.graph),
            checkpoint: CheckpointIntent::from(&commit.checkpoint),
            usage_deltas: &commit.usage_deltas,
            completed_queue_batches: commit
                .completed_queue_claims
                .iter()
                .map(CompletedQueueIntent::from)
                .collect(),
            completed_turn_inputs: commit
                .completed_turn_input_claims
                .iter()
                .map(CompletedTurnInputIntent::from)
                .collect(),
            enqueued_queue_batches: commit
                .enqueued_queue_batches
                .iter()
                .map(QueuedBatchIntent::from)
                .collect(),
            interrupted_turn_input_turn_id: commit.interrupted_turn_input_turn_id.as_deref(),
            committed_attachment_ids: &commit.committed_attachment_ids,
        }
    }
}

#[derive(serde::Serialize)]
struct GraphCommitIntent<'a> {
    nodes: Vec<SessionNodeIntent<'a>>,
    leaf_node_id: Option<&'a str>,
}

impl<'a> From<&'a GraphAppend> for GraphCommitIntent<'a> {
    fn from(graph: &'a GraphAppend) -> Self {
        Self {
            nodes: graph.nodes.iter().map(SessionNodeIntent::from).collect(),
            leaf_node_id: graph.leaf_node_id.as_deref(),
        }
    }
}

#[derive(serde::Serialize)]
struct SessionNodeIntent<'a> {
    node_id: &'a str,
    parent_node_id: Option<&'a str>,
    payload: &'a crate::SessionNodePayload,
}

impl<'a> From<&'a crate::SessionNodeRecord> for SessionNodeIntent<'a> {
    fn from(node: &'a crate::SessionNodeRecord) -> Self {
        Self {
            node_id: &node.node_id,
            parent_node_id: node.parent_node_id.as_deref(),
            payload: &node.payload,
        }
    }
}

#[derive(serde::Serialize)]
struct CheckpointIntent<'a> {
    turn_state: &'a crate::PersistedTurnState,
    tool_state_ref: &'a Option<BlobRef>,
    plugin_snapshot_ref: &'a Option<BlobRef>,
    plugin_snapshot_revision: Option<u64>,
    execution_state_ref: &'a Option<BlobRef>,
}

impl<'a> From<&'a HydratedSessionCheckpoint> for CheckpointIntent<'a> {
    fn from(checkpoint: &'a HydratedSessionCheckpoint) -> Self {
        Self {
            turn_state: &checkpoint.turn_state,
            tool_state_ref: &checkpoint.tool_state_ref,
            plugin_snapshot_ref: &checkpoint.plugin_snapshot_ref,
            plugin_snapshot_revision: checkpoint.plugin_snapshot_revision,
            execution_state_ref: &checkpoint.execution_state_ref,
        }
    }
}

#[derive(serde::Serialize)]
struct CompletedQueueIntent<'a> {
    session_id: &'a str,
    batch_ids: &'a [String],
}

impl<'a> From<&'a crate::QueuedWorkCompletion> for CompletedQueueIntent<'a> {
    fn from(completion: &'a crate::QueuedWorkCompletion) -> Self {
        Self {
            session_id: &completion.session_id,
            batch_ids: &completion.batch_ids,
        }
    }
}

#[derive(serde::Serialize)]
struct CompletedTurnInputIntent<'a> {
    session_id: &'a str,
    input_ids: &'a [String],
    applications: &'a [crate::TurnInputApplication],
}

impl<'a> From<&'a crate::TurnInputCompletion> for CompletedTurnInputIntent<'a> {
    fn from(completion: &'a crate::TurnInputCompletion) -> Self {
        Self {
            session_id: &completion.session_id,
            input_ids: &completion.input_ids,
            applications: &completion.applications,
        }
    }
}

#[derive(serde::Serialize)]
struct QueuedBatchIntent<'a> {
    session_id: &'a str,
    source_key: Option<&'a str>,
    delivery_policy: &'a crate::DeliveryPolicy,
    slot_policy: &'a crate::SlotPolicy,
    merge_key: &'a crate::MergeKey,
    payloads: Vec<QueuedPayloadIntent<'a>>,
}

impl<'a> From<&'a crate::QueuedWorkBatchDraft> for QueuedBatchIntent<'a> {
    fn from(batch: &'a crate::QueuedWorkBatchDraft) -> Self {
        Self {
            session_id: &batch.session_id,
            source_key: batch.source_key.as_deref(),
            delivery_policy: &batch.delivery_policy,
            slot_policy: &batch.slot_policy,
            merge_key: &batch.merge_key,
            payloads: batch
                .payloads
                .iter()
                .map(QueuedPayloadIntent::from)
                .collect(),
        }
    }
}

#[derive(serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum QueuedPayloadIntent<'a> {
    ProcessWake {
        wake_id: &'a str,
        target_session_id: &'a str,
        target_scope_id: &'a crate::SessionScopeId,
        process_id: &'a str,
        sequence: u64,
        event_type: &'a str,
        event_invocation: &'a crate::RuntimeInvocation,
        process_caused_by: &'a Option<crate::CausalRef>,
        dedupe_key: &'a str,
        input: &'a str,
    },
    AgentFrameTask {
        frame_id: &'a str,
        task: &'a str,
        protocol_turn_options: &'a Option<crate::ProtocolTurnOptions>,
    },
    SessionCommand {
        command: &'a crate::SessionCommand,
    },
}

impl<'a> From<&'a crate::QueuedWorkPayload> for QueuedPayloadIntent<'a> {
    fn from(payload: &'a crate::QueuedWorkPayload) -> Self {
        match payload {
            crate::QueuedWorkPayload::ProcessWake { wake } => Self::ProcessWake {
                wake_id: &wake.wake_id,
                target_session_id: &wake.target_session_id,
                target_scope_id: &wake.target_scope_id,
                process_id: &wake.process_id,
                sequence: wake.sequence,
                event_type: &wake.event_type,
                event_invocation: &wake.event_invocation,
                process_caused_by: &wake.process_caused_by,
                dedupe_key: &wake.dedupe_key,
                input: &wake.input,
            },
            crate::QueuedWorkPayload::AgentFrameTask {
                frame_id,
                task,
                protocol_turn_options,
            } => Self::AgentFrameTask {
                frame_id,
                task,
                protocol_turn_options,
            },
            crate::QueuedWorkPayload::SessionCommand { command } => {
                Self::SessionCommand { command }
            }
        }
    }
}

pub(super) fn turn_commit_hash(commit: &RuntimeCommit) -> Result<String, StoreError> {
    let projection = RuntimeCommitIntent::from(commit);
    let semantic_commit = serde_json::to_value(&projection).map_err(|err| {
        StoreError::Backend(format!("failed to serialize runtime turn commit: {err}"))
    })?;
    let encoded = crate::stable_hash::stable_json_string(&semantic_commit).map_err(|err| {
        StoreError::Backend(format!(
            "failed to serialize runtime turn commit hash: {err}"
        ))
    })?;
    Ok(domain_hash("lash-intent/v1", &[encoded.as_bytes()]))
}

fn domain_hash(domain: &str, components: &[&[u8]]) -> String {
    use sha2::Digest;

    let mut hasher = sha2::Sha256::new();
    for component in std::iter::once(domain.as_bytes()).chain(components.iter().copied()) {
        hasher.update((component.len() as u64).to_be_bytes());
        hasher.update(component);
    }
    format!("{:x}", hasher.finalize())
}

pub fn derive_history_node_id(
    incarnation_id: &crate::IncarnationId,
    operation: &OperationId,
    ordinal: u64,
) -> Result<String, StoreError> {
    let operation = serde_json::to_value(operation).map_err(|err| {
        StoreError::Backend(format!(
            "failed to serialize node operation identity: {err}"
        ))
    })?;
    let operation = crate::stable_hash::stable_json_string(&operation).map_err(|err| {
        StoreError::Backend(format!("failed to encode node operation identity: {err}"))
    })?;
    Ok(format!(
        "n_{}",
        domain_hash(
            "lash-history-node/v1",
            &[
                incarnation_id.as_str().as_bytes(),
                operation.as_bytes(),
                &ordinal.to_be_bytes(),
            ],
        )
    ))
}

pub fn graph_realization_digest(graph: &GraphAppend) -> String {
    let mut components = Vec::<Vec<u8>>::new();
    for node in &graph.nodes {
        components.push(node.node_id.as_bytes().to_vec());
        components.push(optional_digest_component(node.parent_node_id.as_deref()));
    }
    components.push(optional_digest_component(
        graph.leaf_node_id().map(String::as_str),
    ));
    let refs = components.iter().map(Vec::as_slice).collect::<Vec<_>>();
    domain_hash("lash-realization/v1", &refs)
}

fn optional_digest_component(value: Option<&str>) -> Vec<u8> {
    let mut encoded = Vec::with_capacity(value.map_or(1, |value| value.len() + 1));
    encoded.push(u8::from(value.is_some()));
    if let Some(value) = value {
        encoded.extend_from_slice(value.as_bytes());
    }
    encoded
}
