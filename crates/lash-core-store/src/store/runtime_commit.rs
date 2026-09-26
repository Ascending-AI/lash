//! Runtime commit envelope and result types.

use super::{
    BlobRef, GraphAppend, HydratedSessionCheckpoint, OperationId, RealizedNodeTimestamp,
    SessionCheckpoint, SessionExecutionLeaseAuthority, StoreError, commit_identity,
    ensure_supported_record_schema_version_for_fleet, ensure_supported_schema_version_for_fleet,
};
use crate::SessionId;
use crate::TurnId;

const USAGE_PAYLOAD_FAMILY_VERSION: u8 = 4;
pub(super) const USAGE_PAYLOAD_ENCODING_V4: u32 = USAGE_PAYLOAD_FAMILY_VERSION as u32;

/// Permanent tag registry for runtime-usage payload identities.
///
/// Retired tags remain burned when variants are introduced in a later family
/// version. Version 4 canonical bytes, in order:
///
/// The shared framing header owns the domain and family version. Source and
/// model follow as length-prefixed strings, then the five signed counters in
/// declaration order as big-endian `i64` values, then the disposition: tag `0`
/// reported; tag `1` unreported followed by a `u64` hole count and, per hole,
/// the call id, the `u32` attempt ordinal, and an optional generation id
/// (`0` absent / `1` present + string); tag `2` reconciled followed by the
/// corrected call id and its `u32` attempt ordinal.
///
/// The full destructures deliberately omit `..`: adding a semantic field to
/// either durable DTO fails compilation until this projection is reconsidered.
fn usage_payload_identity_bytes(entry: &crate::TokenLedgerEntry) -> Vec<u8> {
    let crate::TokenLedgerEntry {
        source,
        model,
        usage,
        usage_disposition,
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
    // v4: the disposition is part of the row's identity, so an unreported
    // hole and a reconciled correction can never alias a reported row. Each
    // hole projects its full descriptor — v3 projected only a count, which is
    // why a reloaded row could not rebuild the attempts a host owes usage for.
    match usage_disposition {
        crate::LedgerUsageDisposition::Reported => identity.tag(0),
        crate::LedgerUsageDisposition::Unreported { attempts } => {
            identity.tag(1);
            identity.sequence(attempts, |identity, attempt| {
                let crate::UnreportedLedgerAttempt {
                    call_id,
                    attempt_ordinal,
                    generation_id,
                } = attempt;
                identity.string(call_id);
                identity.u32(*attempt_ordinal);
                identity.optional(generation_id.as_deref(), |identity, generation_id| {
                    identity.string(generation_id)
                });
            });
        }
        crate::LedgerUsageDisposition::Reconciled {
            call_id,
            attempt_ordinal,
        } => {
            identity.tag(2);
            identity.string(call_id);
            identity.u32(*attempt_ordinal);
        }
    }
    identity.finish()
}

fn usage_payload_identity_hash(entry: &crate::TokenLedgerEntry) -> String {
    crate::stable_hash::blake3_hex(
        "lash-runtime-usage-payload/v4",
        &usage_payload_identity_bytes(entry),
    )
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct RuntimeCommit {
    /// Host policy carried to the shared facade and backend validation seams.
    /// It is operational authority and intentionally excluded from the durable
    /// semantic commit identity projection.
    pub commit_budget: super::CommitBudget,
    pub session_id: SessionId,
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
    /// The drive fence of the admission this commit's root was sealed under
    /// (ADR 0105 §2, FIG-3600 S7). A transaction predicate like the lease
    /// fence, never commit content: the backend refuses the commit
    /// [`StoreError::StaleDriveFence`](super::StoreError::StaleDriveFence)
    /// unless it is still the session's current drive fence, checked in the
    /// commit's own transaction before anything is written. `None` for a
    /// commit no drive sealed (a runtime operation, a process-scoped turn).
    #[serde(skip)]
    pub drive_fence: Option<Box<super::DriveFence>>,
    /// The logical root's terminal evidence, present exactly on the commit of
    /// the root's final physical turn (FIG-3600 S7): written in this commit's
    /// transaction, refused [`StoreError::RootAlreadyTerminal`](super::StoreError::RootAlreadyTerminal)
    /// when the root already ended otherwise. An instruction to the store
    /// derived from the turn it commits, never commit content: like the
    /// fences, it is excluded from the commit's serialized form.
    #[serde(skip)]
    pub root_terminal: Option<Box<super::RootTerminalWrite>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_session_execution_lease: Option<SessionExecutionLeaseAuthority>,
    pub config: crate::PersistedSessionConfig,
    /// The config the committing root ran under, when it is not the config
    /// the commit writes: a root runs under its recorded execution view and
    /// writes the head's sticky config back (FIG-3841). The view is the
    /// commit's content, so the commit identity covers it in place of
    /// [`Self::config`]; the sticky config is the head's, not the operation's,
    /// and may have moved since the root first committed, so a redrive that
    /// replays the root's committed operation still answers its receipt. An
    /// input to the identity, never stored: `None` when the two agree.
    #[serde(skip)]
    pub execution_config: Option<Box<crate::PersistedSessionConfig>>,
    pub current_frame_node_id: Option<crate::FrameNodeId>,
    pub graph: GraphAppend,
    /// Resident leaf observed when this commit was built. For
    /// `GraphAppend::PreserveHead` this is the effective committed leaf bound
    /// into the whole-commit hash; for `Extend` it records the base the
    /// appended nodes extend.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub graph_base_leaf_node_id: Option<crate::NodeId>,
    pub checkpoint: HydratedSessionCheckpoint,
    /// Usage rows published atomically by this commit, each carrying a stable
    /// identity so retrying an unknown commit outcome cannot double-account.
    pub usage_deltas: Vec<RuntimeUsageDelta>,
    /// Bounded, non-transcript evidence settled with this turn record.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_evidence: Vec<crate::TurnFailureEvidence>,
    pub turn_commit: RuntimeTurnCommitStamp,
    /// Durable queued-run progress accepted atomically with this physical turn.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queued_run: Option<Box<super::QueuedRunCommit>>,
    pub completed_queue_claims: Vec<crate::QueuedWorkCompletion>,
    pub completed_turn_input_claims: Vec<crate::TurnInputCompletion>,
    /// Turn input the interrupted turn claimed at its terminal checkpoint and
    /// withheld for a follow-on turn that its cancellation means never runs
    /// (FIG-3531). The model never saw it, so it is never completed: in the
    /// same transaction the backend releases each claim under its own fence,
    /// and the cancellation's undelivered disposition then settles and
    /// records the rows exactly as it does an unclaimed active-turn row.
    /// Whole claims rather than completions: the release needs the claim
    /// identity and the rows it covers. Meaningful only beside
    /// `interrupted_turn_input_turn_id`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub undelivered_turn_input_claims: Vec<crate::turn_input_vocabulary::TurnInputClaim>,
    /// The follow-on the head owes once this commit publishes (ADR 0101 §3):
    /// the value the head holds after the write, not a delta. A frame-switch
    /// commit writes it, the follow-on's terminal commit clears or replaces
    /// it, and every other commit carries the head's value unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_follow_on: Option<super::PendingFollowOn>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_turn_input_turn_id: Option<TurnId>,
    /// Exact cancellation evidence returned by the authoritative turn gate.
    ///
    /// Absence explicitly selects ordinary non-cancellation re-deferral. Store
    /// implementations must never infer this decision from a request row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_turn_input_cancellation: Option<crate::TurnCancellationEvidence>,
    /// Transient predicate observed before the turn gate was settled. Backends
    /// compare it atomically before cancellation-dependent publication.
    #[serde(skip)]
    pub interrupted_turn_cancel_intent: Option<crate::TurnCancelIntentSnapshot>,
    /// Exact pending closure authorization consumed atomically with a fresh
    /// cancellation-dependent commit. Receipt replay is adjudicated first and
    /// may consume only the same exact still-pending authorization.
    #[serde(skip)]
    pub turn_cancel_closure_settlement: Option<crate::TurnCancelClosureSettlement>,
    /// Unique attachment-manifest rows this commit will stamp as adopted.
    /// Runtime assembly derives this from explicit attachment references and
    /// turn-owned write-ahead intents before store validation begins. Per ADR
    /// 0058 this count is a declared estimate, not a store query: replay can
    /// undercount prior-attempt turn-owned rows, and cancelled or failed puts
    /// can overcount — that residual is accepted, do not re-engineer it.
    #[serde(default)]
    pub adopted_intent_rows: u64,
    /// Attachment ids explicitly adopted by this commit. In the same
    /// transaction the backend also stamps every uncommitted manifest row owned
    /// by the turn id in `turn_commit.operation`, including ids that appear only in plain tool
    /// JSON. This list preserves typed-output and cross-turn re-references.
    /// Adoption is an upsert keyed on (session, attachment): when this session
    /// has no manifest row for an adopted id, the backend creates one —
    /// stamping the commit's intent time and copying the earliest proven
    /// upload evidence recorded under any session.
    pub committed_attachment_ids: Vec<crate::AttachmentId>,
}

#[cfg(any(test, feature = "testing"))]
impl RuntimeCommit {
    const fn recommended_test_commit_budget() -> super::CommitBudget {
        super::CommitBudget::bounded(1024 * 1024, 512)
    }

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

    #[track_caller]
    #[expect(
        clippy::expect_used,
        reason = "test-only constructor: node-id derivation failing here is a broken fixture, which must abort the test"
    )]
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
            crate::store::FleetFormat::current(),
        )
        .expect("test commit must be hashable")
    }

    #[expect(
        clippy::expect_used,
        reason = "test-only constructor: node-id derivation failing here is a broken fixture, which must abort the test"
    )]
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
            crate::store::FleetFormat::current(),
        )
        .expect("fixed-identity test commit must be hashable")
    }

    #[track_caller]
    #[expect(
        clippy::expect_used,
        reason = "test-only constructor: node-id derivation failing here is a broken fixture, which must abort the test"
    )]
    pub fn persisted_state_with_graph_commit(
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
            crate::store::FleetFormat::current(),
        )
        .expect("test graph commit must be hashable")
    }

    pub fn persisted_state_with_operation(
        state: &mut crate::RuntimeSessionState,
        usage_deltas: &[crate::TokenLedgerEntry],
        operation: OperationId,
    ) -> Result<(Self, Vec<crate::NodeId>), StoreError> {
        Self::persisted_state_with_operation_and_budget(
            state,
            usage_deltas,
            operation,
            Self::recommended_test_commit_budget(),
            crate::store::FleetFormat::current(),
        )
    }

    pub fn persisted_state_with_operation_and_staged_usage(
        state: &mut crate::RuntimeSessionState,
        usage_deltas: &[RuntimeUsageDelta],
        operation: OperationId,
    ) -> Result<(Self, Vec<crate::NodeId>), StoreError> {
        Self::persisted_state_with_operation_and_staged_usage_and_budget(
            state,
            usage_deltas,
            operation,
            Self::recommended_test_commit_budget(),
            crate::store::FleetFormat::current(),
        )
    }

    pub fn persisted_state_with_graph_commit_and_operation(
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
            crate::store::FleetFormat::current(),
        )
    }
}

/// Durable identity for one usage row submitted through a runtime commit.
///
/// The operation key, ordinal, payload-encoding version, and payload hash are assigned before
/// the first commit attempt and must be reused byte-for-byte until a commit containing the row
/// has a confirmed outcome.
///
/// `payload_hash` is lowercase hexadecimal BLAKE3 of Lash's hand-written,
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
#[derive(
    Clone, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
pub struct RuntimeUsageDeltaIdentity {
    /// Canonical [`OperationId::storage_key`] of the operation that first
    /// staged this row.
    pub operation_storage_key: String,
    /// Zero-based row position within that operation's staged usage batch.
    pub entry_ordinal: u64,
    /// Version of the hand-written payload projection used by `payload_hash`.
    pub payload_encoding_version: u32,
    /// BLAKE3 of the entry's versioned canonical projection, encoded as 64
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
            payload_encoding_version: USAGE_PAYLOAD_ENCODING_V4,
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

    macro_rules! define_usage_payload_v4_corpus {
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
                },
                usage_disposition: $usage_disposition:expr $(,)?
            }
        ),+ $(,)?) => {
            fn usage_payload_v4_corpus() -> Vec<(&'static str, crate::TokenLedgerEntry)> {
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
                        usage_disposition: $usage_disposition,
                    },
                )),+]
            }
        };
    }

    // The macro repeats the complete TokenLedgerEntry and TokenUsage shapes in
    // every fixture. A field addition cannot compile until the corpus is
    // updated; any projection change then moves the exact golden bytes.
    // Neither v4 DTO has an Option field, so empty/non-empty strings pin the
    // string absence/presence boundary; the disposition variants pin each
    // tagged arm of the v4 suffix (reported, unreported hole, reconciled
    // correction).
    define_usage_payload_v4_corpus! {
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
            usage_disposition: crate::LedgerUsageDisposition::Reported,
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
            usage_disposition: crate::LedgerUsageDisposition::Reported,
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
            usage_disposition: crate::LedgerUsageDisposition::Reported,
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
            usage_disposition: crate::LedgerUsageDisposition::Reported,
        },
        "unreported_after_abort_hole" => crate::TokenLedgerEntry {
            source: "turn".to_string(),
            model: "openrouter/model".to_string(),
            usage: crate::TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 0,
            },
            usage_disposition: crate::LedgerUsageDisposition::unreported([
                crate::UnreportedLedgerAttempt {
                    call_id: "call-hole-1".to_string(),
                    attempt_ordinal: 0,
                    generation_id: Some("gen-hole-1".to_string()),
                },
                // Pins the absent-generation tag: an attempt that never got far
                // enough to have a generation id is a fact, not missing data.
                crate::UnreportedLedgerAttempt {
                    call_id: "call-hole-2".to_string(),
                    attempt_ordinal: 3,
                    generation_id: None,
                },
            ]),
        },
        "reconciled_correction" => crate::TokenLedgerEntry {
            source: "turn".to_string(),
            model: "openrouter/model".to_string(),
            usage: crate::TokenUsage {
                input_tokens: 120,
                output_tokens: 35,
                cache_read_input_tokens: 0,
                cache_write_input_tokens: 0,
                reasoning_output_tokens: 7,
            },
            usage_disposition: crate::LedgerUsageDisposition::Reconciled {
                call_id: "call-7".to_string(),
                attempt_ordinal: 1,
            },
        },
    }

    #[test]
    fn usage_payload_encoding_v4_golden_identity_corpus() {
        let rendered = usage_payload_v4_corpus()
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
        let expected = include_str!("testdata/usage_payload_encoding_v4.hex")
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
                "v4 preimage, payload hash, or full identity moved for {name}"
            );
        }
    }

    #[test]
    fn usage_identity_version_participates_in_equality() {
        let entry = usage_payload_v4_corpus().pop().expect("usage fixture").1;
        let current = RuntimeUsageDeltaIdentity::for_entry("operation".to_string(), 0, &entry);
        assert_eq!(current.payload_encoding_version, USAGE_PAYLOAD_ENCODING_V4);
        let mut future = current.clone();
        future.payload_encoding_version += 1;
        assert_ne!(current, future);
    }

    #[test]
    fn runtime_commit_rejects_a_payload_version_hash_mismatch() {
        let entry = usage_payload_v4_corpus().pop().expect("usage fixture").1;
        let state = crate::RuntimeSessionState {
            session_id: SessionId::from("usage-payload-version"),
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
    pub fn for_operation(
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

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize, schemars::JsonSchema)]
pub struct RuntimeCommitReceipt {
    pub schema_version: u32,
    pub head_revision: u64,
    pub checkpoint_ref: BlobRef,
    pub manifest: SessionCheckpoint,
    /// Leaf selected by the committed operation. Receipt replay returns the
    /// first attempt's value even when later commits have advanced the session.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub committed_leaf_node_id: Option<crate::NodeId>,
    /// Node timestamps are clock-derived and excluded from commit intent, so a
    /// receipt replay must return the first attempt's values for the resident
    /// graph to converge with durable history.
    pub realized_node_timestamps: Vec<RealizedNodeTimestamp>,
    /// Usage identities actually present in the transaction represented by
    /// this result. A replay returns the first attempt's list, allowing a host
    /// to retain re-ridden staged rows that the first attempt did not carry.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub committed_usage_delta_identities: Vec<RuntimeUsageDeltaIdentity>,
    /// Bounded failure evidence owned by this durable turn settlement.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub failure_evidence: Vec<crate::TurnFailureEvidence>,
    /// The follow-on the head owes after this commit (ADR 0101 §3), so a
    /// replayed switch commit returns the fact it wrote.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_follow_on: Option<super::PendingFollowOn>,
    /// Canonical input applications settled by this idempotent turn commit.
    ///
    /// Keeping these identities in the durable turn-commit result lets hosts
    /// reconcile after the bounded live observation window has been lost.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub turn_input_applications: Vec<crate::TurnInputApplication>,
    /// Undelivered active-turn inputs disposed by this commit.
    ///
    /// This is a store-derived result, not commit intent, and is deliberately
    /// absent from `turn_commit_hash`.
    #[serde(
        default,
        skip_serializing_if = "crate::TurnCancelInputOutcome::is_empty"
    )]
    pub turn_cancel_input_outcome: crate::TurnCancelInputOutcome,
    /// Whether the store answered this attempt from an existing durable receipt.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**
    /// set this transient decision bit when returning an earlier commit result;
    /// it is stored as `false` in the receipt itself.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub receipt_replayed: bool,
}

/// The durable `result_json` receipt schema this build writes.
///
/// Version 1 is the first stamped encoding. Receipts written before the field
/// existed carry no `schema_version` at all and are refused as
/// [`StoreError::MissingRecordSchemaVersion`], matching the exact-version
/// refusal every other durable record follows.
///
/// Version 2 (FIG-3542) replaces the frame-handoff `enqueued_queue_batches`
/// with the `pending_follow_on` the commit left on the head. A version-1
/// receipt is refused, not converted.
pub const RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION: u32 = 2;

/// Stable record-kind label the receipt's decode refusals carry.
pub const RUNTIME_COMMIT_RECEIPT_RECORD_KIND: &str = "RuntimeCommitReceipt";

/// Decode one persisted `result_json` receipt body for a session's operation.
///
/// Every read of the receipt column fails closed through this one codec: a
/// payload that is not valid JSON, a missing or invalid `schema_version`, a
/// version this binary does not support, or a body outside the current shape
/// is a refusal, never a skipped row. `turn_id` is the row's operation storage
/// key, named so a refusal identifies the exact durable record.
///
/// This is the no-store form: it answers what a context with no recorded `F`
/// can admit — this build's newest version alone. Reads on a bound store go
/// through [`decode_runtime_commit_receipt_for_fleet`].
pub fn decode_runtime_commit_receipt(
    session_id: &SessionId,
    turn_id: &str,
    json: &str,
) -> Result<RuntimeCommitReceipt, StoreError> {
    decode_runtime_commit_receipt_for_fleet(
        session_id,
        turn_id,
        json,
        super::FleetFormat::current(),
    )
}

/// The fleet leg of [`decode_runtime_commit_receipt`]: the receipt admits the
/// version `fleet` records for the surface as well as this build's newest —
/// the `[N-1, N]` reader window of ADR 0106 §2 (FIG-3796). An admitted older
/// payload climbs to the newest through the surface's [`RecordUpcaster`]
/// hooks before it decodes.
pub fn decode_runtime_commit_receipt_for_fleet(
    session_id: &SessionId,
    turn_id: &str,
    json: &str,
    fleet: super::FleetFormat,
) -> Result<RuntimeCommitReceipt, StoreError> {
    let mut value: serde_json::Value =
        serde_json::from_str(json).map_err(|error| StoreError::StoredDataCorrupt {
            record_kind: RUNTIME_COMMIT_RECEIPT_RECORD_KIND,
            message: format!(
                "session `{session_id}` receipt `{turn_id}` is not valid JSON: {error}"
            ),
        })?;
    let actual = ensure_supported_record_schema_version_for_fleet(
        RUNTIME_COMMIT_RECEIPT_RECORD_KIND,
        &value,
        crate::surface_format!(RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION),
        fleet,
    )?;
    let window = fleet.read_window(crate::surface_format!(
        RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION
    ));
    if actual != window.newest() {
        super::upcast_json_record(
            RUNTIME_COMMIT_RECEIPT_RECORD_KIND,
            crate::surface_format!(RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION),
            actual,
            window.newest(),
            &mut value,
        )?;
    }
    serde_json::from_value(value).map_err(|error| StoreError::StoredDataCorrupt {
        record_kind: RUNTIME_COMMIT_RECEIPT_RECORD_KIND,
        message: format!(
            "session `{session_id}` receipt `{turn_id}` does not match the supported shape: {error}"
        ),
    })
}

/// Enforce the receipt version contract on an already-typed receipt.
///
/// Backends that hold the receipt as a value rather than serialized bytes
/// apply the same refusal the JSON codec does.
///
/// This is the no-store form; a bound store's reads go through
/// [`ensure_supported_receipt_version_for_fleet`].
pub fn ensure_supported_receipt_version(receipt: &RuntimeCommitReceipt) -> Result<(), StoreError> {
    ensure_supported_receipt_version_for_fleet(receipt, super::FleetFormat::current())
}

/// The fleet leg of [`ensure_supported_receipt_version`]: admits the version
/// `fleet` records for the surface alongside this build's newest (FIG-3796).
pub fn ensure_supported_receipt_version_for_fleet(
    receipt: &RuntimeCommitReceipt,
    fleet: super::FleetFormat,
) -> Result<(), StoreError> {
    ensure_supported_schema_version_for_fleet(
        RUNTIME_COMMIT_RECEIPT_RECORD_KIND,
        receipt.schema_version,
        crate::surface_format!(RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION),
        fleet,
    )
}

/// Replay identity carried by one runtime commit.
///
/// A plain commit carries no append identity. An append carries its canonical
/// version, hash, and node count as one variant; only the ancestor fence is
/// genuinely optional. A semantic boundary carries the FIG-2480 request-identity
/// receipt for exactly the record-config, create-session, and usage-ledger
/// operations: the operation is a typed field beside the versioned canonical
/// request hash, never recoverable only from the hash.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AppendRequestIdentity {
    /// A commit adjudicated only by its canonical runtime-commit hash.
    PlainCommit,
    /// A semantic append request with a comparable versioned identity.
    Append {
        /// Version of the canonical append-request encoding.
        #[serde(rename = "identity_encoding_version")]
        encoding_version: u32,
        /// SHA-256 identity of the semantic append request.
        #[serde(rename = "request_identity_hash")]
        request_hash: String,
        /// Number of semantic nodes supplied by the append caller.
        requested_node_count: u64,
        /// Branch ancestor named by the append caller, when one was required.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        requested_ancestor_node_id: Option<String>,
    },
    /// A non-append boundary request with a comparable versioned identity.
    ///
    /// Receipts under this identity answer "same request retried?" for the
    /// operation named by the typed tag and never reconstruct requests
    /// (store-as-continuation doctrine). One generic variant serves every
    /// adopting operation; per-operation variants were rejected in the
    /// FIG-869 ratification.
    SemanticBoundary {
        /// Operation family that owns this receipt identity.
        operation: SemanticBoundaryOperation,
        /// Version of that operation's canonical request encoding.
        #[serde(rename = "identity_encoding_version")]
        encoding_version: u32,
        /// BLAKE3 identity of the canonical semantic boundary request.
        #[serde(rename = "request_identity_hash")]
        request_hash: String,
    },
}

/// Boundary operations that adjudicate retries through a semantic-boundary
/// receipt identity (FIG-2480).
///
/// This vocabulary is persisted replay identity: variants serialize as the
/// exact operation-key strings and must never be renamed (ADR 0063 discipline
/// applies). `commit_identity` accepts the identity for exactly these
/// operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SemanticBoundaryOperation {
    /// `record-config`: persist the materialized protocol configuration.
    #[serde(rename = "record-config")]
    RecordConfig,
    /// `create-session`: register a newly materialized child session.
    #[serde(rename = "create-session")]
    CreateSession,
    /// `usage-ledger`: flush staged child usage after its turn.
    #[serde(rename = "usage-ledger")]
    UsageLedger,
}

impl SemanticBoundaryOperation {
    /// The [`OperationId::key`] this identity family is valid for.
    pub fn operation_key(self) -> &'static str {
        match self {
            Self::RecordConfig => "record-config",
            Self::CreateSession => "create-session",
            Self::UsageLedger => "usage-ledger",
        }
    }

    /// Returns `None` for every key outside the adopted set; callers refuse
    /// the identity rather than guessing a family.
    pub fn from_operation_key(key: &str) -> Option<Self> {
        match key {
            "record-config" => Some(Self::RecordConfig),
            "create-session" => Some(Self::CreateSession),
            "usage-ledger" => Some(Self::UsageLedger),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RuntimeTurnCommitStamp {
    pub operation: OperationId,
    /// Plain-commit or append-request replay identity.
    ///
    /// Integrator class (ADR 0051): **store and durable-substrate implementors**.
    // No durable JSON path writes this stamp; the column path is the durable one.
    pub append_request_identity: AppendRequestIdentity,
}

impl RuntimeTurnCommitStamp {
    /// Binds one operation identity to a runtime commit for store implementors enforcing replay and
    /// idempotency at the atomic commit boundary.
    pub fn new(operation: OperationId) -> Self {
        Self {
            operation,
            append_request_identity: AppendRequestIdentity::PlainCommit,
        }
    }

    pub fn append_session_nodes(
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
            append_request_identity: AppendRequestIdentity::Append {
                encoding_version: commit_identity::APPEND_REQUEST_IDENTITY_ENCODING_VERSION,
                request_hash: request_identity_hash,
                requested_node_count: u64::try_from(nodes.len()).map_err(|_| {
                    StoreError::Backend("append requested-node count does not fit u64".to_string())
                })?,
                requested_ancestor_node_id: requested_ancestor_node_id.map(str::to_string),
            },
        })
    }
}

impl RuntimeCommit {
    /// Stamp the semantic-boundary replay identity derived from this commit's
    /// operation and canonical request content (FIG-2480).
    ///
    /// Call this as the final step of building a record-config,
    /// create-session, or usage-ledger commit, after every semantic field is
    /// in place: the identity hash is computed from the commit itself, and
    /// store validation refuses a stamp that no longer matches the content it
    /// rides with. Refuses every operation outside the adopted set.
    pub fn stamp_semantic_boundary(&mut self) -> Result<(), StoreError> {
        let operation =
            SemanticBoundaryOperation::from_operation_key(&self.turn_commit.operation.key)
                .ok_or_else(|| {
                    StoreError::Backend(format!(
                        "semantic-boundary receipt identity is not defined for operation `{}`",
                        self.turn_commit.operation.key
                    ))
                })?;
        let (encoding_version, request_hash) =
            super::semantic_boundary::semantic_boundary_request_identity(self, operation)?;
        self.turn_commit.append_request_identity = AppendRequestIdentity::SemanticBoundary {
            operation,
            encoding_version,
            request_hash,
        };
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn append_identity_cannot_deserialize_half_populated() {
        let operation = OperationId::new(
            crate::ExecutionScope::runtime_operation("partial-json-stamp"),
            "append-session-nodes",
        );
        let json = serde_json::json!({
            "operation": operation,
            "append_request_identity": {
                "kind": "append",
                "identity_encoding_version": 2,
                "requested_node_count": 1
            }
        });

        serde_json::from_value::<RuntimeTurnCommitStamp>(json)
            .expect_err("append identity without its hash must be refused");
    }

    #[test]
    fn semantic_boundary_wire_values_match_the_persisted_receipt_encoding() {
        // Hand-spelled wire literals: this vocabulary is persisted replay
        // identity, and a serde-rename drift would be globally self-consistent
        // while silently orphaning every stored receipt.
        for (operation, wire_operation) in [
            (SemanticBoundaryOperation::RecordConfig, "record-config"),
            (SemanticBoundaryOperation::CreateSession, "create-session"),
            (SemanticBoundaryOperation::UsageLedger, "usage-ledger"),
        ] {
            let identity = AppendRequestIdentity::SemanticBoundary {
                operation,
                encoding_version: 1,
                request_hash: "boundary-hash".to_string(),
            };
            let encoded = serde_json::to_value(&identity).expect("encode semantic identity");
            assert_eq!(
                encoded,
                serde_json::json!({
                    "kind": "semantic_boundary",
                    "operation": wire_operation,
                    "identity_encoding_version": 1,
                    "request_identity_hash": "boundary-hash",
                }),
                "semantic-boundary wire shape moved for {wire_operation}"
            );
            let decoded: AppendRequestIdentity =
                serde_json::from_value(encoded).expect("decode semantic identity");
            assert_eq!(decoded, identity, "operation tag must round-trip");
            assert_eq!(operation.operation_key(), wire_operation);
            assert_eq!(
                SemanticBoundaryOperation::from_operation_key(wire_operation),
                Some(operation)
            );
        }
    }

    #[test]
    fn semantic_boundary_identity_refuses_unknown_operations() {
        assert_eq!(
            SemanticBoundaryOperation::from_operation_key("append-session-nodes"),
            None,
            "append must never resolve to a semantic-boundary family"
        );
        serde_json::from_value::<AppendRequestIdentity>(serde_json::json!({
            "kind": "semantic_boundary",
            "operation": "initial-park",
            "identity_encoding_version": 1,
            "request_identity_hash": "boundary-hash",
        }))
        .expect_err("an unadopted operation tag must be refused at deserialization");
        serde_json::from_value::<AppendRequestIdentity>(serde_json::json!({
            "kind": "semantic_boundary",
            "operation": "record-config",
            "identity_encoding_version": 1,
        }))
        .expect_err("a semantic-boundary identity without its hash must be refused");
    }
}
