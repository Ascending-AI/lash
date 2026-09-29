//! Process terminal publication (S-14).
//!
//! A Lashlang process sleeps and finishes with a value. An engine waiter — a
//! `LashProcessWorkflow` `await_terminal` invocation, the call an in-journal
//! `processes.await` makes — waits on the process's terminal promise. The
//! process workflow writes the terminal to SQL and then resolves that
//! promise; the waiter completes only through the promise.
//!
//! The mid-journal cell kills the deployment at a seeded step of the process
//! workflow's run. The invocation-lost cell loses the run between its
//! terminal write and the promise's resolution; the terminal transaction's
//! `ProcessTerminal` obligation (ADR 0109 §3) recovers it, and the relay
//! publishes the stored terminal to the waiter.
//!
//! The caller-killed cell starts the process from a host job that stays
//! open, and once the process's run has started an operator kills the job:
//! Restate's kill cascades into the run the job sent, which ends `409
//! killed` without the process's terminal, and Restate never runs that
//! workflow key again. The recovery tick's lost-run scan finds the finished
//! run and ends the process `SubstrateLost` (ADR 0110), and the terminal's
//! publication answers the waiter.

use std::sync::Arc;

use lash_core::ProcessId;
use lash_core::llm::transport::LlmTransportError;
use lash_core::llm::types::{LlmRequest, LlmResponse};
use lash_restate_test::{CrashPoint as EngineCut, CrashRule};
use lashlang::testing::ast_builders as b;

use super::Staged;
use crate::crash_matrix::CrashPoint;
use crate::crash_matrix::invariants::{CustomCheck, Expected};
use crate::crash_matrix::world::{CoreBuild, CrashWorld};

pub(crate) const PROCESS_WORKFLOW: &str = "LashProcessWorkflow";
const PROCESS: &str = "main";

/// A registration committed immediately before a deployment crash has no
/// caller left to submit its run. Its start obligation must do so after the
/// new deployment begins reconciling.
pub(super) async fn stage_start(point: CrashPoint, seed: u64) -> Result<Staged, String> {
    if point != CrashPoint::AfterStateCommit {
        return Err(format!("process start has no {point:?} cell"));
    }
    let world = CrashWorld::new(seed, rlm_core(), true).await?;
    world.restart().await?;
    let request = publish_process(&world, "500ms")
        .await?
        .with_host_start_key(format!("crash-matrix-process-start-{seed}"));
    let env_spec = request.env_spec.clone();
    let registration = request.into_registration(None);
    let registry = world.backend().process_registry();
    let env_store = world.backend().process_env_store();
    let starter =
        lash_core::ExecutionScope::runtime_operation(format!("crash-matrix-process-start-{seed}"))
            .journal_identity()
            .map_err(|error| format!("start journal identity: {error}"))?;
    let started = lash_core::runtime::register_process_start(
        &lash_core::runtime::ProcessStartStores {
            registry: registry.as_ref(),
            env_store: Some(&env_store),
            engines: None,
            engines_required: false,
            executor: "process start crash matrix",
            starter: &starter,
        },
        registration,
        &[],
        env_spec.as_ref(),
    )
    .await
    .map_err(|error| format!("register before the crash: {error}"))?;
    let process = started.record.id;
    let origin_ms = world.now_ms();
    world.crash_and_restart().await?;
    let expected = Expected {
        custom: vec![(
            "process_start",
            Arc::new(move |world: &CrashWorld| {
                let process = process.clone();
                Box::pin(async move {
                    match world
                        .backend()
                        .process_registry()
                        .get_process(&process)
                        .await
                    {
                        Ok(Some(record)) if record.first_started.is_some() => Vec::new(),
                        Ok(Some(_)) => vec![format!("process `{process}` has not started")],
                        Ok(None) => vec![format!("process `{process}` disappeared")],
                        Err(error) => vec![format!("read process `{process}`: {error}")],
                    }
                })
            }),
        )],
        ..Expected::default()
    };
    Ok(Staged {
        world,
        notes: Vec::new(),
        expected,
        origin_ms: Some(origin_ms),
    })
}

/// The journal commands of the process workflow's run the mid-journal cut
/// draws from: after the input command (0) come the segment admission and
/// start, the sleep's frontier, call and timer, the terminal write, the
/// parent-end step, the two promise completions, the peek that proves the
/// terminal promise stored, the publication's settle step and the output —
/// 14 more.
const RUN_JOURNAL_CUTS: u64 = 14;

/// The run step after the terminal write and before the terminal promise is
/// resolved: the window S-14 names.
const AFTER_TERMINAL_WRITE: &str = "lash.process.parent-end";

pub(crate) fn model_spec() -> Result<lash_core::ModelSpec, String> {
    lash_core::ModelSpec::builder("crash-matrix-model")
        .context_window_tokens(200_000)
        .build()
        .map_err(|error| format!("model spec: {error}"))
}

/// An RLM core: the process runs no model; the provider only has to exist.
fn rlm_core() -> CoreBuild {
    Arc::new(|backend, owner| {
        let provider = lash_core::testing::TestProvider::builder()
            .kind("crash-matrix-process")
            .complete(|_request: LlmRequest| async move {
                Ok::<_, LlmTransportError>(LlmResponse::default())
            })
            .build()
            .into_handle();
        let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
            lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                .channel(lash_protocol_rlm::RlmChannel::Cell)
                .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                .build(),
            &backend,
        );
        lash::LashCore::rlm_builder(backend, lash::TurnBudget::Unbounded, factory)
            .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
            .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
            .recovery_lease(super::recovery_lease())
            .provider(provider)
            .model(model_spec()?)
            .build(owner)
            .map_err(|error| format!("build the lash core: {error}"))
    })
}

/// Publish `process main() { sleep for <sleep>; finish 7 }` and answer its
/// start request.
pub(crate) async fn publish_process(
    world: &CrashWorld,
    sleep: &str,
) -> Result<lash_core::ProcessStartRequest, String> {
    let program = b::module(
        vec![b::process(
            PROCESS,
            Vec::new(),
            b::block(vec![b::sleep_for(b::string(sleep)), b::finish(b::num(7.0))]),
        )],
        Vec::new(),
    );
    let linked = lashlang::LinkedModule::link(
        program,
        lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            lashlang::LashlangAbilities::default().with_sleep(),
        ),
    )
    .map_err(|error| format!("link the process: {error:?}"))?;
    lashlang::LashlangArtifacts::new(world.backend().module_artifacts())
        .publish_module_artifact(
            &lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                lash_core::HostArtifactPin::mint(),
            ))
            .map_err(|error| format!("host pin claim: {error}"))?,
            &linked.artifact,
        )
        .await
        .map_err(|error| format!("publish the process artifact: {error}"))?;
    let process_ref = linked
        .artifact
        .process_ref(PROCESS)
        .ok_or_else(|| "the process has no ref".to_owned())?
        .clone();
    let input = lash_lashlang_runtime::LashlangProcessInput {
        module_ref: linked.artifact.module_ref().clone(),
        process_ref,
        host_requirements_ref: linked.artifact.host_requirements_ref().clone(),
        process_name: PROCESS.to_owned(),
        args: serde_json::Map::new(),
    }
    .into_process_input()
    .map_err(|error| format!("the process input: {error}"))?;
    Ok(lash_core::ProcessStartRequest::new(
        input,
        lash_core::ProcessOriginator::host(),
        lash_core::Lifetime::Detached,
    )
    .with_env_spec(lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::default(),
        lash_core::SessionPolicy {
            model: model_spec()?,
            ..lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded)
        },
    ))
    .with_extra_event_types(lash_lashlang_runtime::lashlang_process_event_types()))
}

/// Start `request` from a handler of the host's own, as a deployment's start
/// endpoint does.
async fn start_process(
    world: &CrashWorld,
    request: lash_core::ProcessStartRequest,
) -> Result<ProcessId, String> {
    let core = world.core()?;
    let restate = world.engine().clone();
    let requested = request.originator.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let started = world
        .host_op(async move {
            restate
                .run_in_handler(
                    lash_core::AdmittedScope::runtime_operation("crash-matrix-start".to_owned()),
                    Arc::new(move |scoped| {
                        let core = core.clone();
                        let request = request.clone();
                        let tx = tx.clone();
                        Box::pin(async move {
                            let _ = tx.send(
                                core.processes()
                                    .start(request, scoped)
                                    .await
                                    .map_err(|error| error.to_string()),
                            );
                        })
                    }),
                )
                .await
        })
        .await;
    match rx.try_recv() {
        Ok(Ok(receipt)) => {
            world.record_start_answered(&requested, &receipt).await;
            Ok(receipt.process_id)
        }
        Ok(Err(error)) => Err(error),
        Err(_) => Err(format!("the start did not answer: {started:?}")),
    }
}

/// The engine waiter's end: its invocation completed, answered by the
/// terminal promise.
pub(crate) fn waiter_completed(waiter: String, process: ProcessId) -> CustomCheck {
    Arc::new(move |world: &CrashWorld| {
        let waiter = waiter.clone();
        let process = process.clone();
        Box::pin(async move {
            let mut violations = Vec::new();
            match world
                .backend()
                .process_registry()
                .get_process(&process)
                .await
            {
                Ok(Some(record)) if record.is_terminal() => {}
                Ok(Some(record)) => violations.push(format!(
                    "process `{process}` is {:?}, not terminal",
                    record.status
                )),
                Ok(None) => violations.push(format!("process `{process}` is gone")),
                Err(error) => violations.push(format!("read process `{process}`: {error}")),
            }
            match world.engine().outcome(&waiter).await {
                Some(Ok(())) => {}
                Some(Err(failure)) => violations.push(format!(
                    "the engine waiter on `{process}`'s terminal failed: {failure}"
                )),
                None => violations.push(format!(
                    "the engine waiter on `{process}`'s terminal is stranded: its promise was never resolved"
                )),
            }
            if !violations.is_empty() {
                let workflow = format!("{PROCESS_WORKFLOW}/{process}/");
                for view in world.invocations().await {
                    if view.target.starts_with(&workflow) {
                        let outcome = world.engine().outcome(&view.id).await;
                        violations.push(format!(
                            "engine: {} {} attempts={} last_failure={:?} outcome={outcome:?}",
                            view.target, view.status, view.attempts, view.last_failure
                        ));
                    }
                }
            }
            violations
        })
    })
}

/// The process ended `Abandoned` because lash refused to resume it:
/// `ResumeRefused { SubstrateLost }`.
fn ended_substrate_lost(process: ProcessId) -> CustomCheck {
    Arc::new(move |world: &CrashWorld| {
        let process = process.clone();
        Box::pin(async move {
            let outcome = match world
                .backend()
                .process_registry()
                .get_process(&process)
                .await
            {
                Ok(record) => record.and_then(|record| record.outcome),
                Err(error) => return vec![format!("read process `{process}`: {error}")],
            };
            match outcome {
                Some(lash_core::ProcessAwaitOutput::Abandoned { evidence, .. })
                    if evidence.writer
                        == (lash_core::AbandonWriter::ResumeRefused {
                            reason: lash_core::ProcessResumeRefusal::SubstrateLost,
                        }) =>
                {
                    Vec::new()
                }
                Some(other) => vec![format!(
                    "process `{process}` ended {other:?}, not substrate-lost"
                )],
                None => vec![format!("process `{process}` has no terminal")],
            }
        })
    })
}

/// Wait, at most `budget` of wall time, until `process` recorded its start.
async fn wait_started(
    world: &CrashWorld,
    process: &ProcessId,
    budget: std::time::Duration,
) -> Result<(), String> {
    let registry = world.backend().process_registry();
    tokio::time::timeout(budget, async {
        loop {
            if let Ok(Some(record)) = registry.get_process(process).await
                && record.first_started.is_some()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await
    .map_err(|_| format!("process `{process}`'s run never started"))
}

/// [`CrashPoint::CallerKilled`]: see the module documentation.
async fn stage_caller_killed(world: CrashWorld) -> Result<Staged, String> {
    // Long enough that the run is still asleep when the kill lands, on the
    // live server's wall clock too.
    let request = publish_process(&world, "1h").await?;
    let core = world.core()?;
    let engine = world.engine().clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let _job = world.spawn_host(async move {
        engine
            .run_in_handler(
                lash_core::AdmittedScope::runtime_operation("crash-matrix-start".to_owned()),
                Arc::new(move |scoped| {
                    let core = core.clone();
                    let request = request.clone();
                    let tx = tx.clone();
                    Box::pin(async move {
                        let _ = tx.send(
                            core.processes()
                                .start(request, scoped)
                                .await
                                .map(|receipt| receipt.process_id)
                                .map_err(|error| error.to_string()),
                        );
                        // The job goes on with other work, as a host's does,
                        // until the operator kills it.
                        std::future::pending::<()>().await;
                    })
                }),
            )
            .await
    });
    let process = tokio::time::timeout(std::time::Duration::from_secs(20), rx.recv())
        .await
        .map_err(|_| "the start did not answer".to_owned())?
        .ok_or_else(|| "the start's job ended without answering".to_owned())??;
    let waiter = world
        .engine()
        .ingress()
        .send_workflow_json(
            PROCESS_WORKFLOW,
            process.as_str(),
            "await_terminal",
            &lash_restate::Call::new(lash_restate::RestateProcessAwaitRequest {
                process_id: process.clone(),
            }),
        )
        .await
        .map_err(|error| format!("arm the engine waiter: {error}"))?
        .into_string();
    wait_started(&world, &process, std::time::Duration::from_secs(20)).await?;
    let mut killed = 0;
    for view in world.invocations().await {
        if view
            .target
            .starts_with(crate::crash_matrix::engine::HANDLER_HOST)
            && view.status != "completed"
        {
            world.kill_invocation(&view.id).await?;
            killed += 1;
        }
    }
    if killed == 0 {
        return Err("no open host job to kill".to_owned());
    }
    let origin_ms = Some(world.now_ms());
    let expected = Expected {
        custom: vec![
            (
                "process_terminal",
                waiter_completed(waiter, process.clone()),
            ),
            ("substrate_lost", ended_substrate_lost(process)),
        ],
        ..Expected::default()
    };
    Ok(Staged {
        world,
        notes: Vec::new(),
        expected,
        origin_ms,
    })
}

pub(super) async fn stage(point: CrashPoint, seed: u64) -> Result<Staged, String> {
    let world = CrashWorld::new(seed, rlm_core(), true).await?;
    world.restart().await?;
    if point == CrashPoint::CallerKilled {
        return Box::pin(stage_caller_killed(world)).await;
    }
    let request = publish_process(&world, "500ms").await?;
    let mut notes = Vec::new();
    match point {
        CrashPoint::MidJournalStep => {
            let index = 1 + world.draw(0..RUN_JOURNAL_CUTS) as usize;
            notes.push(format!("cut=command {index}"));
            world.crash_on(
                CrashRule::new(EngineCut::BeforeCommand { index })
                    .service(PROCESS_WORKFLOW)
                    .handler("run")
                    .within_attempts(1),
            );
        }
        CrashPoint::InvocationLost => {
            world.crash_on(
                CrashRule::new(EngineCut::BeforeRun {
                    name: AFTER_TERMINAL_WRITE.to_owned(),
                })
                .service(PROCESS_WORKFLOW)
                .handler("run")
                .within_attempts(1),
            );
        }
        other => return Err(format!("process terminal has no {other:?} cell")),
    }
    // The process's workflow waits until the start answered and the waiter
    // is armed: a live server dispatches the run the moment the start
    // journals it, and a cut the run reached first would kill the host
    // inside the start, whose job dies with it and takes the run it called
    // along.
    let hold = world.hold_service(PROCESS_WORKFLOW).await;
    let process = start_process(&world, request).await?;
    let waiter = world
        .engine()
        .ingress()
        .send_workflow_json(
            PROCESS_WORKFLOW,
            process.as_str(),
            "await_terminal",
            &lash_restate::Call::new(lash_restate::RestateProcessAwaitRequest {
                process_id: process.clone(),
            }),
        )
        .await
        .map_err(|error| format!("arm the engine waiter: {error}"))?
        .into_string();
    hold.release();
    let origin_ms = match world.trip().wait(std::time::Duration::from_secs(20)).await {
        Some(tripped) => {
            if point == CrashPoint::InvocationLost {
                let run = format!("{PROCESS_WORKFLOW}/{process}");
                for view in world.invocations().await {
                    if view.target.starts_with(&run)
                        && view.target.ends_with("/run")
                        && view.status != "completed"
                    {
                        world.kill_invocation(&view.id).await?;
                    }
                }
            }
            world.crash_and_restart().await?;
            Some(tripped.at_ms)
        }
        None => None,
    };
    let expected = Expected {
        custom: vec![("process_terminal", waiter_completed(waiter, process))],
        ..Expected::default()
    };
    Ok(Staged {
        world,
        notes,
        expected,
        origin_ms,
    })
}
