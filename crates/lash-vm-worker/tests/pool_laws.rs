#![cfg(unix)]
#![expect(
    clippy::expect_used,
    reason = "integration test helpers fail on broken fixture assumptions"
)]

use lash_vm_protocol::*;
use lash_vm_worker::*;
use lashlang::{AbilityOp, AbilityOutcome, ExecutionMode};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::mpsc;
use std::time::{Duration, Instant};

fn config(mode: &str) -> PoolConfig {
    let mut entry = WorkerEntry::helper(env!("CARGO_BIN_EXE_lash-vm-worker-fixture"));
    if !mode.is_empty() {
        entry.args.push(mode.into());
    }
    let mut config = PoolConfig::standard(entry);
    config.max_workers = 1;
    config.deadlines.checkout = Duration::from_millis(150);
    config.protocol.no_response_watchdog = Duration::from_secs(2);
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
                serde_json::to_vec(&RunContext {
                    environment: lashlang::testing::harness::test_environment(),
                    mode,
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
            let op: AbilityOp = serde_json::from_slice(&request.payload.0).expect("operation");
            let result = match op {
                AbilityOp::ResourceOperation(op) => {
                    lashlang::testing::harness::EchoHost::perform_resource_operation(*op)
                        .map(AbilityOutcome::Value)
                }
                AbilityOp::Finish(v) | AbilityOp::Fail(v) => Ok(AbilityOutcome::Value(v)),
                AbilityOp::Print(_) => Ok(AbilityOutcome::Unit),
                _ => panic!("unexpected effect"),
            };
            match result {
                Ok(result) => EffectOutcome::Value(EncodedPayload(
                    serde_json::to_vec(&result).expect("answer"),
                )),
                Err(error) => EffectOutcome::Failed(EncodedPayload(
                    serde_json::to_vec(&error).expect("error"),
                )),
            }
        }
    };
    EffectResponse {
        id: request.id,
        outcome,
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
        let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
            .args([
                "--exact",
                "worker_sees_no_parent_environment_or_descriptors",
                "--nocapture",
            ])
            .env(SENTINEL, "parent-only-secret")
            .status()
            .expect("sentinel parent");
        assert!(status.success());
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
        let parked = worker.park().expect("sentinel continuation");
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
            serde_json::from_slice(&input.contexts[0].body.0).expect("context");
        context.environment = context.environment.with_globals([
            "planted",
            "plantedClosure",
            "plantedError",
            "plantedRecord",
            "plantedEcho",
            "plantedPattern",
        ]);
        input.contexts[0].body.0 = serde_json::to_vec(&context).expect("context");
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

#[test]
fn single_slot_nested_compile_refuses_without_silent_deadlock() {
    let pool = WorkerPool::new(config("")).expect("pool");
    let mut worker = checkout(&pool);
    let message = worker
        .start(start(
            "finish(await tools.echo({ value: 7 }));",
            ExecutionMode::Foreground,
        ))
        .expect("start");
    assert_eq!(worker.park(), Err(PoolError::PendingEffectParkingRequired));
    assert!(matches!(
        pool.checkout(1, OwnerEpoch(1), FrameEpoch(1), ExecutionBudget::default()),
        Err(PoolError::CheckoutTimedOut)
    ));
    let WorkerMessage::EffectRequest(request) = message else {
        panic!("request");
    };
    let message = worker.effect_result(answer(request)).expect("resume");
    drive(&mut worker, message);
    worker.release().expect("release");
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
    let state = worker.park().expect("park");
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
fn wrong_build_is_refused_before_model_code() {
    let mut cfg = config("");
    cfg.entry.build = BuildIdentity::new("another-build");
    assert!(matches!(
        WorkerPool::new(cfg),
        Err(PoolError::Infrastructure(
            InfrastructureOutcome::ProtocolViolation { .. }
        ))
    ));
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
        serde_json::from_slice(&outcome).expect("outcome")
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
        serde_json::from_slice(&serde_json::to_vec(&tuple).expect("tuple bytes")).expect("tuple");
    let AbilityOutcome::Value(Value::Tuple(values)) = decoded else {
        panic!("tuple identity lost");
    };
    assert_eq!(values[0], Value::Undefined);
    assert_eq!(values[1], Value::Number(f64::NEG_INFINITY));
    worker.release().expect("reset");
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
    assert!(matches!(
        worker.start(start(
            "finish('x'.repeat(1024));",
            ExecutionMode::Foreground
        )),
        Err(PoolError::Infrastructure(
            InfrastructureOutcome::PayloadTooLarge { limit: 200, .. }
        ))
    ));
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
            lashlang::vm_contract_identity(),
            if kind == VmStateKind::Snapshot {
                lashlang::LASHLANG_SNAPSHOT_VERSION
            } else {
                lashlang::VM_CONTINUATION_FORMAT_VERSION
            },
            vec![1, 2, 3],
        );
        input.state = match kind {
            VmStateKind::Snapshot => StartState::Continuation(state),
            VmStateKind::Continuation => StartState::Snapshot(state),
        };
        assert!(matches!(
            worker.start(input),
            Err(PoolError::Infrastructure(
                InfrastructureOutcome::ProtocolViolation { .. }
            ))
        ));
        assert_reaped(pid);
        let replacement = checkout(&pool);
        assert_ne!(replacement.pid(), Some(pid));
        replacement.release().expect("replacement reset");
    }
}
