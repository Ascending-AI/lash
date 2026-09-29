//! A row admitted to one root is answered only by that root (FIG-3552,
//! FIG-3927).
//!
//! A root's follow-on physical turn admits a row at its `AfterWork`
//! checkpoint, the model answers it, and the worker dies at the commit that
//! would settle it. The row stays bound to the unfinished root: nothing a
//! worker's death does releases it (ADR 0101, FIG-3927 amendment). Then
//! another drive runs the session on a fresh journal with a peer's input
//! waiting, the dead drive is redriven on its own journal, and later drives
//! run until the session is idle.
//!
//! Whichever path the tier takes — the peer's drive resuming the unfinished
//! root first, or ending it because its execution was lost — the row is
//! answered exactly once, and no other root is ever bound to it while the
//! root that admitted it has no terminal: a root's admission binds only open
//! rows, and only that root's commit or terminal write releases them. The
//! dead drive's redrive comes after the peer's drive sealed a later drive
//! epoch, so it writes nothing and its journaled answer is never committed.
//! The peer's input is answered by the peer's own root.
//!
//! The row is once a steering input addressed to the follow-on turn and once
//! a ready process wake. Every drive runs on the tier's
//! [`ConformanceTurnRunner`](crate::ConformanceTurnRunner): a crash kills the
//! drive's execution where it stands, and the next run of the same scope is
//! its redrive.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION;
use lash_sansio::sync::MutexExt;
use lash_sansio::{SessionId, TurnId};
use pretty_assertions::assert_eq;

use crate::admit;
use crate::plugin::PluginFactory;

/// The reply the first execution journals for the follow-on turn.
const FIRST_EXECUTION_REPLY: &str = "the first execution answered the checkpoint row";
/// The reply every later drive's model gives once its step ran.
const RECOVERY_REPLY: &str = "a later drive answered its turn";
/// How long one drive may take before the law calls it wedged.
const DRIVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(90);

/// Two tools: one whose call switches the agent frame, so the root continues
/// in a follow-on physical turn, and one plain tool, so a turn reaches an
/// `AfterWork` checkpoint before its next model call.
struct RootRowsTools;

fn tool(name: &str, description: &str) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        description,
        crate::ToolDefinition::default_input_schema(),
        serde_json::json!({"type": "object", "additionalProperties": true}),
    )
}

fn switch_frame_tool() -> crate::ToolDefinition {
    tool(
        "switch_frame",
        "Continue the run in a follow-on agent frame.",
    )
}

fn step_tool() -> crate::ToolDefinition {
    tool("step", "One unit of work.")
}

#[async_trait::async_trait]
impl crate::ToolProvider for RootRowsTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![switch_frame_tool().manifest(), step_tool().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        match name {
            "switch_frame" => Some(Arc::new(switch_frame_tool().contract())),
            "step" => Some(Arc::new(step_tool().contract())),
            _ => None,
        }
    }

    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: non-empty frame material always derives"
    )]
    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        let output = crate::ToolCallOutput::success(serde_json::json!({"done": true}));
        let output = if call.name() == "switch_frame" {
            output.with_control(crate::ToolControl::SwitchAgentFrame {
                frame_key: crate::FrameKey::from_caller_material("root-answers-its-rows-follow-on")
                    .expect("non-empty frame material derives"),
                initial_nodes: Vec::new(),
                task: Some("continue in the follow-on frame".to_string()),
            })
        } else {
            output
        };
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(output))
    }
}

fn tools_plugin() -> Arc<dyn PluginFactory> {
    Arc::new(crate::plugin::StaticPluginFactory::new(
        "conformance-root-answers-its-rows-tools",
        crate::facade_support::PluginSpec::new().with_tool_provider(Arc::new(RootRowsTools)),
    ))
}

fn tool_call(name: &str, call_id: &str) -> crate::LlmResponse {
    crate::LlmResponse {
        parts: vec![crate::LlmOutputPart::ToolCall {
            call_id: call_id.to_string(),
            tool_name: name.to_string(),
            input_json: "{}".to_string(),
            replay: None,
        }],
        ..crate::LlmResponse::default()
    }
}

fn text_response(text: &str) -> crate::LlmResponse {
    crate::LlmResponse {
        parts: vec![crate::LlmOutputPart::Text {
            text: text.to_string(),
            response_meta: None,
        }],
        ..crate::LlmResponse::default()
    }
}

/// Whether a tool already ran in the frame `request` renders.
fn a_tool_ran(request: &crate::LlmRequest) -> bool {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .any(|block| matches!(block, crate::llm::types::LlmContentBlock::ToolResult { .. }))
}

/// What the follow-on turn's checkpoint admits in the first execution.
#[derive(Clone, Copy)]
enum CheckpointRow {
    /// A steering input addressed to the follow-on turn.
    TurnInput,
    /// A ready process wake.
    QueuedWork,
}

/// The id of the row a law's checkpoint admits, recorded when it is enqueued.
#[derive(Default)]
struct LawRow(Mutex<Option<String>>);

impl LawRow {
    fn get(&self) -> Option<String> {
        self.0.lock_recover().clone()
    }

    fn set(&self, id: String) {
        *self.0.lock_recover() = Some(id);
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each write is established by the setup"
)]
async fn enqueue_checkpoint_row(
    store: &Arc<dyn crate::RuntimeStore>,
    session_id: &SessionId,
    row: CheckpointRow,
    follow_on: &TurnId,
    words: &str,
) -> String {
    match row {
        CheckpointRow::TurnInput => store
            .enqueue_pending_turn_input(crate::PendingTurnInputDraft::new(
                session_id.clone(),
                crate::TurnInputIngress::active_turn(
                    follow_on.clone(),
                    crate::TurnInputCheckpointBoundary::AfterWork,
                ),
                crate::TurnInput::text(words),
            ))
            .await
            .expect("enqueue the steering input addressed to the follow-on turn")
            .input_id
            .to_string(),
        CheckpointRow::QueuedWork => {
            let process_id = crate::ProcessId::fixture(&format!("{session_id}-producer"));
            let wake_id = format!("wake:{session_id}:1");
            let wake = crate::ProcessWakeDelivery {
                version: crate::FleetFormat::current().writer_version(lash_core::surface_format!(
                    PROCESS_WAKE_DELIVERY_FORMAT_VERSION
                )),
                wake_id: wake_id.clone(),
                target_session_id: session_id.clone(),
                process_id: process_id.clone(),
                sequence: 1,
                event_type: "producer.wake".to_string(),
                event_invocation: crate::RuntimeInvocation::effect(
                    crate::EffectAddress::new(
                        crate::ExecutionScope::process(process_id),
                        wake_id.clone(),
                    )
                    .expect("valid process wake address"),
                    crate::RuntimeAttribution::none(),
                    wake_id,
                ),
                process_caused_by: None,
                authority: crate::QueuedWorkAuthority::default(),
                input: words.to_string(),
                created_at_ms: 1,
            };
            store
                .enqueue_queued_work(crate::process_wake_batch_draft(wake))
                .await
                .expect("enqueue the ready process wake")
                .batch_id
                .to_string()
        }
    }
}

/// The first execution's model: the root's first turn switches frames, and
/// the follow-on turn enqueues the law's row, runs one step, and answers the
/// row its checkpoint delivered.
fn first_execution_model(
    store: Arc<dyn crate::RuntimeStore>,
    session_id: SessionId,
    row: CheckpointRow,
    follow_on: TurnId,
    words: String,
    law_row: Arc<LawRow>,
) -> crate::ProviderHandle {
    let calls = Arc::new(AtomicUsize::new(0));
    crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |_request| {
            let store = Arc::clone(&store);
            let session_id = session_id.clone();
            let follow_on = follow_on.clone();
            let words = words.clone();
            let law_row = Arc::clone(&law_row);
            let call = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(match call {
                    0 => tool_call("switch_frame", "root-answers-its-rows-switch"),
                    1 => {
                        law_row.set(
                            enqueue_checkpoint_row(&store, &session_id, row, &follow_on, &words)
                                .await,
                        );
                        tool_call("step", "root-answers-its-rows-step")
                    }
                    _ => text_response(FIRST_EXECUTION_REPLY),
                })
            }
        })
        .build()
        .into_handle()
}

/// Every later drive's model: one step, so a checkpoint follows it, then the
/// answer once a tool ran in the frame.
fn later_model() -> crate::ProviderHandle {
    crate::testing::TestProvider::builder()
        .kind("stub")
        .complete(move |request| {
            let answered = a_tool_ran(&request);
            async move {
                Ok(if answered {
                    text_response(RECOVERY_REPLY)
                } else {
                    tool_call("step", "root-answers-its-rows-later-step")
                })
            }
        })
        .build()
        .into_handle()
}

/// Whether `commit` settles the law's row.
fn settles(commit: &crate::store::RuntimeCommit, row: &str) -> bool {
    commit.ingress.as_ref().is_some_and(|settlement| {
        settlement
            .completed_inputs
            .iter()
            .any(|completion| completion.input_ids.iter().any(|id| id.as_str() == row))
            || settlement
                .completed_batches
                .iter()
                .any(|completion| completion.batch_ids.iter().any(|id| id.as_str() == row))
    })
}

/// A worker that dies at the commit that would settle the law's row: the
/// commit never reaches the store and the drive never returns.
struct CrashAtSettlingCommit {
    inner: Arc<dyn crate::RuntimeStore>,
    row: Arc<LawRow>,
    crash: crate::ConformanceCrash,
}

#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for CrashAtSettlingCommit {
    fn inner(&self) -> &(dyn crate::RuntimeStore + '_) {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        commit: crate::store::RuntimeCommit,
    ) -> Result<crate::store::RuntimeCommitReceipt, crate::StoreError> {
        if self.row.get().is_some_and(|row| settles(&commit, &row)) {
            self.crash.fire();
            return std::future::pending().await;
        }
        self.inner.commit_runtime_state(commit).await
    }
}

/// One admission that bound the law's row: the root it bound it to, and
/// whether the root that first admitted the row had terminal evidence then.
#[derive(Clone, Debug)]
struct RowBinding {
    root: TurnId,
    admitting_root_ended: bool,
}

/// Every drive's store: it records each admission — a root's or a
/// checkpoint's — that binds the law's row.
struct RowWitness {
    inner: Arc<dyn crate::RuntimeStore>,
    session_id: SessionId,
    row: Arc<LawRow>,
    bindings: Mutex<Vec<RowBinding>>,
}

impl RowWitness {
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the store reads its own roots"
    )]
    async fn witness(&self, root: &TurnId, bound: impl IntoIterator<Item = String>) {
        let Some(row) = self.row.get() else {
            return;
        };
        if !bound.into_iter().any(|id| id == row) {
            return;
        }
        let first = self
            .bindings
            .lock_recover()
            .first()
            .map(|binding| binding.root.clone());
        let admitting_root_ended = match &first {
            Some(first) => self
                .inner
                .root_terminal(&self.session_id, first)
                .await
                .expect("read the admitting root's terminal")
                .is_some(),
            None => false,
        };
        self.bindings.lock_recover().push(RowBinding {
            root: root.clone(),
            admitting_root_ended,
        });
    }

    fn bindings(&self) -> Vec<RowBinding> {
        self.bindings.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl crate::store::RuntimeStoreDecorator for RowWitness {
    fn inner(&self) -> &(dyn crate::RuntimeStore + '_) {
        self.inner.as_ref()
    }

    async fn admit_root(
        &self,
        request: &crate::store::AdmitRootRequest,
    ) -> Result<Option<crate::store::RootAdmission>, crate::StoreError> {
        let admission = self.inner.admit_root(request).await?;
        if let Some(admission) = &admission {
            let bound = admission
                .input_ids()
                .into_iter()
                .map(|id| id.to_string())
                .chain(admission.batch_ids().into_iter().map(|id| id.to_string()))
                .collect::<Vec<_>>();
            self.witness(&request.root, bound).await;
        }
        Ok(admission)
    }

    async fn admit_at_checkpoint(
        &self,
        request: &crate::store::CheckpointAdmissionRequest,
    ) -> Result<crate::store::CheckpointAdmission, crate::StoreError> {
        let admission = self.inner.admit_at_checkpoint(request).await?;
        let bound = admission
            .inputs
            .iter()
            .flat_map(|inputs| inputs.input_ids())
            .map(|id| id.to_string())
            .chain(
                admission
                    .queued
                    .iter()
                    .flat_map(|queued| queued.batch_ids())
                    .map(|id| id.to_string()),
            )
            .collect::<Vec<_>>();
        self.witness(&request.root, bound).await;
        Ok(admission)
    }
}

/// Everything a drive's runtime is built from.
#[derive(Clone)]
struct LawParts {
    session_id: SessionId,
    stores: Arc<dyn crate::StoreSet>,
    effect_host: Arc<dyn crate::EffectHost>,
}

impl LawParts {
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn runtime(
        &self,
        store: Arc<dyn crate::RuntimeStore>,
        model: crate::ProviderHandle,
    ) -> crate::LashRuntime {
        let mut host =
            crate::LawBackend::over_stores(Arc::clone(&self.stores), Arc::clone(&self.effect_host))
                .host_config(
                    crate::CommitBudget::bounded(1024 * 1024, 512),
                    crate::QueuedWorkBatchingConfig::new(1),
                );
        host.providers.provider_resolver = Arc::new(crate::SingleProviderResolver::new(model));
        let mut policy = crate::testing::mock_session_policy();
        policy.session_id = Some(self.session_id.clone());
        Box::pin(
            crate::LashRuntime::builder(host, crate::testing::runtime_lease_owner())
                .with_session_id(&self.session_id)
                .with_policy(policy)
                .with_plugin_factories(
                    crate::testing::test_standard_protocol_factories()
                        .into_iter()
                        .chain([tools_plugin()])
                        .collect(),
                )
                .with_store(store)
                .build(),
        )
        .await
        .expect("build the root-answers-its-rows runtime")
    }

    /// One drive of the session's next root on `store`, answered by `model`.
    fn drive(
        &self,
        store: Arc<dyn crate::RuntimeStore>,
        model: crate::ProviderHandle,
    ) -> crate::ConformanceTurnAttempt {
        let parts = self.clone();
        Arc::new(move |scope| {
            let parts = parts.clone();
            let store = Arc::clone(&store);
            let model = model.clone();
            Box::pin(async move {
                let mut runtime = parts.runtime(store, model).await;
                let drive = Box::pin(runtime.drive_next_queued_root(crate::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    scope,
                )))
                .await;
                crate::ConformanceTurnEnd::of(&drive)
            })
        })
    }

    fn scope(&self, name: &str) -> crate::AdmittedScope {
        admit(crate::ExecutionScope::queue_drain(&self.session_id, name))
    }
}

/// How many committed message parts of the session's current frame carry
/// `text`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn committed_mentions(store: &Arc<dyn crate::RuntimeStore>, text: &str) -> usize {
    crate::load_persisted_session_state(store.as_ref())
        .await
        .expect("read the committed session")
        .expect("the session has committed turns")
        .session_graph
        .read_model(None)
        .expect("the committed frame resolves")
        .messages
        .iter()
        .flat_map(|message| message.parts.iter())
        .filter(|part| part.content().contains(text))
        .count()
}

/// Whether the session has anything left to drive.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the store answers its own reads"
)]
async fn has_work(store: &Arc<dyn crate::RuntimeStore>, session_id: &SessionId) -> bool {
    !store
        .list_pending_turn_inputs(session_id)
        .await
        .expect("list pending inputs")
        .is_empty()
        || !store
            .list_queued_work(session_id)
            .await
            .expect("list queued work")
            .is_empty()
        || store
            .unfinished_root(session_id)
            .await
            .expect("read the unfinished root")
            .is_some()
}

/// The law for `row`: the first execution dies at the commit that would
/// settle the row its follow-on admitted; a drive on a fresh journal runs
/// with a peer's input waiting; the dead drive is redriven on its journal;
/// later drives run until the session is idle. Then the row is answered once,
/// no other root was bound to it while its root was unfinished, and the
/// peer's input is answered by the peer's root.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn a_row_is_answered_only_by_its_root(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    row: CheckpointRow,
    words: &str,
) {
    let session_id = SessionId::from(format!("{prefix}-root-answers-its-rows"));
    let raw = crate::conformance::law_session_store(stores.as_ref(), &session_id).await;
    let law_row = Arc::new(LawRow::default());
    let witness = Arc::new(RowWitness {
        inner: Arc::clone(&raw),
        session_id: session_id.clone(),
        row: Arc::clone(&law_row),
        bindings: Mutex::new(Vec::new()),
    });
    let store = Arc::clone(&witness) as Arc<dyn crate::RuntimeStore>;
    let parts = LawParts {
        session_id: session_id.clone(),
        stores,
        effect_host,
    };
    let root = TurnId::from(format!("{prefix}-root"));
    let follow_on = crate::store::PhysicalTurn::derive_turn_id(&root, 1);
    let peer = TurnId::from(format!("{prefix}-peer"));
    raw.enqueue_pending_turn_input(
        crate::PendingTurnInputDraft::new(
            session_id.clone(),
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("start the root"),
        )
        .with_source_key(root.as_str()),
    )
    .await
    .expect("accept the root's input");

    // 1. The first execution dies at the commit that would settle the row.
    let crash = crate::ConformanceCrash::new();
    let dying: Arc<dyn crate::RuntimeStore> = Arc::new(CrashAtSettlingCommit {
        inner: Arc::clone(&store),
        row: Arc::clone(&law_row),
        crash: crash.clone(),
    });
    let first_scope = parts.scope(&format!("{prefix}-first-drive"));
    tokio::time::timeout(
        DRIVE_TIMEOUT,
        runner.run_turn_until_crash(
            first_scope.clone(),
            parts.drive(
                dying,
                first_execution_model(
                    Arc::clone(&raw),
                    session_id.clone(),
                    row,
                    follow_on.clone(),
                    words.to_string(),
                    Arc::clone(&law_row),
                ),
            ),
            crash,
        ),
    )
    .await
    .expect("the first execution reaches the commit that settles its row");
    let row_id = law_row.get().expect("the follow-on enqueued its row");
    let bindings = witness.bindings();
    assert!(
        bindings.first().is_some_and(|binding| binding.root == root),
        "the follow-on's checkpoint bound the row to its root: {bindings:?}"
    );

    // 2. A drive on a fresh journal runs with a peer's input waiting.
    raw.enqueue_pending_turn_input(
        crate::PendingTurnInputDraft::new(
            session_id.clone(),
            crate::TurnInputIngress::NextTurn,
            crate::TurnInput::text("the peer's own input"),
        )
        .with_source_key(peer.as_str()),
    )
    .await
    .expect("accept the peer's input");
    tokio::time::timeout(
        DRIVE_TIMEOUT,
        runner.run_turn(
            parts.scope(&format!("{prefix}-peer-drive")),
            parts.drive(Arc::clone(&store), later_model()),
        ),
    )
    .await
    .expect("the peer's drive ends");

    // 3. The dead drive is redriven on its own journal.
    tokio::time::timeout(
        DRIVE_TIMEOUT,
        runner.run_turn(first_scope, parts.drive(Arc::clone(&store), later_model())),
    )
    .await
    .expect("the redrive ends");

    // 4. Later drives run until the session is idle.
    let mut drives = 0;
    while has_work(&raw, &session_id).await {
        assert!(
            drives < 4,
            "later drives make no progress: {:?} {:?}",
            raw.list_pending_turn_inputs(&session_id).await,
            raw.list_queued_work(&session_id).await
        );
        drives += 1;
        tokio::time::timeout(
            DRIVE_TIMEOUT,
            runner.run_turn(
                parts.scope(&format!("{prefix}-later-drive-{drives}")),
                parts.drive(Arc::clone(&store), later_model()),
            ),
        )
        .await
        .expect("a later drive ends");
    }

    let bindings = witness.bindings();
    for binding in &bindings {
        assert!(
            binding.root == root || binding.admitting_root_ended,
            "no other root is bound to the row while its root is unfinished: {bindings:?}"
        );
    }
    let applications = raw
        .list_turn_input_applications(&session_id)
        .await
        .expect("read the applications");
    match row {
        CheckpointRow::TurnInput => {
            let answered = applications
                .iter()
                .filter(|application| application.input_id.as_str() == row_id)
                .map(|application| application.turn_id.clone())
                .collect::<Vec<_>>();
            assert_eq!(
                answered.len(),
                1,
                "the input is answered once: {answered:?}"
            );
        }
        CheckpointRow::QueuedWork => assert!(
            raw.list_queued_work(&session_id)
                .await
                .expect("list queued work")
                .iter()
                .all(|batch| batch.batch_id.as_str() != row_id),
            "the wake is settled"
        ),
    }
    assert_eq!(
        committed_mentions(&raw, words).await,
        1,
        "the row is committed once"
    );
    assert_eq!(
        committed_mentions(&raw, FIRST_EXECUTION_REPLY).await,
        0,
        "the peer's drive sealed a later drive epoch, so the dead drive's redrive \
         writes nothing: its journaled answer is never committed"
    );
    assert!(
        applications
            .iter()
            .any(|application| application.turn_id == peer),
        "the peer's input is answered by the peer's root: {applications:?}"
    );
}

/// A steering input the follow-on's checkpoint admitted is answered only by
/// its root, once.
pub async fn a_checkpoint_input_is_answered_only_by_the_root_that_admitted_it(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    Box::pin(a_row_is_answered_only_by_its_root(
        prefix,
        effect_host,
        stores,
        runner,
        CheckpointRow::TurnInput,
        "steer the follow-on turn once",
    ))
    .await;
}

/// A process wake the follow-on's checkpoint admitted is answered only by
/// its root, once.
pub async fn a_checkpoint_wake_is_answered_only_by_the_root_that_admitted_it(
    prefix: &str,
    effect_host: Arc<dyn crate::EffectHost>,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    Box::pin(a_row_is_answered_only_by_its_root(
        prefix,
        effect_host,
        stores,
        runner,
        CheckpointRow::QueuedWork,
        "the producer woke the session once",
    ))
    .await;
}

/// Register the laws of a row admitted to one root being answered only by
/// that root (FIG-3552, FIG-3927). The fixture hands back a guard, a prefix,
/// the tier's effect host, the store set under test and its
/// [`ConformanceTurnRunner`](crate::ConformanceTurnRunner).
#[macro_export]
macro_rules! root_answers_its_rows_tests {
    ($(#[$attr:meta])* $fixture:block) => {
        $crate::root_answers_its_rows_tests!(@law [$(#[$attr])*] $fixture;
            (a_checkpoint_input_is_answered_only_by_the_root_that_admitted_it, "root-rows-input"));
        $crate::root_answers_its_rows_tests!(@law [$(#[$attr])*] $fixture;
            (a_checkpoint_wake_is_answered_only_by_the_root_that_admitted_it, "root-rows-wake"));
    };
    (@law [$(#[$attr:meta])*] $fixture:block; ($law:ident, $label:literal)) => {
        $(#[$attr])*
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $law() {
            let (_guard, prefix, host, stores, runner) = $fixture;
            let _ = $label;
            $crate::registration_macro_support::$law(prefix, host, stores, runner).await;
        }
    };
}
