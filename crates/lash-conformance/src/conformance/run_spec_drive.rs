//! A root runs under the run spec its inputs carry (FIG-3838).
//!
//! The drive resolves a root's spec once, against the root's snapshot after
//! the boundary's command drain, and records the result. A next-turn claim
//! never mixes specs, so the admission order of ADR 0101 splits `A, A, B, A`
//! into three roots. A spec's overrides shape its own root only: the sticky
//! session config never takes them. A replay reads the record back instead
//! of resolving again, and a definition this worker does not register ends
//! the attempt unrecorded until a deployment serves it.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_core::engine::{DriveOutcome, DriveStop, RootOutcome};
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;

use super::drive_admission::{DriveParts, on_tier};
use crate::admit;

/// The model the law sessions start on.
const SESSION_MODEL: &str = "mock-model";
/// The model a config command moves a session to.
const COMMANDED_MODEL: &str = "run-spec-commanded-model";
/// The model a spec pins its root to.
const PINNED_MODEL: &str = "run-spec-pinned-model";
/// The model a later deployment's definition would pick.
const REDEPLOYED_MODEL: &str = "run-spec-redeployed-model";
/// Guidance only a spec's prompt layer carries.
const PINNED_GUIDANCE: &str = "run-spec pinned guidance";

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: a literal model spec always builds"
)]
fn model(id: &str) -> crate::ModelSpec {
    crate::ModelSpec::builder(id)
        .context_window_tokens(200_000)
        .build()
        .expect("the law's model spec builds")
}

/// A spec pinning its root to [`PINNED_MODEL`] with [`PINNED_GUIDANCE`].
fn pinned_spec() -> crate::RunSpec {
    crate::RunSpec::overrides(crate::RunOverrides {
        model: Some(model(PINNED_MODEL)),
        prompt: Some(crate::PromptLayer::new().with_contribution(
            crate::PromptContribution::guidance("Pinned", PINNED_GUIDANCE),
        )),
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
    ) -> Result<crate::RunOverrides, crate::RunShapeError> {
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
fn record_models(parts: &mut DriveParts) -> Arc<std::sync::Mutex<Vec<String>>> {
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
                    .push(request.model.clone());
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
    parts.host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(provider.into_handle()));
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
async fn enqueue(
    parts: &DriveParts,
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

/// Drive the law's session to a stop on the tier.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each drive runs to a stop"
)]
async fn drive(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &DriveParts,
    id: &str,
) -> DriveOutcome {
    let request = parts.request(id);
    on_tier(runner, parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            lash_core::drive::drive_session(&mut runtime, &scope, &request)
                .await
                .expect("the drive runs")
        })
    })
    .await
}

fn committed_roots(outcome: &DriveOutcome) -> Vec<String> {
    outcome
        .ran
        .iter()
        .map(|run| match run {
            RootOutcome::Committed { root, .. } => root.to_string(),
            other => panic!("every root commits: {other:?}"),
        })
        .collect()
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the law's session committed"
)]
async fn head_config(parts: &DriveParts) -> crate::PersistedSessionConfig {
    parts
        .store
        .load_session_head_meta()
        .await
        .expect("read the head")
        .expect("the session committed")
        .config
}

/// Exact spec equality is a constraint inside ADR 0101's selector: with the
/// prefix `A, A, B, A` pending and a permissive bound, one drive runs three
/// roots in admission order, `[A, A]`, `[B]`, `[A]`, never reordering `A`
/// past `B`. Four input identities, three roots, each on its own shape.
pub async fn run_specs_split_roots_in_admission_order(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "run-spec-selector", &effect_host, &stores, 8).await;
    let models = record_models(&mut parts);
    let mut inputs = Vec::new();
    for (key, spec) in [
        ("selector-a1", pinned_spec()),
        ("selector-a2", pinned_spec()),
        ("selector-b", crate::RunSpec::default()),
        ("selector-a3", pinned_spec()),
    ] {
        inputs.push(enqueue(&parts, key, key, spec).await);
    }
    let outcome = drive(&runner, &parts, "run-spec-selector-drive").await;
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(
        committed_roots(&outcome),
        vec!["selector-a1", "selector-b", "selector-a3"],
        "three roots in admission order: {outcome:?}"
    );
    let root = |key: &str| TurnId::from(key);
    assert_eq!(
        parts.applications().await,
        vec![
            (inputs[0].clone(), root("selector-a1")),
            (inputs[1].clone(), root("selector-a1")),
            (inputs[2].clone(), root("selector-b")),
            (inputs[3].clone(), root("selector-a3")),
        ],
        "four inputs, each applied once to the root of its own shape"
    );
    assert_eq!(
        recorded(&models),
        vec![PINNED_MODEL, SESSION_MODEL, PINNED_MODEL],
        "each root ran on its own spec's shape"
    );
    assert_eq!(
        head_config(&parts).await.model.id,
        SESSION_MODEL,
        "the pinned roots left the session's model alone"
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

/// The default spec is the root's snapshot after the boundary's command
/// drain (ADR 0101 §4), and overrides never reach the sticky config.
///
/// An input accepted before a config command still runs on the commanded
/// model: the command lane drains first. A pinned root then runs on its own
/// model and prompt, and the default root after it is back on the commanded
/// model, with the head never having taken the pin.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn the_default_spec_is_the_snapshot_after_the_command_drain(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "run-spec-default", &effect_host, &stores, 8).await;
    let models = record_models(&mut parts);
    enqueue(
        &parts,
        "before the command",
        "default-before",
        crate::RunSpec::default(),
    )
    .await;
    let request = parts.request("run-spec-default-drive");
    let first: DriveOutcome = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            runtime
                .submit_session_command(
                    crate::SessionCommand::ApplyConfigPatch {
                        patch: Box::new(crate::ApplyConfigPatch {
                            model: Some(model(COMMANDED_MODEL)),
                            ..crate::ApplyConfigPatch::default()
                        }),
                    },
                    "run-spec-default-command",
                )
                .await
                .expect("the config command is accepted");
            lash_core::drive::drive_session(&mut runtime, &scope, &request)
                .await
                .expect("the drive runs")
        })
    })
    .await;
    // The pending command makes the root a queued run that drains the
    // command lane first and then answers the input.
    assert_eq!(
        committed_roots(&first).len(),
        1,
        "one root drains the command and answers the input: {first:?}"
    );
    assert_eq!(
        recorded(&models),
        vec![COMMANDED_MODEL],
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
    let second = drive(&runner, &parts, "run-spec-default-drive-2").await;
    assert_eq!(
        committed_roots(&second),
        vec!["default-pinned", "default-after"]
    );
    assert_eq!(
        recorded(&models),
        vec![COMMANDED_MODEL, PINNED_MODEL, COMMANDED_MODEL],
        "the pin shaped its own root only"
    );
    let head = head_config(&parts).await;
    assert_eq!(
        head.model.id, COMMANDED_MODEL,
        "the sticky model is the command's"
    );
    assert!(
        !format!("{:?}", head.prompt).contains(PINNED_GUIDANCE),
        "the pinned prompt never reached the sticky config: {:?}",
        head.prompt
    );
}

/// Crashes a root's execution after its shape is recorded and before its
/// model call.
struct CrashBeforeModelCall;

impl lash_core::runtime::RuntimeTurnPhaseProbe for CrashBeforeModelCall {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PromptBuild {
            panic!("injected crash after the root's resolution and before its model call");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

/// A root resolves its spec once, even across a crash: its redrive reads the
/// recorded shape back. The redriving worker's deployment registers the same
/// definition revision resolving to another model; the redrive never asks
/// it, and the root's one model call names the recorded model.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_root_resolves_its_spec_once_across_a_crash(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "run-spec-once", &effect_host, &stores, 8).await;
    let models = record_models(&mut parts);
    let first_resolutions = Arc::new(AtomicUsize::new(0));
    let redrive_resolutions = Arc::new(AtomicUsize::new(0));
    let input = enqueue(
        &parts,
        "resolve once",
        "once-root",
        definition_spec("run-spec-once"),
    )
    .await;
    let mut first = parts.clone();
    first.host.providers.run_definitions = definitions_with(CountingDefinition {
        name: "run-spec-once",
        model: PINNED_MODEL,
        resolved: Arc::clone(&first_resolutions),
    });
    let mut redeployed = parts.clone();
    redeployed.host.providers.run_definitions = definitions_with(CountingDefinition {
        name: "run-spec-once",
        model: REDEPLOYED_MODEL,
        resolved: Arc::clone(&redrive_resolutions),
    });
    let request = parts.request("run-spec-once-drive");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<DriveOutcome>();
    let attempt = |parts: DriveParts, crash: bool| -> crate::ConformanceTurnAttempt {
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
                let outcome = lash_core::drive::drive_session(&mut runtime, &scope, &request)
                    .await
                    .expect("the redriven drive runs");
                assert!(!crash, "the crash fires before the root's model call");
                let _ = tx.send(outcome);
                crate::ConformanceTurnEnd::Settled
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(
                &parts.session_id,
                TurnId::from("drive-law-driver"),
            )),
            attempt(first, true),
            attempt(redeployed, false),
        )
        .await;
    let outcome = rx.recv().await.expect("the redrive ran the drive");
    assert_eq!(committed_roots(&outcome), vec!["once-root"]);
    assert_eq!(
        first_resolutions.load(Ordering::SeqCst),
        1,
        "the first execution resolved the root's spec once"
    );
    assert_eq!(
        redrive_resolutions.load(Ordering::SeqCst),
        0,
        "the redrive read the recorded shape back instead of resolving again"
    );
    assert_eq!(
        recorded(&models),
        vec![PINNED_MODEL],
        "the root's one model call ran on its recorded shape"
    );
    assert_eq!(
        parts.applications().await,
        vec![(input, TurnId::from("once-root"))],
        "the root commits once"
    );
    assert_eq!(head_config(&parts).await.model.id, SESSION_MODEL);
}

/// A definition revision this worker does not register ends the root's
/// attempt unrecorded and retryable, never as the root's outcome; once a
/// deployment serves the revision, the root resolves under it and commits
/// once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_missing_definition_retries_unrecorded_until_it_is_deployed(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "run-spec-missing", &effect_host, &stores, 8).await;
    let models = record_models(&mut parts);
    let resolutions = Arc::new(AtomicUsize::new(0));
    let input = enqueue(
        &parts,
        "wait for the deployment",
        "missing-root",
        definition_spec("run-spec-missing"),
    )
    .await;
    let mut deployed = parts.clone();
    deployed.host.providers.run_definitions = definitions_with(CountingDefinition {
        name: "run-spec-missing",
        model: PINNED_MODEL,
        resolved: Arc::clone(&resolutions),
    });
    let request = parts.request("run-spec-missing-drive");
    let attempts = Arc::new(AtomicUsize::new(0));
    let (tx, mut rx) =
        tokio::sync::mpsc::unbounded_channel::<Result<DriveOutcome, crate::RuntimeError>>();
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
                match lash_core::drive::drive_session(&mut runtime, &scope, &request).await {
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
        TurnId::from("drive-law-driver"),
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
                .committed_turn_exists(&TurnId::from("missing-root"))
                .await
                .expect("read the root's commit"),
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
        "the undeployed attempt did not end the root: {outcomes:?}"
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
        .flat_map(committed_roots)
        .collect::<Vec<_>>();
    assert_eq!(
        committed,
        vec!["missing-root"],
        "the deployed retry commits the root once: {outcomes:?}"
    );
    assert_eq!(
        resolutions.load(Ordering::SeqCst),
        1,
        "the deployed definition resolved the root once"
    );
    assert_eq!(recorded(&models), vec![PINNED_MODEL]);
    assert_eq!(
        parts.applications().await,
        vec![(input, TurnId::from("missing-root"))]
    );
    assert!(
        parts
            .store
            .load_turn_park(&parts.session_id)
            .await
            .expect("read the session's park")
            .is_none(),
        "the recovered root leaves no park"
    );
}

/// Accept `keys` as one batch of next-turn inputs under `spec` (FIG-3842),
/// answering their input ids in request order.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the session store admits the batch"
)]
async fn enqueue_batch(
    store: &Arc<dyn crate::RuntimePersistence>,
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

/// A batch shares one spec, and its roots resolve it once each: four inputs
/// sent as one batch under a definition spec, with a claim bound of two, run
/// as two roots in request order, each resolving the definition once and
/// running on its shape; the session's own model is never taken.
pub async fn a_batch_shares_one_spec_that_each_root_resolves_once(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "run-spec-batch", &effect_host, &stores, 2).await;
    let models = record_models(&mut parts);
    let resolutions = Arc::new(AtomicUsize::new(0));
    parts.host.providers.run_definitions = definitions_with(CountingDefinition {
        name: "run-spec-batch",
        model: PINNED_MODEL,
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
    let outcome = drive(&runner, &parts, "run-spec-batch-drive").await;
    assert_eq!(outcome.stop, DriveStop::Idle);
    assert_eq!(
        committed_roots(&outcome),
        vec!["batch-1", "batch-3"],
        "two roots in request order: {outcome:?}"
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
        "the shared spec resolved once per root"
    );
    assert_eq!(recorded(&models), vec![PINNED_MODEL, PINNED_MODEL]);
    assert_eq!(head_config(&parts).await.model.id, SESSION_MODEL);
}

/// A batch keeps its place in the turn lane, and the command lane still
/// drains first (ADR 0101 §4): with a single send, then a config command,
/// then a batch under a pinned spec, then another single send all pending at
/// one boundary, the command applies before any turn-lane claim, and the
/// turn lane runs in admission order: the first send, the batch as one root
/// on its own shape, the last send. Both sends run on the commanded model.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_batch_keeps_its_turn_lane_place_behind_the_command_lane(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "run-spec-batch-order", &effect_host, &stores, 8).await;
    let models = record_models(&mut parts);
    let before = enqueue(
        &parts,
        "before the command",
        "order-before",
        crate::RunSpec::default(),
    )
    .await;
    let request = parts.request("run-spec-batch-order-drive");
    let store = Arc::clone(&parts.store);
    let session_id = parts.session_id.clone();
    let (outcome, batch, after) = on_tier(&runner, &parts, move |mut runtime, scope| {
        let request = request.clone();
        let store = Arc::clone(&store);
        let session_id = session_id.clone();
        Box::pin(async move {
            runtime
                .submit_session_command(
                    crate::SessionCommand::ApplyConfigPatch {
                        patch: Box::new(crate::ApplyConfigPatch {
                            model: Some(model(COMMANDED_MODEL)),
                            ..crate::ApplyConfigPatch::default()
                        }),
                    },
                    "run-spec-batch-order-command",
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
            let outcome = lash_core::drive::drive_session(&mut runtime, &scope, &request)
                .await
                .expect("the drive runs");
            (outcome, batch, after)
        })
    })
    .await;
    assert_eq!(outcome.stop, DriveStop::Idle);
    let applications = parts.applications().await;
    let root_of = |input: &crate::InputId| {
        applications
            .iter()
            .find(|(applied, _)| applied == input)
            .map(|(_, root)| root.clone())
            .expect("every input is applied")
    };
    let roots = [&before, &batch[0], &after].map(root_of);
    assert_eq!(
        applications
            .iter()
            .map(|(input, _)| input)
            .collect::<Vec<_>>(),
        [&before, &batch[0], &batch[1], &batch[2], &after].to_vec(),
        "the turn lane applied in admission order: {applications:?}"
    );
    assert!(
        batch.iter().all(|input| root_of(input) == roots[1]),
        "the batch ran as one root: {applications:?}"
    );
    assert_eq!(
        roots[1],
        TurnId::from("order-batch-1"),
        "the batch's root is started by its first input in request order"
    );
    assert_eq!(
        committed_roots(&outcome),
        roots.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "three roots, one per shape run, in admission order: {outcome:?}"
    );
    assert_eq!(
        recorded(&models),
        vec![COMMANDED_MODEL, PINNED_MODEL, COMMANDED_MODEL],
        "the command applied before the first turn-lane claim; the batch ran on its own shape"
    );
    assert_eq!(head_config(&parts).await.model.id, COMMANDED_MODEL);
}

/// The tool whose call closes the first frame with a switch.
const SWITCH_TOOL: &str = "run_spec_switch_probe";

/// Panics as the first committed turn's delivery begins: the switch commit is
/// durable and the root has not ended.
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

fn switch_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{SWITCH_TOOL}"),
        SWITCH_TOOL,
        "A tool whose call switches the turn to a follow-on agent frame.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
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
    parts: &DriveParts,
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
            .with_store(Arc::clone(&parts.store))
            .with_queued_work(Arc::new(crate::NoSessionWork::new()))
            .build(),
    )
    .await
    .expect("build the follow-on conformance runtime")
}

/// A follow-on recovered after a crash runs under the shape its parent root
/// recorded at the switch, not the spec resolved fresh against the session's
/// current defaults (FIG-3877): the root's spec pins `PINNED_MODEL`, so the
/// follow-on's model call must name `PINNED_MODEL` too.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_recovered_follow_on_inherits_its_roots_recorded_run(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let mut parts = DriveParts::new(prefix, "run-spec-follow-on", &effect_host, &stores, 8).await;
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
                    .push(request.model.clone());
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
    parts.host.providers.provider_resolver =
        Arc::new(crate::SingleProviderResolver::new(provider.into_handle()));
    let executed = Arc::new(AtomicUsize::new(0));
    let tool: Arc<dyn crate::plugin::PluginFactory> =
        Arc::new(crate::plugin::StaticPluginFactory::new(
            "conformance-run-spec-switch-probe",
            crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(SwitchTool {
                executed: Arc::clone(&executed),
            })),
        ));
    enqueue(
        &parts,
        "switch frames, then answer",
        "follow-on-root",
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
                let drive = Box::pin(runtime.drive_next_queued_root(crate::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    scope,
                )))
                .await;
                if !report {
                    panic!(
                        "the crash probe did not fire after the switch commit: {:?}",
                        drive.map(crate::facade_support::QueuedTurnDrain::ran)
                    );
                }
                let end = crate::ConformanceTurnEnd::of(&drive);
                let _ = tx.send(drive);
                end
            })
        })
    };
    let scope = admit(crate::ExecutionScope::queue_drain(
        &parts.session_id,
        format!("{prefix}-run-spec-follow-on-drive"),
    ));
    tokio::time::timeout(
        std::time::Duration::from_secs(90),
        runner.run_crashed_then_redriven_turn(scope, attempt(true, false), attempt(false, true)),
    )
    .await
    .expect("the recovered follow-on ends (it diverged from the recorded shape)");
    let drive = rx
        .recv()
        .await
        .expect("the tier's runner ran the recovering drive")
        .unwrap_or_else(|error| panic!("the recovered follow-on replays: {error:?}"));
    let turn = drive.ran().expect("the recovery ran the root to its end");
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the root finishes in the follow-on frame: {:?}; errors: {:?}",
        turn.outcome,
        turn.errors
    );
    assert_eq!(
        turn.assistant_output.safe_text, "answered in the follow-on frame",
        "the follow-on frame answers"
    );
    assert_eq!(
        recorded(&models),
        vec![PINNED_MODEL, PINNED_MODEL],
        "the recovered follow-on ran under the shape its parent root recorded, \
         not the session's default model"
    );
    assert_eq!(
        executed.load(Ordering::SeqCst),
        1,
        "the switch tool ran once and its result is read back"
    );
    let committed = parts
        .store
        .load_session_head_meta()
        .await
        .expect("read the committed head")
        .expect("the root committed");
    assert!(
        committed.pending_follow_on.is_none(),
        "the completed follow-on cleared its fact"
    );
}
