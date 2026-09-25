//! A live store or session fault inside the journaled `SyncExecutionEnvironment`
//! or `AssistantResponseHooks` step is the attempt's fault, never the step's
//! recorded outcome (FIG-3726).
//!
//! Each law drives one effect through `execute_effect` inside a
//! `LashTestHandlerHost` handler on the in-process Restate double, with a test
//! executor whose first run returns the fault the real executor marks
//! retryable and whose next run answers the step's outcome. The contract each
//! law asserts: the local executor ran twice — the failed attempt left the
//! step's run unjournaled, so the retry ran it again — the invocation retried
//! and completed, and the run's one journaled completion carries the step's
//! outcome, not the fault. A control law per command pins the other half of
//! the seam: a deterministic outcome or failure is journaled on the first
//! attempt and never re-executed.

use super::recording_context::runtime_invocation;
use super::*;
use lash_core::RuntimeEffectControllerError;
use lash_restate_test::protocol::MessageType;

/// What a local-executor run answers on its nth execution.
type EffectAnswer =
    Arc<dyn Fn(usize) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> + Send + Sync>;

/// One journaled effect driven through a handler on the server double.
struct DrivenEffect {
    backend: lash_restate_test::RestateTestBackend,
    /// The local executor's run count: one per attempt that reached it.
    executions: Arc<AtomicUsize>,
    /// What the last `execute_effect` to return produced.
    returned: Arc<Mutex<Option<Result<(), RuntimeEffectControllerError>>>>,
}

/// The fault a store that did not answer hands the environment sync: the real
/// executor marks it `retryable_uncommitted_derivation` (FIG-3726).
fn live_store_fault() -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::from(lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::StoreCommitFailed,
        "fig3726: the environment store did not answer",
    ))
    .retryable_uncommitted_derivation()
}

/// The fault a session that died mid-derivation hands the response hooks: the
/// real executor marks it `retryable_response_derivation` (FIG-3726).
fn live_session_fault() -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::retryable_response_derivation(
        "fig3726: the session lease was lost mid-derivation",
    )
}

/// A failure nothing marked retryable: what a deterministic hook failure or a
/// recorded sync refusal produces.
fn deterministic_fault() -> RuntimeEffectControllerError {
    RuntimeEffectControllerError::new(
        lash_core::RuntimeErrorCode::RestateEffectController,
        "fig3726: deterministic failure",
    )
}

fn synced_environment() -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::SyncExecutionEnvironment {
        result: Ok(Some(lash_core::sansio::ExecutionEnvironmentSync {
            system_prompt: Arc::from("fig3726 system prompt"),
            tool_specs: Arc::new(Vec::new()),
            projector_turn_inputs: None,
        })),
        cell_replay_grammar: None,
        tool_surface: Vec::new(),
    }
}

fn hooked_response() -> RuntimeEffectOutcome {
    RuntimeEffectOutcome::AssistantResponseHooks {
        response: Box::default(),
        events: Vec::new(),
    }
}

/// Runs `envelope` inside a `LashTestHandlerHost` handler on a fresh server
/// double, `answer` deciding the local executor's nth run.
async fn drive_journaled_effect(
    envelope: RuntimeEffectEnvelope,
    answer: EffectAnswer,
) -> DrivenEffect {
    let backend = lash_restate_test::backend(0x3726, lash_restate_test::ServerConfig::default())
        .await
        .expect("build the Restate test backend");
    let admitted = lash_core::AdmittedScope::turn("session", "turn");
    let executions = Arc::new(AtomicUsize::new(0));
    let returned = Arc::new(Mutex::new(None));
    let attempt: lash_restate_test::HandlerAttempt = {
        let executions = Arc::clone(&executions);
        let returned = Arc::clone(&returned);
        Arc::new(move |scoped| {
            let envelope = envelope.clone();
            let answer = Arc::clone(&answer);
            let executions = Arc::clone(&executions);
            let returned = Arc::clone(&returned);
            Box::pin(async move {
                let result = scoped
                    .execute_effect(
                        envelope,
                        RuntimeEffectLocalExecutor::testing(move |_| {
                            let run = executions.fetch_add(1, Ordering::SeqCst) + 1;
                            let answered = answer(run);
                            async move { answered }
                        }),
                    )
                    .await;
                *returned.lock().unwrap() = Some(result.map(|_| ()));
            })
        })
    };
    backend
        .run_in_handler(admitted, attempt)
        .await
        .expect("the effect's handler completes");
    backend.server().settle().await;
    DrivenEffect {
        backend,
        executions,
        returned,
    }
}

/// The handler invocation's view and journal.
fn handler_journal(
    effect: &DrivenEffect,
) -> (
    lash_restate_test::InvocationView,
    Vec<lash_restate_test::JournalEntryView>,
) {
    let view = effect
        .backend
        .server()
        .invocations()
        .into_iter()
        .find(|view| view.target.starts_with("LashTestHandlerHost/"))
        .expect("the handler invocation ran");
    let journal = effect
        .backend
        .server()
        .journal(&view.id)
        .expect("the handler invocation has a journal");
    (view, journal)
}

/// The one journaled `ctx.run` of the driven effect: its command entry and
/// its decoded completion record.
fn journaled_run(
    journal: &[lash_restate_test::JournalEntryView],
    effect_name: &str,
) -> JournaledEffectRecord {
    let commands: Vec<_> = journal
        .iter()
        .filter(|entry| {
            entry.ty == MessageType::RunCommand && entry.name.as_deref() == Some(effect_name)
        })
        .collect();
    assert_eq!(
        commands.len(),
        1,
        "the effect's run command is journaled once: {journal:?}"
    );
    let completions: Vec<_> = journal
        .iter()
        .filter(|entry| entry.ty == MessageType::RunCompletionNotification)
        .collect();
    assert_eq!(
        completions.len(),
        1,
        "the run completes exactly once in the journal: {journal:?}"
    );
    let completion = completions[0]
        .run_completion()
        .expect("a run completion notification decodes")
        .expect("the journaled run completion is a value, not a failure");
    serde_json::from_slice(&completion).expect("the run's journaled value is an effect record")
}

/// A law's first assertion set: the step's live fault ended the attempt, the
/// retry ran the step again, and the journal holds its outcome alone.
fn assert_retried(
    effect: &DrivenEffect,
    effect_name: &str,
    outcome_is_ok: impl FnOnce(&RuntimeEffectOutcome) -> bool,
) -> Vec<lash_restate_test::JournalEntryView> {
    assert_eq!(
        effect.executions.load(Ordering::SeqCst),
        2,
        "the faulted attempt never journaled the step, so the retry ran it again"
    );
    assert!(
        effect
            .returned
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|result| result.is_ok()),
        "the retry's `execute_effect` answered the step's outcome"
    );
    let (view, journal) = handler_journal(effect);
    assert_eq!(view.status, "completed", "the invocation completed");
    assert_eq!(view.retry_count, 1, "the live fault retried the attempt");
    let JournaledEffectRecord::Recorded(recorded) = journaled_run(&journal, effect_name) else {
        panic!("the run's journaled entry is a recorded effect");
    };
    let outcome = recorded
        .outcome
        .expect("the journaled outcome is the step's, not a fault");
    assert!(
        outcome_is_ok(&outcome),
        "the journaled outcome: {outcome:?}"
    );
    journal
}

/// The fault text never reached the journal: no entry's bytes carry it.
fn assert_fault_unjournaled(journal: &[lash_restate_test::JournalEntryView], fault_text: &str) {
    assert!(
        journal.iter().all(|entry| !entry
            .payload
            .windows(fault_text.len())
            .any(|window| window == fault_text.as_bytes())),
        "the live fault was journaled: {journal:?}"
    );
}

/// A live fault of the environment sync's store read ends the attempt; the
/// retry runs the step again and journals its synced environment — the fault
/// is never the step's recorded outcome (FIG-3726).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_environment_sync_store_fault_retries_the_step_without_journaling_it() {
    let envelope = RuntimeEffectEnvelope::new(
        runtime_invocation(RuntimeEffectKind::SyncExecutionEnvironment, "fig3726-sync"),
        RuntimeEffectCommand::SyncExecutionEnvironment,
    );
    let effect = drive_journaled_effect(
        envelope,
        Arc::new(|run| {
            if run == 1 {
                Err(live_store_fault())
            } else {
                Ok(synced_environment())
            }
        }),
    )
    .await;
    let journal = assert_retried(
        &effect,
        "lash:session:turn:1:0:sync_execution_environment:fig3726-sync",
        |outcome| {
            matches!(
                outcome,
                RuntimeEffectOutcome::SyncExecutionEnvironment {
                    result: Ok(Some(_)),
                    ..
                }
            )
        },
    );
    assert_fault_unjournaled(&journal, "fig3726: the environment store did not answer");
}

/// A live fault of the session the assistant-response hooks derive over ends
/// the attempt; the retry runs the step again and journals its derived
/// response — the fault is never the step's recorded outcome (FIG-3726).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_assistant_hook_session_fault_retries_the_step_without_journaling_it() {
    let envelope = RuntimeEffectEnvelope::new(
        runtime_invocation(RuntimeEffectKind::AssistantResponseHooks, "fig3726-hooks"),
        RuntimeEffectCommand::AssistantResponseHooks {
            response: Box::default(),
            stream_hook_states: Vec::new(),
        },
    );
    let effect = drive_journaled_effect(
        envelope,
        Arc::new(|run| {
            if run == 1 {
                Err(live_session_fault())
            } else {
                Ok(hooked_response())
            }
        }),
    )
    .await;
    let journal = assert_retried(
        &effect,
        "lash:session:turn:1:0:assistant_response_hooks:fig3726-hooks",
        |outcome| matches!(outcome, RuntimeEffectOutcome::AssistantResponseHooks { .. }),
    );
    assert_fault_unjournaled(
        &journal,
        "fig3726: the session lease was lost mid-derivation",
    );
}

/// The control for the sync arm: a deterministic refusal the sync itself
/// recorded is journaled on the first attempt — no retry, no re-execution.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deterministic_environment_sync_refusal_is_the_steps_recorded_outcome() {
    let envelope = RuntimeEffectEnvelope::new(
        runtime_invocation(RuntimeEffectKind::SyncExecutionEnvironment, "fig3726-sync"),
        RuntimeEffectCommand::SyncExecutionEnvironment,
    );
    let effect = drive_journaled_effect(
        envelope,
        Arc::new(|_| {
            Ok(RuntimeEffectOutcome::SyncExecutionEnvironment {
                result: Err("fig3726: the catalog refused the sync".to_string()),
                cell_replay_grammar: None,
                tool_surface: Vec::new(),
            })
        }),
    )
    .await;
    assert_eq!(effect.executions.load(Ordering::SeqCst), 1);
    assert!(
        effect
            .returned
            .lock()
            .unwrap()
            .as_ref()
            .is_some_and(|result| result.is_ok()),
        "a journaled deterministic refusal is the step's outcome, not an attempt fault"
    );
    let (view, journal) = handler_journal(&effect);
    assert_eq!(view.status, "completed");
    assert_eq!(view.retry_count, 0, "a journaled outcome never retries");
    let JournaledEffectRecord::Recorded(recorded) = journaled_run(
        &journal,
        "lash:session:turn:1:0:sync_execution_environment:fig3726-sync",
    ) else {
        panic!("the run's journaled entry is a recorded effect");
    };
    let outcome = recorded
        .outcome
        .expect("the journaled outcome is the step's refusal");
    assert!(
        matches!(
            outcome,
            RuntimeEffectOutcome::SyncExecutionEnvironment { result: Err(_), .. }
        ),
        "the journaled outcome: {outcome:?}"
    );
}

/// The control for the hooks arm: a deterministic hook failure — a fault the
/// executor did not mark retryable — is journaled as the step's recorded
/// outcome on the first attempt, and the step never runs again.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_deterministic_assistant_hook_failure_is_the_steps_recorded_outcome() {
    let envelope = RuntimeEffectEnvelope::new(
        runtime_invocation(RuntimeEffectKind::AssistantResponseHooks, "fig3726-hooks"),
        RuntimeEffectCommand::AssistantResponseHooks {
            response: Box::default(),
            stream_hook_states: Vec::new(),
        },
    );
    let effect = drive_journaled_effect(envelope, Arc::new(|_| Err(deterministic_fault()))).await;
    assert_eq!(effect.executions.load(Ordering::SeqCst), 1);
    let returned = effect.returned.lock().unwrap();
    let error = returned
        .as_ref()
        .expect("the step returned")
        .as_ref()
        .expect_err("a deterministic failure surfaces as the journaled error");
    assert!(
        error.journaled,
        "the error the step hands back is the journaled one"
    );
    let (view, journal) = handler_journal(&effect);
    assert_eq!(view.status, "completed");
    assert_eq!(view.retry_count, 0, "a journaled failure never retries");
    let JournaledEffectRecord::Recorded(recorded) = journaled_run(
        &journal,
        "lash:session:turn:1:0:assistant_response_hooks:fig3726-hooks",
    ) else {
        panic!("the run's journaled entry is a recorded effect");
    };
    let error = recorded
        .outcome
        .expect_err("the journaled outcome is the deterministic failure");
    assert!(
        error.to_string().contains("fig3726: deterministic failure"),
        "the journaled failure: {error}"
    );
}
