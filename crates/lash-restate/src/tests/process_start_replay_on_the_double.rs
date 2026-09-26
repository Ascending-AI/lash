//! A parent's process start replays to the id it recorded (ADR 0107).
//!
//! Each law runs one process start inside a handler on the in-process Restate
//! double, crashes the attempt after the start, and lets the server replay the
//! invocation into a redrive that issues the same start. Restate replays a
//! handler from its first journal entry on every resume, so the redrive must
//! read the recorded registration: registering again would mint a second id
//! and send to a workflow key the journal never recorded (RT0016).

use super::*;
use lash_core::{ProcessListFilter, ProcessStatusFilter};

/// The ids each attempt's start returned, in attempt order.
type StartedIds = Arc<Mutex<Vec<ProcessId>>>;

/// The process-start envelope a host start issues under `scoped`: a start is
/// addressed by its key.
fn start_envelope(
    scoped: &lash_core::ScopedEffectController<'_>,
    registration: ProcessRegistration,
) -> RuntimeEffectEnvelope {
    let command = ProcessCommand::Start {
        registration,
        observers: Vec::new(),
        env_spec: None,
        execution_context: Box::new(ProcessExecutionContext::default()),
    };
    let effect_id = command.effect_id();
    RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(scoped.execution_scope().clone(), effect_id.clone())
                .expect("valid process-start address"),
            lash_core::RuntimeAttribution::none(),
            effect_id,
        ),
        RuntimeEffectCommand::process(command),
    )
}

/// Starts one process under `scoped` and records the id the start returned.
async fn start_and_record(
    scoped: &lash_core::ScopedEffectController<'_>,
    registry: &Arc<dyn ProcessRegistry>,
    registration: ProcessRegistration,
    started: &StartedIds,
) -> ProcessId {
    let outcome = scoped
        .execute_effect(
            start_envelope(scoped, registration),
            registry_local_executor(Arc::clone(registry)),
        )
        .await
        .expect("the start runs");
    let RuntimeEffectOutcome::Process {
        result: ProcessEffectOutcome::Start { record },
    } = outcome
    else {
        panic!("a start reports its process: {outcome:?}");
    };
    started.lock().unwrap().push(record.id.clone());
    record.id
}

/// Every process the registry retains, whatever its status.
async fn retained_processes(registry: &Arc<dyn ProcessRegistry>) -> Vec<ProcessId> {
    registry
        .list_processes(&ProcessListFilter {
            status: ProcessStatusFilter::Any,
            ..Default::default()
        })
        .await
        .expect("list processes")
        .into_iter()
        .map(|record| record.id)
        .collect()
}

/// ADR 0107, FIG-3607 review item 1: a parent that started a child, whose
/// child then finished and was pruned, replays its start to the recorded id.
///
/// The crashing attempt starts the child, completes it and prunes it — the
/// child finishing and host retention reclaiming it while the parent is
/// suspended — then dies. The server replays the parent. The redrive's start
/// must return the id the journal recorded, register no second process, and
/// send to the recorded workflow key; a live re-registration would mint a new
/// id, and its send would not match the journaled one, wedging the parent.
pub(super) async fn a_parent_replay_after_its_child_was_pruned_returns_the_recorded_id(seed: u64) {
    let backend = lash_restate_test::backend(seed, lash_restate_test::ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let registry = backend.lash_backend().process_registry();
    let started: StartedIds = Arc::default();
    let registration = || {
        external_registration().with_start_key(Some(lash_core::StartKey::for_orchestration_call(
            &lash_core::ExecutionScope::turn("session", "turn"),
            "spawn-child",
            0,
        )))
    };
    let crashing: lash_restate_test::HandlerAttempt = {
        let registry = Arc::clone(&registry);
        let started = Arc::clone(&started);
        Arc::new(move |scoped| {
            let registry = Arc::clone(&registry);
            let started = Arc::clone(&started);
            Box::pin(async move {
                let child = start_and_record(&scoped, &registry, registration(), &started).await;
                registry
                    .complete_process(
                        &child,
                        process_success(serde_json::json!({ "child": "done" })),
                        lash_core::ProcessCompletionAuthority::external_owner(),
                    )
                    .await
                    .expect("the child finishes");
                registry
                    .prune_terminal_processes(
                        u64::MAX,
                        None,
                        lash_core::ProjectionWatermark::NoProjector,
                    )
                    .await
                    .expect("host retention prunes the finished child");
                panic!("the parent dies after its child was pruned");
            })
        })
    };
    let redrive: lash_restate_test::HandlerAttempt = {
        let registry = Arc::clone(&registry);
        let started = Arc::clone(&started);
        Arc::new(move |scoped| {
            let registry = Arc::clone(&registry);
            let started = Arc::clone(&started);
            Box::pin(async move {
                start_and_record(&scoped, &registry, registration(), &started).await;
            })
        })
    };
    backend
        .run_crashed_then_redriven(
            lash_core::AdmittedScope::turn("session", "turn"),
            crashing,
            redrive,
        )
        .await
        .expect("the replayed parent completes without a journal mismatch");

    let started = started.lock().unwrap().clone();
    assert_eq!(
        started.len(),
        2,
        "each attempt ran the start once: {started:?}"
    );
    assert_eq!(
        started[1], started[0],
        "the replay returns the recorded id, never a second minted one"
    );
    assert_eq!(
        retained_processes(&registry).await,
        Vec::<ProcessId>::new(),
        "the replay registers no second process beside the pruned one"
    );
}

/// ADR 0107, FIG-3607 review item 2: a keyless host start inside a durable
/// handler replays to the process it started.
///
/// A keyless start's key is derived from the admitted scope and its ordinal
/// among the run's keyless starts, so the redrive re-issues the same key, the
/// same start effect, and reads the recorded registration. A key drawn at
/// random would address a new effect on every replay and start a second
/// process each time.
pub(super) async fn a_keyless_host_start_replays_to_the_process_it_started(seed: u64) {
    let backend = lash_restate_test::backend(seed, lash_restate_test::ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let registry = backend.lash_backend().process_registry();
    let started: StartedIds = Arc::default();
    let keyless = |scoped: &lash_core::ScopedEffectController<'_>| {
        lash_core::ProcessStartRequest::external(
            lash_core::ProcessOriginator::host(),
            serde_json::json!({ "work_item": "keyless" }),
            lash_core::Lifetime::Detached,
        )
        .keyed_in(scoped)
        .into_registration(None)
    };
    let attempt = |crash: bool| -> lash_restate_test::HandlerAttempt {
        let registry = Arc::clone(&registry);
        let started = Arc::clone(&started);
        Arc::new(move |scoped| {
            let registry = Arc::clone(&registry);
            let started = Arc::clone(&started);
            Box::pin(async move {
                let registration = keyless(&scoped);
                start_and_record(&scoped, &registry, registration, &started).await;
                assert!(!crash, "the first attempt dies after its keyless start");
            })
        })
    };
    backend
        .run_crashed_then_redriven(
            lash_core::AdmittedScope::runtime_operation("keyless-host-start"),
            attempt(true),
            attempt(false),
        )
        .await
        .expect("the replayed handler completes without a journal mismatch");

    let started = started.lock().unwrap().clone();
    assert_eq!(
        started.len(),
        2,
        "each attempt ran the start once: {started:?}"
    );
    assert_eq!(
        started[1], started[0],
        "the replayed keyless start returns the process it started"
    );
    assert_eq!(
        retained_processes(&registry).await,
        vec![started[0].clone()],
        "a keyless start replayed starts no second process"
    );
}

/// Twenty seeds per law (lane rule for laws on the double).
const SEEDS: std::ops::Range<u64> = 0x3607_0000..0x3607_0014;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_parent_replay_after_its_child_was_pruned_returns_the_recorded_id_on_the_double() {
    for seed in SEEDS {
        a_parent_replay_after_its_child_was_pruned_returns_the_recorded_id(seed).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_keyless_host_start_replays_to_the_process_it_started_on_the_double() {
    for seed in SEEDS {
        a_keyless_host_start_replays_to_the_process_it_started(seed).await;
    }
}
