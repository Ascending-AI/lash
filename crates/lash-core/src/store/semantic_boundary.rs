//! Canonical request identity for FIG-2480 semantic-boundary receipts.
//!
//! Operations adopting the `SemanticBoundary` receipt identity answer "is this
//! the same request retried?" from a versioned canonical encoding of the
//! request content, so a retry rebuilt after the head has advanced still
//! replays while differing content is refused. Receipts never reconstruct
//! requests (store-as-continuation doctrine).

use super::*;

const RECORD_CONFIG_REQUEST_IDENTITY_ENCODING_VERSION: u32 = 1;
const CREATE_SESSION_REQUEST_IDENTITY_ENCODING_VERSION: u32 = 1;
// Version 2 (FIG-2765): staged usage rows carry their usage disposition through
// the v3 usage-payload identity, so a retried usage-ledger commit whose rows
// gained a hole or a correction no longer matches a v1 receipt. The projection
// and domain are unchanged; the version is the fence.
const USAGE_LEDGER_REQUEST_IDENTITY_ENCODING_VERSION: u32 = 2;

/// Refuse settlement or evidence content on a semantic-boundary commit.
///
/// The canonical request encoding deliberately excludes these fields, so a
/// commit carrying them could be deduplicated by receipt replay while its
/// settlements were silently dropped. The adopting boundary operations never
/// carry them; refusing keeps that a checked invariant instead of a habit.
pub(super) fn validate_semantic_boundary_commit_is_pure(
    commit: &RuntimeCommit,
) -> Result<(), StoreError> {
    let carried: &[(&str, bool)] = &[
        ("failure_evidence", !commit.failure_evidence.is_empty()),
        (
            "completed_queue_claims",
            !commit.completed_queue_claims.is_empty(),
        ),
        (
            "completed_turn_input_claims",
            !commit.completed_turn_input_claims.is_empty(),
        ),
        (
            "enqueued_queue_batches",
            !commit.enqueued_queue_batches.is_empty(),
        ),
        (
            "interrupted_turn_input_turn_id",
            commit.interrupted_turn_input_turn_id.is_some(),
        ),
        ("adopted_intent_rows", commit.adopted_intent_rows != 0),
        (
            "committed_attachment_ids",
            !commit.committed_attachment_ids.is_empty(),
        ),
    ];
    if let Some((field, _)) = carried.iter().find(|(_, present)| *present) {
        return Err(StoreError::Backend(format!(
            "semantic-boundary receipt identity for operation `{}` cannot ride a commit carrying `{field}`",
            commit.turn_commit.operation.key
        )));
    }
    Ok(())
}

/// Canonical semantic-boundary request projection (FIG-2480).
///
/// The projection is what the boundary caller asked the store to write:
/// operation identity, session binding, the persisted config, the appended
/// graph content, and staged usage identities. The rebuilt baseline —
/// checkpoint components, the CAS revision, and the derived frame pointer —
/// is deliberately excluded so a retry rebuilt after the head has advanced
/// still encodes the same request.
#[derive(serde::Serialize)]
struct SemanticBoundaryRequestIntent<'a> {
    operation_key: &'a str,
    session_id: &'a str,
    config: &'a crate::PersistedSessionConfig,
    /// Appended payload content only. Graph placement (node ids, parent
    /// linkage, the committed leaf) is derived position — it moves when other
    /// operations advance the head, exactly the retry window this identity
    /// exists to answer — so it is excluded like the CAS revision.
    appended_payloads: Vec<&'a crate::SessionNodePayload>,
    usage_deltas: &'a [crate::store::RuntimeUsageDelta],
}

fn semantic_boundary_request_intent_encoding(commit: &RuntimeCommit) -> Result<String, StoreError> {
    // The full destructure deliberately omits `..`: a new commit field fails
    // compilation until its place in (or out of) the canonical encoding is
    // decided here.
    let RuntimeCommit {
        commit_budget: _, // host operational policy
        session_id,
        expected_head_revision: _, // CAS is excluded from replay identity
        session_execution_lease_fence: _, // transaction predicate, not content
        release_session_execution_lease: _, // transport authority
        config,
        current_frame_node_id: _, // derived from the graph leaf
        graph,
        checkpoint: _, // rebuilt baseline, not the request
        usage_deltas,
        failure_evidence: _, // refused non-empty by validation
        turn_commit,
        completed_queue_claims: _,      // refused non-empty by validation
        completed_turn_input_claims: _, // refused non-empty by validation
        enqueued_queue_batches: _,      // refused non-empty by validation
        interrupted_turn_input_turn_id: _, // refused present by validation
        adopted_intent_rows: _,         // refused non-zero by validation
        committed_attachment_ids: _,    // refused non-empty by validation
    } = commit;
    let operation_key = turn_commit.operation.storage_key()?;
    let projection = SemanticBoundaryRequestIntent {
        operation_key: &operation_key,
        session_id,
        config,
        appended_payloads: graph.nodes.iter().map(|node| &node.payload).collect(),
        usage_deltas,
    };
    let value = serde_json::to_value(&projection).map_err(|err| {
        StoreError::Backend(format!(
            "failed to serialize semantic-boundary request identity: {err}"
        ))
    })?;
    crate::stable_hash::stable_json_string(&value).map_err(|err| {
        StoreError::Backend(format!(
            "failed to encode semantic-boundary request identity: {err}"
        ))
    })
}

/// Compute the versioned canonical request identity for one adopting
/// operation. Every version-1 encoding hashes the shared request projection
/// under an operation-owned domain string, so identical bytes for different
/// operations can never collide into one identity family.
pub(super) fn semantic_boundary_request_identity(
    commit: &RuntimeCommit,
    operation: crate::store::SemanticBoundaryOperation,
) -> Result<(u32, String), StoreError> {
    use crate::store::SemanticBoundaryOperation as Operation;
    let encoded = semantic_boundary_request_intent_encoding(commit)?;
    Ok(match operation {
        Operation::RecordConfig => (
            RECORD_CONFIG_REQUEST_IDENTITY_ENCODING_VERSION,
            crate::stable_hash::blake3_hex("lash-record-config-request/v1", encoded.as_bytes()),
        ),
        Operation::CreateSession => (
            CREATE_SESSION_REQUEST_IDENTITY_ENCODING_VERSION,
            crate::stable_hash::blake3_hex("lash-create-session-request/v1", encoded.as_bytes()),
        ),
        Operation::UsageLedger => (
            USAGE_LEDGER_REQUEST_IDENTITY_ENCODING_VERSION,
            crate::stable_hash::blake3_hex("lash-usage-ledger-request/v1", encoded.as_bytes()),
        ),
    })
}

#[cfg(test)]
mod semantic_boundary_request_identity_tests {
    use super::*;
    use crate::store::SemanticBoundaryOperation;

    fn boundary_commit(boundary: &str, key: &str) -> RuntimeCommit {
        let state = crate::RuntimeSessionState {
            session_id: "root".to_string(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        RuntimeCommit::persisted_state_with_operation_for_testing(
            &state,
            &[],
            OperationId::new(
                crate::ExecutionScope::runtime_operation(format!(
                    "session:root:boundary:{boundary}"
                )),
                key,
            ),
        )
    }

    fn stamped_record_config_commit() -> RuntimeCommit {
        let mut commit = boundary_commit("protocol-materialization", "record-config");
        commit
            .stamp_semantic_boundary()
            .expect("stamp record-config commit");
        commit
    }

    #[test]
    fn semantic_boundary_request_identity_v1_golden_corpus() {
        // Versioned durability corpus: the exact canonical preimage and hash
        // per adopting operation. Any projection change requires an explicit
        // per-operation encoding-version bump and corpus replacement. To
        // refresh after an intentional grammar change:
        // UPDATE_SEMANTIC_BOUNDARY_REQUEST_V1_GOLDEN=1 cargo test -p lash-core \
        //   semantic_boundary_request_identity_v1_golden_corpus -- --exact
        let rows = [
            ("record-config", "protocol-materialization", 1),
            ("create-session", "child-1", 1),
            ("usage-ledger", "child-turn", 2),
        ]
        .into_iter()
        .map(|(key, boundary, expected_version)| {
            let commit = boundary_commit(boundary, key);
            let operation =
                SemanticBoundaryOperation::from_operation_key(key).expect("adopted operation key");
            let preimage = semantic_boundary_request_intent_encoding(&commit)
                .expect("encode canonical request");
            let (encoding_version, hash) = semantic_boundary_request_identity(&commit, operation)
                .expect("hash canonical request");
            assert_eq!(
                encoding_version, expected_version,
                "{key} request identity encoding version"
            );
            format!("{key}={preimage}|{hash}")
        })
        .collect::<Vec<_>>()
        .join("\n")
            + "\n";
        if std::env::var_os("UPDATE_SEMANTIC_BOUNDARY_REQUEST_V1_GOLDEN").is_some() {
            std::fs::write(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("src/store/testdata/semantic_boundary_request_v1.golden"),
                &rows,
            )
            .expect("write semantic-boundary golden corpus");
        }
        assert_eq!(
            rows,
            include_str!("testdata/semantic_boundary_request_v1.golden"),
            "v1 canonical request bytes moved; use the documented refresh command only for an \
             intentional grammar change with a version bump"
        );
    }

    #[test]
    fn semantic_boundary_identity_excludes_the_rebuilt_baseline() {
        let commit = stamped_record_config_commit();
        let mut rebuilt = commit.clone();
        rebuilt.expected_head_revision += 7;
        rebuilt.checkpoint.turn_state.turn_index += 3;
        assert_ne!(
            commit.turn_commit_hash().expect("original commit hash"),
            rebuilt.turn_commit_hash().expect("rebuilt commit hash"),
            "the rebuilt baseline must change the exact commit hash"
        );
        assert_eq!(
            semantic_boundary_request_identity(&commit, SemanticBoundaryOperation::RecordConfig)
                .expect("original identity"),
            semantic_boundary_request_identity(&rebuilt, SemanticBoundaryOperation::RecordConfig)
                .expect("rebuilt identity"),
            "CAS revision and checkpoint baseline are excluded from the request identity"
        );
    }

    #[test]
    fn semantic_boundary_identity_covers_config_usage_and_operation() {
        let commit = stamped_record_config_commit();
        let (_, original) =
            semantic_boundary_request_identity(&commit, SemanticBoundaryOperation::RecordConfig)
                .expect("original identity");

        let mut changed_config = commit.clone();
        changed_config.config = crate::PersistedSessionConfig::new(crate::TurnBudget::bounded(7));
        let (_, changed) = semantic_boundary_request_identity(
            &changed_config,
            SemanticBoundaryOperation::RecordConfig,
        )
        .expect("changed-config identity");
        assert_ne!(original, changed, "config participates in the identity");

        let mut changed_usage = commit.clone();
        changed_usage.usage_deltas = RuntimeUsageDelta::for_operation(
            &changed_usage.turn_commit.operation,
            &[crate::TokenLedgerEntry {
                source: "turn".to_string(),
                model: "model".to_string(),
                usage: crate::TokenUsage {
                    input_tokens: 1,
                    output_tokens: 2,
                    cache_read_input_tokens: 0,
                    cache_write_input_tokens: 0,
                    reasoning_output_tokens: 0,
                },
                usage_disposition: Default::default(),
            }],
        )
        .expect("stage usage");
        let (_, changed) = semantic_boundary_request_identity(
            &changed_usage,
            SemanticBoundaryOperation::RecordConfig,
        )
        .expect("changed-usage identity");
        assert_ne!(original, changed, "usage identities participate");

        let (_, foreign) =
            semantic_boundary_request_identity(&commit, SemanticBoundaryOperation::UsageLedger)
                .expect("foreign-family identity");
        assert_ne!(
            original, foreign,
            "the operation-owned hash domain must separate identical bytes"
        );
    }

    #[test]
    fn stamped_semantic_boundary_commit_validates_and_stale_stamps_are_refused() {
        let commit = stamped_record_config_commit();
        commit
            .validate_operation_session()
            .expect("a freshly stamped commit must validate");

        let mut stale = commit.clone();
        stale.config = crate::PersistedSessionConfig::new(crate::TurnBudget::bounded(3));
        let error = stale
            .validate_operation_session()
            .expect_err("a stamp that no longer matches its content must be refused");
        assert!(
            error.to_string().contains(
                "semantic-boundary receipt identity does not match the canonical `record-config` request encoding"
            ),
            "unexpected stale-stamp refusal: {error}"
        );
    }

    #[test]
    fn semantic_boundary_identity_is_refused_for_foreign_operations() {
        let mut foreign = boundary_commit("identity-adoption", "initial-park");
        assert!(
            foreign
                .stamp_semantic_boundary()
                .expect_err("initial-park must not stamp a semantic boundary")
                .to_string()
                .contains(
                    "semantic-boundary receipt identity is not defined for operation `initial-park`"
                ),
        );
        foreign.turn_commit.append_request_identity = AppendRequestIdentity::SemanticBoundary {
            operation: SemanticBoundaryOperation::RecordConfig,
            encoding_version: 1,
            request_hash: "smuggled".to_string(),
        };
        let error = foreign
            .validate_operation_session()
            .expect_err("a mislabeled semantic identity must be refused");
        assert!(
            error.to_string().contains(
                "semantic-boundary receipt identity `record-config` is invalid for operation `initial-park`"
            ),
            "unexpected mislabel refusal: {error}"
        );
    }

    #[test]
    fn semantic_boundary_commit_refuses_settlement_content() {
        let mut commit = boundary_commit("protocol-materialization", "record-config");
        commit
            .completed_queue_claims
            .push(crate::QueuedWorkCompletion {
                session_id: "root".to_string(),
                claim_id: "claim".to_string(),
                lease_token: "token".to_string(),
                data: crate::QueuedWorkCompletionData {
                    batch_ids: vec!["batch".to_string()],
                },
            });
        commit
            .stamp_semantic_boundary()
            .expect("stamping does not adjudicate purity");
        let error = commit
            .validate_operation_session()
            .expect_err("settlement content must not ride a semantic-boundary commit");
        assert!(
            error.to_string().contains(
                "semantic-boundary receipt identity for operation `record-config` cannot ride a commit carrying `completed_queue_claims`"
            ),
            "unexpected purity refusal: {error}"
        );
    }

    #[test]
    fn receipt_decision_table_pins_semantic_boundary_precedence() {
        use crate::store::SemanticBoundaryOperation;
        use RuntimeCommitReceiptDecision::{
            Replay, RuntimeCommitConflict, SemanticBoundaryIdentityConflict,
        };

        let semantic =
            |operation, version, request_hash: &str| AppendRequestIdentity::SemanticBoundary {
                operation,
                encoding_version: version,
                request_hash: request_hash.to_string(),
            };
        let record_config =
            |version, hash: &str| semantic(SemanticBoundaryOperation::RecordConfig, version, hash);
        let plain = AppendRequestIdentity::PlainCommit;
        let append = AppendRequestIdentity::Append {
            encoding_version: 1,
            request_hash: "append".to_string(),
            requested_node_count: 1,
            requested_ancestor_node_id: None,
        };

        assert_eq!(
            decide_runtime_commit_receipt(
                "old",
                "new",
                &record_config(1, "same-request"),
                &record_config(1, "same-request"),
            ),
            Replay,
            "a rebuilt same-request retry replays after the head has advanced"
        );
        assert_eq!(
            decide_runtime_commit_receipt(
                "old",
                "new",
                &record_config(1, "original-request"),
                &record_config(1, "changed-request"),
            ),
            SemanticBoundaryIdentityConflict,
            "a differing canonical encoding is refused, never silently deduplicated"
        );
        assert_eq!(
            decide_runtime_commit_receipt(
                "same",
                "same",
                &record_config(1, "original-request"),
                &record_config(1, "changed-request"),
            ),
            SemanticBoundaryIdentityConflict,
            "exact commit hashes cannot conceal comparable semantic identity drift"
        );
        assert_eq!(
            decide_runtime_commit_receipt(
                "same",
                "same",
                &record_config(1, "same-request"),
                &record_config(1, "same-request"),
            ),
            Replay
        );
        assert_eq!(
            decide_runtime_commit_receipt(
                "old",
                "new",
                &record_config(1, "same-request"),
                &record_config(2, "same-request"),
            ),
            RuntimeCommitConflict,
            "identities compare only at one encoding version"
        );
        assert_eq!(
            decide_runtime_commit_receipt(
                "old",
                "new",
                &record_config(1, "same-request"),
                &semantic(SemanticBoundaryOperation::UsageLedger, 1, "same-request"),
            ),
            RuntimeCommitConflict,
            "identities compare only inside one operation family"
        );
        assert_eq!(
            decide_runtime_commit_receipt("old", "new", &plain, &record_config(1, "request")),
            RuntimeCommitConflict,
            "a pre-adoption plain receipt has no comparable semantic identity"
        );
        assert_eq!(
            decide_runtime_commit_receipt("old", "new", &append, &record_config(1, "request")),
            RuntimeCommitConflict,
            "append and semantic-boundary identities never compare"
        );
        assert_eq!(
            decide_runtime_commit_receipt("same", "same", &plain, &record_config(1, "request")),
            Replay,
            "legacy exact-hash replay tolerates absent legacy metadata"
        );
    }
}
