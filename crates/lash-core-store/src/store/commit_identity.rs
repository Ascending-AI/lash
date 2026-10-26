//! Durable commit-operation identity: the append-request receipt hash, the
//! whole-commit intent hash, and history node id derivation.
//!
//! The three families minted here (`lash.append-request`, `lash.intent`,
//! `lash.history-node`) are grandfathered frozen-unframed families: their
//! preimages are built with `IdentityEncoder` but carry no framing header,
//! because the minted digests are persisted equality-compared evidence that
//! predates the framed identity kit. ADR 0097 is the authority; the golden
//! corpora in this module pin the exact bytes.

/// version_surface = "coexist"
/// version_guard(items(LASH_APPEND_REQUEST_DOMAIN_VERSION, append_request_identity_hash))
const LASH_APPEND_REQUEST_DOMAIN_VERSION: &str = "lash-append-request/v2";

/// version_surface = "coexist"
/// version_guard(items(LASH_INTENT_DOMAIN_VERSION, turn_commit_hash))
const LASH_INTENT_DOMAIN_VERSION: &str = "lash-intent/v2";

/// version_surface = "coexist"
/// version_guard(items(LASH_HISTORY_NODE_DOMAIN_VERSION, derive_history_node_id))
const LASH_HISTORY_NODE_DOMAIN_VERSION: &str = "lash-history-node/v3";

use super::*;
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::TurnId;

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

/// Encoding version of the append-request identity bytes a retried commit must
/// reproduce exactly.
///
/// version_guard(
///     items(
///         path = "crates/lash-core-store/src/store/commit_identity.rs",
///         path = "crates/lash-core-ids/src/stable_identity.rs", push_json_value, push_message_role,
///         push_message_origin, push_attachment_type_metadata, push_attachment_ref, push_part_kind, push_part, append_node_identity_bytes,
///         append_request_identity_bytes, new_unframed, tag, u8, u32, u64, i64, string, bytes,
///         optional, sequence, finish, provider_route,
///     ),
/// )
/// version_surface = "coexist"
/// format_manifest = "AppendRequestIdentity"
pub const APPEND_REQUEST_IDENTITY_ENCODING_VERSION: u32 = 7;

/// Frozen durable-identity family domains minted by this module (ADR 0097).
/// These are `FAMILY_DOMAINS`-registered names whose preimages carry no
/// framing header: the digests they produce are persisted opaque evidence
/// compared by exact equality, so the grammar is frozen byte-for-byte. The
/// `lash-*/vN` hash labels are registered separately in `BLAKE3_DOMAINS`.
const APPEND_REQUEST_IDENTITY_DOMAIN: &str = "lash.append-request";
const TURN_COMMIT_IDENTITY_DOMAIN: &str = "lash.intent";
const HISTORY_NODE_IDENTITY_DOMAIN: &str = "lash.history-node";

/// Shared backend-independent decision for an existing runtime commit receipt.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
/// Backends perform their own transactional reads and writes, but must use this
/// decision so exact-hash, append-identity, encoding-version, and node-count
/// precedence cannot drift between implementations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeCommitReceiptDecision {
    Replay,
    /// The append operation id was reused for different semantic request content.
    AppendIdentityConflict,
    /// The semantic-boundary operation id was reused for different canonical
    /// request content.
    SemanticBoundaryIdentityConflict,
    /// The receipt has no comparable append identity and its commit hash differs.
    RuntimeCommitConflict,
    /// Matching receipt evidence carries contradictory requested-node counts.
    CorruptRequestedNodeCount {
        /// Requested-node count stored with the first-attempt receipt.
        stored: u64,
        /// Requested-node count supplied by the attempted append stamp.
        attempted: u64,
    },
}

/// Exact commit hashes retain legacy replay precedence only after any
/// comparable append identity agrees. When both exact-hash receipts carry a
/// requested-node count, the count must agree. Otherwise,
/// matching append identities replay only at the same encoding version and only
/// when their contracted node-count cross-check agrees. Missing count metadata
/// on an identity-bearing receipt is corruption; legacy receipts have no
/// comparable identity and continue through exact-hash semantics.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
pub fn decide_runtime_commit_receipt(
    stored_commit_hash: &str,
    attempted_commit_hash: &str,
    stored_identity: &AppendRequestIdentity,
    attempted_identity: &AppendRequestIdentity,
) -> RuntimeCommitReceiptDecision {
    if stored_commit_hash == attempted_commit_hash {
        if let (
            AppendRequestIdentity::Append {
                encoding_version: stored_version,
                request_hash: stored_hash,
                ..
            },
            AppendRequestIdentity::Append {
                encoding_version: attempted_version,
                request_hash: attempted_hash,
                ..
            },
        ) = (stored_identity, attempted_identity)
            && stored_version == attempted_version
            && stored_hash != attempted_hash
        {
            return RuntimeCommitReceiptDecision::AppendIdentityConflict;
        } else if let (
            AppendRequestIdentity::Append {
                requested_node_count: stored_count,
                ..
            },
            AppendRequestIdentity::Append {
                requested_node_count: attempted_count,
                ..
            },
        ) = (stored_identity, attempted_identity)
            && stored_count != attempted_count
        {
            return RuntimeCommitReceiptDecision::CorruptRequestedNodeCount {
                stored: *stored_count,
                attempted: *attempted_count,
            };
        } else if let (
            AppendRequestIdentity::SemanticBoundary {
                operation: stored_operation,
                encoding_version: stored_version,
                request_hash: stored_hash,
            },
            AppendRequestIdentity::SemanticBoundary {
                operation: attempted_operation,
                encoding_version: attempted_version,
                request_hash: attempted_hash,
            },
        ) = (stored_identity, attempted_identity)
            && stored_operation == attempted_operation
            && stored_version == attempted_version
            && stored_hash != attempted_hash
        {
            return RuntimeCommitReceiptDecision::SemanticBoundaryIdentityConflict;
        }
        return RuntimeCommitReceiptDecision::Replay;
    }

    if let (
        AppendRequestIdentity::Append {
            encoding_version: stored_version,
            request_hash: stored_hash,
            requested_node_count: stored_count,
            ..
        },
        AppendRequestIdentity::Append {
            encoding_version: attempted_version,
            request_hash: attempted_hash,
            requested_node_count: attempted_count,
            ..
        },
    ) = (stored_identity, attempted_identity)
        && stored_version == attempted_version
    {
        if stored_hash != attempted_hash {
            return RuntimeCommitReceiptDecision::AppendIdentityConflict;
        }
        if stored_count != attempted_count {
            return RuntimeCommitReceiptDecision::CorruptRequestedNodeCount {
                stored: *stored_count,
                attempted: *attempted_count,
            };
        }
        return RuntimeCommitReceiptDecision::Replay;
    }

    if let (
        AppendRequestIdentity::SemanticBoundary {
            operation: stored_operation,
            encoding_version: stored_version,
            request_hash: stored_hash,
        },
        AppendRequestIdentity::SemanticBoundary {
            operation: attempted_operation,
            encoding_version: attempted_version,
            request_hash: attempted_hash,
        },
    ) = (stored_identity, attempted_identity)
        && stored_operation == attempted_operation
        && stored_version == attempted_version
    {
        // The semantic-boundary answer to "same request retried?": a rebuilt
        // commit whose canonical request matches replays even after the head
        // has advanced; a differing canonical encoding is refused, never
        // silently deduplicated.
        if stored_hash != attempted_hash {
            return RuntimeCommitReceiptDecision::SemanticBoundaryIdentityConflict;
        }
        return RuntimeCommitReceiptDecision::Replay;
    }

    RuntimeCommitReceiptDecision::RuntimeCommitConflict
}

pub(super) fn validate_receipt_identity(commit: &RuntimeCommit) -> Result<(), StoreError> {
    let completed = &commit.turn_commit;
    match &completed.append_request_identity {
        AppendRequestIdentity::PlainCommit => Ok(()),
        AppendRequestIdentity::Append { .. } => {
            if completed.operation.key != "append-session-nodes" {
                return Err(StoreError::Backend(format!(
                    "append receipt identity metadata is invalid for operation `{}`",
                    completed.operation.key
                )));
            }
            Ok(())
        }
        AppendRequestIdentity::SemanticBoundary {
            operation,
            encoding_version,
            request_hash,
        } => {
            if completed.operation.key != operation.operation_key() {
                return Err(StoreError::Backend(format!(
                    "semantic-boundary receipt identity `{}` is invalid for operation `{}`",
                    operation.operation_key(),
                    completed.operation.key
                )));
            }
            super::semantic_boundary::validate_semantic_boundary_commit_is_pure(commit)?;
            let (expected_version, expected_hash) =
                super::semantic_boundary::semantic_boundary_request_identity(commit, *operation)?;
            if *encoding_version != expected_version || *request_hash != expected_hash {
                return Err(StoreError::Backend(format!(
                    "semantic-boundary receipt identity does not match the canonical `{}` request encoding",
                    operation.operation_key()
                )));
            }
            Ok(())
        }
    }
}

/// Tagged-binary JSON grammar frozen into the append-request preimage (ADR
/// 0097). This predates `identity_json`'s normalized serde leaf and produces
/// different bytes — it type-tags every level and keeps `i64`/`u64`/`f64`
/// numbers distinct without leaning on serde_json's number rendering — so the
/// two encodings cannot be unified without moving the minted digests. The
/// golden corpora below pin the exact byte grammar.
fn push_json_value(
    identity: &mut crate::stable_identity::IdentityEncoder,
    value: &serde_json::Value,
) -> Result<(), StoreError> {
    match value {
        serde_json::Value::Null => identity.tag(0),
        serde_json::Value::Bool(false) => identity.tag(1),
        serde_json::Value::Bool(true) => identity.tag(2),
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                identity.tag(3);
                identity.i64(value);
            } else if let Some(value) = number.as_u64() {
                identity.tag(4);
                identity.u64(value);
            } else if let Some(value) = number.as_f64() {
                identity.tag(5);
                identity.u64(value.to_bits());
            } else {
                return Err(StoreError::Backend(format!(
                    "append identity cannot encode JSON number `{number}`"
                )));
            }
        }
        serde_json::Value::String(value) => {
            identity.tag(6);
            identity.string(value);
        }
        serde_json::Value::Array(values) => {
            identity.tag(7);
            identity.u64(values.len() as u64);
            for value in values {
                push_json_value(identity, value)?;
            }
        }
        serde_json::Value::Object(values) => {
            identity.tag(8);
            identity.u64(values.len() as u64);
            let mut fields = values.iter().collect::<Vec<_>>();
            fields.sort_unstable_by_key(|(name, _)| *name);
            for (name, value) in fields {
                identity.string(name);
                push_json_value(identity, value)?;
            }
        }
    }
    Ok(())
}

fn push_message_role(
    identity: &mut crate::stable_identity::IdentityEncoder,
    role: crate::MessageRole,
) {
    identity.tag(match role {
        crate::MessageRole::User => 0,
        crate::MessageRole::Assistant => 1,
        crate::MessageRole::System => 2,
        crate::MessageRole::Event => 3,
    });
}

/// A host-appended origin this encoder generation has no projection for is
/// refused rather than hashed by a guess: `MessageOrigin` is
/// `#[non_exhaustive]`, and a new variant needs its own tag here.
fn push_message_origin(
    identity: &mut crate::stable_identity::IdentityEncoder,
    origin: &crate::MessageOrigin,
) -> Result<(), StoreError> {
    match origin {
        crate::MessageOrigin::Plugin {
            plugin_id,
            transient,
        } => {
            identity.tag(0);
            identity.string(plugin_id);
            identity.u8(u8::from(*transient));
        }
        // Tag 1 is retired with the process-wake origin.
        crate::MessageOrigin::TurnInput { turn_id, input_id } => {
            identity.tag(2);
            identity.string(turn_id);
            identity.optional(input_id.as_ref(), |identity, value| identity.string(value));
        }
        crate::MessageOrigin::TurnOutput {
            turn_id,
            source,
            cell_id,
        } => {
            identity.tag(3);
            identity.string(turn_id);
            match source {
                crate::TurnOutputSource::Runtime => identity.tag(0),
                crate::TurnOutputSource::Plugin { plugin_id } => {
                    identity.tag(1);
                    identity.string(plugin_id);
                }
            }
            identity.optional(cell_id.as_ref(), |identity, value| identity.string(value));
        }
        unprojected => {
            return Err(StoreError::Backend(format!(
                "append request identity has no projection for message origin {unprojected:?}"
            )));
        }
    }
    Ok(())
}

fn push_attachment_type_metadata(
    identity: &mut crate::stable_identity::IdentityEncoder,
    metadata: &crate::AttachmentTypeMetadata,
) {
    match metadata {
        crate::AttachmentTypeMetadata::Image { width, height } => {
            identity.tag(0);
            identity.optional(width.as_ref(), |identity, value| identity.u32(*value));
            identity.optional(height.as_ref(), |identity, value| identity.u32(*value));
        }
    }
}

fn push_attachment_ref(
    identity: &mut crate::stable_identity::IdentityEncoder,
    attachment: &crate::AttachmentRef,
) {
    let crate::AttachmentRef {
        id,
        media_type,
        byte_len,
        type_metadata,
        label,
    } = attachment;
    identity.string(id.as_str());
    identity.string(media_type.as_str());
    identity.u64(*byte_len);
    identity.optional(type_metadata.as_ref(), push_attachment_type_metadata);
    identity.optional(label.as_ref(), |identity, value| identity.string(value));
}

fn push_part_kind(identity: &mut crate::stable_identity::IdentityEncoder, kind: crate::PartKind) {
    identity.tag(lash_protocol_types::core_support::fold_part_kind(
        kind,
        [0, 1, 2, 3, 4, 5, 6, 7, 8],
    ));
}

fn push_part(identity: &mut crate::stable_identity::IdentityEncoder, part: &crate::Part) {
    identity.string(part.id());
    push_part_kind(identity, part.kind());
    if let Some(status) = part.tool_result_status() {
        identity.tag(match status {
            lash_protocol_types::ToolCallStatus::Success => 0,
            lash_protocol_types::ToolCallStatus::Failure => 1,
            lash_protocol_types::ToolCallStatus::Cancelled => 2,
        });
    }
    // A tool result's content is its ordered blocks; every other kind's is
    // its text.
    match part.tool_result_content() {
        Some(blocks) => identity.sequence(blocks, |identity, block| match block {
            lash_protocol_types::ModelToolReturnPart::Text { text } => {
                identity.tag(0);
                identity.string(text);
            }
            lash_protocol_types::ModelToolReturnPart::Attachment(source) => {
                identity.tag(1);
                push_attachment_ref(identity, source);
            }
            // A retained output is its witness and the reference to its
            // bytes, never the bytes (FIG-1643).
            lash_protocol_types::ModelToolReturnPart::Retained(
                lash_protocol_types::RetainedOutput { reference, witness },
            ) => {
                identity.tag(2);
                push_attachment_ref(identity, reference);
                identity.string(witness);
            }
        }),
        None => identity.string(&part.content()),
    }
    identity.optional(part.attachment(), |identity, attachment| {
        let lash_protocol_types::PartAttachment { reference } = attachment;
        push_attachment_ref(identity, reference)
    });
    identity.optional(part.call_id(), |identity, value| {
        identity.string(value.as_str())
    });
    identity.optional(part.provider_call_id(), |identity, value| {
        identity.string(value)
    });
    identity.optional(part.tool_name(), |identity, value| identity.string(value));
    identity.optional(part.tool_replay(), |identity, replay| {
        let lash_protocol_types::llm::types::ProviderReplayMeta {
            item_id,
            opaque,
            origin,
        } = replay;
        identity.optional(item_id.as_ref(), |identity, value| identity.string(value));
        identity.optional(opaque.as_ref(), |identity, value| identity.string(value));
        identity.optional(origin.as_ref(), crate::stable_identity::provider_route);
    });
    identity.optional(part.reasoning_meta(), |identity, replay| {
        let lash_protocol_types::llm::types::ProviderReasoningReplay {
            item_id,
            encrypted_content,
            signature,
            redacted,
            summary,
            origin,
        } = replay;
        identity.optional(item_id.as_ref(), |identity, value| identity.string(value));
        identity.optional(encrypted_content.as_ref(), |identity, value| {
            identity.string(value)
        });
        identity.optional(signature.as_ref(), |identity, value| identity.string(value));
        identity.u8(u8::from(*redacted));
        identity.sequence(summary, |identity, value| identity.string(value));
        identity.optional(origin.as_ref(), crate::stable_identity::provider_route);
    });
    identity.optional(part.response_meta(), |identity, response| {
        let lash_protocol_types::llm::types::ResponseTextMeta {
            id,
            status,
            phase,
            provider_payload,
            origin,
        } = response;
        for value in [
            id.as_deref(),
            status.as_deref(),
            (*phase).map(|phase| phase.as_str()),
            provider_payload.as_deref(),
        ] {
            identity.optional(value, crate::stable_identity::IdentityEncoder::string);
        }
        identity.optional(origin.as_ref(), crate::stable_identity::provider_route);
    });
}

fn append_node_identity_bytes(node: &crate::SessionAppendNode) -> Result<Vec<u8>, StoreError> {
    let mut identity =
        crate::stable_identity::IdentityEncoder::new_unframed(APPEND_REQUEST_IDENTITY_DOMAIN);
    match node {
        crate::SessionAppendNode::Message { message } => {
            let crate::PluginMessage {
                id,
                role,
                origin,
                parts,
            } = message;
            identity.tag(0);
            identity.optional(id.as_ref(), |identity, value| identity.string(value));
            push_message_role(&mut identity, *role);
            match origin {
                None => identity.tag(0),
                Some(origin) => {
                    identity.tag(1);
                    push_message_origin(&mut identity, origin)?;
                }
            }
            identity.sequence(parts, push_part);
        }
        crate::SessionAppendNode::ProtocolEvent { event } => {
            let crate::ProtocolEvent { plugin_id, payload } = event;
            identity.tag(1);
            identity.string(plugin_id);
            push_json_value(&mut identity, payload)?;
        }
        crate::SessionAppendNode::Plugin { plugin_type, body } => {
            identity.tag(2);
            identity.string(plugin_type);
            push_json_value(&mut identity, body)?;
        }
    }
    Ok(identity.finish())
}

/// Canonical request bytes, in order:
///
/// 1. operation storage key: `u64` big-endian UTF-8 byte length, then bytes;
/// 2. requested ancestor: one byte (`0` for absent, `1` for present), followed
///    when present by its `u64` big-endian UTF-8 byte length and bytes;
/// 3. ordered semantic nodes: `u64` big-endian node count, then for each node
///    its hand-written tagged projection for the selected encoding generation,
///    framed by a `u64` big-endian length.
///
/// No domain string, encoding version, node id, timestamp, head, or other
/// environmental value is included. The version lives beside the digest in
/// the receipt so a future encoder can fall back to exact commit hashes.
///
/// The preimage is minted through `IdentityEncoder::new_unframed`: the family
/// predates the framed-identity kit and its digest bytes are frozen
/// equality-compared evidence, so the header can never be added (ADR 0097).
fn append_request_identity_bytes(
    operation: &OperationId,
    requested_ancestor_node_id: Option<&str>,
    nodes: &[crate::SessionAppendNode],
) -> Result<Vec<u8>, StoreError> {
    let operation_key = operation.storage_key()?;
    let mut identity =
        crate::stable_identity::IdentityEncoder::new_unframed(APPEND_REQUEST_IDENTITY_DOMAIN);
    identity.bytes(operation_key.as_bytes());
    identity.optional(requested_ancestor_node_id, |identity, ancestor| {
        identity.string(ancestor)
    });
    identity.u64(nodes.len() as u64);
    for node in nodes {
        let semantic_node = append_node_identity_bytes(node)?;
        identity.bytes(&semantic_node);
    }
    Ok(identity.finish())
}

pub(super) fn append_request_identity_hash(
    operation: &OperationId,
    requested_ancestor_node_id: Option<&str>,
    nodes: &[crate::SessionAppendNode],
) -> Result<String, StoreError> {
    Ok(crate::stable_hash::blake3_hex(
        LASH_APPEND_REQUEST_DOMAIN_VERSION,
        &append_request_identity_bytes(operation, requested_ancestor_node_id, nodes)?,
    ))
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)] // FIG-2971: test module is a host; ambient fs/env/process access is sanctioned
mod append_request_identity_tests {
    use super::*;

    fn operation(id: &str) -> OperationId {
        OperationId::new(
            crate::ExecutionScope::runtime_operation(format!("session:root:boundary:{id}")),
            "append-session-nodes",
        )
    }

    /// FIG-6518: a retry changing only the terminal fact is changed content.
    #[test]
    fn append_request_identity_distinguishes_tool_result_status() {
        let operation = operation("tool-result-status");
        let hashes = [
            lash_protocol_types::ToolCallStatus::Success,
            lash_protocol_types::ToolCallStatus::Failure,
            lash_protocol_types::ToolCallStatus::Cancelled,
        ]
        .map(|status| {
            let node = crate::SessionAppendNode::message(crate::PluginMessage {
                id: None,
                role: crate::MessageRole::User,
                origin: None,
                parts: vec![crate::Part::tool_result(
                    "result".into(),
                    vec![lash_protocol_types::ModelToolReturnPart::text("same body")],
                    lash_protocol_types::ToolCallId::fixture("call"),
                    "tool".into(),
                    status,
                )],
            });
            append_request_identity_hash(&operation, None, &[node]).expect("request identity")
        });
        for (index, hash) in hashes.iter().enumerate() {
            for other in &hashes[index + 1..] {
                assert_ne!(hash, other, "different terminal facts must not deduplicate");
            }
        }
    }
}

impl OperationId {
    pub fn new(scope: crate::ExecutionScope, key: impl Into<String>) -> Self {
        Self {
            scope,
            key: key.into(),
        }
    }

    /// Constructs a turn-scoped idempotency identity for store implementors, binding the operation
    /// key to both session and turn IDs.
    pub fn turn(
        session_id: impl Into<SessionId>,
        turn_id: impl Into<TurnId>,
        key: impl Into<String>,
    ) -> Self {
        Self::new(crate::ExecutionScope::turn(session_id, turn_id), key)
    }

    /// Derives the canonical durable idempotency key for store implementors and rejects operation
    /// components that cannot be encoded safely.
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

    pub fn turn_id(&self) -> Option<&TurnId> {
        self.scope.turn_id()
    }
}

fn failure_evidence_is_empty(evidence: &&[crate::TurnFailureEvidence]) -> bool {
    evidence.is_empty()
}

#[derive(serde::Serialize)]
struct RuntimeCommitIntent<'a> {
    session_id: &'a SessionId,
    config: &'a crate::PersistedSessionConfig,
    graph: GraphCommitIntent<'a>,
    checkpoint: CheckpointIntent<'a>,
    #[serde(skip_serializing_if = "failure_evidence_is_empty")]
    failure_evidence: &'a [crate::TurnFailureEvidence],
    #[serde(skip_serializing_if = "Option::is_none")]
    outcome: Option<&'a super::TurnCommitOutcome>,
    #[serde(skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    command_outcomes: &'a std::collections::BTreeMap<
        lash_vocabulary::identity::BatchId,
        crate::SessionCommandOutcome,
    >,
    completed_queue_batches: Vec<CompletedQueueIntent<'a>>,
    completed_turn_inputs: Vec<CompletedTurnInputIntent<'a>>,
    /// Admitted input a commit hands back or drops (FIG-3531, FIG-3927):
    /// the rows of [`IngressSettlement::released`](super::IngressSettlement)
    /// and `dropped`. Absent from every other commit's identity.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    undelivered_turn_inputs: Vec<&'a lash_vocabulary::identity::InputId>,
    /// Admitted wakes a commit hands back (FIG-3543, FIG-3927). Absent from
    /// every other commit's identity.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    undelivered_queue_batches: Vec<&'a lash_vocabulary::identity::BatchId>,
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
        let append = &commit.graph;
        let graph = GraphCommitIntent {
            nodes: append.nodes().iter().map(SessionNodeIntent::from).collect(),
            leaf_node_id: append
                .leaf_node_id()
                .or(commit.graph_base_leaf_node_id.as_ref())
                .map(|node_id| node_id.as_str()),
        };
        Self {
            session_id: &commit.session_id,
            config: commit.execution_config.as_deref().unwrap_or(&commit.config),
            graph,
            checkpoint: CheckpointIntent::from(&commit.checkpoint),
            failure_evidence: &commit.failure_evidence,
            outcome: commit.outcome.as_ref(),
            command_outcomes: &commit.command_outcomes,
            completed_queue_batches: commit
                .ingress
                .iter()
                .flat_map(|ingress| ingress.completed_batches.iter())
                .chain(commit.applied_commands.iter())
                .map(CompletedQueueIntent::from)
                .collect(),
            completed_turn_inputs: commit
                .ingress
                .iter()
                .flat_map(|ingress| ingress.completed_inputs.iter())
                .map(CompletedTurnInputIntent::from)
                .collect(),
            undelivered_turn_inputs: undelivered_rows(commit)
                .filter_map(|row| match row {
                    super::IngressRowId::Input(input) => Some(input),
                    super::IngressRowId::Batch(_) => None,
                })
                .collect(),
            undelivered_queue_batches: undelivered_rows(commit)
                .filter_map(|row| match row {
                    super::IngressRowId::Batch(batch) => Some(batch),
                    super::IngressRowId::Input(_) => None,
                })
                .collect(),
            committed_attachment_ids: &commit.committed_attachment_ids,
        }
    }
}

#[derive(serde::Serialize)]
struct GraphCommitIntent<'a> {
    nodes: Vec<SessionNodeIntent<'a>>,
    leaf_node_id: Option<&'a str>,
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
    components: Vec<CheckpointComponentIntent<'a>>,
    /// The SessionExecution root: the graph's identity is its root's.
    execution: Option<BlobRef>,
}

/// A checkpoint component as intent: its key and the content hash of its
/// body. The encoding stamp is write metadata the fleet selects, not intent
/// (FIG-4262).
#[derive(serde::Serialize)]
struct CheckpointComponentIntent<'a> {
    key: &'a str,
    blob_ref: Option<BlobRef>,
}

impl<'a> From<&'a HydratedSessionCheckpoint> for CheckpointIntent<'a> {
    fn from(checkpoint: &'a HydratedSessionCheckpoint) -> Self {
        Self {
            turn_state: &checkpoint.turn_state,
            components: checkpoint
                .components
                .iter()
                .map(|(key, component)| {
                    let blob_ref = match component {
                        HydratedCheckpointComponent::Changed { body_ref, .. } => {
                            Some(body_ref.clone())
                        }
                        HydratedCheckpointComponent::Unchanged { descriptor } => {
                            Some(descriptor.blob_ref.clone())
                        }
                        HydratedCheckpointComponent::Hydrated { body, .. } => {
                            let blob_ref = BlobRef::for_content(body);
                            #[cfg(feature = "perf-witness")]
                            crate::perf_witness::record_hash_pass(body.len());
                            Some(blob_ref)
                        }
                    };
                    CheckpointComponentIntent { key, blob_ref }
                })
                .collect(),
            execution: checkpoint
                .execution
                .as_ref()
                .map(super::CheckpointExecution::root_ref),
        }
    }
}

/// The admitted rows a commit hands back or drops, in settlement order. The
/// settling run and the disposition are not identity: the run is fixed by
/// the operation's scope and the disposition by the cancellation evidence.
fn undelivered_rows(commit: &RuntimeCommit) -> impl Iterator<Item = &super::IngressRowId> {
    commit
        .ingress
        .iter()
        .flat_map(|ingress| ingress.released.iter().chain(ingress.dropped.iter()))
}

#[derive(serde::Serialize)]
struct CompletedQueueIntent<'a> {
    session_id: &'a SessionId,
    batch_ids: &'a [lash_vocabulary::identity::BatchId],
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
    session_id: &'a SessionId,
    input_ids: &'a [lash_vocabulary::identity::InputId],
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

/// Whole-commit hash. The intent projection round-trips through a fixed-shape
/// `Value` tree whose map keys are already canonically ordered, so
/// `identity_json`'s normalization for caller-supplied `Value` trees has
/// nothing to reduce here — and swapping leaf encodings would move the minted
/// digest regardless. The serialized leaf is framed `len || bytes` into the
/// frozen `lash.intent` preimage (ADR 0097).
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
    let mut identity =
        crate::stable_identity::IdentityEncoder::new_unframed(TURN_COMMIT_IDENTITY_DOMAIN);
    identity.bytes(encoded.as_bytes());
    Ok(crate::stable_hash::blake3_hex(
        LASH_INTENT_DOMAIN_VERSION,
        &identity.finish(),
    ))
}

pub fn derive_history_node_id(
    session_id: &SessionId,
    operation: &OperationId,
    ordinal: u64,
) -> Result<lash_vocabulary::identity::NodeId, StoreError> {
    let preimage = history_node_preimage(session_id, operation, ordinal)?;
    Ok(lash_vocabulary::identity::NodeId::prefixed(
        "n_",
        crate::stable_hash::blake3_hex(LASH_HISTORY_NODE_DOMAIN_VERSION, &preimage),
    ))
}

fn history_node_preimage(
    session_id: &SessionId,
    operation: &OperationId,
    ordinal: u64,
) -> Result<Vec<u8>, StoreError> {
    let operation = serde_json::to_value(operation).map_err(|err| {
        StoreError::Backend(format!(
            "failed to serialize node operation identity: {err}"
        ))
    })?;
    let operation = crate::stable_hash::stable_json_string(&operation).map_err(|err| {
        StoreError::Backend(format!("failed to encode node operation identity: {err}"))
    })?;
    let mut identity =
        crate::stable_identity::IdentityEncoder::new_unframed(HISTORY_NODE_IDENTITY_DOMAIN);
    identity.bytes(session_id.as_bytes());
    identity.bytes(operation.as_bytes());
    identity.bytes(&ordinal.to_be_bytes());
    Ok(identity.finish())
}
