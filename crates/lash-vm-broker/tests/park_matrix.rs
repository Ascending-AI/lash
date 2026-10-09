//! A kernel run's parks on the durable store (kernel spec §2.3, §5; ADR
//! 0132 §8): three tasks each perform one effect, then `main` performs one
//! over their results. The run's parks commit to `lash_exec_snapshots` and
//! `lash_run_records` on SQLite in memory.
//!
//! Crash laws, with every labelled commit cut under every fault and the run
//! resumed on another node (before the park commits, after it commits and
//! before any body runs, after one outcome commits, after two, and on):
//!
//! - **P1, admitted before executed:** a body runs only once the
//!   transaction that saved the state standing on its effect, with its
//!   admission, stands.
//! - **P2, no committed effect runs again:** each effect's `Once` body
//!   runs at most once.
//! - **P3, identities hold:** an effect identity is admitted under one
//!   call, whichever activation admits it.
//! - **P4, answered from records:** each result the run ends with is what
//!   one of that effect's bodies answered, or its interruption.
//! - **P5, the end names nothing:** the last checkpoint records the end,
//!   with a ledger that admitted two parks and stands on nothing.
//!
//! Delivery law: the same committed outcomes, delivered to two machines
//! resumed from one saved state in opposite orders, leave both machines at
//! a park the broker commits.
//!
//! Number law (`K-EFF-005`): through the admitted-execution lifecycle and
//! its run records, a result's numbers reach the machine as written, and an
//! integer or a float argument leaves as digits that lose nothing.

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
use lash_durable::domain::{CellId, ExecKey, RunRecordKind, RunSeq};
use lash_durable::runner::{Activation, Exit, Owned};
use lash_durable::{
    ActorKey, ActorState, CommitLabel, DurableError, DurableStore, FormatSet, MailTx, Release,
};
use lash_durable_test::{Matrix, Scenario, SimClock, SimNodes, SimNodesConfig};
use lash_kernel_doc::{
    Datum, Document, EffectIdentity, EffectName, ErrorDatum, Float, FunctionRegistry, Handle,
    Integer, Manifest, Name, NumberPolicy, NumberToken, Site, SpawnIdentity, TaskIdentity,
    Timestamp, Type, Unit,
};
use lash_kernel_vm::{
    Bindings, Bounds, DeliverError, Delivered, EffectRequest, End, ExportError, Finished, Host,
    ImportError, Machine, MachineError, Meters, Outcome, Park, Program, Request, Start, StartError,
    Step, Target, WaitId,
};
use lash_sansio::{SessionId, ToolCallId, ToolId, TurnId};
use lash_vm_broker::kernel::{
    AdmitAs, EFFECT_INTERRUPTED, EffectAdmission, EffectLedger, InProcess, KernelBroker,
    KernelCeilings, KernelEffects, KernelEnd, ParkSave, ParkedCheckpoint, Settled, datum_to_json,
};
use lash_vm_broker::{
    CodeCallIdentities, Driven, DurableSnapshotStore, MemberDraft, OperationId, ParentFault,
};
use lash_vm_protocol::EncodedPayload;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

const FORMATS: &str = "park-law-v1";
const SESSION: &str = "park-law";
const CREATE: CommitLabel = CommitLabel::new("law.create");
const DONE: CommitLabel = CommitLabel::new("law.done");
const WORKERS: u64 = 3;

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

fn identities() -> CodeCallIdentities {
    CodeCallIdentities::cell(
        lash_core_store::effect_opener::EffectOpener::turn(SESSION, "turn-1"),
        "cell-1",
    )
}

fn program(numbers: NumberPolicy) -> Program {
    Program {
        document: Arc::new(Document {
            manifest: Manifest::new(numbers),
            functions: BTreeMap::new(),
            entries: BTreeMap::new(),
            private_bindings: Default::default(),
            main: Vec::new(),
        }),
        registry: Arc::new(FunctionRegistry::new()),
    }
}

fn bounds() -> Bounds {
    Bounds {
        charge: u64::MAX,
        memory: u64::MAX,
        call_depth: 64,
        live_tasks: 8,
        requests_per_park: 8,
        join_members: 8,
    }
}

fn start() -> Start {
    Start {
        target: Target::Main,
        args: Vec::new(),
        bindings: Bindings::default(),
    }
}

/// The identity of worker `index`'s `perform`: the task the `index`th run
/// of `main`'s spawn started, at the one `perform` of its function.
fn worker_identity(index: u64) -> EffectIdentity {
    EffectIdentity {
        task: TaskIdentity::Spawned(SpawnIdentity {
            parent: Arc::new(TaskIdentity::Main),
            site: Site::new(Unit::Main, [0]),
            occurrence: index,
        }),
        site: Site::new(Unit::Function(Name::new("worker")), [0]),
        occurrence: 0,
        loops: Vec::new(),
    }
}

fn sum_identity() -> EffectIdentity {
    EffectIdentity {
        task: TaskIdentity::Main,
        site: Site::new(Unit::Main, [2]),
        occurrence: 0,
        loops: Vec::new(),
    }
}

/// What a machine that decodes by spelling makes of a result's number
/// tokens, so the results can leave again as arguments.
fn decoded(value: &Datum) -> Datum {
    match value {
        Datum::Number(token) if token.is_integer_spelling() => {
            Datum::Int(Integer::parse(token.as_str()).expect("integer digits"))
        }
        Datum::Number(token) => Datum::Float(Float::new(
            token.as_str().parse().expect("a JSON number is a float"),
        )),
        Datum::List(items) => Datum::List(items.iter().map(decoded).collect()),
        Datum::Record(fields) => Datum::Record(
            fields
                .iter()
                .map(|(key, value)| (key.clone(), decoded(value)))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// The run as a machine: `main` spawns [`WORKERS`] tasks that each perform
/// `tools.work`, joins them all, performs `tools.sum` over what they
/// answered, and finishes with all four results.
struct FanOut {
    state: FanState,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
struct FanState {
    /// The waits handed out so far.
    waits: u64,
    /// Each wait's outcome, once delivered.
    results: BTreeMap<u64, Datum>,
    /// The order the outcomes were delivered in.
    order: Vec<u64>,
    ended: bool,
}

impl FanOut {
    fn request(&mut self, identity: EffectIdentity, effect: &str, args: Vec<Datum>) -> Request {
        let wait = WaitId(self.state.waits);
        self.state.waits += 1;
        Request::Effect(EffectRequest {
            wait,
            identity,
            effect: EffectName::new(effect).expect("an effect name"),
            args,
            result: Type::Any,
        })
    }
}

impl Machine for FanOut {
    type Parked = FanState;

    fn start(_: Program, _: Bounds, _: Start) -> Result<Self, StartError> {
        Ok(Self {
            state: FanState::default(),
        })
    }

    fn run(&mut self, _: &mut dyn Host, _: u64) -> Result<Step, MachineError> {
        if self.state.ended {
            return Err(MachineError::Ended);
        }
        let mut park = Park::default();
        let delivered = self.state.results.len() as u64;
        if self.state.waits == 0 {
            for index in 0..WORKERS {
                let request = self.request(
                    worker_identity(index),
                    "tools.work",
                    vec![Datum::Int(Integer::from(index as i64))],
                );
                park.requests.push(request);
            }
        } else if self.state.waits == WORKERS && delivered == WORKERS {
            let answers = (0..WORKERS)
                .map(|wait| match &self.state.results[&wait] {
                    Datum::Record(fields) => fields
                        .iter()
                        .find(|(key, _)| key == "n")
                        .map_or(Datum::Null, |(_, value)| decoded(value)),
                    _ => Datum::Null,
                })
                .collect();
            let request = self.request(sum_identity(), "tools.sum", vec![Datum::List(answers)]);
            park.requests.push(request);
        } else if delivered == WORKERS + 1 {
            self.state.ended = true;
            return Ok(Step::Ended(End::Finished(Finished {
                result: Datum::List(self.state.results.values().cloned().collect()),
                finish: true,
                bindings: Bindings::default(),
                not_carried: Vec::new(),
                closures: Bindings::default(),
            })));
        }
        Ok(Step::Parked(park))
    }

    fn deliver(&mut self, wait: WaitId, outcome: Outcome) -> Result<Delivered, DeliverError> {
        if self.state.ended {
            return Err(DeliverError::Ended);
        }
        if wait.0 >= self.state.waits {
            return Err(DeliverError::UnknownWait { wait });
        }
        if self.state.results.contains_key(&wait.0) {
            return Err(DeliverError::AlreadyDelivered { wait });
        }
        let result = match outcome {
            Outcome::Completed(result) => result,
            Outcome::Failed(error) => Datum::Error(Box::new(error)),
            Outcome::Elapsed => return Err(DeliverError::WrongOutcome { wait }),
        };
        self.state.results.insert(wait.0, result);
        self.state.order.push(wait.0);
        Ok(Delivered::Accepted)
    }

    fn export(&mut self) -> Result<FanState, ExportError> {
        if self.state.ended {
            return Err(ExportError::Ended);
        }
        Ok(self.state.clone())
    }

    fn import(_: Program, _: Bounds, state: FanState) -> Result<Self, ImportError> {
        Ok(Self { state })
    }

    fn meters(&self) -> Meters {
        Meters::default()
    }
}

struct NoReads;

impl Host for NoReads {
    fn clock(&mut self) -> Timestamp {
        Timestamp {
            nanoseconds: Integer::from(0),
        }
    }

    fn random(&mut self) -> u64 {
        0
    }

    fn read(&mut self, _: &Handle, _: &Datum) -> Result<Datum, ErrorDatum> {
        Ok(Datum::Null)
    }

    fn print(&mut self, _: &Datum) {}

    fn cancel_requested(&mut self) -> bool {
        false
    }
}

/// One body run: its execution, its call, the token it answered, and
/// whether its admission was durable when it ran.
#[derive(Clone, Debug)]
struct Body {
    operation: OperationId,
    call: ToolCallId,
    token: u64,
    admitted_first: bool,
}

/// What every node's activations share.
#[derive(Default)]
struct Shared {
    backend: Mutex<Option<Backend>>,
    bodies: Mutex<Vec<Body>>,
    tokens: AtomicUsize,
    /// Every admission the parent drafted: the effect's identity, its call
    /// and the request it was admitted over.
    admissions: Mutex<Vec<(EffectIdentity, ToolCallId, String)>>,
    ends: Mutex<Vec<Datum>>,
    /// Law violations an activation found itself.
    violations: Mutex<Vec<String>>,
    /// Why activations stopped short, for a stalled run's report.
    stops: Mutex<Vec<String>>,
}

impl Shared {
    fn backend(&self) -> Backend {
        self.backend
            .lock()
            .expect("backend")
            .clone()
            .expect("the database opened first")
    }

    fn violation(&self, violation: String) {
        self.violations.lock().expect("violations").push(violation);
    }
}

/// How an activation drives the run.
#[derive(Clone, Copy)]
enum Drive {
    /// Through [`KernelBroker::run`], start to end.
    Broker,
    /// The first park by hand, to deliver its outcomes in two orders.
    TwoOrders,
}

struct ParkScenario {
    shared: Arc<Shared>,
    drive: Drive,
    numbers: NumberPolicy,
}

#[async_trait::async_trait]
impl Scenario for ParkScenario {
    async fn database(&self, clock: Arc<SimClock>) -> Arc<dyn DurableStore> {
        let stores = lash_sqlite_store::SqliteStoreSet::memory_with_clock(clock)
            .await
            .expect("an in-memory store set opens");
        let durable: Arc<dyn DurableStore> = Arc::new(stores.durable_store());
        *self.shared.backend.lock().expect("backend") =
            Some(Backend::for_testing(Arc::new(stores)));
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
        Arc::new(ParkActivation {
            shared: Arc::clone(&self.shared),
            drive: self.drive,
            numbers: self.numbers,
        })
    }

    async fn start(&self, nodes: &Arc<SimNodes>) -> Result<(), String> {
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
        let mut violations = self.shared.violations.lock().expect("violations").clone();
        let bodies = self.shared.bodies.lock().expect("bodies").clone();
        let mut runs: BTreeMap<OperationId, usize> = BTreeMap::new();
        for body in &bodies {
            if !body.admitted_first {
                violations.push(format!(
                    "P1: {:?} ran before the park admitting it committed",
                    body.operation
                ));
            }
            *runs.entry(body.operation).or_default() += 1;
        }
        for (operation, count) in &runs {
            if *count > 1 {
                violations.push(format!("P2: the body of {operation:?} ran {count} times"));
            }
        }
        let admissions = self.shared.admissions.lock().expect("admissions").clone();
        let mut calls: BTreeMap<EffectIdentity, ToolCallId> = BTreeMap::new();
        for (identity, call, _) in &admissions {
            let first = calls
                .entry(identity.clone())
                .or_insert_with(|| call.clone());
            if first != call {
                violations.push(format!(
                    "P3: {identity:?} was admitted as {first} and as {call}"
                ));
            }
        }
        let ends = self.shared.ends.lock().expect("ends").clone();
        let Some(end) = ends.last() else {
            violations.push(format!(
                "the run never ended: {:?}",
                self.shared.stops.lock().expect("stops")
            ));
            return violations;
        };
        if ends.iter().any(|other| other != end) {
            violations.push(format!("P4: the run ended differently: {ends:?}"));
        }
        let results = match end {
            Datum::List(results) if results.len() as u64 == WORKERS + 1 => results.clone(),
            other => {
                violations.push(format!("P4: the run ended with {other:?}"));
                Vec::new()
            }
        };
        let identities: Vec<EffectIdentity> = (0..WORKERS)
            .map(worker_identity)
            .chain([sum_identity()])
            .collect();
        for (identity, result) in identities.iter().zip(&results) {
            let call = calls.get(identity);
            let answered = match result {
                Datum::Record(fields) => fields.iter().any(|(key, value)| {
                    key == "token"
                        && bodies.iter().any(|body| {
                            Some(&body.call) == call
                                && *value == Datum::Number(token(&body.token.to_string()))
                        })
                }),
                Datum::Error(error) => error.kind == EFFECT_INTERRUPTED,
                _ => false,
            };
            if !answered {
                violations.push(format!(
                    "P4: {identity:?} ended as {result:?}, neither a body's answer nor its interruption"
                ));
            }
        }
        match nodes.database().snapshot(&exec()).await {
            Ok(Some(row)) => {
                match serde_json::from_str::<ParkedCheckpoint<FanState>>(&row.snapshot_ref) {
                    Ok(checkpoint) => {
                        if checkpoint.end.is_none()
                            || checkpoint.state.is_some()
                            || checkpoint.ledger.pending().next().is_some()
                            || !checkpoint.ledger.released().is_empty()
                            || checkpoint.ledger.next_park() != 2
                        {
                            violations.push(format!(
                                "P5: the last checkpoint's ledger disagrees with its ended run: {:?}",
                                checkpoint.ledger
                            ));
                        }
                    }
                    Err(error) => {
                        violations.push(format!("P5: the checkpoint does not decode: {error}"));
                    }
                }
            }
            other => violations.push(format!("P5: no last checkpoint: {other:?}")),
        }
        violations
    }
}

fn token(text: &str) -> NumberToken {
    NumberToken::new(text).expect("a JSON number")
}

struct ParkActivation {
    shared: Arc<Shared>,
    drive: Drive,
    numbers: NumberPolicy,
}

impl ParkActivation {
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
impl Activation for ParkActivation {
    async fn activate(&self, owned: Owned) -> Exit {
        let cx = ActorContext::claimed(
            self.shared.backend(),
            &owned,
            AdmittedScope::runtime_operation("park-law"),
            CancellationToken::new(),
            Arc::new(lash_durable::NoProbe),
        );
        let store = Arc::new(DurableSnapshotStore::new(&cx, exec()));
        store
            .bind_members(
                Arc::new(LawBodies {
                    shared: Arc::clone(&self.shared),
                    database: Arc::clone(owned.store()),
                    clock: Arc::clone(cx.clock()),
                }),
                PolicyView::new(["work", "sum"].map(|tool| {
                    (
                        ToolId::new(format!("tool:{tool}")),
                        lash_sansio::ExecutionPolicy::Once,
                    )
                })),
            )
            .await;
        let host = LawHost {
            shared: Arc::clone(&self.shared),
            store: Arc::clone(&store),
        };
        let identities = identities();
        let broker = KernelBroker {
            store: &store,
            effects: &host,
            identities: &identities,
            ceilings: KernelCeilings {
                requests_per_park: 16,
                join_members: 16,
            },
            slice: u64::MAX,
        };
        for _ in 0..8 {
            if let Drive::TwoOrders = self.drive
                && let Err(why) = two_orders(&self.shared, &store, &host, self.numbers).await
            {
                self.stop(&why);
                if lost(&owned).await {
                    return Exit::Released;
                }
                continue;
            }
            let machines =
                InProcess::<FanOut, _>::new(program(self.numbers), bounds(), start(), NoReads)
                    .expect("the document has an identity");
            let run = broker.run(&machines, &CancellationToken::new()).await;
            match run {
                Ok(KernelEnd::Ended(End::Finished(finished))) => {
                    self.shared.ends.lock().expect("ends").push(finished.result);
                    let Ok(mut tx) = owned.begin().await else {
                        return Exit::Released;
                    };
                    tx.give_up(Release::Terminal);
                    match owned.commit(tx, DONE).await {
                        Ok(_) | Err(DurableError::OwnershipLost(_)) => return Exit::Released,
                        Err(_) => continue,
                    }
                }
                Ok(other) => panic!("the run finishes: {other:?}"),
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

/// The delivery and number laws, on the run's first park: commit it, let
/// its three outcomes commit, then resume the saved state twice and deliver
/// the outcomes in opposite orders. Both machines must park on the same
/// next effect; the reversed one's park is committed, and the broker runs
/// on from it.
async fn two_orders(
    shared: &Shared,
    store: &DurableSnapshotStore,
    host: &LawHost,
    numbers: NumberPolicy,
) -> Result<(), String> {
    let refused = |error: &dyn std::fmt::Display| error.to_string();
    if store
        .latest_park::<FanState>()
        .await
        .map_err(|error| refused(&error))?
        .is_some()
    {
        // A later attempt of this activation: the hand-driven stretch
        // committed, and the broker resumes from it.
        return Ok(());
    }
    let program = program(numbers);
    let document = program
        .document
        .identity()
        .map_err(|error| refused(&error))?;
    let mut machine =
        FanOut::start(program.clone(), bounds(), start()).map_err(|error| refused(&error))?;
    let Step::Parked(park) = machine
        .run(&mut NoReads, u64::MAX)
        .map_err(|error| refused(&error))?
    else {
        return Err("the run parks first".into());
    };
    let committed = store
        .commit_park(ParkSave {
            document,
            state: machine.export().map_err(|error| refused(&error))?,
            ledger: EffectLedger::new(),
            admit: admissions(host, 0, park.requests).await?,
            host: None,
            with: Vec::new(),
        })
        .await
        .map_err(|error| refused(&error))?;
    let saved = committed.checkpoint;
    // Every outcome of the park, as the records hold them.
    let mut waiting = saved.ledger.clone();
    let mut settled: Vec<Settled> = Vec::new();
    while waiting.pending().next().is_some() {
        let Driven::Answered(more) = store
            .drive_effects(&waiting, &CancellationToken::new())
            .await
            .map_err(|error| refused(&error))?
        else {
            return Err("the park's effects settle on this activation".into());
        };
        for outcome in more {
            waiting.consume(&outcome.identity);
            settled.push(outcome);
        }
    }
    settled.sort_by_key(|outcome| outcome.wait);
    let expected = [
        Datum::Number(token("9007199254740993")),
        Datum::Number(token("18446744073709551617")),
        Datum::List(vec![
            Datum::Number(token("5.0")),
            Datum::Number(token("1e3")),
        ]),
    ];
    for (outcome, expected) in settled.iter().zip(&expected) {
        let written = match &outcome.outcome {
            Outcome::Completed(Datum::Record(fields)) => fields
                .iter()
                .find(|(key, _)| key == "n")
                .map(|(_, value)| value.clone()),
            _ => None,
        };
        if written.as_ref() != Some(expected) {
            shared.violation(format!(
                "numbers: wait {} arrived as {written:?}, not as written: {expected:?}",
                outcome.wait.0
            ));
        }
    }
    let Some(state) = saved.state.clone() else {
        return Err("a park saves its state".into());
    };
    let mut parked = Vec::new();
    for order in [[0_usize, 1, 2], [2, 1, 0]] {
        let mut machine = FanOut::import(program.clone(), bounds(), state.clone())
            .map_err(|error| refused(&error))?;
        let mut ledger = saved.ledger.clone();
        for index in order {
            let outcome = settled[index].clone();
            machine
                .deliver(outcome.wait, outcome.outcome)
                .map_err(|error| refused(&error))?;
            ledger.consume(&outcome.identity);
        }
        match machine.run(&mut NoReads, u64::MAX) {
            Ok(Step::Parked(park)) => parked.push((machine, ledger, park)),
            other => shared.violation(format!(
                "delivery: after order {order:?} the machine did not park: {other:?}"
            )),
        }
    }
    let [(_, _, forward), (_, _, reversed)] = &parked[..] else {
        return Err("both delivery orders park".into());
    };
    if forward != reversed {
        shared.violation(format!(
            "delivery: the two orders parked on different effects: {forward:?} and {reversed:?}"
        ));
    }
    // The reversed order's park commits: its state consumed all three
    // outcomes, and the broker resumes from it.
    let Some((mut machine, ledger, park)) = parked.pop() else {
        return Err("both delivery orders park".into());
    };
    let state = machine.export().map_err(|error| refused(&error))?;
    if state.order != [2, 1, 0] {
        shared.violation(format!("delivery: the saved order is {:?}", state.order));
    }
    store
        .commit_park(ParkSave {
            document,
            state,
            admit: admissions(host, ledger.next_park(), park.requests).await?,
            ledger,
            host: None,
            with: Vec::new(),
        })
        .await
        .map_err(|error| refused(&error))?;
    let sum = shared
        .admissions
        .lock()
        .expect("admissions")
        .iter()
        .find(|(identity, _, _)| *identity == sum_identity())
        .map(|(_, _, request)| request.clone());
    let exact =
        r#"{"effect":"tools.sum","args":[[9007199254740993,18446744073709551617,[5.0,1000.0]]]}"#;
    if sum.as_deref() != Some(exact) {
        shared.violation(format!(
            "numbers: the arguments left as {sum:?}, not {exact}"
        ));
    }
    Ok(())
}

/// How `park`'s requests are admitted, as the broker's own loop drafts
/// them.
async fn admissions(
    host: &LawHost,
    park: u64,
    requests: Vec<Request>,
) -> Result<Vec<EffectAdmission>, String> {
    let identities = identities();
    let mut admit = Vec::new();
    for (index, request) in requests.into_iter().enumerate() {
        let Request::Effect(request) = request else {
            return Err("the run sleeps nowhere".into());
        };
        let call = identities.child_call_id(park, index as u64);
        let draft = host
            .admit(&request, call)
            .await
            .map_err(|fault| fault.to_string())?
            .map_err(|refusal| refusal.message)?;
        admit.push(EffectAdmission {
            identity: request.identity,
            wait: request.wait,
            effect: Some(request.effect),
            admit: AdmitAs::Execution(Box::new(draft)),
        });
    }
    Ok(admit)
}

/// The effect bodies: each answers a fresh token beside the numbers its
/// worker returns, and records whether its admission was durable when it
/// ran. A worker's body holds its answer until the workers before it have
/// their outcomes committed, so each outcome commits on its own.
struct LawBodies {
    shared: Arc<Shared>,
    database: Arc<dyn DurableStore>,
    /// The store's clock: a waiting body sleeps on it, so the nodes' time
    /// moves on while it waits.
    clock: Arc<dyn lash_core_execution::Clock>,
}

/// The numbers worker `index` answers, as its tool writes them.
fn numbers_of(index: u64) -> &'static str {
    match index {
        0 => "9007199254740993",
        1 => "18446744073709551617",
        _ => "[5.0,1e3]",
    }
}

impl MemberBodies for LawBodies {
    fn stop_grace(&self) -> Duration {
        Duration::from_secs(2)
    }

    fn body(&self, execution: &AdmittedExecution) -> MemberBody {
        let shared = Arc::clone(&self.shared);
        let database = Arc::clone(&self.database);
        let clock = Arc::clone(&self.clock);
        let operation = OperationId::of(execution.id());
        let call = execution.call().clone();
        // Which worker's effect this is, by the identity its call was
        // admitted for; none for the sum.
        let worker = self
            .shared
            .admissions
            .lock()
            .expect("admissions")
            .iter()
            .find(|(_, admitted, _)| *admitted == call)
            .and_then(|(identity, _, _)| {
                (0..WORKERS).find(|index| worker_identity(*index) == *identity)
            });
        Box::new(move |_| {
            Box::pin(async move {
                // The state and the admission commit in one transaction, so
                // a stored ledger that names the execution means its
                // admission stood.
                let admitted_first = match database.snapshot(&exec()).await {
                    Ok(Some(row)) => {
                        serde_json::from_str::<ParkedCheckpoint<FanState>>(&row.snapshot_ref)
                            .is_ok_and(|checkpoint| {
                                checkpoint
                                    .ledger
                                    .executions()
                                    .any(|admitted| admitted.operation == operation)
                            })
                    }
                    _ => false,
                };
                let token = shared.tokens.fetch_add(1, Ordering::SeqCst) as u64;
                shared.bodies.lock().expect("bodies").push(Body {
                    operation,
                    call,
                    token,
                    admitted_first,
                });
                while let Some(index) = worker {
                    let outcomes = database
                        .run_records(&exec().owner())
                        .await
                        .map_or(0, |rows| {
                            rows.iter()
                                .filter(|row| {
                                    row.run == RunSeq(operation.run)
                                        && row.kind == RunRecordKind::XOutcome
                                })
                                .count() as u64
                        });
                    if outcomes >= index {
                        break;
                    }
                    clock.sleep(Duration::from_millis(2)).await;
                }
                let numbers = worker.map_or("null", numbers_of);
                SettledOutput::Completed(Material::journal_local(
                    MaterialOwner::Run {
                        opener: identities().opener().clone(),
                    },
                    MaterialRole::AttemptOutput,
                    format!(r#"{{"token":{token},"n":{numbers}}}"#),
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
        unreachable!("no law effect parks")
    }
}

/// The parent: every effect is one `Once` execution over its arguments as
/// JSON text.
struct LawHost {
    shared: Arc<Shared>,
    store: Arc<DurableSnapshotStore>,
}

#[async_trait::async_trait]
impl KernelEffects for LawHost {
    async fn admit(
        &self,
        request: &EffectRequest,
        call: ToolCallId,
    ) -> Result<Result<MemberDraft, ErrorDatum>, ParentFault> {
        let now = self
            .store
            .context()
            .durable_now()
            .await
            .map_err(|error| ParentFault(error.to_string()))?;
        let args = datum_to_json(&Datum::List(request.args.clone()))
            .map_err(|refusal| ParentFault(refusal.to_string()))?;
        let text = format!(r#"{{"effect":"{}","args":{args}}}"#, request.effect);
        self.shared.admissions.lock().expect("admissions").push((
            request.identity.clone(),
            call.clone(),
            text.clone(),
        ));
        let tool = request.effect.as_str().trim_start_matches("tools.");
        MemberDraft::new(
            call,
            ToolId::new(format!("tool:{tool}")),
            EncodedPayload(text.into_bytes()),
            identities().opener(),
            (
                lash_sansio::ExecutionPolicy::Once,
                lash_sansio::ExecutionLimit::starting_at(
                    u64::try_from(now.0).expect("a time after the epoch"),
                    Duration::from_secs(3600),
                    Duration::from_secs(3600),
                ),
            ),
            None,
        )
        .map(Ok)
    }
}

/// P1 to P5 hold on SQLite in memory with every labelled commit of the
/// run cut under every fault: the park commits (`cell.snapshot+admit`),
/// each effect's outcome (`round.outcome`), an interrupted effect's
/// settlement (`cell.inject`) and the end (`cell.snapshot`).
#[tokio::test]
async fn a_run_of_three_tasks_resumes_from_its_parks_at_every_cut_on_sqlite_memory() {
    let report = Matrix::new()
        .run(|| ParkScenario {
            shared: Arc::default(),
            drive: Drive::Broker,
            numbers: NumberPolicy::Float,
        })
        .await;
    report.assert_held();
    // The uncut run: the first park admits the three workers, each outcome
    // commits on its own, the second park saves the state that consumed
    // them and admits the sum, and the end commits last.
    report.assert_baseline_labels(&[
        CommitLabel::CELL_SNAPSHOT_ADMIT,
        CommitLabel::ROUND_OUTCOME,
        CommitLabel::ROUND_OUTCOME,
        CommitLabel::ROUND_OUTCOME,
        CommitLabel::CELL_SNAPSHOT_ADMIT,
        CommitLabel::ROUND_OUTCOME,
        CommitLabel::CELL_SNAPSHOT,
        DONE,
    ]);
    assert!(
        report
            .cells
            .iter()
            .any(|cell| cell.trace.contains(CommitLabel::CELL_INJECT.as_str())),
        "some cut left a started effect to be interrupted on restore"
    );
}

/// The delivery law and the number law, under each bare-number policy: the
/// effect-value path is the same under both, because it decodes nothing.
#[tokio::test]
async fn committed_outcomes_delivered_in_either_order_resume_and_keep_their_numbers() {
    for numbers in [NumberPolicy::Float, NumberPolicy::BySpelling] {
        Matrix::new()
            .faults(&[])
            .run(|| ParkScenario {
                shared: Arc::default(),
                drive: Drive::TwoOrders,
                numbers,
            })
            .await
            .assert_held();
    }
}
