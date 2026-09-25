use super::*;
use lash_sansio::ProcessId;
use lash_sansio::SessionId;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use lash_conformance::{
    StoreContractHandles, StoreContractOp, StoreContractScenario, sample_store_contract_operations,
};
use lash_core::{
    AdmittedScope, AttachmentCreateMeta, AttachmentStore, AwaitEventWaitIdentity,
    CancellationToken, ChildDrainOutcome, EffectAddress, EffectGroupHandle, EffectHost,
    EffectJournalRetirement, ExecutionScope, GroupExecutors, GroupWakePolicy, LoserPolicy,
    MediaType, ProcessExecutionEnvRef, ProcessIdentity, ProcessInput, ProcessOriginator,
    Resolution, RuntimeAttribution, RuntimeEffectCommand, RuntimeEffectController,
    RuntimeEffectEnvelope, RuntimeEffectGroup, RuntimeEffectLocalExecutor, RuntimeEffectOutcome,
    RuntimePersistence, SessionScope, StoreEffectGroupDrain, TriggerCommand, TriggerInputBinding,
    TriggerOccurrenceRequest, TriggerOwnerScope, TriggerStore, TriggerSubscriptionDraft,
    facade_support::LeaseTimings,
    facade_support::SystemClock,
    facade_support::effect_replay_driver::{EffectGroupChildCommitOutcome, GroupChildFinalCommit},
};
use lash_postgres_store::{PostgresEffectHost, PostgresEffectReplayOptions};
use lash_s3_store::{S3AttachmentStore, S3AttachmentStoreConfig};
use lash_sqlite_store::{
    SqliteEffectHost, SqliteEffectReplayOptions, SqliteProcessRegistry, SqliteTriggerStore,
    Store as SqliteStore,
};

const DEFAULT_CASES: usize = 4;
const DEFAULT_SEED: u64 = 852;
const OPS_PER_CASE: usize = 55;
const SURFACE_SESSION: &str = "surface-session";
const SURFACE_TURN: &str = "surface-turn";
/// The session the scenario's runtime store is bound to: the runtime ops the
/// generated contract history drives all commit against it, so a turn park
/// lands in a session the history already has.
const SURFACE_RUNTIME_SESSION: &str = "prop-runtime-session";
#[path = "generated_surface/groups.rs"]
mod groups;
use groups::*;

const ALL_SURFACE_OPERATION_KINDS: &[&str] = &[
    "register",
    "first_start",
    "enter_wait",
    "clear_wait",
    "set_external_ref",
    "signal",
    "cancel_request",
    "terminal",
    "add_observer",
    "remove_observer",
    "retarget",
    "claim_lease",
    "release_lease",
    "claim_wake",
    "mark_wake",
    "discard_wake",
    "defer_wake",
    "enqueue_wake",
    "consume_wake",
    "prune",
    "compact_tombstones",
    "trigger_register",
    "trigger_disable",
    "trigger_occurrence",
    "trigger_occurrence_null_source",
    "process_signal_zero",
    "effect_record",
    "tool_intent_batch",
    "await_resolve",
    "await_revoke_session",
    "effect_group_open",
    "effect_group_release",
    "effect_group_release_both",
    "effect_group_await",
    "effect_group_close",
    "effect_group_commit",
    "effect_group_commit_both",
    "effect_group_drain_blocked",
    "effect_group_crash",
    "effect_group_drain",
    "turn_park_record",
    "turn_park_load",
    "turn_park_settle",
];

#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "surface", content = "operation", rename_all = "snake_case")]
enum SurfaceOperation {
    StoreContract(StoreContractOp),
    TriggerRegister {
        key: u8,
    },
    TriggerDisable {
        key: u8,
    },
    TriggerOccurrence {
        key: u8,
    },
    TriggerOccurrenceNullSource {
        key: u8,
    },
    ProcessSignalZero {
        negative: bool,
    },
    EffectRecord {
        key: u8,
        duration_ms: u8,
    },
    ToolIntentBatch,
    AwaitResolve {
        key: u8,
    },
    AwaitRevokeSession,
    /// Journal one effect and one resolved promise under a runtime-operation
    /// scope: the rows only `RetireRuntimeOperation` can reclaim (FIG-2499,
    /// FIG-2500).
    RuntimeOperationRecord {
        key: u8,
    },
    /// Retire one runtime-operation scope: effect rows, groups, and promise
    /// rows in one transaction plus the permanent fence.
    RetireRuntimeOperation {
        key: u8,
    },
    /// Open a durable effect group on a fresh per-group opener (one thread,
    /// one runtime, one host), with `children` parked on release flags.
    /// `cancel_losers` declares the group's loser disposition.
    EffectGroupOpen {
        group: u8,
        children: u8,
        cancel_losers: bool,
    },
    /// Flip one parked child's release flag. The child's own writes are
    /// already fenced wherever a release lands, so the release waits for the
    /// child's settlement to journal before returning — the generated prefix
    /// stays deterministic.
    EffectGroupRelease {
        group: u8,
        position: u8,
    },
    /// Flip two parked children's release flags together, then wait for both
    /// settlements. Releasing one committed child alone is not a barrier it
    /// can always pass: when it holds the higher `commit_seq`, its own drain
    /// is gated on the lower rank discharging first, so a one-sided release
    /// would park behind a sibling nobody released.
    EffectGroupReleaseBoth {
        group: u8,
        a: u8,
        b: u8,
    },
    /// Serve the next settlement rank on the group's open handle.
    EffectGroupAwait {
        group: u8,
    },
    /// Close the group under its declared loser disposition.
    EffectGroupClose {
        group: u8,
    },
    /// Commit one child's §4 boundary out from under its parked executor —
    /// the committed-but-undrained durable state (`committed` + `in_progress`
    /// + `drain_input`, no terminal) a crash before discharge leaves behind.
    EffectGroupCommit {
        group: u8,
        position: u8,
    },
    /// Issue two children's §4 commits concurrently on the same opener.
    /// Which position wins which `commit_seq` is a scheduler fact; the
    /// recorded outcome is sorted by sequence, and a local law check requires
    /// both commits to land on distinct consecutive positions.
    EffectGroupCommitBoth {
        group: u8,
        a: u8,
        b: u8,
    },
    /// Probe the durable §5 barrier for the child holding commit rank `rank`
    /// (1-based into the group's sorted recorded `commit_seq` values). Keyed
    /// by rank rather than position because the concurrent group assigns
    /// ranks to positions nondeterministically.
    EffectGroupDrainBlocked {
        group: u8,
        rank: u8,
    },
    /// Kill the group's opener: its runtime is shut down, its claims stop
    /// renewing and lapse, and later opener-bound commands answer a fixed
    /// "opener crashed" refusal.
    EffectGroupCrash {
        group: u8,
    },
    /// Run the successor host's drain over the group, polling while children
    /// report `LeaseLive` or `Contested`, bounded by `GROUP_OP_BOUND`.
    EffectGroupDrain {
        group: u8,
    },
    /// Park turn `key` of the runtime session with generated but valid
    /// fields (FIG-3586). One record per session: a second record replaces
    /// the first.
    TurnParkRecord {
        key: u8,
    },
    /// Read the runtime session's park back through `load_turn_park` and
    /// record the answer for the cross-backend comparison.
    TurnParkLoad,
    /// Commit turn `key` on the runtime session. A turn's commit settles its
    /// own park inside the commit's transaction and leaves another turn's
    /// (FIG-3586), so which park a settle clears is decided by the turn it
    /// names.
    TurnParkSettle {
        key: u8,
    },
}

impl SurfaceOperation {
    fn kind(&self) -> &'static str {
        match self {
            Self::StoreContract(operation) => match operation {
                StoreContractOp::Register { .. } => "register",
                StoreContractOp::FirstStart { .. } => "first_start",
                StoreContractOp::EnterWait { .. } => "enter_wait",
                StoreContractOp::ClearWait { .. } => "clear_wait",
                StoreContractOp::SetExternalRef { .. } => "set_external_ref",
                StoreContractOp::Signal { .. } => "signal",
                StoreContractOp::CancelRequest { .. } => "cancel_request",
                StoreContractOp::Terminal { .. } => "terminal",
                StoreContractOp::AddObserver { .. } => "add_observer",
                StoreContractOp::RemoveObserver { .. } => "remove_observer",
                StoreContractOp::Retarget { .. } => "retarget",
                StoreContractOp::ClaimLease { .. } => "claim_lease",
                StoreContractOp::ReleaseLease { .. } => "release_lease",
                StoreContractOp::ClaimWake => "claim_wake",
                StoreContractOp::MarkWake { .. } => "mark_wake",
                StoreContractOp::DiscardWake { .. } => "discard_wake",
                StoreContractOp::DeferWake { .. } => "defer_wake",
                StoreContractOp::EnqueueWake { .. } => "enqueue_wake",
                StoreContractOp::ConsumeWake { .. } => "consume_wake",
                StoreContractOp::Prune { .. } => "prune",
                StoreContractOp::CompactTombstones { .. } => "compact_tombstones",
            },
            Self::TriggerRegister { .. } => "trigger_register",
            Self::TriggerDisable { .. } => "trigger_disable",
            Self::TriggerOccurrence { .. } => "trigger_occurrence",
            Self::TriggerOccurrenceNullSource { .. } => "trigger_occurrence_null_source",
            Self::ProcessSignalZero { .. } => "process_signal_zero",
            Self::EffectRecord { .. } => "effect_record",
            Self::ToolIntentBatch => "tool_intent_batch",
            Self::AwaitResolve { .. } => "await_resolve",
            Self::AwaitRevokeSession => "await_revoke_session",
            Self::RuntimeOperationRecord { .. } => "runtime_operation_record",
            Self::RetireRuntimeOperation { .. } => "retire_runtime_operation",
            Self::EffectGroupOpen { .. } => "effect_group_open",
            Self::EffectGroupRelease { .. } => "effect_group_release",
            Self::EffectGroupReleaseBoth { .. } => "effect_group_release_both",
            Self::EffectGroupAwait { .. } => "effect_group_await",
            Self::EffectGroupClose { .. } => "effect_group_close",
            Self::EffectGroupCommit { .. } => "effect_group_commit",
            Self::EffectGroupCommitBoth { .. } => "effect_group_commit_both",
            Self::EffectGroupDrainBlocked { .. } => "effect_group_drain_blocked",
            Self::EffectGroupCrash { .. } => "effect_group_crash",
            Self::EffectGroupDrain { .. } => "effect_group_drain",
            Self::TurnParkRecord { .. } => "turn_park_record",
            Self::TurnParkLoad => "turn_park_load",
            Self::TurnParkSettle { .. } => "turn_park_settle",
        }
    }
}

#[path = "generated_surface/observation.rs"]
mod observation;
pub(super) use observation::*;

struct SurfaceRunner {
    name: &'static str,
    scenario: StoreContractScenario,
    process_registry: Arc<dyn lash_core::ProcessRegistry>,
    /// Where the runner's process service publishes a started process's
    /// execution environment.
    process_env_store: Arc<dyn lash_core::ProcessExecutionEnvStore>,
    trigger_store: Arc<dyn TriggerStore>,
    effect_host: Arc<dyn EffectHost>,
    /// `None` on the memory runner: it exercises no durable groups, and its
    /// `book` alone keeps the fixed refusal strings identical.
    groups: Option<GroupSurface>,
    book: BTreeMap<u8, GroupBook>,
    group_outcomes: Vec<serde_json::Value>,
    /// The session-bound runtime store the scenario drives; the turn-park
    /// ops apply to it directly.
    runtime: Arc<dyn RuntimePersistence>,
    /// The `load_turn_park` answers this runner observed, in operation
    /// order. Compared across every backend: each lane's runtime store is a
    /// real durable one.
    turn_park_loads: Vec<serde_json::Value>,
    reader: SurfaceReader,
}

struct SurfaceIntentProvider;

impl SurfaceIntentProvider {
    fn definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "tool:surface_intent_provider",
            "surface_intent_provider",
            "Return a literal result and two durable process intents.",
            lash_core::ToolDefinition::default_input_schema(),
            serde_json::json!({"type": "object"}),
        )
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for SurfaceIntentProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![Self::definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        (name == "surface_intent_provider").then(|| Arc::new(Self::definition().contract()))
    }

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let parent_scope = call
            .context
            .child_process_parent_scope()
            .expect("recorded attempt carries its parent scope");

        assert_eq!(call.context.session_id(), SURFACE_SESSION);
        assert_eq!(call.context.execution_scope_id(), SURFACE_TURN);
        assert_eq!(call.context.tool_call_id(), Some("surface-intent-call"));
        assert_eq!(call.context.attempt_number(), 1);
        assert_eq!(call.context.max_attempts(), 1);
        lash_core::ToolAttemptOutcome::done(
            lash_core::ToolOutcomeDone::ok(serde_json::json!({"ok": true})),
            lash_core::ToolIntents::v3(
                (0..2)
                    .map(|index| {
                        lash_core::ToolIntent::StartProcess(Box::new(
                            lash_core::StartProcessIntent {
                                session_id: SessionId::from(SURFACE_SESSION.to_string()),
                                declaration: lash_core::ProcessStartDeclaration::external(
                                    ProcessOriginator::session(SessionScope::new(SURFACE_SESSION)),
                                    serde_json::json!({
                                        "source": "literal-intent-row",
                                        "index": index,
                                    }),
                                    lash_core::ProcessLifecyclePolicy::new(
                                        parent_scope.clone(),
                                        lash_core::OnParentEnd::Cancel,
                                    ),
                                ),
                            },
                        ))
                    })
                    .collect(),
            ),
        )
    }
}

struct LiteralFrameController {
    inner: lash_core::ScopedEffectController<'static>,
    frames: Arc<Mutex<Vec<RuntimeEffectEnvelope>>>,
}

#[async_trait::async_trait]
impl lash_core::AwaitEventResolver for LiteralFrameController {
    async fn prepare_completion_key(
        &self,
        scope: &lash_core::ExecutionScope,
        wait: lash_core::AwaitEventWaitIdentity,
        may_defer: bool,
    ) -> Result<lash_core::CompletionKeyPreparation, lash_core::RuntimeError> {
        self.inner
            .controller()
            .prepare_completion_key(scope, wait, may_defer)
            .await
    }
}

#[async_trait::async_trait]
impl lash_core::RuntimeEffectController for LiteralFrameController {
    fn effect_journaling(&self) -> lash_core::EffectJournaling {
        self.inner.controller().effect_journaling()
    }

    async fn drive_independent_effect_work<'work>(
        &self,
        work: Vec<lash_core::IndependentEffectWork<'work>>,
    ) {
        self.inner
            .controller()
            .drive_independent_effect_work(work)
            .await;
    }

    fn wants_segment_boundary(
        &self,
        progress: &lash_core::SegmentProgress,
    ) -> Option<lash_core::BoundaryReason> {
        self.inner.controller().wants_segment_boundary(progress)
    }

    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, lash_core::RuntimeEffectControllerError> {
        self.frames
            .lock()
            .expect("literal frame recorder lock")
            .push(envelope.clone());
        self.inner
            .controller()
            .execute_effect(envelope, local_executor)
            .await
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.inner.controller().open_effect_group(group).await
    }

    fn register_group_executors(
        &self,
        executors: Arc<dyn lash_core::GroupExecutors>,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.controller().register_group_executors(executors)
    }

    fn native_effect_groups_substrate(&self) -> Option<Arc<dyn std::any::Any + Send + Sync>> {
        self.inner.controller().native_effect_groups_substrate()
    }

    fn group_child_scoped_controller(
        &self,
        admitted: lash_core::AdmittedScope,
        binding: lash_core::GroupChildBinding,
    ) -> Result<Option<lash_core::ScopedEffectController<'static>>, lash_core::RuntimeError> {
        self.inner
            .controller()
            .group_child_scoped_controller(admitted, binding)
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.inner
            .controller()
            .await_next_settlement(handle, cancel)
            .await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner
            .controller()
            .close_effect_group(handle, disposition)
            .await
    }

    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<
        Option<lash_core::runtime::effect::RankedGroupSettlement>,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner
            .controller()
            .read_group_settlement(group_key, rank)
            .await
    }

    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner
            .controller()
            .commit_group_child_final(commit)
            .await
    }

    async fn await_group_child_drain_admission(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner
            .controller()
            .await_group_child_drain_admission(group_key, commit_seq)
            .await
    }
}

fn surface_operation_id(key: u8) -> String {
    format!("surface-op-{key}")
}

fn surface_parked_turn_id(key: u8) -> lash_core::TurnId {
    lash_core::TurnId::from(format!("surface-parked-turn-{key}"))
}

/// One deterministic park record per key, cycling every reason shape the
/// persisted `reason_json` carries so each variant round-trips.
fn surface_turn_park(key: u8) -> lash_core::store::TurnParkWrite {
    let message = format!("surface park {key}: the journal refused replay");
    let reason = match key % 4 {
        0 => lash_core::store::ParkReason::ReplayDivergence { message },
        1 => lash_core::store::ParkReason::KeyFormatCutover { message },
        2 => lash_core::store::ParkReason::BindingDrift { message },
        _ => lash_core::store::ParkReason::EffectReplayDivergence {
            effect_kind: "llm_call".to_string(),
            message,
        },
    };
    lash_core::store::TurnParkWrite {
        session_id: SessionId::from(SURFACE_RUNTIME_SESSION.to_string()),
        turn_id: surface_parked_turn_id(key),
        reason,
        at_ms: 1_000 + u64::from(key),
    }
}

fn generated_surface_operations(seed: u64) -> Vec<SurfaceOperation> {
    let contract = sample_store_contract_operations(seed, OPS_PER_CASE - 33);
    let mut operations = vec![
        SurfaceOperation::TriggerRegister { key: 0 },
        SurfaceOperation::TriggerOccurrence { key: 0 },
        SurfaceOperation::EffectRecord {
            key: 0,
            duration_ms: 1,
        },
        SurfaceOperation::ToolIntentBatch,
        SurfaceOperation::AwaitResolve { key: 0 },
    ];
    for (index, operation) in contract.into_iter().enumerate() {
        operations.push(SurfaceOperation::StoreContract(operation));
        if index == 3 {
            operations.push(SurfaceOperation::RuntimeOperationRecord { key: 0 });
            operations.push(SurfaceOperation::RuntimeOperationRecord { key: 1 });
        }

        if index == 5 {
            operations.push(SurfaceOperation::TriggerDisable { key: 0 });
        }
        // Park turn 0, read it back, then replace it with turn 1's park —
        // one record per session. Turn 0's settle leaves turn 1's park (a
        // commit clears only the park naming its own turn), and turn 1's
        // settle clears it.
        if index == 6 {
            operations.extend([
                SurfaceOperation::TurnParkLoad,
                SurfaceOperation::TurnParkRecord { key: 0 },
                SurfaceOperation::TurnParkLoad,
                SurfaceOperation::TurnParkRecord { key: 1 },
                SurfaceOperation::TurnParkLoad,
                SurfaceOperation::TurnParkSettle { key: 0 },
                SurfaceOperation::TurnParkLoad,
                SurfaceOperation::TurnParkSettle { key: 1 },
                SurfaceOperation::TurnParkLoad,
            ]);
        }
        // Final lands before the cancel: child 0 commits and discharges at
        // rank 1, then closing the group decides child 1's cancel at rank 2.
        // Releasing it afterwards must journal nothing further.
        if index == 7 {
            operations.extend([
                SurfaceOperation::EffectGroupOpen {
                    group: 0,
                    children: 2,
                    cancel_losers: true,
                },
                SurfaceOperation::EffectGroupRelease {
                    group: 0,
                    position: 0,
                },
                SurfaceOperation::EffectGroupAwait { group: 0 },
                SurfaceOperation::EffectGroupClose { group: 0 },
                SurfaceOperation::EffectGroupRelease {
                    group: 0,
                    position: 1,
                },
            ]);
        }
        // Cancel is decided before the late final: closing under
        // `LoserPolicy::Cancel` decides both parked children, and the §4
        // commit that lands afterwards is refused.
        if index == 10 {
            operations.extend([
                SurfaceOperation::EffectGroupOpen {
                    group: 1,
                    children: 2,
                    cancel_losers: true,
                },
                SurfaceOperation::EffectGroupClose { group: 1 },
                SurfaceOperation::EffectGroupCommit {
                    group: 1,
                    position: 0,
                },
            ]);
        }
        // Two finals race the §4 boundary concurrently: both commit, on
        // distinct consecutive ranks; the durable barrier then blocks the
        // higher rank until the lower has discharged.
        if index == 13 {
            operations.extend([
                SurfaceOperation::EffectGroupOpen {
                    group: UNORDERED_GROUP,
                    children: 2,
                    cancel_losers: false,
                },
                SurfaceOperation::EffectGroupCommitBoth {
                    group: UNORDERED_GROUP,
                    a: 0,
                    b: 1,
                },
                SurfaceOperation::EffectGroupDrainBlocked {
                    group: UNORDERED_GROUP,
                    rank: 1,
                },
                SurfaceOperation::EffectGroupDrainBlocked {
                    group: UNORDERED_GROUP,
                    rank: 2,
                },
                SurfaceOperation::EffectGroupReleaseBoth {
                    group: UNORDERED_GROUP,
                    a: 0,
                    b: 1,
                },
                SurfaceOperation::EffectGroupAwait {
                    group: UNORDERED_GROUP,
                },
                SurfaceOperation::EffectGroupAwait {
                    group: UNORDERED_GROUP,
                },
                SurfaceOperation::EffectGroupClose {
                    group: UNORDERED_GROUP,
                },
            ]);
        }
        // Committed before discharge, then the opener dies: the successor's
        // drain discharges child 0 from its recorded `drain_input` at the
        // committed rank and executes child 1 beneath it.
        if index == 16 {
            operations.extend([
                SurfaceOperation::EffectGroupOpen {
                    group: 3,
                    children: 2,
                    cancel_losers: false,
                },
                SurfaceOperation::EffectGroupCommit {
                    group: 3,
                    position: 0,
                },
                SurfaceOperation::EffectGroupCrash { group: 3 },
                SurfaceOperation::EffectGroupRelease {
                    group: 3,
                    position: 0,
                },
                SurfaceOperation::EffectGroupRelease {
                    group: 3,
                    position: 1,
                },
                SurfaceOperation::EffectGroupDrain { group: 3 },
            ]);
        }
        if index == 14 {
            operations.push(SurfaceOperation::RetireRuntimeOperation { key: 0 });
        }
        if index == 11 {
            operations.push(SurfaceOperation::AwaitRevokeSession);
        }
        if index == 17 {
            operations.push(SurfaceOperation::EffectRecord {
                key: 0,
                duration_ms: 1,
            });
        }
    }
    operations
}

impl SurfaceRunner {
    #[expect(
        clippy::expect_used,
        reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
    )]
    async fn apply(&mut self, operation: &SurfaceOperation) -> Result<(), String> {
        match operation {
            SurfaceOperation::StoreContract(operation) => self.scenario.apply(operation).await,
            SurfaceOperation::TriggerRegister { key } => {
                let subscription_key = format!("surface-{key}");
                let mut inputs = BTreeMap::new();
                inputs.insert("event".to_string(), TriggerInputBinding::Event);
                let command = TriggerCommand::Register {
                    owner_scope: TriggerOwnerScope::session(SURFACE_SESSION),
                    actor: ProcessOriginator::session(SessionScope::new(SURFACE_SESSION)),
                    draft: TriggerSubscriptionDraft {
                        source_capture: lash_core::TriggerSourceCapture::provider(
                            ["surface", "event"],
                            lash_core::LashSchema::any(),
                            "surface-provider",
                            serde_json::json!({"account": "surface"}),
                        ),
                        subscription_key,
                        env_ref: ProcessExecutionEnvRef::new("surface-env"),
                        wake_target: Some(SessionScope::new(SURFACE_SESSION)),
                        name: Some("surface-worker".to_string()),
                        source_type: "surface.event".to_string(),
                        source_key: format!("source-{key}"),
                        source: serde_json::json!({"source": key}),
                        payload_schema: lash_core::LashSchema::any(),
                        target: ProcessInput::Engine {
                            kind: "surface".to_string(),
                            payload: serde_json::json!({"key": key}),
                        },
                        target_identity: ProcessIdentity::labelled(
                            "surface",
                            Some("surface-worker".to_string()),
                        ),
                        event_types: Vec::new(),
                        input_template: inputs,
                        target_label: Some("surface-worker".to_string()),
                    },
                };
                self.trigger_store
                    .execute_command(&format!("surface-register-{key}"), command)
                    .await
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::TriggerDisable { key } => {
                let command = TriggerCommand::Disable {
                    owner_scope: TriggerOwnerScope::session(SURFACE_SESSION),
                    actor: ProcessOriginator::session(SessionScope::new(SURFACE_SESSION)),
                    subscription_key: format!("surface-{key}"),
                    expected_revision: 1,
                };
                self.trigger_store
                    .execute_command(&format!("surface-disable-{key}"), command)
                    .await
                    .map_err(|error| error.to_string())?
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::TriggerOccurrence { key } => {
                self.trigger_store
                    .ingest_occurrence(
                        TriggerOccurrenceRequest::new(
                            "surface.event",
                            format!("source-{key}"),
                            serde_json::json!({"event": key}),
                            format!("surface-occurrence-{key}"),
                        )
                        .with_source(serde_json::json!({"source": key}))
                        .for_session(SURFACE_SESSION),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::TriggerOccurrenceNullSource { key } => {
                self.trigger_store
                    .ingest_occurrence(
                        TriggerOccurrenceRequest::new(
                            "surface.event",
                            format!("null-source-{key}"),
                            serde_json::json!({"event": key}),
                            format!("surface-null-source-occurrence-{key}"),
                        )
                        .with_source(serde_json::Value::Null)
                        .for_session(SURFACE_SESSION),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::ProcessSignalZero { negative } => {
                let payload = if *negative {
                    serde_json::json!({"value": -0.0})
                } else {
                    serde_json::json!({"value": 0.0})
                };
                self.process_registry
                    .append_event(
                        &ProcessId::from("prop-process-0"),
                        lash_core::ProcessEventAppendRequest::new("property.signal", payload)
                            .with_replay_key("surface-zero-replay"),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::EffectRecord { key, duration_ms } => {
                let replay_key = format!("surface-effect-{key}");
                let scope = ExecutionScope::turn(SURFACE_SESSION, SURFACE_TURN);
                let envelope = RuntimeEffectEnvelope::new(
                    lash_core::RuntimeEffectInvocation::new(
                        EffectAddress::new(scope.clone(), replay_key.clone())
                            .expect("surface effect carries an admitted effect scope"),
                        RuntimeAttribution::for_turn(SURFACE_SESSION, SURFACE_TURN, 1, 0),
                        replay_key.clone(),
                    ),
                    RuntimeEffectCommand::Sleep {
                        spec: lash_core::SleepSpec::For {
                            duration_ms: u64::from(*duration_ms),
                        },
                    },
                );
                let controller = self
                    .effect_host
                    .scoped(AdmittedScope::unpinned(scope).expect("a turn scope admits unpinned"))
                    .map_err(|error| error.to_string())?;
                let result = controller
                    .controller()
                    .execute_effect(
                        envelope,
                        RuntimeEffectLocalExecutor::testing(|_| async {
                            Ok(RuntimeEffectOutcome::Sleep)
                        }),
                    )
                    .await;
                result.map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::ToolIntentBatch => {
                let scope = AdmittedScope::turn(SURFACE_SESSION, SURFACE_TURN);
                let inner = self
                    .effect_host
                    .scoped_static(scope.clone())
                    .map_err(|error| error.to_string())?;
                let inner = inner.ok_or_else(|| {
                    format!("{} omitted a static differential controller", self.name)
                })?;
                let frames = Arc::new(Mutex::new(Vec::new()));
                let controller = lash_core::ScopedEffectController::shared(
                    Arc::new(LiteralFrameController {
                        inner,
                        frames: Arc::clone(&frames),
                    }),
                    scope,
                )
                .map_err(|error| error.to_string())?;
                let processes = lash_core::testing::effect_backed_process_service(
                    Arc::clone(&self.process_registry),
                    Arc::clone(&self.process_env_store),
                );
                let (completed, settled) =
                    Box::pin(lash_conformance::coordinate_tool_provider_with_services(
                        controller.clone(),
                        Arc::clone(&processes),
                        &SessionId::from(SURFACE_SESSION),
                        SurfaceIntentProvider::definition(),
                        Arc::new(SurfaceIntentProvider),
                        lash_core::PreparedToolCall::from_parts(
                            "surface-intent-call",
                            lash_core::ToolId::from("tool:surface_intent_provider"),
                            "surface_intent_provider",
                            serde_json::json!({}),
                            None,
                            serde_json::Value::Null,
                        ),
                    ))
                    .await
                    .map_err(|error| {
                        format!("{} provider/coordinator row failed: {error}", self.name)
                    })?;
                let intent_outcomes = completed.intent_outcomes.clone();
                let literal_intent_outcomes = vec![
                    lash_core::ToolIntentExecutionOutcome::Executed {
                        identity: lash_core::ToolIntentIdentity {
                            session_id: SessionId::from("surface-session"),
                            execution_scope_id: "surface-turn".to_string(),
                            tool_call_id: "surface-intent-call".to_string(),
                            intent_index: 0,
                            replay_key: "tool-intent:v2:blake3:32ea5ca081ab578194a6d210ecdf6e71c1ddcbae1b53001de32028ebeebe594f".to_string(),
                            minting_emission_replay_key: Some(
                                "tool-batch:surface-intent-call:surface-intent-call:attempt:1".to_string(),
                            ),
                        },
                        kind: lash_core::ToolIntentKind::StartProcess,
                        result: serde_json::json!({
                            "__handle__": "lash",
                            "id": "p.1.tool-intent:v2:blake3:32ea5ca081ab578194a6d210ecdf6e71c1ddcbae1b53001de32028ebeebe594f",
                            "process_id": "tool-intent:v2:blake3:32ea5ca081ab578194a6d210ecdf6e71c1ddcbae1b53001de32028ebeebe594f",
                            "incarnation": 1,
                            "kind": "external",
                            "status": "running"
                        }),
                    },
                    lash_core::ToolIntentExecutionOutcome::Executed {
                        identity: lash_core::ToolIntentIdentity {
                            session_id: SessionId::from("surface-session"),
                            execution_scope_id: "surface-turn".to_string(),
                            tool_call_id: "surface-intent-call".to_string(),
                            intent_index: 1,
                            replay_key: "tool-intent:v2:blake3:03cdeb1bb968e557d64e8f9718c2e7f34bf844071e614cd573224babd6c35397".to_string(),
                            minting_emission_replay_key: Some(
                                "tool-batch:surface-intent-call:surface-intent-call:attempt:1".to_string(),
                            ),
                        },
                        kind: lash_core::ToolIntentKind::StartProcess,
                        result: serde_json::json!({
                            "__handle__": "lash",
                            "id": "p.2.tool-intent:v2:blake3:03cdeb1bb968e557d64e8f9718c2e7f34bf844071e614cd573224babd6c35397",
                            "process_id": "tool-intent:v2:blake3:03cdeb1bb968e557d64e8f9718c2e7f34bf844071e614cd573224babd6c35397",
                            "incarnation": 2,
                            "kind": "external",
                            "status": "running"
                        }),
                    },
                ];
                if intent_outcomes != literal_intent_outcomes {
                    return Err(format!(
                        "{} intent row differed from its independent literal oracle: {intent_outcomes:?}",
                        self.name
                    ));
                }
                let observed = serde_json::to_value(&settled)
                    .expect("serialize observed non-empty intent settlement");
                // The settlement a group child journals for this terminal
                // (ADR 0099 §6): the dispatch outcome with its realized intent
                // outcomes moved into the settlement, which carries them, the
                // started-process possession and the resolved model return.
                // Each declaration's `event_types` is the product's default
                // process event vocabulary, not something the store derives:
                // the provider's `ProcessStartDeclaration::external` stamps it,
                // and the store must round-trip it verbatim. Take it from that
                // same constructor so a vocabulary change (FIG-3464 added
                // `process.effect_outcome` and `process.effect_omissions`)
                // cannot leave this oracle stale; every other field stays a
                // literal.
                let process_event_types = serde_json::to_value(
                    lash_core::ProcessStartDeclaration::external(
                        ProcessOriginator::session(SessionScope::new(SURFACE_SESSION)),
                        serde_json::Value::Null,
                        lash_core::ProcessLifecyclePolicy::new(
                            lash_core::ParentScope::Host,
                            lash_core::OnParentEnd::Cancel,
                        ),
                    )
                    .event_types,
                )
                .expect("serialize the default process event vocabulary");
                let literal_settlement = serde_json::json!({
                    "outcome": {
                        "attempts": [
                            {
                                "duration_ms": 0,
                                "ordinal": 1,
                                "outcome": "completed"
                            }
                        ],
                        "captures": [
                            {
                                "version": 5
                            }
                        ],
                        "intents": {
                            "intents": [
                                {
                                    "intent": {
                                        "declaration": {
                                            "disposition": "externally_owned",
                                            "event_types": process_event_types.clone(),
                                            "input": {
                                                "metadata": {
                                                    "index": 0,
                                                    "source": "literal-intent-row"
                                                },
                                                "type": "external"
                                            },
                                            "lifecycle": {
                                                "on_parent_end": "cancel",
                                                "parent": {
                                                    "kind": "owned",
                                                    "opener": {
                                                        "kind": "turn",
                                                        "session_id": "surface-session",
                                                        "turn_id": "surface-turn"
                                                    }
                                                }
                                            },
                                            "originator": {
                                                "session_id": "surface-session",
                                                "type": "session"
                                            }
                                        },
                                        "session_id": "surface-session"
                                    },
                                    "kind": "start_process"
                                },
                                {
                                    "intent": {
                                        "declaration": {
                                            "disposition": "externally_owned",
                                            "event_types": process_event_types.clone(),
                                            "input": {
                                                "metadata": {
                                                    "index": 1,
                                                    "source": "literal-intent-row"
                                                },
                                                "type": "external"
                                            },
                                            "lifecycle": {
                                                "on_parent_end": "cancel",
                                                "parent": {
                                                    "kind": "owned",
                                                    "opener": {
                                                        "kind": "turn",
                                                        "session_id": "surface-session",
                                                        "turn_id": "surface-turn"
                                                    }
                                                }
                                            },
                                            "originator": {
                                                "session_id": "surface-session",
                                                "type": "session"
                                            }
                                        },
                                        "session_id": "surface-session"
                                    },
                                    "kind": "start_process"
                                }
                            ],
                            "protocol_version": 3
                        },
                        "record": {
                            "args": {},
                            "call_id": "surface-intent-call",
                            "duration_ms": 0,
                            "output": {
                                "outcome": {
                                    "payload": {
                                        "$lash_tool_value": "untrusted_json",
                                        "value": {
                                            "ok": true
                                        }
                                    },
                                    "status": "success"
                                }
                            },
                            "tool": "surface_intent_provider"
                        }
                    },
                    "settlement": {
                        "intent_outcomes": [
                            {
                                "identity": {
                                    "execution_scope_id": "surface-turn",
                                    "intent_index": 0,
                                    "minting_emission_replay_key": "tool-batch:surface-intent-call:surface-intent-call:attempt:1",
                                    "replay_key": "tool-intent:v2:blake3:32ea5ca081ab578194a6d210ecdf6e71c1ddcbae1b53001de32028ebeebe594f",
                                    "session_id": "surface-session",
                                    "tool_call_id": "surface-intent-call"
                                },
                                "kind": "start_process",
                                "result": {
                                    "__handle__": "lash",
                                    "id": "p.1.tool-intent:v2:blake3:32ea5ca081ab578194a6d210ecdf6e71c1ddcbae1b53001de32028ebeebe594f",
                                    "incarnation": 1,
                                    "kind": "external",
                                    "process_id": "tool-intent:v2:blake3:32ea5ca081ab578194a6d210ecdf6e71c1ddcbae1b53001de32028ebeebe594f",
                                    "status": "running"
                                },
                                "status": "executed"
                            },
                            {
                                "identity": {
                                    "execution_scope_id": "surface-turn",
                                    "intent_index": 1,
                                    "minting_emission_replay_key": "tool-batch:surface-intent-call:surface-intent-call:attempt:1",
                                    "replay_key": "tool-intent:v2:blake3:03cdeb1bb968e557d64e8f9718c2e7f34bf844071e614cd573224babd6c35397",
                                    "session_id": "surface-session",
                                    "tool_call_id": "surface-intent-call"
                                },
                                "kind": "start_process",
                                "result": {
                                    "__handle__": "lash",
                                    "id": "p.2.tool-intent:v2:blake3:03cdeb1bb968e557d64e8f9718c2e7f34bf844071e614cd573224babd6c35397",
                                    "incarnation": 2,
                                    "kind": "external",
                                    "process_id": "tool-intent:v2:blake3:03cdeb1bb968e557d64e8f9718c2e7f34bf844071e614cd573224babd6c35397",
                                    "status": "running"
                                },
                                "status": "executed"
                            }
                        ],
                        "model_return": {
                            "call_id": "surface-intent-call",
                            "parts": [
                                {
                                    "text": "{\"ok\":true}",
                                    "type": "text"
                                },
                                {
                                    "text": "[tool intent start_process #0 executed: {\"__handle__\":\"lash\",\"id\":\"p.1.tool-intent:v2:blake3:32ea5ca081ab578194a6d210ecdf6e71c1ddcbae1b53001de32028ebeebe594f\",\"incarnation\":1,\"kind\":\"external\",\"process_id\":\"tool-intent:v2:blake3:32ea5ca081ab578194a6d210ecdf6e71c1ddcbae1b53001de32028ebeebe594f\",\"status\":\"running\"}]",
                                    "type": "text"
                                },
                                {
                                    "text": "[tool intent start_process #1 executed: {\"__handle__\":\"lash\",\"id\":\"p.2.tool-intent:v2:blake3:03cdeb1bb968e557d64e8f9718c2e7f34bf844071e614cd573224babd6c35397\",\"incarnation\":2,\"kind\":\"external\",\"process_id\":\"tool-intent:v2:blake3:03cdeb1bb968e557d64e8f9718c2e7f34bf844071e614cd573224babd6c35397\",\"status\":\"running\"}]",
                                    "type": "text"
                                }
                            ],
                            "tool_name": "surface_intent_provider"
                        },
                        "possession": [
                            "tool-intent:v2:blake3:32ea5ca081ab578194a6d210ecdf6e71c1ddcbae1b53001de32028ebeebe594f",
                            "tool-intent:v2:blake3:03cdeb1bb968e557d64e8f9718c2e7f34bf844071e614cd573224babd6c35397"
                        ],
                        "version": 6
                    },
                    "type": "tool_invocation"
                });
                if observed != literal_settlement {
                    return Err(format!(
                        "{} non-empty intent settlement differed from its literal per-tier oracle: {observed}",
                        self.name
                    ));
                }
                Ok(())
            }
            SurfaceOperation::AwaitResolve { key } => {
                let scope = ExecutionScope::turn(SURFACE_SESSION, SURFACE_TURN);
                let await_key = self
                    .effect_host
                    .await_event_key(
                        &scope,
                        AwaitEventWaitIdentity::tool_completion(format!("surface-call-{key}")),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                self.effect_host
                    .resolve_await_event(
                        &await_key,
                        Resolution::Ok(serde_json::json!({"resolved": key})),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::AwaitRevokeSession => self
                .effect_host
                .revoke_await_events_for_session(&SessionId::from(SURFACE_SESSION))
                .await
                .map_err(|error| error.to_string()),
            SurfaceOperation::RuntimeOperationRecord { key } => {
                let scope = ExecutionScope::runtime_operation(surface_operation_id(*key));
                let replay_key = format!("surface-op-effect-{key}");
                let envelope = RuntimeEffectEnvelope::new(
                    lash_core::RuntimeEffectInvocation::new(
                        EffectAddress::new(scope.clone(), replay_key.clone())
                            .expect("surface runtime operation carries an admitted effect scope"),
                        RuntimeAttribution::for_session(SURFACE_SESSION),
                        replay_key.clone(),
                    ),
                    RuntimeEffectCommand::Sleep {
                        spec: lash_core::SleepSpec::For { duration_ms: 1 },
                    },
                );
                self.effect_host
                    .scoped(
                        AdmittedScope::unpinned(scope.clone())
                            .expect("a runtime-operation scope admits unpinned"),
                    )
                    .map_err(|error| error.to_string())?
                    .controller()
                    .execute_effect(
                        envelope,
                        RuntimeEffectLocalExecutor::testing(|_| async {
                            Ok(RuntimeEffectOutcome::Sleep)
                        }),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                let await_key = self
                    .effect_host
                    .await_event_key(
                        &scope,
                        AwaitEventWaitIdentity::tool_completion(format!("surface-op-call-{key}")),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                self.effect_host
                    .resolve_await_event(
                        &await_key,
                        Resolution::Ok(serde_json::json!({"operation": key})),
                    )
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(())
            }
            SurfaceOperation::RetireRuntimeOperation { key } => self
                .effect_host
                .retire_effect_journal(EffectJournalRetirement::runtime_operation(
                    surface_operation_id(*key),
                ))
                .await
                .map(|_| ())
                .map_err(|error| error.to_string()),
            SurfaceOperation::EffectGroupOpen {
                group,
                children,
                cancel_losers,
            } => self.group_open(*group, *children, *cancel_losers).await,
            SurfaceOperation::EffectGroupRelease { group, position } => {
                self.group_release(*group, *position).await
            }
            SurfaceOperation::EffectGroupReleaseBoth { group, a, b } => {
                self.group_release_both(*group, *a, *b).await
            }
            SurfaceOperation::EffectGroupAwait { group } => self.group_await(*group).await,
            SurfaceOperation::EffectGroupClose { group } => self.group_close(*group).await,
            SurfaceOperation::EffectGroupCommit { group, position } => {
                self.group_commit(*group, *position).await
            }
            SurfaceOperation::EffectGroupCommitBoth { group, a, b } => {
                self.group_commit_both(*group, *a, *b).await
            }
            SurfaceOperation::EffectGroupDrainBlocked { group, rank } => {
                self.group_drain_blocked(*group, *rank).await
            }
            SurfaceOperation::EffectGroupCrash { group } => self.group_crash(*group).await,
            SurfaceOperation::EffectGroupDrain { group } => self.group_drain(*group).await,
            SurfaceOperation::TurnParkRecord { key } => self
                .runtime
                .record_turn_park(&surface_turn_park(*key))
                .await
                .map(|_park| ())
                .map_err(|error| error.to_string()),
            SurfaceOperation::TurnParkLoad => {
                let loaded = self
                    .runtime
                    .load_turn_park(&SessionId::from(SURFACE_RUNTIME_SESSION.to_string()))
                    .await
                    .map_err(|error| error.to_string())?;
                self.turn_park_loads
                    .push(serde_json::to_value(&loaded).map_err(|error| error.to_string())?);
                Ok(())
            }
            SurfaceOperation::TurnParkSettle { key } => {
                let session = SessionId::from(SURFACE_RUNTIME_SESSION.to_string());
                let state = lash_core::store::load_persisted_session_state(self.runtime.as_ref())
                    .await
                    .map_err(|error| error.to_string())?
                    .unwrap_or_else(|| RuntimeSessionState {
                        session_id: session.clone(),
                        ..RuntimeSessionState::new(lash_core::SessionPolicy::new(
                            lash_core::TurnBudget::Unbounded,
                        ))
                    });
                let commit = RuntimeCommit::persisted_state_with_operation_for_testing(
                    &state,
                    &[],
                    lash_core::store::OperationId::turn(
                        session,
                        surface_parked_turn_id(*key),
                        format!("surface-park-settle-{key}"),
                    ),
                );
                lash_core::testing::store_fixtures::commit_runtime_state_for_test(
                    &self.runtime,
                    commit,
                    "surface-park-settler",
                )
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
            }
        }
    }

    /// The shared refusal for every grouped op on a group that cannot take
    /// it: missing groups and crashed openers answer the same fixed strings
    /// on every backend, so minimized prefixes still agree.
    fn group_gate(&mut self, group: u8, op: &'static str) -> Result<(), String> {
        match self.book.get(&group) {
            None => {
                self.record_group(group, op, serde_json::json!({"error": "group_not_open"}));
                Err(format!("effect group {group} is not open"))
            }
            Some(book) if book.crashed => {
                self.record_group(group, op, serde_json::json!({"error": "opener_crashed"}));
                Err(format!("effect group {group} opener crashed"))
            }
            Some(_) => Ok(()),
        }
    }

    fn record_group(&mut self, group: u8, op: &'static str, mut outcome: serde_json::Value) {
        if self.groups.is_none() {
            return;
        }
        if let Some(fields) = outcome.as_object_mut() {
            fields.insert("op".to_string(), serde_json::json!(op));
            fields.insert("group".to_string(), serde_json::json!(group));
        }
        self.group_outcomes.push(outcome);
    }

    /// Send one command to the group's opener and record its reply. The
    /// memory runner has no opener, so it answers `Ok` — the gate above
    /// already filtered every refusal a backend would give.
    #[expect(
        clippy::expect_used,
        reason = "test support: the gate guarantees an opener's group is booked; a miss is a harness defect"
    )]
    async fn group_send(
        &mut self,
        group: u8,
        op: &'static str,
        command: GroupOpCommand,
    ) -> Result<(), String> {
        let Some(groups) = &mut self.groups else {
            return Ok(());
        };
        let Some(opener) = groups.openers.get_mut(&group) else {
            self.record_group(group, op, serde_json::json!({"error": "group_not_open"}));
            return Err(format!("effect group {group} is not open"));
        };
        let reply = opener.send(command).await;
        for (position, commit_seq) in reply.committed {
            self.book
                .get_mut(&group)
                .expect("a group with an opener is booked")
                .commit_seqs
                .insert(position, commit_seq);
        }
        self.record_group(group, op, reply.outcome);
        Ok(())
    }

    async fn group_open(
        &mut self,
        group: u8,
        children: u8,
        cancel_losers: bool,
    ) -> Result<(), String> {
        if self.book.get(&group).is_some_and(|book| book.crashed) {
            self.record_group(
                group,
                "effect_group_open",
                serde_json::json!({"error": "opener_crashed"}),
            );
            return Err(format!("effect group {group} opener crashed"));
        }
        if let Some(groups) = &mut self.groups
            && !groups.openers.contains_key(&group)
        {
            let opener = GroupOpener::spawn(
                groups.backend.clone(),
                Arc::clone(&groups.executors),
                group_key(group),
                group_scope_id(),
            )?;
            groups.openers.insert(group, opener);
        }
        self.book.entry(group).or_insert(GroupBook {
            crashed: false,
            cancel_losers,
            commit_seqs: BTreeMap::new(),
        });
        self.group_send(
            group,
            "effect_group_open",
            GroupOpCommand::Open {
                group: Box::new(surface_group(group, children, cancel_losers)),
            },
        )
        .await?;
        // The open returns before its children finish claiming; wait until
        // every child's executor is entered so no claim write is in flight
        // when the observation runs.
        if let Some(groups) = &self.groups {
            let executors = Arc::clone(&groups.executors);
            let key = group_key(group);
            for position in 0..children {
                if !wait_group_child_started(&executors, &key, position).await {
                    self.record_group(
                        group,
                        "effect_group_open",
                        serde_json::json!({"error": "children_not_started"}),
                    );
                    return Err(format!("effect group {group} children never started"));
                }
            }
        }
        Ok(())
    }

    /// The shared refusal for grouped ops that do not go through the opener:
    /// `Release` flips an executor flag and `Drain` runs on the successor, so
    /// both still apply to a crashed group — only a never-opened one refuses.
    fn group_exists_gate(&mut self, group: u8, op: &'static str) -> Result<(), String> {
        if self.book.contains_key(&group) {
            Ok(())
        } else {
            self.record_group(group, op, serde_json::json!({"error": "group_not_open"}));
            Err(format!("effect group {group} is not open"))
        }
    }

    async fn group_release(&mut self, group: u8, position: u8) -> Result<(), String> {
        self.group_exists_gate(group, "effect_group_release")?;
        if let Some(groups) = &self.groups {
            groups
                .executors
                .release(&group_key(group), usize::from(position));
        }
        self.record_group(
            group,
            "effect_group_release",
            serde_json::json!({"released": true, "position": position}),
        );
        // A live opener's released child finalizes asynchronously; wait for
        // its settlement rank so a prefix ending here observes a quiesced
        // row. A crashed opener's children settle in the drain instead.
        if !self.book[&group].crashed {
            self.wait_group_row_settled(&group_child_replay_key(group, position))
                .await;
        }
        Ok(())
    }

    async fn group_release_both(&mut self, group: u8, a: u8, b: u8) -> Result<(), String> {
        self.group_exists_gate(group, "effect_group_release_both")?;
        if let Some(groups) = &self.groups {
            groups.executors.release(&group_key(group), usize::from(a));
            groups.executors.release(&group_key(group), usize::from(b));
        }
        self.record_group(
            group,
            "effect_group_release_both",
            serde_json::json!({"released": [a, b]}),
        );
        // Both flags land before either settlement is awaited: the child
        // holding the higher commit rank drains only after its lower-ranked
        // sibling discharges, so a barrier on one released child alone could
        // wait on a settle the sibling's park makes unreachable.
        if !self.book[&group].crashed {
            for position in [a, b] {
                self.wait_group_row_settled(&group_child_replay_key(group, position))
                    .await;
            }
        }
        Ok(())
    }

    /// Poll the durable journal until the child's settlement rank is
    /// allocated, bounded by `GROUP_OP_BOUND`.
    async fn wait_group_row_settled(&self, replay_key: &str) {
        // A lane without groups opened none, so no row is owed.
        if self.groups.is_none() {
            return;
        }
        let deadline = std::time::Instant::now() + GROUP_OP_BOUND;
        loop {
            if self
                .reader
                .group_row_settled(&group_scope_id(), replay_key)
                .await
            {
                return;
            }
            if std::time::Instant::now() >= deadline {
                return;
            }
            tokio::time::sleep(GROUP_POLL).await;
        }
    }

    async fn group_await(&mut self, group: u8) -> Result<(), String> {
        self.group_gate(group, "effect_group_await")?;
        self.group_send(group, "effect_group_await", GroupOpCommand::Await)
            .await
    }

    async fn group_close(&mut self, group: u8) -> Result<(), String> {
        self.group_gate(group, "effect_group_close")?;
        let disposition = if self.book[&group].cancel_losers {
            LoserPolicy::Cancel
        } else {
            LoserPolicy::RunToCompletion
        };
        self.group_send(
            group,
            "effect_group_close",
            GroupOpCommand::Close { disposition },
        )
        .await?;
        self.wait_group_lifecycle_settled(group).await
    }

    /// ADR 0099 §7: `close` records `closing` and returns; the opener's
    /// finalizer then runs the four steps on a spawned task and CASes the
    /// lifecycle to `settled`. Observing the row the instant `close` returns
    /// would compare how far each backend's finalizer task had got, which is
    /// a scheduler fact, so the step waits for the durable end state.
    ///
    /// Every close in the generated catalog leaves obligations this host can
    /// discharge — a cancel-decided child's parked body is dropped with its
    /// token and a `RunToCompletion` close follows the release of every
    /// child — so `settled` is owed. A backend whose finalizer does not get
    /// there within the bound refuses the step, and that refusal diverges
    /// from the memory runner's `Ok`.
    async fn wait_group_lifecycle_settled(&mut self, group: u8) -> Result<(), String> {
        // A lane without groups opened none, so no finalization is owed.
        if self.groups.is_none() {
            return Ok(());
        }
        let deadline = std::time::Instant::now() + GROUP_OP_BOUND;
        loop {
            if self.reader.group_lifecycle_settled(&group_key(group)).await {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                self.record_group(
                    group,
                    "effect_group_close",
                    serde_json::json!({"error": "finalization_not_settled"}),
                );
                return Err(format!(
                    "effect group {group} recorded `closing` but its finalization never \
                     settled the lifecycle"
                ));
            }
            tokio::time::sleep(GROUP_POLL).await;
        }
    }

    async fn group_commit(&mut self, group: u8, position: u8) -> Result<(), String> {
        self.group_gate(group, "effect_group_commit")?;
        self.group_send(
            group,
            "effect_group_commit",
            GroupOpCommand::Commit { position },
        )
        .await
    }

    async fn group_commit_both(&mut self, group: u8, a: u8, b: u8) -> Result<(), String> {
        self.group_gate(group, "effect_group_commit_both")?;
        self.group_send(
            group,
            "effect_group_commit_both",
            GroupOpCommand::CommitBoth { a, b },
        )
        .await?;
        if self
            .group_outcomes
            .last()
            .and_then(|outcome| outcome.get("law_ok"))
            == Some(&serde_json::Value::Bool(false))
        {
            return Err(format!(
                "effect group {group} concurrent commits violated the distinct-consecutive law"
            ));
        }
        Ok(())
    }

    async fn group_drain_blocked(&mut self, group: u8, rank: u8) -> Result<(), String> {
        self.group_gate(group, "effect_group_drain_blocked")?;
        let mut seqs: Vec<u64> = self.book[&group].commit_seqs.values().copied().collect();
        seqs.sort_unstable();
        let Some(commit_seq) = seqs.get(usize::from(rank.saturating_sub(1))) else {
            self.record_group(
                group,
                "effect_group_drain_blocked",
                serde_json::json!({"error": "no_commit_at_rank", "rank": rank}),
            );
            return Ok(());
        };
        self.group_send(
            group,
            "effect_group_drain_blocked",
            GroupOpCommand::DrainBlocked {
                commit_seq: *commit_seq,
            },
        )
        .await
    }

    #[expect(
        clippy::expect_used,
        reason = "test support: the gate guarantees the book; a miss is a harness defect"
    )]
    async fn group_crash(&mut self, group: u8) -> Result<(), String> {
        self.group_gate(group, "effect_group_crash")?;
        if let Some(groups) = &mut self.groups
            && let Some(opener) = groups.openers.get_mut(&group)
        {
            opener.crash().await;
        }
        self.book
            .get_mut(&group)
            .expect("the gate guarantees the book")
            .crashed = true;
        self.record_group(
            group,
            "effect_group_crash",
            serde_json::json!({"crashed": true}),
        );
        Ok(())
    }

    /// The successor host's drain over the group, retried while children
    /// report `LeaseLive`/`Contested` so the pass rides out the crashed
    /// opener's lease boundary rather than racing it.
    async fn group_drain(&mut self, group: u8) -> Result<(), String> {
        self.group_exists_gate(group, "effect_group_drain")?;
        let Some(groups) = &self.groups else {
            return Ok(());
        };
        let drain = groups.successor.group_drain();
        let key = group_key(group);
        let deadline = std::time::Instant::now() + GROUP_OP_BOUND;
        let outcome = loop {
            match drain.drain_group(&key, &CancellationToken::new()).await {
                Ok(report) => {
                    let children: Vec<serde_json::Value> = report
                        .children
                        .iter()
                        .map(|child| {
                            serde_json::json!({
                                "replay_key": child.replay_key,
                                // Whether the last pass ran a child to its rank
                                // (`Settled`) or only seated a rank an earlier pass
                                // left held at the commit-order barrier (`Decided`)
                                // depends on which lease lapsed first, which is a
                                // scheduler fact. Both end with the child ranked,
                                // and the rank itself is compared in the group rows.
                                "outcome": match &child.outcome {
                                    ChildDrainOutcome::Settled
                                    | ChildDrainOutcome::Decided => "ranked",
                                    ChildDrainOutcome::Contested => "contested",
                                    ChildDrainOutcome::LeaseLive { .. } => "lease_live",
                                    ChildDrainOutcome::NoExecutor => "no_executor",
                                    ChildDrainOutcome::Interrupted => "interrupted",
                                    ChildDrainOutcome::Corrupt { .. } => "corrupt",
                                },
                            })
                        })
                        .collect();
                    let still_live = report.children.iter().any(|child| {
                        matches!(
                            child.outcome,
                            ChildDrainOutcome::Contested | ChildDrainOutcome::LeaseLive { .. }
                        )
                    });
                    if still_live && std::time::Instant::now() < deadline {
                        tokio::time::sleep(GROUP_POLL).await;
                        continue;
                    }
                    break serde_json::json!({
                        "disposition": format!("{:?}", report.disposition),
                        "children": children,
                    });
                }
                Err(error) => {
                    break serde_json::json!({"error": neutral_error_code(&error)});
                }
            }
        };
        self.record_group(group, "effect_group_drain", outcome);
        Ok(())
    }

    async fn observe(&self) -> SurfaceState {
        let mut state = self.reader.observe().await;
        state.group_outcomes = self.group_outcomes.clone();
        state.turn_park_loads = self.turn_park_loads.clone();
        state
    }
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn reset_postgres_surface(storage: &PostgresStorage) {
    let tables: Vec<String> = sqlx::query_scalar("SELECT tablename FROM pg_tables WHERE schemaname = 'public' AND tablename LIKE 'lash\\_%' AND tablename NOT IN ('lash_schema_versions', 'lash_await_event_meta') ORDER BY tablename").fetch_all(storage.pool()).await.unwrap();
    sqlx::query(&format!(
        "TRUNCATE {} RESTART IDENTITY CASCADE",
        tables.join(", ")
    ))
    .execute(storage.pool())
    .await
    .unwrap();
    sqlx::query("INSERT INTO lash_process_change_clock (singleton, current_seq, tombstone_compaction_horizon) VALUES (TRUE, 0, 0) ON CONFLICT (singleton) DO UPDATE SET current_seq = 0, tombstone_compaction_horizon = 0").execute(storage.pool()).await.unwrap();
    sqlx::query("INSERT INTO lash_turn_park_clock (singleton, current_seq, compaction_horizon) VALUES (TRUE, 0, 0) ON CONFLICT (singleton) DO UPDATE SET current_seq = 0, compaction_horizon = 0").execute(storage.pool()).await.unwrap();
}

/// A fresh SQLite memory backend's process-exec-env store (ADR 0102): the
/// environment store a runner with no durable one of its own publishes to.
#[expect(
    clippy::expect_used,
    reason = "test support: a memory backend that fails to open aborts the harness"
)]
async fn memory_backend_env_store() -> Arc<dyn lash_core::ProcessExecutionEnvStore> {
    lash_sqlite_store::SqliteBackend::memory()
        .await
        .expect("open a memory backend")
        .process_env_store()
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn surface_runners(
    root: &Path,
    storage: &PostgresStorage,
    database_url: &str,
    clock: Arc<dyn Clock>,
) -> Vec<SurfaceRunner> {
    // The memory lane's stores are one SQLite memory backend's. Its effect
    // host stays the in-process one until the Restate test engine replaces it
    // (FIG-3665); the effect surface is compared SQLite vs PostgreSQL only.
    let memory = lash_sqlite_store::SqliteBackend::memory_with_clock(Arc::clone(&clock))
        .await
        .unwrap();
    let memory_runtime: Arc<dyn RuntimePersistence> = Arc::new(memory.open_store().await.unwrap());
    let memory_registry = memory.process_registry();
    let memory_triggers = memory.trigger_store();
    let memory_effect: Arc<dyn EffectHost> =
        Arc::new(lash_core::facade_support::NativeEffectHost::default());

    let sqlite_runtime_path = root.join("runtime.db");
    let sqlite_process_path = root.join("process.db");
    let sqlite_trigger_path = root.join("trigger.db");
    let sqlite_effect_path = root.join("effect.db");
    // Grouped children journal into their own database: opener and successor
    // are distinct hosts (distinct lease identities) over the same journal.
    let sqlite_group_path = root.join("groups.db");
    let sqlite_runtime: Arc<dyn RuntimePersistence> =
        Arc::new(SqliteStore::open(&sqlite_runtime_path).await.unwrap());
    let sqlite_registry = Arc::new(
        SqliteProcessRegistry::open_with_clock(
            &sqlite_process_path,
            Arc::clone(&clock),
            root.join("sessions"),
        )
        .await
        .unwrap(),
    );
    let sqlite_triggers = Arc::new(
        SqliteTriggerStore::open_with_clock(&sqlite_trigger_path, Arc::clone(&clock))
            .await
            .unwrap(),
    );
    let sqlite_effect = Arc::new(
        SqliteEffectHost::open_with_clock(&sqlite_effect_path, Arc::clone(&clock))
            .await
            .unwrap(),
    );
    let sqlite_groups = {
        let successor = GroupHost::Sqlite(
            SqliteEffectHost::open_with_options_and_clock(
                &sqlite_group_path,
                sqlite_group_options(),
                Arc::new(SystemClock),
            )
            .await
            .unwrap(),
        );
        let executors = Arc::new(DifferentialGroupExecutors::default());
        successor
            .register_group_executors(Arc::clone(&executors) as Arc<dyn GroupExecutors>)
            .unwrap();
        GroupSurface {
            executors,
            backend: GroupBackend::Sqlite {
                path: sqlite_group_path.clone(),
            },
            successor,
            openers: BTreeMap::new(),
        }
    };

    let postgres_runtime: Arc<dyn RuntimePersistence> = Arc::new(
        storage
            .session_store("prop-runtime-session")
            .with_clock(Arc::clone(&clock)),
    );
    let postgres_registry = Arc::new(storage.process_registry().with_clock(Arc::clone(&clock)));
    let postgres_triggers = Arc::new(storage.trigger_store());
    let postgres_effect = Arc::new(storage.effect_host());
    let postgres_groups = {
        let successor = GroupHost::Postgres(PostgresEffectHost::with_options_and_clock(
            storage,
            postgres_group_options(),
            Arc::new(SystemClock),
        ));
        let executors = Arc::new(DifferentialGroupExecutors::default());
        successor
            .register_group_executors(Arc::clone(&executors) as Arc<dyn GroupExecutors>)
            .unwrap();
        GroupSurface {
            executors,
            backend: GroupBackend::Postgres {
                database_url: database_url.to_string(),
            },
            successor,
            openers: BTreeMap::new(),
        }
    };

    vec![
        SurfaceRunner {
            name: "sqlite-memory",
            scenario: StoreContractScenario::new(StoreContractHandles {
                registry: memory_registry.clone(),
                runtime: Arc::clone(&memory_runtime),
            }),
            process_registry: memory_registry,
            process_env_store: memory.process_env_store(),
            trigger_store: memory_triggers,
            effect_host: memory_effect,
            groups: None,
            book: BTreeMap::new(),
            group_outcomes: Vec::new(),
            runtime: memory_runtime,
            turn_park_loads: Vec::new(),
            reader: SurfaceReader::Sqlite {
                runtime_path: PathBuf::from(
                    memory.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
                ),
                process_path: PathBuf::from(
                    memory.database_uri(lash_sqlite_store::SqliteDatabase::ProcessRegistry),
                ),
                trigger_path: PathBuf::from(
                    memory.database_uri(lash_sqlite_store::SqliteDatabase::Triggers),
                ),
                effect_path: PathBuf::from(
                    memory.database_uri(lash_sqlite_store::SqliteDatabase::EffectReplay),
                ),
                group_path: PathBuf::from(
                    memory.database_uri(lash_sqlite_store::SqliteDatabase::EffectReplay),
                ),
            },
        },
        SurfaceRunner {
            name: "sqlite",
            scenario: StoreContractScenario::new(StoreContractHandles {
                registry: sqlite_registry.clone(),
                runtime: Arc::clone(&sqlite_runtime),
            }),
            process_registry: sqlite_registry,
            process_env_store: memory_backend_env_store().await,
            trigger_store: sqlite_triggers,
            effect_host: sqlite_effect,
            groups: Some(sqlite_groups),
            book: BTreeMap::new(),
            group_outcomes: Vec::new(),
            runtime: sqlite_runtime,
            turn_park_loads: Vec::new(),
            reader: SurfaceReader::Sqlite {
                runtime_path: sqlite_runtime_path,
                process_path: sqlite_process_path,
                trigger_path: sqlite_trigger_path,
                effect_path: sqlite_effect_path,
                group_path: sqlite_group_path,
            },
        },
        SurfaceRunner {
            name: "postgres",
            scenario: StoreContractScenario::new(StoreContractHandles {
                registry: postgres_registry.clone(),
                runtime: Arc::clone(&postgres_runtime),
            }),
            process_registry: postgres_registry,
            process_env_store: Arc::new(storage.process_env_store()),
            trigger_store: postgres_triggers,
            effect_host: postgres_effect,
            groups: Some(postgres_groups),
            book: BTreeMap::new(),
            group_outcomes: Vec::new(),
            runtime: postgres_runtime,
            turn_park_loads: Vec::new(),
            reader: SurfaceReader::Postgres {
                pool: storage.pool().clone(),
            },
        },
    ]
}

fn operation_results_agree(results: &[(&str, Option<String>)]) -> bool {
    results.windows(2).all(|pair| pair[0].1 == pair[1].1)
}

#[derive(Debug)]
struct SurfaceDivergence {
    step: usize,
    operation: SurfaceOperation,
    operation_results: Vec<(&'static str, Option<String>)>,
    observations: Vec<(&'static str, SurfaceState)>,
}

async fn apply_and_observe(
    runners: &mut [SurfaceRunner],
    operation: &SurfaceOperation,
) -> (
    Vec<(&'static str, Option<String>)>,
    Vec<(&'static str, SurfaceState)>,
) {
    let mut operation_results = Vec::with_capacity(runners.len());
    for runner in runners.iter_mut() {
        operation_results.push((runner.name, Box::pin(runner.apply(operation)).await.err()));
    }
    let mut observations = Vec::with_capacity(runners.len());
    for runner in runners {
        observations.push((runner.name, runner.observe().await));
    }
    (operation_results, observations)
}

fn counterexample_path(seed: u64) -> PathBuf {
    std::env::var_os("LASH_CROSS_BACKEND_COUNTEREXAMPLE_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CARGO_TARGET_DIR").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("target"))
        .join("cross-backend-counterexamples")
        .join(format!("seed-{seed}.json"))
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn persist_counterexample(
    seed: u64,
    operations: &[SurfaceOperation],
    divergence: &SurfaceDivergence,
) -> PathBuf {
    let path = counterexample_path(seed);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "seed": seed,
            "minimal_operations": operations,
            "first_diverging_step": divergence.step,
            "operation": divergence.operation,
            "operation_results": divergence.operation_results,
            "rows": divergence.observations,
        }))
        .unwrap(),
    )
    .unwrap();
    path
}

#[expect(
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
async fn first_divergence(
    storage: &PostgresStorage,
    database_url: &str,
    operations: &[SurfaceOperation],
) -> Option<SurfaceDivergence> {
    reset_postgres_surface(storage).await;
    let root = tempfile::tempdir().unwrap();
    let clock = Arc::new(DifferentialClock) as Arc<dyn Clock>;
    let mut runners = surface_runners(root.path(), storage, database_url, clock).await;
    for (step, operation) in operations.iter().enumerate() {
        let (operation_results, observations) =
            Box::pin(apply_and_observe(&mut runners, operation)).await;
        if !operation_results_agree(&operation_results) || !states_agree(&observations) {
            return Some(SurfaceDivergence {
                step: step + 1,
                operation: operation.clone(),
                operation_results,
                observations,
            });
        }
    }
    None
}

async fn minimize_diverging_prefix(
    storage: &PostgresStorage,
    database_url: &str,
    operations: &[SurfaceOperation],
) -> Vec<SurfaceOperation> {
    let mut minimal = operations.to_vec();
    let mut index = 0;
    while index + 1 < minimal.len() {
        let mut candidate = minimal.clone();
        candidate.remove(index);
        if Box::pin(first_divergence(storage, database_url, &candidate))
            .await
            .is_some()
        {
            minimal = candidate;
        } else {
            index += 1;
        }
    }
    minimal
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "compares three durable backends; requires Postgres (`just cross-backend-store-soak`, or LASH_POSTGRES_DATABASE_URL with --include-ignored)"]
async fn generated_cross_backend_surface_differential_agrees() {
    let database_url = match std::env::var("LASH_POSTGRES_DATABASE_URL") {
        Ok(value) if !value.is_empty() => value,
        _ if std::env::var("LASH_REQUIRE_POSTGRES").as_deref() == Ok("1") => {
            panic!("LASH_POSTGRES_DATABASE_URL must be set when LASH_REQUIRE_POSTGRES=1")
        }
        _ => {
            eprintln!(
                "SKIPPED generated cross-backend surface differential: PostgreSQL is not configured"
            );
            return;
        }
    };
    let mut database_lock = PgConnection::connect(&database_url).await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(SHARED_DATABASE_LOCK_KEY)
        .execute(&mut database_lock)
        .await
        .unwrap();
    let storage = PostgresStorage::connect(&database_url).await.unwrap();
    // CI seed 852 minimized to occurrence ingestion with no subscription state.
    if let Some(divergence) = Box::pin(first_divergence(
        &storage,
        &database_url,
        &[SurfaceOperation::TriggerOccurrence { key: 0 }],
    ))
    .await
    {
        panic!("seed-852 minimized trigger-occurrence regression diverged: {divergence:#?}");
    }
    // PR #570 seed 852 at 9eef49f32 minimized to one session-owned registration.
    if let Some(divergence) = Box::pin(first_divergence(
        &storage,
        &database_url,
        &[SurfaceOperation::TriggerRegister { key: 0 }],
    ))
    .await
    {
        panic!("seed-852 minimized trigger-register regression diverged: {divergence:#?}");
    }
    let canonical_conflict_material = [
        SurfaceOperation::TriggerOccurrenceNullSource { key: 0 },
        SurfaceOperation::TriggerOccurrenceNullSource { key: 0 },
        SurfaceOperation::StoreContract(StoreContractOp::Register {
            process: 0,
            disposition: 0,
            max_attempts: 1,
            wake_target: None,
        }),
        SurfaceOperation::ProcessSignalZero { negative: true },
        SurfaceOperation::ProcessSignalZero { negative: false },
    ];
    if let Some(divergence) = Box::pin(first_divergence(
        &storage,
        &database_url,
        &canonical_conflict_material,
    ))
    .await
    {
        panic!("canonical conflict-material differential diverged: {divergence:#?}");
    }
    let cases = std::env::var("LASH_CROSS_BACKEND_CASES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_CASES);
    let runner_seed = std::env::var("LASH_CROSS_BACKEND_SEED")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(DEFAULT_SEED);
    assert!(
        cases > 0,
        "LASH_CROSS_BACKEND_CASES must be greater than zero"
    );
    eprintln!(
        "cross-backend generated coverage is bounded: cases={cases} first_seed={runner_seed} \
         operations_per_case={OPS_PER_CASE}; omitted_seeds=all seeds outside the configured \
         contiguous range; effect_await_backends=sqlite,postgres (the memory runner's \
         in-process effect host has no durable journal)"
    );
    for case_index in 0..cases {
        reset_postgres_surface(&storage).await;
        let root = tempfile::tempdir().unwrap();
        let seed = runner_seed.wrapping_add(case_index as u64);
        let operations = generated_surface_operations(seed);
        let covered = operations
            .iter()
            .map(SurfaceOperation::kind)
            .collect::<BTreeSet<_>>();
        let omitted = ALL_SURFACE_OPERATION_KINDS
            .iter()
            .copied()
            .filter(|kind| !covered.contains(kind))
            .collect::<Vec<_>>();
        eprintln!(
            "cross-backend generated case seed={seed}: covered_operation_kinds={covered:?}; \
             omitted_operation_kinds={omitted:?}"
        );
        let clock = Arc::new(DifferentialClock) as Arc<dyn Clock>;
        let mut runners = surface_runners(root.path(), &storage, &database_url, clock).await;
        for (step, operation) in operations.iter().enumerate() {
            let (operation_results, observations) =
                Box::pin(apply_and_observe(&mut runners, operation)).await;
            if !operation_results_agree(&operation_results) || !states_agree(&observations) {
                let observed = SurfaceDivergence {
                    step: step + 1,
                    operation: operation.clone(),
                    operation_results,
                    observations,
                };
                // Minimizing replays prefixes on every backend and can outrun
                // the test timeout, so the divergence is on record before it
                // starts.
                eprintln!(
                    "cross-backend generated case seed={seed} diverged at step={} \
                     operation={:?}; minimizing the prefix",
                    observed.step, observed.operation,
                );
                let minimal = Box::pin(minimize_diverging_prefix(
                    &storage,
                    &database_url,
                    &operations[..=step],
                ))
                .await;
                // A prefix that stops reproducing is a harness defect, not a
                // clean run: say which divergence was observed and then lost,
                // so the report never hides behind a bare expect.
                let Some(minimal_divergence) =
                    Box::pin(first_divergence(&storage, &database_url, &minimal)).await
                else {
                    let path = persist_counterexample(seed, &operations[..=step], &observed);
                    panic!(
                        "cross-backend surface state diverged, but replaying the same prefix \
                         stopped diverging: the differential harness is not replay-deterministic. \
                         Observed divergence persisted to {}\nseed={seed} step={} \
                         operation={:?} operation_results={:#?} rows={:#?}",
                        path.display(),
                        observed.step,
                        observed.operation,
                        observed.operation_results,
                        observed.observations,
                    );
                };
                let divergence = format!(
                    "seed={seed} step={} operation={:?} operation_results={:#?} rows={:#?}",
                    minimal_divergence.step,
                    minimal_divergence.operation,
                    minimal_divergence.operation_results,
                    minimal_divergence.observations,
                );
                let path = persist_counterexample(seed, &minimal, &minimal_divergence);
                panic!(
                    "cross-backend surface state diverged; prefix-minimized reproduction persisted to {}\n{divergence}",
                    path.display()
                );
            }
        }
    }
}

#[derive(Clone, Debug)]
enum BlobOperation {
    Put(Vec<u8>),
    DeleteFirst,
    DeleteAbsent,
}

#[tokio::test]
#[ignore = "compares file and S3 blob stores; requires a live S3 server (`scripts/ci/with-service.sh s3`, or LASH_REQUIRE_S3=1 with --include-ignored)"]
async fn attachment_blob_store_differential_agrees() {
    if std::env::var("LASH_REQUIRE_S3").as_deref() != Ok("1") {
        eprintln!("SKIPPED attachment blob-store differential: LASH_REQUIRE_S3 is not set");
        return;
    }
    let memory_backend = lash_sqlite_store::SqliteBackend::memory().await.unwrap();
    let memory = memory_backend.attachment_store();
    let root = tempfile::tempdir().unwrap();
    let file = lash_core::facade_support::FileAttachmentStore::new(root.path());
    // The S3 server this runs against is named by the same LASH_S3_* settings
    // the lash-s3-store suite reads (`scripts/ci/s3-service.sh` owns them), so
    // no literal here pins the endpoint or the credentials to one deployment.
    let required = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| panic!("LASH_REQUIRE_S3=1 requires {name}"))
    };
    let s3 = S3AttachmentStore::from_config(S3AttachmentStoreConfig {
        endpoint_url: Some(required("LASH_S3_ENDPOINT")),
        region: std::env::var("LASH_S3_REGION").unwrap_or_else(|_| "us-east-1".to_string()),
        bucket: std::env::var("LASH_S3_BUCKET").unwrap_or_else(|_| "lash-attachments".to_string()),
        prefix: Some(format!("cross-backend/{}", run_nonce())),
        access_key_id: Some(required("LASH_S3_ACCESS_KEY")),
        secret_access_key: Some(required("LASH_S3_SECRET_KEY").into()),
        path_style: true,
    })
    .unwrap();
    let operations = [
        BlobOperation::Put(vec![1, 2, 3]),
        BlobOperation::Put(vec![9, 8]),
        BlobOperation::Put(vec![1, 2, 3]),
        BlobOperation::DeleteFirst,
        BlobOperation::DeleteAbsent,
    ];
    eprintln!(
        "attachment blob differential coverage is bounded: operations={operations:?}; \
         backends=sqlite-memory,file,s3; omitted_operations=all other byte sequences and operation \
         sequences"
    );
    let mut first_id = None;
    for operation in &operations {
        match operation {
            BlobOperation::Put(bytes) => {
                let meta = || {
                    AttachmentCreateMeta::new(
                        MediaType::parse("application/octet-stream").unwrap(),
                        None,
                        Some("surface".to_string()),
                    )
                };
                let memory_ref = memory.put(bytes.clone(), meta()).await.unwrap();
                let file_ref = file.put(bytes.clone(), meta()).await.unwrap();
                let s3_ref = s3.put(bytes.clone(), meta()).await.unwrap();
                assert_eq!(memory_ref.id, file_ref.id);
                assert_eq!(file_ref.id, s3_ref.id);
                first_id.get_or_insert(memory_ref.id);
            }
            BlobOperation::DeleteFirst => {
                let id = first_id.as_ref().unwrap();
                memory.delete(id).await.unwrap();
                file.delete(id).await.unwrap();
                s3.delete(id).await.unwrap();
            }
            BlobOperation::DeleteAbsent => {
                let id = lash_core::AttachmentId::parse("absent").expect("valid attachment id");
                memory.delete(&id).await.unwrap();
                file.delete(&id).await.unwrap();
                s3.delete(&id).await.unwrap();
            }
        }
        let memory_rows = raw_sqlite_blobs(
            &memory_backend.database_uri(lash_sqlite_store::SqliteDatabase::DurableCore),
        );
        let file_rows = raw_file_blobs(root.path());
        let s3_rows = s3.raw_blobs_for_testing().await.unwrap();
        assert_eq!(
            memory_rows, file_rows,
            "SQLite memory/file attachment blobs diverged after {operation:?}"
        );
        assert_eq!(
            file_rows, s3_rows,
            "file/S3 attachment blobs diverged after {operation:?}"
        );
    }
}

#[expect(
    clippy::expect_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn raw_sqlite_blobs(database_uri: &str) -> Vec<(lash_core::AttachmentId, Vec<u8>)> {
    let connection = rusqlite::Connection::open(database_uri).expect("open the blob reader");
    let mut statement = connection
        .prepare("SELECT attachment_id, content FROM attachment_blobs ORDER BY attachment_id")
        .expect("prepare the blob read");
    statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .expect("read the attachment blobs")
        .map(|row| {
            let (id, bytes) = row.expect("decode an attachment blob row");
            (
                lash_core::AttachmentId::parse(id).expect("valid attachment id"),
                bytes,
            )
        })
        .collect()
}

#[expect(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test support: the surrounding harness code establishes this value; a refusal panics the harness with its case name by design"
)]
fn raw_file_blobs(root: &Path) -> Vec<(lash_core::AttachmentId, Vec<u8>)> {
    let mut rows = Vec::new();
    let content_root = root.join("blake3");
    if !content_root.exists() {
        return rows;
    }
    for prefix in fs::read_dir(content_root).unwrap() {
        for entry in fs::read_dir(prefix.unwrap().path()).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().to_string();
            if !name.contains(".staging.") {
                rows.push((
                    lash_core::AttachmentId::parse(name).expect("valid attachment id"),
                    fs::read(entry.path()).unwrap(),
                ));
            }
        }
    }
    rows.sort_by(|left, right| left.0.cmp(&right.0));
    rows
}
