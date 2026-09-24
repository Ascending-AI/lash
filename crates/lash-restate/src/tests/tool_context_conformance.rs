use super::*;
use lash_core::facade_support::RuntimeSessionStateFacadeOps;
use lash_sansio::SessionId;
use lash_sansio::TurnId;

use lash_core::{ToolCall, ToolProvider};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct CountingFirstPartyProvider {
    inner: Arc<dyn ToolProvider>,
    executions: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ToolProvider for CountingFirstPartyProvider {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        self.inner.tool_manifests()
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        self.inner.resolve_contract(name)
    }

    async fn execute(&self, call: ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        (async {
            self.executions.fetch_add(1, Ordering::SeqCst);
            self.inner.execute(call).await
        })
        .await
    }
}

fn args_for(tool_name: &str) -> serde_json::Value {
    match tool_name {
        "llm_query" => serde_json::json!({
            "task": "Return the covered answer",
            "inputs": {"answer": "covered"},
            "output": {"answer": "str"}
        }),
        other => panic!(
            "first-party tool `{other}` was registered without a conformance fixture; add its arguments before merging"
        ),
    }
}

fn typescript_source_for(tool_name: &str) -> &'static str {
    match tool_name {
        "llm_query" => {
            r#"<typescript>
const result = await llm.query({
  task: "Return the covered answer",
  inputs: { answer: "covered" },
  output: { answer: "str" }
});
finish(result);
</typescript>"#
        }
        other => panic!(
            "first-party tool `{other}` was registered without a production TypeScript fixture; add its caller path before merging"
        ),
    }
}

/// The live pass's store refuses every commit, so its worker dies at its final commit: every effect ran and was
/// journaled, and nothing was committed.
struct RefusesEveryCommitStore {
    inner: Arc<dyn lash_core::RuntimePersistence>,
}

#[async_trait::async_trait]
impl lash_core::store::RuntimePersistenceDecorator for RefusesEveryCommitStore {
    fn inner(&self) -> &(dyn lash_core::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        _commit: lash_core::store::RuntimeCommit,
    ) -> Result<lash_core::store::RuntimeCommitReceipt, lash_core::StoreError> {
        Err(lash_core::StoreError::Backend(
            "the live worker died at its final commit".to_string(),
        ))
    }
}

struct ProductionToolCell {
    _dir: tempfile::TempDir,
    session_id: SessionId,
    turn_id: TurnId,
    policy: lash_core::SessionPolicy,
    initial_state: lash_core::RuntimeSessionState,
    host: lash_core::facade_support::RuntimeHostConfig,
    runtime_store: Arc<dyn lash_core::RuntimePersistence>,
    plugin_factories: Vec<Arc<dyn lash_core::facade_support::PluginFactory>>,
    tool_executions: Arc<AtomicUsize>,
    llm_provider_calls: Arc<AtomicUsize>,
}

enum ControllerMode {
    Local,
    Durable,
}

/// The `llm_query` direct completion's answer, as the provider returns it.
const LLM_QUERY_ANSWER: &str = r#"{"kind":"value","value":{"answer":"covered"},"error":null}"#;

impl ProductionToolCell {
    async fn new(mode: ControllerMode, tool_name: &str) -> Self {
        let script = vec![
            typescript_source_for(tool_name).to_string(),
            LLM_QUERY_ANSWER.to_string(),
        ];
        Self::scripted(mode, tool_name, script).await
    }

    /// A cell whose provider answers call `n` with `script[n]` and refuses
    /// any call past the script: a re-issued call on replay panics.
    async fn scripted(mode: ControllerMode, tool_name: &str, script: Vec<String>) -> Self {
        let context_name = match mode {
            ControllerMode::Local => "local",
            ControllerMode::Durable => "restate-durable",
        };
        let session_id = SessionId::from(format!("tool-context-{context_name}-{tool_name}"));
        let turn_id = TurnId::from(format!("{session_id}-turn"));
        let dir = tempfile::tempdir().expect("tool-context tempdir");
        let first_party: Arc<dyn ToolProvider> =
            Arc::new(lash_llm_tools::llm_query_provider(None, None, None));
        let tool_executions = Arc::new(AtomicUsize::new(0));
        let counting_provider: Arc<dyn ToolProvider> = Arc::new(CountingFirstPartyProvider {
            inner: first_party,
            executions: Arc::clone(&tool_executions),
        });
        let tool_plugin: Arc<dyn lash_core::facade_support::PluginFactory> =
            Arc::new(lash_core::plugin::StaticPluginFactory::new(
                "tool-context-first-party",
                lash_core::facade_support::PluginSpec::new().with_tool_provider(counting_provider),
            ));
        let artifact_backend = lash_sqlite_store::SqliteBackend::memory()
            .await
            .expect("open the artifact backend");
        let rlm_plugin: Arc<dyn lash_core::facade_support::PluginFactory> = Arc::new(
            lash_protocol_rlm::RlmProtocolPluginFactory::new(
                lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                    .channel(lash_protocol_rlm::RlmChannel::Cell)
                    .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
                    .wall_clock(lash_protocol_rlm::WallClockBound::secs(30))
                    .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                    .build(),
                &artifact_backend,
            )
            .with_process_lifecycle(false),
        );
        let plugin_factories = vec![rlm_plugin, tool_plugin];

        let llm_provider_calls = Arc::new(AtomicUsize::new(0));
        let script = Arc::new(script);
        let provider = lash_core::testing::TestProvider::builder()
            .kind("stub")
            .complete({
                let llm_provider_calls = Arc::clone(&llm_provider_calls);
                move |_request| {
                    let llm_provider_calls = Arc::clone(&llm_provider_calls);
                    let script = Arc::clone(&script);
                    async move {
                        let call = llm_provider_calls.fetch_add(1, Ordering::SeqCst);
                        let Some(text) = script.get(call).cloned() else {
                            panic!(
                                "live+replay must not execute an unjournaled provider call #{call}"
                            );
                        };
                        Ok(lash_core::LlmResponse {
                            parts: vec![lash_core::LlmOutputPart::Text {
                                text,
                                response_meta: None,
                            }],
                            response_metadata: Default::default(),
                            ..lash_core::LlmResponse::default()
                        })
                    }
                }
            })
            .build()
            .into_handle();
        let mut host = memory_host_config().await;
        host.providers.provider_resolver = Arc::new(
            lash_core::facade_support::SingleProviderResolver::new(provider),
        );
        host.durability.attachment_store = Arc::new(
            lash_core::facade_support::SessionAttachmentStore::ephemeral(Arc::new(
                DurableMemoryAttachmentStore::default(),
            )),
        );
        host.durability.process_env_store = Arc::new(DurableMemoryProcessEnvStore::default());

        let store = Arc::new(
            lash_sqlite_store::Store::open(&dir.path().join("session.db"))
                .await
                .expect("open production-path session store"),
        );
        let runtime_store: Arc<dyn lash_core::RuntimePersistence> = store;
        let policy = replay_test_policy(&session_id);
        let initial_state = replay_test_state(&session_id, &policy);
        Self {
            _dir: dir,
            session_id,
            turn_id,
            policy,
            initial_state,
            host,
            runtime_store,
            plugin_factories,
            tool_executions,
            llm_provider_calls,
        }
    }

    async fn run_once(
        &self,
        runtime: &mut lash_core::facade_support::LashRuntime,
        effect_host: &dyn EffectHost,
    ) -> Result<lash_core::facade_support::AssembledTurn, lash_core::RuntimeError> {
        let turn_scope = runtime.export_persistence_state().turn_scope(&self.turn_id);
        let scoped_effect_controller = effect_host
            .scoped(durable_admission(&turn_scope))
            .expect("scope production tool cell");
        runtime
            .stream_turn(
                replay_test_input(&self.turn_id),
                lash_core::facade_support::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    scoped_effect_controller,
                ),
            )
            .await
    }

    async fn run(&self, effect_host: &dyn EffectHost, start_replay: impl FnOnce()) {
        let mut live = replay_test_runtime_with_plugins(
            &self.session_id,
            self.policy.clone(),
            self.initial_state.clone(),
            self.host.clone(),
            Arc::new(RefusesEveryCommitStore {
                inner: Arc::clone(&self.runtime_store),
            }),
            self.plugin_factories.clone(),
        )
        .await;
        self.run_once(&mut live, effect_host)
            .await
            .expect_err("the live worker dies at its final commit");
        assert_eq!(
            self.tool_executions.load(Ordering::SeqCst),
            1,
            "the real caller must execute the first-party tool once on the live pass"
        );

        // The replay is the same handler redriven against the same store: it
        // drives the journaled acceptance and drive set (ADR 0069 §6), replays
        // every journaled effect, and commits the turn the live pass could not.
        start_replay();
        let mut replay = replay_test_runtime_with_plugins(
            &self.session_id,
            self.policy.clone(),
            self.initial_state.clone(),
            self.host.clone(),
            Arc::clone(&self.runtime_store),
            self.plugin_factories.clone(),
        )
        .await;
        let replay_turn = self
            .run_once(&mut replay, effect_host)
            .await
            .expect("the replay commits the production tool cell");
        assert!(matches!(
            replay_turn.outcome,
            lash_core::facade_support::TurnOutcome::Finished(_)
        ));
        let replay_state = replay
            .snapshot_execution_state()
            .await
            .expect("snapshot replayed execution state");
        // The live pass never committed, so its resident state was rolled back;
        // the replay re-runs the cell, so it holds the cell's global. (The
        // crash-redrive tests below compare it byte for byte to a clean run.)
        assert_binds_result_global(replay_state.as_ref());
        assert_eq!(
            self.tool_executions.load(Ordering::SeqCst),
            1,
            "the caller-emitted ToolAttempt must replay without re-executing the first-party tool"
        );
        assert_eq!(
            self.llm_provider_calls.load(Ordering::SeqCst),
            2,
            "outer RLM generation and llm_query direct completion must each execute only on the live pass"
        );
    }
}

#[tokio::test]
async fn every_registered_first_party_tool_succeeds_and_replays_in_every_context() {
    let provider: Arc<dyn ToolProvider> =
        Arc::new(lash_llm_tools::llm_query_provider(None, None, None));
    let manifests = provider.tool_manifests();
    assert!(
        !manifests.is_empty(),
        "the first-party tool registry must not be empty"
    );

    for manifest in manifests {
        let _ = args_for(&manifest.name);

        let (local_cell, local) = ProductionToolCell::sqlite(&manifest.name).await;
        local_cell
            .run(local.as_ref(), || local.start_replay())
            .await;

        let mut durable_cell =
            ProductionToolCell::new(ControllerMode::Durable, &manifest.name).await;
        let context = Arc::new(ReplayableRecordingContext::default());
        let durable = Arc::new(RestateRuntimeEffectController::new_for_test(Arc::clone(
            &context,
        )));
        durable_cell.host.control.effect_host = Arc::clone(&durable) as Arc<dyn EffectHost>;
        durable_cell
            .run(durable.as_ref(), || context.start_replay())
            .await;
        let tool_attempts = context
            .recorded_runtime_effect_envelopes()
            .into_iter()
            .filter(|(_, envelope)| {
                matches!(
                    &envelope.command,
                    RuntimeEffectCommand::ToolAttempt { call, .. }
                        if call.tool_name == manifest.name
                )
            })
            .count();
        assert_eq!(
            tool_attempts, 1,
            "the production durable caller must emit one ToolAttempt for {}",
            manifest.name
        );
    }
}

/// The production cell binds `const result`, so its interpreter state names it.
fn assert_binds_result_global(state: Option<&lash_core::plugin::HydratedExecutionState>) {
    let state = state.expect("an RLM turn leaves execution state");
    assert!(
        String::from_utf8_lossy(&state.root).contains("result"),
        "the cell's `result` global must be in the execution state root"
    );
}

/// Fails the first turn-final commit before it reaches the store: the crash
/// window between the last journaled effect and the durable head.
struct CrashAtFinalCommit {
    inner: Arc<dyn lash_core::RuntimePersistence>,
    armed: AtomicBool,
}

#[async_trait::async_trait]
impl lash_core::store::RuntimePersistenceDecorator for CrashAtFinalCommit {
    fn inner(&self) -> &(dyn lash_core::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        commit: lash_core::store::RuntimeCommit,
    ) -> Result<lash_core::store::RuntimeCommitReceipt, lash_core::StoreError> {
        if commit.turn_commit.operation.key == "final" && self.armed.swap(false, Ordering::SeqCst) {
            return Err(lash_core::StoreError::Backend(
                "injected crash at the turn-final commit".to_string(),
            ));
        }
        self.inner.commit_runtime_state(commit).await
    }
}

impl ProductionToolCell {
    /// A cell on the SQLite effect host with its session bound, ready to run.
    async fn sqlite(tool_name: &str) -> (Self, Arc<lash_sqlite_store::SqliteEffectHost>) {
        let script = vec![
            typescript_source_for(tool_name).to_string(),
            LLM_QUERY_ANSWER.to_string(),
        ];
        Self::sqlite_scripted(tool_name, script).await
    }

    async fn sqlite_scripted(
        tool_name: &str,
        script: Vec<String>,
    ) -> (Self, Arc<lash_sqlite_store::SqliteEffectHost>) {
        let mut cell = Self::scripted(ControllerMode::Local, tool_name, script).await;
        cell.runtime_store
            .admit_and_bind_session(&lash_core::SessionBinding::root(cell.session_id.clone()))
            .await
            .expect("bind local session");
        let host = Arc::new(
            lash_sqlite_store::SqliteEffectHost::open(&cell._dir.path().join("effects.db"))
                .await
                .expect("in-process production replay host"),
        );
        cell.host.control.effect_host = Arc::clone(&host) as Arc<dyn EffectHost>;
        (cell, host)
    }

    async fn runtime_on(
        &self,
        store: Arc<dyn lash_core::RuntimePersistence>,
    ) -> lash_core::facade_support::LashRuntime {
        replay_test_runtime_with_plugins(
            &self.session_id,
            self.policy.clone(),
            self.initial_state.clone(),
            self.host.clone(),
            store,
            self.plugin_factories.clone(),
        )
        .await
    }

    /// The execution state of the store's durable head, restored the way a
    /// fresh process restores it: from the head, with no caller-supplied state.
    async fn committed_execution_state(&self) -> Option<lash_core::plugin::HydratedExecutionState> {
        let mut runtime = self.runtime_from_head().await;
        runtime
            .snapshot_execution_state()
            .await
            .expect("snapshot committed execution state")
    }

    /// A runtime restored from the store's durable head, the way a fresh
    /// worker restores it: no caller-supplied state.
    async fn runtime_from_head(&self) -> lash_core::facade_support::LashRuntime {
        Box::pin(
            lash_core::facade_support::LashRuntime::builder(
                self.host.clone(),
                lash_core::LeaseOwnerIdentity::opaque(
                    "lash-restate-head-reader",
                    "lash-restate-head-reader-boot",
                ),
            )
            .with_session_id(&self.session_id)
            .with_policy(self.policy.clone())
            .with_plugin_factories(self.plugin_factories.clone())
            .with_store(Arc::clone(&self.runtime_store))
            .build(),
        )
        .await
        .expect("open a runtime on the committed head")
    }
}

/// FIG-3549: a crash between the last journaled effect and the turn-final
/// commit, then a same-store redrive in strict replay, commits the live pass's
/// interpreter state. The redrive re-runs every code cell (ADR 0103); each
/// cell's nested calls answer from the journal on the same replay keys — a
/// strict-replay miss would fail the turn — so no provider call is re-issued.
async fn assert_crash_at_final_commit_redrive_commits_the_live_state(script: Vec<String>) {
    let provider_calls = script.len();
    let (control, control_host) =
        ProductionToolCell::sqlite_scripted("llm_query", script.clone()).await;
    let mut control_runtime = control.runtime_on(Arc::clone(&control.runtime_store)).await;
    let control_turn = control
        .run_once(&mut control_runtime, control_host.as_ref())
        .await
        .expect("the control turn commits");
    assert!(matches!(
        control_turn.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    drop(control_runtime);
    assert_eq!(
        control.llm_provider_calls.load(Ordering::SeqCst),
        provider_calls
    );
    let live_head = control.committed_execution_state().await;
    assert!(live_head.is_some(), "an RLM turn leaves execution state");

    let (cell, host) = ProductionToolCell::sqlite_scripted("llm_query", script).await;
    let crashing: Arc<dyn lash_core::RuntimePersistence> = Arc::new(CrashAtFinalCommit {
        inner: Arc::clone(&cell.runtime_store),
        armed: AtomicBool::new(true),
    });
    let mut crashed = cell.runtime_on(Arc::clone(&crashing)).await;
    let turn_scope = crashed.export_persistence_state().turn_scope(&cell.turn_id);
    let crashed_turn = crashed
        .stream_turn(
            replay_test_input(&cell.turn_id),
            lash_core::facade_support::TurnOptions::new(
                tokio_util::sync::CancellationToken::new(),
                host.scoped(durable_admission(&turn_scope))
                    .expect("scope crashing tool cell"),
            ),
        )
        .await;
    assert!(
        !matches!(
            crashed_turn.as_ref().map(|turn| &turn.outcome),
            Ok(lash_core::facade_support::TurnOutcome::Finished(_))
        ),
        "the injected crash must stop the turn-final commit"
    );
    drop(crashed);
    let tool_executions = cell.tool_executions.load(Ordering::SeqCst);
    assert_eq!(
        cell.llm_provider_calls.load(Ordering::SeqCst),
        provider_calls
    );

    host.start_replay();
    let mut redrive = cell.runtime_on(Arc::clone(&cell.runtime_store)).await;
    let redriven = cell
        .run_once(&mut redrive, host.as_ref())
        .await
        .expect("the redrive commits");
    assert!(matches!(
        redriven.outcome,
        lash_core::facade_support::TurnOutcome::Finished(_)
    ));
    assert_eq!(
        redrive
            .snapshot_execution_state()
            .await
            .expect("snapshot redriven execution state"),
        live_head,
        "the redrive re-runs the cells and holds the live interpreter state"
    );
    drop(redrive);
    assert_eq!(
        cell.committed_execution_state().await,
        live_head,
        "the redriven turn commits the live pass's execution state byte for byte"
    );
    assert_eq!(
        cell.tool_executions.load(Ordering::SeqCst),
        tool_executions,
        "every ToolAttempt replays from the journal"
    );
    assert_eq!(
        cell.llm_provider_calls.load(Ordering::SeqCst),
        provider_calls,
        "the redrive re-issues neither an outer generation nor a nested llm_query call"
    );
}

#[tokio::test]
async fn redrive_after_a_crash_at_final_commit_commits_the_live_execution_state() {
    Box::pin(assert_crash_at_final_commit_redrive_commits_the_live_state(
        vec![
            typescript_source_for("llm_query").to_string(),
            LLM_QUERY_ANSWER.to_string(),
        ],
    ))
    .await;
}

/// A loop issuing several nested calls: each iteration's call is keyed by its
/// call site and per-VM occurrence, so the re-run reaches the same keys in
/// the same order.
#[tokio::test]
async fn redrive_re_runs_a_loop_of_nested_calls_on_stable_keys() {
    let cell = r#"<typescript>
let answers = "";
for (let i = 0; i < 3; i++) {
  const reply = await llm.query({
    task: "Return the covered answer",
    inputs: { answer: "covered" },
    output: { answer: "str" }
  });
  answers = answers + reply.answer;
}
finish(answers);
</typescript>"#;
    Box::pin(assert_crash_at_final_commit_redrive_commits_the_live_state(
        vec![
            cell.to_string(),
            LLM_QUERY_ANSWER.to_string(),
            LLM_QUERY_ANSWER.to_string(),
            LLM_QUERY_ANSWER.to_string(),
        ],
    ))
    .await;
}

/// Two cells in one turn: the second cell reads the first cell's global, so
/// the re-run must rebuild the first cell's state before the second runs, and
/// each cell's nested call keeps its own key.
#[tokio::test]
async fn redrive_re_runs_two_cells_of_one_turn_on_stable_keys() {
    let first = r#"<typescript>
const first = await llm.query({
  task: "Return the covered answer",
  inputs: { answer: "covered" },
  output: { answer: "str" }
});
print(first.answer);
</typescript>"#;
    let second = r#"<typescript>
const second = await llm.query({
  task: "Return the covered answer again",
  inputs: { answer: first.answer },
  output: { answer: "str" }
});
finish(second);
</typescript>"#;
    Box::pin(assert_crash_at_final_commit_redrive_commits_the_live_state(
        vec![
            first.to_string(),
            LLM_QUERY_ANSWER.to_string(),
            second.to_string(),
            LLM_QUERY_ANSWER.to_string(),
        ],
    ))
    .await;
}

/// Every `(scope_id, replay_key)` of a code cell the journal's rows name.
/// The cell's own row is gone (ADR 0103), but each turn effect's key is
/// `{turn prefix}{kind}:{ordinal}`, and the cell's nested rows extend the
/// cell's key, so a row keyed `{turn prefix}exec_code:{n}:...` names cell `n`.
fn journaled_exec_code_addresses(effects_db: &std::path::Path) -> Vec<(String, String)> {
    let connection = rusqlite::Connection::open(effects_db).expect("open the effect journal");
    let mut statement = connection
        .prepare("SELECT scope_id, replay_key FROM runtime_effect_replay")
        .expect("prepare the journal scan");
    let rows: Vec<(String, String)> = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("scan the journal")
        .map(|row| row.expect("journal row"))
        .collect();
    let turn_prefixes: std::collections::BTreeSet<(String, String)> = rows
        .iter()
        .filter_map(|(scope_id, key)| {
            key.find("sync_execution_environment:")
                .map(|at| (scope_id.clone(), key[..at].to_string()))
        })
        .collect();
    let mut addresses = std::collections::BTreeSet::new();
    for (scope_id, prefix) in &turn_prefixes {
        let cell_prefix = format!("{prefix}exec_code:");
        for (row_scope, key) in &rows {
            let Some(rest) = key.strip_prefix(&cell_prefix) else {
                continue;
            };
            if row_scope != scope_id {
                continue;
            }
            let ordinal: String = rest.chars().take_while(char::is_ascii_digit).collect();
            addresses.insert((scope_id.clone(), format!("{cell_prefix}{ordinal}")));
        }
    }
    addresses.into_iter().collect()
}

/// Seed the row a pre-ADR-0103 worker leaves when it crashes mid-cell: the
/// cell's own journal row, claimed and never finalized, under a lease that
/// has long expired. Nothing in this build claims an `ExecCode` row again.
fn seed_abandoned_exec_code_row(effects_db: &std::path::Path, scope_id: &str, replay_key: &str) {
    let connection = rusqlite::Connection::open(effects_db).expect("open the effect journal");
    connection
        .execute(
            "INSERT INTO runtime_effect_replay (
                 scope_id, session_id, replay_key, envelope_hash, envelope_json, status,
                 lease_owner_id, lease_token, lease_expires_at_ms, commit_state,
                 created_at_ms, updated_at_ms
             ) SELECT scope_id, session_id, ?2, 'pre-cutover-exec-code', envelope_json,
                      'in_progress', 'crashed-old-build-worker', 'crashed-lease', 1,
                      'pending', 1, 1
               FROM runtime_effect_replay WHERE scope_id = ?1 LIMIT 1",
            rusqlite::params![scope_id, replay_key],
        )
        .expect("seed the crashed worker's exec_code row");
}

async fn drive_drain(
    runtime: &mut lash_core::facade_support::LashRuntime,
    host: &dyn EffectHost,
    drain_scope: &lash_core::ExecutionScope,
) -> Result<(), lash_core::RuntimeError> {
    let scope = host
        .scoped(durable_admission(drain_scope))
        .expect("scope the drain");
    runtime
        .stream_next_queued_work(lash_core::facade_support::TurnOptions::new(
            tokio_util::sync::CancellationToken::new(),
            scope,
        ))
        .await
        .map(|_| ())
}

/// FIG-3549 review F1: an `in_progress` `exec_code` row left by a pre-cutover
/// worker that crashed mid-cell must not wedge the queue-drain end. The
/// re-executed cell discards the row at its own address before it runs, so
/// the drain's scope reads quiescent and the retried drain writes its end.
#[tokio::test]
async fn a_pre_cutover_in_progress_cell_row_does_not_wedge_the_drain_end() {
    let (cell, host) = ProductionToolCell::sqlite("llm_query").await;
    cell.runtime_store
        .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
            cell.session_id.clone(),
            lash_core::TurnInputIngress::NextTurn,
            replay_test_input(&cell.turn_id),
        ))
        .await
        .expect("seed the drain's turn input");
    let drain_id = "pre-cutover-cell-drain";
    let drain_scope = lash_core::ExecutionScope::queue_drain(cell.session_id.clone(), drain_id);

    // The worker dies at the drain run's turn-final commit: the cell and its
    // nested effects are journaled, and the drain owns no end yet.
    let crashing: Arc<dyn lash_core::RuntimePersistence> = Arc::new(CrashAtFinalCommit {
        inner: Arc::clone(&cell.runtime_store),
        armed: AtomicBool::new(true),
    });
    let mut crashed = cell.runtime_on(Arc::clone(&crashing)).await;
    let _ = drive_drain(&mut crashed, host.as_ref(), &drain_scope).await;
    drop(crashed);
    assert!(
        !lash_core::SessionCommitStore::drain_end_exists(cell.runtime_store.as_ref(), drain_id)
            .await
            .expect("read the drain-end receipt"),
        "the crashed drain has not ended"
    );

    // What an old build would have left at the same point, had it crashed
    // mid-cell: the cell's own row, `in_progress` forever.
    let effects_db = cell._dir.path().join("effects.db");
    let addresses = journaled_exec_code_addresses(&effects_db);
    assert!(
        !addresses.is_empty(),
        "the crashed run journaled the cell's nested effects"
    );
    for (scope_id, replay_key) in &addresses {
        seed_abandoned_exec_code_row(&effects_db, scope_id, replay_key);
    }
    let closing = host
        .effect_group_closing()
        .expect("the SQLite host has a closing seam");
    assert!(
        !closing
            .scope_is_quiescent(&drain_scope)
            .await
            .expect("read quiescence"),
        "the seeded row holds the drain scope non-quiescent"
    );

    // The retried drain re-runs the cell over its journaled nested effects,
    // discards the leftover row, and ends.
    let mut retried = cell.runtime_on(Arc::clone(&cell.runtime_store)).await;
    drive_drain(&mut retried, host.as_ref(), &drain_scope)
        .await
        .expect("the retried drain runs");
    assert!(
        closing
            .scope_is_quiescent(&drain_scope)
            .await
            .expect("read quiescence"),
        "the re-executed cell removed the pre-cutover row"
    );
    assert!(
        lash_core::SessionCommitStore::drain_end_exists(cell.runtime_store.as_ref(), drain_id)
            .await
            .expect("read the drain-end receipt"),
        "the retried drain writes its end"
    );
    assert_eq!(
        cell.llm_provider_calls.load(Ordering::SeqCst),
        2,
        "the retry re-issues no provider call"
    );
}

/// Lets the drain's turn-final commit land, then never returns: the worker
/// dies after its commit, before the drain ends.
struct DiesAfterFinalCommit {
    inner: Arc<dyn lash_core::RuntimePersistence>,
    armed: AtomicBool,
    committed: Arc<tokio::sync::Notify>,
}

#[async_trait::async_trait]
impl lash_core::store::RuntimePersistenceDecorator for DiesAfterFinalCommit {
    fn inner(&self) -> &(dyn lash_core::RuntimePersistence + '_) {
        self.inner.as_ref()
    }

    async fn commit_runtime_state(
        &self,
        commit: lash_core::store::RuntimeCommit,
    ) -> Result<lash_core::store::RuntimeCommitReceipt, lash_core::StoreError> {
        let dies =
            commit.turn_commit.operation.key == "final" && self.armed.swap(false, Ordering::SeqCst);
        let receipt = self.inner.commit_runtime_state(commit).await?;
        if dies {
            self.committed.notify_one();
            return std::future::pending().await;
        }
        Ok(receipt)
    }
}

/// Expire the lane a crashed worker still holds, so the redrive's claim
/// displaces it instead of waiting out its term.
async fn expire_crashed_worker_lane(
    store: &dyn lash_core::RuntimePersistence,
    session_id: &SessionId,
) {
    if let Some(lease) = store
        .get_session_execution_lease(session_id)
        .await
        .expect("read the crashed worker's lane")
        .lease
    {
        store
            .renew_session_execution_lease(&lease.authority(), 1)
            .await
            .expect("shorten the crashed worker's lane to its minimum term");
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
}

/// FIG-3590: a worker that dies after its drain's turn-final commit and
/// before the drain ends is redriven, in strict replay on the same store,
/// with the same drain identity. The redrive replays the settled run's
/// receipt: the committed execution state stays byte-identical to a clean
/// run's, and no tool or provider call is issued again.
///
/// `from_head` restores the redriven runtime from the durable head; otherwise
/// it starts from the pre-turn state the crashed worker started from.
async fn assert_after_commit_drain_redrive_keeps_the_committed_state(from_head: bool) {
    async fn seed_drain_input(cell: &ProductionToolCell) {
        cell.runtime_store
            .enqueue_pending_turn_input(lash_core::PendingTurnInputDraft::new(
                cell.session_id.clone(),
                lash_core::TurnInputIngress::NextTurn,
                replay_test_input(&cell.turn_id),
            ))
            .await
            .expect("seed the drain's turn input");
    }
    let drain_id = "after-commit-cell-drain";

    let (control, control_host) = ProductionToolCell::sqlite("llm_query").await;
    seed_drain_input(&control).await;
    let control_scope =
        lash_core::ExecutionScope::queue_drain(control.session_id.clone(), drain_id);
    let mut control_runtime = control.runtime_on(Arc::clone(&control.runtime_store)).await;
    drive_drain(&mut control_runtime, control_host.as_ref(), &control_scope)
        .await
        .expect("the control drain commits");
    drop(control_runtime);
    let live_head = control.committed_execution_state().await;
    assert!(live_head.is_some(), "an RLM turn leaves execution state");

    let (cell, host) = ProductionToolCell::sqlite("llm_query").await;
    seed_drain_input(&cell).await;
    let drain_scope = lash_core::ExecutionScope::queue_drain(cell.session_id.clone(), drain_id);
    let committed = Arc::new(tokio::sync::Notify::new());
    let dying: Arc<dyn lash_core::RuntimePersistence> = Arc::new(DiesAfterFinalCommit {
        inner: Arc::clone(&cell.runtime_store),
        armed: AtomicBool::new(true),
        committed: Arc::clone(&committed),
    });
    let mut crashed = cell.runtime_on(dying).await;
    let crashed_host = Arc::clone(&host);
    let crashed_scope = drain_scope.clone();
    let worker = tokio::spawn(async move {
        let _ = drive_drain(&mut crashed, crashed_host.as_ref(), &crashed_scope).await;
    });
    tokio::time::timeout(std::time::Duration::from_secs(60), committed.notified())
        .await
        .expect("the drain reaches its turn-final commit");
    worker.abort();
    let _ = worker.await;
    assert_eq!(
        cell.committed_execution_state().await,
        live_head,
        "the crashed worker's commit is durable"
    );
    assert!(
        !lash_core::SessionCommitStore::drain_end_exists(cell.runtime_store.as_ref(), drain_id)
            .await
            .expect("read the drain-end receipt"),
        "the worker died before its drain ended"
    );
    let tool_executions = cell.tool_executions.load(Ordering::SeqCst);
    assert_eq!(tool_executions, 1, "the live pass ran the tool once");
    assert_eq!(cell.llm_provider_calls.load(Ordering::SeqCst), 2);

    expire_crashed_worker_lane(cell.runtime_store.as_ref(), &cell.session_id).await;
    host.start_replay();
    let mut redrive = if from_head {
        cell.runtime_from_head().await
    } else {
        cell.runtime_on(Arc::clone(&cell.runtime_store)).await
    };
    let scope = host
        .scoped(durable_admission(&drain_scope))
        .expect("scope the redriven drain");
    let drain = redrive
        .stream_next_queued_work(lash_core::facade_support::TurnOptions::new(
            tokio_util::sync::CancellationToken::new(),
            scope,
        ))
        .await
        .expect("the after-commit redrive completes");
    let lash_core::facade_support::QueuedTurnDrain::Replayed(receipt) = drain else {
        panic!("the redrive must replay the settled run's receipt");
    };
    assert!(
        matches!(
            receipt.terminal,
            Some(lash_core::store::QueuedRunTerminal::Completed { .. })
        ),
        "the receipt is the committed turn's terminal: {:?}",
        receipt.terminal
    );
    drop(redrive);
    assert_eq!(
        cell.committed_execution_state().await,
        live_head,
        "the committed execution state is byte-identical after the redrive"
    );
    assert_eq!(
        cell.tool_executions.load(Ordering::SeqCst),
        tool_executions,
        "the redrive runs no tool"
    );
    assert_eq!(
        cell.llm_provider_calls.load(Ordering::SeqCst),
        2,
        "the redrive re-issues neither an outer generation nor a nested llm_query call"
    );
}

#[tokio::test]
async fn an_after_commit_drain_redrive_from_the_pre_turn_state_keeps_the_committed_state() {
    Box::pin(assert_after_commit_drain_redrive_keeps_the_committed_state(
        false,
    ))
    .await;
}

#[tokio::test]
async fn an_after_commit_drain_redrive_from_the_head_keeps_the_committed_state() {
    Box::pin(assert_after_commit_drain_redrive_keeps_the_committed_state(
        true,
    ))
    .await;
}
