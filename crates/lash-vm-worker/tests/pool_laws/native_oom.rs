//! A real worker's native allocation failure, recovered on the durable store.
//!
//! The fixture worker refuses itself every new mapping and fails one 16 MiB
//! allocation, once, at a chosen point of a cell's run: mid-compute, before
//! the cell dispatched anything, or as the recorded outcome of its one tool
//! call is delivered. The supervisor reports the crash apart from a guest
//! error, reaps the child before replacing it, and leaves the parent running.
//! The cell's quiet points are committed to `lash_exec_snapshots` and
//! `lash_run_records` on SQLite in memory, so the activation that runs it
//! again resumes from the last one: compute before any operation runs again
//! from the start, and the outcome lost in delivery is fed back by identity,
//! never dispatched a second time.

use super::*;
use lash_core_execution::{
    ActorContext, AdmittedScope, Backend, CancellationToken, EffectOpener, ToolCallId,
};
use lash_durable::domain::{CellId, ExecKey};
use lash_durable::{ActorKey, CommitLabel, FormatSet, MailTx, NoProbe, NodeId, NodeSpec};
use lash_vm_broker::*;
use std::sync::{Arc, Mutex};

const FORMATS: &str = "native-oom-law/1";
const SESSION: &str = "native-oom";
const TURN: &str = "turn";
const CELL: &str = "native-oom-cell";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Phase {
    ComputeBeforeEffectDispatch,
    RecordedEffectBeforeDelivery,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::ComputeBeforeEffectDispatch => "compute_before_effect_dispatch",
            Self::RecordedEffectBeforeDelivery => "recorded_effect_before_delivery",
        }
    }

    fn mode(self) -> &'static str {
        match self {
            Self::ComputeBeforeEffectDispatch => "native_oom_compute",
            Self::RecordedEffectBeforeDelivery => "native_oom_recorded",
        }
    }

    fn source(self) -> &'static str {
        match self {
            Self::ComputeBeforeEffectDispatch => {
                "let n = 0; while (n < 200000) { n++; } finish(n);"
            }
            Self::RecordedEffectBeforeDelivery => "finish(await tools.echo({ value: 7 }));",
        }
    }
}

/// The parent: `tools.echo` is a `Once` tool whose body answers a fixed
/// value and counts itself.
struct Parent {
    context: AdmittedContext,
    dispatches: Mutex<Vec<ToolCallId>>,
    checkpoints: Mutex<Vec<u64>>,
}

#[async_trait::async_trait]
impl ParentEffects for Parent {
    fn admission(&self, operation: &AdmittedOperation) -> Result<Admission, ParentFault> {
        Ok(Admission {
            draft: matches!(operation.kind, AdmittedKind::Invoke(_)).then(|| {
                lash_vm_broker::testing::operation_draft(
                    &self.context,
                    operation,
                    "echo",
                    lash_sansio::ExecutionPolicy::Once,
                    0,
                )
            }),
            waits: Vec::new(),
        })
    }

    async fn perform(
        &self,
        operation: &AdmittedOperation,
        _waits: &[(WaitRef, Option<PinnedKey>)],
    ) -> Result<Performed, ParentFault> {
        let value = match &operation.kind {
            AdmittedKind::Invoke(call) => {
                self.dispatches
                    .lock()
                    .expect("dispatches")
                    .push(call.call_id.clone());
                lashlang::from_json(serde_json::json!({"value": 7, "dispatch": 1}))
            }
            AdmittedKind::Control { payload, .. } => {
                let OperationRequest::Finish(value) =
                    OperationRequest::decode(payload).expect("a control request")
                else {
                    panic!("the only control effect is finish");
                };
                value
            }
            other => panic!("unexpected operation {other:?}"),
        };
        Ok(Performed::outcome(EffectOutcome::Value(EncodedPayload(
            rmp_serde::to_vec_named(&AbilityOutcome::Value(value)).expect("worker result"),
        ))))
    }

    fn interrupted(&self, operation: &AdmittedOperation) -> EffectOutcome {
        panic!("no operation is interrupted: {operation:?}")
    }

    async fn observe_cancellation(&self, checkpoint: u64) -> Result<bool, ParentFault> {
        assert!(checkpoint > 0, "guest instructions have executed");
        self.checkpoints
            .lock()
            .expect("checkpoints")
            .push(checkpoint);
        Ok(false)
    }
}

/// The cell's session actor, claimed on a SQLite memory store set.
async fn claimed_session() -> ActorContext {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    let backend = Backend::for_testing(stores);
    let actor = ActorKey::session(SESSION).expect("a valid session actor");
    let mut created = MailTx::new();
    created.create_actor(actor.clone(), FormatSet::new(FORMATS));
    backend
        .durable()
        .commit_mail(created, CommitLabel::new("law.create"))
        .await
        .expect("create the cell's session actor");
    let lease = backend
        .durable()
        .register_node(&NodeSpec {
            node: NodeId::new("native-oom-law"),
            decodes: vec![FormatSet::new(FORMATS)],
            ttl_millis: 600_000,
        })
        .await
        .expect("register the law's node");
    let epoch = backend
        .durable()
        .claim(&lease, 1)
        .await
        .expect("claim the cell's session actor")
        .into_iter()
        .find(|claimed| claimed.actor == actor)
        .expect("the session actor is claimable")
        .epoch;
    ActorContext::new(
        backend,
        actor,
        epoch,
        AdmittedScope::turn(SESSION, TURN),
        CancellationToken::new(),
        Arc::new(NoProbe),
    )
}

fn exec() -> ExecKey {
    ExecKey::Cell(
        lash_sansio::SessionId::from(SESSION),
        lash_sansio::TurnId::from(TURN),
        CellId::new(CELL),
    )
}

#[expect(
    clippy::disallowed_methods,
    reason = "the law reads the child's retained native allocation-failure evidence"
)]
async fn law(phase: Phase) {
    let directory = tempfile::tempdir().expect("fault evidence directory");
    let witness = directory.path().join("allocation.json");
    let mut cfg = config(phase.mode());
    cfg.min_workers = 0;
    cfg.entry
        .args
        .push(witness.to_str().expect("UTF-8 fixture path").into());
    let pool = WorkerPool::new(cfg).expect("pool");
    let first = checkout(&pool);
    let dead = first.pid().expect("initial pid");
    first.release().expect("initial reset");

    let cx = claimed_session().await;
    let store = DurableSnapshotStore::new(&cx, exec());
    let context = AdmittedContext {
        owner: VmOwner::new("native-oom"),
        owner_epoch: OwnerEpoch(1),
        identities: CodeCallIdentities::cell(EffectOpener::turn(SESSION, TURN), CELL),
        bindings: Arc::new(FrozenBindings::new().bind(
            "tools",
            "echo",
            BoundOperation {
                tool: ToolRoute {
                    tool_id: "echo".into(),
                    tool_name: "echo".into(),
                },
                arguments: ArgumentContract::Any,
            },
        )),
    };
    let parent = Parent {
        context: context.clone(),
        dispatches: Mutex::default(),
        checkpoints: Mutex::default(),
    };
    let budget = ExecutionBudget::default();
    let slots = PoolSlots {
        pool: pool.clone(),
        owner_epoch: OwnerEpoch(1),
        frame_epoch: FrameEpoch(1),
        budget: budget.clone(),
        service: None,
    };
    store
        .open_frame(FrameEpoch(1))
        .await
        .expect("open the cell's frame");
    let activation = async || {
        let from = store
            .latest()
            .await
            .expect("read the cell's latest quiet point")
            .map(|(_, checkpoint)| checkpoint);
        let input = start(phase.source(), ExecutionMode::Foreground);
        Broker {
            context: &context,
            effects: &parent,
            checkpoints: &store,
            slots: &slots,
            codec: FrameCodec::new(pool.config().protocol.decode),
            contract: lashlang::vm_contract_reads(),
            bounds: BrokerBounds::standard(),
            frames: FrameFence::new(FrameEpoch(1)),
        }
        .run(
            RunStart {
                program: input.program,
                contexts: input.contexts,
                limits: input.limits,
                from,
                fresh: StartState::Fresh,
            },
            &CancellationToken::new(),
        )
        .await
    };

    let crashed = tokio::time::timeout(Duration::from_secs(30), activation())
        .await
        .expect("the crashing activation is bounded");
    let Err(failure) = crashed else {
        panic!("native allocation failure must crash the worker before completion");
    };
    assert!(
        matches!(
            failure,
            BrokerFailure::WorkerLost {
                outcome: InfrastructureOutcome::WorkerCrashed { .. },
            }
        ),
        "a supervisor crash, separate from a guest error: {failure:?}"
    );
    assert!(failure.is_retryable(), "{failure:?}");
    let evidence: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&witness).expect("allocation witness"))
            .expect("allocation witness JSON");
    assert_eq!(evidence["phase"], phase.name());
    assert_eq!(evidence["allocation_failed"], true);
    assert_eq!(evidence["address_space_ceiling_bytes"], 0);
    assert_eq!(evidence["requested_bytes"], 16 * 1024 * 1024);
    assert_eq!(evidence["errno"], libc::ENOMEM);
    assert_eq!(evidence["pid"], dead);
    assert_reaped(dead);
    let stats = pool.stats();
    assert_eq!(
        (
            stats.workers,
            stats.idle,
            stats.queued_items,
            stats.queued_bytes
        ),
        (0, 0, 0, 0),
        "the dead worker is gone and nothing waits"
    );
    let dispatched = parent.dispatches.lock().expect("dispatches").clone();
    match phase {
        Phase::ComputeBeforeEffectDispatch => {
            assert!(dispatched.is_empty(), "{dispatched:?}");
            assert!(
                !parent.checkpoints.lock().expect("checkpoints").is_empty(),
                "the compute crossed an instruction checkpoint"
            );
        }
        Phase::RecordedEffectBeforeDelivery => assert_eq!(dispatched.len(), 1),
    }

    let resumed = tokio::time::timeout(Duration::from_secs(30), activation())
        .await
        .expect("the resuming activation is bounded")
        .expect("the cell resumes from its committed quiet point");
    let BrokeredEnd::Complete { value, .. } = resumed else {
        panic!("the resumed cell completes: {resumed:?}");
    };
    let outcome: lashlang::ExecutionOutcome = rmp_serde::from_slice(&value.0).expect("completion");
    let expected = match phase {
        Phase::ComputeBeforeEffectDispatch => serde_json::json!(200000),
        Phase::RecordedEffectBeforeDelivery => serde_json::json!({"value": 7, "dispatch": 1}),
    };
    assert_eq!(
        outcome,
        lashlang::ExecutionOutcome::Finished(lashlang::from_json(expected))
    );
    assert_eq!(
        *parent.dispatches.lock().expect("dispatches"),
        dispatched,
        "the outcome lost in delivery is fed back by identity, never dispatched again"
    );
    let stats = pool.stats();
    assert_eq!(
        (
            stats.workers,
            stats.idle,
            stats.queued_items,
            stats.queued_bytes
        ),
        (1, 1, 0, 0),
        "one replacement worker serves the resumed cell and goes idle"
    );
    assert!(!stats.restart_storm);
    assert_eq!(
        budget.totals().0,
        2,
        "the replacement keeps the crashed attempt's count"
    );
}

macro_rules! native_oom_laws {
    ($($name:ident => $phase:ident),* $(,)?) => {$(
        #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
        async fn $name() { law(Phase::$phase).await; }
    )*};
}

native_oom_laws! {
    compute_before_effect_dispatch => ComputeBeforeEffectDispatch,
    recorded_effect_before_delivery => RecordedEffectBeforeDelivery,
}
