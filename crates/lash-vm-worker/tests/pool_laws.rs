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

fn config(mode: &str) -> PoolConfig {
    let mut entry = WorkerEntry::helper(env!("CARGO_BIN_EXE_lash-vm-worker-fixture"));
    if !mode.is_empty() {
        entry.args.push(mode.into());
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
    let embedding =
        lash_vm_worker::standard(&WorkerTuning::standard()).expect("the standard embedding");
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
        document: document.clone(),
        from: StartFrom::Fresh(
            wire::encode(
                PayloadKind::Start,
                &StartWire::from(RunStart {
                    target,
                    args,
                    bindings: Bindings::default(),
                }),
            )
            .expect("start"),
        ),
        bounds: bounds(),
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
    end.map_or(End::Cancelled, wire::RecordedEnd::into_end)
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
    assert!(!first.deliver(wait, completed(argument)).expect("deliver"));
    let (wait, argument) = requested(first.run(SLICE, false).expect("run"));
    asked.push(argument.clone());
    let state = first.export().expect("export");
    assert_eq!(state.document(), identity);
    // Killed at the park: the worker is discarded, not reset.
    drop(first);

    let mut second = checkout(pool);
    assert_ne!(second.pid(), Some(pid));
    second
        .start(
            Start {
                owner: owner(),
                document: document.clone(),
                from: StartFrom::Parked(state),
                bounds: bounds(),
            },
            identity,
            ExecutionClass::Cell,
        )
        .expect("resume");
    assert!(!second.deliver(wait, completed(argument)).expect("deliver"));
    let (wait, argument) = requested(second.run(SLICE, false).expect("run"));
    asked.push(argument.clone());
    assert!(!second.deliver(wait, completed(argument)).expect("deliver"));
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
#[test]
fn a_parked_run_carries_on_in_another_worker_without_asking_again() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let (identity, document) = document(TOOL_LOOP);
    let (finished, asked) = killed_at_a_park_and_resumed(&pool, &identity, &document);
    assert_eq!(finished.result, int(6));
    assert_eq!(asked, [int(1), int(2), int(3)], "each effect is asked once");
}

/// Targets 1 and 5: a TypeScript cell is lowered in the worker against the
/// functions it registered at startup, and the document it gives runs,
/// parks, is killed and resumes like any other. The cell's top-level
/// binding is the session's when the run finishes.
#[test]
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

/// A parked run names its document; a worker handed another document
/// refuses the state before any machine is built.
#[test]
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
    requested(worker.run(SLICE, false).expect("run"));
    let state = worker.export().expect("export");
    worker.release().expect("release at the park");

    let mut next = checkout(&pool);
    let refused = next
        .start(
            Start {
                owner: owner(),
                document: document_b,
                from: StartFrom::Parked(state),
                bounds: bounds(),
            },
            &other,
            ExecutionClass::Cell,
        )
        .expect_err("another document's state");
    assert!(
        matches!(
            refused,
            PoolError::Infrastructure(InfrastructureOutcome::RunRefused {
                refusal: RunRefusal::State {
                    refusal: OpaqueStateRefusal::WrongDocument { .. },
                }
            })
        ),
        "{refused:?}"
    );
}

/// Target 1: the machine's clock, random source and projection reads are
/// answered by the parent in the middle of a slice, and what the run prints
/// arrives with the slice, in order.
#[test]
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
#[test]
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
#[test]
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
#[test]
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
    assert!(matches!(
        worker.deliver(wait, completed(argument)),
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
#[test]
fn a_forbidden_syscall_kills_the_worker_with_typed_evidence() {
    let (identity, document) = document(TOOL_LOOP);
    for mode in ["open_socket", "map_write_exec"] {
        let pool = WorkerPool::new(config(mode)).expect("pool");
        let mut worker = checkout(&pool);
        let pid = worker.pid().expect("pid");
        let started = worker.start(
            fresh(&document, Target::Main, Vec::new()),
            &identity,
            ExecutionClass::Cell,
        );
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
#[test]
fn a_panic_with_a_backtrace_requested_is_typed_not_a_forbidden_syscall() {
    let pool = WorkerPool::new(config("panic_backtrace")).expect("pool");
    let (identity, document) = document(TOOL_LOOP);
    let mut worker = checkout(&pool);
    let started = worker.start(
        fresh(&document, Target::Main, Vec::new()),
        &identity,
        ExecutionClass::Cell,
    );
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
#[test]
fn the_address_space_ceiling_bounds_a_runs_allocation() {
    const GIB: u64 = 1024 * 1024 * 1024;
    let mut config = config("");
    config.confinement.address_space_bytes = GIB;
    // Room for the parser stack of the largest admitted source.
    config.protocol.max_source_bytes = 1024;
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

/// The kernel worker's live protocol is admitted before any guest request.
/// Synthetic-next keeps the same refusal as the plain parent.
#[test]
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
