//! Runtime commit envelope and result types.

use super::{
    BlobRef, GraphAppend, HydratedSessionCheckpoint, OperationId, RealizedNodeTimestamp,
    SessionCheckpoint, SessionExecutionLeaseAuthority, StoreError, commit_identity,
};

const USAGE_PAYLOAD_FAMILY_VERSION: u8 = 2;
pub(super) const USAGE_PAYLOAD_ENCODING_V2: u32 = USAGE_PAYLOAD_FAMILY_VERSION as u32;

/// Permanent tag registry for runtime-usage payload identities.
///
/// Version 2 has no sum or optional variants: source, model, and the five
/// counters form its complete sequence. Retired tags remain burned when
/// variants are introduced in a later family version.
/// Version 2 canonical bytes, in order:
///
/// The shared framing header owns the domain and family version. Source and
/// model follow as length-prefixed strings, then the five signed counters in
/// declaration order as big-endian `i64` values.
///
/// The full destructures deliberately omit `..`: adding a semantic field to
/// either durable DTO fails compilation until this projection is reconsidered.
fn usage_payload_identity_bytes(entry: &crate::TokenLedgerEntry) -> Vec<u8> {
    let crate::TokenLedgerEntry {
        source,
        model,
        usage,
    } = entry;
    let crate::TokenUsage {
        input_tokens,
        output_tokens,
        cache_read_input_tokens,
        cache_write_input_tokens,
        reasoning_output_tokens,
    } = usage;

    let mut identity = crate::stable_identity::IdentityEncoder::new(
        "lash.runtime-usage-payload",
        USAGE_PAYLOAD_FAMILY_VERSION,
    );
    identity.string(source);
    identity.string(model);
    identity.i64(*input_tokens);
    identity.i64(*output_tokens);
    identity.i64(*cache_read_input_tokens);
    identity.i64(*cache_write_input_tokens);
    identity.i64(*reasoning_output_tokens);
    identity.finish()
}

fn usage_payload_identity_hash(entry: &crate::TokenLedgerEntry) -> String {
    crate::stable_hash::sha256_hex(&usage_payload_identity_bytes(entry))
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeCommit {
    /// Host policy carried to the shared facade and backend validation seams.
    /// It is operational authority and intentionally excluded from the durable
    /// semantic commit identity projection.
    pub commit_budget: super::CommitBudget,
    pub session_id: String,
    pub expected_head_revision: u64,
    /// Current execution-lane authority required by a borrowed-lane commit.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// enforce this transaction predicate before consulting a durable receipt.
    ///
    /// This is a transaction predicate, not semantic commit content: backends
    /// validate it with the ordinary owner/generation/current-token/expiry
    /// fence before receipt replay or mutation, and never rotate or release the
    /// matching lease row.
    #[serde(skip)]
    pub session_execution_lease_fence: Option<SessionExecutionLeaseAuthority>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_session_execution_lease: Option<SessionExecutionLeaseAuthority>,
    pub config: crate::PersistedSessionConfig,
    pub current_frame_node_id: Option<String>,
    pub graph: GraphAppend,
    pub checkpoint: HydratedSessionCheckpoint,
    /// Usage rows published atomically by this commit, each carrying a stable
    /// identity so retrying an unknown commit outcome cannot double-account.
    pub usage_deltas: Vec<RuntimeUsageDelta>,
    pub turn_commit: RuntimeTurnCommitStamp,
    pub completed_queue_claims: Vec<crate::QueuedWorkCompletion>,
    pub completed_turn_input_claims: Vec<crate::TurnInputCompletion>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enqueued_queue_batches: Vec<crate::QueuedWorkBatchDraft>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_turn_input_turn_id: Option<String>,
    /// Attachment ids explicitly adopted by this commit. In the same
    /// transaction the backend also stamps every uncommitted manifest row owned
    /// by the turn id in `turn_commit.operation`, including ids that appear only in plain tool
    /// JSON. This list preserves typed-output and cross-turn re-references;
    /// adoption updates existing rows only and deliberately no-ops when this
    /// session has no matching intent.
    pub committed_attachment_ids: Vec<crate::AttachmentId>,
}

#[cfg(any(test, feature = "testing"))]
impl RuntimeCommit {
    const fn recommended_test_commit_budget() -> super::CommitBudget {
        super::CommitBudget::bounded(1024 * 1024, 512)
    }

    #[doc(hidden)]
    #[track_caller]
    pub fn persisted_state_for_test(
        state: &crate::RuntimeSessionState,
        usage_deltas: &[crate::TokenLedgerEntry],
    ) -> Self {
        Self::persisted_state_for_test_with_budget(
            state,
            usage_deltas,
            Self::recommended_test_commit_budget(),
        )
    }

    /// Build a test commit with explicit host-owned byte and node limits.
    #[doc(hidden)]
    #[track_caller]
    pub fn persisted_state_for_test_with_budget(
        state: &crate::RuntimeSessionState,
        usage_deltas: &[crate::TokenLedgerEntry],
        commit_budget: super::CommitBudget,
    ) -> Self {
        let caller = std::panic::Location::caller();
        let operation = OperationId::new(
            crate::ExecutionScope::runtime_operation(format!(
                "test-commit:{}:{}:{}",
                caller.file(),
                caller.line(),
                state.head_revision
            )),
            "commit",
        );
        let mut graph = state.pending_graph_commit();
        graph
            .derive_node_ids(&state.session_id, &operation)
            .expect("test commit node ids must be derivable");
        Self::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            graph,
            usage_deltas,
            operation,
            commit_budget,
        )
        .expect("test commit must be hashable")
    }

    /// Build a test commit with a fixed operation identity.
    #[doc(hidden)]
    pub fn persisted_state_with_operation_for_testing(
        state: &crate::RuntimeSessionState,
        usage_deltas: &[crate::TokenLedgerEntry],
        operation: OperationId,
    ) -> Self {
        let mut graph = state.pending_graph_commit();
        graph
            .derive_node_ids(&state.session_id, &operation)
            .expect("fixed-identity test commit node ids must be derivable");
        Self::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            graph,
            usage_deltas,
            operation,
            Self::recommended_test_commit_budget(),
        )
        .expect("fixed-identity test commit must be hashable")
    }

    #[doc(hidden)]
    #[track_caller]
    pub(crate) fn persisted_state_with_graph_commit(
        state: &crate::RuntimeSessionState,
        mut graph: GraphAppend,
        usage_deltas: &[crate::TokenLedgerEntry],
    ) -> Self {
        let caller = std::panic::Location::caller();
        let operation = OperationId::new(
            crate::ExecutionScope::runtime_operation(format!(
                "test-graph-commit:{}:{}:{}",
                caller.file(),
                caller.line(),
                state.head_revision
            )),
            "commit",
        );
        graph
            .derive_node_ids(&state.session_id, &operation)
            .expect("test graph commit node ids must be derivable");
        Self::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            graph,
            usage_deltas,
            operation,
            Self::recommended_test_commit_budget(),
        )
        .expect("test graph commit must be hashable")
    }

    pub(crate) fn persisted_state_with_operation(
        state: &mut crate::RuntimeSessionState,
        usage_deltas: &[crate::TokenLedgerEntry],
        operation: OperationId,
    ) -> Result<(Self, Vec<String>), StoreError> {
        Self::persisted_state_with_operation_and_budget(
            state,
            usage_deltas,
            operation,
            Self::recommended_test_commit_budget(),
        )
    }

    pub(crate) fn persisted_state_with_operation_and_staged_usage(
        state: &mut crate::RuntimeSessionState,
        usage_deltas: &[RuntimeUsageDelta],
        operation: OperationId,
    ) -> Result<(Self, Vec<String>), StoreError> {
        Self::persisted_state_with_operation_and_staged_usage_and_budget(
            state,
            usage_deltas,
            operation,
            Self::recommended_test_commit_budget(),
        )
    }

    pub(crate) fn persisted_state_with_graph_commit_and_operation(
        state: &crate::RuntimeSessionState,
        graph: GraphAppend,
        usage_deltas: &[crate::TokenLedgerEntry],
        operation: OperationId,
    ) -> Result<Self, StoreError> {
        Self::persisted_state_with_graph_commit_and_operation_and_budget(
            state,
            graph,
            usage_deltas,
            operation,
            Self::recommended_test_commit_budget(),
        )
    }
}

/// Durable identity for one usage row submitted through a runtime commit.
///
/// The operation key, ordinal, payload-encoding version, and payload hash are
/// assigned before the first commit attempt and must be reused byte-for-byte
/// until a commit containing the row has a confirmed outcome. Stores enforce
/// uniqueness per session over all four fields.
///
/// `payload_hash` is lowercase hexadecimal SHA-256 of Lash's hand-written,
/// domain-prefixed, length-framed projection of [`crate::TokenLedgerEntry`] and
/// its nested [`crate::TokenUsage`]. Binding both version and content makes
/// reuse of an operation ordinal for a different row a distinct durable
/// identity while preserving exact retry deduplication at one encoder version.
///
/// This store family has no in-place schema migration. Bumping the payload
/// encoder version therefore follows the same recreation-only operator flow as
/// a store schema bump. Cross-version retry continuity is bounded by that
/// policy: Lash does not claim that usage identities survive store recreation
/// or deduplicate across encoder versions.
#[derive(Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub struct RuntimeUsageDeltaIdentity {
    /// Canonical [`OperationId::storage_key`] of the operation that first
    /// staged this row.
    pub operation_storage_key: String,
    /// Zero-based row position within that operation's staged usage batch.
    pub entry_ordinal: u64,
    /// Version of the hand-written payload projection used by `payload_hash`.
    pub payload_encoding_version: u32,
    /// SHA-256 of the entry's versioned canonical projection, encoded as 64
    /// lowercase hexadecimal characters.
    pub payload_hash: String,
}

impl RuntimeUsageDeltaIdentity {
    /// Construct the full identity for `entry` using Lash's canonical payload
    /// encoding.
    pub fn for_entry(
        operation_storage_key: String,
        entry_ordinal: u64,
        entry: &crate::TokenLedgerEntry,
    ) -> Self {
        let payload_hash = usage_payload_identity_hash(entry);
        Self {
            operation_storage_key,
            entry_ordinal,
            payload_encoding_version: USAGE_PAYLOAD_ENCODING_V2,
            payload_hash,
        }
    }
}

#[cfg(test)]
mod usage_payload_identity_tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    macro_rules! define_usage_payload_v2_corpus {
        ($(
            $row:literal => crate::TokenLedgerEntry {
                source: $source:expr,
                model: $model:expr,
                usage: crate::TokenUsage {
                    input_tokens: $input_tokens:expr,
                    output_tokens: $output_tokens:expr,
                    cache_read_input_tokens: $cache_read_input_tokens:expr,
                    cache_write_input_tokens: $cache_write_input_tokens:expr,
                    reasoning_output_tokens: $reasoning_output_tokens:expr $(,)?
                } $(,)?
            }
        ),+ $(,)?) => {
            fn usage_payload_v2_corpus() -> Vec<(&'static str, crate::TokenLedgerEntry)> {
                vec![$((
                    $row,
                    crate::TokenLedgerEntry {
                        source: $source,
                        model: $model,
                        usage: crate::TokenUsage {
                            input_tokens: $input_tokens,
                            output_tokens: $output_tokens,
                            cache_read_input_tokens: $cache_read_input_tokens,
                            cache_write_input_tokens: $cache_write_input_tokens,
                            reasoning_output_tokens: $reasoning_output_tokens,
                        },
                    },
                )),+]
            }
        };
    }

    // The macro repeats the complete TokenLedgerEntry and TokenUsage shapes in
    // every fixture. A field addition cannot compile until the corpus is
    // updated; any projection change then moves the exact golden bytes.
    // Neither v2 DTO has an Option field, so empty/non-empty strings pin its
    // only absence/presence boundary instead of inventing None/Some semantics.
    define_usage_payload_v2_corpus! {
        "empty_strings_zero_usage" => crate::TokenLedgerEntry {
            source: String::new(),
            model: String::new(),
            usage: crate::TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            },
        },
        "representative_nested_usage" => crate::TokenLedgerEntry {
            source: "turn\0source".to_string(),
            model: "provider/model-λ".to_string(),
            usage: crate::TokenUsage {
                input_tokens: 1,
                output_tokens: 2,
                cache_read_input_tokens: 3,
                cache_write_input_tokens: 4,
                reasoning_output_tokens: 5,
            },
        },
        "all_counters_i64_max" => crate::TokenLedgerEntry {
            source: "max".to_string(),
            model: "max".to_string(),
            usage: crate::TokenUsage {
                input_tokens: i64::MAX,
                output_tokens: i64::MAX,
                cache_read_input_tokens: i64::MAX,
                cache_write_input_tokens: i64::MAX,
                reasoning_output_tokens: i64::MAX,
            },
        },
        "signed_counter_edges" => crate::TokenLedgerEntry {
            source: "signed".to_string(),
            model: "edges".to_string(),
            usage: crate::TokenUsage {
                input_tokens: i64::MIN,
                output_tokens: -1,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 1,
                reasoning_output_tokens: i64::MAX,
            },
        },
    }

    #[test]
    fn usage_payload_encoding_v2_golden_identity_corpus() {
        let rendered = usage_payload_v2_corpus()
            .into_iter()
            .enumerate()
            .map(|(entry_ordinal, (name, entry))| {
                let identity = RuntimeUsageDeltaIdentity::for_entry(
                    format!("golden:{name}"),
                    entry_ordinal as u64,
                    &entry,
                );
                let rendered_identity = format!(
                    "{}:{}:{}:{}",
                    identity.operation_storage_key,
                    identity.entry_ordinal,
                    identity.payload_encoding_version,
                    identity.payload_hash
                );
                (
                    name,
                    format!(
                        "{}|{}|{rendered_identity}",
                        hex(&usage_payload_identity_bytes(&entry)),
                        identity.payload_hash
                    ),
                )
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let expected = include_str!("testdata/usage_payload_encoding_v2.hex")
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(|line| {
                line.split_once('=')
                    .expect("name=preimage|payload_hash|full_identity golden corpus row")
            })
            .collect::<std::collections::BTreeMap<_, _>>();
        let missing = rendered
            .iter()
            .filter(|(name, _)| !expected.contains_key(*name))
            .map(|(name, actual)| format!("{name}={actual}"))
            .collect::<Vec<_>>();
        assert!(
            missing.is_empty(),
            "missing golden corpus rows:\n{}",
            missing.join("\n")
        );
        assert_eq!(rendered.len(), expected.len(), "golden corpus row count");
        for (name, actual) in rendered {
            let expected = expected
                .get(name)
                .expect("rendered golden corpus row was checked above");
            assert_eq!(
                actual, **expected,
                "v2 preimage, payload hash, or full identity moved for {name}"
            );
        }
    }

    #[test]
    fn usage_identity_version_participates_in_equality() {
        let entry = usage_payload_v2_corpus().pop().expect("usage fixture").1;
        let current = RuntimeUsageDeltaIdentity::for_entry("operation".to_string(), 0, &entry);
        assert_eq!(current.payload_encoding_version, USAGE_PAYLOAD_ENCODING_V2);
        let mut future = current.clone();
        future.payload_encoding_version += 1;
        assert_ne!(current, future);
    }

    #[test]
    fn runtime_commit_rejects_a_payload_version_hash_mismatch() {
        let entry = usage_payload_v2_corpus().pop().expect("usage fixture").1;
        let state = crate::RuntimeSessionState {
            session_id: "usage-payload-version".to_string(),
            ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
            ))
        };
        let mut commit = RuntimeCommit::persisted_state_for_test(&state, &[entry]);
        commit.usage_deltas[0].identity.payload_encoding_version += 1;

        let error = commit
            .validate_operation_session()
            .expect_err("future version with a v2 hash must be rejected");
        assert!(
            error
                .to_string()
                .contains("payload encoding version or hash does not match"),
            "unexpected validation error: {error}"
        );
    }
}

/// One identity-bearing usage row in a [`RuntimeCommit`].
///
/// Integrator class (ADR 0051): **store and durable-substrate implementors**
/// persist the identity beside the row and ignore a duplicate identity inside
/// the same transaction as the rest of the commit.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeUsageDelta {
    /// Stable exactly-once publication identity.
    pub identity: RuntimeUsageDeltaIdentity,
    /// Usage counters published under that identity.
    pub entry: crate::TokenLedgerEntry,
}

impl RuntimeUsageDelta {
    pub(crate) fn for_operation(
        operation: &OperationId,
        entries: &[crate::TokenLedgerEntry],
    ) -> Result<Vec<Self>, StoreError> {
        let operation_storage_key = operation.storage_key()?;
        entries
            .iter()
            .cloned()
            .enumerate()
            .map(|(ordinal, entry)| {
                let entry_ordinal = u64::try_from(ordinal).map_err(|_| {
                    StoreError::Backend(
                        "usage delta ordinal does not fit durable u64 identity".to_string(),
                    )
                })?;
                let identity = RuntimeUsageDeltaIdentity::for_entry(
                    operation_storage_key.clone(),
                    entry_ordinal,
                    &entry,
                );
                Ok(Self { identity, entry })
            })
            .collect()
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeCommitResult {
    pub head_revision: u64,
    pub checkpoint_ref: BlobRef,
    pub manifest: SessionCheckpoint,
    /// Leaf selected by the committed operation. Receipt replay returns the
    /// first attempt's value even when later commits have advanced the session.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_leaf_node_id: Option<String>,
    /// Store-realized timestamps for nodes appended by this operation.
    ///
    /// Node timestamps are clock-derived and excluded from commit intent, so a
    /// receipt replay must return the first attempt's values for the resident
    /// graph to converge with durable history.
    pub realized_node_timestamps: Vec<RealizedNodeTimestamp>,
    /// Usage identities actually present in the transaction represented by
    /// this result. A replay returns the first attempt's list, allowing a host
    /// to retain re-ridden staged rows that the first attempt did not carry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub committed_usage_delta_identities: Vec<RuntimeUsageDeltaIdentity>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub enqueued_queue_batches: Vec<crate::QueuedWorkBatch>,
    /// Canonical input applications settled by this idempotent turn commit.
    ///
    /// Keeping these identities in the durable turn-commit result lets hosts
    /// reconcile after the bounded live observation window has been lost.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turn_input_applications: Vec<crate::TurnInputApplication>,
    /// Whether the store answered this attempt from an existing durable receipt.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// set this transient decision bit when returning an earlier commit result;
    /// it is stored as `false` in the receipt itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub receipt_replayed: bool,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeTurnCommitStamp {
    pub operation: OperationId,
    /// Version of the append-request canonical encoding, or `None` for a
    /// non-append operation or a legacy receipt.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub identity_encoding_version: Option<u32>,
    /// SHA-256 identity of the semantic append request, or `None` when exact
    /// commit-hash replay semantics apply.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_identity_hash: Option<String>,
    /// Number of semantic nodes supplied by the append caller.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_node_count: Option<usize>,
    /// Branch ancestor named by the append caller, when one was required.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requested_ancestor_node_id: Option<String>,
}

impl RuntimeTurnCommitStamp {
    /// Binds one operation identity to a runtime commit for store implementors enforcing replay and
    /// idempotency at the atomic commit boundary.
    pub fn new(operation: OperationId) -> Self {
        Self {
            operation,
            identity_encoding_version: None,
            request_identity_hash: None,
            requested_node_count: None,
            requested_ancestor_node_id: None,
        }
    }

    pub(crate) fn append_session_nodes(
        operation: OperationId,
        requested_ancestor_node_id: Option<&str>,
        nodes: &[crate::SessionAppendNode],
    ) -> Result<Self, StoreError> {
        let request_identity_hash = commit_identity::append_request_identity_hash(
            &operation,
            requested_ancestor_node_id,
            nodes,
        )?;
        Ok(Self {
            operation,
            identity_encoding_version: Some(
                commit_identity::append_request_identity_encoding_version(nodes),
            ),
            request_identity_hash: Some(request_identity_hash),
            requested_node_count: Some(nodes.len()),
            requested_ancestor_node_id: requested_ancestor_node_id.map(str::to_string),
        })
    }
}
