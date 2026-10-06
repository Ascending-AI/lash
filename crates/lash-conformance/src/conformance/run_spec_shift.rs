//! A run executes under the run spec its inputs carry (FIG-3838).
//!
//! The shift resolves a run's spec once, against the run's snapshot after
//! the boundary's command drain, and records the result. A next-turn claim
//! never mixes specs, so the admission order of ADR 0101 splits `A, A, B, A`
//! into three runs. A spec's overrides shape its own run only: the sticky
//! session config never takes them. A replay reads the record back instead
//! of resolving again, and a definition this worker does not register ends
//! the attempt unrecorded until a deployment serves it.

use crate::ActorContext;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::engine::{RunOutcome, ShiftOutcome, ShiftStop};
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;

use super::shift_admission::{ShiftParts, on_tier};
use crate::admit;

/// The model the law sessions start on.
const SESSION_PROFILE: &str = "mock-model";
/// The model a config command moves a session to.
const COMMANDED_PROFILE: &str = "run-spec-commanded-model";
/// The model a spec pins its run to.
const PINNED_PROFILE: &str = "run-spec-pinned-model";
/// The model a later deployment's definition would pick.
const REDEPLOYED_PROFILE: &str = "run-spec-redeployed-model";
/// A generation seed only the pinned spec states.
const PINNED_SEED: i64 = 4_589;

fn model(id: &str) -> crate::LlmProfileKey {
    crate::LlmProfileKey::new(id)
}

/// The law's models: every model it names, all served by `provider`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the law's distinct literal keys always register"
)]
fn law_llm_profiles(provider: crate::ProviderHandle) -> Arc<crate::LlmProfileRegistry> {
    let registry = [
        SESSION_PROFILE,
        COMMANDED_PROFILE,
        PINNED_PROFILE,
        REDEPLOYED_PROFILE,
    ]
    .into_iter()
    .try_fold(crate::LlmProfileRegistry::new(), |registry, id| {
        registry.register(
            id,
            crate::RegisteredLlmProfile::new(
                crate::testing::test_llm_profile_metadata(id),
                provider.clone(),
            ),
        )
    })
    .expect("every law model registers once");
    Arc::new(registry)
}

/// A spec pinning its run to [`PINNED_PROFILE`] with [`PINNED_SEED`].
fn pinned_spec() -> crate::RunSpec {
    crate::RunSpec::overrides(crate::RunOverrides {
        model: Some(model(PINNED_PROFILE)),
        generation: Some(crate::GenerationOptions {
            seed: Some(PINNED_SEED),
            ..crate::GenerationOptions::default()
        }),
        ..crate::RunOverrides::default()
    })
}

/// A spec that names a definition revision and nothing else.
fn definition_spec(name: &str) -> crate::RunSpec {
    crate::RunSpec {
        definition: Some(crate::DefinitionRef::new(name, 1)),
        context: serde_json::json!({ "law": name }),
        ..crate::RunSpec::default()
    }
}

/// A definition that picks `model` and counts its resolutions.
struct CountingDefinition {
    name: &'static str,
    model: &'static str,
    resolved: Arc<AtomicUsize>,
}

impl crate::RunDefinition for CountingDefinition {
    fn reference(&self) -> crate::DefinitionRef {
        crate::DefinitionRef::new(self.name, 1)
    }

    fn resolve(
        &self,
        _snapshot: &crate::PersistedSessionConfig,
        _context: &serde_json::Value,
    ) -> Result<crate::RunOverrides, crate::RunDefinitionRefusal> {
        self.resolved.fetch_add(1, Ordering::SeqCst);
        Ok(crate::RunOverrides {
            model: Some(model(self.model)),
            ..crate::RunOverrides::default()
        })
    }
}

fn definitions_with(definition: CountingDefinition) -> crate::RunDefinitions {
    let mut definitions = crate::RunDefinitions::default();
    definitions.register(Arc::new(definition));
    definitions
}

/// Serve every model through one provider that records the model each call
/// named, answering `answer <n>` to its n-th call.
fn record_llm_profiles(parts: &mut ShiftParts) -> Arc<std::sync::Mutex<Vec<String>>> {
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let models = Arc::clone(&models);
            move |request| {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                models
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(request.model.wire_model().to_string());
                async move {
                    Ok(crate::LlmResponse {
                        parts: vec![crate::LlmOutputPart::Text {
                            text: format!("answer {}", index + 1),
                            response_meta: None,
                        }],
                        ..crate::LlmResponse::default()
                    })
                }
            }
        })
        .build();
    parts.host.providers.models = law_llm_profiles(provider.into_handle());
    models
}

fn recorded(models: &Arc<std::sync::Mutex<Vec<String>>>) -> Vec<String> {
    models
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// Accept `text` as next-turn input keyed `key`, under `spec`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the session store admits the row"
)]
pub(super) async fn enqueue(
    parts: &ShiftParts,
    text: &str,
    key: &str,
    spec: crate::RunSpec,
) -> crate::InputId {
    parts
        .store
        .enqueue_pending_turn_input(
            crate::PendingTurnInputDraft::new(
                parts.session_id.clone(),
                crate::TurnInputIngress::next_turn(),
                crate::TurnInput::text(text),
            )
            .with_source_key(key)
            .with_run_spec(spec),
        )
        .await
        .expect("accept the law's input")
        .input_id
}

/// Execute the law's session to a stop on the tier.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each shift runs to a stop"
)]
pub(super) async fn shift(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &ShiftParts,
    id: &str,
) -> ShiftOutcome {
    let request = parts.request(id);
    on_tier(runner, parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            lash_core::shift::work_session(&mut runtime, &scope, &request)
                .await
                .expect("the shift runs")
        })
    })
    .await
}

/// The runs that ran turns, in admission order. A command run applies the
/// command lane first (ADR 0101 §4) and runs no turn, so it is skipped.
pub(super) fn committed_runs(outcome: &ShiftOutcome) -> Vec<String> {
    outcome
        .ran
        .iter()
        .filter(|executed| !matches!(executed, RunOutcome::Applied { .. }))
        .map(|executed| match executed {
            RunOutcome::Committed { run, .. } => run.to_string(),
            other => panic!("every turn-lane run commits: {other:?}"),
        })
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the law's session committed"
)]
pub(super) async fn head_config(parts: &ShiftParts) -> crate::PersistedSessionConfig {
    parts
        .store
        .load_session_head_meta(&parts.session_id)
        .await
        .expect("read the head")
        .expect("the session committed")
        .config
}

/// Exact spec equality is a constraint inside ADR 0101's selector: with the
/// prefix `A, A, B, A` pending and a permissive bound, one shift runs three
/// runs in admission order, `[A, A]`, `[B]`, `[A]`, never reordering `A`
/// past `B`. Four input identities, three runs, each on its own shape.
pub async fn run_specs_split_runs_in_admission_order(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "run-spec-selector", &effect_host, &stores, 8).await;
    parts.compose_inputs();
    let models = record_llm_profiles(&mut parts);
    let mut inputs = Vec::new();
    for (key, spec) in [
        ("selector-a1", pinned_spec()),
        ("selector-a2", pinned_spec()),
        ("selector-b", crate::RunSpec::default()),
        ("selector-a3", pinned_spec()),
    ] {
        inputs.push(enqueue(&parts, key, key, spec).await);
    }
    let outcome = shift(&runner, &parts, "run-spec-selector-shift").await;
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(
        committed_runs(&outcome),
        vec!["selector-a1", "selector-b", "selector-a3"],
        "three runs in admission order: {outcome:?}"
    );
    let run = |key: &str| TurnId::fixture(key);
    assert_eq!(
        parts.applications().await,
        vec![
            (inputs[0].clone(), run("selector-a1")),
            (inputs[1].clone(), run("selector-a1")),
            (inputs[2].clone(), run("selector-b")),
            (inputs[3].clone(), run("selector-a3")),
        ],
        "four inputs, each applied once to the run of its own shape"
    );
    assert_eq!(
        recorded(&models),
        vec![PINNED_PROFILE, SESSION_PROFILE, PINNED_PROFILE],
        "each run ran on its own spec's shape"
    );
    assert_eq!(
        crate::conformance::helpers::recorded_profile_key(&head_config(&parts).await.model),
        SESSION_PROFILE,
        "the pinned runs left the session's model alone"
    );
    assert_eq!(
        inputs
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        4,
        "four distinct input identities"
    );
}

/// The default spec is the run's snapshot after the boundary's command
/// drain (ADR 0101 §4), and overrides never reach the sticky config.
///
/// An input accepted before a config command still runs on the commanded
/// model: the command lane drains first. A pinned run then runs on its own
/// model and prompt, and the default run after it is back on the commanded
/// model, with the head never having taken the pin.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn the_default_spec_is_the_snapshot_after_the_command_drain(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "run-spec-default", &effect_host, &stores, 8).await;
    let models = record_llm_profiles(&mut parts);
    enqueue(
        &parts,
        "before the command",
        "default-before",
        crate::RunSpec::default(),
    )
    .await;
    let request = parts.request("run-spec-default-shift");
    let first: ShiftOutcome = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            let revision = runtime.config_revision();
            runtime
                .submit_config_transaction(
                    "run-spec-default-command",
                    revision,
                    &crate::ConfigTransaction::of(crate::plugin::config::core::SetLlmProfile {
                        model: model(COMMANDED_PROFILE),
                    }),
                )
                .await
                .expect("the config command is accepted");
            lash_core::shift::work_session(&mut runtime, &scope, &request)
                .await
                .expect("the shift runs")
        })
    })
    .await;
    // The pending command is applied by a command run first, and then one
    // run answers the input.
    assert!(
        matches!(first.ran.first(), Some(RunOutcome::Applied { .. })),
        "the command lane applies first: {first:?}"
    );
    assert_eq!(
        committed_runs(&first).len(),
        1,
        "one run answers the input after the command: {first:?}"
    );
    assert_eq!(
        recorded(&models),
        vec![COMMANDED_PROFILE],
        "the input accepted before the command ran after its drain"
    );

    enqueue(&parts, "pinned", "default-pinned", pinned_spec()).await;
    enqueue(
        &parts,
        "after the pin",
        "default-after",
        crate::RunSpec::default(),
    )
    .await;
    let second = shift(&runner, &parts, "run-spec-default-shift-2").await;
    assert_eq!(
        committed_runs(&second),
        vec!["default-pinned", "default-after"]
    );
    assert_eq!(
        recorded(&models),
        vec![COMMANDED_PROFILE, PINNED_PROFILE, COMMANDED_PROFILE],
        "the pin shaped its own run only"
    );
    let head = head_config(&parts).await;
    assert_eq!(
        crate::conformance::helpers::recorded_profile_key(&head.model),
        COMMANDED_PROFILE,
        "the sticky model is the command's"
    );
    assert_ne!(
        head.generation.seed,
        Some(PINNED_SEED),
        "the pinned generation never reached the sticky config"
    );
}

/// The budget the law's command sets.
const COMMANDED_TURNS: usize = 7;

/// A config command applied after a pinned run, by the runtime that ran
/// the run, resolves over the sticky config (FIG-4646): the head keeps the
/// session's model and generation, takes the command's budget alone, and the
/// next default run executes on the session's model.
///
/// The runtime's head is its own after the run's commit, so nothing reloads
/// it before the command applies: the run's recorded view is still resident
/// then, and uninstalling it is what restores the sticky config.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_config_command_after_a_pinned_run_resolves_over_the_sticky_config(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts =
        ShiftParts::new(prefix, "run-spec-sticky-command", &effect_host, &stores, 8).await;
    let models = record_llm_profiles(&mut parts);
    enqueue(&parts, "pinned", "sticky-pinned", pinned_spec()).await;
    let pinned_request = parts.request("run-spec-sticky-command-pinned");
    let command_request = parts.request("run-spec-sticky-command-apply");
    let submitter = parts.clone();
    let (pinned, applied): (ShiftOutcome, ShiftOutcome) =
        on_tier(&runner, &parts, move |mut runtime, scope| {
            let pinned_request = pinned_request.clone();
            let command_request = command_request.clone();
            let submitter = submitter.clone();
            Box::pin(async move {
                let pinned = lash_core::shift::work_session(&mut runtime, &scope, &pinned_request)
                    .await
                    .expect("the pinned run's shift runs");
                // Another runtime submits the command, so the executing
                // runtime's resident state is never invalidated.
                let mut other = submitter.runtime().await;
                other
                    .reload_invalidated_resident_session_state()
                    .await
                    .expect("the submitting runtime loads the head");
                other.adopt_committed_head().await.expect("read the head");
                let revision = other.config_revision();
                other
                    .submit_config_transaction(
                        "run-spec-sticky-command",
                        revision,
                        &crate::ConfigTransaction::of(crate::plugin::config::core::SetTurnBudget {
                            turn_budget: crate::TurnBudget::bounded(COMMANDED_TURNS),
                        }),
                    )
                    .await
                    .expect("the config command is accepted");
                let applied =
                    lash_core::shift::work_session(&mut runtime, &scope, &command_request)
                        .await
                        .expect("the command's shift runs");
                (pinned, applied)
            })
        })
        .await;
    assert_eq!(committed_runs(&pinned), vec!["sticky-pinned"]);
    assert!(
        matches!(applied.ran.as_slice(), [RunOutcome::Applied { .. }]),
        "the command lane applies the command: {applied:?}"
    );
    let head = head_config(&parts).await;
    assert_eq!(
        (
            crate::conformance::helpers::recorded_profile_key(&head.model).to_string(),
            head.generation.seed,
            head.turn_budget,
            head.config_revision,
        ),
        (
            SESSION_PROFILE.to_string(),
            None,
            crate::TurnBudget::bounded(COMMANDED_TURNS),
            1,
        ),
        "the command published its budget over the sticky config, not the pinned run's view"
    );

    enqueue(
        &parts,
        "after the command",
        "sticky-after",
        crate::RunSpec::default(),
    )
    .await;
    let after = shift(&runner, &parts, "run-spec-sticky-command-after").await;
    assert_eq!(committed_runs(&after), vec!["sticky-after"]);
    assert_eq!(
        recorded(&models),
        vec![PINNED_PROFILE, SESSION_PROFILE],
        "the default run after the command runs on the session's model"
    );
}

/// Crashes a run's execution after its shape is recorded and before its
/// model call.
pub(super) struct CrashBeforeModelCall;

impl lash_core::runtime::RuntimeTurnPhaseProbe for CrashBeforeModelCall {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PromptBuild {
            panic!("injected crash after the run's resolution and before its model call");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

/// A run resolves its spec once, even across a crash: its redrive reads the
/// recorded shape back. The redriving worker's deployment registers the same
/// definition revision resolving to another model; the redrive never asks
/// it, and the run's one model call names the recorded model.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_run_resolves_its_spec_once_across_a_crash(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "run-spec-once", &effect_host, &stores, 8).await;
    let models = record_llm_profiles(&mut parts);
    let first_resolutions = Arc::new(AtomicUsize::new(0));
    let redrive_resolutions = Arc::new(AtomicUsize::new(0));
    let input = enqueue(
        &parts,
        "resolve once",
        "once-run",
        definition_spec("run-spec-once"),
    )
    .await;
    let mut first = parts.clone();
    first.host.providers.run_definitions = definitions_with(CountingDefinition {
        name: "run-spec-once",
        model: PINNED_PROFILE,
        resolved: Arc::clone(&first_resolutions),
    });
    let mut redeployed = parts.clone();
    redeployed.host.providers.run_definitions = definitions_with(CountingDefinition {
        name: "run-spec-once",
        model: REDEPLOYED_PROFILE,
        resolved: Arc::clone(&redrive_resolutions),
    });
    let request = parts.request("run-spec-once-shift");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ShiftOutcome>();
    let attempt = |parts: ShiftParts, crash: bool| -> crate::ConformanceTurnAttempt {
        let request = request.clone();
        let tx = tx.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let request = request.clone();
            let tx = tx.clone();
            Box::pin(async move {
                let mut runtime = parts.runtime().await;
                if crash {
                    runtime.set_turn_phase_probe(Arc::new(CrashBeforeModelCall));
                }
                let outcome = lash_core::shift::work_session(&mut runtime, &scope, &request)
                    .await
                    .expect("the redriven shift runs");
                assert!(!crash, "the crash fires before the run's model call");
                let _ = tx.send(outcome);
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(
                &parts.session_id,
                TurnId::from("shift-law-driver"),
            )),
            attempt(first, true),
            attempt(redeployed, false),
        )
        .await;
    let outcome = rx.recv().await.expect("the redrive ran the shift");
    assert_eq!(committed_runs(&outcome), vec!["once-run"]);
    assert_eq!(
        first_resolutions.load(Ordering::SeqCst),
        1,
        "the first execution resolved the run's spec once"
    );
    assert_eq!(
        redrive_resolutions.load(Ordering::SeqCst),
        0,
        "the redrive read the recorded shape back instead of resolving again"
    );
    assert_eq!(
        recorded(&models),
        vec![PINNED_PROFILE],
        "the run's one model call ran on its recorded shape"
    );
    assert_eq!(
        parts.applications().await,
        vec![(input, TurnId::from("once-run"))],
        "the run commits once"
    );
    assert_eq!(
        crate::conformance::helpers::recorded_profile_key(&head_config(&parts).await.model),
        SESSION_PROFILE
    );
}

/// A definition revision this worker does not register ends the run's
/// attempt unrecorded and retryable, never as the run's outcome; once a
/// deployment serves the revision, the run resolves under it and commits
/// once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_missing_definition_retries_unrecorded_until_it_is_deployed(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "run-spec-missing", &effect_host, &stores, 8).await;
    let models = record_llm_profiles(&mut parts);
    let resolutions = Arc::new(AtomicUsize::new(0));
    let input = enqueue(
        &parts,
        "wait for the deployment",
        "missing-run",
        definition_spec("run-spec-missing"),
    )
    .await;
    let mut deployed = parts.clone();
    deployed.host.providers.run_definitions = definitions_with(CountingDefinition {
        name: "run-spec-missing",
        model: PINNED_PROFILE,
        resolved: Arc::clone(&resolutions),
    });
    let request = parts.request("run-spec-missing-shift");
    let attempts = Arc::new(AtomicUsize::new(0));
    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<Result<ShiftOutcome, crate::RuntimeError>>();
    // The first attempt runs on a deployment without the definition; every
    // later one, the tier's own retry included, on one that serves it.
    let attempt: crate::ConformanceTurnAttempt = {
        let undeployed = parts.clone();
        let attempts = Arc::clone(&attempts);
        Arc::new(move |scope| {
            let parts = if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                undeployed.clone()
            } else {
                deployed.clone()
            };
            let request = request.clone();
            let tx = tx.clone();
            Box::pin(async move {
                let mut runtime = parts.runtime().await;
                match lash_core::shift::work_session(&mut runtime, &scope, &request).await {
                    Ok(outcome) => {
                        let _ = tx.send(Ok(outcome));
                        crate::ConformanceTurnEnd::Settled
                    }
                    Err(abort) => {
                        let error = abort.into_error();
                        let cause = error.turn_failure_cause();
                        let _ = tx.send(Err(error));
                        crate::ConformanceTurnEnd::Aborted(cause)
                    }
                }
            })
        })
    };
    let scope = admit(crate::ExecutionScope::turn(
        &parts.session_id,
        TurnId::from("shift-law-driver"),
    ));
    runner.run_turn(scope.clone(), Arc::clone(&attempt)).await;
    // A tier that returns the undeployed attempt's abort hands it back here;
    // an engine that retries the step itself runs the deployed attempt
    // before this run returns.
    let mut outcomes = Vec::new();
    while let Ok(outcome) = rx.try_recv() {
        outcomes.push(outcome);
    }
    if outcomes.iter().all(Result::is_err) {
        assert!(
            !parts
                .store
                .committed_turn_exists(&parts.session_id, &TurnId::from("missing-run"))
                .await
                .expect("read the run's commit"),
            "the refused attempt recorded no outcome"
        );
        assert_eq!(
            recorded(&models),
            Vec::<String>::new(),
            "no model was asked"
        );
        runner.run_turn(scope, attempt).await;
        while let Ok(outcome) = rx.try_recv() {
            outcomes.push(outcome);
        }
    }
    assert!(
        attempts.load(Ordering::SeqCst) >= 2,
        "the undeployed attempt did not end the run: {outcomes:?}"
    );
    for refused in outcomes.iter().filter_map(|outcome| outcome.as_ref().err()) {
        assert_eq!(
            refused.code,
            crate::RuntimeErrorCode::RunDefinitionUnavailable,
            "an abort names the missing definition: {refused:?}"
        );
        assert!(refused.is_retryable(), "{refused:?}");
    }
    let committed = outcomes
        .iter()
        .filter_map(|outcome| outcome.as_ref().ok())
        .flat_map(committed_runs)
        .collect::<Vec<_>>();
    assert_eq!(
        committed,
        vec!["missing-run"],
        "the deployed retry commits the run once: {outcomes:?}"
    );
    assert_eq!(
        resolutions.load(Ordering::SeqCst),
        1,
        "the deployed definition resolved the run once"
    );
    assert_eq!(recorded(&models), vec![PINNED_PROFILE]);
    assert_eq!(
        parts.applications().await,
        vec![(input, TurnId::from("missing-run"))]
    );
    assert!(
        parts
            .store
            .load_turn_park(&parts.session_id)
            .await
            .expect("read the session's park")
            .is_none(),
        "the recovered run leaves no park"
    );
}

/// Accept `keys` as one batch of next-turn inputs under `spec` (FIG-3842),
/// answering their input ids in request order.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the session store admits the batch"
)]
async fn enqueue_batch(
    store: &Arc<dyn crate::RuntimeStore>,
    session_id: &lash_sansio::SessionId,
    keys: &[&str],
    spec: &crate::RunSpec,
) -> Vec<crate::InputId> {
    let drafts = keys
        .iter()
        .map(|key| {
            crate::PendingTurnInputDraft::new(
                session_id.clone(),
                crate::TurnInputIngress::next_turn(),
                crate::TurnInput::text(*key),
            )
            .with_source_key(*key)
            .with_run_spec(spec.clone())
        })
        .collect();
    store
        .enqueue_pending_turn_inputs(
            crate::PendingTurnInputBatch::new(session_id.clone(), drafts)
                .expect("the law's batch names each key once"),
        )
        .await
        .expect("accept the law's batch")
        .into_iter()
        .map(|row| row.input_id)
        .collect()
}

/// A batch shares one spec, and its runs resolve it once each: four inputs
/// sent as one batch under a definition spec, with a claim bound of two, run
/// as two runs in request order, each resolving the definition once and
/// running on its shape; the session's own model is never taken.
pub async fn a_batch_shares_one_spec_that_each_run_resolves_once(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "run-spec-batch", &effect_host, &stores, 2).await;
    parts.compose_inputs();
    let models = record_llm_profiles(&mut parts);
    let resolutions = Arc::new(AtomicUsize::new(0));
    parts.host.providers.run_definitions = definitions_with(CountingDefinition {
        name: "run-spec-batch",
        model: PINNED_PROFILE,
        resolved: Arc::clone(&resolutions),
    });
    let keys = ["batch-1", "batch-2", "batch-3", "batch-4"];
    let inputs = enqueue_batch(
        &parts.store,
        &parts.session_id,
        &keys,
        &definition_spec("run-spec-batch"),
    )
    .await;
    let outcome = shift(&runner, &parts, "run-spec-batch-shift").await;
    assert_eq!(outcome.stop, ShiftStop::Idle);
    assert_eq!(
        committed_runs(&outcome),
        vec!["batch-1", "batch-3"],
        "two runs in request order: {outcome:?}"
    );
    assert_eq!(
        parts.applications().await,
        vec![
            (inputs[0].clone(), TurnId::from("batch-1")),
            (inputs[1].clone(), TurnId::from("batch-1")),
            (inputs[2].clone(), TurnId::from("batch-3")),
            (inputs[3].clone(), TurnId::from("batch-3")),
        ],
        "each input applied once, in request order"
    );
    assert_eq!(
        resolutions.load(Ordering::SeqCst),
        2,
        "the shared spec resolved once per run"
    );
    assert_eq!(recorded(&models), vec![PINNED_PROFILE, PINNED_PROFILE]);
    assert_eq!(
        crate::conformance::helpers::recorded_profile_key(&head_config(&parts).await.model),
        SESSION_PROFILE
    );
}

/// A batch keeps its place in the turn lane, and the command lane still
/// drains first (ADR 0101 §4): with a single send, then a config command,
/// then a batch under a pinned spec, then another single send all pending at
/// one boundary, the command applies before any turn-lane claim, and the
/// turn lane runs in admission order: the first send, the batch as one run
/// on its own shape, the last send. Both sends run on the commanded model.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_batch_keeps_its_turn_lane_place_behind_the_command_lane(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "run-spec-batch-order", &effect_host, &stores, 8).await;
    parts.compose_inputs();
    let models = record_llm_profiles(&mut parts);
    let before = enqueue(
        &parts,
        "before the command",
        "order-before",
        crate::RunSpec::default(),
    )
    .await;
    let request = parts.request("run-spec-batch-order-shift");
    let store = Arc::clone(&parts.store);
    let session_id = parts.session_id.clone();
    let (outcome, batch, after) = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        let store = Arc::clone(&store);
        let session_id = session_id.clone();
        Box::pin(async move {
            let revision = runtime.config_revision();
            runtime
                .submit_config_transaction(
                    "run-spec-batch-order-command",
                    revision,
                    &crate::ConfigTransaction::of(crate::plugin::config::core::SetLlmProfile {
                        model: model(COMMANDED_PROFILE),
                    }),
                )
                .await
                .expect("the config command is accepted");
            let batch = enqueue_batch(
                &store,
                &session_id,
                &["order-batch-1", "order-batch-2", "order-batch-3"],
                &pinned_spec(),
            )
            .await;
            let after = store
                .enqueue_pending_turn_input(
                    crate::PendingTurnInputDraft::new(
                        session_id.clone(),
                        crate::TurnInputIngress::next_turn(),
                        crate::TurnInput::text("after the batch"),
                    )
                    .with_source_key("order-after"),
                )
                .await
                .expect("accept the send after the batch")
                .input_id;
            let outcome = lash_core::shift::work_session(&mut runtime, &scope, &request)
                .await
                .expect("the shift runs");
            (outcome, batch, after)
        })
    })
    .await;
    assert_eq!(outcome.stop, ShiftStop::Idle);
    let applications = parts.applications().await;
    let run_of = |input: &crate::InputId| {
        applications
            .iter()
            .find(|(applied, _)| applied == input)
            .map(|(_, run)| run.clone())
            .expect("every input is applied")
    };
    let runs = [&before, &batch[0], &after].map(run_of);
    assert_eq!(
        applications
            .iter()
            .map(|(input, _)| input)
            .collect::<Vec<_>>(),
        [&before, &batch[0], &batch[1], &batch[2], &after].to_vec(),
        "the turn lane applied in admission order: {applications:?}"
    );
    assert!(
        batch.iter().all(|input| run_of(input) == runs[1]),
        "the batch ran as one run: {applications:?}"
    );
    assert_eq!(
        runs[1],
        TurnId::from("order-batch-1"),
        "the batch's run is started by its first input in request order"
    );
    assert_eq!(
        committed_runs(&outcome),
        runs.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "three runs, one per shape run, in admission order: {outcome:?}"
    );
    assert_eq!(
        recorded(&models),
        vec![COMMANDED_PROFILE, PINNED_PROFILE, COMMANDED_PROFILE],
        "the command applied before the first turn-lane claim; the batch ran on its own shape"
    );
    assert_eq!(
        crate::conformance::helpers::recorded_profile_key(&head_config(&parts).await.model),
        COMMANDED_PROFILE
    );
}

/// The tool whose call closes the first frame with a switch.
const SWITCH_TOOL: &str = "run_spec_switch_probe";

/// Panics as the first committed turn's delivery begins: the switch commit is
/// durable and the run has not ended.
struct PanicAfterSwitchCommit;

impl lash_core::runtime::RuntimeTurnPhaseProbe for PanicAfterSwitchCommit {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PostCommitDelivery {
            panic!("injected crash after the switched turn's commit");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

struct SwitchTool {
    executed: Arc<AtomicUsize>,
}

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn switch_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{SWITCH_TOOL}"),
        SWITCH_TOOL,
        "A tool whose call switches the turn to a follow-on agent frame.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
    .expect("valid declared tool schemas")
}

#[async_trait::async_trait]
impl crate::ToolProvider for SwitchTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![switch_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == SWITCH_TOOL).then(|| Arc::new(switch_tool().contract()))
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: non-empty frame material always derives"
    )]
    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executed.fetch_add(1, Ordering::SeqCst);
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(
            crate::ToolCallOutput::success(serde_json::json!({"switched": true})).with_control(
                crate::ToolControl::SwitchAgentFrame {
                    frame_key: crate::FrameKey::from_caller_material("run-spec-follow-on")
                        .expect("non-empty frame material derives"),
                    initial_nodes: Vec::new(),
                    task: Some("run-spec follow-on".to_string()),
                },
            ),
        ))
    }
}

/// The law's runtime plus the switch tool.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the law's runtime builds"
)]
async fn runtime_with_switch(
    parts: &ShiftParts,
    tool: Arc<dyn crate::plugin::PluginFactory>,
) -> crate::LashRuntime {
    let state = parts.initial_state();
    let policy = state.policy.clone();
    Box::pin(
        crate::LashRuntime::builder(parts.host.clone(), crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_initial_state(state)
            .with_plugin_factories(
                crate::testing::test_standard_protocol_factories()
                    .into_iter()
                    .chain([tool])
                    .collect(),
            )
            .with_store(crate::conformance::helpers::session_view(
                &parts.store,
                parts.session_id.clone(),
            ))
            .with_queued_work(Arc::new(crate::NoSessionWork::new()))
            .build(),
    )
    .await
    .expect("build the follow-on conformance runtime")
}

/// A follow-on recovered after a crash runs under the shape its parent run
/// recorded at the switch, not the spec resolved fresh against the session's
/// current defaults (FIG-3877): the run's spec pins `PINNED_PROFILE`, so the
/// follow-on's model call must name `PINNED_PROFILE` too.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_recovered_follow_on_inherits_its_runs_recorded_execution(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = ShiftParts::new(prefix, "run-spec-follow-on", &effect_host, &stores, 8).await;
    // The first frame asks the model for the switch tool; the follow-on
    // frame answers with text. Every call records the model it named.
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let models = Arc::clone(&models);
            move |request| {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                models
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(request.model.wire_model().to_string());
                async move {
                    let part = if index == 0 {
                        crate::LlmOutputPart::ToolCall {
                            call_id: "switch-call".into(),
                            tool_name: SWITCH_TOOL.into(),
                            input_json: "{}".into(),
                            replay: None,
                        }
                    } else {
                        crate::LlmOutputPart::Text {
                            text: "answered in the follow-on frame".into(),
                            response_meta: None,
                        }
                    };
                    Ok(crate::LlmResponse {
                        parts: vec![part],
                        ..crate::LlmResponse::default()
                    })
                }
            }
        })
        .build();
    parts.host.providers.models = law_llm_profiles(provider.into_handle());
    let executed = Arc::new(AtomicUsize::new(0));
    let tool: Arc<dyn crate::plugin::PluginFactory> =
        Arc::new(crate::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("conformance-run-spec-switch-probe"),
            crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(SwitchTool {
                executed: Arc::clone(&executed),
            })),
        ));
    enqueue(
        &parts,
        "switch frames, then answer",
        "follow-on-run",
        pinned_spec(),
    )
    .await;

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let attempt = |crash: bool, report: bool| -> crate::ConformanceTurnAttempt {
        let parts = parts.clone();
        let tool = Arc::clone(&tool);
        let tx = tx.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let tool = Arc::clone(&tool);
            let tx = tx.clone();
            Box::pin(async move {
                let mut runtime = runtime_with_switch(&parts, tool).await;
                if crash {
                    runtime.set_turn_phase_probe(Arc::new(PanicAfterSwitchCommit));
                }
                let shift = Box::pin(runtime.execute_next_queued_run(crate::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    scope,
                )))
                .await;
                if !report {
                    panic!(
                        "the crash probe did not fire after the switch commit: {:?}",
                        shift.map(crate::facade_support::QueuedTurnDrain::ran)
                    );
                }
                let end = crate::ConformanceTurnEnd::of(&shift);
                let _ = tx.send(shift);
                end
            })
        })
    };
    let scope = admit(crate::ExecutionScope::turn(
        &parts.session_id,
        TurnId::fixture(format!("{prefix}-run-spec-follow-on-shift")),
    ));
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        runner.run_crashed_then_redriven_turn(scope, attempt(true, false), attempt(false, true)),
    )
    .await
    .expect("the recovered follow-on ends (it diverged from the recorded shape)");
    let shift = rx
        .recv()
        .await
        .expect("the tier's runner ran the recovering shift")
        .unwrap_or_else(|error| panic!("the recovered follow-on replays: {error:?}"));
    let turn = shift.ran().expect("the recovery ran the run to its end");
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the run finishes in the follow-on frame: {:?}; errors: {:?}",
        turn.outcome,
        turn.errors
    );
    assert_eq!(
        turn.assistant_output.safe_text, "answered in the follow-on frame",
        "the follow-on frame answers"
    );
    assert_eq!(
        recorded(&models),
        vec![PINNED_PROFILE, PINNED_PROFILE],
        "the recovered follow-on ran under the shape its parent run recorded, \
         not the session's default model"
    );
    assert_eq!(
        executed.load(Ordering::SeqCst),
        1,
        "the switch tool ran once and its result is read back"
    );
    let committed = parts
        .store
        .load_session_head_meta(&parts.session_id)
        .await
        .expect("read the committed head")
        .expect("the run committed");
    assert!(
        committed.pending_follow_on.is_none(),
        "the completed follow-on cleared its fact"
    );
}

/// The follow-on recovery bound of the host that resolves the law's run.
const RESOLVED_RECOVERIES: u32 = 1;
/// The bound of the host that redrives the run's switching commit.
const REDRIVING_RECOVERIES: u32 = 5;

/// The follow-on a switch owes carries the recovery bound its run resolved
/// under (FIG-4646): a run that resolved on a host with one bound, crashed
/// before its switching commit and was redriven on a host with another
/// writes the bound it recorded, never the redriving host's.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_redriven_switch_owes_its_follow_on_under_the_bound_its_run_resolved(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts =
        ShiftParts::new(prefix, "run-spec-follow-on-bound", &effect_host, &stores, 8).await;
    // The first frame asks for the switch tool. The follow-on frame's call
    // runs after the switching commit, so it reads the fact that commit
    // wrote before it answers.
    let owed_bounds = Arc::new(std::sync::Mutex::new(Vec::new()));
    let calls = Arc::new(AtomicUsize::new(0));
    let provider = crate::testing::TestProvider::builder()
        .kind("stub")
        .complete({
            let owed_bounds = Arc::clone(&owed_bounds);
            let store = Arc::clone(&parts.store);
            let session_id = parts.session_id.clone();
            move |_request| {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                let owed_bounds = Arc::clone(&owed_bounds);
                let store = Arc::clone(&store);
                let session_id = session_id.clone();
                async move {
                    let part = if index == 0 {
                        crate::LlmOutputPart::ToolCall {
                            call_id: "switch-call".into(),
                            tool_name: SWITCH_TOOL.into(),
                            input_json: "{}".into(),
                            replay: None,
                        }
                    } else {
                        let owed = store
                            .load_session_head_meta(&session_id)
                            .await
                            .expect("read the head the switch committed")
                            .and_then(|head| head.pending_follow_on)
                            .map(|owed| owed.resolved_run.follow_on_recoveries);
                        owed_bounds
                            .lock()
                            .unwrap_or_else(std::sync::PoisonError::into_inner)
                            .push(owed);
                        crate::LlmOutputPart::Text {
                            text: "answered in the follow-on frame".into(),
                            response_meta: None,
                        }
                    };
                    Ok(crate::LlmResponse {
                        parts: vec![part],
                        ..crate::LlmResponse::default()
                    })
                }
            }
        })
        .build();
    parts.host.providers.models = law_llm_profiles(provider.into_handle());
    let tool: Arc<dyn crate::plugin::PluginFactory> =
        Arc::new(crate::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("conformance-run-spec-switch-probe"),
            crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(SwitchTool {
                executed: Arc::new(AtomicUsize::new(0)),
            })),
        ));
    enqueue(
        &parts,
        "switch frames, then answer",
        "follow-on-bound-run",
        crate::RunSpec::default(),
    )
    .await;
    let bounded = |recoveries: u32| {
        let mut parts = parts.clone();
        parts.host.durability.queued_work_batching = parts
            .host
            .durability
            .queued_work_batching
            .clone()
            .with_max_follow_on_recoveries(recoveries);
        parts
    };
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let attempt = |parts: ShiftParts, crash: bool| -> crate::ConformanceTurnAttempt {
        let tool = Arc::clone(&tool);
        let tx = tx.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let tool = Arc::clone(&tool);
            let tx = tx.clone();
            Box::pin(async move {
                let mut runtime = runtime_with_switch(&parts, tool).await;
                if crash {
                    runtime.set_turn_phase_probe(Arc::new(CrashBeforeModelCall));
                }
                let shift = Box::pin(runtime.execute_next_queued_run(crate::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    scope,
                )))
                .await;
                assert!(!crash, "the crash fires before the run's model call");
                let end = crate::ConformanceTurnEnd::of(&shift);
                let _ = tx.send(shift);
                end
            })
        })
    };
    let scope = admit(crate::ExecutionScope::turn(
        &parts.session_id,
        TurnId::fixture(format!("{prefix}-run-spec-follow-on-bound-shift")),
    ));
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        runner.run_crashed_then_redriven_turn(
            scope,
            attempt(bounded(RESOLVED_RECOVERIES), true),
            attempt(bounded(REDRIVING_RECOVERIES), false),
        ),
    )
    .await
    .expect("the redriven run ends");
    let shift = rx
        .recv()
        .await
        .expect("the tier's runner ran the redrive")
        .unwrap_or_else(|error| panic!("the redriven run executes: {error:?}"));
    let turn = shift.ran().expect("the redrive ran the run to its end");
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the run finishes in the follow-on frame: {:?}; errors: {:?}",
        turn.outcome,
        turn.errors
    );
    assert_eq!(
        *owed_bounds
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        vec![Some(RESOLVED_RECOVERIES)],
        "the switch owed its follow-on under the bound the run resolved, not the redriving \
         host's"
    );
}
