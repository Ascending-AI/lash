//! The core prompt is recorded session config (FIG-4397): a root's prompt
//! sync builds the core prompt the root recorded, never the prompt of the
//! worker that runs it.
use super::*;
use pretty_assertions::assert_eq;

/// The core prompt the root records: the crashing execution's default.
const RECORDED_CORE_PROMPT: &str = "RECORDED CORE PROMPT FOR THE ROOT";
/// The core prompt of the redeployed worker that redrives the root.
const REDEPLOYED_CORE_PROMPT: &str = "REDEPLOYED CORE PROMPT OF THE WORKER";

/// A model that answers every call and keeps each request it served.
fn capturing_model(
    requests: &Arc<std::sync::Mutex<Vec<crate::LlmRequest>>>,
) -> crate::ProviderHandle {
    let requests = Arc::clone(requests);
    crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |request| {
            let requests = Arc::clone(&requests);
            async move {
                lash_sansio::sync::MutexExt::lock_recover(&*requests).push(request);
                Ok(crate::LlmResponse {
                    parts: vec![crate::LlmOutputPart::Text {
                        text: "served".into(),
                        response_meta: None,
                    }],
                    ..crate::LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// A policy whose creation default states `core_prompt`, over the mock route.
fn policy_with_core_prompt(core_prompt: &str) -> crate::SessionPolicy {
    crate::SessionPolicy {
        core_prompt: crate::PromptLayer::new()
            .with_contribution(crate::PromptContribution::guidance("Core", core_prompt)),
        ..crate::testing::mock_session_policy()
    }
}

/// An unresolved prompt sync after a core prompt change still builds the
/// prompt the root recorded (FIG-4397, ADR 0105 §1). The root's first
/// execution records its config, core prompt included, and dies before its
/// prompt sync runs. The redrive opens the session on a worker whose creation
/// default states another core prompt, as a redeployed core would: the sync
/// reads the record.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_redrive_builds_the_core_prompt_its_root_recorded(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let parts = law_session(
        prefix,
        "recorded-core-prompt-redrive",
        &effect_host,
        &stores,
        turn_config_models(capturing_model(&requests)),
    )
    .await;
    let root = TurnId::from(format!("{prefix}-turn-config-recorded-core-prompt-root"));
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    let crashing: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        let root = root.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let root = root.clone();
            Box::pin(async move {
                let mut runtime =
                    build_runtime_under(parts, policy_with_core_prompt(RECORDED_CORE_PROMPT)).await;
                runtime.set_turn_phase_probe(Arc::new(CrashBeforeFirstModelCall));
                let _ = runtime
                    .drive_turn(
                        text_input(&root, "answer once"),
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    )
                    .await;
                panic!("the crash fires before the root's prompt sync");
            })
        })
    };
    let redriven: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        let root = root.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let root = root.clone();
            let turn_tx = turn_tx.clone();
            Box::pin(async move {
                let mut runtime =
                    build_runtime_under(parts, policy_with_core_prompt(REDEPLOYED_CORE_PROMPT))
                        .await;
                let turn = runtime
                    .drive_turn(
                        text_input(&root, "answer once"),
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    )
                    .await;
                let end = crate::ConformanceTurnEnd::of(&turn);
                let _ = turn_tx.send(turn);
                end
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, &root)),
            crashing,
            redriven,
        )
        .await;
    let turn = turn_rx
        .recv()
        .await
        .expect("the tier's runner redrove the root")
        .unwrap_or_else(|error| panic!("the redriven root runs: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the redriven root answers: {:?}",
        turn.outcome
    );
    let requests = lash_sansio::sync::MutexExt::lock_recover(&*requests).clone();
    assert_eq!(requests.len(), 1, "the root made exactly one model call");
    let instructions = requests[0].instructions.as_deref().unwrap_or_default();
    assert!(
        instructions.contains(RECORDED_CORE_PROMPT),
        "the redriven sync builds the core prompt its root recorded: {instructions}"
    );
    assert!(
        !instructions.contains(REDEPLOYED_CORE_PROMPT),
        "the redeployed worker's core prompt never reaches the root: {instructions}"
    );
}
