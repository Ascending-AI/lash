#![cfg(unix)]
#![expect(
    clippy::expect_used,
    reason = "integration test helpers fail on broken fixture assumptions"
)]
//! A pooled worker hosts one kernel machine for one execution (kernel spec
//! §9 rule 5): the parent drives it a slice at a time, answers its host
//! reads, and can stop at any park and carry on in another process.

use lash_kernel_doc::{Datum, Float, Handle, Integer, Name, Timestamp, parse_document};
use lash_kernel_vm::{Bindings, Bound, End, Outcome, Request, RunError, Start as RunStart, Target};
use lash_vm_client::wire::{self, EndWire, OutcomeWire, ParkWire, ProjectionRead, StartWire};
use lash_vm_client::*;
use lash_vm_protocol::*;

const SLICE: u64 = 1_000_000;

/// The dialect the laws' runs are written in, which every embedding the
/// laws run under installs.
const DIALECT: &str = "typescript";

thread_local! {
    /// The dialects the host of the law running on this thread names: what
    /// its workers' embedding installs.
    static HOST_DIALECTS: std::cell::Cell<&'static str> = const { std::cell::Cell::new("") };
}

/// Runs `law` with workers whose host names `dialects`.
fn under(dialects: &'static str, law: fn()) {
    HOST_DIALECTS.set(dialects);
    law();
}

/// Every law holds whichever dialects the host names beside the one its
/// runs are written in: with a TypeScript-only embedding, and with one of
/// TypeScript and Python (FIG-5990).
macro_rules! under_each_embedding {
    ($($law:ident),* $(,)?) => {
        $(
            mod $law {
                #[test]
                fn typescript_only() {
                    super::under("typescript", super::$law);
                }

                #[test]
                fn typescript_and_python() {
                    super::under("typescript,python", super::$law);
                }
            }
        )*
    };
}

under_each_embedding!(
    a_parked_run_carries_on_in_another_worker_without_asking_again,
    parts_that_outgrow_one_frame_cross_apart_and_the_run_goes_on_the_same,
    a_run_exported_as_graph_state_resumes_in_another_worker_from_its_admission,
    a_typescript_cell_lowered_in_the_worker_parks_and_resumes,
    a_parked_run_is_refused_under_another_document,
    host_reads_are_answered_by_the_parent_and_prints_arrive_in_order,
    a_run_cancel_is_observed_where_a_slice_returns,
    a_passed_bound_is_the_runs_typed_end_and_the_worker_is_reused,
    worker_crash_mid_run_leaves_the_parent_running,
    a_forbidden_syscall_kills_the_worker_with_typed_evidence,
    a_panic_with_a_backtrace_requested_is_typed_not_a_forbidden_syscall,
    the_address_space_ceiling_bounds_a_runs_allocation,
    workers_refuse_another_protocol_before_guest_admission,
    a_worker_silent_during_bootstrap_reports_the_time_it_was_silent,
);

fn config(mode: &str) -> PoolConfig {
    let mut entry = WorkerEntry::helper(env!("CARGO_BIN_EXE_lash-vm-worker-fixture"));
    // The fixture reads its mode first.
    entry.args.push(mode.into());
    match HOST_DIALECTS.get() {
        "typescript" => entry.args.push("--dialects=typescript".into()),
        "typescript,python" => {}
        other => panic!("a law runs under a named embedding, not `{other}`"),
    }
    // The shipped watchdog: it also bounds a worker's startup.
    let mut config = PoolConfig::standard(entry);
    config.max_workers = 1;
    config
}

fn checkout(pool: &WorkerPool) -> Checkout {
    pool.checkout(
        4 * 1024 * 1024 + FRAME_HEADER_BYTES,
        OwnerEpoch(1),
        FrameEpoch(1),
        ExecutionBudget::default(),
    )
    .expect("checkout")
}

fn owner() -> VmOwner {
    VmOwner::new("session-A")
}

/// `body` as a document against the registry every worker assembles, with
/// its identity and its wire payload.
fn document(body: &str) -> (String, EncodedPayload) {
    let embedding = lash_vm_worker::built_dialects(&WorkerTuning::standard())
        .expect("an embedding of every built dialect");
    let mut text = String::from("numbers by_spelling\nkernel 1\neffect echo(x?: Any) -> Any\n");
    for (name, id) in embedding.library().iter() {
        if ["num.add", "num.lt", "text.repeat"].contains(&name.to_string().as_str()) {
            text.push_str(&format!("use {name} = @{id}\n"));
        }
    }
    text.push_str(body);
    let document = parse_document(&text).unwrap_or_else(|error| panic!("{error}\n{text}"));
    (
        document.identity().expect("identity").to_string(),
        wire::wrap(
            PayloadKind::Document,
            document.to_json().expect("json").as_bytes(),
        )
        .expect("payload"),
    )
}

fn bounds() -> RunBounds {
    PoolConfig::standard(WorkerEntry::helper("unused")).run_bounds
}

fn fresh(document: &EncodedPayload, target: Target, args: Vec<Datum>) -> Start {
    Start {
        owner: owner(),
        dialect: DIALECT.into(),
        document: document.clone(),
        from: StartFrom::Fresh(
            wire::encode(
                PayloadKind::Start,
                &StartWire::of(
                    RunStart {
                        target,
                        args,
                        bindings: Bindings::default(),
                    },
                    &lash_kernel_vm::EffectModes::recorded(),
                ),
            )
            .expect("start"),
        ),
        bounds: bounds(),
        export: Some(binding()),
    }
}

/// The one effect the park requests: its wait and its argument.
fn requested(step: RunStep) -> (u64, Datum) {
    let RunStep::Parked { park, .. } = step else {
        panic!("the run parks: {step:?}")
    };
    let park: ParkWire = wire::decode(PayloadKind::Park, &park).expect("park");
    let park = lash_kernel_vm::Park::try_from(park).expect("canonical park");
    let [Request::Effect(request)] = &park.requests[..] else {
        panic!("one effect: {park:?}")
    };
    assert_eq!(request.effect.to_string(), "echo");
    (request.wait.0, request.args[0].clone())
}

fn completed(value: Datum) -> EncodedPayload {
    wire::encode(
        PayloadKind::Outcome,
        &OutcomeWire::from(Outcome::Completed(value)),
    )
    .expect("outcome")
}

fn ended(step: RunStep) -> End {
    let RunStep::Ended { end, .. } = step else {
        panic!("the run ends: {step:?}")
    };
    let end: EndWire = wire::decode(PayloadKind::End, &end).expect("end");
    // The read decodes the release's nodes beside the end's own, so the
    // end's size does not bound it.
    end.map_or(End::Cancelled, |state| {
        lash_vm_client::graph::read_end(
            &state,
            lash_core_store::checkpoint_graph::DecodeLimits::within_total(1 << 30),
        )
        .expect("the end reads back")
    })
}

/// The one cell every run here is bound to.
fn binding() -> EncodedPayload {
    EncodedPayload(lash_kernel_doc::graph_tokens::encode_body(
        &lash_core_store::checkpoint_graph::bodies::OwnerBinding::Cell {
            session: "session-A".into(),
            turn: "turn-1".into(),
            cell: "cell-1".into(),
        },
    ))
}

/// The run's state at its park, bound to its cell.
fn export(checkout: &mut Checkout) -> SealedGraphState {
    checkout.export_graph(binding()).expect("export graph")
}

fn int(value: i64) -> Datum {
    Datum::Int(Integer::from(value))
}

const TOOL_LOOP: &str = r#"
main {
  let total = 0
  for step in [1, 2, 3] {
    let got = perform echo(step) as Any
    set total = num.add(total, got)
  }
  return total
}"#;

/// Runs `document`, which performs `echo` three times in turn, answering
/// each with its argument: the worker is killed at the second park, after
/// the state is exported, and another worker carries on from the state.
/// Returns how the run finished and every argument `echo` was asked with.
fn killed_at_a_park_and_resumed(
    pool: &WorkerPool,
    identity: &str,
    document: &EncodedPayload,
) -> (lash_kernel_vm::Finished, Vec<Datum>) {
    let mut asked = Vec::new();
    let mut first = checkout(pool);
    let pid = first.pid().expect("pid");
    first
        .start(
            fresh(document, Target::Main, Vec::new()),
            identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    let (wait, argument) = requested(first.run(SLICE, false).expect("run"));
    asked.push(argument.clone());
    first.deliver(wait, completed(argument)).expect("deliver");
    let (wait, argument, admission) = admitted(first.run(SLICE, false).expect("run"));
    asked.push(argument.clone());
    let state = export(&mut first);
    assert_eq!(state.document(), identity);
    // Killed at the park: the worker is discarded, not reset.
    drop(first);

    let mut second = checkout(pool);
    assert_ne!(second.pid(), Some(pid));
    second
        .start(
            Start {
                owner: owner(),
                dialect: DIALECT.into(),
                document: document.clone(),
                from: StartFrom::ParkedGraph(ParkedGraph {
                    state,
                    admissions: vec![admission],
                }),
                bounds: bounds(),
                export: Some(binding()),
            },
            identity,
            ExecutionClass::Cell,
        )
        .expect("resume");
    second.deliver(wait, completed(argument)).expect("deliver");
    let (wait, argument) = requested(second.run(SLICE, false).expect("run"));
    asked.push(argument.clone());
    second.deliver(wait, completed(argument)).expect("deliver");
    let End::Finished(finished) = ended(second.run(SLICE, false).expect("run")) else {
        panic!("the run finishes")
    };
    second
        .release()
        .expect("the worker resets at the run's end");
    (finished, asked)
}

/// Targets 1 and 2: a run that loops over effects parks at each, its state
/// is exported at the park, its worker is killed, and another worker carries
/// on from the state. The effect answered before the export is not asked
/// for again.
fn a_parked_run_carries_on_in_another_worker_without_asking_again() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let (identity, document) = document(TOOL_LOOP);
    let (finished, asked) = killed_at_a_park_and_resumed(&pool, &identity, &document);
    assert_eq!(finished.result, int(6));
    assert_eq!(asked, [int(1), int(2), int(3)], "each effect is asked once");
}

/// FIG-5921: a run's start crosses with its first slice, the outcomes it
/// waits on with its next, and a saved park's state with the park, each in
/// one frame. Parts that outgrow one frame together cross apart, and the
/// run goes on the same: a resumed run's start and a large outcome each
/// cross alone before the slice, and a park whose state and request
/// together outgrow a frame hands the state over when the parent asks for
/// it.
fn parts_that_outgrow_one_frame_cross_apart_and_the_run_goes_on_the_same() {
    const FRAME: u32 = 64 * 1024;
    let mut config = config("");
    config.protocol.decode.max_frame_bytes = FRAME;
    let pool = WorkerPool::new(config).expect("pool");
    let sent = || pool.measurements().counters.ipc_sent_messages;
    // Never run: it makes the document, and so each start, large.
    let padding = "p".repeat(30 * 1024);
    let (identity, document) = document(&format!(
        r#"
main {{
  if num.lt(1, 0) {{ let unused = "{padding}" }}
  let kept = perform echo("a") as Any
  let again = perform echo(kept) as Any
  return again
}}"#
    ));
    let large = Datum::Text("y".repeat(40 * 1024));

    let mut first = checkout(&pool);
    first
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    let (wait, _, admission) = admitted(first.run(SLICE, false).expect("run"));
    let before = sent();
    let state = export(&mut first);
    assert_eq!(sent(), before, "a small park carries its state");
    drop(first);

    let mut second = checkout(&pool);
    second
        .start(
            Start {
                owner: owner(),
                dialect: DIALECT.into(),
                document: document.clone(),
                from: StartFrom::ParkedGraph(ParkedGraph {
                    state,
                    admissions: vec![admission],
                }),
                bounds: bounds(),
                export: Some(binding()),
            },
            &identity,
            ExecutionClass::Cell,
        )
        .expect("resume");
    second
        .deliver(wait, completed(large.clone()))
        .expect("deliver");
    let before = sent();
    let (_, argument) = requested(second.run(SLICE, false).expect("run"));
    assert_eq!(argument, large, "the large outcome was delivered");
    assert_eq!(
        sent() - before,
        3,
        "the start, the outcome and the slice cross apart"
    );
    let before = sent();
    let state = export(&mut second);
    assert_eq!(state.document(), identity);
    assert_eq!(sent() - before, 1, "the parent asks for the large state");
    second.release().expect("release at the park");
}

/// The one effect the park requests, as the parent's admission freezes it.
fn admitted(step: RunStep) -> (u64, Datum, EncodedPayload) {
    let RunStep::Parked { park, .. } = step else {
        panic!("the run parks: {step:?}")
    };
    let park: ParkWire = wire::decode(PayloadKind::Park, &park).expect("park");
    let park = lash_kernel_vm::Park::try_from(park).expect("canonical park");
    let [Request::Effect(request)] = &park.requests[..] else {
        panic!("one effect: {park:?}")
    };
    let admission = lash_vm_client::graph::admission(&lash_kernel_state::AdmittedWait {
        identity: request.identity.clone(),
        request: lash_kernel_state::AdmittedRequest::Effect {
            effect: request.effect.clone(),
            args: request.args.clone(),
        },
    });
    (request.wait.0, request.args[0].clone(), admission)
}

/// FIG-5887 §3: a run exported at a park as checkpoint graph state, its
/// worker killed, resumes in another worker from the state and the
/// admission of the wait it holds, and finishes without asking again. The
/// state alone, without the admission, is refused with the kernel's typed
/// refusal.
fn a_run_exported_as_graph_state_resumes_in_another_worker_from_its_admission() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let (identity, document) = document(TOOL_LOOP);
    let mut first = checkout(&pool);
    first
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    let (wait, argument) = requested(first.run(SLICE, false).expect("run"));
    first.deliver(wait, completed(argument)).expect("deliver");
    let (wait, argument, admission) = admitted(first.run(SLICE, false).expect("run"));
    let state = export(&mut first);
    assert_eq!(state.document(), identity);
    drop(first);

    let resume = |admissions: Vec<EncodedPayload>| Start {
        owner: owner(),
        dialect: DIALECT.into(),
        document: document.clone(),
        from: StartFrom::ParkedGraph(ParkedGraph {
            state: state.clone(),
            admissions,
        }),
        bounds: bounds(),
        export: Some(binding()),
    };
    let mut refused = checkout(&pool);
    refused
        .start(resume(Vec::new()), &identity, ExecutionClass::Cell)
        .expect("the parent takes the state");
    let refusal = refused
        .run(SLICE, false)
        .expect_err("a wait with no admission");
    assert!(
        matches!(
            refusal,
            PoolError::Infrastructure(InfrastructureOutcome::RunRefused {
                refusal: RunRefusal::Restore {
                    refusal: KernelRestoreRefusal::AdmissionUnresolved { .. },
                }
            })
        ),
        "{refusal:?}"
    );

    let mut second = checkout(&pool);
    second
        .start(resume(vec![admission]), &identity, ExecutionClass::Cell)
        .expect("resume from graph state");
    second.deliver(wait, completed(argument)).expect("deliver");
    let (wait, argument) = requested(second.run(SLICE, false).expect("run"));
    assert_eq!(argument, int(3), "the answered effects are not asked again");
    second.deliver(wait, completed(argument)).expect("deliver");
    let End::Finished(finished) = ended(second.run(SLICE, false).expect("run")) else {
        panic!("the run finishes")
    };
    assert_eq!(finished.result, int(6));
}

/// Targets 1 and 5: a TypeScript cell is lowered in the worker against the
/// functions it registered at startup, and the document it gives runs,
/// parks, is killed and resumes like any other. The cell's top-level
/// binding is the session's when the run finishes.
fn a_typescript_cell_lowered_in_the_worker_parks_and_resumes() {
    use lash_vm_client::service::{Request, Response};
    let pool = WorkerPool::new(config("")).expect("pool");
    let echo = lash_kernel_doc::Signature {
        params: vec![lash_kernel_doc::Param {
            name: Name::new("x"),
            ty: lash_kernel_doc::Type::Any,
            optional: false,
        }],
        result: lash_kernel_doc::Type::Any,
    };
    let request = Request::Lower {
        dialect: "typescript".into(),
        source:
            "let total = 0; for (const step of [1, 2, 3]) { total = total + await echo(step); }"
                .into(),
        effects: [(
            lash_kernel_doc::EffectName::new("echo").expect("effect name"),
            echo,
        )]
        .into(),
        controls: Default::default(),
        tool_roots: Default::default(),
        bindings: Default::default(),
        functions: Default::default(),
        helpers: lash_vm_library::HELPER_RELEASE,
        reads: Default::default(),
    };
    let mut worker = checkout(&pool);
    let response = worker
        .prepare(
            owner(),
            EncodedPayload(rmp_serde::to_vec_named(&request).expect("request")),
        )
        .expect("lowered in the worker");
    worker.release().expect("reset");
    let Response::Lowered { document, .. } = rmp_serde::from_slice(&response.0).expect("response")
    else {
        panic!("the cell lowers")
    };
    let identity = lash_kernel_doc::Document::from_json(
        std::str::from_utf8(&document).expect("document text"),
    )
    .expect("document")
    .identity()
    .expect("identity")
    .to_string();
    let document = wire::wrap(PayloadKind::Document, &document).expect("payload");
    let (finished, asked) = killed_at_a_park_and_resumed(&pool, &identity, &document);
    assert_eq!(asked.len(), 3, "each call is asked once: {asked:?}");
    assert_eq!(
        format!("{:?}", finished.bindings.variables.get(&Name::new("total"))),
        format!("{:?}", Some(lash_kernel_doc::Value::Float(Float::new(6.0)))),
    );
}

/// FIG-5990: a run names the dialect its session or process recorded at
/// creation, and a worker whose host did not name that dialect refuses it
/// with a typed refusal before it reads the document or runs anything: a
/// new run before its first slice, and a parked one before its state. The
/// worker of a host that named the dialect runs the same start.
#[test]
fn a_worker_refuses_a_run_in_a_dialect_its_host_did_not_name() {
    let (identity, document) = {
        HOST_DIALECTS.set("typescript,python");
        document(TOOL_LOOP)
    };
    let python = |from: StartFrom| Start {
        owner: owner(),
        dialect: "python".into(),
        document: document.clone(),
        from,
        bounds: bounds(),
        export: Some(binding()),
    };
    let new_run = || python(fresh(&document, Target::Main, Vec::new()).from);

    // A host that names Python runs it, to a park another worker resumes.
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut worker = checkout(&pool);
    worker
        .start(new_run(), &identity, ExecutionClass::Cell)
        .expect("start");
    let (_, _, admission) = admitted(worker.run(SLICE, false).expect("a Python run"));
    let state = export(&mut worker);
    worker.release().expect("release at the park");
    drop(pool);

    // A host that names TypeScript alone refuses both starts.
    HOST_DIALECTS.set("typescript");
    let pool = WorkerPool::new(config("")).expect("pool");
    let parked = python(StartFrom::ParkedGraph(ParkedGraph {
        state,
        admissions: vec![admission],
    }));
    for (start, from) in [(new_run(), "a new run"), (parked, "a parked run")] {
        let mut worker = checkout(&pool);
        let refused = worker
            .start(start, &identity, ExecutionClass::Cell)
            .and_then(|()| worker.run(SLICE, false))
            .expect_err("the worker installs no Python");
        assert!(
            matches!(
                &refused,
                PoolError::Infrastructure(InfrastructureOutcome::RunRefused {
                    refusal: RunRefusal::DialectNotInstalled { dialect }
                }) if dialect == "python"
            ),
            "{from}: {refused:?}"
        );
    }
}

/// FIG-5990: a Python cell is lowered and run by the worker of a host that
/// names Python, through the entry and the confinement every worker has:
/// lowering it, running it and parking at each effect makes no system call
/// the confinement forbids, and the run pins Python's helper release, on
/// which another worker resumes it.
#[test]
fn a_python_cell_lowered_in_the_worker_runs_under_its_confinement() {
    use lash_vm_client::service::{Request, Response};
    HOST_DIALECTS.set("typescript,python");
    let pool = WorkerPool::new(config("")).expect("pool");
    let echo = lash_kernel_doc::Signature {
        params: vec![lash_kernel_doc::Param {
            name: Name::new("x"),
            ty: lash_kernel_doc::Type::Any,
            optional: false,
        }],
        result: lash_kernel_doc::Type::Any,
    };
    let request = Request::Lower {
        dialect: "python".into(),
        source: "total = 0\nfor step in [1, 2, 3]:\n    total = total + await echo(step)\n".into(),
        effects: [(
            lash_kernel_doc::EffectName::new("echo").expect("effect name"),
            echo,
        )]
        .into(),
        controls: Default::default(),
        tool_roots: Default::default(),
        bindings: Default::default(),
        functions: Default::default(),
        helpers: lash_vm_library::HELPER_RELEASE,
        reads: Default::default(),
    };
    let mut worker = checkout(&pool);
    let response = worker
        .prepare(
            owner(),
            EncodedPayload(rmp_serde::to_vec_named(&request).expect("request")),
        )
        .expect("lowered in the worker");
    let Response::Lowered { document, .. } = rmp_serde::from_slice(&response.0).expect("response")
    else {
        panic!("the cell lowers")
    };
    let identity = lash_kernel_doc::Document::from_json(
        std::str::from_utf8(&document).expect("document text"),
    )
    .expect("document")
    .identity()
    .expect("identity")
    .to_string();
    let document = wire::wrap(PayloadKind::Document, &document).expect("payload");
    let mut start = fresh(&document, Target::Main, Vec::new());
    start.dialect = "python".into();
    // The worker that lowered the cell starts its run.
    worker
        .start(start, &identity, ExecutionClass::Cell)
        .expect("start");
    // Its first park is exported and the worker killed: another resumes it
    // only on the release it pinned, which holds Python's helpers and no
    // other dialect's.
    let (wait, argument, admission) = admitted(
        worker
            .run(SLICE, false)
            .expect("no system call the confinement forbids"),
    );
    let state = export(&mut worker);
    drop(worker);
    let mut worker = checkout(&pool);
    worker
        .start(
            Start {
                owner: owner(),
                dialect: "python".into(),
                document: document.clone(),
                from: StartFrom::ParkedGraph(ParkedGraph {
                    state,
                    admissions: vec![admission],
                }),
                bounds: bounds(),
                export: Some(binding()),
            },
            &identity,
            ExecutionClass::Cell,
        )
        .expect("resumed on Python's release");
    worker.deliver(wait, completed(argument)).expect("deliver");
    let mut asked = 1;
    let finished = loop {
        let step = worker
            .run(SLICE, false)
            .expect("no system call the confinement forbids");
        if !matches!(step, RunStep::Parked { .. }) {
            break ended(step);
        }
        let (wait, argument) = requested(step);
        asked += 1;
        worker.deliver(wait, completed(argument)).expect("deliver");
    };
    let End::Finished(finished) = finished else {
        panic!("the cell finishes: {finished:?}")
    };
    assert_eq!(asked, 3, "each call is asked once");
    assert_eq!(
        format!("{:?}", finished.bindings.variables.get(&Name::new("total"))),
        format!("{:?}", Some(lash_kernel_doc::Value::Int(Integer::from(6)))),
    );
    worker
        .release()
        .expect("the worker resets at the run's end");
}

/// A parked run names its document; a worker handed another document
/// refuses the state before any machine is built.
fn a_parked_run_is_refused_under_another_document() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let (identity, document_a) = document(TOOL_LOOP);
    let (other, document_b) = document("main { let got = perform echo(1) as Any return got }");
    let mut worker = checkout(&pool);
    worker
        .start(
            fresh(&document_a, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    let (_, _, admission) = admitted(worker.run(SLICE, false).expect("run"));
    let state = export(&mut worker);
    worker.release().expect("release at the park");

    let mut next = checkout(&pool);
    let refused = next
        .start(
            Start {
                owner: owner(),
                dialect: DIALECT.into(),
                document: document_b,
                from: StartFrom::ParkedGraph(ParkedGraph {
                    state,
                    admissions: vec![admission],
                }),
                bounds: bounds(),
                export: Some(binding()),
            },
            &other,
            ExecutionClass::Cell,
        )
        .expect_err("another document's state");
    assert!(
        matches!(
            refused,
            PoolError::Infrastructure(InfrastructureOutcome::RunRefused {
                refusal: RunRefusal::GraphState {
                    refusal: GraphStateRefusal::WrongDocument { .. },
                }
            })
        ),
        "{refused:?}"
    );
}

/// Target 1: the machine's clock, random source and projection reads are
/// answered by the parent in the middle of a slice, and what the run prints
/// arrives with the slice, in order.
fn host_reads_are_answered_by_the_parent_and_prints_arrive_in_order() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let (identity, document) = document(
        r#"
entry go(h: Handle("table")) -> Any
fn go(h) {
  print "before"
  let row = read(h, "row 7")
  print row
  return (clock, random, row)
}
main { }"#,
    );
    let handle = Handle {
        kind: "table".into(),
        id: "t1".into(),
    };
    let mut worker = checkout(&pool);
    worker
        .start(
            fresh(
                &document,
                Target::Entry(Name::new("go")),
                vec![Datum::Handle(handle.clone())],
            ),
            &identity,
            ExecutionClass::Process,
        )
        .expect("start");
    let now = Timestamp {
        nanoseconds: Integer::from(1_700_000_000),
    };
    let mut kinds = Vec::new();
    let mut step = worker.run(SLICE, false).expect("run");
    let end = loop {
        match step {
            RunStep::HostRead { id, kind, request } => {
                kinds.push(kind);
                let answer = match kind {
                    HostReadKind::Projection => {
                        let read: ProjectionRead =
                            wire::decode(PayloadKind::HostRead, &request).expect("read");
                        assert_eq!(read.handle, handle);
                        assert_eq!(read.request, Datum::Text("row 7".into()));
                        let answer: wire::ProjectionAnswer = Ok(Datum::Text("seven".into()));
                        wire::encode(PayloadKind::HostAnswer, &answer)
                    }
                    HostReadKind::Clock => wire::encode(PayloadKind::HostAnswer, &now),
                    // The top 53 bits, times 2^-53: one half.
                    HostReadKind::Random => wire::encode(PayloadKind::HostAnswer, &(1_u64 << 63)),
                    HostReadKind::Effect => panic!("the run binds no read"),
                }
                .expect("answer");
                step = worker.host_answer(id, answer).expect("answered");
            }
            other => break ended(other),
        }
    };
    assert_eq!(
        kinds,
        [
            HostReadKind::Projection,
            HostReadKind::Clock,
            HostReadKind::Random
        ]
    );
    let printed: Vec<Datum> = worker
        .take_printed()
        .iter()
        .flat_map(|payload| {
            rmp_serde::from_slice::<Vec<serde_bytes::ByteBuf>>(&payload.0).expect("chunk")
        })
        .map(|value| wire::from_json(PayloadKind::Printed, &value).expect("value"))
        .collect();
    assert_eq!(
        printed,
        [Datum::Text("before".into()), Datum::Text("seven".into())]
    );
    let End::Finished(finished) = end else {
        panic!("the run finishes: {end:?}")
    };
    assert_eq!(
        finished.result,
        Datum::Tuple(vec![
            Datum::Timestamp(now),
            Datum::Float(Float::new(0.5)),
            Datum::Text("seven".into())
        ])
    );
    worker.release().expect("reset");
}

/// Target 2: a slice returns to the parent with the run still ready, and a
/// run cancel sent with the next slice ends the run cancelled. The worker
/// is whole and resets.
fn a_run_cancel_is_observed_where_a_slice_returns() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let (identity, document) = document("main { while true { } }");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    worker
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    assert!(matches!(
        worker.run(1_000, false).expect("run"),
        RunStep::Slice { .. }
    ));
    assert_eq!(ended(worker.run(1_000, true).expect("run")), End::Cancelled);
    worker.release().expect("reset");
    assert_eq!(checkout(&pool).pid(), Some(pid), "the worker is reused");
}

/// A run that passes a bound ends with that bound's typed error. It is the
/// run's own end, not a worker failure: the worker resets and is reused.
fn a_passed_bound_is_the_runs_typed_end_and_the_worker_is_reused() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let (identity, document) = document("main { while true { } }");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    let mut start = fresh(&document, Target::Main, Vec::new());
    start.bounds.charge = 500;
    worker
        .start(start, &identity, ExecutionClass::Cell)
        .expect("start");
    let End::Error(RunError::Bound(exceeded)) = ended(worker.run(SLICE, false).expect("run"))
    else {
        panic!("the charge bound ends the run")
    };
    assert_eq!(exceeded.bound, Bound::Charge);
    assert_eq!(exceeded.limit, 500);
    worker.release().expect("reset");
    assert_eq!(checkout(&pool).pid(), Some(pid));
}

/// A worker that dies in the middle of a run fails the exchange typed, is
/// reaped, and leaves the parent with a pool that hands out a replacement.
fn worker_crash_mid_run_leaves_the_parent_running() {
    let pool = WorkerPool::new(config("abort")).expect("pool");
    let (identity, document) = document(TOOL_LOOP);
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    worker
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    let (wait, argument) = requested(worker.run(SLICE, false).expect("run"));
    worker
        .deliver(wait, completed(argument))
        .expect("the outcome waits for the next slice");
    assert!(matches!(
        worker.run(SLICE, false),
        Err(PoolError::Infrastructure(
            InfrastructureOutcome::WorkerCrashed { .. }
        ))
    ));
    let next = checkout(&pool);
    assert_ne!(next.pid(), Some(pid));
    next.release().expect("pristine replacement");
}

/// FIG-5858: a worker serves under a system-call allowlist. A call outside
/// it, such as opening a socket or mapping memory writable and executable
/// at once, kills the worker; the parent reads that as a typed crash and
/// the pool hands out a replacement.
fn a_forbidden_syscall_kills_the_worker_with_typed_evidence() {
    let (identity, document) = document(TOOL_LOOP);
    for mode in ["open_socket", "map_write_exec"] {
        let pool = WorkerPool::new(config(mode)).expect("pool");
        let mut worker = checkout(&pool);
        let pid = worker.pid().expect("pid");
        worker
            .start(
                fresh(&document, Target::Main, Vec::new()),
                &identity,
                ExecutionClass::Cell,
            )
            .expect("the parent takes the run");
        let started = worker.run(SLICE, false);
        assert!(
            matches!(
                started,
                Err(PoolError::Infrastructure(
                    InfrastructureOutcome::WorkerCrashed {
                        evidence: SupervisorEvidence::ForbiddenSyscall
                    }
                ))
            ),
            "{mode}: {started:?}"
        );
        drop(worker);
        let next = checkout(&pool);
        assert_ne!(next.pid(), Some(pid), "{mode}");
        next.release().expect("pristine replacement");
    }
}

/// FIG-5876: a worker that panics with RUST_BACKTRACE=1 reports the typed
/// panic. Printing the backtrace asks for the working directory, which the
/// confinement refuses rather than kills on, so the panic still reaches the
/// parent.
fn a_panic_with_a_backtrace_requested_is_typed_not_a_forbidden_syscall() {
    let pool = WorkerPool::new(config("panic_backtrace")).expect("pool");
    let (identity, document) = document(TOOL_LOOP);
    let mut worker = checkout(&pool);
    worker
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("the parent takes the run");
    let started = worker.run(SLICE, false);
    let Err(PoolError::Infrastructure(InfrastructureOutcome::ProtocolViolation {
        breach: ProtocolBreach::Panicked { detail },
    })) = started
    else {
        panic!("a typed panic: {started:?}")
    };
    assert_eq!(detail, Detail::new("a panic with a backtrace requested"));
}

/// FIG-5858: a worker's address space has a ceiling, which bounds what a
/// run can allocate even where the run's memory bound would allow more.
/// The allocation is refused as the run's memory bound, and the worker,
/// which never held the memory, is reused.
fn the_address_space_ceiling_bounds_a_runs_allocation() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let mut config = config("");
    config.confinement.address_space_bytes = GIB;
    // Room for the parser stack of the largest admitted source.
    config.protocol.max_source_bytes = 1024;
    config.tuning.typescript_source_bytes = 1024;
    config.run_bounds.memory = 8 * GIB;
    config.run_bounds.charge = u64::MAX;
    let pool = WorkerPool::new(config).expect("pool");
    let (identity, document) = document(
        r#"
main {
  let big = text.repeat("x", 1500000000)
  return 0
}"#,
    );
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    let mut start = fresh(&document, Target::Main, Vec::new());
    start.bounds.memory = 8 * GIB;
    start.bounds.charge = u64::MAX;
    worker
        .start(start, &identity, ExecutionClass::Cell)
        .expect("start");
    let End::Error(RunError::Bound(exceeded)) = ended(worker.run(SLICE, false).expect("run"))
    else {
        panic!("the ceiling refuses the text")
    };
    assert_eq!(exceeded.bound, Bound::Memory);
    worker.release().expect("reset");
    assert_eq!(checkout(&pool).pid(), Some(pid), "the worker is reused");
}

/// FIG-5876: a heap-profiled worker serves under the same confinement as
/// any other, and still writes its DHAT profile and receipt when its first
/// clean reset ends the window: the flush acknowledges the reset instead of
/// killing the worker, which then serves on.
#[cfg(feature = "dhat-heap")]
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the law hands the worker a fresh profile directory and reads what it wrote"
)]
fn a_heap_profiled_worker_writes_its_profile_under_confinement_across_a_reset() {
    let directory = std::env::temp_dir().join(format!("lash-heap-profile-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    let mut entry = WorkerEntry::helper(env!("CARGO_BIN_EXE_lash-vm-worker"));
    entry.args = vec![
        "--heap-profile-dir".into(),
        directory.to_str().expect("utf-8 directory").into(),
    ];
    let mut config = PoolConfig::standard(entry);
    config.max_workers = 1;
    let pool = WorkerPool::new(config).expect("pool");
    let (identity, document) = document("main { return num.add(1, 2) }");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    worker
        .start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        )
        .expect("start");
    let End::Finished(finished) = ended(worker.run(SLICE, false).expect("run")) else {
        panic!("the run finishes")
    };
    assert_eq!(finished.result, int(3));
    worker
        .release()
        .expect("the reset flushes the profile and is acknowledged");
    let profile = directory.join(format!("vm-worker-{pid}.dhat.json"));
    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&profile).expect("the profile")).expect("json");
    assert_eq!(written["mode"], "rust-heap", "{profile:?}");
    assert!(
        written["pps"]
            .as_array()
            .is_some_and(|sites| !sites.is_empty())
    );
    let receipt: serde_json::Value = serde_json::from_slice(
        &std::fs::read(profile.with_extension("receipt.json")).expect("the receipt"),
    )
    .expect("json");
    assert_eq!(receipt["completed"], true);
    let next = checkout(&pool);
    assert_eq!(next.pid(), Some(pid), "the profiled worker serves on");
    next.release().expect("a second reset after the window");
    let _ = std::fs::remove_dir_all(&directory);
}

/// FIG-6007: silence during the Ready handshake reports the actual wait,
/// just as silence during guest execution does. It is not an instant exit.
fn a_worker_silent_during_bootstrap_reports_the_time_it_was_silent() {
    let mut config = config("bootstrap_wait");
    config.protocol.no_response_watchdog = std::time::Duration::from_millis(20);
    let error = WorkerPool::new(config).err().expect("the watchdog fires");
    let PoolError::Infrastructure(InfrastructureOutcome::WorkerUnresponsive { silent_ms }) = error
    else {
        panic!("a typed silence verdict: {error:?}")
    };
    assert!(
        silent_ms >= 20,
        "the handshake waited, but reports {silent_ms} ms"
    );
}

/// The kernel worker's live protocol is admitted before any guest request.
/// Synthetic-next keeps the same refusal as the plain parent.
fn workers_refuse_another_protocol_before_guest_admission() {
    for mode in ["protocol_below", "protocol_above", "opposite_generation"] {
        let error = WorkerPool::new(config(mode))
            .err()
            .expect("incompatible worker");
        let PoolError::ProtocolVersion(refusal) = error else {
            panic!("typed protocol refusal: {error:?}");
        };
        assert_eq!(refusal.parent_version, WORKER_PROTOCOL_VERSION);
        assert_eq!(
            refusal.minimum_supported_version,
            MIN_SUPPORTED_WORKER_PROTOCOL_VERSION
        );
        assert!(
            refusal.worker_version < MIN_SUPPORTED_WORKER_PROTOCOL_VERSION
                || refusal.worker_version > WORKER_PROTOCOL_VERSION
        );
    }
}

/// FIG-6008: allocation tracing can put Ready beyond a serving worker's
/// silence window. Only profiled builds admit that longer bootstrap; the
/// configured live silence window remains the parent's working policy.
#[test]
fn bootstrap_uses_the_builds_allowance_without_extending_live_silence() {
    HOST_DIALECTS.set("typescript,python");
    let mut config = config("bootstrap_delayed");
    let silence = std::time::Duration::from_secs(1);
    config.protocol.no_response_watchdog = silence;
    let result = WorkerPool::new(config);
    #[cfg(feature = "dhat-heap")]
    {
        let pool = result.expect("Ready fits the profiled build's startup allowance");
        assert_eq!(pool.config().protocol.no_response_watchdog, silence);
    }
    #[cfg(not(feature = "dhat-heap"))]
    {
        let error = result
            .err()
            .expect("default builds keep one silence window");
        let PoolError::Infrastructure(InfrastructureOutcome::WorkerUnresponsive { silent_ms }) =
            error
        else {
            panic!("a typed startup silence verdict: {error:?}");
        };
        assert!(silent_ms >= 1_000, "the default startup watchdog waited");
    }
}
