//! Durable commit-operation identity: the append-request receipt hash, the
//! whole-commit intent hash, and history node id derivation.
//!
//! The three families minted here (`lash.append-request`, `lash.intent`,
//! `lash.history-node`) are grandfathered frozen-unframed families: their
//! preimages are built with `IdentityEncoder` but carry no framing header,
//! because the minted digests are persisted equality-compared evidence that
//! predates the framed identity kit. ADR 0097 is the authority; the golden
//! corpora in this module pin the exact bytes.

use super::*;
use crate::ProcessId;
use crate::SessionId;
use crate::TurnId;

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

pub(super) const LEGACY_APPEND_REQUEST_IDENTITY_ENCODING_VERSION: u32 = 1;
pub(super) const APPEND_REQUEST_IDENTITY_ENCODING_VERSION: u32 = 4;

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
    /// Return the stored first-attempt result without applying the attempted commit.
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

/// Decide how an existing runtime commit receipt applies to one attempted commit.
///
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

/// An `EffectAddress` cannot be built without an admitted execution scope, and
/// the scope is what carries the journal identity read back here.
#[expect(clippy::expect_used, reason = "the address carries the scope")]
fn push_causal_ref(
    identity: &mut crate::stable_identity::IdentityEncoder,
    caused_by: &crate::CausalRef,
) {
    match caused_by {
        crate::CausalRef::Turn {
            session_id,
            turn_id,
        } => {
            identity.tag(0);
            identity.string(session_id);
            identity.string(turn_id);
        }
        crate::CausalRef::Effect { address } => {
            identity.tag(1);
            identity.string(
                address
                    .execution_scope
                    .journal_identity()
                    .expect("causal effect address contains a valid execution scope")
                    .key(),
            );
            identity.string(&address.replay_key);
        }
        crate::CausalRef::ToolCall {
            session_id,
            call_id,
        } => {
            identity.tag(2);
            identity.string(session_id);
            identity.string(call_id);
        }
        crate::CausalRef::Process { process_id } => {
            identity.tag(3);
            identity.string(process_id);
        }
        crate::CausalRef::ProcessEvent {
            process_id,
            sequence,
        } => {
            identity.tag(4);
            identity.string(process_id);
            identity.u64(*sequence);
        }
        crate::CausalRef::TriggerOccurrence {
            occurrence_id,
            subscription_id,
            subscription_incarnation,
            subscription_revision,
        } => {
            identity.tag(5);
            identity.string(occurrence_id);
            identity.optional(subscription_id.as_ref(), |identity, value| {
                identity.string(value)
            });
            identity.optional(subscription_incarnation.as_ref(), |identity, value| {
                identity.string(value)
            });
            identity.optional(subscription_revision.as_ref(), |identity, value| {
                identity.u64(*value)
            });
        }
        crate::CausalRef::SessionNode {
            session_id,
            node_id,
        } => {
            identity.tag(6);
            identity.string(session_id);
            identity.string(node_id);
        }
    }
}

fn push_message_origin(
    identity: &mut crate::stable_identity::IdentityEncoder,
    origin: &crate::MessageOrigin,
) {
    match origin {
        crate::MessageOrigin::Plugin {
            plugin_id,
            transient,
        } => {
            identity.tag(0);
            identity.string(plugin_id);
            identity.u8(u8::from(*transient));
        }
        crate::MessageOrigin::Process {
            process_id,
            event_type,
            sequence,
            wake_id,
            caused_by,
        } => {
            identity.tag(1);
            identity.string(process_id);
            identity.string(event_type);
            identity.u64(*sequence);
            identity.optional(wake_id.as_ref(), |identity, value| identity.string(value));
            identity.optional(caused_by.as_ref(), push_causal_ref);
        }
        crate::MessageOrigin::TurnInput { turn_id, input_id } => {
            identity.tag(2);
            identity.string(turn_id);
            identity.optional(input_id.as_ref(), |identity, value| identity.string(value));
        }
        crate::MessageOrigin::TurnOutput { turn_id, source } => {
            identity.tag(3);
            identity.string(turn_id);
            match source {
                crate::TurnOutputSource::Runtime => identity.tag(0),
                crate::TurnOutputSource::Plugin { plugin_id } => {
                    identity.tag(1);
                    identity.string(plugin_id);
                }
            }
        }
    }
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

fn push_attachment_source(
    identity: &mut crate::stable_identity::IdentityEncoder,
    source: &crate::AttachmentSource,
) {
    match source {
        crate::AttachmentSource::Inline { media_type, bytes } => {
            identity.tag(0);
            identity.string(media_type.as_str());
            identity.bytes(bytes);
        }
        crate::AttachmentSource::Stored { attachment_ref } => {
            identity.tag(1);
            push_attachment_ref(identity, attachment_ref);
        }
        crate::AttachmentSource::ExternalUrl { media_type, url } => {
            identity.tag(2);
            identity.string(media_type.as_str());
            identity.string(url);
        }
        crate::AttachmentSource::ProviderFile {
            provider_scope,
            id,
            media_type,
        } => {
            let crate::ProviderFileScope {
                provider,
                credential_scope,
            } = provider_scope;
            identity.tag(3);
            identity.string(provider);
            identity.string(credential_scope);
            identity.string(id);
            identity.optional(media_type.as_ref(), |identity, value| {
                identity.string(value.as_str())
            });
        }
    }
}

fn push_part_kind(identity: &mut crate::stable_identity::IdentityEncoder, kind: crate::PartKind) {
    identity.tag(match kind {
        crate::PartKind::Text => 0,
        crate::PartKind::Attachment => 1,
        crate::PartKind::Code => 2,
        crate::PartKind::Output => 3,
        crate::PartKind::Error => 4,
        crate::PartKind::Prose => 5,
        crate::PartKind::ToolCall => 6,
        crate::PartKind::ToolResult => 7,
        crate::PartKind::Reasoning => 8,
    });
}

fn push_prune_state(
    identity: &mut crate::stable_identity::IdentityEncoder,
    state: &crate::PruneState,
) {
    match state {
        crate::PruneState::Intact => identity.tag(0),
        crate::PruneState::Cleared => identity.tag(1),
        crate::PruneState::Deleted {
            breadcrumb,
            archive_hash,
        } => {
            identity.tag(2);
            identity.string(breadcrumb);
            identity.string(archive_hash);
        }
        crate::PruneState::Summarized {
            summary,
            archive_hash,
        } => {
            identity.tag(3);
            identity.string(summary);
            identity.string(archive_hash);
        }
    }
}

fn push_part(
    identity: &mut crate::stable_identity::IdentityEncoder,
    part: &crate::Part,
    encoding_version: u32,
) {
    identity.string(part.id());
    push_part_kind(identity, part.kind());
    identity.string(part.content());
    identity.optional(part.attachment(), |identity, attachment| {
        let lash_sansio::PartAttachment { source } = attachment;
        push_attachment_source(identity, source)
    });
    identity.optional(part.tool_call_id(), |identity, value| {
        identity.string(value)
    });
    identity.optional(part.tool_name(), |identity, value| identity.string(value));
    identity.optional(part.tool_replay(), |identity, replay| {
        let lash_sansio::llm::types::ProviderReplayMeta {
            item_id,
            opaque,
            origin,
        } = replay;
        identity.optional(item_id.as_ref(), |identity, value| identity.string(value));
        identity.optional(opaque.as_ref(), |identity, value| identity.string(value));
        if encoding_version == APPEND_REQUEST_IDENTITY_ENCODING_VERSION {
            identity.optional(origin.as_ref(), crate::stable_identity::provider_route);
        }
    });
    push_prune_state(identity, part.prune_state());
    identity.optional(part.reasoning_meta(), |identity, replay| {
        let lash_sansio::llm::types::ProviderReasoningReplay {
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
        if encoding_version == APPEND_REQUEST_IDENTITY_ENCODING_VERSION {
            identity.optional(origin.as_ref(), crate::stable_identity::provider_route);
        }
    });
    identity.optional(part.response_meta(), |identity, response| {
        let lash_sansio::llm::types::ResponseTextMeta {
            id,
            status,
            phase,
            provider_payload,
            origin,
            legacy_origin_provider,
            legacy_origin_model,
        } = response;
        for value in [
            id.as_deref(),
            status.as_deref(),
            (*phase).map(|phase| phase.as_str()),
            provider_payload.as_deref(),
        ] {
            identity.optional(value, crate::stable_identity::IdentityEncoder::string);
        }
        if encoding_version == LEGACY_APPEND_REQUEST_IDENTITY_ENCODING_VERSION {
            // ResponseTextMeta carried provider/model before the unified
            // route. Project those two legacy leaves and deliberately omit
            // endpoint so migrated values retain their exact v1 preimage.
            let provider = legacy_origin_provider
                .as_ref()
                .map(String::as_str)
                .or_else(|| origin.as_ref().map(|route| route.provider.as_ref()));
            let model = legacy_origin_model
                .as_ref()
                .map(String::as_str)
                .or_else(|| origin.as_ref().map(|route| route.model.as_ref()));
            identity.optional(provider, crate::stable_identity::IdentityEncoder::string);
            identity.optional(model, crate::stable_identity::IdentityEncoder::string);
        } else {
            identity.optional(origin.as_ref(), crate::stable_identity::provider_route);
        }
    });
}

fn append_node_identity_bytes_with_version(
    node: &crate::SessionAppendNode,
    encoding_version: u32,
) -> Result<Vec<u8>, StoreError> {
    let mut identity =
        crate::stable_identity::IdentityEncoder::new_unframed(APPEND_REQUEST_IDENTITY_DOMAIN);
    match node {
        crate::SessionAppendNode::Message { message } => {
            let crate::PluginMessage {
                id,
                role,
                content,
                origin,
                parts,
                attachments,
            } = message;
            identity.tag(0);
            identity.optional(id.as_ref(), |identity, value| identity.string(value));
            push_message_role(&mut identity, *role);
            identity.string(content);
            identity.optional(origin.as_ref(), push_message_origin);
            identity.sequence(parts, |identity, part| {
                push_part(identity, part, encoding_version)
            });
            identity.sequence(attachments, push_attachment_source);
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

#[cfg(test)]
fn append_node_identity_bytes(node: &crate::SessionAppendNode) -> Result<Vec<u8>, StoreError> {
    append_node_identity_bytes_with_version(node, LEGACY_APPEND_REQUEST_IDENTITY_ENCODING_VERSION)
}

pub(super) fn append_request_identity_encoding_version(nodes: &[crate::SessionAppendNode]) -> u32 {
    let has_current_identity_vocabulary = nodes.iter().any(|node| match node {
        crate::SessionAppendNode::Message { message } => {
            matches!(
                message.origin.as_ref(),
                Some(crate::MessageOrigin::Process {
                    caused_by: Some(crate::CausalRef::Effect { .. }),
                    ..
                })
            ) || message.parts.iter().any(|part| {
                part.tool_replay()
                    .and_then(|replay| replay.origin.as_ref())
                    .or_else(|| {
                        part.reasoning_meta()
                            .and_then(|replay| replay.origin.as_ref())
                    })
                    .or_else(|| part.response_meta().and_then(|meta| meta.origin.as_ref()))
                    .is_some()
            })
        }
        _ => false,
    });
    if has_current_identity_vocabulary {
        APPEND_REQUEST_IDENTITY_ENCODING_VERSION
    } else {
        LEGACY_APPEND_REQUEST_IDENTITY_ENCODING_VERSION
    }
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
    let encoding_version = append_request_identity_encoding_version(nodes);
    let operation_key = operation.storage_key()?;
    let mut identity =
        crate::stable_identity::IdentityEncoder::new_unframed(APPEND_REQUEST_IDENTITY_DOMAIN);
    identity.bytes(operation_key.as_bytes());
    identity.optional(requested_ancestor_node_id, |identity, ancestor| {
        identity.string(ancestor)
    });
    identity.u64(nodes.len() as u64);
    for node in nodes {
        let semantic_node = append_node_identity_bytes_with_version(node, encoding_version)?;
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
        "lash-append-request/v2",
        &append_request_identity_bytes(operation, requested_ancestor_node_id, nodes)?,
    ))
}

#[cfg(test)]
#[path = "commit_identity_v4_effect_tests.rs"]
mod commit_identity_v4_effect_tests;

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

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn node_fixture(value: serde_json::Value) -> crate::SessionAppendNode {
        serde_json::from_value(value).expect("valid append-node fixture")
    }

    macro_rules! define_whole_request_variant_corpus {
        ($(
            $pattern:pat => {
                row: $row:literal,
                ancestor: $ancestor:expr,
                node: $node:expr,
                trailing_nodes: [$($trailing_node:expr),* $(,)?] $(,)?
            }
        ),+ $(,)?) => {
            fn whole_request_variant_row(node: &crate::SessionAppendNode) -> &'static str {
                match node {
                    $($pattern => $row),+
                }
            }

            fn whole_request_variant_corpus(
            ) -> Vec<(&'static str, Option<&'static str>, Vec<crate::SessionAppendNode>)> {
                vec![$({
                    let nodes = vec![$node, $($trailing_node),*];
                    ($row, $ancestor, nodes)
                }),+]
            }
        };
    }

    // This single declaration generates both the exhaustive variant-to-row
    // match and the fixture list. Adding a top-level SessionAppendNode variant
    // makes the match fail to compile; adding its required arm also creates a
    // whole-envelope corpus fixture whose returned key must exist in the
    // golden file.
    define_whole_request_variant_corpus! {
        crate::SessionAppendNode::Message { .. } => {
            row: "whole_request_message_ancestor_absent",
            ancestor: None,
            node: crate::SessionAppendNode::message(crate::PluginMessage::text(
                crate::MessageRole::User,
                "whole message",
            )),
            trailing_nodes: [],
        },
        crate::SessionAppendNode::ProtocolEvent { .. } => {
            row: "whole_request_protocol_event_ancestor_present",
            ancestor: Some("whole-ancestor"),
            node: crate::SessionAppendNode::protocol_event(crate::ProtocolEvent {
                plugin_id: "whole-protocol".to_string(),
                payload: serde_json::json!({"event": true}),
            }),
            trailing_nodes: [],
        },
        crate::SessionAppendNode::Plugin { .. } => {
            row: "whole_request_plugin_multi_node",
            ancestor: Some("multi-ancestor"),
            node: crate::SessionAppendNode::plugin(
                "whole-plugin",
                serde_json::json!({"plugin": "λ"}),
            ),
            trailing_nodes: [
                crate::SessionAppendNode::message(crate::PluginMessage::text(
                    crate::MessageRole::Assistant,
                    "second node",
                )),
            ],
        },
    }

    #[test]
    fn receipt_decision_table_pins_count_corruption_and_precedence() {
        use RuntimeCommitReceiptDecision::{
            AppendIdentityConflict, CorruptRequestedNodeCount, Replay, RuntimeCommitConflict,
        };

        let identity =
            |version, request_hash: &str, requested_node_count| AppendRequestIdentity::Append {
                encoding_version: version,
                request_hash: request_hash.to_string(),
                requested_node_count,
                requested_ancestor_node_id: None,
            };
        let plain = AppendRequestIdentity::PlainCommit;
        let decide = |stored_hash,
                      attempted_hash,
                      stored_identity: &AppendRequestIdentity,
                      attempted_identity: &AppendRequestIdentity| {
            decide_runtime_commit_receipt(
                stored_hash,
                attempted_hash,
                stored_identity,
                attempted_identity,
            )
        };

        assert_eq!(
            decide(
                "same",
                "same",
                &identity(1, "original-ancestor-identity", 1),
                &identity(1, "changed-ancestor-identity", 1),
            ),
            AppendIdentityConflict,
            "exact commit hashes cannot conceal comparable append identity drift"
        );
        assert_eq!(
            decide("same", "same", &plain, &identity(1, "new", 2)),
            Replay,
            "legacy exact-hash replay tolerates absent legacy metadata"
        );
        assert_eq!(
            decide("same", "same", &identity(1, "id", 1), &identity(1, "id", 2),),
            CorruptRequestedNodeCount {
                stored: 1,
                attempted: 2,
            }
        );
        assert_eq!(
            decide("old", "new", &identity(1, "id", 2), &identity(1, "id", 2),),
            Replay
        );
        assert_eq!(
            decide("old", "new", &plain, &identity(1, "id", 2)),
            RuntimeCommitConflict
        );
        assert_eq!(
            decide(
                "old",
                "new",
                &identity(1, "old-id", 2),
                &identity(1, "new-id", 2),
            ),
            AppendIdentityConflict
        );
        assert_eq!(
            decide("old", "new", &identity(1, "id", 2), &identity(2, "id", 2),),
            RuntimeCommitConflict
        );
    }

    #[test]
    fn append_request_identity_v1_golden_byte_corpus() {
        // Versioned durability corpus. These are the exact v1 bytes, not merely
        // relational hashes. Any projection change requires an explicit
        // APPEND_REQUEST_IDENTITY_ENCODING_VERSION bump and corpus replacement.
        let numeric_payload: serde_json::Value = serde_json::from_str(
            r#"{
                "null": null,
                "false": false,
                "true": true,
                "i64_min": -9223372036854775808,
                "i64_max": 9223372036854775807,
                "u64_max": 18446744073709551615,
                "negative_zero": -0.0,
                "fraction": 1.5,
                "text": "λ",
                "array": [0, 1.0]
            }"#,
        )
        .expect("numeric JSON fixture");
        let comprehensive_message = node_fixture(serde_json::json!({
            "kind": "message",
            "message": {
                "id": "message-id",
                "role": "Assistant",
                "content": "message-content",
                "origin": {
                    "kind": "process",
                    "process_id": "process-id",
                    "event_type": "event-type",
                    "sequence": 18446744073709551615_u64,
                    "wake_id": "wake-id",
                    "caused_by": {
                        "type": "trigger_occurrence",
                        "occurrence_id": "occurrence-id",
                        "subscription_id": "subscription-id",
                        "subscription_incarnation": "incarnation-id",
                        "subscription_revision": 18446744073709551615_u64
                    }
                },
                "parts": [
                    {
                        "id": "p0", "kind": "ToolCall", "content": "tool-call",
                        "tool_call_id": "call-id",
                        "tool_name": "tool-name",
                        "tool_replay": {"item_id": "item-id", "opaque": "opaque"},
                        "prune_state": {"Deleted": {"breadcrumb": "crumb", "archive_hash": "archive"}}
                    },
                    {
                        "id": "p1", "kind": "Text", "content": "text",
                        "response_meta": {
                            "id": "response-id", "status": "complete", "phase": "final_answer",
                            "provider_payload": "payload",
                            "origin_provider": "provider", "origin_model": "model"
                        },
                        "prune_state": "Intact"
                    },
                    {
                        "id": "p2", "kind": "Attachment", "content": "attachment",
                        "attachment": {"source": {
                            "source": "stored",
                            "attachment_ref": {
                                "id": "attachment-id", "media_type": "image/png",
                                "byte_len": 18446744073709551615_u64,
                                "type_metadata": {"type": "image", "width": 0, "height": 4294967295_u32},
                                "label": "attachment-label"
                            }
                        }},
                        "prune_state": "Cleared"
                    },
                    {"id": "p3", "kind": "Code", "content": "code", "prune_state": {"Summarized": {"summary": "short", "archive_hash": "hash"}}},
                    {"id": "p4", "kind": "Output", "content": "output", "prune_state": "Intact"},
                    {"id": "p5", "kind": "Error", "content": "error", "prune_state": "Intact"},
                    {"id": "p6", "kind": "Prose", "content": "prose", "prune_state": "Intact"},
                    {
                        "id": "p7", "kind": "ToolResult", "content": "tool-result",
                        "tool_call_id": "call-id", "tool_name": "tool-name",
                        "prune_state": "Intact"
                    },
                    {
                        "id": "p8", "kind": "Reasoning", "content": "reasoning",
                        "reasoning_meta": {
                            "item_id": "reason-id", "encrypted_content": "encrypted",
                            "signature": "signature", "redacted": true,
                            "summary": ["summary-a", "summary-b"]
                        },
                        "prune_state": "Intact"
                    }
                ],
                "attachments": [
                    {"source": "inline", "media_type": "application/octet-stream", "bytes": [0, 255]},
                    {"source": "stored", "attachment_ref": {
                        "id": "stored-min", "media_type": "text/plain", "byte_len": 0
                    }},
                    {"source": "external_url", "media_type": "image/jpeg", "url": "https://example.test/image.jpg"},
                    {"source": "provider_file", "provider_scope": {
                        "provider": "openai", "credential_scope": "account"
                    }, "id": "file-id", "media_type": "application/pdf"}
                ]
            }
        }));
        let plugin_origin_message = node_fixture(serde_json::json!({
            "kind": "message",
            "message": {
                "role": "Event",
                "content": "event",
                "origin": {"kind": "plugin", "plugin_id": "plugin-id", "transient": true}
            }
        }));
        assert_eq!(
            append_request_identity_encoding_version(std::slice::from_ref(&comprehensive_message)),
            LEGACY_APPEND_REQUEST_IDENTITY_ENCODING_VERSION,
            "actual base-era ResponseTextMeta JSON must stay in the v1 family"
        );
        let system_message = crate::SessionAppendNode::message(crate::PluginMessage::text(
            crate::MessageRole::System,
            "system",
        ));
        let cases = [
            (
                "message_optional_fields_absent",
                crate::SessionAppendNode::message(crate::PluginMessage::text(
                    crate::MessageRole::User,
                    "",
                )),
            ),
            ("message_plugin_origin_present", plugin_origin_message),
            ("message_system_role", system_message),
            (
                "message_all_fields_and_nested_variants",
                comprehensive_message,
            ),
            (
                "protocol_event",
                crate::SessionAppendNode::protocol_event(crate::ProtocolEvent {
                    plugin_id: "protocol-plugin".to_string(),
                    payload: serde_json::json!({"z": [true, null], "a": "event"}),
                }),
            ),
            (
                "plugin_json_numeric_edges",
                crate::SessionAppendNode::plugin("plugin-type", numeric_payload),
            ),
        ];

        let causal_cases = [
            crate::CausalRef::Turn {
                session_id: SessionId::from("s"),
                turn_id: TurnId::from("t"),
            },
            crate::CausalRef::ToolCall {
                session_id: SessionId::from("s"),
                call_id: "c".to_string(),
            },
            crate::CausalRef::Process {
                process_id: ProcessId::from("p"),
            },
            crate::CausalRef::ProcessEvent {
                process_id: ProcessId::from("p"),
                sequence: u64::MAX,
            },
            crate::CausalRef::TriggerOccurrence {
                occurrence_id: "o".to_string(),
                subscription_id: None,
                subscription_incarnation: None,
                subscription_revision: None,
            },
            crate::CausalRef::SessionNode {
                session_id: SessionId::from("s"),
                node_id: "n".to_string(),
            },
        ];

        let causal_names = [
            "causal_variant_0",
            "causal_variant_2",
            "causal_variant_3",
            "causal_variant_4",
            "causal_variant_5",
            "causal_variant_6",
        ];
        let whole_requests =
            whole_request_variant_corpus()
                .into_iter()
                .map(|(name, ancestor, nodes)| {
                    assert_eq!(
                        whole_request_variant_row(&nodes[0]),
                        name,
                        "variant fixture must return its declared golden row key"
                    );
                    (
                        name,
                        hex(
                            &append_request_identity_bytes(&operation(name), ancestor, &nodes)
                                .expect("encode whole request"),
                        ),
                    )
                });
        let empty_request = {
            let name = "whole_request_empty_node_envelope";
            (
                name,
                hex(&append_request_identity_bytes(&operation(name), None, &[])
                    .expect("encode whole request")),
            )
        };
        let rendered = cases
            .iter()
            .map(|(name, node)| {
                (
                    *name,
                    hex(&append_node_identity_bytes(node).expect("encode node")),
                )
            })
            .chain(causal_cases.iter().enumerate().map(|(index, causal)| {
                let mut identity = crate::stable_identity::IdentityEncoder::new_unframed(
                    APPEND_REQUEST_IDENTITY_DOMAIN,
                );
                push_causal_ref(&mut identity, causal);
                (causal_names[index], hex(&identity.finish()))
            }))
            .chain(whole_requests)
            .chain(std::iter::once(empty_request))
            .collect::<Vec<_>>();
        let expected = include_str!("testdata/append_request_identity_v1.hex")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.split_once('=').expect("name=hex golden corpus row"))
            .collect::<std::collections::BTreeMap<_, _>>();
        assert_eq!(
            expected.get("causal_variant_1"),
            Some(&"0100000000000000017300000000000000000165"),
            "the unrepresentable pre-address Effect cause remains frozen as historical v1 bytes"
        );
        let rendered_len = rendered.len();
        for (name, actual) in rendered {
            let Some(expected) = expected.get(name) else {
                panic!("missing golden corpus row {name}={actual}");
            };
            assert_eq!(actual, **expected, "v1 bytes moved for {name}");
        }
        assert_eq!(
            rendered_len + 1,
            expected.len(),
            "golden corpus row count includes one historical Effect cause that current types cannot manufacture"
        );
    }

    #[test]
    fn base_response_text_meta_json_retains_its_exact_v1_preimage() {
        // This is the ResponseTextMeta vocabulary emitted by 01aaf70cc: the
        // provider/model leaves are siblings and no endpoint exists.
        let node = node_fixture(
            serde_json::from_str(
                r#"{"kind":"message","message":{"role":"Assistant","content":"base-era response","parts":[{"id":"p0","kind":"Prose","content":"answer","prune_state":"Intact","response_meta":{"id":"response-id","status":"complete","phase":"final_answer","provider_payload":"signature","origin_provider":"google_oauth","origin_model":"gemini-base"}}]}}"#,
            )
            .expect("literal base-commit JSON"),
        );
        assert_eq!(
            append_request_identity_encoding_version(std::slice::from_ref(&node)),
            LEGACY_APPEND_REQUEST_IDENTITY_ENCODING_VERSION
        );
        assert_eq!(
            hex(&append_node_identity_bytes(&node).expect("encode legacy node")),
            include_str!("testdata/response_text_meta_base_v1.hex").trim(),
            "the v1 preimage of actual base-era JSON must never move"
        );
    }

    #[test]
    fn append_request_identity_v2_golden_byte_corpus() {
        // To refresh after an intentional v2 grammar change:
        // UPDATE_APPEND_REQUEST_IDENTITY_V2_GOLDEN=1 cargo test -p lash-core \
        //   append_request_identity_v2_golden_byte_corpus -- --exact
        // The v1 corpus above is never regenerated by this procedure.
        let node = node_fixture(serde_json::json!({
            "kind": "message",
            "message": {
                "role": "Assistant",
                "content": "route-owned replay",
                "parts": [
                    {
                        "id": "p0",
                        "kind": "ToolCall",
                        "content": "tool",
                        "tool_call_id": "call-id",
                        "tool_name": "tool-name",
                        "tool_replay": {
                            "item_id": "tool-item",
                            "opaque": "tool-opaque",
                            "origin": {
                                "provider": "openai-compatible",
                                "endpoint": "https://gateway.example/v1",
                                "model": "shared-model"
                            }
                        },
                        "prune_state": "Intact"
                    },
                    {
                        "id": "p1",
                        "kind": "Reasoning",
                        "content": "reasoning",
                        "reasoning_meta": {
                            "signature": "reasoning-signature",
                            "origin": {
                                "provider": "openai-compatible",
                                "endpoint": "https://gateway.example/v1",
                                "model": "shared-model"
                            }
                        },
                        "prune_state": "Intact"
                    },
                    {
                        "id": "p2",
                        "kind": "Prose",
                        "content": "response",
                        "response_meta": {
                            "id": "response-id",
                            "status": "completed",
                            "phase": "final_answer",
                            "origin": {
                                "provider": "openai-compatible",
                                "endpoint": "https://gateway.example/v1",
                                "model": "shared-model"
                            }
                        },
                        "prune_state": "Intact"
                    }
                ]
            }
        }));
        assert_eq!(
            append_request_identity_encoding_version(std::slice::from_ref(&node)),
            APPEND_REQUEST_IDENTITY_ENCODING_VERSION
        );
        let rows = [
            (
                "route_node",
                hex(&append_node_identity_bytes_with_version(
                    &node,
                    APPEND_REQUEST_IDENTITY_ENCODING_VERSION,
                )
                .expect("encode v2 node")),
            ),
            (
                "route_request",
                hex(&append_request_identity_bytes(
                    &operation("route-request"),
                    Some("ancestor"),
                    std::slice::from_ref(&node),
                )
                .expect("encode v2 request")),
            ),
        ];
        let rendered = rows
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        if std::env::var_os("UPDATE_APPEND_REQUEST_IDENTITY_V2_GOLDEN").is_some() {
            std::fs::write(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("src/store/testdata/append_request_identity_v2.hex"),
                &rendered,
            )
            .expect("write v2 golden corpus");
        }
        assert_eq!(
            rendered,
            include_str!("testdata/append_request_identity_v2.hex"),
            "v2 bytes moved; use the documented refresh command only for an intentional grammar change"
        );
    }

    #[test]
    fn append_request_identity_covers_only_ordered_semantic_request_fields() {
        let nodes = vec![
            crate::SessionAppendNode::plugin("receipt", serde_json::json!({"b": 2, "a": 1})),
            crate::SessionAppendNode::plugin("receipt", serde_json::json!({"value": 2})),
        ];
        let first = append_request_identity_hash(&operation("op-1"), Some("ancestor"), &nodes)
            .expect("first identity");
        let same = append_request_identity_hash(&operation("op-1"), Some("ancestor"), &nodes)
            .expect("same identity");
        assert_eq!(first, same);

        let mut reversed = nodes.clone();
        reversed.reverse();
        assert_ne!(
            first,
            append_request_identity_hash(&operation("op-1"), Some("ancestor"), &reversed)
                .expect("reordered identity")
        );
        assert_ne!(
            first,
            append_request_identity_hash(&operation("op-2"), Some("ancestor"), &nodes)
                .expect("changed operation identity")
        );
        assert_ne!(
            first,
            append_request_identity_hash(&operation("op-1"), None, &nodes)
                .expect("changed ancestor identity")
        );
    }
}

impl OperationId {
    /// Constructs a `OperationId` for store, effect-host, and protocol implementors while
    /// materializing, executing, or persisting a session turn.
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

    /// Exposes turn id to store, effect-host, and protocol implementors while materializing,
    /// executing, or persisting a session turn. Returns `None` when no turn id is present.
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
    current_frame_node_id: Option<&'a str>,
    graph: GraphCommitIntent<'a>,
    checkpoint: CheckpointIntent<'a>,
    usage_deltas: &'a [crate::store::RuntimeUsageDelta],
    #[serde(skip_serializing_if = "failure_evidence_is_empty")]
    failure_evidence: &'a [crate::TurnFailureEvidence],
    completed_queue_batches: Vec<CompletedQueueIntent<'a>>,
    completed_turn_inputs: Vec<CompletedTurnInputIntent<'a>>,
    enqueued_queue_batches: Vec<QueuedBatchIntent<'a>>,
    interrupted_turn_input_turn_id: Option<&'a TurnId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    interrupted_turn_input_cancellation: Option<&'a crate::TurnCancellationEvidence>,
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
            config: &commit.config,
            current_frame_node_id: commit.current_frame_node_id.as_deref(),
            graph,
            checkpoint: CheckpointIntent::from(&commit.checkpoint),
            usage_deltas: &commit.usage_deltas,
            failure_evidence: &commit.failure_evidence,
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
            interrupted_turn_input_turn_id: commit.interrupted_turn_input_turn_id.as_ref(),
            interrupted_turn_input_cancellation: commit
                .interrupted_turn_input_cancellation
                .as_ref(),
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
}

#[derive(serde::Serialize)]
struct CheckpointComponentIntent<'a> {
    key: &'a str,
    blob_ref: Option<BlobRef>,
    encoding_version: u32,
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
                    CheckpointComponentIntent {
                        key,
                        blob_ref,
                        encoding_version: component.encoding_version(),
                    }
                })
                .collect(),
        }
    }
}

#[derive(serde::Serialize)]
struct CompletedQueueIntent<'a> {
    session_id: &'a SessionId,
    batch_ids: &'a [crate::BatchId],
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
    input_ids: &'a [crate::InputId],
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
    session_id: &'a SessionId,
    source_key: Option<&'a str>,
    delivery_policy: &'a crate::DeliveryPolicy,
    kind: crate::QueuedWorkKind,
    authority: &'a crate::QueuedWorkAuthority,
    merge_key: Option<&'a str>,
    payloads: Vec<QueuedPayloadIntent<'a>>,
}

impl<'a> From<&'a crate::QueuedWorkBatchDraft> for QueuedBatchIntent<'a> {
    fn from(batch: &'a crate::QueuedWorkBatchDraft) -> Self {
        Self {
            session_id: &batch.session_id,
            source_key: batch.source_key.as_deref(),
            delivery_policy: &batch.delivery_policy,
            kind: batch.kind(),
            authority: &batch.authority,
            merge_key: batch.merge_key.as_deref(),
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
        target_session_id: &'a SessionId,
        process_id: &'a ProcessId,
        sequence: u64,
        event_type: &'a str,
        event_invocation: &'a crate::RuntimeInvocation,
        process_caused_by: &'a Option<crate::CausalRef>,
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
                process_id: &wake.process_id,
                sequence: wake.sequence,
                event_type: &wake.event_type,
                event_invocation: &wake.event_invocation,
                process_caused_by: &wake.process_caused_by,
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
        "lash-intent/v2",
        &identity.finish(),
    ))
}

#[cfg(test)]
mod blake3_vector_tests {
    #[test]
    fn commit_identity_v2_blake3_vector_is_pinned() {
        let mut identity = crate::stable_identity::IdentityEncoder::new_unframed(
            super::TURN_COMMIT_IDENTITY_DOMAIN,
        );
        identity.bytes(b"lash-commit-vector");
        assert_eq!(
            crate::stable_hash::blake3_hex("lash-intent/v2", &identity.finish()),
            "120001d338cb60a97d39d2f223d690a8f7548bf8e42beb0a3d7c03a36b477443"
        );
    }
}

pub fn derive_history_node_id(
    session_id: &SessionId,
    operation: &OperationId,
    ordinal: u64,
) -> Result<crate::NodeId, StoreError> {
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
    Ok(crate::NodeId::new(format!(
        "n_{}",
        crate::stable_hash::blake3_hex("lash-history-node/v3", &identity.finish())
    )))
}
