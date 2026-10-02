//! The session config a logical turn runs under (FIG-3600 S6, D3 §2).
//!
//! A run resolves its session config once, as a recorded step at the top of
//! the logical-turn funnel, and every physical turn of the run executes under
//! that record. A redrive replays the record instead of reading the live
//! head, so a config change that landed after the run committed never
//! reaches the run's replay, and an input that arrives after the change
//! runs under it.

use lash_core::testing::TestTurnExecution as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

use crate::admit;

/// The model the session starts on.
const FIRST_PROFILE: &str = "mock-model";
/// The model a config command moves the session to.
const SECOND_PROFILE: &str = "turn-config-second-model";

/// Everything a runtime for these laws is built from, shared by every
/// attempt so each is the same session on the same store.
#[derive(Clone)]
struct ConfigParts {
    session_id: SessionId,
    host: crate::RuntimeHostConfig,
    store: Arc<dyn crate::RuntimeStore>,
    /// The protocol the session runs: the standard fake unless the law
    /// needs another.
    protocol: Vec<Arc<dyn crate::plugin::PluginFactory>>,
    /// Plugins the law adds to the protocol.
    tools: Vec<Arc<dyn crate::plugin::PluginFactory>>,
}

async fn build_runtime(parts: ConfigParts) -> crate::LashRuntime {
    build_runtime_under(parts, crate::testing::mock_session_policy()).await
}

/// The law's runtime, opened with `policy` as its creation defaults: what a
/// session with no head yet starts from, and what its first commit records.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn build_runtime_under(
    parts: ConfigParts,
    policy: crate::SessionPolicy,
) -> crate::LashRuntime {
    Box::pin(
        crate::LashRuntime::builder(parts.host, crate::testing::runtime_lease_owner())
            .with_session_id(&parts.session_id)
            .with_policy(policy)
            .with_plugin_factories(parts.protocol.into_iter().chain(parts.tools).collect())
            .with_store(crate::conformance::helpers::session_view(
                &parts.store,
                parts.session_id.clone(),
            ))
            .with_queued_work(Arc::new(crate::NoSessionWork::new()))
            .build(),
    )
    .await
    .expect("build the turn-config conformance runtime")
}

/// The host's models for these laws: [`FIRST_PROFILE`] and [`SECOND_PROFILE`],
/// both served by `provider`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: two distinct literal keys always register"
)]
fn turn_config_llm_profiles(provider: crate::ProviderHandle) -> Arc<crate::LlmProfileRegistry> {
    Arc::new(
        crate::LlmProfileRegistry::new()
            .register(
                FIRST_PROFILE,
                crate::RegisteredLlmProfile::new(
                    crate::testing::test_llm_profile_metadata(FIRST_PROFILE),
                    provider.clone(),
                ),
            )
            .and_then(|registry| {
                registry.register(
                    SECOND_PROFILE,
                    crate::RegisteredLlmProfile::new(
                        crate::testing::test_llm_profile_metadata(SECOND_PROFILE),
                        provider,
                    ),
                )
            })
            .expect("two distinct keys register"),
    )
}

/// Move the session to [`SECOND_PROFILE`] through the command lane.
async fn command_second_profile(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &ConfigParts,
) {
    let receipt = submit_second_profile(parts, "turn-config-command").await;
    let outcome = execute_config_command(runner, parts, receipt, "turn-config-command").await;
    assert!(
        matches!(outcome, crate::ConfigTransactionOutcome::Applied { .. }),
        "the model change applies: {outcome:?}"
    );
}

/// Submit a config transaction moving the session to [`SECOND_PROFILE`] under
/// `id`, from a runtime of its own, and return once it is durable.
async fn submit_second_profile(parts: &ConfigParts, id: &str) -> crate::SessionCommandReceipt {
    submit_transaction(
        parts,
        id,
        &crate::ConfigTransaction::of(crate::plugin::config::core::SetLlmProfile {
            model: crate::LlmProfileKey::new(SECOND_PROFILE),
        }),
    )
    .await
}

/// Submit `transaction` under `id` against the session's current revision,
/// from a runtime of its own, and return once it is durable.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the transaction is admitted on a live store"
)]
async fn submit_transaction(
    parts: &ConfigParts,
    id: &str,
    transaction: &crate::ConfigTransaction,
) -> crate::SessionCommandReceipt {
    let mut runtime = build_runtime(parts.clone()).await;
    let revision = runtime.config_revision();
    runtime
        .submit_config_transaction(id, revision, transaction)
        .await
        .expect("the transaction enters the command lane")
}

/// Execute the command run that applies the config transaction `receipt`
/// names, as the tier's runner runs it, and answer how it settled.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the config command settles on a live store"
)]
async fn execute_config_command(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &ConfigParts,
    receipt: crate::SessionCommandReceipt,
    request: &'static str,
) -> crate::ConfigTransactionOutcome {
    let (settled_tx, mut settled_rx) = tokio::sync::mpsc::unbounded_channel();
    let attempt_parts = parts.clone();
    let scope = crate::ExecutionScope::session_operation(&parts.session_id, request);
    runner
        .run_turn(
            admit(scope),
            Arc::new(move |controller| {
                let parts = attempt_parts.clone();
                let settled_tx = settled_tx.clone();
                let receipt = receipt.clone();
                Box::pin(async move {
                    let mut runtime = build_runtime(parts).await;
                    runtime
                        .execute_next_run(
                            request,
                            crate::TurnOptions::new(
                                tokio_util::sync::CancellationToken::new(),
                                controller,
                            ),
                        )
                        .await
                        .expect("engine executes the config transaction");
                    let settled = runtime
                        .settle_session_command(receipt)
                        .await
                        .expect("read the config transaction's settlement");
                    let _ = settled_tx.send(settled);
                    crate::ConformanceTurnEnd::Settled
                })
            }),
        )
        .await;
    match settled_rx
        .recv()
        .await
        .expect("the tier's runner drove the config transaction")
    {
        crate::SessionCommandSettlement::Applied {
            outcome: crate::runtime::SessionCommandOutcome::ConfigTransaction { outcome },
            ..
        } => outcome,
        settlement => panic!("the config transaction settles with its outcome: {settlement:?}"),
    }
}

fn text_input(turn_id: &TurnId, text: &str) -> crate::TurnInput {
    let mut input = crate::TurnInput::text(text);
    input.trace_turn_id = Some(turn_id.clone());
    input
}

/// A model that answers `answer <n>` to its n-th call and records the model
/// every call named.
fn recording_model(
    calls: &Arc<AtomicUsize>,
    models: &Arc<std::sync::Mutex<Vec<String>>>,
) -> crate::ProviderHandle {
    let calls = Arc::clone(calls);
    let models = Arc::clone(models);
    crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |request| {
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
        })
        .build()
        .into_handle()
}

/// [`recording_model`] whose first call signals `entered` and waits for
/// `release`: the run that makes it owns the session head until released.
fn gated_recording_model(
    calls: &Arc<AtomicUsize>,
    models: &Arc<std::sync::Mutex<Vec<String>>>,
    entered: &Arc<tokio::sync::Notify>,
    release: &Arc<tokio::sync::Notify>,
) -> crate::ProviderHandle {
    let calls = Arc::clone(calls);
    let models = Arc::clone(models);
    let entered = Arc::clone(entered);
    let release = Arc::clone(release);
    crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |request| {
            let index = calls.fetch_add(1, Ordering::SeqCst);
            models
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(request.model.wire_model().to_string());
            let entered = Arc::clone(&entered);
            let release = Arc::clone(&release);
            async move {
                if index == 0 {
                    entered.notify_one();
                    release.notified().await;
                }
                Ok(crate::LlmResponse {
                    parts: vec![crate::LlmOutputPart::Text {
                        text: format!("answer {}", index + 1),
                        response_meta: None,
                    }],
                    ..crate::LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

type TurnResultTx =
    tokio::sync::mpsc::UnboundedSender<Result<crate::AssembledTurn, crate::RuntimeError>>;

/// Run A committed on the first model with its reply lost, and the model
/// change that landed after it: where both stale-fence laws start.
struct CommittedRunUnderALlmProfileChange {
    parts: ConfigParts,
    run: TurnId,
    /// The scope run A was admitted under, which its redrive runs on.
    admitted: crate::AdmittedScope,
    /// The shift fence run A's execution was sealed under. The model
    /// change's seal made it stale.
    fence: crate::store::ShiftFence,
    calls: Arc<AtomicUsize>,
    models: Arc<std::sync::Mutex<Vec<String>>>,
    /// The durable head's revision once the model change applied.
    revision_after_change: u64,
}

impl CommittedRunUnderALlmProfileChange {
    /// Run A commits on the first model and its execution dies before its
    /// reply leaves it. The session's next boundary then applies the model
    /// change, whose seal raises the shift epoch past A's fence.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn new(
        prefix: &str,
        name: &str,
        effect_host: &Arc<dyn crate::EffectHost>,
        stores: &Arc<dyn crate::StoreSet>,
        runner: &Arc<dyn crate::ConformanceTurnRunner>,
    ) -> Self {
        let calls = Arc::new(AtomicUsize::new(0));
        let models = Arc::new(std::sync::Mutex::new(Vec::new()));
        let parts = law_session(
            prefix,
            name,
            effect_host,
            stores,
            turn_config_llm_profiles(recording_model(&calls, &models)),
        )
        .await;
        let run = TurnId::fixture(format!("{prefix}-turn-config-{name}-run"));
        let crash = crate::ConformanceCrash::new();
        let crashing: crate::ConformanceTurnAttempt = {
            let parts = parts.clone();
            let run = run.clone();
            let crash = crash.clone();
            Arc::new(move |scope| {
                let parts = parts.clone();
                let run = run.clone();
                let crash = crash.clone();
                Box::pin(async move {
                    let mut runtime = build_runtime(parts).await;
                    let turn = runtime
                        .execute_turn(
                            text_input(&run, "first question"),
                            crate::TurnOptions::new(
                                tokio_util::sync::CancellationToken::new(),
                                scope,
                            ),
                        )
                        .await
                        .unwrap_or_else(|error| {
                            panic!("run A commits on the first model: {error:?}")
                        });
                    assert!(
                        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
                        "run A finishes on its first execution: {:?}",
                        turn.outcome
                    );
                    crash.fire();
                    std::future::pending().await
                })
            })
        };
        let admitted = admit(crate::ExecutionScope::turn(&parts.session_id, &run));
        runner
            .run_turn_until_crash(admitted.clone(), crashing, crash)
            .await;

        // Run A's seal is still the session's: the same admission and start
        // marker answer its stored fence and raise nothing (ADR 0105 §2).
        let sealed = parts
            .store
            .shift_epoch(&parts.session_id)
            .await
            .expect("read the shift epoch run A sealed");
        let Some(crate::store::ShiftRaise::Sealed {
            admission,
            run_start,
        }) = sealed.last_raise.as_ref()
        else {
            panic!("run A's execution sealed the epoch with its start marker: {sealed:?}");
        };
        let seal = parts
            .store
            .seal_shift_epoch(&parts.session_id, admission, sealed.epoch, run_start, None)
            .await
            .expect("read run A's fence back");
        let crate::store::ShiftEpochSeal::Sealed(fence) = seal else {
            panic!("run A's seal answers its stored fence: {seal:?}");
        };

        command_second_profile(runner, &parts).await;
        let committed = parts
            .store
            .load_session_head_meta(&parts.session_id)
            .await
            .expect("read the head after the model change")
            .expect("run A's commit and the model change are durable");
        assert_eq!(
            crate::conformance::helpers::recorded_profile_key(&committed.config.model),
            SECOND_PROFILE,
            "precondition: the model change landed on the durable head"
        );
        let epoch = parts
            .store
            .shift_epoch(&parts.session_id)
            .await
            .expect("read the shift epoch after the model change")
            .epoch;
        assert!(
            epoch > fence.epoch(),
            "precondition: the model change raised the shift epoch past run A's fence"
        );
        Self {
            parts,
            run,
            admitted,
            fence,
            calls,
            models,
            revision_after_change: committed.head_revision,
        }
    }

    /// Nothing ran or was written past the model change: run A's one model
    /// call stands alone, no park was recorded, and the durable head is the
    /// one the model change left.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the store reads its own head and park"
    )]
    async fn assert_nothing_moved(&self, what: &str) {
        assert_eq!(
            self.calls.load(Ordering::SeqCst),
            1,
            "{what} makes no new model call"
        );
        assert!(
            self.parts
                .store
                .load_turn_park(&self.parts.session_id)
                .await
                .expect("read the session's park")
                .is_none(),
            "{what} records no park"
        );
        let head = self
            .parts
            .store
            .load_session_head_meta(&self.parts.session_id)
            .await
            .expect("read the head")
            .expect("the head is durable");
        assert_eq!(
            head.head_revision, self.revision_after_change,
            "{what} writes no head"
        );
        assert_eq!(
            crate::conformance::helpers::recorded_profile_key(&head.config.model),
            SECOND_PROFILE,
            "{what} leaves the model change on the durable head"
        );
    }

    /// A run that follows runs on the model the change moved the session to.
    async fn assert_the_next_run_executes_on_the_second_profile(
        &self,
        prefix: &str,
        name: &str,
        runner: &Arc<dyn crate::ConformanceTurnRunner>,
    ) {
        let next = TurnId::fixture(format!("{prefix}-turn-config-{name}-next"));
        let next_turn = run_text_turn(
            runner,
            &self.parts,
            &next,
            "second question",
            BeforeSend::Nothing,
        )
        .await
        .unwrap_or_else(|error| panic!("the next run executes: {error:?}"));
        assert!(
            matches!(next_turn.outcome, crate::TurnOutcome::Finished(_)),
            "the next run finishes: {:?}",
            next_turn.outcome
        );
        assert_eq!(
            recorded_models(&self.models),
            vec![FIRST_PROFILE.to_string(), SECOND_PROFILE.to_string()],
            "run A's one model call named the first model, and the next run's the second"
        );
    }
}

/// A committed run redriven after a later model change answers from what it
/// stored, under the config it recorded (D3 §2.2, ADR 0105 §9).
///
/// Run A commits on the first model. Before its reply reaches anyone, the
/// execution dies and the session's next boundary applies a model change,
/// whose seal makes A's shift fence stale. The tier redrives A, and its
/// journal repeats A's commit under that stale fence. The commit is the exact
/// replay of one the store holds, so its receipt answers it: the redrive
/// returns A's committed answer with no new model call, no head write and no
/// park. A run that follows runs on the second model.
///
/// The other side of the boundary is
/// [`an_older_admission_redriven_after_a_profile_change_is_fenced_out`].
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_committed_run_redriven_after_a_profile_change_answers_from_its_receipt(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let name = "replay";
    let law =
        CommittedRunUnderALlmProfileChange::new(prefix, name, &effect_host, &stores, &runner).await;
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(
            law.admitted.clone(),
            text_attempt(&law.parts, &law.run, "first question", turn_tx),
        )
        .await;
    let turn = turn_rx
        .recv()
        .await
        .expect("the tier's runner ran the redriven run")
        .unwrap_or_else(|error| {
            panic!("the redrive of committed run A answers from its receipt: {error:?}")
        });
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the redrive of run A finishes: {:?}; errors: {:?}",
        turn.outcome,
        turn.errors
    );
    assert_eq!(
        turn.assistant_output.safe_text, "answer 1",
        "the redrive answers with what run A committed"
    );
    assert_eq!(
        crate::conformance::helpers::recorded_profile_key(&turn.state.policy.model),
        FIRST_PROFILE,
        "the redrive answers under the config run A recorded"
    );
    law.assert_nothing_moved("the redrive of a committed run")
        .await;
    law.assert_the_next_run_executes_on_the_second_profile(prefix, name, &runner)
        .await;
}

/// A later model change raises the shift epoch and fences out an older
/// admission's redrive (D24a, ADR 0105 §2 and §9, ADR 0101 §7).
///
/// Run A commits on the first model, its execution dies, and the session's
/// next boundary applies a model change. An execution of A's admission that
/// does not repeat A's stored commit, as one that lost its journal and ran
/// the run again would, presents A's stale fence with a commit the store
/// holds no receipt for. It is refused whole as a stale shift fence, which
/// the runtime classifies as a superseded commit, and the model change stands. A run that follows runs on the second model.
///
/// The other side of the boundary is
/// [`a_committed_run_redriven_after_a_profile_change_answers_from_its_receipt`].
pub async fn an_older_admission_redriven_after_a_profile_change_is_fenced_out(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let name = "fenced-out";
    let law =
        CommittedRunUnderALlmProfileChange::new(prefix, name, &effect_host, &stores, &runner).await;
    let policy = crate::testing::mock_session_policy();
    let rerun = crate::RuntimeSessionState {
        session_id: law.parts.session_id.clone(),
        policy,
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    // Run A's own final commit, with other content, and a commit of a turn
    // the store never saw: neither is the stored commit's exact replay.
    for (what, turn) in [
        ("run A's final commit run again", law.run.clone()),
        (
            "a turn run A never committed",
            TurnId::fixture(format!("{}-rerun", law.run)),
        ),
    ] {
        let mut commit = super::run_control::run_final_commit(&rerun, &law.run, &turn, 0);
        commit.shift_fence = Some(Box::new(law.fence.clone()));
        let refused = law.parts.store.commit_runtime_state(commit).await;
        let Err(refusal) = refused else {
            panic!("{what} under the older admission's fence is refused: {refused:?}");
        };
        assert!(
            matches!(
                &refusal,
                crate::StoreError::StaleShiftFence { fence_epoch, .. }
                    if *fence_epoch == law.fence.epoch()
            ),
            "{what} under the older admission's fence is refused as stale: {refusal:?}"
        );
        // The runtime ends the run on it instead of retrying a fence that can
        // never commit again (FIG-4512).
        let error =
            lash_core::testing::conformance_support::runtime_error_from_store_commit(refusal);
        assert_eq!(
            error.code,
            crate::RuntimeErrorCode::StoreCommitSuperseded,
            "{what} is a superseded commit at the runtime boundary: {error:?}"
        );
        law.assert_nothing_moved(what).await;
    }
    law.assert_the_next_run_executes_on_the_second_profile(prefix, name, &runner)
        .await;
}

/// The parts of a turn-config law's session: a host whose models serve the
/// recording model, and the session's store.
async fn law_session(
    prefix: &str,
    name: &str,
    effect_host: &Arc<dyn crate::EffectHost>,
    stores: &Arc<dyn crate::StoreSet>,
    models: Arc<dyn crate::LlmProfiles>,
) -> ConfigParts {
    law_session_created_with(prefix, name, effect_host, stores, models, Vec::new()).await
}

/// [`law_session`], except the session is created with `tools` installed:
/// the created head records the plugin configuration a creator on that
/// plugin set resolves — the protocol pointer and every installed owner's
/// namespace (FIG-4379) — so a runtime that opens it later reads exactly the
/// namespaces creation recorded (FIG-4764).
async fn law_session_created_with(
    prefix: &str,
    name: &str,
    effect_host: &Arc<dyn crate::EffectHost>,
    stores: &Arc<dyn crate::StoreSet>,
    models: Arc<dyn crate::LlmProfiles>,
    tools: Vec<Arc<dyn crate::plugin::PluginFactory>>,
) -> ConfigParts {
    law_session_recording(
        prefix,
        name,
        effect_host,
        stores,
        models,
        crate::testing::mock_session_policy(),
        tools,
    )
    .await
}

/// [`law_session`], except the created head records `policy` and the session
/// is created with `tools` installed: the session is created under the
/// policy and plugin set a creating deployment would mint, so a runtime that
/// opens it later adopts exactly the config the law means to record
/// (FIG-4553, FIG-4764).
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the law's own plugin set's creation config resolves"
)]
async fn law_session_recording(
    prefix: &str,
    name: &str,
    effect_host: &Arc<dyn crate::EffectHost>,
    stores: &Arc<dyn crate::StoreSet>,
    models: Arc<dyn crate::LlmProfiles>,
    policy: crate::SessionPolicy,
    tools: Vec<Arc<dyn crate::plugin::PluginFactory>>,
) -> ConfigParts {
    let session_id = SessionId::fixture(format!("{prefix}-turn-config-{name}-session"));
    let mut host = crate::LawBackend::over_stores(Arc::clone(stores), Arc::clone(effect_host))
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
    host.providers.models = models;
    // The created head records what a creator on the session's plugin set
    // resolves (FIG-4379). These laws run the standard fake protocol, which
    // owns no plugin configuration; the law's tools are installed at
    // creation because an owner installed only afterwards never reaches the
    // recorded head.
    let protocol = crate::testing::test_standard_protocol_factories();
    let mut config = crate::PersistedSessionConfig::from(&policy);
    config.plugin_config = crate::plugin::PluginHost::new(
        protocol
            .iter()
            .cloned()
            .chain(tools.iter().cloned())
            .collect(),
    )
    .resolve_creation_plugin_config(
        Some("test_protocol"),
        &crate::PluginOptions::default(),
        None,
        true,
        &crate::store::plugin_writers::PluginAdmission::default(),
    )
    .expect("the law's plugin set resolves its creation plugin config");
    let store =
        crate::conformance::law_session_store_with_config(stores.as_ref(), &session_id, config)
            .await;
    ConfigParts {
        session_id,
        host,
        store,
        protocol,
        tools,
    }
}

/// What one turn attempt does before it sends its input.
#[derive(Clone, Copy)]
enum BeforeSend {
    Nothing,
    /// Move the session to [`SECOND_PROFILE`] through the command lane first.
    CommandSecondModel,
}

/// Run `run` with `text` as one attempt on the tier's runner and hand back
/// how its turn returned.
async fn run_text_turn(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &ConfigParts,
    run: &TurnId,
    text: &'static str,
    before: BeforeSend,
) -> Result<crate::AssembledTurn, crate::RuntimeError> {
    if matches!(before, BeforeSend::CommandSecondModel) {
        command_second_profile(runner, parts).await;
    }
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, run)),
            text_attempt(parts, run, text, turn_tx),
        )
        .await;
    turn_rx
        .recv()
        .await
        .unwrap_or_else(|| panic!("the tier's runner ran run `{run}`"))
}

fn text_attempt(
    parts: &ConfigParts,
    run: &TurnId,
    text: &'static str,
    turn_tx: TurnResultTx,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    let run = run.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let run = run.clone();
        let turn_tx = turn_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(parts).await;
            let turn = runtime
                .execute_turn(
                    text_input(&run, text),
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                )
                .await;
            let end = crate::ConformanceTurnEnd::of(&turn);
            let _ = turn_tx.send(turn);
            end
        })
    })
}

fn recorded_models(models: &Arc<std::sync::Mutex<Vec<String>>>) -> Vec<String> {
    models
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone()
}

/// An input sent after a config command runs under the new config (D3 §3.1):
/// `command(M2)` then `send(x)` runs x on M2, and the run before the command
/// ran on M1.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_input_sent_after_a_config_command_runs_on_the_new_profile(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let parts = law_session(
        prefix,
        "after-command",
        &effect_host,
        &stores,
        turn_config_llm_profiles(recording_model(&calls, &models)),
    )
    .await;
    let first = TurnId::fixture(format!("{prefix}-turn-config-after-command-first"));
    let turn = run_text_turn(&runner, &parts, &first, "first", BeforeSend::Nothing)
        .await
        .unwrap_or_else(|error| panic!("the first run executes: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the first run finishes: {:?}",
        turn.outcome
    );
    let second = TurnId::fixture(format!("{prefix}-turn-config-after-command-second"));
    let turn = run_text_turn(
        &runner,
        &parts,
        &second,
        "second",
        BeforeSend::CommandSecondModel,
    )
    .await
    .unwrap_or_else(|error| panic!("the run sent after the command runs: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the run sent after the command finishes: {:?}",
        turn.outcome
    );
    assert_eq!(
        recorded_models(&models),
        vec![FIRST_PROFILE.to_string(), SECOND_PROFILE.to_string()],
        "the run before the command ran on the first model, the one after it on the second"
    );
    let head = parts
        .store
        .load_session_head_meta(&parts.session_id)
        .await
        .expect("read the head")
        .expect("the session committed");
    assert_eq!(
        crate::conformance::helpers::recorded_profile_key(&head.config.model),
        SECOND_PROFILE
    );
}

/// A config transaction submitted while a run owns the session head waits
/// for that run (FIG-4379): its submission completes, the run finishes
/// under the config it was admitted with, and nothing of the transaction is
/// published while the run executes or by the run's commit. Once the run
/// releases the head the command lane applies it with one revision step,
/// and the next run executes under it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_config_transaction_waits_while_a_run_owns_the_head(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let entered = Arc::new(tokio::sync::Notify::new());
    let release = Arc::new(tokio::sync::Notify::new());
    let parts = law_session(
        prefix,
        "pending-while-run",
        &effect_host,
        &stores,
        turn_config_llm_profiles(gated_recording_model(&calls, &models, &entered, &release)),
    )
    .await;
    let run = TurnId::fixture(format!("{prefix}-turn-config-pending-while-run"));
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    let submitting = async {
        entered.notified().await;
        let receipt = submit_second_profile(&parts, "pending-while-run").await;
        // The session's first run commits its head, so while it runs the
        // head is either still unwritten or the creation config.
        let head = parts
            .store
            .load_session_head_meta(&parts.session_id)
            .await
            .expect("read the head while the run executes");
        if let Some(head) = head {
            assert_eq!(
                (head.config.wire_model(), head.config.config_revision),
                (Some(FIRST_PROFILE), 0),
                "nothing is published while the run owns the head"
            );
        }
        assert!(
            parts
                .store
                .list_queued_work(&parts.session_id)
                .await
                .expect("read the command lane")
                .iter()
                .any(|batch| batch.batch_id == receipt.batch_id),
            "the submitted transaction is pending"
        );
        release.notify_one();
        receipt
    };
    let ((), receipt) = tokio::join!(
        runner.run_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, &run)),
            text_attempt(&parts, &run, "first", turn_tx),
        ),
        submitting,
    );
    let turn = turn_rx
        .recv()
        .await
        .expect("the tier's runner ran the run")
        .unwrap_or_else(|error| panic!("the run executes: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the run finishes: {:?}",
        turn.outcome
    );
    let head = parts
        .store
        .load_session_head_meta(&parts.session_id)
        .await
        .expect("read the head after the run")
        .expect("the run committed");
    assert_eq!(
        (head.config.wire_model(), head.config.config_revision),
        (Some(FIRST_PROFILE), 0),
        "the run's commit does not publish the pending transaction"
    );

    let outcome = execute_config_command(&runner, &parts, receipt, "pending-while-run-drain").await;
    assert_eq!(
        outcome,
        crate::ConfigTransactionOutcome::Applied {
            base_revision: 0,
            revision: 1,
            outputs: vec![serde_json::Value::Null],
        },
        "the lane applies the transaction once the run released the head"
    );

    let next = TurnId::fixture(format!("{prefix}-turn-config-pending-while-run-next"));
    let turn = run_text_turn(&runner, &parts, &next, "second", BeforeSend::Nothing)
        .await
        .unwrap_or_else(|error| panic!("the next run executes: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the next run finishes: {:?}",
        turn.outcome
    );
    assert_eq!(
        recorded_models(&models),
        vec![FIRST_PROFILE.to_string(), SECOND_PROFILE.to_string()],
        "the run that owned the head ran on its admitted model, the next run on the new one"
    );
}

/// The tool whose call closes the first frame with a switch, so the run
/// runs a second physical turn.
const SWITCH_TOOL: &str = "turn_config_switch_probe";

struct SwitchTool;

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
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(
            crate::ToolCallOutput::success(serde_json::json!({"switched": true})).with_control(
                crate::ToolControl::SwitchAgentFrame {
                    frame_key: crate::FrameKey::from_caller_material("turn-config-switch")
                        .expect("non-empty frame material derives"),
                    initial_nodes: Vec::new(),
                    task: Some("turn-config follow-on".to_string()),
                },
            ),
        ))
    }
}

/// One config resolution per run (D3 §2.1, Q11): a run whose first frame
/// switches runs two physical turns under one recorded config. Both model
/// calls name the same provider and model, and a tier that can read its
/// journal holds exactly one `turn-config:{run}` entry for the run.
pub async fn one_config_resolution_per_run(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let model = {
        let calls = Arc::clone(&calls);
        let models = Arc::clone(&models);
        crate::testing::TestProvider::builder()
            .kind("stub")
            .complete(move |request| {
                let index = calls.fetch_add(1, Ordering::SeqCst);
                models
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .push(request.model.wire_model().to_string());
                async move {
                    let part = if index == 0 {
                        crate::LlmOutputPart::ToolCall {
                            call_id: "turn-config-switch-call".into(),
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
            })
            .build()
            .into_handle()
    };
    let parts = law_session_created_with(
        prefix,
        "one-resolution",
        &effect_host,
        &stores,
        turn_config_llm_profiles(model),
        vec![Arc::new(crate::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("conformance-turn-config-switch-probe"),
            crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(SwitchTool)),
        ))],
    )
    .await;
    let run = TurnId::fixture(format!("{prefix}-turn-config-one-resolution-run"));
    let executed = run_text_turn(
        &runner,
        &parts,
        &run,
        "switch, then answer",
        BeforeSend::Nothing,
    )
    .await
    .unwrap_or_else(|error| panic!("the switching run executes: {error:?}"));
    assert!(
        matches!(executed.outcome, crate::TurnOutcome::Finished(_)),
        "the run finishes in its follow-on frame: {:?}; errors: {:?}",
        executed.outcome,
        executed.errors
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "precondition: the run ran two physical turns, one model call each"
    );
    assert_eq!(
        recorded_models(&models),
        vec![FIRST_PROFILE.to_string(), FIRST_PROFILE.to_string()],
        "every physical turn of the run names the one recorded model"
    );
    if let Some(keys) = runner
        .recorded_replay_keys(&crate::ExecutionScope::turn(&parts.session_id, &run))
        .await
    {
        let resolutions = keys
            .iter()
            .filter(|key| key.starts_with("turn-config:"))
            .collect::<Vec<_>>();
        assert_eq!(
            resolutions,
            vec![&format!("turn-config:{run}")],
            "the run resolved its config exactly once: {keys:?}"
        );
    }
}

/// A recorded model this worker cannot bind retries and never fails the turn
/// (D3 Q3): the run aborts retryably with nothing recorded as its outcome,
/// and once the key is served again its redrive completes it once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_unbindable_llm_profile_retries_and_never_fails_the_turn(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let served = law_session(
        prefix,
        "unbindable",
        &effect_host,
        &stores,
        turn_config_llm_profiles(recording_model(&calls, &models)),
    )
    .await;
    // The same session on a worker whose models lack the recorded key.
    let mut unserved = served.clone();
    unserved.host.providers.models = Arc::new(crate::LlmProfileRegistry::new());
    let run = TurnId::fixture(format!("{prefix}-turn-config-unbindable-run"));
    let attempts = Arc::new(AtomicUsize::new(0));
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    // The first attempt runs on the worker without the key; every later one,
    // the tier's own retry of the unsealed model call included, on the one
    // that serves it.
    let attempt: crate::ConformanceTurnAttempt = {
        let served = text_attempt(&served, &run, "hello", turn_tx.clone());
        let unserved = text_attempt(&unserved, &run, "hello", turn_tx);
        let attempts = Arc::clone(&attempts);
        Arc::new(move |scope| {
            if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                unserved(scope)
            } else {
                served(scope)
            }
        })
    };
    let scope = admit(crate::ExecutionScope::turn(&served.session_id, &run));
    runner.run_turn(scope.clone(), Arc::clone(&attempt)).await;
    // A tier that returns the unserved attempt's abort hands it back here; an
    // engine that retries the step itself runs the served attempt before
    // this run returns.
    let mut turns = Vec::new();
    while let Ok(turn) = turn_rx.try_recv() {
        turns.push(turn);
    }
    if turns.iter().all(Result::is_err) {
        assert_eq!(calls.load(Ordering::SeqCst), 0, "no model was asked");
        assert!(
            !served
                .store
                .committed_turn_exists(&served.session_id, &run)
                .await
                .expect("read the run's commit"),
            "the aborted run recorded no outcome"
        );
        assert!(
            served
                .store
                .load_turn_park(&served.session_id)
                .await
                .expect("read the session's park")
                .is_none(),
            "a retry is not a park"
        );
        runner.run_turn(scope, attempt).await;
        while let Ok(turn) = turn_rx.try_recv() {
            turns.push(turn);
        }
    }
    assert!(
        attempts.load(Ordering::SeqCst) >= 2,
        "the unserved attempt did not end the run: {turns:?}"
    );
    for aborted in turns.iter().filter_map(|turn| turn.as_ref().err()) {
        assert_eq!(
            aborted.code,
            crate::RuntimeErrorCode::LlmProfileUnavailable,
            "the abort names the unbindable model: {aborted:?}"
        );
        assert_eq!(
            aborted.profile_key(),
            Some(&crate::LlmProfileKey::new(FIRST_PROFILE)),
            "the abort carries the recorded key typed: {aborted:?}"
        );
        assert!(
            aborted.is_retryable(),
            "an unbindable model is retried, never the turn's outcome: {aborted:?}"
        );
    }
    let finished = turns
        .iter()
        .filter_map(|turn| turn.as_ref().ok())
        .collect::<Vec<_>>();
    assert_eq!(
        finished.len(),
        1,
        "the attempt with the model back completes the run once: {turns:?}"
    );
    assert!(
        matches!(finished[0].outcome, crate::TurnOutcome::Finished(_)),
        "the redrive completes the run: {:?}",
        finished[0].outcome
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the run ran once");
    assert!(
        served
            .store
            .committed_turn_exists(&served.session_id, &run)
            .await
            .expect("read the run's commit"),
        "the redrive committed the run"
    );
    assert!(
        served
            .store
            .load_turn_park(&served.session_id)
            .await
            .expect("read the session's park")
            .is_none(),
        "the recovered run leaves no park"
    );
}

/// A model change naming a key this host's models do not register is
/// refused typed by the core owner when the transaction resolves, and
/// publishes nothing (FIG-4374): its command settles with the refusal, and
/// the session keeps its recorded model and its config revision.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_unknown_profile_key_is_refused_typed_and_publishes_nothing(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let parts = law_session(
        prefix,
        "unknown-key",
        &effect_host,
        &stores,
        turn_config_llm_profiles(recording_model(&calls, &models)),
    )
    .await;
    let store = Arc::clone(&parts.store);
    let session_id = parts.session_id.clone();
    let receipt = submit_transaction(
        &parts,
        "turn-config-unknown-key",
        &crate::ConfigTransaction::of(crate::plugin::config::core::SetLlmProfile {
            model: crate::LlmProfileKey::new("turn-config-unknown-model"),
        }),
    )
    .await;
    let outcome = execute_config_command(&runner, &parts, receipt, "turn-config-unknown-key").await;
    let crate::ConfigTransactionOutcome::Refused { refusal } = outcome else {
        panic!("a key the host's models do not register settles refused: {outcome:?}");
    };
    assert_eq!(refusal.owner, crate::CORE_CONFIG_OWNER);
    assert_eq!(
        refusal
            .owner_refusal::<crate::CoreConfigRefusal>()
            .expect("the core owner's typed refusal"),
        crate::CoreConfigRefusal::UnknownLlmProfile {
            key: crate::LlmProfileKey::new("turn-config-unknown-model"),
        }
    );
    let head = store
        .load_session_head_meta(&session_id)
        .await
        .expect("read the head")
        .expect("the drain committed the session's head");
    assert_eq!(
        head.config.profile_key().map(crate::LlmProfileKey::as_str),
        Some(FIRST_PROFILE),
        "nothing was published"
    );
    assert_eq!(head.config.config_revision, 0, "the revision did not move");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model was asked");
}

/// The plugin id and config namespace of [`ShapedFactory`].
const SHAPED: &str = "conformance-shaped-config";

/// The namespace the session records under [`SHAPED`].
#[derive(
    Clone,
    Debug,
    Default,
    serde::Serialize,
    serde::Deserialize,
    lash_core::facade_support::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
struct RecordedShape {
    value: String,
}

/// A recorded type the bytes of a [`RecordedShape`] do not read as.
#[derive(
    Clone,
    Debug,
    Default,
    serde::Serialize,
    serde::Deserialize,
    lash_core::facade_support::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
struct OtherShape {
    count: u64,
}

/// The owner of the [`SHAPED`] namespace, reading it as `R`: it records
/// `R`'s default and admits every candidate.
struct ShapedOwner<R>(std::marker::PhantomData<fn() -> R>);

impl<R> crate::ConfigOwner for ShapedOwner<R>
where
    R: crate::ConfigWire + Clone + Default,
{
    type Create = R;
    type Recorded = R;
    type Refusal = String;
    type RunOptions = crate::NoRunOptions;

    fn create(
        &self,
        input: Option<R>,
        _facts: crate::CreationFacts<'_, R>,
    ) -> Result<Option<R>, String> {
        Ok(Some(input.unwrap_or_default()))
    }

    fn validate(
        &self,
        _value: &R,
        _base: Option<&R>,
        _facts: &crate::CandidateFacts<'_>,
    ) -> Result<(), String> {
        Ok(())
    }

    fn apply_run_options(&self, recorded: &R, _options: crate::NoRunOptions) -> Result<R, String> {
        Ok(recorded.clone())
    }
}

struct ShapedPlugin;

impl crate::plugin::SessionPlugin for ShapedPlugin {
    fn id(&self) -> &'static str {
        SHAPED
    }

    fn register(
        &self,
        _reg: &mut crate::plugin::PluginRegistrar,
    ) -> Result<(), crate::PluginError> {
        Ok(())
    }
}

/// A plugin whose owner reads the [`SHAPED`] namespace as `R`. Two builds
/// that install it under different `R` stand for a namespace whose stored
/// bytes its owner can no longer read.
struct ShapedFactory<R>(std::marker::PhantomData<fn() -> R>);

impl<R> ShapedFactory<R>
where
    R: crate::ConfigWire + Clone + Default,
{
    fn installed() -> Vec<Arc<dyn crate::plugin::PluginFactory>> {
        vec![Arc::new(Self(std::marker::PhantomData))]
    }
}

impl<R> crate::plugin::PluginFactory for ShapedFactory<R>
where
    R: crate::ConfigWire + Clone + Default,
{
    fn id(&self) -> &'static str {
        SHAPED
    }

    fn register_config(
        &self,
        registrar: &mut crate::ConfigRegistrar,
    ) -> Result<(), crate::ConfigRegistrationError> {
        registrar.owner(ShapedOwner::<R>(std::marker::PhantomData))
    }

    fn declaration(&self) -> crate::plugin::PluginDeclaration {
        crate::plugin::PluginDeclaration::initial(crate::plugin::PluginFactory::id(self))
    }

    fn build(
        &self,
        _ctx: &crate::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn crate::plugin::SessionPlugin>, crate::PluginError> {
        Ok(Arc::new(ShapedPlugin))
    }
}

/// A recorded namespace its owner cannot read is corruption of the
/// session's stored config, never a refusal of the transaction that met it
/// (FIG-4652): the command's resolution fails as corrupt stored data,
/// nothing settles as `Refused`, and nothing is published.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_corrupt_recorded_namespace_is_corruption_and_never_a_recorded_refusal(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    const REQUEST: &str = "turn-config-corrupt-namespace";
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let parts = law_session_created_with(
        prefix,
        "corrupt-namespace",
        &effect_host,
        &stores,
        turn_config_llm_profiles(recording_model(&calls, &models)),
        ShapedFactory::<RecordedShape>::installed(),
    )
    .await;
    // The session recorded its namespace as one shape at creation, and a
    // first command commits the head that carries it.
    command_second_profile(&runner, &parts).await;
    let recorded_head = parts
        .store
        .load_session_head_meta(&parts.session_id)
        .await
        .expect("read the head")
        .expect("the first command committed the session's head");
    assert_eq!(
        recorded_head.config.plugin_config.get(SHAPED),
        Some(&serde_json::json!({ "value": "" })),
        "the session recorded the namespace"
    );

    // An owner that reads another shape meets bytes it cannot read.
    let mut reading = parts.clone();
    reading.tools = ShapedFactory::<OtherShape>::installed();
    let receipt = submit_transaction(
        &reading,
        REQUEST,
        &crate::ConfigTransaction::of(crate::plugin::config::core::SetLlmProfile {
            model: crate::LlmProfileKey::new(FIRST_PROFILE),
        }),
    )
    .await;
    let (executed_tx, mut executed_rx) = tokio::sync::mpsc::unbounded_channel();
    let attempt_parts = reading.clone();
    runner
        .run_turn(
            admit(crate::ExecutionScope::session_operation(
                &parts.session_id,
                REQUEST,
            )),
            Arc::new(move |controller| {
                let parts = attempt_parts.clone();
                let executed_tx = executed_tx.clone();
                let receipt = receipt.clone();
                Box::pin(async move {
                    let mut runtime = build_runtime(parts).await;
                    let executed = runtime
                        .execute_next_run(
                            REQUEST,
                            crate::TurnOptions::new(
                                tokio_util::sync::CancellationToken::new(),
                                controller,
                            ),
                        )
                        .await
                        .map(drop);
                    let end = crate::ConformanceTurnEnd::of(&executed);
                    let settled = match &executed {
                        Ok(()) => Some(runtime.settle_session_command(receipt).await),
                        Err(_) => None,
                    };
                    let _ = executed_tx.send((executed, settled));
                    end
                })
            }),
        )
        .await;
    let (executed, settled) = executed_rx
        .recv()
        .await
        .expect("the tier's runner drove the config transaction");
    let error = executed.expect_err(&format!(
        "a namespace its owner cannot read resolves nothing; the command settled {settled:?}"
    ));
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::RuntimeStoreCorrupt,
        "an unreadable recorded namespace is corrupt stored data: {error:?}"
    );
    assert!(error.is_terminal(), "corruption is never retried");
    assert!(
        error.message.contains(SHAPED),
        "the error names the namespace's owner: {error:?}"
    );
    let head = parts
        .store
        .load_session_head_meta(&parts.session_id)
        .await
        .expect("read the head")
        .expect("the session's head");
    assert_eq!(
        head.config.profile_key().map(crate::LlmProfileKey::as_str),
        Some(SECOND_PROFILE),
        "nothing was published"
    );
    assert_eq!(
        head.config.config_revision, recorded_head.config.config_revision,
        "the revision did not move"
    );
}

/// A model change records the binding the host's models minted where the
/// transaction resolved (FIG-4374): the worker that submitted it need not
/// serve the key, and the session records exactly the metadata the resolving
/// worker's registry held, never re-deriving it from a later catalog.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_profile_change_records_the_binding_minted_where_it_resolves(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let session = recording_model(&calls, &models);
    // The worker that submits the change serves only the session's model.
    let sender = law_session(
        prefix,
        "minted-at-resolution",
        &effect_host,
        &stores,
        crate::testing::standard_test_llm_profiles(session.clone()),
    )
    .await;
    // The worker that resolves it serves the second model, with metadata of
    // its own.
    let resolving_metadata = crate::LlmProfileMetadata::builder(SECOND_PROFILE)
        .context_window_tokens(77_777)
        .build()
        .expect("the resolving worker's metadata");
    let mut applier = sender.clone();
    applier.host.providers.models = Arc::new(
        crate::LlmProfileRegistry::new()
            .register(
                FIRST_PROFILE,
                crate::RegisteredLlmProfile::new(
                    crate::testing::test_llm_profile_metadata(FIRST_PROFILE),
                    session.clone(),
                ),
            )
            .and_then(|registry| {
                registry.register(
                    SECOND_PROFILE,
                    crate::RegisteredLlmProfile::new(resolving_metadata.clone(), session),
                )
            })
            .expect("two distinct keys register"),
    );
    let receipt = submit_transaction(
        &sender,
        "turn-config-minted-at-resolution",
        &crate::ConfigTransaction::of(crate::plugin::config::core::SetLlmProfile {
            model: crate::LlmProfileKey::new(SECOND_PROFILE),
        }),
    )
    .await;
    let outcome = execute_config_command(
        &runner,
        &applier,
        receipt,
        "turn-config-minted-at-resolution",
    )
    .await;
    assert!(
        matches!(outcome, crate::ConfigTransactionOutcome::Applied { .. }),
        "the resolving worker mints and applies the change: {outcome:?}"
    );
    let head = sender
        .store
        .load_session_head_meta(&sender.session_id)
        .await
        .expect("read the head")
        .expect("the drain committed the session's head");
    assert_eq!(
        head.config.model,
        Some(crate::LlmProfileConfig::new(
            crate::RecordedLlmProfile::mint(
                crate::LlmProfileKey::new(SECOND_PROFILE),
                resolving_metadata,
            )
        )),
        "the session records the binding the resolving worker minted"
    );
    assert_eq!(head.config.config_revision, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model was asked");
}

/// A model that declares one configurable effort, `deep`.
const REASONING_MODEL: &str = "turn-config-reasoning-model";

/// A reasoning change is judged against the model the transaction's final
/// candidate records (FIG-4374): an effort the session's model does not
/// declare is refused typed and publishes nothing, and the same effort
/// applies in one transaction that also moves the session to a model that
/// declares it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_reasoning_change_is_judged_against_the_final_recorded_llm_profile(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = recording_model(&calls, &models);
    let reasoning_metadata = crate::testing::test_llm_profile_metadata(REASONING_MODEL)
        .with_capability(crate::LlmProfileCapability {
            reasoning: Some(crate::ReasoningCapability {
                efforts: vec!["deep".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        });
    let registry = crate::LlmProfileRegistry::new()
        .register(
            FIRST_PROFILE,
            crate::RegisteredLlmProfile::new(
                crate::testing::test_llm_profile_metadata(FIRST_PROFILE),
                provider.clone(),
            ),
        )
        .and_then(|registry| {
            registry.register(
                REASONING_MODEL,
                crate::RegisteredLlmProfile::new(reasoning_metadata.clone(), provider),
            )
        })
        .expect("two distinct keys register");
    let parts = law_session(
        prefix,
        "reasoning",
        &effect_host,
        &stores,
        Arc::new(registry),
    )
    .await;
    let deep = crate::ReasoningSelection::Effort("deep".to_string());

    let receipt = submit_transaction(
        &parts,
        "turn-config-reasoning-alone",
        &crate::ConfigTransaction::of(crate::plugin::config::core::SetReasoning {
            reasoning: deep.clone(),
        }),
    )
    .await;
    let outcome =
        execute_config_command(&runner, &parts, receipt, "turn-config-reasoning-alone").await;
    let crate::ConfigTransactionOutcome::Refused { refusal } = outcome else {
        panic!("an effort the session's model does not declare is refused: {outcome:?}");
    };
    assert_eq!(refusal.owner, crate::CORE_CONFIG_OWNER);
    let refusal = refusal
        .owner_refusal::<crate::CoreConfigRefusal>()
        .expect("the core owner's typed refusal");
    assert!(
        matches!(
            &refusal,
            crate::CoreConfigRefusal::ReasoningRefused { key, reasoning, .. }
                if key.as_str() == FIRST_PROFILE && *reasoning == deep
        ),
        "the refusal names the recorded model and the effort: {refusal:?}"
    );
    let head = parts
        .store
        .load_session_head_meta(&parts.session_id)
        .await
        .expect("read the head")
        .expect("the drain committed the session's head");
    assert_eq!(
        head.config.config_revision, 0,
        "the refusal published nothing"
    );

    let receipt = submit_transaction(
        &parts,
        "turn-config-reasoning-with-model",
        &crate::ConfigTransaction::of(crate::plugin::config::core::SetReasoning {
            reasoning: deep.clone(),
        })
        .then(crate::plugin::config::core::SetLlmProfile {
            model: crate::LlmProfileKey::new(REASONING_MODEL),
        }),
    )
    .await;
    let outcome =
        execute_config_command(&runner, &parts, receipt, "turn-config-reasoning-with-model").await;
    assert!(
        matches!(outcome, crate::ConfigTransactionOutcome::Applied { .. }),
        "the effort applies with a model that declares it: {outcome:?}"
    );
    let head = parts
        .store
        .load_session_head_meta(&parts.session_id)
        .await
        .expect("read the head")
        .expect("the drain committed the session's head");
    assert_eq!(
        head.config.model,
        Some(
            crate::LlmProfileConfig::new(crate::RecordedLlmProfile::mint(
                crate::LlmProfileKey::new(REASONING_MODEL),
                reasoning_metadata,
            ))
            .with_reasoning(deep)
        ),
        "one revision step records the new model with the effort"
    );
    assert_eq!(head.config.config_revision, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model was asked");
}

mod command_settlement;
pub use command_settlement::*;
mod recorded_request_defaults;
pub use recorded_request_defaults::*;

/// The tool a looping model calls on every iteration of its turn.
const LOOKUP_TOOL: &str = "turn_config_lookup_probe";

struct LookupTool;

#[expect(
    clippy::expect_used,
    reason = "this module declares the tool or payload schema and admission checks its invariant"
)]
fn lookup_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{LOOKUP_TOOL}"),
        LOOKUP_TOOL,
        "A tool that answers every call.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
    .expect("valid declared tool schemas")
}

#[async_trait::async_trait]
impl crate::ToolProvider for LookupTool {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![lookup_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == LOOKUP_TOOL).then(|| Arc::new(lookup_tool().contract()))
    }

    async fn execute(&self, _call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(
            crate::ToolCallOutput::success(serde_json::json!({"found": true})),
        ))
    }
}

/// A model that calls [`LOOKUP_TOOL`] on every call and never answers, so a
/// turn runs until its budget stops it. `calls` counts its calls.
fn looping_model(calls: &Arc<AtomicUsize>) -> crate::ProviderHandle {
    let calls = Arc::clone(calls);
    crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |_request| {
            let index = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(crate::LlmResponse {
                    parts: vec![crate::LlmOutputPart::ToolCall {
                        call_id: format!("turn-config-lookup-{index}"),
                        tool_name: LOOKUP_TOOL.into(),
                        input_json: "{}".into(),
                        replay: None,
                    }],
                    ..crate::LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// A looping-model law's session: [`LookupTool`] installed at creation and
/// `calls` counting the model's calls, the created head recording `recorded`.
async fn looping_session(
    prefix: &str,
    name: &str,
    effect_host: &Arc<dyn crate::EffectHost>,
    stores: &Arc<dyn crate::StoreSet>,
    calls: &Arc<AtomicUsize>,
    recorded: crate::SessionPolicy,
) -> ConfigParts {
    law_session_recording(
        prefix,
        name,
        effect_host,
        stores,
        turn_config_llm_profiles(looping_model(calls)),
        recorded,
        vec![Arc::new(crate::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginDeclaration::initial("conformance-turn-config-lookup-probe"),
            crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(LookupTool)),
        ))],
    )
    .await
}

/// A policy whose execution controls are `turn_budget`, over the mock route.
fn policy_with_budget(turn_budget: crate::TurnBudget) -> crate::SessionPolicy {
    crate::SessionPolicy {
        turn_budget,
        ..crate::testing::mock_session_policy()
    }
}

/// One turn attempt of `run` on a runtime opened with `policy`, sending how
/// the turn returned on `turn_tx`.
fn looping_attempt(
    parts: &ConfigParts,
    run: &TurnId,
    policy: crate::SessionPolicy,
    turn_tx: TurnResultTx,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    let run = run.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let run = run.clone();
        let policy = policy.clone();
        let turn_tx = turn_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime_under(parts, policy).await;
            let turn = runtime
                .execute_turn(
                    text_input(&run, "look everything up"),
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                )
                .await;
            let end = crate::ConformanceTurnEnd::of(&turn);
            let _ = turn_tx.send(turn);
            end
        })
    })
}

/// Crashes a run's execution after its config is recorded and before its
/// first model call.
struct CrashBeforeFirstModelCall;

impl lash_core::runtime::RuntimeTurnPhaseProbe for CrashBeforeFirstModelCall {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PromptBuild {
            panic!("injected crash after the run's config record and before its model call");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

/// A redrive runs under the execution controls its run recorded (FIG-4376,
/// ADR 0105 §1). The run's first execution records its config, turn budget
/// included, and dies before its first model call. The redrive opens the
/// session under other creation defaults, as a redeployed worker with another
/// default budget would: it reads the record back and stops at the recorded
/// bound.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_redrive_runs_under_the_execution_controls_its_run_recorded(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    const RECORDED_TURNS: usize = 2;
    const REDEPLOYED_TURNS: usize = 5;
    let calls = Arc::new(AtomicUsize::new(0));
    // The session is created under the crashing execution's bound: the
    // created head records it, and the redrive's open adopts it (FIG-4553).
    let parts = looping_session(
        prefix,
        "recorded-controls-redrive",
        &effect_host,
        &stores,
        &calls,
        policy_with_budget(crate::TurnBudget::bounded(RECORDED_TURNS)),
    )
    .await;
    let run = TurnId::fixture(format!("{prefix}-turn-config-recorded-controls-run"));
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    let crashing: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        let run = run.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let run = run.clone();
            Box::pin(async move {
                let mut runtime = build_runtime_under(
                    parts,
                    policy_with_budget(crate::TurnBudget::bounded(RECORDED_TURNS)),
                )
                .await;
                runtime.set_turn_phase_probe(Arc::new(CrashBeforeFirstModelCall));
                let _ = runtime
                    .execute_turn(
                        text_input(&run, "look everything up"),
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    )
                    .await;
                panic!("the crash fires before the run's first model call");
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, &run)),
            crashing,
            looping_attempt(
                &parts,
                &run,
                policy_with_budget(crate::TurnBudget::bounded(REDEPLOYED_TURNS)),
                turn_tx,
            ),
        )
        .await;
    let turn = turn_rx
        .recv()
        .await
        .expect("the tier's runner redrove the run")
        .unwrap_or_else(|error| panic!("the redriven run executes: {error:?}"));
    assert_eq!(
        turn.outcome,
        crate::TurnOutcome::Stopped(crate::TurnStop::MaxTurns),
        "the redriven run stops at a bound"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        RECORDED_TURNS,
        "the redriven run stops at the bound its run recorded, not the redrive's default"
    );
}

/// A worker's termination policy that says a turn ending without `Done`
/// fails when `missing_done_fails`, and finishes otherwise.
fn termination(missing_done_fails: bool) -> crate::TerminationPolicy {
    crate::TerminationPolicy {
        treat_missing_done_as_failure: missing_done_fails,
    }
}

/// A missing run record refuses terminal assembly without panicking,
/// retrying, or losing its code at a plugin or host boundary (FIG-4508).
#[expect(clippy::expect_used, reason = "conformance fixture results must exist")]
pub async fn a_missing_recorded_termination_is_a_typed_terminal_refusal(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let parts = law_session(
        prefix,
        "missing-recorded-termination",
        &effect_host,
        &stores,
        turn_config_llm_profiles(recording_model(&calls, &models)),
    )
    .await;
    let run = TurnId::fixture(format!("{prefix}-missing-recorded-termination-run"));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, &run)),
            Arc::new(move |scope| {
                let parts = parts.clone();
                let run = run.clone();
                let tx = tx.clone();
                Box::pin(async move {
                    let mut runtime = build_runtime(parts).await;
                    let result = runtime
                        .finish_without_recorded_run_for_testing(
                            run,
                            crate::TurnOptions::new(
                                tokio_util::sync::CancellationToken::new(),
                                scope,
                            ),
                        )
                        .await;
                    let _ = tx.send(result);
                    crate::ConformanceTurnEnd::Settled
                })
            }),
        )
        .await;
    let error = rx
        .recv()
        .await
        .expect("the commit attempt returned")
        .expect_err("a run without its record cannot assemble a terminal");
    let expected = crate::RuntimeErrorCode::from_wire_code("recorded_termination_unavailable");
    assert_eq!(error.code, expected);
    assert!(!error.is_retryable());
    assert!(error.is_terminal());
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let plugin = crate::plugin::PluginError::Runtime(error.clone());
    let encoded = serde_json::to_vec(&plugin).expect("encode the plugin refusal");
    let decoded: crate::plugin::PluginError =
        serde_json::from_slice(&encoded).expect("decode the plugin refusal");
    let returned = decoded.into_turn_failure(crate::RuntimeErrorCode::PluginFinalizeTurn);
    assert_eq!(
        returned.code, expected,
        "the plugin boundary retains the cause"
    );
    assert!(!returned.is_retryable());
    let host = crate::SessionError::Plugin(crate::plugin::PluginError::Runtime(returned));
    let crate::SessionError::Plugin(crate::plugin::PluginError::Runtime(returned)) = host else {
        panic!("the host retains the typed runtime refusal");
    };
    assert_eq!(returned.code, expected);
    let controller = crate::RuntimeEffectControllerError::from(error);
    let encoded = serde_json::to_vec(&controller).expect("encode the controller refusal");
    let decoded: crate::RuntimeEffectControllerError =
        serde_json::from_slice(&encoded).expect("decode the controller refusal");
    let returned = crate::plugin::PluginError::RuntimeEffectController(decoded)
        .into_turn_failure(crate::RuntimeErrorCode::PluginFinalizeTurn);
    assert_eq!(
        returned.code, expected,
        "the controller boundary retains the cause"
    );
    assert!(!returned.is_retryable());
}

/// One attempt of `run` on a worker whose host termination policy is
/// `termination`, under the protocol that ends its turn without `Done`.
/// With `crash`, the attempt dies after the run's config record and before
/// its first model call; otherwise it sends how the turn returned on
/// `turn_tx`.
fn missing_done_attempt(
    parts: &ConfigParts,
    run: &TurnId,
    termination: crate::TerminationPolicy,
    crash: bool,
    turn_tx: TurnResultTx,
) -> crate::ConformanceTurnAttempt {
    let mut parts = parts.clone();
    parts.host.control.termination = termination;
    let run = run.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let run = run.clone();
        let turn_tx = turn_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(parts).await;
            if crash {
                runtime.set_turn_phase_probe(Arc::new(CrashBeforeFirstModelCall));
            }
            let turn = runtime
                .execute_turn(
                    text_input(&run, "answer without ending the stream"),
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                )
                .await;
            assert!(!crash, "the crash fires before the run's first model call");
            let end = crate::ConformanceTurnEnd::of(&turn);
            let _ = turn_tx.send(turn);
            end
        })
    })
}

/// Whether `turn` carries the missing-`Done` fallback's issue.
fn has_missing_done_issue(turn: &crate::AssembledTurn) -> bool {
    turn.errors
        .iter()
        .any(|issue| issue.code == Some(crate::TurnFailureCode::MissingDone.into()))
}

/// A redrive assembles the terminal its run's recorded termination policy
/// decides (FIG-4389, ADR 0105 §1). The turn's protocol ends its stream with
/// neither an outcome nor `Done`, so its terminal is the missing-`Done`
/// fallback. The run's first execution, on a worker with one policy,
/// records its config and dies before its first model call; the redrive runs
/// on a worker with the opposite policy. Both directions assemble the
/// terminal the recorded policy decides: a runtime error with a `MissingDone`
/// issue when it fails a missing `Done`, a finished turn without that issue
/// when it does not.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_redrive_assembles_the_terminal_its_run_recorded_termination_decides(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    for recorded_fails in [true, false] {
        let name = if recorded_fails {
            "missing-done-recorded-fails"
        } else {
            "missing-done-recorded-finishes"
        };
        let calls = Arc::new(AtomicUsize::new(0));
        let models = Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut parts = law_session(
            prefix,
            name,
            &effect_host,
            &stores,
            turn_config_llm_profiles(recording_model(&calls, &models)),
        )
        .await;
        parts.protocol = crate::testing::test_protocol_factories_ending_without_done();
        let run = TurnId::fixture(format!("{prefix}-turn-config-{name}-run"));
        let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
        runner
            .run_crashed_then_redriven_turn(
                admit(crate::ExecutionScope::turn(&parts.session_id, &run)),
                missing_done_attempt(
                    &parts,
                    &run,
                    termination(recorded_fails),
                    true,
                    turn_tx.clone(),
                ),
                missing_done_attempt(&parts, &run, termination(!recorded_fails), false, turn_tx),
            )
            .await;
        let turn = turn_rx
            .recv()
            .await
            .expect("the tier's runner redrove the run")
            .unwrap_or_else(|error| panic!("{name}: the redriven run executes: {error:?}"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{name}: the redriven run makes its one model call and its stream ends there"
        );
        if recorded_fails {
            assert_eq!(
                turn.outcome,
                crate::TurnOutcome::Stopped(crate::TurnStop::RuntimeError),
                "{name}: the redrive fails the missing Done, as its run recorded"
            );
            assert!(
                has_missing_done_issue(&turn),
                "{name}: the failure is the missing Done: {:?}",
                turn.errors
            );
        } else {
            assert!(
                matches!(
                    turn.outcome,
                    crate::TurnOutcome::Finished(crate::TurnFinish::AssistantMessage { .. })
                ),
                "{name}: the redrive finishes the turn, as its run recorded: {:?}",
                turn.outcome
            );
            assert!(
                !has_missing_done_issue(&turn),
                "{name}: the run recorded no missing-Done failure: {:?}",
                turn.errors
            );
        }
    }
}
