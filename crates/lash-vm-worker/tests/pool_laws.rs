#![cfg(unix)]
#![expect(
    clippy::expect_used,
    reason = "integration test helpers fail on broken fixture assumptions"
)]

use lash_vm_client::*;
use lash_vm_protocol::*;
use lashlang::testing::ast_builders as b;
use lashlang::{
    AbilityOp, AbilityOutcome, ExecutionMode, ResourceOperationBatchLeaf, ResourceOperationOutcome,
};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc;
use std::time::{Duration, Instant};

#[cfg(target_os = "linux")]
#[path = "pool_laws/native_oom.rs"]
mod native_oom;

/// Counts the bytes each thread allocates, for the laws that bound what the
/// parent copies (FIG-4433).
struct CountingAllocator;

thread_local! {
    static ALLOCATED_BYTES: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[expect(
    unsafe_code,
    reason = "parent copies are measured with a counting global allocator, and GlobalAlloc is an unsafe trait"
)]
unsafe impl std::alloc::GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // The key is const-initialised and holds a Copy type, so charging it
        // allocates nothing and cannot re-enter the allocator.
        let _ = ALLOCATED_BYTES
            .try_with(|bytes| bytes.set(bytes.get().saturating_add(layout.size() as u64)));
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        unsafe { std::alloc::System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn config(mode: &str) -> PoolConfig {
    let mut entry = WorkerEntry::helper(env!("CARGO_BIN_EXE_lash-vm-worker-fixture"));
    if !mode.is_empty() {
        entry.args.push(mode.into());
    }
    // Worker startup and replacement are setup for these laws. The short
    // checkout deadline belongs only to laws that assert CheckoutTimedOut.
    let mut config = PoolConfig::standard(entry);
    config.max_workers = 1;
    config.protocol.no_response_watchdog = Duration::from_secs(2);
    config
}
fn checkout_timeout_config() -> PoolConfig {
    let mut config = config("");
    config.deadlines.checkout = Duration::from_millis(150);
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
fn start(source: &str, mode: ExecutionMode) -> Start {
    Start {
        owner: VmOwner::new("session-A"),
        program: ProgramSource::Source {
            dialect: "typescript".into(),
            text: source.into(),
        },
        contexts: vec![ContextDescription {
            kind: "vm_run".into(),
            name: "context".into(),
            body: EncodedPayload(
                rmp_serde::to_vec_named(&RunContext {
                    environment: lashlang::testing::harness::test_environment(),
                    mode,
                    ..RunContext::default()
                })
                .expect("context"),
            ),
        }],
        state: StartState::Fresh,
        limits: PoolConfig::standard(WorkerEntry::helper("unused")).vm_limits,
    }
}
fn answer(request: EffectRequest) -> EffectResponse {
    let outcome = match request.kind {
        EffectKind::CancelCheckpoint => EffectOutcome::Checkpoint { cancelled: false },
        EffectKind::ProcessBoundary | EffectKind::ParkDeclined => EffectOutcome::Unit,
        _ => {
            let op: AbilityOp = rmp_serde::from_slice(&request.payload.0).expect("operation");
            let result = match op {
                AbilityOp::ResourceOperation(op) => {
                    lashlang::testing::harness::EchoHost::perform_resource_operation(*op)
                        .map(AbilityOutcome::Value)
                }
                AbilityOp::ResourceOperationBatch(batch) => {
                    let results = batch
                        .leaves
                        .iter()
                        .map(|leaf| match leaf {
                            ResourceOperationBatchLeaf::Operation(operation) => {
                                ResourceOperationOutcome::from_result(
                                    lashlang::testing::harness::EchoHost::perform_resource_operation(
                                        operation.clone(),
                                    ),
                                )
                            }
                            ResourceOperationBatchLeaf::Timer(_) => {
                                ResourceOperationOutcome::Value(lashlang::Value::Undefined)
                            }
                        })
                        .collect();
                    Ok(AbilityOutcome::ResourceOperationBatch(
                        batch.answer_in_leaf_order(results),
                    ))
                }
                // The awaited process's terminal: a value that names the
                // handle, so every leaf of an aggregate is told apart.
                AbilityOp::Await(handle) => Ok(AbilityOutcome::Value(lashlang::Value::String(
                    format!("settled {handle:?}").into(),
                ))),
                AbilityOp::Finish(v) | AbilityOp::Fail(v) => Ok(AbilityOutcome::Value(v)),
                AbilityOp::Print(_) => Ok(AbilityOutcome::Unit),
                _ => panic!("unexpected effect"),
            };
            match result {
                Ok(result) => EffectOutcome::Value(EncodedPayload(
                    rmp_serde::to_vec_named(&result).expect("answer"),
                )),
                Err(error) => EffectOutcome::Failed(EncodedPayload(
                    rmp_serde::to_vec_named(&error).expect("error"),
                )),
            }
        }
    };
    EffectResponse {
        id: request.id,
        outcome,
    }
}
fn parked(outcome: ParkOutcome) -> OpaqueVmState {
    match outcome {
        ParkOutcome::Parked(state) => state,
        ParkOutcome::Declined(request) => panic!("the run declined its park: {request:?}"),
    }
}
fn drive(worker: &mut Checkout, mut message: WorkerMessage) -> WorkerMessage {
    loop {
        match message {
            WorkerMessage::EffectRequest(request) => {
                message = worker.effect_result(answer(request)).expect("resume")
            }
            _ => return message,
        }
    }
}
fn complete(worker: &mut Checkout, source: &str) -> (OpaqueVmState, Vec<u8>) {
    let message = worker
        .start(start(source, ExecutionMode::Foreground))
        .expect("start");
    match drive(worker, message) {
        WorkerMessage::Complete { state, value } => (state, value.0),
        other => panic!("expected Complete, received {other:?}"),
    }
}
fn assert_reaped(pid: u32) {
    // SAFETY: observing wait status cannot affect a live unrelated process;
    // the supervisor has already killed and reaped this exact child.
    #[expect(
        unsafe_code,
        reason = "the crash law proves the worker is reaped, not merely signalled"
    )]
    let result = unsafe { libc::waitpid(pid as i32, std::ptr::null_mut(), libc::WNOHANG) };
    assert_eq!(result, -1);
    assert_eq!(
        std::io::Error::last_os_error().raw_os_error(),
        Some(libc::ECHILD)
    );
}
fn wait_queued(pool: &WorkerPool, count: usize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while pool.stats().queued_items != count {
        assert!(Instant::now() < deadline, "waiter did not enter queue");
        std::thread::yield_now();
    }
}

#[test]
fn worker_crash_mid_cell_leaves_the_parent_running() {
    let pool = WorkerPool::new(config("abort")).expect("pool");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    let message = worker
        .start(start(
            "finish(await tools.echo({ value: 7 }));",
            ExecutionMode::Foreground,
        ))
        .expect("start");
    let WorkerMessage::EffectRequest(request) = message else {
        panic!("effect required");
    };
    assert!(matches!(
        worker.effect_result(answer(request)),
        Err(PoolError::Infrastructure(
            InfrastructureOutcome::WorkerCrashed { .. }
        ))
    ));
    assert_reaped(pid);
    let next = checkout(&pool);
    assert_ne!(next.pid(), Some(pid));
    next.release().expect("pristine replacement");
}

#[test]
fn worker_limit_exhaustion_discards_not_resets() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    let mut input = start(
        "let n = 0; while (true) { n++; }",
        ExecutionMode::Foreground,
    );
    input.limits.instruction_budget = Some(8);
    assert_eq!(
        worker.start(input),
        Err(PoolError::Infrastructure(
            InfrastructureOutcome::WorkerLimitExceeded {
                limit: WorkerLimit::Fuel
            }
        ))
    );
    assert_reaped(pid);
    let mut next = checkout(&pool);
    assert_ne!(next.pid(), Some(pid));
    complete(&mut next, "finish(42);");
    next.release().expect("reset");
}

#[test]
fn unresponsive_worker_is_killed_reaped_and_replaced() {
    let mut cfg = config("hang");
    cfg.protocol.no_response_watchdog = Duration::from_millis(500);
    let pool = WorkerPool::new(cfg).expect("pool");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    let error = worker
        .start(start("1 + 1;", ExecutionMode::Foreground))
        .expect_err("hang refused");
    assert!(matches!(
        error,
        PoolError::Infrastructure(InfrastructureOutcome::WorkerUnresponsive { .. })
    ));
    assert_reaped(pid);
    assert_ne!(checkout(&pool).pid(), Some(pid));
}

#[test]
fn pool_queue_bound_refuses_typed() {
    let mut cfg = config("");
    cfg.max_queue_items = 1;
    cfg.max_queue_bytes = 100;
    cfg.deadlines.checkout = Duration::from_secs(3);
    let pool = WorkerPool::new(cfg).expect("pool");
    let held = checkout(&pool);
    let other = pool.clone();
    let thread = std::thread::spawn(move || {
        other.checkout(80, OwnerEpoch(1), FrameEpoch(1), ExecutionBudget::default())
    });
    wait_queued(&pool, 1);
    assert!(matches!(
        pool.checkout(1, OwnerEpoch(1), FrameEpoch(1), ExecutionBudget::default()),
        Err(PoolError::QueueFull { .. })
    ));
    held.release().expect("return");
    thread
        .join()
        .expect("waiter")
        .expect("admitted")
        .release()
        .expect("return");
    // Independently pin the byte bound while the item bound has room.
    let mut cfg = config("");
    cfg.max_queue_items = 2;
    cfg.max_queue_bytes = 50;
    let pool = WorkerPool::new(cfg).expect("pool");
    let held = checkout(&pool);
    assert!(matches!(
        pool.checkout(51, OwnerEpoch(1), FrameEpoch(1), ExecutionBudget::default()),
        Err(PoolError::QueueFull { .. })
    ));
    held.release().expect("return");
}

#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "isolate the sentinel environment in a child test process and open an inheritable descriptor"
)]
fn worker_sees_no_parent_environment_or_descriptors() {
    const SENTINEL: &str = "LASH_PARENT_SECRET_SENTINEL";
    if std::env::var_os(SENTINEL).is_none() {
        let output = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "worker_sees_no_parent_environment_or_descriptors",
                "--nocapture",
            ])
            .env(SENTINEL, "parent-only-secret")
            .output()
            .expect("sentinel parent");
        assert!(
            output.status.success(),
            "sentinel child failed: {}\nstdout:\n{}\nstderr:\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        return;
    }
    let file = std::fs::File::open("/dev/null").expect("sentinel file");
    // SAFETY: duplicate the owned file as a deliberately inheritable sentinel.
    #[expect(unsafe_code, reason = "the law needs a non-CLOEXEC parent descriptor")]
    let fd = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD, 100) };
    assert!(fd >= 100);
    #[expect(
        unsafe_code,
        reason = "successful F_DUPFD transfers one valid owned descriptor"
    )]
    let sentinel = unsafe { OwnedFd::from_raw_fd(fd) };
    let mut cfg = config("probe");
    cfg.entry.args.push(fd.to_string());
    let pool = WorkerPool::new(cfg).expect("pool");
    let mut worker = checkout(&pool);
    complete(&mut worker, "finish(1);");
    worker.release().expect("reset");
    drop(sentinel);
}

#[test]
fn pooled_worker_reset_leaks_nothing_across_sessions() {
    let plant = r#"
const planted = "SENTINEL-A-4158";
const plantedClosure = () => planted + "-closure";
const plantedPattern = /SENTINEL-A-4158/g;
const plantedMatched = plantedPattern.test("xx SENTINEL-A-4158");
const plantedError = new Error(planted);
const plantedRecord = { secret: planted, nested: [planted, plantedClosure()] };
const plantedEcho = await tools.echo({ value: planted });
finish(plantedClosure());
"#;
    let probes = [
        "finish(typeof planted);",
        "finish(typeof plantedClosure);",
        "finish(typeof plantedError);",
        "finish(typeof plantedRecord);",
        "finish(typeof plantedEcho);",
        "finish(typeof plantedPattern);",
        "const probe = /SENTINEL-A-4158/g; finish([probe.lastIndex, probe.test('SENTINEL-A-4158'), probe.lastIndex]);",
        "finish(String(new Error('probe')));",
        "finish(await tools.echo({ value: 'probe' }));",
        plant,
    ];
    for probe in probes {
        let pool = WorkerPool::new(config("")).expect("pool");
        let mut worker = checkout(&pool);
        let pid = worker.pid();
        let message = worker
        .start(start(
            "const parked = await tools.echo({ value: 'SENTINEL-A-4158-parked' }); finish(parked);",
            ExecutionMode::Process,
        ))
        .expect("parked sentinel start");
        let WorkerMessage::EffectRequest(request) = message else {
            panic!("effect");
        };
        worker
            .effect_result(answer(request))
            .expect("sentinel boundary");
        let parked = parked(worker.park().expect("sentinel continuation"));
        assert_eq!(parked.kind(), VmStateKind::Continuation);
        worker.release().expect("reset parked sentinel");
        let mut worker = checkout(&pool);
        assert_eq!(worker.pid(), pid);
        let (state, outcome) = complete(&mut worker, plant);
        assert!(String::from_utf8_lossy(&outcome).contains("SENTINEL"));
        assert!(state.len() > 100);
        worker.release().expect("reset");
        // Plant a pending handle and continuation scratch, then cleanly abandon
        // after the broker has accounted for the operation.
        let mut worker = checkout(&pool);
        assert_eq!(worker.pid(), pid);
        let message = worker.start(start("const pending = await tools.echo({ value: 'SENTINEL-A-4158-in-flight' }); finish(pending);", ExecutionMode::Foreground)).expect("pending");
        assert!(matches!(message, WorkerMessage::EffectRequest(_)));
        worker.release().expect("reset pending run");

        let mut input = start(probe, ExecutionMode::Foreground);
        input.owner = VmOwner::new("session-B");
        let mut context: RunContext =
            rmp_serde::from_slice(&input.contexts[0].body.0).expect("context");
        context.environment = context.environment.with_globals([
            "planted",
            "plantedClosure",
            "plantedError",
            "plantedRecord",
            "plantedEcho",
            "plantedPattern",
        ]);
        input.contexts[0].body.0 = rmp_serde::to_vec_named(&context).expect("context");
        let mut reused = checkout(&pool);
        assert_eq!(reused.pid(), pid);
        let message = reused.start(input.clone()).expect("B start");
        let after_reset = drive(&mut reused, message);
        let fresh_pool = WorkerPool::new(config("")).expect("fresh pool");
        let mut fresh = checkout(&fresh_pool);
        let message = fresh.start(input).expect("fresh start");
        let on_fresh = drive(&mut fresh, message);
        assert_eq!(after_reset, on_fresh, "probe {probe}");
        if matches!(after_reset, WorkerMessage::GuestError { .. }) {
            assert_reaped(pid.expect("reused pid"));
            // Guest errors discard by contract; each probe plants A anew.
        } else {
            reused.release().expect("reset probe");
            fresh.release().expect("fresh reset");
        }
    }
}

#[test]
fn restart_storm_fails_queued_work_typed() {
    let mut cfg = config("abort");
    cfg.max_restarts = 1;
    cfg.deadlines.checkout = Duration::from_secs(3);
    let pool = WorkerPool::new(cfg).expect("pool");
    let mut worker = checkout(&pool);
    let message = worker
        .start(start(
            "finish(await tools.echo({ value: 7 }));",
            ExecutionMode::Foreground,
        ))
        .expect("start");
    let other = pool.clone();
    let (tx, rx) = mpsc::channel();
    let thread = std::thread::spawn(move || {
        tx.send(
            other
                .checkout(1, OwnerEpoch(1), FrameEpoch(1), ExecutionBudget::default())
                .err(),
        )
        .expect("send");
    });
    wait_queued(&pool, 1);
    let WorkerMessage::EffectRequest(request) = message else {
        panic!("request");
    };
    assert!(worker.effect_result(answer(request)).is_err());
    assert_eq!(
        rx.recv_timeout(Duration::from_secs(2))
            .expect("queued refusal"),
        Some(PoolError::RestartStorm)
    );
    thread.join().expect("thread");
    assert!(pool.stats().restart_storm);
}

/// A run awaiting an effect that needs a worker of its own (a nested
/// compilation) parks and gives its slot back, so a pool of one worker runs
/// the nested work and then resumes the run, which issues its request again
/// and completes as it would have straight through (FIG-4159).
#[test]
fn single_slot_nested_compile_completes_by_parking_the_awaiting_run() {
    let source = "finish(await tools.echo({ value: 7 }));";
    let pool = WorkerPool::new(checkout_timeout_config()).expect("pool");
    let mut straight = checkout(&pool);
    let (_, expected) = complete(&mut straight, source);
    straight.release().expect("release");

    let mut worker = checkout(&pool);
    let message = worker
        .start(start(source, ExecutionMode::Foreground))
        .expect("start");
    let WorkerMessage::EffectRequest(awaited) = message else {
        panic!("the run asks for its tool call, received {message:?}");
    };
    assert!(awaited.kind.parkable(), "{:?} is parkable", awaited.kind);
    assert!(
        matches!(
            pool.checkout(1, OwnerEpoch(1), FrameEpoch(1), ExecutionBudget::default()),
            Err(PoolError::CheckoutTimedOut)
        ),
        "the one slot is held while the run awaits its effect"
    );
    let ParkOutcome::Parked(parked) = worker.park().expect("park") else {
        panic!("the awaiting run parks");
    };
    worker.release().expect("the parked run's slot goes back");

    let mut nested = checkout(&pool);
    let (_, nested_value) = complete(&mut nested, "finish(6 * 7);");
    assert!(!nested_value.is_empty(), "the nested compilation completes");
    nested.release().expect("release");

    let mut resumed = checkout(&pool);
    let mut input = start(source, ExecutionMode::Foreground);
    input.state = StartState::Continuation(parked);
    let message = resumed.start(input).expect("resume");
    let WorkerMessage::EffectRequest(again) = message else {
        panic!("the resumed run asks again, received {message:?}");
    };
    assert_eq!(
        (again.kind, &again.payload),
        (awaited.kind, &awaited.payload),
        "the resumed run issues the request it parked on"
    );
    let message = resumed.effect_result(answer(again)).expect("answer");
    let WorkerMessage::Complete { value, .. } = drive(&mut resumed, message) else {
        panic!("the resumed run completes");
    };
    assert_eq!(
        value.0, expected,
        "the parked run ends as it would straight through"
    );
    resumed.release().expect("release");
}

#[test]
fn parked_projected_tool_arguments_keep_the_recorded_request() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut input = start(
        "finish(await tools.echo({ value: session_projection.length }));",
        ExecutionMode::Foreground,
    );
    input.contexts[0].body = EncodedPayload(
        rmp_serde::to_vec_named(&RunContext {
            environment: lashlang::testing::harness::test_environment()
                .with_globals(["session_projection".to_string()]),
            mode: ExecutionMode::Foreground,
            projected: vec![ProjectionDescription {
                name: "session_projection".into(),
                key: 0,
                type_name: "string".into(),
                scalar: Some(lashlang::Value::String("session:durable".into())),
            }],
            ..RunContext::default()
        })
        .expect("projected context"),
    );
    let mut worker = checkout(&pool);
    let WorkerMessage::EffectRequest(request) = worker.start(input.clone()).expect("start") else {
        panic!("the cell requests its projected tool call");
    };
    assert_eq!(request.kind, EffectKind::ResourceOperation);
    input.state = StartState::Continuation(parked(worker.park().expect("park")));
    worker.release().expect("release the one slot");
    let mut resumed = checkout(&pool);
    let WorkerMessage::EffectRequest(again) = resumed.start(input).expect("resume") else {
        panic!("the cell reissues its pending tool call");
    };
    assert_eq!(
        (again.kind, &again.payload),
        (request.kind, &request.payload),
        "parking must preserve the issued request, including a derived projected scalar"
    );
    let message = resumed.effect_result(answer(again)).expect("answer");
    let WorkerMessage::Complete { value, .. } = drive(&mut resumed, message) else {
        panic!("the resumed cell completes");
    };
    let outcome: lashlang::ExecutionOutcome = rmp_serde::from_slice(&value.0).expect("outcome");
    let lashlang::ExecutionOutcome::Finished(lashlang::Value::Projected(value)) = outcome else {
        panic!("the terminal value keeps its projected provenance: {outcome:?}");
    };
    assert_eq!(value.scalar_value(), Some(&lashlang::Value::Number(15.0)));
    resumed.release().expect("release");
}

#[test]
fn process_boundary_releases_and_resumes_on_one_worker() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut worker = checkout(&pool);
    let pid = worker.pid();
    let mut input = start(
        "const held = await tools.echo({ value: 'parked' }); finish(held);",
        ExecutionMode::Process,
    );
    let WorkerMessage::EffectRequest(request) = worker.start(input.clone()).expect("start") else {
        panic!("effect");
    };
    let message = worker.effect_result(answer(request)).expect("effect done");
    assert!(matches!(
        message,
        WorkerMessage::EffectRequest(EffectRequest {
            kind: EffectKind::ProcessBoundary,
            ..
        })
    ));
    let state = parked(worker.park().expect("park"));
    worker.release().expect("reset and release");
    let mut nested = checkout(&pool);
    assert_eq!(nested.pid(), pid);
    complete(&mut nested, "finish(1);");
    nested.release().expect("nested reset");
    input.state = StartState::Continuation(state);
    let mut resumed = checkout(&pool);
    let message = resumed.start(input).expect("resume parked");
    assert!(matches!(
        drive(&mut resumed, message),
        WorkerMessage::Complete { .. }
    ));
    resumed.release().expect("reset");
}

#[test]
fn replacement_preserves_cpu_and_retry_totals() {
    let pool = WorkerPool::new(config("abort")).expect("pool");
    let budget = ExecutionBudget::default();
    for attempt in 1..=3 {
        let mut worker = pool
            .checkout(
                4 * 1024 * 1024 + FRAME_HEADER_BYTES,
                OwnerEpoch(1),
                FrameEpoch(1),
                budget.clone(),
            )
            .expect("attempt");
        let WorkerMessage::EffectRequest(request) = worker
            .start(start(
                "finish(await tools.echo({ value: 7 }));",
                ExecutionMode::Foreground,
            ))
            .expect("start")
        else {
            panic!("request");
        };
        assert!(worker.effect_result(answer(request)).is_err());
        assert_eq!(budget.totals().0, attempt);
        assert!(!budget.totals().1.is_zero());
    }
    assert!(matches!(
        pool.checkout(1, OwnerEpoch(1), FrameEpoch(1), budget),
        Err(PoolError::RetryLimitExceeded)
    ));
    let exhausted = ExecutionBudget::restored(0, pool.config().deadlines.cumulative_cpu);
    assert!(matches!(
        pool.checkout(1, OwnerEpoch(1), FrameEpoch(1), exhausted),
        Err(PoolError::Infrastructure(
            InfrastructureOutcome::WorkerLimitExceeded {
                limit: WorkerLimit::Deadline
            }
        ))
    ));
}

#[test]
fn parent_effect_wait_pauses_deadlines() {
    let mut cfg = config("");
    cfg.protocol.no_response_watchdog = Duration::from_millis(500);
    let pool = WorkerPool::new(cfg).expect("pool");
    let mut worker = checkout(&pool);
    let WorkerMessage::EffectRequest(request) = worker
        .start(start(
            "finish(await tools.echo({ value: 7 }));",
            ExecutionMode::Foreground,
        ))
        .expect("start")
    else {
        panic!("request");
    };
    std::thread::sleep(Duration::from_millis(750));
    let message = worker
        .effect_result(answer(request))
        .expect("host wait excluded");
    assert!(matches!(
        drive(&mut worker, message),
        WorkerMessage::Complete { .. }
    ));
    worker.release().expect("reset");
}

#[test]
fn out_of_range_protocol_is_typed_and_refused_before_model_code() {
    for (mode, version) in [
        ("protocol_below", MIN_SUPPORTED_WORKER_PROTOCOL_VERSION - 1),
        ("protocol_above", WORKER_PROTOCOL_VERSION + 1),
    ] {
        let Err(PoolError::ProtocolVersion(refusal)) = WorkerPool::new(config(mode)) else {
            panic!("{mode}: expected a typed protocol refusal");
        };
        assert_eq!(refusal.parent_version, WORKER_PROTOCOL_VERSION);
        assert_eq!(
            refusal.minimum_supported_version,
            MIN_SUPPORTED_WORKER_PROTOCOL_VERSION
        );
        assert_eq!(refusal.worker_version, version);
        assert_eq!(refusal.parent_crate_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(refusal.worker_crate_version, "9.8.7-diagnostic-only");
    }
}

#[test]
fn synthetic_next_and_plain_workers_refuse_each_other_at_the_handshake() {
    let Err(PoolError::ProtocolVersion(refusal)) = WorkerPool::new(config("opposite_generation"))
    else {
        panic!("opposite generation must fail before model code");
    };
    assert_eq!(
        refusal.parent_version,
        1 + u32::from(cfg!(feature = "synthetic-next"))
    );
    assert_eq!(
        refusal.worker_version,
        if cfg!(feature = "synthetic-next") {
            1
        } else {
            2
        }
    );
}

#[test]
fn equal_protocol_accepts_a_different_crate_version() {
    let pool = WorkerPool::new(config("crate_version")).expect("crate version is diagnostic only");
    assert_eq!(pool.stats().workers, 1);
}

#[test]
fn helper_entry_preserves_effect_values_losslessly() {
    use lashlang::{ExecutionOutcome, Value};
    let mut cfg = config("");
    cfg.entry = WorkerEntry::helper(env!("CARGO_BIN_EXE_lash-vm-worker"));
    let pool = WorkerPool::new(cfg).expect("production helper");
    let mut worker = checkout(&pool);
    let (_, outcome) = complete(&mut worker, "finish([undefined, NaN, Infinity, -0]);");
    let ExecutionOutcome::Finished(Value::List(values)) =
        rmp_serde::from_slice(&outcome).expect("outcome")
    else {
        panic!("list");
    };
    assert_eq!(values[0], Value::Undefined);
    let Value::Number(nan) = values[1] else {
        panic!("NaN");
    };
    assert!(nan.is_nan());
    assert_eq!(values[2], Value::Number(f64::INFINITY));
    let Value::Number(zero) = values[3] else {
        panic!("zero");
    };
    assert_eq!(zero.to_bits(), (-0_f64).to_bits());
    let tuple = AbilityOutcome::Value(Value::Tuple(
        vec![Value::Undefined, Value::Number(f64::NEG_INFINITY)].into(),
    ));
    let decoded: AbilityOutcome =
        rmp_serde::from_slice(&rmp_serde::to_vec_named(&tuple).expect("tuple bytes"))
            .expect("tuple");
    let AbilityOutcome::Value(Value::Tuple(values)) = decoded else {
        panic!("tuple identity lost");
    };
    assert_eq!(values[0], Value::Undefined);
    assert_eq!(values[1], Value::Number(f64::NEG_INFINITY));
    worker.release().expect("reset");
}

#[test]
fn cell_completion_carries_state_metadata_and_reset_clears_the_projection() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut worker = checkout(&pool);
    let mut input = start(
        "const planted = 7; print(planted); finish(planted);",
        ExecutionMode::Foreground,
    );
    let mut context: RunContext =
        rmp_serde::from_slice(&input.contexts[0].body.0).expect("context");
    context.capture_state_view = true;
    input.contexts[0].body = EncodedPayload(rmp_serde::to_vec_named(&context).expect("context"));
    let message = worker.start(input).expect("start");
    let WorkerMessage::Complete { state, value } = drive(&mut worker, message) else {
        panic!("the cell completes with its state metadata");
    };
    let completion: service::CellCompletion =
        rmp_serde::from_slice(&value.0).expect("cell completion");
    let metadata: service::StateMetadata =
        rmp_serde::from_slice(&completion.state.0).expect("state metadata");
    assert_eq!(
        completion.outcome,
        lashlang::ExecutionOutcome::Finished(lashlang::Value::Number(7.0))
    );
    assert!(metadata.names.contains("planted"));
    assert_eq!(
        metadata.globals.get("planted"),
        Some(&lashlang::Value::Number(7.0))
    );
    assert!(
        metadata
            .definition_ids
            .iter()
            .eq(state.definition_ids().iter())
    );
    worker.release().expect("reset cell");

    let mut next = checkout(&pool);
    let (_, value) = complete(&mut next, "finish(typeof planted);");
    assert_eq!(
        rmp_serde::from_slice::<lashlang::ExecutionOutcome>(&value).expect("plain outcome"),
        lashlang::ExecutionOutcome::Finished(lashlang::Value::String("undefined".into()))
    );
    next.release().expect("reset plain run");
}

#[test]
fn reset_failure_discards_and_reaps_before_replacement() {
    let pool = WorkerPool::new(config("reset_abort")).expect("pool");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    complete(&mut worker, "finish(1);");
    assert!(worker.release().is_err());
    assert_reaped(pid);
    let replacement = checkout(&pool);
    assert_ne!(replacement.pid(), Some(pid));
    replacement.release().expect("unused replacement");
}

#[test]
fn oversized_worker_output_is_typed_and_discards() {
    let mut cfg = config("");
    cfg.protocol.max_effect_value_bytes = 200;
    let pool = WorkerPool::new(cfg).expect("pool");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    let error = worker
        .start(start(
            "finish('x'.repeat(1024));",
            ExecutionMode::Foreground,
        ))
        .expect_err("effect size limit");
    let PoolError::Infrastructure(outcome) = error else {
        panic!("typed size cause")
    };
    let encoded = serde_json::to_value(&outcome).expect("cause");
    let cause = &encoded["worker_limit_exceeded"]["limit"]["effect_value"];
    assert_eq!(cause["bound"], 200);
    assert!(cause["size"].as_u64().is_some_and(|size| size > 200));
    assert!(!outcome.is_retryable());
    assert_reaped(pid);
    assert_ne!(checkout(&pool).pid(), Some(pid));
}

#[test]
fn physical_cancel_discards_without_answering_the_pending_effect() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    assert!(matches!(
        worker
            .start(start(
                "finish(await tools.echo({ value: 7 }));",
                ExecutionMode::Foreground
            ))
            .expect("start"),
        WorkerMessage::EffectRequest(_)
    ));
    assert_eq!(worker.cancel(), Ok(WorkerMessage::Cancelled));
    assert_reaped(pid);
    assert_ne!(checkout(&pool).pid(), Some(pid));
}

#[test]
fn state_kind_mismatch_is_refused_before_worker_dispatch() {
    for kind in [VmStateKind::Snapshot, VmStateKind::Continuation] {
        let pool = WorkerPool::new(config("")).expect("pool");
        let mut worker = checkout(&pool);
        let pid = worker.pid().expect("pid");
        let mut input = start("finish(1);", ExecutionMode::Foreground);
        let state = OpaqueVmState::seal(
            kind,
            input.owner.clone(),
            lashlang::vm_contract_versions(),
            vec![1, 2, 3],
        );
        input.state = match kind {
            VmStateKind::Snapshot => StartState::Continuation(state),
            VmStateKind::Continuation => StartState::Snapshot(state),
        };
        assert!(matches!(
            worker.start(input),
            Err(PoolError::Infrastructure(
                InfrastructureOutcome::RunRefused {
                    refusal: RunRefusal::State {
                        refusal: OpaqueStateRefusal::WrongKind { .. }
                    }
                }
            ))
        ));
        assert_reaped(pid);
        let replacement = checkout(&pool);
        assert_ne!(replacement.pid(), Some(pid));
        replacement.release().expect("replacement reset");
    }
}

/// FIG-4645: an input of the run the worker cannot read is refused the same
/// way on every attempt. Each refusal crosses the pipe as its typed cause and
/// is terminal: never the attempt's host verdict, which would redrive the
/// run into the same refusal for ever.
#[test]
fn a_deterministic_refusal_of_a_runs_inputs_is_terminal_and_typed() {
    let fresh = || start("finish(1);", ExecutionMode::Foreground);
    let sealed = |kind, owner: &str, bytes: Vec<u8>| {
        OpaqueVmState::seal(
            kind,
            VmOwner::new(owner),
            lashlang::vm_contract_versions(),
            bytes,
        )
    };
    let artifact = || artifact_start(b::program(vec![b::finish(b::num(1.0))]));
    let state = |kind| RunInput::State { kind };
    type Refused = fn(&RunRefusal) -> bool;
    type Typed = Box<dyn Fn(&RunRefusal) -> bool>;
    let cases: Vec<(&str, Start, Typed)> = vec![
        (
            "a continuation that does not decode",
            Start {
                state: StartState::Continuation(sealed(
                    VmStateKind::Continuation,
                    "session-A",
                    vec![0xc1],
                )),
                ..fresh()
            },
            Box::new(move |refusal| {
                matches!(refusal, RunRefusal::Undecodable { input, .. }
                    if *input == state(VmStateKind::Continuation))
            }),
        ),
        (
            "a snapshot that does not decode",
            Start {
                state: StartState::Snapshot(sealed(
                    VmStateKind::Snapshot,
                    "session-A",
                    vec![1, 2, 3],
                )),
                ..fresh()
            },
            Box::new(move |refusal| {
                matches!(refusal, RunRefusal::Undecodable { input, .. }
                    if *input == state(VmStateKind::Snapshot))
            }),
        ),
        (
            "state of another owner",
            Start {
                state: StartState::Snapshot(sealed(
                    VmStateKind::Snapshot,
                    "session-B",
                    vec![1, 2, 3],
                )),
                ..fresh()
            },
            Box::new(
                (|refusal| {
                    matches!(
                        refusal,
                        RunRefusal::State {
                            refusal: OpaqueStateRefusal::WrongOwner { .. }
                        }
                    )
                }) as Refused,
            ),
        ),
        (
            "source that does not parse",
            start("finish(", ExecutionMode::Foreground),
            Box::new((|refusal| matches!(refusal, RunRefusal::Parse { .. })) as Refused),
        ),
        (
            "source in another dialect",
            Start {
                program: ProgramSource::Source {
                    dialect: "another-dialect".into(),
                    text: "finish(1);".into(),
                },
                ..fresh()
            },
            Box::new((|refusal| matches!(refusal, RunRefusal::SourceDialect)) as Refused),
        ),
        (
            "an artifact that does not decode",
            Start {
                program: ProgramSource::Artifact {
                    module_ref: "lashlang:v2:blake3:00".into(),
                    entry: ProgramEntry::Main,
                    artifact: b"not an artifact".to_vec(),
                },
                ..fresh()
            },
            Box::new(
                (|refusal| {
                    matches!(
                        refusal,
                        RunRefusal::Undecodable {
                            input: RunInput::Artifact,
                            ..
                        }
                    )
                }) as Refused,
            ),
        ),
        (
            "an artifact under another module's name",
            {
                let mut input = artifact();
                let ProgramSource::Artifact { module_ref, .. } = &mut input.program else {
                    panic!("an artifact start")
                };
                module_ref.push_str("-another");
                input
            },
            Box::new(
                (|refusal| matches!(refusal, RunRefusal::ArtifactIdentityMismatch)) as Refused,
            ),
        ),
        (
            "a context the worker does not know",
            {
                let mut input = fresh();
                input.contexts[0].kind = "another-context".into();
                input
            },
            Box::new((|refusal| matches!(refusal, RunRefusal::UnknownContext)) as Refused),
        ),
        (
            "a context that does not decode",
            {
                let mut input = fresh();
                input.contexts[0].body = EncodedPayload(vec![0x90]);
                input
            },
            Box::new(
                (|refusal| {
                    matches!(
                        refusal,
                        RunRefusal::Undecodable {
                            input: RunInput::Context,
                            ..
                        }
                    )
                }) as Refused,
            ),
        ),
        (
            "a zero frame depth",
            {
                let mut input = fresh();
                input.limits.max_frame_depth = 0;
                input
            },
            Box::new((|refusal| matches!(refusal, RunRefusal::ZeroLimit)) as Refused),
        ),
    ];
    for (name, input, typed) in cases {
        let pool = WorkerPool::new(config("")).expect("pool");
        let mut worker = checkout(&pool);
        let pid = worker.pid().expect("pid");
        let error = worker.start(input).expect_err(name);
        let PoolError::Infrastructure(outcome) = &error else {
            panic!("{name}: a typed cause, got {error:?}")
        };
        assert!(
            !outcome.is_retryable() && !error.is_host_verdict(),
            "{name}: {outcome:?} refuses every attempt the same way"
        );
        let InfrastructureOutcome::RunRefused { refusal } = outcome else {
            panic!("{name}: the run is refused, got {outcome:?}")
        };
        assert!(typed(refusal), "{name}: {refusal:?}");
        assert_reaped(pid);
        let replacement = checkout(&pool);
        assert_ne!(replacement.pid(), Some(pid));
        replacement.release().expect("replacement reset");
    }
}

#[test]
fn effect_responses_preserve_the_checkout_kernel_cpu_ceiling() {
    let pool = WorkerPool::new(config("cpu_ceiling")).expect("pool");
    let mut worker = checkout(&pool);
    complete(&mut worker, "print(1); print(2); finish(3);");
    worker.release().expect("release");
}

#[test]
fn expected_abandonments_and_guest_errors_leave_the_pool_available() {
    let mut cfg = config("");
    cfg.max_restarts = 2;
    let pool = WorkerPool::new(cfg).expect("pool");
    for _ in 0..4 {
        let mut worker = checkout(&pool);
        let message = worker
            .start(start(
                "finish(await tools.echo({ value: 7 }));",
                ExecutionMode::Foreground,
            ))
            .expect("start");
        assert!(matches!(message, WorkerMessage::EffectRequest(_)));
        drop(worker);
        let mut worker = checkout(&pool);
        let message = worker
            .start(start(
                "throw new Error('guest');",
                ExecutionMode::Foreground,
            ))
            .expect("start guest error");
        assert!(matches!(
            drive(&mut worker, message),
            WorkerMessage::GuestError { .. }
        ));
        assert!(!pool.stats().restart_storm);
    }
    let mut worker = checkout(&pool);
    complete(&mut worker, "finish(3);");
    worker.release().expect("release");
}

/// A cell that awaits a process parks and gives its slot back, so the one
/// worker runs the awaited body, and the cell resumes and completes with the
/// body's terminal (FIG-4275).
#[test]
fn one_slot_process_await_releases_worker_for_the_awaited_body() {
    let process_id = lash_core_execution::ProcessId::fixture("one-slot-awaited-body");
    let source = format!(
        "const handle = await tools.echo({{ value: {{ __handle__: 'lash', id: 'p.{}' }} }}); finish(await handle);",
        process_id.as_str(),
    );
    let pool = WorkerPool::new(checkout_timeout_config()).expect("pool");
    let mut worker = checkout(&pool);
    let mut message = worker
        .start(start(&source, ExecutionMode::Foreground))
        .expect("start");
    let mut requests = 0;
    let awaited = loop {
        requests += 1;
        assert!(requests <= 8, "the cell reaches its process await");
        let WorkerMessage::EffectRequest(request) = message else {
            panic!("the cell asks for its process await: {message:?}");
        };
        if request.kind == EffectKind::Await {
            break request;
        }
        message = worker.effect_result(answer(request)).expect("answer setup");
    };
    // The checkout deadline detects the dependency cycle without hanging:
    // the awaited body needs the slot that its waiting cell still owns.
    assert!(matches!(
        pool.checkout(1, OwnerEpoch(1), FrameEpoch(1), ExecutionBudget::default()),
        Err(PoolError::CheckoutTimedOut)
    ));
    let state = parked(worker.park().expect("a process await must park"));
    worker.release().expect("release the waiting cell");
    let mut body = checkout(&pool);
    let (_, expected) = complete(&mut body, "finish(42);");
    body.release().expect("release the awaited body");
    let mut resumed = checkout(&pool);
    let mut input = start(&source, ExecutionMode::Foreground);
    input.state = StartState::Continuation(state);
    let WorkerMessage::EffectRequest(again) = resumed.start(input).expect("resume") else {
        panic!("the process await is reissued");
    };
    assert_eq!(
        (again.kind, &again.payload),
        (awaited.kind, &awaited.payload)
    );
    let response = EffectResponse {
        id: again.id,
        outcome: EffectOutcome::Value(EncodedPayload(
            rmp_serde::to_vec_named(&AbilityOutcome::Value(lashlang::Value::Number(42.0)))
                .expect("process result"),
        )),
    };
    let message = resumed
        .effect_result(response)
        .expect("answer the process await");
    let WorkerMessage::Complete { value, .. } = drive(&mut resumed, message) else {
        panic!("the cell completes after its awaited body");
    };
    assert_eq!(value.0, expected);
    resumed.release().expect("release resumed cell");
}

/// A process handle literal for the fixture process `name`.
fn process_handle(name: &str) -> lashlang::Expr {
    b::record(vec![
        ("__handle__", b::string("lash")),
        (
            "id",
            b::string(&format!(
                "p.{}",
                lash_core_execution::ProcessId::fixture(name).as_str()
            )),
        ),
    ])
}

/// A foreground run of `program`, sent to the worker as a module artifact.
fn artifact_start(program: lashlang::Program) -> Start {
    let artifact = lashlang::ModuleArtifact::from_program(program).expect("module artifact");
    let mut input = start("", ExecutionMode::Foreground);
    input.program = ProgramSource::Artifact {
        module_ref: artifact.module_ref().to_string(),
        entry: ProgramEntry::Main,
        artifact: artifact.to_store_bytes().expect("artifact bytes"),
    };
    input
}

/// A cell that awaits `awaited`, a container of three process handles.
fn aggregate_await(awaited: lashlang::Expr) -> Start {
    artifact_start(b::program(vec![b::finish(b::await_expr(awaited))]))
}

fn three_handles() -> [lashlang::Expr; 3] {
    ["aggregate-a", "aggregate-b", "aggregate-c"].map(process_handle)
}

/// What a run showed its parent: how it ended, and every request it made
/// that its parent had not already answered, in order.
#[derive(Debug, PartialEq, Eq)]
struct Transcript {
    value: Vec<u8>,
    requests: Vec<(EffectKind, EncodedPayload)>,
}

/// The run answered straight through, on one checkout.
fn straight(pool: &WorkerPool, input: &Start) -> Transcript {
    let mut worker = checkout(pool);
    let mut message = worker.start(input.clone()).expect("start");
    let mut requests = Vec::new();
    loop {
        match message {
            WorkerMessage::EffectRequest(request) => {
                requests.push((request.kind, request.payload.clone()));
                message = worker.effect_result(answer(request)).expect("answer");
            }
            WorkerMessage::Complete { value, .. } => {
                worker.release().expect("release");
                return Transcript {
                    value: value.0,
                    requests,
                };
            }
            other => panic!("expected Complete, received {other:?}"),
        }
    }
}

/// Drives `worker`'s run to its end, parking it on every `parks_on` request
/// the way the broker parks a run whose effect needs a worker of its own:
/// the one slot is held while the run awaits, the run parks and releases
/// it, the awaited body runs on it, and the run resumes from its
/// continuation and issues its request again, which is answered with the
/// outcome the parent held. Answers the run's requests before the first
/// park into `transcript`; returns the number of parks.
fn drive_parking(
    pool: &WorkerPool,
    input: &Start,
    mut worker: Checkout,
    mut message: WorkerMessage,
    parks_on: EffectKind,
    transcript: &mut Transcript,
) -> usize {
    let mut held = None::<EffectRequest>;
    let mut parks = 0;
    loop {
        match message {
            WorkerMessage::EffectRequest(request) => {
                if let Some(parked_on) = held.take() {
                    assert_eq!(
                        (request.kind, &request.payload),
                        (parked_on.kind, &parked_on.payload),
                        "the resumed run issues the request it parked on"
                    );
                    message = worker
                        .effect_result(answer(request))
                        .expect("answer the held outcome");
                    continue;
                }
                transcript
                    .requests
                    .push((request.kind, request.payload.clone()));
                if request.kind != parks_on {
                    message = worker.effect_result(answer(request)).expect("answer");
                    continue;
                }
                assert!(
                    matches!(
                        pool.checkout(1, OwnerEpoch(1), FrameEpoch(1), ExecutionBudget::default()),
                        Err(PoolError::CheckoutTimedOut)
                    ),
                    "the one slot is held while the run awaits its {:?}",
                    request.kind
                );
                let state = parked(worker.park().expect("the awaiting run parks"));
                worker.release().expect("the parked run's slot goes back");
                let mut body = checkout(pool);
                complete(&mut body, "finish(42);");
                body.release().expect("release the awaited body");
                worker = checkout(pool);
                let mut resumed = input.clone();
                resumed.state = StartState::Continuation(state);
                message = worker.start(resumed).expect("resume the parked run");
                held = Some(request);
                parks += 1;
            }
            WorkerMessage::Complete { value, .. } => {
                worker.release().expect("release");
                transcript.value = value.0;
                return parks;
            }
            other => panic!("expected Complete, received {other:?}"),
        }
    }
}

fn run_parking(pool: &WorkerPool, input: &Start, parks_on: EffectKind) -> (Transcript, usize) {
    let mut worker = checkout(pool);
    let message = worker.start(input.clone()).expect("start");
    let mut transcript = Transcript {
        value: Vec::new(),
        requests: Vec::new(),
    };
    let parks = drive_parking(pool, input, worker, message, parks_on, &mut transcript);
    (transcript, parks)
}

/// A cell awaiting an array or a record of three process handles parks on
/// each handle still pending, so a pool of one worker runs every awaited body
/// and the cell ends as it would straight through: the same awaits, each
/// issued once and in the same order, and the same value (FIG-4275).
#[test]
fn one_slot_aggregate_process_await_parks_on_every_pending_handle() {
    let pool = WorkerPool::new(checkout_timeout_config()).expect("pool");
    let [a, b_handle, c] = three_handles();
    let shapes = [
        (
            "array",
            b::list(vec![a.clone(), b_handle.clone(), c.clone()]),
        ),
        (
            "record",
            b::record(vec![("a", a), ("b", b_handle), ("c", c)]),
        ),
    ];
    for (shape, awaited) in shapes {
        let input = aggregate_await(awaited);
        let expected = straight(&pool, &input);
        assert_eq!(
            expected
                .requests
                .iter()
                .filter(|(kind, _)| *kind == EffectKind::Await)
                .count(),
            3,
            "{shape}: one await per handle"
        );
        let (parked, parks) = run_parking(&pool, &input, EffectKind::Await);
        assert_eq!(parks, 3, "{shape}: the cell parks on every pending handle");
        assert_eq!(
            parked, expected,
            "{shape}: the parked cell ends as it would straight through"
        );
    }
}

/// A parent that crashes while a cell is parked mid-aggregate loses nothing
/// the cell needs: a new parent that replays the cell from its start parks
/// and ends as the lost one would have, and the continuation the lost parent
/// held resumes on the new parent's worker with the handles it had already
/// settled, issues only the pending await again, and ends with the same
/// value (FIG-4275).
#[test]
fn a_parked_aggregate_await_resumes_after_a_parent_crash_with_the_same_result() {
    let [a, b_handle, c] = three_handles();
    let input = aggregate_await(b::list(vec![a, b_handle, c]));
    let pool = WorkerPool::new(checkout_timeout_config()).expect("pool");
    let expected = straight(&pool, &input);

    // The first handle's terminal is answered in place; the cell parks on
    // the second, holding the first's result in its continuation.
    let mut worker = checkout(&pool);
    let WorkerMessage::EffectRequest(first) = worker.start(input.clone()).expect("start") else {
        panic!("the cell awaits its first handle");
    };
    assert_eq!(first.kind, EffectKind::Await);
    let WorkerMessage::EffectRequest(second) = worker
        .effect_result(answer(first.clone()))
        .expect("answer the first handle")
    else {
        panic!("the cell awaits its second handle");
    };
    assert_eq!(second.kind, EffectKind::Await);
    let state = parked(worker.park().expect("the cell parks on its second handle"));

    // The parent crashes mid-park: its worker and its pool are gone.
    drop(worker);
    drop(pool);

    let pool = WorkerPool::new(checkout_timeout_config()).expect("the new parent's pool");
    let (replayed, parks) = run_parking(&pool, &input, EffectKind::Await);
    assert_eq!(parks, 3, "the replayed cell parks on every pending handle");
    assert_eq!(
        replayed, expected,
        "the replayed cell issues the recorded awaits and ends the same"
    );

    let mut worker = checkout(&pool);
    let mut resumed = input.clone();
    resumed.state = StartState::Continuation(state);
    let message = worker.start(resumed).expect("resume on the new parent");
    let WorkerMessage::EffectRequest(again) = &message else {
        panic!("the resumed cell issues its pending await: {message:?}");
    };
    assert_eq!(
        (again.kind, &again.payload),
        (second.kind, &second.payload),
        "the resumed cell issues the await it parked on, never the settled one"
    );
    let mut transcript = Transcript {
        value: Vec::new(),
        requests: vec![
            (first.kind, first.payload.clone()),
            (second.kind, second.payload.clone()),
        ],
    };
    let reissued = transcript.requests.len();
    let message = worker
        .effect_result(answer(again.clone()))
        .expect("answer the held outcome");
    let parks = drive_parking(
        &pool,
        &input,
        worker,
        message,
        EffectKind::Await,
        &mut transcript,
    );
    assert_eq!(parks, 1, "only the third handle is still pending");
    assert!(transcript.requests.len() > reissued);
    assert_eq!(
        transcript, expected,
        "the resumed cell ends as it would straight through"
    );
}

/// A `Promise.all` over pending tool calls is one resource-operation batch:
/// the cell parks on it with one slot, the slot runs other work, and the
/// resumed cell issues the same batch again and ends as it would straight
/// through, in both execution modes (FIG-4275).
#[test]
fn one_slot_resource_operation_batch_parks_and_resumes() {
    let source = "const [a, b, c] = await Promise.all([tools.echo({ value: 1 }), tools.echo({ value: 2 }), tools.echo({ value: 3 })]); finish(a + b + c);";
    let pool = WorkerPool::new(checkout_timeout_config()).expect("pool");
    for mode in [ExecutionMode::Foreground, ExecutionMode::Process] {
        let input = start(source, mode);
        let expected = straight(&pool, &input);
        assert_eq!(
            expected
                .requests
                .iter()
                .filter(|(kind, _)| *kind == EffectKind::ResourceOperationBatch)
                .count(),
            1,
            "{mode:?}: the aggregate is one batch"
        );
        let (parked, parks) = run_parking(&pool, &input, EffectKind::ResourceOperationBatch);
        assert_eq!(parks, 1, "{mode:?}: the cell parks on its batch");
        assert_eq!(
            parked, expected,
            "{mode:?}: the parked cell ends as it would straight through"
        );
    }
}

/// Iterations of the long loop: several times the few thousand at which a
/// run's execution observations once outgrew one frame's decode bounds.
const LONG_LOOP: u32 = 20_000;

/// A run that reports its execution observations, as a durable process body
/// or a traced cell does.
fn observed(source: &str, mode: ExecutionMode) -> Start {
    let mut input = start(source, mode);
    input.contexts[0].body = EncodedPayload(
        rmp_serde::to_vec_named(&RunContext {
            environment: lashlang::testing::harness::test_environment(),
            mode,
            observe_execution: true,
            ..RunContext::default()
        })
        .expect("context"),
    );
    input
}

fn long_loop() -> String {
    format!("let n = 0; while (n < {LONG_LOOP}) {{ n++; }} finish(n);")
}

/// A long loop within its fuel, heap and depth budgets completes, and its
/// execution observations cross in chunks that each pass the decode bounds
/// the broker holds an observation payload to (FIG-4458).
#[test]
fn a_long_loop_streams_its_observations_in_chunks_within_the_decode_bounds() {
    let cfg = config("");
    let codec = FrameCodec::new(cfg.protocol.decode);
    let pool = WorkerPool::new(cfg).expect("pool");
    let mut worker = checkout(&pool);
    let message = worker
        .start(observed(&long_loop(), ExecutionMode::Foreground))
        .expect("the long loop runs within its budgets");
    assert!(matches!(
        drive(&mut worker, message),
        WorkerMessage::Complete { .. }
    ));
    let chunks = worker.take_observations();
    let mut loop_steps = 0;
    for chunk in &chunks {
        codec
            .check_payload(&chunk.0)
            .expect("every observation chunk is within the decode bounds");
        let observations: Vec<lashlang::LashlangExecutionObservation> =
            rmp_serde::from_slice(&chunk.0).expect("an observation chunk");
        loop_steps += observations
            .iter()
            .filter(|observation| {
                matches!(
                    observation,
                    lashlang::LashlangExecutionObservation::NodeCompleted { .. }
                )
            })
            .count();
    }
    assert!(
        chunks.len() > 1,
        "the stream is chunked: {} chunk(s)",
        chunks.len()
    );
    assert!(
        loop_steps >= LONG_LOOP as usize,
        "every iteration is observed: {loop_steps} completed nodes"
    );
    worker.release().expect("reset");
}

/// A step whose observation stream outgrows the run's own heap budget ends
/// with the run's typed limit, which no retry answers differently: never a
/// protocol violation retried forever (FIG-4458).
#[test]
fn an_observation_stream_over_the_runs_heap_budget_is_its_typed_run_limit() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    let mut input = observed(&long_loop(), ExecutionMode::Foreground);
    input.limits.memory_limit_bytes = Some(256 * 1024);
    let outcome = match worker.start(input) {
        Err(PoolError::Infrastructure(outcome)) => outcome,
        other => panic!("expected the run's observation limit, received {other:?}"),
    };
    assert_eq!(
        outcome,
        InfrastructureOutcome::WorkerLimitExceeded {
            limit: WorkerLimit::Observations
        }
    );
    assert!(!outcome.is_retryable(), "the run's own limit is final");
    assert_reaped(pid);
}

#[test]
fn an_oversized_effect_value_is_a_typed_non_retryable_run_limit() {
    let source = "const result = await tools.echo({ value: \"a value larger than the configured effect bound\" }); finish(result);";
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut worker = checkout(&pool);
    let WorkerMessage::EffectRequest(request) = worker
        .start(start(source, ExecutionMode::Foreground))
        .expect("request")
    else {
        panic!("effect request");
    };
    let size = request.payload.0.len() as u64;
    drop(worker);
    drop(pool);
    let mut cfg = config("");
    cfg.protocol.max_effect_value_bytes = size - 1;
    let pool = WorkerPool::new(cfg).expect("bounded pool");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    let error = worker
        .start(start(source, ExecutionMode::Foreground))
        .expect_err("effect limit");
    let PoolError::Infrastructure(outcome) = error else {
        panic!("typed cause: {error:?}")
    };
    assert_eq!(
        serde_json::to_value(&outcome).expect("cause"),
        serde_json::json!({
            "worker_limit_exceeded": { "limit": { "effect_value": { "size": size, "bound": size - 1 } } }
        })
    );
    assert!(!outcome.is_retryable());
    assert_reaped(pid);
}

#[test]
fn oversized_vm_state_is_a_typed_non_retryable_run_limit() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut worker = checkout(&pool);
    let message = worker
        .start(start("finish(42);", ExecutionMode::Foreground))
        .expect("start");
    let WorkerMessage::Complete { state, .. } = drive(&mut worker, message) else {
        panic!("complete")
    };
    let size = state.bytes().len() as u64;
    drop(worker);
    drop(pool);
    let mut cfg = config("");
    cfg.protocol.max_vm_state_bytes = size - 1;
    let pool = WorkerPool::new(cfg).expect("bounded pool");
    let mut worker = checkout(&pool);
    let pid = worker.pid().expect("pid");
    let message = worker
        .start(start("finish(42);", ExecutionMode::Foreground))
        .expect("start");
    let WorkerMessage::EffectRequest(request) = message else {
        panic!("finish request")
    };
    let error = worker
        .effect_result(answer(request))
        .expect_err("state limit");
    let PoolError::Infrastructure(outcome) = error else {
        panic!("typed cause: {error:?}")
    };
    assert_eq!(
        serde_json::to_value(&outcome).expect("cause"),
        serde_json::json!({
            "worker_limit_exceeded": { "limit": { "vm_state": { "size": size, "bound": size - 1 } } }
        })
    );
    assert!(!outcome.is_retryable());
    assert_reaped(pid);
}

#[test]
fn oversized_effect_answers_preserve_their_typed_run_limit() {
    for failed in [false, true] {
        let mut cfg = config("");
        cfg.protocol.max_effect_value_bytes = 512;
        let pool = WorkerPool::new(cfg).expect("pool");
        let mut worker = checkout(&pool);
        let WorkerMessage::EffectRequest(request) = worker
            .start(start(
                "const v = await tools.echo({ value: 1 }); finish(v);",
                ExecutionMode::Foreground,
            ))
            .expect("request")
        else {
            panic!("request")
        };
        let payload = EncodedPayload(
            rmp_serde::to_vec_named(&AbilityOutcome::Value(lashlang::Value::String(
                "x".repeat(1024).into(),
            )))
            .expect("answer"),
        );
        let size = payload.0.len() as u64;
        let error = worker
            .effect_result(EffectResponse {
                id: request.id,
                outcome: if failed {
                    EffectOutcome::Failed(payload)
                } else {
                    EffectOutcome::Value(payload)
                },
            })
            .expect_err("effect value limit");
        assert_eq!(
            error,
            PoolError::Infrastructure(InfrastructureOutcome::WorkerLimitExceeded {
                limit: WorkerLimit::EffectValue { size, bound: 512 },
            })
        );
        assert!(!error.is_host_verdict());
    }
}

#[test]
fn oversized_incoming_and_parked_vm_state_preserves_its_typed_run_limit() {
    let source = "const v = await tools.echo({ value: 1 }); finish(v);";
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut worker = checkout(&pool);
    let message = worker
        .start(start(source, ExecutionMode::Foreground))
        .expect("request");
    assert!(matches!(message, WorkerMessage::EffectRequest(_)));
    let state = parked(worker.park().expect("parked"));
    let size = state.bytes().len() as u64;
    drop(worker);
    drop(pool);
    for bound in [size, size - 1] {
        let mut cfg = config("");
        cfg.protocol.max_vm_state_bytes = bound;
        let pool = WorkerPool::new(cfg).expect("pool");
        let mut worker = checkout(&pool);
        let mut input = start(source, ExecutionMode::Foreground);
        input.state = StartState::Continuation(state.clone());
        let resumed = worker.start(input);
        if bound == size {
            assert!(
                matches!(resumed, Ok(WorkerMessage::EffectRequest(_))),
                "the exact bound is admitted: {resumed:?}"
            );
        } else {
            assert_eq!(
                resumed,
                Err(PoolError::Infrastructure(
                    InfrastructureOutcome::WorkerLimitExceeded {
                        limit: WorkerLimit::VmState { size, bound },
                    }
                ))
            );
        }
        drop(worker);
        let mut worker = checkout(&pool);
        worker
            .start(start(source, ExecutionMode::Foreground))
            .expect("request");
        let result = worker.park();
        if bound == size {
            assert!(
                matches!(result, Ok(ParkOutcome::Parked(_))),
                "the exact bound parks: {result:?}"
            );
        } else {
            assert_eq!(
                result.expect_err("state limit"),
                PoolError::Infrastructure(InfrastructureOutcome::WorkerLimitExceeded {
                    limit: WorkerLimit::VmState { size, bound },
                })
            );
        }
    }
}

/// FIG-4433: the socket calls one effect exchange costs the parent are
/// bounded by its frames: one write for the answer, and for each of the four
/// frames that follow (Computing, Serializing, Responding and the next
/// request) at most one timeout and one read. A header and its payload are
/// never two reads, and a write that does not wait arms no timeout.
#[test]
fn an_effect_exchange_costs_the_parent_a_bounded_number_of_socket_calls() {
    const EFFECTS: usize = 20;
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut worker = checkout(&pool);
    let mut message = worker
        .start(start(
            &format!(
                "for (let i = 0; i < {EFFECTS}; i++) {{ await tools.echo({{ value: i }}); }} finish(1);"
            ),
            ExecutionMode::Foreground,
        ))
        .expect("start");
    let mut exchanges = 0;
    loop {
        match message {
            WorkerMessage::EffectRequest(request) => {
                let counted = request.kind == EffectKind::ResourceOperation;
                let response = answer(request);
                let before = lash_vm_client::ipc::socket_calls();
                message = worker.effect_result(response).expect("resume");
                let calls = lash_vm_client::ipc::socket_calls() - before;
                if counted {
                    exchanges += 1;
                    assert!(
                        calls <= 1 + 2 * 4,
                        "effect exchange {exchanges} cost the parent {calls} socket calls"
                    );
                }
            }
            WorkerMessage::Complete { .. } => break,
            other => panic!("expected Complete, received {other:?}"),
        }
    }
    assert_eq!(exchanges, EFFECTS);
    worker.release().expect("reset");
}

/// FIG-4433: the parent encodes an effect answer once. Answering with a
/// large value allocates one frame of its size, not one per encoding pass.
#[test]
fn a_large_effect_answer_is_encoded_once_by_the_parent() {
    const VALUE_BYTES: usize = 512 * 1024;
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut worker = checkout(&pool);
    let mut message = worker
        .start(start(
            &format!(
                "const value = await tools.echo({{ value: 'x'.repeat({VALUE_BYTES}) }}); finish(value.length);"
            ),
            ExecutionMode::Foreground,
        ))
        .expect("start");
    let mut answered = 0;
    loop {
        match message {
            WorkerMessage::EffectRequest(request) => {
                let large = request.kind == EffectKind::ResourceOperation;
                let response = answer(request);
                let before = ALLOCATED_BYTES.with(std::cell::Cell::get);
                message = worker.effect_result(response).expect("resume");
                let allocated = ALLOCATED_BYTES.with(std::cell::Cell::get) - before;
                if large {
                    answered += 1;
                    assert!(
                        allocated < (VALUE_BYTES + VALUE_BYTES / 2) as u64,
                        "answering with a {VALUE_BYTES}-byte value allocated {allocated} bytes in the parent"
                    );
                }
            }
            WorkerMessage::Complete { .. } => break,
            other => panic!("expected Complete, received {other:?}"),
        }
    }
    assert_eq!(answered, 1);
    worker.release().expect("reset");
}

#[tokio::test]
#[expect(
    clippy::disallowed_methods,
    reason = "the law exercises kernel executable permissions on its isolated temporary worker fixture"
)]
async fn missing_and_unexecutable_workers_are_deployment_faults_on_both_pool_seams() {
    use lash_vm_client::service::{Service, runtime_ops::ServiceRuntimeOps as _};
    use std::os::unix::fs::PermissionsExt as _;

    let directory = tempfile::tempdir().expect("tempdir");
    for (name, contents, fault) in [
        ("missing", None, "not_found"),
        ("unexecutable", Some(b"worker".as_slice()), "not_executable"),
    ] {
        let path = directory.path().join(name);
        if let Some(contents) = contents {
            std::fs::write(&path, contents).expect("worker file");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("non-executable permissions");
        }
        let service = Service::subprocess(&path);
        let sync = service.pool().err().expect("sync deployment refusal");
        let asynchronous = service
            .pool_accounted()
            .await
            .err()
            .expect("async deployment refusal");
        assert_eq!(
            sync, asynchronous,
            "both seams classify the same spawn failure"
        );
        for error in [sync, asynchronous] {
            assert!(
                error.is_host_verdict(),
                "a deployment fault records no guest result"
            );
            let error = error.into_runtime_error();
            assert!(!error.is_retryable(), "{error:?}");
            assert!(!error.is_terminal(), "{error:?}");
            assert_eq!(
                error.turn_failure_cause(),
                lash_core_execution::TurnFailureCause::Parked
            );
            let encoded = serde_json::to_value(&error).expect("typed worker error");
            assert_eq!(encoded["cause"]["kind"], "vm_worker");
            assert_eq!(
                encoded["cause"]["outcome"]["worker_deployment"]["executable"],
                path.to_string_lossy().as_ref()
            );
            assert_eq!(
                encoded["cause"]["outcome"]["worker_deployment"]["fault"],
                fault
            );
            let decoded: lash_core_execution::RuntimeError =
                serde_json::from_value(encoded).expect("worker error round trip");
            assert_eq!(decoded.cause, error.cause);
            assert_eq!(decoded.code, error.code);
            assert_eq!(decoded.turn_failure_cause(), error.turn_failure_cause());
        }
    }
}
