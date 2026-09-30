//! The session config a logical turn runs under (FIG-3600 S6, D3 §2).
//!
//! A root resolves its session config once, as a recorded step at the top of
//! the logical-turn funnel, and every physical turn of the root runs under
//! that record. A redrive replays the record instead of reading the live
//! head, so a config change that landed after the root committed never
//! reaches the root's replay, and an input that arrives after the change
//! runs under it.

use lash_core::testing::TestTurnDrive as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

use crate::admit;

/// The model the session starts on.
const FIRST_MODEL: &str = "mock-model";
/// The model a config command moves the session to.
const SECOND_MODEL: &str = "turn-config-second-model";

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
    mut policy: crate::SessionPolicy,
) -> crate::LashRuntime {
    policy.session_id = Some(parts.session_id.clone());
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

/// The host's models for these laws: [`FIRST_MODEL`] and [`SECOND_MODEL`],
/// both served by `provider`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: two distinct literal keys always register"
)]
fn turn_config_models(provider: crate::ProviderHandle) -> Arc<crate::ModelRegistry> {
    Arc::new(
        crate::ModelRegistry::new()
            .register(
                FIRST_MODEL,
                crate::RegisteredModel::new(
                    crate::testing::test_model_metadata(FIRST_MODEL),
                    provider.clone(),
                ),
            )
            .and_then(|registry| {
                registry.register(
                    SECOND_MODEL,
                    crate::RegisteredModel::new(
                        crate::testing::test_model_metadata(SECOND_MODEL),
                        provider,
                    ),
                )
            })
            .expect("two distinct keys register"),
    )
}

/// Move the session to [`SECOND_MODEL`] through the command lane.
async fn command_second_model(runner: &Arc<dyn crate::ConformanceTurnRunner>, parts: &ConfigParts) {
    let receipt = submit_second_model(parts, "turn-config-command").await;
    let outcome = drive_config_command(runner, parts, receipt, "turn-config-command").await;
    assert!(
        matches!(outcome, crate::ConfigTransactionOutcome::Applied { .. }),
        "the model change applies: {outcome:?}"
    );
}

/// Submit a config transaction moving the session to [`SECOND_MODEL`] under
/// `id`, from a runtime of its own, and return once it is durable.
async fn submit_second_model(parts: &ConfigParts, id: &str) -> crate::SessionCommandReceipt {
    submit_transaction(
        parts,
        id,
        &crate::ConfigTransaction::of(crate::plugin::config::core::SetModel {
            model: crate::ModelKey::new(SECOND_MODEL),
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

/// Drive the command root that applies the config transaction `receipt`
/// names, as the tier's runner runs it, and answer how it settled.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the config command settles on a live store"
)]
async fn drive_config_command(
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
                        .drive_next_root(
                            request,
                            crate::TurnOptions::new(
                                tokio_util::sync::CancellationToken::new(),
                                controller,
                            ),
                        )
                        .await
                        .expect("engine drives the config transaction");
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
        })
        .build()
        .into_handle()
}

/// [`recording_model`] whose first call signals `entered` and waits for
/// `release`: the root that makes it owns the session head until released.
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
                .push(request.model.clone());
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

/// Root A committed on the first model with its reply lost, and the model
/// change that landed after it: where both stale-fence laws start.
struct CommittedRootUnderAModelChange {
    parts: ConfigParts,
    root: TurnId,
    /// The scope root A was admitted under, which its redrive runs on.
    admitted: crate::AdmittedScope,
    /// The drive fence root A's execution was sealed under. The model
    /// change's seal made it stale.
    fence: crate::store::DriveFence,
    calls: Arc<AtomicUsize>,
    models: Arc<std::sync::Mutex<Vec<String>>>,
    /// The durable head's revision once the model change applied.
    revision_after_change: u64,
}

impl CommittedRootUnderAModelChange {
    /// Root A commits on the first model and its execution dies before its
    /// reply leaves it. The session's next boundary then applies the model
    /// change, whose seal raises the drive epoch past A's fence.
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
            turn_config_models(recording_model(&calls, &models)),
        )
        .await;
        let root = TurnId::from(format!("{prefix}-turn-config-{name}-root"));
        let crash = crate::ConformanceCrash::new();
        let crashing: crate::ConformanceTurnAttempt = {
            let parts = parts.clone();
            let root = root.clone();
            let crash = crash.clone();
            Arc::new(move |scope| {
                let parts = parts.clone();
                let root = root.clone();
                let crash = crash.clone();
                Box::pin(async move {
                    let mut runtime = build_runtime(parts).await;
                    let turn = runtime
                        .drive_turn(
                            text_input(&root, "first question"),
                            crate::TurnOptions::new(
                                tokio_util::sync::CancellationToken::new(),
                                scope,
                            ),
                        )
                        .await
                        .unwrap_or_else(|error| {
                            panic!("root A commits on the first model: {error:?}")
                        });
                    assert!(
                        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
                        "root A finishes on its first execution: {:?}",
                        turn.outcome
                    );
                    crash.fire();
                    std::future::pending().await
                })
            })
        };
        let admitted = admit(crate::ExecutionScope::turn(&parts.session_id, &root));
        runner
            .run_turn_until_crash(admitted.clone(), crashing, crash)
            .await;

        // Root A's seal is still the session's: the same admission and start
        // marker answer its stored fence and raise nothing (ADR 0105 §2).
        let sealed = parts
            .store
            .drive_epoch(&parts.session_id)
            .await
            .expect("read the drive epoch root A sealed");
        let seal = parts
            .store
            .seal_drive_epoch(
                &parts.session_id,
                sealed
                    .admission
                    .as_ref()
                    .expect("root A's admission raised the epoch"),
                sealed.epoch,
                sealed
                    .root_start
                    .as_ref()
                    .expect("root A's execution stored its start marker"),
            )
            .await
            .expect("read root A's fence back");
        let crate::store::DriveEpochSeal::Sealed(fence) = seal else {
            panic!("root A's seal answers its stored fence: {seal:?}");
        };

        command_second_model(runner, &parts).await;
        let committed = parts
            .store
            .load_session_head_meta(&parts.session_id)
            .await
            .expect("read the head after the model change")
            .expect("root A's commit and the model change are durable");
        assert_eq!(
            crate::conformance::helpers::recorded_model_key(&committed.config.model),
            SECOND_MODEL,
            "precondition: the model change landed on the durable head"
        );
        let epoch = parts
            .store
            .drive_epoch(&parts.session_id)
            .await
            .expect("read the drive epoch after the model change")
            .epoch;
        assert!(
            epoch > fence.epoch(),
            "precondition: the model change raised the drive epoch past root A's fence"
        );
        Self {
            parts,
            root,
            admitted,
            fence,
            calls,
            models,
            revision_after_change: committed.head_revision,
        }
    }

    /// Nothing ran or was written past the model change: root A's one model
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
            crate::conformance::helpers::recorded_model_key(&head.config.model),
            SECOND_MODEL,
            "{what} leaves the model change on the durable head"
        );
    }

    /// A root that follows runs on the model the change moved the session to.
    async fn assert_the_next_root_runs_on_the_second_model(
        &self,
        prefix: &str,
        name: &str,
        runner: &Arc<dyn crate::ConformanceTurnRunner>,
    ) {
        let next = TurnId::from(format!("{prefix}-turn-config-{name}-next"));
        let next_turn = run_text_turn(
            runner,
            &self.parts,
            &next,
            "second question",
            BeforeSend::Nothing,
        )
        .await
        .unwrap_or_else(|error| panic!("the next root runs: {error:?}"));
        assert!(
            matches!(next_turn.outcome, crate::TurnOutcome::Finished(_)),
            "the next root finishes: {:?}",
            next_turn.outcome
        );
        assert_eq!(
            recorded_models(&self.models),
            vec![FIRST_MODEL.to_string(), SECOND_MODEL.to_string()],
            "root A's one model call named the first model, and the next root's the second"
        );
    }
}

/// A committed root redriven after a later model change answers from what it
/// stored, under the config it recorded (D3 §2.2, ADR 0105 §9).
///
/// Root A commits on the first model. Before its reply reaches anyone, the
/// execution dies and the session's next boundary applies a model change,
/// whose seal makes A's drive fence stale. The tier redrives A, and its
/// journal repeats A's commit under that stale fence. The commit is the exact
/// replay of one the store holds, so its receipt answers it: the redrive
/// returns A's committed answer with no new model call, no head write and no
/// park. A root that follows runs on the second model.
///
/// The other side of the boundary is
/// [`an_older_admission_redriven_after_a_model_change_is_fenced_out`].
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_committed_root_redriven_after_a_model_change_answers_from_its_receipt(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let name = "replay";
    let law =
        CommittedRootUnderAModelChange::new(prefix, name, &effect_host, &stores, &runner).await;
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(
            law.admitted.clone(),
            text_attempt(&law.parts, &law.root, "first question", turn_tx),
        )
        .await;
    let turn = turn_rx
        .recv()
        .await
        .expect("the tier's runner ran the redriven root")
        .unwrap_or_else(|error| {
            panic!("the redrive of committed root A answers from its receipt: {error:?}")
        });
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the redrive of root A finishes: {:?}; errors: {:?}",
        turn.outcome,
        turn.errors
    );
    assert_eq!(
        turn.assistant_output.safe_text, "answer 1",
        "the redrive answers with what root A committed"
    );
    assert_eq!(
        crate::conformance::helpers::recorded_model_key(&turn.state.policy.model),
        FIRST_MODEL,
        "the redrive answers under the config root A recorded"
    );
    law.assert_nothing_moved("the redrive of a committed root")
        .await;
    law.assert_the_next_root_runs_on_the_second_model(prefix, name, &runner)
        .await;
}

/// A later model change raises the drive epoch and fences out an older
/// admission's redrive (D24a, ADR 0105 §2 and §9, ADR 0101 §7).
///
/// Root A commits on the first model, its execution dies, and the session's
/// next boundary applies a model change. An execution of A's admission that
/// does not repeat A's stored commit, as one that lost its journal and ran
/// the root again would, presents A's stale fence with a commit the store
/// holds no receipt for. It is refused whole as a stale drive fence, which
/// the runtime classifies as a superseded commit, and the model change stands. A root that follows runs on the second model.
///
/// The other side of the boundary is
/// [`a_committed_root_redriven_after_a_model_change_answers_from_its_receipt`].
pub async fn an_older_admission_redriven_after_a_model_change_is_fenced_out(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let name = "fenced-out";
    let law =
        CommittedRootUnderAModelChange::new(prefix, name, &effect_host, &stores, &runner).await;
    let mut policy = crate::testing::mock_session_policy();
    policy.session_id = Some(law.parts.session_id.clone());
    let rerun = crate::RuntimeSessionState {
        session_id: law.parts.session_id.clone(),
        policy,
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    // Root A's own final commit, with other content, and a commit of a turn
    // the store never saw: neither is the stored commit's exact replay.
    for (what, turn) in [
        ("root A's final commit run again", law.root.clone()),
        (
            "a turn root A never committed",
            TurnId::from(format!("{}-rerun", law.root)),
        ),
    ] {
        let mut commit = super::root_control::root_final_commit(&rerun, &law.root, &turn, 0);
        commit.drive_fence = Some(Box::new(law.fence.clone()));
        let refused = law.parts.store.commit_runtime_state(commit).await;
        let Err(refusal) = refused else {
            panic!("{what} under the older admission's fence is refused: {refused:?}");
        };
        assert!(
            matches!(
                &refusal,
                crate::StoreError::StaleDriveFence { fence_epoch, .. }
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
    law.assert_the_next_root_runs_on_the_second_model(prefix, name, &runner)
        .await;
}

/// The parts of a turn-config law's session: a host whose models serve the
/// recording model, and the session's store.
async fn law_session(
    prefix: &str,
    name: &str,
    effect_host: &Arc<dyn crate::EffectHost>,
    stores: &Arc<dyn crate::StoreSet>,
    models: Arc<dyn crate::RuntimeModels>,
) -> ConfigParts {
    let session_id = SessionId::from(format!("{prefix}-turn-config-{name}-session"));
    let mut host = crate::LawBackend::over_stores(Arc::clone(stores), Arc::clone(effect_host))
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
    host.providers.models = models;
    let store = crate::conformance::law_session_store(stores.as_ref(), &session_id).await;
    ConfigParts {
        session_id,
        host,
        store,
        protocol: crate::testing::test_standard_protocol_factories(),
        tools: Vec::new(),
    }
}

/// What one turn attempt does before it sends its input.
#[derive(Clone, Copy)]
enum BeforeSend {
    Nothing,
    /// Move the session to [`SECOND_MODEL`] through the command lane first.
    CommandSecondModel,
}

/// Run `root` with `text` as one attempt on the tier's runner and hand back
/// how its turn returned.
async fn run_text_turn(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &ConfigParts,
    root: &TurnId,
    text: &'static str,
    before: BeforeSend,
) -> Result<crate::AssembledTurn, crate::RuntimeError> {
    if matches!(before, BeforeSend::CommandSecondModel) {
        command_second_model(runner, parts).await;
    }
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, root)),
            text_attempt(parts, root, text, turn_tx),
        )
        .await;
    turn_rx
        .recv()
        .await
        .unwrap_or_else(|| panic!("the tier's runner ran root `{root}`"))
}

fn text_attempt(
    parts: &ConfigParts,
    root: &TurnId,
    text: &'static str,
    turn_tx: TurnResultTx,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    let root = root.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let root = root.clone();
        let turn_tx = turn_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(parts).await;
            let turn = runtime
                .drive_turn(
                    text_input(&root, text),
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
/// `command(M2)` then `send(x)` runs x on M2, and the root before the command
/// ran on M1.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_input_sent_after_a_config_command_runs_on_the_new_model(
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
        turn_config_models(recording_model(&calls, &models)),
    )
    .await;
    let first = TurnId::from(format!("{prefix}-turn-config-after-command-first"));
    let turn = run_text_turn(&runner, &parts, &first, "first", BeforeSend::Nothing)
        .await
        .unwrap_or_else(|error| panic!("the first root runs: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the first root finishes: {:?}",
        turn.outcome
    );
    let second = TurnId::from(format!("{prefix}-turn-config-after-command-second"));
    let turn = run_text_turn(
        &runner,
        &parts,
        &second,
        "second",
        BeforeSend::CommandSecondModel,
    )
    .await
    .unwrap_or_else(|error| panic!("the root sent after the command runs: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the root sent after the command finishes: {:?}",
        turn.outcome
    );
    assert_eq!(
        recorded_models(&models),
        vec![FIRST_MODEL.to_string(), SECOND_MODEL.to_string()],
        "the root before the command ran on the first model, the one after it on the second"
    );
    let head = parts
        .store
        .load_session_head_meta(&parts.session_id)
        .await
        .expect("read the head")
        .expect("the session committed");
    assert_eq!(
        crate::conformance::helpers::recorded_model_key(&head.config.model),
        SECOND_MODEL
    );
}

/// A config transaction submitted while a root owns the session head waits
/// for that root (FIG-4379): its submission completes, the root finishes
/// under the config it was admitted with, and nothing of the transaction is
/// published while the root runs or by the root's commit. Once the root
/// releases the head the command lane applies it with one revision step,
/// and the next root runs under it.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_config_transaction_waits_while_a_root_owns_the_head(
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
        "pending-while-root",
        &effect_host,
        &stores,
        turn_config_models(gated_recording_model(&calls, &models, &entered, &release)),
    )
    .await;
    let root = TurnId::from(format!("{prefix}-turn-config-pending-while-root"));
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    let submitting = async {
        entered.notified().await;
        let receipt = submit_second_model(&parts, "pending-while-root").await;
        // The session's first root commits its head, so while it runs the
        // head is either still unwritten or the creation config.
        let head = parts
            .store
            .load_session_head_meta(&parts.session_id)
            .await
            .expect("read the head while the root runs");
        if let Some(head) = head {
            assert_eq!(
                (head.config.wire_model(), head.config.config_revision),
                (Some(FIRST_MODEL), 0),
                "nothing is published while the root owns the head"
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
            admit(crate::ExecutionScope::turn(&parts.session_id, &root)),
            text_attempt(&parts, &root, "first", turn_tx),
        ),
        submitting,
    );
    let turn = turn_rx
        .recv()
        .await
        .expect("the tier's runner ran the root")
        .unwrap_or_else(|error| panic!("the root runs: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the root finishes: {:?}",
        turn.outcome
    );
    let head = parts
        .store
        .load_session_head_meta(&parts.session_id)
        .await
        .expect("read the head after the root")
        .expect("the root committed");
    assert_eq!(
        (head.config.wire_model(), head.config.config_revision),
        (Some(FIRST_MODEL), 0),
        "the root's commit does not publish the pending transaction"
    );

    let outcome = drive_config_command(&runner, &parts, receipt, "pending-while-root-drain").await;
    assert_eq!(
        outcome,
        crate::ConfigTransactionOutcome::Applied {
            base_revision: 0,
            revision: 1,
            outputs: vec![serde_json::Value::Null],
        },
        "the lane applies the transaction once the root released the head"
    );

    let next = TurnId::from(format!("{prefix}-turn-config-pending-while-root-next"));
    let turn = run_text_turn(&runner, &parts, &next, "second", BeforeSend::Nothing)
        .await
        .unwrap_or_else(|error| panic!("the next root runs: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the next root finishes: {:?}",
        turn.outcome
    );
    assert_eq!(
        recorded_models(&models),
        vec![FIRST_MODEL.to_string(), SECOND_MODEL.to_string()],
        "the root that owned the head ran on its admitted model, the next root on the new one"
    );
}

/// The tool whose call closes the first frame with a switch, so the root
/// runs a second physical turn.
const SWITCH_TOOL: &str = "turn_config_switch_probe";

struct SwitchTool;

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

/// One config resolution per root (D3 §2.1, Q11): a root whose first frame
/// switches runs two physical turns under one recorded config. Both model
/// calls name the same provider and model, and a tier that can read its
/// journal holds exactly one `turn-config:{root}` entry for the root.
pub async fn one_config_resolution_per_root(
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
                    .push(request.model.clone());
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
    let mut parts = law_session(
        prefix,
        "one-resolution",
        &effect_host,
        &stores,
        turn_config_models(model),
    )
    .await;
    parts.tools = vec![Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-turn-config-switch-probe",
        crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(SwitchTool)),
    ))];
    let root = TurnId::from(format!("{prefix}-turn-config-one-resolution-root"));
    let run = run_text_turn(
        &runner,
        &parts,
        &root,
        "switch, then answer",
        BeforeSend::Nothing,
    )
    .await
    .unwrap_or_else(|error| panic!("the switching root runs: {error:?}"));
    assert!(
        matches!(run.outcome, crate::TurnOutcome::Finished(_)),
        "the root finishes in its follow-on frame: {:?}; errors: {:?}",
        run.outcome,
        run.errors
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "precondition: the root ran two physical turns, one model call each"
    );
    assert_eq!(
        recorded_models(&models),
        vec![FIRST_MODEL.to_string(), FIRST_MODEL.to_string()],
        "every physical turn of the root names the one recorded model"
    );
    if let Some(keys) = runner
        .recorded_replay_keys(&crate::ExecutionScope::turn(&parts.session_id, &root))
        .await
    {
        let resolutions = keys
            .iter()
            .filter(|key| key.starts_with("turn-config:"))
            .collect::<Vec<_>>();
        assert_eq!(
            resolutions,
            vec![&format!("turn-config:{root}")],
            "the root resolved its config exactly once: {keys:?}"
        );
    }
}

/// A recorded model this worker cannot bind retries and never fails the turn
/// (D3 Q3): the root aborts retryably with nothing recorded as its outcome,
/// and once the key is served again its redrive completes it once.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_unbindable_model_retries_and_never_fails_the_turn(
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
        turn_config_models(recording_model(&calls, &models)),
    )
    .await;
    // The same session on a worker whose models lack the recorded key.
    let mut unserved = served.clone();
    unserved.host.providers.models = Arc::new(crate::ModelRegistry::new());
    let root = TurnId::from(format!("{prefix}-turn-config-unbindable-root"));
    let aborted = run_text_turn(&runner, &unserved, &root, "hello", BeforeSend::Nothing)
        .await
        .expect_err("a root whose recorded model cannot be bound aborts");
    assert_eq!(
        aborted.code,
        crate::RuntimeErrorCode::ModelUnavailable,
        "the abort names the unbindable model: {aborted:?}"
    );
    assert!(
        aborted.is_retryable(),
        "an unbindable model is retried, never the turn's outcome: {aborted:?}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model was asked");
    assert!(
        !served
            .store
            .committed_turn_exists(&served.session_id, &root)
            .await
            .expect("read the root's commit"),
        "the aborted root recorded no outcome"
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

    let turn = run_text_turn(&runner, &served, &root, "hello", BeforeSend::Nothing)
        .await
        .unwrap_or_else(|error| panic!("the redrive with the model back runs: {error:?}"));
    assert!(
        matches!(turn.outcome, crate::TurnOutcome::Finished(_)),
        "the redrive completes the root: {:?}",
        turn.outcome
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the root ran once");
    assert!(
        served
            .store
            .committed_turn_exists(&served.session_id, &root)
            .await
            .expect("read the root's commit"),
        "the redrive committed the root"
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
pub async fn an_unknown_model_key_is_refused_typed_and_publishes_nothing(
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
        turn_config_models(recording_model(&calls, &models)),
    )
    .await;
    let store = Arc::clone(&parts.store);
    let session_id = parts.session_id.clone();
    let receipt = submit_transaction(
        &parts,
        "turn-config-unknown-key",
        &crate::ConfigTransaction::of(crate::plugin::config::core::SetModel {
            model: crate::ModelKey::new("turn-config-unknown-model"),
        }),
    )
    .await;
    let outcome = drive_config_command(&runner, &parts, receipt, "turn-config-unknown-key").await;
    let crate::ConfigTransactionOutcome::Refused { refusal } = outcome else {
        panic!("a key the host's models do not register settles refused: {outcome:?}");
    };
    assert_eq!(refusal.owner, crate::CORE_CONFIG_OWNER);
    assert_eq!(
        serde_json::from_value::<crate::CoreConfigRefusal>(refusal.refusal)
            .expect("the core owner's typed refusal"),
        crate::CoreConfigRefusal::UnknownModel {
            key: crate::ModelKey::new("turn-config-unknown-model"),
        }
    );
    let head = store
        .load_session_head_meta(&session_id)
        .await
        .expect("read the head")
        .expect("the drain committed the session's head");
    assert_eq!(
        head.config.model_key().map(crate::ModelKey::as_str),
        Some(FIRST_MODEL),
        "nothing was published"
    );
    assert_eq!(head.config.config_revision, 0, "the revision did not move");
    assert_eq!(calls.load(Ordering::SeqCst), 0, "no model was asked");
}

/// A model change records the binding the host's models minted where the
/// transaction resolved (FIG-4374): the worker that submitted it need not
/// serve the key, and the session records exactly the metadata the resolving
/// worker's registry held, never re-deriving it from a later catalog.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_model_change_records_the_binding_minted_where_it_resolves(
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
        crate::testing::standard_test_models(session.clone()),
    )
    .await;
    // The worker that resolves it serves the second model, with metadata of
    // its own.
    let resolving_metadata = crate::ModelMetadata::builder(SECOND_MODEL)
        .context_window_tokens(77_777)
        .build()
        .expect("the resolving worker's metadata");
    let mut applier = sender.clone();
    applier.host.providers.models = Arc::new(
        crate::ModelRegistry::new()
            .register(
                FIRST_MODEL,
                crate::RegisteredModel::new(
                    crate::testing::test_model_metadata(FIRST_MODEL),
                    session.clone(),
                ),
            )
            .and_then(|registry| {
                registry.register(
                    SECOND_MODEL,
                    crate::RegisteredModel::new(resolving_metadata.clone(), session),
                )
            })
            .expect("two distinct keys register"),
    );
    let receipt = submit_transaction(
        &sender,
        "turn-config-minted-at-resolution",
        &crate::ConfigTransaction::of(crate::plugin::config::core::SetModel {
            model: crate::ModelKey::new(SECOND_MODEL),
        }),
    )
    .await;
    let outcome = drive_config_command(
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
        Some(crate::ModelConfig::new(crate::RecordedModel::mint(
            crate::ModelKey::new(SECOND_MODEL),
            resolving_metadata,
        ))),
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
pub async fn a_reasoning_change_is_judged_against_the_final_recorded_model(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let calls = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider = recording_model(&calls, &models);
    let reasoning_metadata = crate::testing::test_model_metadata(REASONING_MODEL).with_capability(
        crate::ModelCapability {
            reasoning: Some(crate::ReasoningCapability {
                efforts: vec!["deep".to_string()],
                ..Default::default()
            }),
            ..Default::default()
        },
    );
    let registry = crate::ModelRegistry::new()
        .register(
            FIRST_MODEL,
            crate::RegisteredModel::new(
                crate::testing::test_model_metadata(FIRST_MODEL),
                provider.clone(),
            ),
        )
        .and_then(|registry| {
            registry.register(
                REASONING_MODEL,
                crate::RegisteredModel::new(reasoning_metadata.clone(), provider),
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
        drive_config_command(&runner, &parts, receipt, "turn-config-reasoning-alone").await;
    let crate::ConfigTransactionOutcome::Refused { refusal } = outcome else {
        panic!("an effort the session's model does not declare is refused: {outcome:?}");
    };
    assert_eq!(refusal.owner, crate::CORE_CONFIG_OWNER);
    let refusal = serde_json::from_value::<crate::CoreConfigRefusal>(refusal.refusal)
        .expect("the core owner's typed refusal");
    assert!(
        matches!(
            &refusal,
            crate::CoreConfigRefusal::ReasoningRefused { key, reasoning, .. }
                if key.as_str() == FIRST_MODEL && *reasoning == deep
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
        .then(crate::plugin::config::core::SetModel {
            model: crate::ModelKey::new(REASONING_MODEL),
        }),
    )
    .await;
    let outcome =
        drive_config_command(&runner, &parts, receipt, "turn-config-reasoning-with-model").await;
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
            crate::ModelConfig::new(crate::RecordedModel::mint(
                crate::ModelKey::new(REASONING_MODEL),
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

/// The tool a looping model calls on every iteration of its turn.
const LOOKUP_TOOL: &str = "turn_config_lookup_probe";

struct LookupTool;

fn lookup_tool() -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{LOOKUP_TOOL}"),
        LOOKUP_TOOL,
        "A tool that answers every call.",
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
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

/// A looping-model law's session: [`LookupTool`] installed and `calls`
/// counting the model's calls.
async fn looping_session(
    prefix: &str,
    name: &str,
    effect_host: &Arc<dyn crate::EffectHost>,
    stores: &Arc<dyn crate::StoreSet>,
    calls: &Arc<AtomicUsize>,
) -> ConfigParts {
    let mut parts = law_session(
        prefix,
        name,
        effect_host,
        stores,
        turn_config_models(looping_model(calls)),
    )
    .await;
    parts.tools = vec![Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-turn-config-lookup-probe",
        crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(LookupTool)),
    ))];
    parts
}

/// A policy whose execution controls are `turn_budget`, over the mock route.
fn policy_with_budget(turn_budget: crate::TurnBudget) -> crate::SessionPolicy {
    crate::SessionPolicy {
        turn_budget,
        ..crate::testing::mock_session_policy()
    }
}

/// One turn attempt of `root` on a runtime opened with `policy`, sending how
/// the turn returned on `turn_tx`.
fn looping_attempt(
    parts: &ConfigParts,
    root: &TurnId,
    policy: crate::SessionPolicy,
    turn_tx: TurnResultTx,
) -> crate::ConformanceTurnAttempt {
    let parts = parts.clone();
    let root = root.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let root = root.clone();
        let policy = policy.clone();
        let turn_tx = turn_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime_under(parts, policy).await;
            let turn = runtime
                .drive_turn(
                    text_input(&root, "look everything up"),
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                )
                .await;
            let end = crate::ConformanceTurnEnd::of(&turn);
            let _ = turn_tx.send(turn);
            end
        })
    })
}

/// Crashes a root's execution after its config is recorded and before its
/// first model call.
struct CrashBeforeFirstModelCall;

impl lash_core::runtime::RuntimeTurnPhaseProbe for CrashBeforeFirstModelCall {
    fn begin(&self, phase: lash_core::runtime::RuntimeTurnPhase) {
        if phase == lash_core::runtime::RuntimeTurnPhase::PromptBuild {
            panic!("injected crash after the root's config record and before its model call");
        }
    }

    fn end(&self, _phase: lash_core::runtime::RuntimeTurnPhase) {}
}

/// A redrive runs under the execution controls its root recorded (FIG-4376,
/// ADR 0105 §1). The root's first execution records its config, turn budget
/// included, and dies before its first model call. The redrive opens the
/// session under other creation defaults, as a redeployed worker with another
/// default budget would: it reads the record back and stops at the recorded
/// bound.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_redrive_runs_under_the_execution_controls_its_root_recorded(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    const RECORDED_TURNS: usize = 2;
    const REDEPLOYED_TURNS: usize = 5;
    let calls = Arc::new(AtomicUsize::new(0));
    let parts = looping_session(
        prefix,
        "recorded-controls-redrive",
        &effect_host,
        &stores,
        &calls,
    )
    .await;
    let root = TurnId::from(format!("{prefix}-turn-config-recorded-controls-root"));
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    let crashing: crate::ConformanceTurnAttempt = {
        let parts = parts.clone();
        let root = root.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let root = root.clone();
            Box::pin(async move {
                let mut runtime = build_runtime_under(
                    parts,
                    policy_with_budget(crate::TurnBudget::bounded(RECORDED_TURNS)),
                )
                .await;
                runtime.set_turn_phase_probe(Arc::new(CrashBeforeFirstModelCall));
                let _ = runtime
                    .drive_turn(
                        text_input(&root, "look everything up"),
                        crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                    )
                    .await;
                panic!("the crash fires before the root's first model call");
            })
        })
    };
    runner
        .run_crashed_then_redriven_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, &root)),
            crashing,
            looping_attempt(
                &parts,
                &root,
                policy_with_budget(crate::TurnBudget::bounded(REDEPLOYED_TURNS)),
                turn_tx,
            ),
        )
        .await;
    let turn = turn_rx
        .recv()
        .await
        .expect("the tier's runner redrove the root")
        .unwrap_or_else(|error| panic!("the redriven root runs: {error:?}"));
    assert_eq!(
        turn.outcome,
        crate::TurnOutcome::Stopped(crate::TurnStop::MaxTurns),
        "the redriven root stops at a bound"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        RECORDED_TURNS,
        "the redriven root stops at the bound its root recorded, not the redrive's default"
    );
}

/// A worker's termination policy that says a turn ending without `Done`
/// fails when `missing_done_fails`, and finishes otherwise.
fn termination(missing_done_fails: bool) -> crate::TerminationPolicy {
    crate::TerminationPolicy {
        treat_missing_done_as_failure: missing_done_fails,
    }
}

/// A missing root record refuses terminal assembly without panicking,
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
        turn_config_models(recording_model(&calls, &models)),
    )
    .await;
    let root = TurnId::from(format!("{prefix}-missing-recorded-termination-root"));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    runner
        .run_turn(
            admit(crate::ExecutionScope::turn(&parts.session_id, &root)),
            Arc::new(move |scope| {
                let parts = parts.clone();
                let root = root.clone();
                let tx = tx.clone();
                Box::pin(async move {
                    let mut runtime = build_runtime(parts).await;
                    let result = runtime
                        .finish_without_recorded_run_for_testing(
                            root,
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
        .expect_err("a root without its record cannot assemble a terminal");
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

/// One attempt of `root` on a worker whose host termination policy is
/// `termination`, under the protocol that ends its turn without `Done`.
/// With `crash`, the attempt dies after the root's config record and before
/// its first model call; otherwise it sends how the turn returned on
/// `turn_tx`.
fn missing_done_attempt(
    parts: &ConfigParts,
    root: &TurnId,
    termination: crate::TerminationPolicy,
    crash: bool,
    turn_tx: TurnResultTx,
) -> crate::ConformanceTurnAttempt {
    let mut parts = parts.clone();
    parts.host.control.termination = termination;
    let root = root.clone();
    Arc::new(move |scope| {
        let parts = parts.clone();
        let root = root.clone();
        let turn_tx = turn_tx.clone();
        Box::pin(async move {
            let mut runtime = build_runtime(parts).await;
            if crash {
                runtime.set_turn_phase_probe(Arc::new(CrashBeforeFirstModelCall));
            }
            let turn = runtime
                .drive_turn(
                    text_input(&root, "answer without ending the stream"),
                    crate::TurnOptions::new(tokio_util::sync::CancellationToken::new(), scope),
                )
                .await;
            assert!(!crash, "the crash fires before the root's first model call");
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

/// A redrive assembles the terminal its root's recorded termination policy
/// decides (FIG-4389, ADR 0105 §1). The turn's protocol ends its stream with
/// neither an outcome nor `Done`, so its terminal is the missing-`Done`
/// fallback. The root's first execution, on a worker with one policy,
/// records its config and dies before its first model call; the redrive runs
/// on a worker with the opposite policy. Both directions assemble the
/// terminal the recorded policy decides: a runtime error with a `MissingDone`
/// issue when it fails a missing `Done`, a finished turn without that issue
/// when it does not.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_redrive_assembles_the_terminal_its_root_recorded_termination_decides(
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
            turn_config_models(recording_model(&calls, &models)),
        )
        .await;
        parts.protocol = crate::testing::test_protocol_factories_ending_without_done();
        let root = TurnId::from(format!("{prefix}-turn-config-{name}-root"));
        let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
        runner
            .run_crashed_then_redriven_turn(
                admit(crate::ExecutionScope::turn(&parts.session_id, &root)),
                missing_done_attempt(
                    &parts,
                    &root,
                    termination(recorded_fails),
                    true,
                    turn_tx.clone(),
                ),
                missing_done_attempt(&parts, &root, termination(!recorded_fails), false, turn_tx),
            )
            .await;
        let turn = turn_rx
            .recv()
            .await
            .expect("the tier's runner redrove the root")
            .unwrap_or_else(|error| panic!("{name}: the redriven root runs: {error:?}"));
        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "{name}: the redriven root makes its one model call and its stream ends there"
        );
        if recorded_fails {
            assert_eq!(
                turn.outcome,
                crate::TurnOutcome::Stopped(crate::TurnStop::RuntimeError),
                "{name}: the redrive fails the missing Done, as its root recorded"
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
                "{name}: the redrive finishes the turn, as its root recorded: {:?}",
                turn.outcome
            );
            assert!(
                !has_missing_done_issue(&turn),
                "{name}: the root recorded no missing-Done failure: {:?}",
                turn.errors
            );
        }
    }
}
