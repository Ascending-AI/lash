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

pub(super) const APPEND_REQUEST_IDENTITY_ENCODING_VERSION: u32 = 1;

/// Shared backend-independent decision for an existing runtime commit receipt.
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**.
/// Backends perform their own transactional reads and writes, but must use this
/// decision so exact-hash, append-identity, encoding-version, and node-count
/// precedence cannot drift between implementations.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RuntimeCommitReceiptDecision {
    /// Return the stored first-attempt result without applying the attempted commit.
    Replay,
    /// The append operation id was reused for different semantic request content.
    AppendIdentityConflict,
    /// The receipt has no comparable append identity and its commit hash differs.
    RuntimeCommitConflict,
    /// Matching receipt evidence carries contradictory requested-node counts.
    CorruptRequestedNodeCount {
        /// Requested-node count stored with the first-attempt receipt.
        stored: Option<u64>,
        /// Requested-node count supplied by the attempted append stamp.
        attempted: Option<u64>,
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
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub fn decide_runtime_commit_receipt(
    stored_commit_hash: &str,
    attempted_commit_hash: &str,
    stored_identity_encoding_version: Option<u32>,
    attempted_identity_encoding_version: Option<u32>,
    stored_request_identity_hash: Option<&str>,
    attempted_request_identity_hash: Option<&str>,
    stored_requested_node_count: Option<u64>,
    attempted_requested_node_count: Option<u64>,
) -> RuntimeCommitReceiptDecision {
    if stored_commit_hash == attempted_commit_hash {
        if let (
            Some(stored_version),
            Some(attempted_version),
            Some(stored_identity),
            Some(attempted_identity),
        ) = (
            stored_identity_encoding_version,
            attempted_identity_encoding_version,
            stored_request_identity_hash,
            attempted_request_identity_hash,
        ) && stored_version == attempted_version
            && stored_identity != attempted_identity
        {
            return RuntimeCommitReceiptDecision::AppendIdentityConflict;
        }
        if let (Some(stored), Some(attempted)) =
            (stored_requested_node_count, attempted_requested_node_count)
            && stored != attempted
        {
            return RuntimeCommitReceiptDecision::CorruptRequestedNodeCount {
                stored: Some(stored),
                attempted: Some(attempted),
            };
        }
        return RuntimeCommitReceiptDecision::Replay;
    }

    if let (
        Some(stored_version),
        Some(attempted_version),
        Some(stored_identity),
        Some(attempted_identity),
    ) = (
        stored_identity_encoding_version,
        attempted_identity_encoding_version,
        stored_request_identity_hash,
        attempted_request_identity_hash,
    ) && stored_version == attempted_version
    {
        if stored_identity != attempted_identity {
            return RuntimeCommitReceiptDecision::AppendIdentityConflict;
        }
        if stored_requested_node_count != attempted_requested_node_count {
            return RuntimeCommitReceiptDecision::CorruptRequestedNodeCount {
                stored: stored_requested_node_count,
                attempted: attempted_requested_node_count,
            };
        }
        return RuntimeCommitReceiptDecision::Replay;
    }

    RuntimeCommitReceiptDecision::RuntimeCommitConflict
}

pub(super) fn validate_append_receipt_identity(
    completed: &RuntimeTurnCommitStamp,
) -> Result<(), StoreError> {
    let has_append_identity = completed.identity_encoding_version.is_some()
        || completed.request_identity_hash.is_some()
        || completed.requested_node_count.is_some()
        || completed.requested_ancestor_node_id.is_some();
    if !has_append_identity {
        return Ok(());
    }
    if completed.operation.key != "append-session-nodes" {
        return Err(StoreError::Backend(format!(
            "append receipt identity metadata is invalid for operation `{}`",
            completed.operation.key
        )));
    }
    if completed.identity_encoding_version.is_none()
        || completed.request_identity_hash.is_none()
        || completed.requested_node_count.is_none()
    {
        return Err(StoreError::Backend(
            "append receipt identity metadata requires encoding version, request hash, and requested-node count"
                .to_string(),
        ));
    }
    Ok(())
}

fn push_len_prefixed(encoded: &mut Vec<u8>, bytes: &[u8]) {
    encoded.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
    encoded.extend_from_slice(bytes);
}

fn push_string(encoded: &mut Vec<u8>, value: &str) {
    push_len_prefixed(encoded, value.as_bytes());
}

fn push_optional<T>(encoded: &mut Vec<u8>, value: Option<&T>, push: impl FnOnce(&mut Vec<u8>, &T)) {
    match value {
        Some(value) => {
            encoded.push(1);
            push(encoded, value);
        }
        None => encoded.push(0),
    }
}

fn push_slice<T>(encoded: &mut Vec<u8>, values: &[T], push: impl Fn(&mut Vec<u8>, &T)) {
    encoded.extend_from_slice(&(values.len() as u64).to_be_bytes());
    for value in values {
        push(encoded, value);
    }
}

fn push_json_value(encoded: &mut Vec<u8>, value: &serde_json::Value) -> Result<(), StoreError> {
    match value {
        serde_json::Value::Null => encoded.push(0),
        serde_json::Value::Bool(false) => encoded.push(1),
        serde_json::Value::Bool(true) => encoded.push(2),
        serde_json::Value::Number(number) => {
            if let Some(value) = number.as_i64() {
                encoded.push(3);
                encoded.extend_from_slice(&value.to_be_bytes());
            } else if let Some(value) = number.as_u64() {
                encoded.push(4);
                encoded.extend_from_slice(&value.to_be_bytes());
            } else if let Some(value) = number.as_f64() {
                encoded.push(5);
                encoded.extend_from_slice(&value.to_bits().to_be_bytes());
            } else {
                return Err(StoreError::Backend(format!(
                    "append identity cannot encode JSON number `{number}`"
                )));
            }
        }
        serde_json::Value::String(value) => {
            encoded.push(6);
            push_string(encoded, value);
        }
        serde_json::Value::Array(values) => {
            encoded.push(7);
            encoded.extend_from_slice(&(values.len() as u64).to_be_bytes());
            for value in values {
                push_json_value(encoded, value)?;
            }
        }
        serde_json::Value::Object(values) => {
            encoded.push(8);
            encoded.extend_from_slice(&(values.len() as u64).to_be_bytes());
            let mut fields = values.iter().collect::<Vec<_>>();
            fields.sort_unstable_by_key(|(name, _)| *name);
            for (name, value) in fields {
                push_string(encoded, name);
                push_json_value(encoded, value)?;
            }
        }
    }
    Ok(())
}

fn push_message_role(encoded: &mut Vec<u8>, role: crate::MessageRole) {
    encoded.push(match role {
        crate::MessageRole::User => 0,
        crate::MessageRole::Assistant => 1,
        crate::MessageRole::System => 2,
        crate::MessageRole::Event => 3,
    });
}

fn push_causal_ref(encoded: &mut Vec<u8>, caused_by: &crate::CausalRef) {
    match caused_by {
        crate::CausalRef::Turn {
            session_id,
            turn_id,
        } => {
            encoded.push(0);
            push_string(encoded, session_id);
            push_string(encoded, turn_id);
        }
        crate::CausalRef::Effect {
            session_id,
            turn_id,
            effect_id,
        } => {
            encoded.push(1);
            push_string(encoded, session_id);
            push_optional(encoded, turn_id.as_ref(), |encoded, value| {
                push_string(encoded, value)
            });
            push_string(encoded, effect_id);
        }
        crate::CausalRef::ToolCall {
            session_id,
            call_id,
        } => {
            encoded.push(2);
            push_string(encoded, session_id);
            push_string(encoded, call_id);
        }
        crate::CausalRef::Process { process_id } => {
            encoded.push(3);
            push_string(encoded, process_id);
        }
        crate::CausalRef::ProcessEvent {
            process_id,
            sequence,
        } => {
            encoded.push(4);
            push_string(encoded, process_id);
            encoded.extend_from_slice(&sequence.to_be_bytes());
        }
        crate::CausalRef::TriggerOccurrence {
            occurrence_id,
            subscription_id,
            subscription_incarnation,
            subscription_revision,
        } => {
            encoded.push(5);
            push_string(encoded, occurrence_id);
            push_optional(encoded, subscription_id.as_ref(), |encoded, value| {
                push_string(encoded, value)
            });
            push_optional(
                encoded,
                subscription_incarnation.as_ref(),
                |encoded, value| push_string(encoded, value),
            );
            push_optional(encoded, subscription_revision.as_ref(), |encoded, value| {
                encoded.extend_from_slice(&value.to_be_bytes())
            });
        }
        crate::CausalRef::SessionNode {
            session_id,
            node_id,
        } => {
            encoded.push(6);
            push_string(encoded, session_id);
            push_string(encoded, node_id);
        }
    }
}

fn push_message_origin(encoded: &mut Vec<u8>, origin: &crate::MessageOrigin) {
    match origin {
        crate::MessageOrigin::Plugin {
            plugin_id,
            transient,
        } => {
            encoded.push(0);
            push_string(encoded, plugin_id);
            encoded.push(u8::from(*transient));
        }
        crate::MessageOrigin::Process {
            process_id,
            event_type,
            sequence,
            wake_id,
            caused_by,
        } => {
            encoded.push(1);
            push_string(encoded, process_id);
            push_string(encoded, event_type);
            encoded.extend_from_slice(&sequence.to_be_bytes());
            push_optional(encoded, wake_id.as_ref(), |encoded, value| {
                push_string(encoded, value)
            });
            push_optional(encoded, caused_by.as_ref(), push_causal_ref);
        }
        crate::MessageOrigin::TurnInput { turn_id, input_id } => {
            encoded.push(2);
            push_string(encoded, turn_id);
            push_optional(encoded, input_id.as_ref(), |encoded, value| {
                push_string(encoded, value)
            });
        }
    }
}

fn push_attachment_type_metadata(encoded: &mut Vec<u8>, metadata: &crate::AttachmentTypeMetadata) {
    match metadata {
        crate::AttachmentTypeMetadata::Image { width, height } => {
            encoded.push(0);
            push_optional(encoded, width.as_ref(), |encoded, value| {
                encoded.extend_from_slice(&value.to_be_bytes())
            });
            push_optional(encoded, height.as_ref(), |encoded, value| {
                encoded.extend_from_slice(&value.to_be_bytes())
            });
        }
    }
}

fn push_attachment_ref(encoded: &mut Vec<u8>, attachment: &crate::AttachmentRef) {
    let crate::AttachmentRef {
        id,
        media_type,
        byte_len,
        type_metadata,
        label,
    } = attachment;
    push_string(encoded, id.as_str());
    push_string(encoded, media_type.as_str());
    encoded.extend_from_slice(&byte_len.to_be_bytes());
    push_optional(
        encoded,
        type_metadata.as_ref(),
        push_attachment_type_metadata,
    );
    push_optional(encoded, label.as_ref(), |encoded, value| {
        push_string(encoded, value)
    });
}

fn push_attachment_source(encoded: &mut Vec<u8>, source: &crate::AttachmentSource) {
    match source {
        crate::AttachmentSource::Inline { media_type, bytes } => {
            encoded.push(0);
            push_string(encoded, media_type.as_str());
            push_len_prefixed(encoded, bytes);
        }
        crate::AttachmentSource::Stored { attachment_ref } => {
            encoded.push(1);
            push_attachment_ref(encoded, attachment_ref);
        }
        crate::AttachmentSource::ExternalUrl { media_type, url } => {
            encoded.push(2);
            push_string(encoded, media_type.as_str());
            push_string(encoded, url);
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
            encoded.push(3);
            push_string(encoded, provider);
            push_string(encoded, credential_scope);
            push_string(encoded, id);
            push_optional(encoded, media_type.as_ref(), |encoded, value| {
                push_string(encoded, value.as_str())
            });
        }
    }
}

fn push_part_kind(encoded: &mut Vec<u8>, kind: crate::PartKind) {
    encoded.push(match kind {
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

fn push_prune_state(encoded: &mut Vec<u8>, state: &crate::PruneState) {
    match state {
        crate::PruneState::Intact => encoded.push(0),
        crate::PruneState::Cleared => encoded.push(1),
        crate::PruneState::Deleted {
            breadcrumb,
            archive_hash,
        } => {
            encoded.push(2);
            push_string(encoded, breadcrumb);
            push_string(encoded, archive_hash);
        }
        crate::PruneState::Summarized {
            summary,
            archive_hash,
        } => {
            encoded.push(3);
            push_string(encoded, summary);
            push_string(encoded, archive_hash);
        }
    }
}

fn push_part(encoded: &mut Vec<u8>, part: &crate::Part) {
    let crate::Part {
        id,
        kind,
        content,
        attachment,
        tool_call_id,
        tool_name,
        tool_replay,
        prune_state,
        reasoning_meta,
        response_meta,
        ..
    } = part;
    push_string(encoded, id);
    push_part_kind(encoded, *kind);
    push_string(encoded, content);
    push_optional(encoded, attachment.as_ref(), |encoded, attachment| {
        let lash_sansio::PartAttachment { source } = attachment;
        push_attachment_source(encoded, source)
    });
    push_optional(encoded, tool_call_id.as_ref(), |encoded, value| {
        push_string(encoded, value)
    });
    push_optional(encoded, tool_name.as_ref(), |encoded, value| {
        push_string(encoded, value)
    });
    push_optional(encoded, tool_replay.as_ref(), |encoded, replay| {
        let lash_sansio::llm::types::ProviderReplayMeta { item_id, opaque } = replay;
        push_optional(encoded, item_id.as_ref(), |encoded, value| {
            push_string(encoded, value)
        });
        push_optional(encoded, opaque.as_ref(), |encoded, value| {
            push_string(encoded, value)
        });
    });
    push_prune_state(encoded, prune_state);
    push_optional(encoded, reasoning_meta.as_ref(), |encoded, replay| {
        let lash_sansio::llm::types::ProviderReasoningReplay {
            item_id,
            encrypted_content,
            signature,
            redacted,
            summary,
        } = replay;
        push_optional(encoded, item_id.as_ref(), |encoded, value| {
            push_string(encoded, value)
        });
        push_optional(encoded, encrypted_content.as_ref(), |encoded, value| {
            push_string(encoded, value)
        });
        push_optional(encoded, signature.as_ref(), |encoded, value| {
            push_string(encoded, value)
        });
        encoded.push(u8::from(*redacted));
        push_slice(encoded, summary, |encoded, value| {
            push_string(encoded, value)
        });
    });
    push_optional(encoded, response_meta.as_ref(), |encoded, response| {
        let lash_sansio::llm::types::ResponseTextMeta {
            id,
            status,
            phase,
            provider_payload,
            origin_provider,
            origin_model,
        } = response;
        for value in [
            id,
            status,
            phase,
            provider_payload,
            origin_provider,
            origin_model,
        ] {
            push_optional(encoded, value.as_ref(), |encoded, value| {
                push_string(encoded, value)
            });
        }
    });
}

fn append_node_identity_bytes(node: &crate::SessionAppendNode) -> Result<Vec<u8>, StoreError> {
    let mut encoded = Vec::new();
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
            encoded.push(0);
            push_optional(&mut encoded, id.as_ref(), |encoded, value| {
                push_string(encoded, value)
            });
            push_message_role(&mut encoded, *role);
            push_string(&mut encoded, content);
            push_optional(&mut encoded, origin.as_ref(), push_message_origin);
            push_slice(&mut encoded, parts, push_part);
            push_slice(&mut encoded, attachments, push_attachment_source);
        }
        crate::SessionAppendNode::ProtocolEvent { event } => {
            let crate::ProtocolEvent { plugin_id, payload } = event;
            encoded.push(1);
            push_string(&mut encoded, plugin_id);
            push_json_value(&mut encoded, payload)?;
        }
        crate::SessionAppendNode::Plugin { plugin_type, body } => {
            encoded.push(2);
            push_string(&mut encoded, plugin_type);
            push_json_value(&mut encoded, body)?;
        }
    }
    Ok(encoded)
}

/// Version 1 canonical bytes, in order:
///
/// 1. operation storage key: `u64` big-endian UTF-8 byte length, then bytes;
/// 2. requested ancestor: one byte (`0` for absent, `1` for present), followed
///    when present by its `u64` big-endian UTF-8 byte length and bytes;
/// 3. ordered semantic nodes: `u64` big-endian node count, then for each node
///    its hand-written tagged projection, framed by a `u64` big-endian length.
///
/// No domain string, encoding version, node id, timestamp, head, or other
/// environmental value is included. The version lives beside the digest in
/// the receipt so a future encoder can fall back to exact commit hashes.
fn append_request_identity_bytes(
    operation: &OperationId,
    requested_ancestor_node_id: Option<&str>,
    nodes: &[crate::SessionAppendNode],
) -> Result<Vec<u8>, StoreError> {
    let operation_key = operation.storage_key()?;
    let mut encoded = Vec::new();
    push_len_prefixed(&mut encoded, operation_key.as_bytes());
    match requested_ancestor_node_id {
        Some(ancestor) => {
            encoded.push(1);
            push_len_prefixed(&mut encoded, ancestor.as_bytes());
        }
        None => encoded.push(0),
    }
    encoded.extend_from_slice(&(nodes.len() as u64).to_be_bytes());
    for node in nodes {
        let semantic_node = append_node_identity_bytes(node)?;
        push_len_prefixed(&mut encoded, &semantic_node);
    }
    Ok(encoded)
}

pub(super) fn append_request_identity_hash(
    operation: &OperationId,
    requested_ancestor_node_id: Option<&str>,
    nodes: &[crate::SessionAppendNode],
) -> Result<String, StoreError> {
    use sha2::Digest;

    Ok(format!(
        "{:x}",
        sha2::Sha256::digest(append_request_identity_bytes(
            operation,
            requested_ancestor_node_id,
            nodes,
        )?)
    ))
}

#[cfg(test)]
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

        let decide = |stored_hash,
                      attempted_hash,
                      stored_version,
                      attempted_version,
                      stored_identity,
                      attempted_identity,
                      stored_count,
                      attempted_count| {
            decide_runtime_commit_receipt(
                stored_hash,
                attempted_hash,
                stored_version,
                attempted_version,
                stored_identity,
                attempted_identity,
                stored_count,
                attempted_count,
            )
        };

        assert_eq!(
            decide(
                "same",
                "same",
                Some(1),
                Some(1),
                Some("original-ancestor-identity"),
                Some("changed-ancestor-identity"),
                Some(1),
                Some(1),
            ),
            AppendIdentityConflict,
            "exact commit hashes cannot conceal comparable append identity drift"
        );
        assert_eq!(
            decide(
                "same",
                "same",
                None,
                Some(1),
                None,
                Some("new"),
                None,
                Some(2)
            ),
            Replay,
            "legacy exact-hash replay tolerates absent legacy metadata"
        );
        assert_eq!(
            decide(
                "same",
                "same",
                Some(1),
                Some(1),
                Some("id"),
                Some("id"),
                Some(1),
                Some(2),
            ),
            CorruptRequestedNodeCount {
                stored: Some(1),
                attempted: Some(2),
            }
        );
        assert_eq!(
            decide(
                "old",
                "new",
                Some(1),
                Some(1),
                Some("id"),
                Some("id"),
                Some(2),
                Some(2),
            ),
            Replay
        );
        assert_eq!(
            decide(
                "old",
                "new",
                Some(1),
                Some(1),
                Some("id"),
                Some("id"),
                None,
                Some(2),
            ),
            CorruptRequestedNodeCount {
                stored: None,
                attempted: Some(2),
            }
        );
        assert_eq!(
            decide(
                "old",
                "new",
                Some(1),
                Some(1),
                Some("old-id"),
                Some("new-id"),
                Some(2),
                Some(2),
            ),
            AppendIdentityConflict
        );
        assert_eq!(
            decide(
                "old",
                "new",
                Some(1),
                Some(2),
                Some("id"),
                Some("id"),
                Some(2),
                Some(2),
            ),
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
                        "attachment": {"source": {
                            "source": "stored",
                            "attachment_ref": {
                                "id": "attachment-id", "media_type": "image/png",
                                "byte_len": 18446744073709551615_u64,
                                "type_metadata": {"type": "image", "width": 0, "height": 4294967295_u32},
                                "label": "attachment-label"
                            }
                        }},
                        "tool_call_id": "call-id",
                        "tool_name": "tool-name",
                        "tool_replay": {"item_id": "item-id", "opaque": "opaque"},
                        "prune_state": {"Deleted": {"breadcrumb": "crumb", "archive_hash": "archive"}},
                        "reasoning_meta": {
                            "item_id": "reason-id", "encrypted_content": "encrypted",
                            "signature": "signature", "redacted": true,
                            "summary": ["summary-a", "summary-b"]
                        },
                        "response_meta": {
                            "id": "response-id", "status": "complete", "phase": "final_answer",
                            "provider_payload": "payload", "origin_provider": "provider",
                            "origin_model": "model"
                        }
                    },
                    {"id": "p1", "kind": "Text", "content": "text", "prune_state": "Intact"},
                    {"id": "p2", "kind": "Attachment", "content": "attachment", "prune_state": "Cleared"},
                    {"id": "p3", "kind": "Code", "content": "code", "prune_state": {"Summarized": {"summary": "short", "archive_hash": "hash"}}},
                    {"id": "p4", "kind": "Output", "content": "output", "prune_state": "Intact"},
                    {"id": "p5", "kind": "Error", "content": "error", "prune_state": "Intact"},
                    {"id": "p6", "kind": "Prose", "content": "prose", "prune_state": "Intact"},
                    {"id": "p7", "kind": "ToolResult", "content": "tool-result", "prune_state": "Intact"},
                    {"id": "p8", "kind": "Reasoning", "content": "reasoning", "prune_state": "Intact"}
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
                session_id: "s".to_string(),
                turn_id: "t".to_string(),
            },
            crate::CausalRef::Effect {
                session_id: "s".to_string(),
                turn_id: None,
                effect_id: "e".to_string(),
            },
            crate::CausalRef::ToolCall {
                session_id: "s".to_string(),
                call_id: "c".to_string(),
            },
            crate::CausalRef::Process {
                process_id: "p".to_string(),
            },
            crate::CausalRef::ProcessEvent {
                process_id: "p".to_string(),
                sequence: u64::MAX,
            },
            crate::CausalRef::TriggerOccurrence {
                occurrence_id: "o".to_string(),
                subscription_id: None,
                subscription_incarnation: None,
                subscription_revision: None,
            },
            crate::CausalRef::SessionNode {
                session_id: "s".to_string(),
                node_id: "n".to_string(),
            },
        ];

        let causal_names = [
            "causal_variant_0",
            "causal_variant_1",
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
                let mut encoded = Vec::new();
                push_causal_ref(&mut encoded, causal);
                (causal_names[index], hex(&encoded))
            }))
            .chain(whole_requests)
            .chain(std::iter::once(empty_request))
            .collect::<Vec<_>>();
        let expected = include_str!("testdata/append_request_identity_v1.hex")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| line.split_once('=').expect("name=hex golden corpus row"))
            .collect::<std::collections::BTreeMap<_, _>>();
        let rendered_len = rendered.len();
        for (name, actual) in rendered {
            let Some(expected) = expected.get(name) else {
                panic!("missing golden corpus row {name}={actual}");
            };
            assert_eq!(actual, **expected, "v1 bytes moved for {name}");
        }
        assert_eq!(rendered_len, expected.len(), "golden corpus row count");
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
        session_id: impl Into<String>,
        turn_id: impl Into<String>,
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
    usage_deltas: &'a [crate::store::RuntimeUsageDelta],
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
    components: Vec<CheckpointComponentIntent<'a>>,
    plugin_snapshot_revision: Option<u64>,
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
                .map(|(key, component)| CheckpointComponentIntent {
                    key,
                    blob_ref: component.body().map_or_else(
                        || component.blob_ref().cloned(),
                        |body| Some(BlobRef(crate::stable_hash::sha256_hex(body))),
                    ),
                    encoding_version: component.encoding_version(),
                })
                .collect(),
            plugin_snapshot_revision: checkpoint.plugin_snapshot_revision,
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
        process_id: &'a str,
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
    session_id: &str,
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
            "lash-history-node/v2",
            &[
                session_id.as_bytes(),
                operation.as_bytes(),
                &ordinal.to_be_bytes(),
            ],
        )
    ))
}
