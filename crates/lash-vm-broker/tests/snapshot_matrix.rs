//! VM coherence on the durable store (ADR 0132 §8): a code cell's run on
//! the fake worker, its quiet points committed to `lash_exec_snapshots` and
//! `lash_run_records` on SQLite in memory, cut at every labelled commit under
//! every fault and resumed on another node.
//!
//! Laws, at every cut:
//!
//! - **V1, snapshot before body:** every operation body runs only once the
//!   transaction that committed the VM's snapshot on it, with its
//!   admission, stands.
//! - **V2, no completed operation re-runs:** a `Once` body runs at most
//!   once; a `Repeatable` body runs again only at its same identity.
//! - **V3, by identity:** the cell's result for each operation is the value
//!   one of that operation's bodies answered, or its interruption; the
//!   parent answers from the member's committed outcome, never running a
//!   settled body again.
//! - **NR, no program re-entry:** the VM's program is entered only by an
//!   activation that found no snapshot; every other resume continues one.
//! - **V4, ledger and VM agree:** the cell's last snapshot records its end,
//!   with a ledger that issued exactly its two operations and stands on
//!   none.

#![allow(clippy::expect_used)]

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core_execution::runtime::actor::round::lifecycle::{MemberBodies, MemberBody};
use lash_core_execution::runtime::actor::round::{
    AdmittedExecution, Material, PolicyView, SettledOutput,
};
use lash_core_execution::runtime::actor::waits::Resolution;
use lash_core_execution::{ActorContext, AdmittedScope, Backend};
use lash_core_store::tool_run::{CompletionSource, MaterialOwner, MaterialRole};
use lash_durable::domain::{CellId, ExecKey};
use lash_durable::runner::{Activation, Exit, Owned};
use lash_durable::{
    ActorKey, ActorState, CommitLabel, DurableError, DurableStore, FormatSet, MailTx, Release,
};
use lash_durable_test::{Matrix, Scenario, SimClock, SimNodes, SimNodesConfig};
use lash_sansio::ToolId;
use lash_sansio::{SessionId, TurnId};
use lash_vm_broker::authority::{decode_value, encode_value};
use lash_vm_broker::testing::{FAKE_VM_CONTRACT, FakeWorkerPool, ScriptedProgram, Step};
use lash_vm_broker::{
    Admission, AdmittedContext, AdmittedKind, AdmittedOperation, ArgumentContract, BoundOperation,
    Broker, BrokerBounds, BrokeredEnd, Checkpoint, CodeCallIdentities, Decide, Driven,
    DurableSnapshotStore, FrameFence, FrozenBindings, Invocation, OperationId, ParentEffects,
    ParentFault, Performed, PinnedKey, RunStart, SnapshotStore, ToolRoute, WaitRef,
};
use lash_vm_protocol::{
    DecodeLimits, EffectOutcome, FrameCodec, FrameEpoch, OwnerEpoch, StartState, VmLimits, VmOwner,
};
use tokio_util::sync::CancellationToken;

const FORMATS: &str = "vm-law-v1";
const SESSION: &str = "vm-law";
const CREATE: CommitLabel = CommitLabel::new("law.create");
const DONE: CommitLabel = CommitLabel::new("law.done");

fn actor() -> ActorKey {
    ActorKey::session(SESSION).expect("a valid session actor")
}

fn exec() -> ExecKey {
    ExecKey::Cell(
        SessionId::try_from(SESSION.to_owned()).expect("a valid session id"),
        TurnId::try_from("turn-1".to_owned()).expect("a valid turn id"),
        CellId::new("cell-1"),
    )
}

fn invoke(operation: &str) -> Step {
    Step::Invoke(Invocation {
        binding: "tools".into(),
        operation: operation.into(),
        arguments: serde_json::json!({}),
    })
}

/// The cell: a `Once` operation, pure work, a `Repeatable` operation, pure
/// work.
fn program() -> ScriptedProgram {
    ScriptedProgram::new(vec![
        invoke("write"),
        Step::Compute,
        invoke("read"),
        Step::Compute,
    ])
}

fn context() -> AdmittedContext {
    let route = |name: &str| BoundOperation {
        tool: ToolRoute {
            tool_id: format!("tool:{name}"),
            tool_name: name.into(),
        },
        arguments: ArgumentContract::Any,
    };
    AdmittedContext {
        owner: VmOwner::new("cell:vm-law"),
        owner_epoch: OwnerEpoch(0),
        identities: CodeCallIdentities::cell(
            lash_core_store::effect_opener::EffectOpener::turn(SESSION, "turn-1"),
            "cell-1",
        ),
        bindings: Arc::new(
            FrozenBindings::new()
                .bind("tools", "write", route("write"))
                .bind("tools", "read", route("read")),
        ),
    }
}

/// One body run: the operation, its tool, and whether its admission was
/// durable when it ran.
#[derive(Clone, Debug)]
struct Body {
    operation: Option<OperationId>,
    tool: String,
    token: u64,
    admitted_first: bool,
}

/// What every node's activations share.
#[derive(Default)]
struct Shared {
    backend: Mutex<Option<Backend>>,
    bodies: Mutex<Vec<Body>>,
    tokens: AtomicUsize,
    /// Activations that found no snapshot, so entered the program.
    fresh: AtomicUsize,
    ends: Mutex<Vec<Vec<serde_json::Value>>>,
    /// Why activations stopped short, for a stalled run's report.
    stops: Mutex<Vec<String>>,
    pool: Mutex<Option<Arc<FakeWorkerPool>>>,
}

impl Shared {
    fn backend(&self) -> Backend {
        self.backend
            .lock()
            .expect("backend")
            .clone()
            .expect("the database opened first")
    }

    fn pool(&self) -> Arc<FakeWorkerPool> {
        Arc::clone(self.pool.lock().expect("pool").as_ref().expect("a pool"))
    }
}

struct CellScenario {
    shared: Arc<Shared>,
}

#[async_trait::async_trait]
impl Scenario for CellScenario {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
            .await
            .expect("an in-memory store set opens");
        let durable: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
        *self.shared.backend.lock().expect("backend") =
            Some(Backend::for_testing(Arc::new(stores)));
        *self.shared.pool.lock().expect("pool") = Some(Arc::new(FakeWorkerPool::new(
            FrameCodec::new(DecodeLimits::standard()),
            4,
            Duration::from_secs(5),
        )));
        durable
    }

    fn config(&self) -> SimNodesConfig {
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: vec![FormatSet::new(FORMATS)],
            max_active: 4,
        }
    }

    fn activation(&self) -> Arc<dyn Activation> {
        Arc::new(CellActivation {
            shared: Arc::clone(&self.shared),
        })
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
        // Seeded past the faults: the laws are the cell's, and a node's poll
        // finds the actor.
        let mut create = MailTx::new();
        create.create_actor(actor(), FormatSet::new(FORMATS));
        nodes
            .database()
            .commit_mail(create, CREATE)
            .await
            .map_err(|error| error.to_string())?;
        nodes.start("a");
        nodes.start("b");
        Ok(())
    }

    fn actors(&self) -> Vec<ActorKey> {
        vec![actor()]
    }

    async fn done(&self, nodes: &SimNodes) -> bool {
        matches!(
            nodes.database().actor(&actor()).await,
            Ok(Some(snapshot)) if snapshot.state == ActorState::Terminal
        )
    }

    async fn check(&self, nodes: &SimNodes, _cut: Option<&lash_durable_test::Cut>) -> Vec<String> {
        let mut violations = Vec::new();
        let bodies = self.shared.bodies.lock().expect("bodies").clone();
        for body in &bodies {
            if !body.admitted_first {
                violations.push(format!(
                    "V1: {} ran before the snapshot admitting {:?} committed",
                    body.tool, body.operation
                ));
            }
        }
        let mut runs: BTreeMap<(String, Option<OperationId>), usize> = BTreeMap::new();
        for body in &bodies {
            *runs.entry((body.tool.clone(), body.operation)).or_default() += 1;
        }
        for ((tool, operation), count) in &runs {
            if tool == "write" && *count > 1 {
                violations.push(format!(
                    "V2: the Once body of {operation:?} ran {count} times"
                ));
            }
        }
        let ends = self.shared.ends.lock().expect("ends").clone();
        let Some(end) = ends.last() else {
            violations.push(format!(
                "the cell never ended: {:?}",
                self.shared.stops.lock().expect("stops")
            ));
            return violations;
        };
        if ends.iter().any(|other| other != end) {
            violations.push(format!("V3: the cell ended differently: {ends:?}"));
        }
        for (index, tool) in ["write", "read"].into_iter().enumerate() {
            let Some(result) = end.get(index) else {
                violations.push(format!("V3: no result for {tool}: {end:?}"));
                continue;
            };
            let answered = result.get("token").and_then(serde_json::Value::as_u64);
            let interrupted = result.get("failed").is_some();
            let produced = bodies
                .iter()
                .any(|body| body.tool == tool && Some(body.token) == answered);
            if !(produced || interrupted && tool == "write") {
                violations.push(format!(
                    "V3: {tool}'s result {result} is neither a body's answer nor its interruption"
                ));
            }
        }
        let entries = self.shared.pool().stats().entries;
        let fresh = self.shared.fresh.load(Ordering::SeqCst);
        if entries != fresh {
            violations.push(format!(
                "NR: the program was entered {entries} times, by {fresh} activations without a snapshot"
            ));
        }
        match nodes.database().snapshot(&exec()).await {
            Ok(Some(row)) => match serde_json::from_str::<Checkpoint>(&row.snapshot_ref) {
                Ok(checkpoint) => {
                    if checkpoint.end.is_none()
                        || checkpoint.ledger.pending.is_some()
                        || checkpoint.ledger.next_admission != 2
                    {
                        violations.push(format!(
                            "V4: the last snapshot's ledger disagrees with its ended VM: {:?}",
                            checkpoint.ledger
                        ));
                    }
                }
                Err(error) => violations.push(format!("V4: the snapshot does not decode: {error}")),
            },
            other => violations.push(format!("V4: no last snapshot: {other:?}")),
        }
        violations
    }
}

struct CellActivation {
    shared: Arc<Shared>,
}

impl CellActivation {
    fn stop(&self, why: &dyn std::fmt::Display) {
        self.shared
            .stops
            .lock()
            .expect("stops")
            .push(why.to_string());
    }
}

/// Whether `owned` is someone else's now.
async fn lost(owned: &Owned) -> bool {
    matches!(owned.begin().await, Err(DurableError::OwnershipLost(_)))
}

#[async_trait::async_trait]
impl Activation for CellActivation {
    async fn activate(&self, owned: Owned) -> Exit {
        let cx = ActorContext::claimed(
            self.shared.backend(),
            &owned,
            AdmittedScope::runtime_operation("vm-law"),
            CancellationToken::new(),
            Arc::new(lash_durable::NoProbe),
        );
        let store = Arc::new(DurableSnapshotStore::new(&cx, exec()));
        store
            .bind_members(
                Arc::new(LawBodies {
                    shared: Arc::clone(&self.shared),
                    database: Arc::clone(owned.store()),
                }),
                PolicyView::new(
                    ["write", "read"]
                        .map(|tool| (ToolId::new(format!("tool:{tool}")), policy(tool))),
                ),
            )
            .await;
        let context = context();
        let host = LawHost {
            context: context.clone(),
            store: Arc::clone(&store),
        };
        let pool = self.shared.pool();
        for _ in 0..8 {
            let latest = match store.latest().await {
                Ok(latest) => latest.map(|(_, checkpoint)| checkpoint),
                Err(error) => {
                    self.stop(&error);
                    if lost(&owned).await {
                        return Exit::Released;
                    }
                    continue;
                }
            };
            if latest.is_none() {
                self.shared.fresh.fetch_add(1, Ordering::SeqCst);
            }
            let broker = Broker {
                context: &context,
                effects: &host,
                checkpoints: &*store,
                slots: &*pool,
                codec: FrameCodec::new(DecodeLimits::standard()),
                contract: FAKE_VM_CONTRACT.exact_reads(),
                bounds: BrokerBounds::standard(),
                frames: FrameFence::new(FrameEpoch(0)),
            };
            let start = RunStart {
                program: program().source(),
                contexts: Vec::new(),
                limits: VmLimits {
                    instruction_budget: None,
                    memory_limit_bytes: None,
                    max_frame_depth: 64,
                },
                from: latest,
                fresh: StartState::Fresh,
            };
            match broker.run(start, &CancellationToken::new()).await {
                Ok(BrokeredEnd::Complete { value, .. }) => {
                    let results = decode_value(&value)
                        .ok()
                        .and_then(|value| value.as_array().cloned())
                        .unwrap_or_default();
                    self.shared.ends.lock().expect("ends").push(results);
                    let Ok(mut tx) = owned.begin().await else {
                        return Exit::Released;
                    };
                    tx.give_up(Release::Terminal);
                    match owned.commit(tx, DONE).await {
                        Ok(_) => return Exit::Released,
                        Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                        Err(_) => continue,
                    }
                }
                Ok(other) => panic!("the cell ends complete: {other:?}"),
                Err(failure) => {
                    self.stop(&failure);
                    if lost(&owned).await {
                        return Exit::Released;
                    }
                }
            }
        }
        Exit::Released
    }
}

/// Cuts, across the matrix's cells, that left a started `Once` for the
/// next owner to interrupt.
static INTERRUPTED: AtomicUsize = AtomicUsize::new(0);

fn policy(tool: &str) -> lash_sansio::ExecutionPolicy {
    if tool == "read" {
        lash_sansio::ExecutionPolicy::repeatable(
            std::num::NonZeroU32::new(3).expect("non-zero"),
            0,
            0,
        )
    } else {
        lash_sansio::ExecutionPolicy::Once
    }
}

/// The member bodies: each answers a fresh token, and records whether its
/// admission was durable when it ran.
struct LawBodies {
    shared: Arc<Shared>,
    database: Arc<dyn DurableStore>,
}

impl MemberBodies for LawBodies {
    fn stop_grace(&self) -> std::time::Duration {
        std::time::Duration::from_secs(2)
    }

    fn body(&self, execution: &AdmittedExecution) -> MemberBody {
        let shared = Arc::clone(&self.shared);
        let database = Arc::clone(&self.database);
        let operation = OperationId::of(execution.id());
        let tool = execution
            .draft()
            .tool()
            .as_str()
            .trim_start_matches("tool:")
            .to_owned();
        Box::new(move |_| {
            Box::pin(async move {
                // The snapshot and the admission commit in one transaction,
                // so a stored ledger past the operation's run means its
                // admission stood.
                let admitted_first = match database.snapshot(&exec()).await {
                    Ok(Some(row)) => serde_json::from_str::<Checkpoint>(&row.snapshot_ref)
                        .is_ok_and(|checkpoint| checkpoint.ledger.next_admission > operation.run),
                    _ => false,
                };
                let token = shared.tokens.fetch_add(1, Ordering::SeqCst) as u64;
                shared.bodies.lock().expect("bodies").push(Body {
                    operation: Some(operation),
                    tool,
                    token,
                    admitted_first,
                });
                let text = serde_json::json!({ "token": token }).to_string();
                SettledOutput::Completed(Material::journal_local(
                    MaterialOwner::Run {
                        opener: context().identities.opener().clone(),
                    },
                    MaterialRole::AttemptOutput,
                    text,
                ))
                .into()
            })
        })
    }

    fn resolved(
        &self,
        _execution: &AdmittedExecution,
        _parked: &Material<CompletionSource>,
        _resolution: Resolution,
    ) -> SettledOutput {
        unreachable!("no law tool parks")
    }
}

/// The parent: `write` is a `Once` tool, `read` a `Repeatable` one. Each
/// operation is one admitted member, and the parent answers it from that
/// member's committed outcome alone: a token, or its interruption.
struct LawHost {
    context: AdmittedContext,
    store: Arc<DurableSnapshotStore>,
}

fn tool(operation: &AdmittedOperation) -> String {
    match &operation.kind {
        AdmittedKind::Invoke(call) => call.call.operation.clone(),
        _ => "other".into(),
    }
}

#[async_trait::async_trait]
impl ParentEffects for LawHost {
    async fn admission(&self, operation: &AdmittedOperation) -> Result<Admission, ParentFault> {
        let tool = tool(operation);
        let now = self
            .store
            .context()
            .durable_now()
            .await
            .map_err(|error| ParentFault(error.to_string()))?;
        Ok(Admission {
            members: vec![lash_vm_broker::testing::member_draft(
                &self.context,
                operation,
                &format!("tool:{tool}"),
                policy(&tool),
                u64::try_from(now.0).expect("a time after the epoch"),
            )],
            waits: Vec::new(),
        })
    }

    async fn perform(
        &self,
        operation: &AdmittedOperation,
        _waits: &[(WaitRef, Option<PinnedKey>)],
    ) -> Result<Performed, ParentFault> {
        let driven = self
            .store
            .drive(operation.run, &CancellationToken::new(), &mut |ends, _| {
                ends.first()
                    .and_then(|end| end.settled.clone())
                    .map_or(Decide::Wait { until: None }, Decide::Answer)
            })
            .await
            .map_err(|refusal| ParentFault(refusal.0))?;
        let Driven::Answered(settled) = driven else {
            return Err(ParentFault("a law member waits on nothing".into()));
        };
        let outcome = match &settled.output {
            SettledOutput::Completed(material) => EffectOutcome::Value(encode_value(
                &serde_json::from_str::<serde_json::Value>(material.payload()).expect("a token"),
            )),
            _ => {
                INTERRUPTED.fetch_add(1, Ordering::SeqCst);
                EffectOutcome::Failed(encode_value(
                    &serde_json::json!({ "interrupted": operation.run }),
                ))
            }
        };
        Ok(Performed::outcome(outcome))
    }

    async fn observe_cancellation(&self, _checkpoint: u64) -> Result<bool, ParentFault> {
        Ok(false)
    }
}

/// V1–V4 and NR hold on SQLite in memory with every labelled commit of a
/// cell's run cut under every fault, including `cell.snapshot+admit`,
/// `round.outcome`, `cell.inject` and `cell.snapshot`.
#[tokio::test]
#[allow(
    clippy::disallowed_methods,
    reason = "the test host selects the optional lease proof from its action environment"
)]
async fn a_cell_resumes_from_its_quiet_points_at_every_cut_on_sqlite_memory() {
    let matrix = Matrix::new();
    let make = || CellScenario {
        shared: Arc::default(),
    };
    let report = if std::env::var("LASH_MATRIX_VERIFY_LEASE").as_deref() == Ok("1") {
        matrix.run_lease_equivalence(make).await
    } else {
        matrix.run(make).await
    };
    report.assert_held();
    let labels = report.labels();
    for label in [
        CommitLabel::CELL_SNAPSHOT_ADMIT,
        CommitLabel::ROUND_OUTCOME,
        CommitLabel::CELL_SNAPSHOT,
    ] {
        assert!(
            labels.contains(&label),
            "the matrix cut at {label}: {labels:?}"
        );
    }
    assert!(
        report
            .cells
            .iter()
            .any(|cell| cell.trace.contains(CommitLabel::CELL_INJECT.as_str())),
        "some cut left a started Once to be interrupted on restore"
    );
    assert!(
        INTERRUPTED.load(Ordering::SeqCst) > 0,
        "an interrupted Once answered its cell"
    );
}
