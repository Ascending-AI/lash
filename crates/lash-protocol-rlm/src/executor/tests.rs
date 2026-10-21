//! The laws of a session's cells on the kernel: each runs real cells
//! through the production entry, in a worker, on a claimed session actor
//! over a SQLite durable store.

mod carried_data;
mod crossing;
mod document_trips;
mod finish_value;
mod graph;
mod python;
mod tool_input;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use super::CodeModeExecutionState;
use crate::CellDialect;
use crate::testing::{DurableHost, cell_context, cell_services, python_workers, run_cell};

mod repeat;
mod saved_functions;
mod source_map;
mod tool_args;
mod written;

const SESSION: &str = "cell-laws";
const TURN: &str = "turn-1";

fn typescript_state() -> CodeModeExecutionState {
    crate::testing::session_state(&CellDialect::typescript())
}

fn typescript_services() -> super::CellServices {
    cell_services(
        &CellDialect::typescript(),
        lash_vm_client::service::Service::default(),
    )
}

/// The cell context `tools` run under, with `deferred` as the run's
/// deferred provider when there is one.
fn context_with(
    host: &DurableHost,
    replay_key: &'static str,
    tools: Arc<dyn lash_core::ToolProvider>,
    deferred: Option<&Arc<dyn lash_core::DeferredToolProvider>>,
) -> lash_core::RuntimeExecutionContext<'static> {
    match deferred {
        Some(deferred) => crate::testing::deferred_cell_context(
            host,
            SESSION,
            TURN,
            replay_key,
            tools,
            Arc::clone(deferred),
        ),
        None => cell_context(host, SESSION, TURN, replay_key, tools),
    }
}

/// A deterministic race: fast waits until slow is live, and slow settles
/// only when the cell explicitly releases it.
#[derive(Default)]
struct ControlRaceTools {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for ControlRaceTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_core::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok({
            ["slow", "fast", "release"]
                .map(|name| definition(name, &format!("tools.{name}")).manifest())
                .into()
        })
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        let tool = name.split_once('.').filter(|(root, _)| *root == "tools")?.1;
        ["slow", "fast", "release"]
            .contains(&tool)
            .then(|| Arc::new(definition(tool, name).contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        match call.name().as_str() {
            "tools.slow" => {
                self.entered.notify_one();
                self.release.notified().await;
            }
            "tools.fast" => self.entered.notified().await,
            "tools.release" => self.release.notify_one(),
            name => panic!("unexpected race tool {name}"),
        }
        lash_core::ToolOutcome::ok(serde_json::json!(call.name().trim_start_matches("tools.")))
            .into()
    }
}

/// FIG-5803 / DESIGN v3 §3: a passed race loser still running blocks a
/// control, with its typed cause; observing its settlement admits a control.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_race_loser_blocks_control_until_it_settles() {
    let host = open_host().await;
    let mut state = typescript_state();
    let response = run_cell(
        &mut state,
        cell_context(
            &host,
            SESSION,
            TURN,
            "exec-code:0",
            Arc::new(ControlRaceTools::default()),
        ),
        &typescript_services(),
        r#"let refusal = null;
const slow = tools.slow({});
const winner = await Promise.race([slow, tools.fast({})]);
try { await control.finish(winner); }
catch (error) { refusal = error; }
await tools.release({});
await slow;
await control.finish(winner);"#,
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    // The turn commits the cell's end; its bindings are read from there.
    crate::testing::commit_working(&mut state, &host).await;
    let bindings: std::collections::BTreeMap<_, _> = state
        .bound_variable_values(
            &std::collections::BTreeSet::new(),
            host.session_pages().as_ref(),
        )
        .await
        .expect("the bindings read")
        .into_iter()
        .filter_map(|(name, kind)| match kind {
            crate::session_globals::SessionGlobalKind::Data(value) => Some((name, value)),
            _ => None,
        })
        .collect();
    let refusal = &bindings["refusal"];
    assert_eq!(refusal["kind"], "control_refused", "{refusal}");
    assert_eq!(
        refusal["data"]["kind"], "control_with_outstanding_tasks",
        "{refusal}"
    );
    assert_eq!(finish_of(&response), serde_json::json!("fast"));
}

/// A finished cell's carry names pages its run committed when it parked
/// for its tool: the host reads them (a prompt's bindings) before any
/// commit owns the carry, under the run's lease the session holds until
/// one does.
#[tokio::test(flavor = "multi_thread")]
async fn a_parked_cells_carry_reads_before_the_turn_commits_it() {
    let host = open_host().await;
    let mut state = typescript_state();
    let response = run_cell(
        &mut state,
        cell_context(
            &host,
            SESSION,
            TURN,
            "exec-code:0",
            Arc::new(ControlRaceTools::default()),
        ),
        &typescript_services(),
        r#"const kept = [];
for (let i = 0; i < 2000; i++) kept.push(i);
const answer = await tools.release({});"#,
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    let bindings: std::collections::BTreeMap<_, _> = state
        .bound_variable_values(
            &std::collections::BTreeSet::new(),
            host.session_pages().as_ref(),
        )
        .await
        .expect("the carry reads before its commit")
        .into_iter()
        .filter_map(|(name, kind)| match kind {
            crate::session_globals::SessionGlobalKind::Data(value) => Some((name, value)),
            _ => None,
        })
        .collect();
    assert_eq!(bindings["answer"], serde_json::json!("release"));
    assert_eq!(bindings["kept"].as_array().map(Vec::len), Some(2000));
}

/// FIG-5764: a corrupt descriptor refuses the whole restore, and the
/// host's error retains the typed refusal.
#[tokio::test]
async fn a_corrupt_descriptor_refuses_the_restore_with_a_typed_cause() {
    use std::collections::BTreeSet;
    use std::error::Error as _;

    use super::snapshot::SessionOpenError;

    let mut state = typescript_state();
    state
        .patch_globals(
            &lash_rlm_types::RlmGlobalsPatchPluginBody {
                set_default: serde_json::Map::from_iter([(
                    "notes".to_string(),
                    serde_json::json!("x".repeat(1_000)),
                )]),
            },
            &BTreeSet::new(),
        )
        .await
        .expect("bind a value");
    state
        .capture_execution_graph(lash_core::plugin::ExecutionGraphPurpose::Final)
        .await
        .expect("seal the bindings");
    let mut saved = state
        .hydrated_execution_graph()
        .await
        .expect("the session's descriptor");
    let mut corrupt = saved.descriptor.to_vec();
    corrupt.push(b' ');
    saved.descriptor = corrupt.into();

    let mut restored = typescript_state();
    let error = restored
        .restore_execution_graph(&saved, lash_core::plugin::ExecutionGraphPurpose::Working)
        .map_err(lash_core::SessionError::from)
        .expect_err("a corrupt descriptor refuses the restore");
    let cause = error
        .source()
        .and_then(|source| source.downcast_ref::<SessionOpenError>())
        .expect("the public restore error retains its typed cause");
    assert!(matches!(cause, SessionOpenError::Heap(_)), "{cause}");
    assert!(restored.names().is_empty());
}

/// Law S1 (FIG-6424): a session stages no page silently — a host that named
/// no staging directory is refused, not defaulted.
#[tokio::test]
async fn a_session_whose_host_names_no_staging_refuses_to_stage() {
    use std::collections::BTreeSet;

    let dialect = CellDialect::typescript();
    let mut state = CodeModeExecutionState::new(dialect.name(), dialect.numbers());
    state
        .patch_globals(
            &lash_rlm_types::RlmGlobalsPatchPluginBody {
                set_default: serde_json::Map::from_iter([(
                    "notes".to_string(),
                    serde_json::json!("x".repeat(1_000)),
                )]),
            },
            &BTreeSet::new(),
        )
        .await
        .expect("bind a value");
    let error = state
        .capture_execution_graph(lash_core::plugin::ExecutionGraphPurpose::Final)
        .await
        .expect_err("a session whose host named no staging directory refuses to stage");
    assert!(
        error.to_string().contains("named no staging directory"),
        "{error}"
    );
}

async fn open_host() -> DurableHost {
    DurableHost::open(lash_core::AdmittedScope::turn(
        lash_core::SessionId::from(SESSION),
        lash_core::TurnId::from(TURN),
    ))
    .await
}

fn definition(id: &str, operation: &str) -> lash_core::ToolDefinition {
    typed_definition(
        id,
        operation,
        lash_core::ToolDefinition::default_input_schema(),
    )
}

fn typed_definition(
    id: &str,
    operation: &str,
    input: serde_json::Value,
) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        id,
        operation,
        "Echo the text back",
        input,
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
}

/// `echo.say`: it echoes its `text`, and takes a function in `add`, the one
/// slot of its input that admits one.
fn echo_say() -> lash_core::ToolDefinition {
    typed_definition(
        "tool:echo",
        "echo.say",
        serde_json::json!({
            "type": "object",
            "properties": {
                "text": { "type": "string" },
                "add": { "x-lash": { "kind": "function" } }
            },
            "additionalProperties": false
        }),
    )
}

/// The tools a law's cells call: `echo.say` and `echo.await`, listed in the
/// catalog, and `web.fetch`, which is not listed and runs only under a grant. Each
/// answers its `text`. A call whose text is the held one, while the hold is
/// on, reports that it started and never returns: the node dies with it
/// running.
#[derive(Default)]
struct CellTools {
    refusal: Option<lash_core::ToolOutcome>,
    /// How many calls ran to their answer, by text.
    answered: std::sync::Mutex<Vec<String>>,
    hold: AtomicBool,
    held_started: tokio::sync::Notify,
}

const HELD: &str = "held";

impl CellTools {
    fn answered(&self) -> Vec<String> {
        self.answered.lock().expect("answered calls").clone()
    }
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for CellTools {
    fn tool_manifests(
        &self,
        _config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_core::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        Ok({
            vec![
                echo_say().manifest(),
                definition("tool:echo_await", "echo.await").manifest(),
            ]
        })
    }

    fn resolve_manifest_by_id(
        &self,
        id: &lash_core::ToolId,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<lash_core::ToolManifest> {
        match id.as_str() {
            "tool:echo" => Some(echo_say().manifest()),
            "tool:echo_await" => Some(definition("tool:echo_await", "echo.await").manifest()),
            "tool:web_fetch" => Some(definition("tool:web_fetch", "web.fetch").manifest()),
            _ => None,
        }
    }

    fn resolve_contract(
        &self,
        name: &str,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_core::ToolContract>> {
        match name {
            "echo.say" | "tool:echo" => Some(Arc::new(echo_say().contract())),
            "echo.await" | "tool:echo_await" => Some(Arc::new(
                definition("tool:echo_await", "echo.await").contract(),
            )),
            "web.fetch" | "tool:web_fetch" => Some(Arc::new(
                definition("tool:web_fetch", "web.fetch").contract(),
            )),
            "alpha.lookup" | "tool:alpha_lookup" => Some(Arc::new(
                definition("tool:alpha_lookup", "alpha.lookup").contract(),
            )),
            _ => None,
        }
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if let Some(refusal) = &self.refusal {
            return refusal.clone().into();
        }
        let text = call
            .args
            .get("text")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if text == HELD && self.hold.load(Ordering::SeqCst) {
            self.held_started.notify_one();
            std::future::pending::<()>().await;
        }
        self.answered
            .lock()
            .expect("answered calls")
            .push(text.clone());
        lash_core::ToolAttemptOutcome::done_without_intents(lash_core::ToolOutcomeDone::ok(
            serde_json::Value::String(text),
        ))
    }
}

/// FIG-6320, FIG-6387: a host refusal keeps its code and plain message in
/// call feedback, the committed verdict and the uncaught cell error.
#[tokio::test]
async fn a_tool_failure_keeps_its_code_and_plain_message_in_feedback() {
    let host = open_host().await;
    let tools = CellTools {
        refusal: Some(lash_core::ToolOutcome::failure(
            lash_core::ToolFailure::new("effect_unbound", "Bind the effect before calling it")
                .with_raw(serde_json::json!({ "effect": "echo.say" })),
        )),
        ..CellTools::default()
    };
    let response = run_cell(
        &mut typescript_state(),
        cell_context(&host, SESSION, TURN, "exec-code:0", Arc::new(tools)),
        &typescript_services(),
        "await echo.say({});",
    )
    .await;
    let calls = crate::call_feedback::with_result_summaries(
        response.calls.clone(),
        &response.tool_calls,
        crate::call_feedback::CallFeedbackPolicy::standard().max_result_bytes(),
    );
    let summary = calls[0]
        .result_summary
        .as_ref()
        .expect("the call has model feedback");
    assert!(!summary.truncated);
    let feedback: serde_json::Value =
        serde_json::from_str(&summary.text).expect("the model receives the structured failure");
    assert_eq!(feedback["code"], "effect_unbound", "{feedback}");
    assert_eq!(feedback["message"], "Bind the effect before calling it");
    assert_eq!(feedback["raw"], serde_json::json!({ "effect": "echo.say" }));
    assert_eq!(
        response.calls[0].outcome,
        lash_core::ExecutedCallOutcome::Failed {
            code: "effect_unbound".into(),
            message: "Bind the effect before calling it".into(),
        },
        "the committed call verdict carries the tool's code"
    );
    let rendered = crate::feedback::render(response.error().expect("the tool failed"), "cell");
    assert!(
        rendered.starts_with(
            "uncaught tool_failed (effect_unbound): Bind the effect before calling it\n"
        ),
        "{rendered}"
    );
}

/// Grants `web.fetch` and counts how often it was asked.
#[derive(Default)]
struct GrantsWebFetch {
    asked: AtomicUsize,
    paths: std::sync::Mutex<Vec<String>>,
}

/// A deferred provider advertises nothing and runs no tool itself: what it
/// resolves names the provider that executes it.
macro_rules! resolves_only {
    ($provider:ty) => {
        #[async_trait::async_trait]
        impl lash_core::ToolProvider for $provider {
            fn tool_manifests(
                &self,
                _config: &lash_core::OwnerConfigView,
            ) -> std::result::Result<
                Vec<lash_core::ToolManifest>,
                lash_execution_contracts::plugin_error::PluginError,
            > {
                Ok({ Vec::new() })
            }

            fn resolve_contract(
                &self,
                _name: &str,
                _config: &lash_core::OwnerConfigView,
            ) -> Option<Arc<lash_core::ToolContract>> {
                None
            }

            async fn execute(
                &self,
                call: lash_core::ToolCall<'_>,
            ) -> lash_core::ToolAttemptOutcome {
                lash_core::ToolOutcome::err_fmt(format_args!("Unknown tool: {}", call.name()))
                    .into()
            }
        }
    };
}

resolves_only!(GrantsWebFetch);

#[async_trait::async_trait]
impl lash_core::DeferredToolProvider for GrantsWebFetch {
    async fn resolve(
        &self,
        _cx: &lash_core::DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> std::collections::BTreeMap<String, lash_core::DeferredOutcome> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        self.paths
            .lock()
            .unwrap()
            .extend(paths.iter().map(|path| (*path).to_owned()));
        paths
            .iter()
            .map(|path| {
                let outcome = if *path == "web.fetch" {
                    lash_core::DeferredOutcome::resolved(
                        lash_core::testing::test_tools_provider(),
                        definition("tool:web_fetch", "web.fetch"),
                        None,
                    )
                } else {
                    lash_core::DeferredOutcome::Unavailable { note: None }
                };
                ((*path).to_string(), outcome)
            })
            .collect()
    }
}

fn finish_of(response: &lash_core::ExecResponse) -> serde_json::Value {
    assert!(
        response.error().is_none(),
        "the cell failed: {:?}",
        response.error()
    );
    response
        .finish_value()
        .cloned()
        .expect("the cell finished with a value")
}

/// `K-SES-001`/`K-SES-002`: a session's state is its bindings. What one
/// cell binds the next reads, two bindings of one object still share it,
/// and all of it survives a save and a load into another node's state.
#[tokio::test(flavor = "multi_thread")]
async fn session_bindings_are_carried_between_cells_and_across_a_reload() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let services = typescript_services();
    let mut state = typescript_state();

    let bound = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &services,
        "const total = 40 + 2;\nlet rows = [{ n: 1 }, { n: 2 }];\nconst alias = rows;",
    )
    .await;
    assert!(bound.error().is_none(), "{:?}", bound.error());

    let read = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", tools.clone()),
        &services,
        "rows.push({ n: 3 });\nawait control.finish({ total, count: alias.length });",
    )
    .await;
    assert_eq!(
        finish_of(&read),
        serde_json::json!({ "total": 42, "count": 3 }),
        "the second cell reads the first cell's bindings, and `alias` is still `rows`"
    );

    crate::testing::commit_working(&mut state, &host).await;
    let saved = state
        .hydrated_execution_graph()
        .await
        .expect("the saved session state");
    let mut reloaded = typescript_state();
    reloaded
        .restore_execution_graph(&saved, lash_core::plugin::ExecutionGraphPurpose::Working)
        .expect("load the session's bindings");

    let after = run_cell(
        &mut reloaded,
        cell_context(&host, SESSION, TURN, "exec-code:2", tools),
        &services,
        "alias.push({ n: 4 });\nawait control.finish(rows.length + total);",
    )
    .await;
    assert_eq!(
        finish_of(&after),
        serde_json::json!(46),
        "the reloaded session holds the same bindings, with the shared object still shared"
    );
}

/// A tool whose operation is a reserved word is called as the member it is
/// (`processes.await(…)` is a first-party binding).
#[tokio::test(flavor = "multi_thread")]
async fn a_tool_named_by_a_reserved_word_is_called_as_a_member() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let response = run_cell(
        &mut typescript_state(),
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &typescript_services(),
        "await control.finish(await echo.await({ text: \"member\" }));",
    )
    .await;
    assert_eq!(finish_of(&response), serde_json::json!("member"));
    assert_eq!(tools.answered(), ["member"]);
}

/// A cell's prints are its observations, in order, each with the text a
/// model reads and the typed value it printed.
#[tokio::test(flavor = "multi_thread")]
async fn printed_values_are_the_cells_observations() {
    let host = open_host().await;
    let response = run_cell(
        &mut typescript_state(),
        cell_context(
            &host,
            SESSION,
            TURN,
            "exec-code:0",
            Arc::new(CellTools::default()),
        ),
        &typescript_services(),
        "console.log(\"hello\", 42);\nconsole.log(\"second\");",
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    assert_eq!(
        response
            .prints
            .iter()
            .map(|print| print.text.as_str())
            .collect::<Vec<_>>(),
        vec!["hello 42", "second"]
    );
    assert_eq!(response.prints[1].value, serde_json::json!("second"));
}

/// A cell's tool calls are kernel effects: a loop of awaited calls runs
/// each tool once, in order, and the cell records every call.
#[tokio::test(flavor = "multi_thread")]
async fn a_tool_loop_runs_each_call_once_and_records_it() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let response = run_cell(
        &mut typescript_state(),
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &typescript_services(),
        "const seen = [];\nfor (const word of [\"a\", \"b\", \"c\"]) {\n  seen.push(await echo.say({ text: word }));\n}\nawait control.finish(seen);",
    )
    .await;
    assert_eq!(finish_of(&response), serde_json::json!(["a", "b", "c"]));
    assert_eq!(tools.answered(), vec!["a", "b", "c"]);
    assert_eq!(
        response
            .calls
            .iter()
            .map(|call| (call.operation.as_str(), call.outcome.clone()))
            .collect::<Vec<_>>(),
        [
            vec![("echo.say", lash_core::ExecutedCallOutcome::Ok {}); 3],
            vec![("control.finish", lash_core::ExecutedCallOutcome::Ok {})],
        ]
        .concat(),
        "the three calls, then the control call that ended the cell"
    );
    assert_eq!(response.tool_calls.len(), 4);
}

/// TypeScript cell services whose worker pool holds each cell to the
/// lifetime `limit` makes of the standard one.
fn lifetime_services(limit: impl FnOnce(&mut lash_vm_client::CellLifetime)) -> super::CellServices {
    let mut config = lash_vm_client::service::Service::default().config().clone();
    limit(&mut config.cell_lifetime);
    cell_services(
        &CellDialect::typescript(),
        lash_vm_client::service::Service::new(config),
    )
}

/// Runs `code` to its end, then again on another node after the first
/// dies: the second reads the end the first recorded.
async fn run_and_replay(
    services: &super::CellServices,
    tools: &Arc<CellTools>,
    code: &str,
) -> (lash_core::ExecResponse, lash_core::ExecResponse) {
    let mut host = open_host().await;
    let first = run_cell(
        &mut typescript_state(),
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        services,
        code,
    )
    .await;
    host.kill_and_resume().await;
    let again = run_cell(
        &mut typescript_state(),
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        services,
        code,
    )
    .await;
    (first, again)
}

/// FIG-6788: a cell's parks admit at most its pool's lifetime effects. The
/// park that would pass the bound admits nothing and ends the cell with
/// the typed bound, recorded over its last root: the cell run again on
/// another node answers the same end and calls nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_past_its_lifetime_effects_ends_with_the_recorded_typed_bound() {
    let tools = Arc::new(CellTools::default());
    let (first, again) = run_and_replay(
        &lifetime_services(|lifetime| lifetime.effects = 3),
        &tools,
        "for (const word of [\"a\", \"b\", \"c\", \"d\", \"e\"]) {\n  await echo.say({ text: word });\n}\nawait control.finish(1);",
    )
    .await;
    let failure = first.error().expect("the cell ends at its lifetime bound");
    assert!(
        failure.message.starts_with(crate::CELL_BOUND_EXCEEDED)
            && failure
                .message
                .contains("tool calls and sleeps over its life")
            && failure.message.contains("(limit 3)"),
        "{}",
        failure.message
    );
    assert_eq!(
        tools.answered(),
        vec!["a", "b", "c"],
        "the fourth park admitted nothing"
    );
    assert_eq!(
        again.error(),
        Some(failure),
        "the recorded end answers the cell again"
    );
    assert_eq!(tools.answered(), vec!["a", "b", "c"]);
}

/// FIG-6788: the pages a cell seals into its checkpoints count over its
/// whole life. The save that would pass the pool's bound admits nothing
/// and ends the cell with the typed bound, recorded over its last root.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_past_its_lifetime_checkpoint_bytes_ends_with_the_recorded_typed_bound() {
    let tools = Arc::new(CellTools::default());
    let (first, again) = run_and_replay(
        &lifetime_services(|lifetime| lifetime.checkpoint_bytes = 256 * 1024),
        &tools,
        // About 372 KB of text no two chunks of which are alike: identical
        // pages are sealed once.
        "const first = await echo.say({ text: \"a\" });\nconst big = Array.from({ length: 6000 }, (_, i) => String(i).repeat(16)).join(\",\");\nconst second = await echo.say({ text: \"b\" });\nawait control.finish(big.length + first.length + second.length);",
    )
    .await;
    let failure = first.error().expect("the cell ends at its lifetime bound");
    assert!(
        failure.message.starts_with(crate::CELL_BOUND_EXCEEDED)
            && failure
                .message
                .contains("saved too much state over its life")
            && failure.message.contains("(limit 262144)"),
        "{}",
        failure.message
    );
    assert_eq!(
        tools.answered(),
        vec!["a"],
        "the save past the bound admitted nothing"
    );
    assert_eq!(
        again.error(),
        Some(failure),
        "the recorded end answers the cell again"
    );
    assert_eq!(tools.answered(), vec!["a"]);
}

/// Runs `code` until the held tool call starts, then kills the node with
/// the call running and resumes the cell on another node, where the call
/// may finish.
async fn crash_at_the_held_call_and_resume(
    host: &mut DurableHost,
    tools: &Arc<CellTools>,
    services: &super::CellServices,
    deferred: Option<&Arc<dyn lash_core::DeferredToolProvider>>,
    code: &str,
) -> lash_core::ExecResponse {
    tools.hold.store(true, Ordering::SeqCst);
    {
        let mut state = typescript_state();
        let cell = run_cell(
            &mut state,
            context_with(host, "exec-code:0", tools.clone(), deferred),
            services,
            code,
        );
        tokio::select! {
            response = cell => panic!("the cell ended with its call held: {:?}", response.result),
            () = tools.held_started.notified() => {}
        }
    }
    host.kill_and_resume().await;
    tools.hold.store(false, Ordering::SeqCst);
    run_cell(
        &mut typescript_state(),
        context_with(host, "exec-code:0", tools.clone(), deferred),
        services,
        code,
    )
    .await
}

/// ADR 0132 §8: a cell is durable through its parked state. A node that
/// dies inside a cell loses nothing that committed: the cell resumes on
/// another node from its last park, runs no statement before it again, and
/// keeps the prints and the calls it had made. The call that was running
/// when the node died is not run a second time: the program sees it fail as
/// interrupted, and carries on.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_resumes_from_its_park_after_its_node_dies() {
    let mut host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let response = crash_at_the_held_call_and_resume(
        &mut host,
        &tools,
        &typescript_services(),
        None,
        "console.log(\"before\");\nconst first = await echo.say({ text: \"first\" });\nlet second = \"unset\";\ntry {\n  second = await echo.say({ text: \"held\" });\n} catch (error) {\n  second = \"interrupted\";\n}\nconsole.log(\"after\");\nawait control.finish([first, second]);",
    )
    .await;
    assert_eq!(
        finish_of(&response),
        serde_json::json!(["first", "interrupted"])
    );
    assert_eq!(
        tools.answered(),
        vec!["first"],
        "the call that settled before the crash is not run again, nor is the interrupted one"
    );
    assert_eq!(
        response
            .prints
            .iter()
            .map(|print| print.text.as_str())
            .collect::<Vec<_>>(),
        vec!["before", "after"],
        "the print before the park is kept once"
    );
    assert_eq!(
        response
            .calls
            .iter()
            .map(|call| call.outcome.clone())
            .collect::<Vec<_>>(),
        vec![
            lash_core::ExecutedCallOutcome::Ok {},
            lash_core::ExecutedCallOutcome::Failed {
                code: "tool_interrupted".into(),
                message: "tool was interrupted by a runtime restart; it may or may not have taken effect, and may still be running.".into(),
            },
            // The control call that ended the cell.
            lash_core::ExecutedCallOutcome::Ok {}
        ]
    );
}

/// A tool granted to a cell by the session's deferred resolver is bound as
/// an effect of that cell and recorded with it: after a restart inside the
/// cell the tool is still callable, under the recorded grant, and the
/// resolver is not asked again.
#[tokio::test(flavor = "multi_thread")]
async fn a_deferred_tool_granted_to_a_cell_is_callable_after_a_restart() {
    let mut host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let resolver = Arc::new(GrantsWebFetch::default());
    let response = crash_at_the_held_call_and_resume(
        &mut host,
        &tools,
        &typescript_services(),
        Some(&(resolver.clone() as Arc<dyn lash_core::DeferredToolProvider>)),
        "const first = await web.fetch({ text: \"first\" });\ntry {\n  await web.fetch({ text: \"held\" });\n} catch (error) {\n  console.log(\"interrupted\");\n}\nconst third = await web.fetch({ text: \"third\" });\nawait control.finish([first, third]);",
    )
    .await;
    assert_eq!(finish_of(&response), serde_json::json!(["first", "third"]));
    assert_eq!(
        tools.answered(),
        vec!["first", "third"],
        "the tool granted before the restart runs after it"
    );
    assert_eq!(
        resolver.asked.load(Ordering::SeqCst),
        1,
        "the grant is the cell's record; the resumed cell does not resolve it again"
    );
}

/// OFFERED-ONE: every operation a cell is offered, a deferred grant and an
/// unavailable note included, is one entry of the cell's operations and
/// resolves through its namespaces: `alpha.lookup`, which the deferred
/// provider grants, performs, and `beta.gone`, which it explains away,
/// raises the host's note, in either dialect.
#[tokio::test(flavor = "multi_thread")]
async fn a_deferred_grant_performs_and_an_unavailable_note_raises_through_namespaces() {
    struct GrantsAlpha;

    resolves_only!(GrantsAlpha);

    #[async_trait::async_trait]
    impl lash_core::DeferredToolProvider for GrantsAlpha {
        async fn resolve(
            &self,
            _cx: &lash_core::DeferredResolveContext<'_>,
            paths: &[&str],
        ) -> std::collections::BTreeMap<String, lash_core::DeferredOutcome> {
            paths
                .iter()
                .filter_map(|path| {
                    let outcome = match *path {
                        "alpha.lookup" => lash_core::DeferredOutcome::resolved(
                            lash_core::testing::test_tools_provider(),
                            definition("tool:alpha_lookup", "alpha.lookup"),
                            None,
                        ),
                        "beta.gone" => lash_core::DeferredOutcome::Unavailable {
                            note: Some("beta is retired; use alpha".to_owned()),
                        },
                        _ => return None,
                    };
                    Some(((*path).to_owned(), outcome))
                })
                .collect()
        }
    }

    for dialect in [CellDialect::typescript(), CellDialect::python()] {
        let host = open_host().await;
        let tools = Arc::new(CellTools::default());
        let deferred = Arc::new(GrantsAlpha) as Arc<dyn lash_core::DeferredToolProvider>;
        let services = cell_services(
            &dialect,
            if dialect.name() == CellDialect::python().name() {
                python_workers()
            } else {
                lash_vm_client::service::Service::default()
            },
        );
        let (lookup, gone) = if dialect.name() == CellDialect::python().name() {
            (
                "await control.finish(await alpha.lookup({'text': 'found'}))",
                "await beta.gone({})",
            )
        } else {
            (
                "await control.finish(await alpha.lookup({ text: 'found' }));",
                "await beta.gone({});",
            )
        };
        let found = run_cell(
            &mut crate::testing::session_state(&dialect),
            context_with(&host, "exec-code:0", tools.clone(), Some(&deferred)),
            &services,
            lookup,
        )
        .await;
        assert_eq!(
            finish_of(&found),
            serde_json::json!("found"),
            "{}",
            dialect.name()
        );
        assert_eq!(tools.answered(), ["found"]);
        let gone = run_cell(
            &mut crate::testing::session_state(&dialect),
            context_with(&host, "exec-code:1", tools.clone(), Some(&deferred)),
            &services,
            gone,
        )
        .await;
        let failure = gone.error().expect("`beta.gone` is unavailable");
        assert!(
            failure.message.ends_with("beta is retired; use alpha"),
            "{}: {failure:?}",
            dialect.name()
        );
    }
}

/// FIG-6590 / FIG-6011: host notes and refusal reasons survive commit and
/// reopen verbatim in either dialect, without rediscovery on redrive.
#[tokio::test(flavor = "multi_thread")]
async fn deferred_host_explanations_survive_commit_and_reopen_verbatim() {
    struct Explains {
        asked: AtomicUsize,
        refused: bool,
    }

    fn host_note() -> String {
        format!("{}éé", "é🦀".repeat(170))
    }

    resolves_only!(Explains);

    #[async_trait::async_trait]
    impl lash_core::DeferredToolProvider for Explains {
        async fn resolve(
            &self,
            _cx: &lash_core::DeferredResolveContext<'_>,
            _paths: &[&str],
        ) -> std::collections::BTreeMap<String, lash_core::DeferredOutcome> {
            let attempt = self.asked.fetch_add(1, Ordering::SeqCst);
            std::collections::BTreeMap::from([("web.fetch".into(), {
                let note = if attempt == 0 {
                    host_note()
                } else {
                    "a different discovery answer".to_owned()
                };
                if self.refused {
                    lash_core::DeferredOutcome::Refused { reason: note }
                } else {
                    lash_core::DeferredOutcome::Unavailable { note: Some(note) }
                }
            })])
        }
    }

    let expected = host_note();
    assert_eq!(expected.len(), 1024);
    for refused in [true, false] {
        for dialect in [CellDialect::typescript(), CellDialect::python()] {
            let mut host = open_host().await;
            let tools = Arc::new(CellTools::default());
            let resolver = Arc::new(Explains {
                asked: AtomicUsize::new(0),
                refused,
            });
            let services = cell_services(
                &dialect,
                if dialect.name() == CellDialect::python().name() {
                    python_workers()
                } else {
                    lash_vm_client::service::Service::default()
                },
            );
            let deferred = resolver.clone() as Arc<dyn lash_core::DeferredToolProvider>;
            let code = if dialect.name() == CellDialect::python().name() {
                "try:\n    await echo.say({'text': 'held'})\nexcept Exception:\n    pass\nawait web.fetch({})"
            } else {
                "try { await echo.say({ text: 'held' }); } catch (error) {}\nawait web.fetch({});"
            };
            tools.hold.store(true, Ordering::SeqCst);
            {
                let mut state = crate::testing::session_state(&dialect);
                let running = run_cell(
                    &mut state,
                    context_with(&host, "exec-code:0", tools.clone(), Some(&deferred)),
                    &services,
                    code,
                );
                tokio::select! {
                    response = running => panic!("the cell ended before its held call: {:?}", response.error()),
                    () = tools.held_started.notified() => {}
                }
            }
            assert_eq!(resolver.asked.load(Ordering::SeqCst), 1);
            host.kill_and_resume().await;
            tools.hold.store(false, Ordering::SeqCst);
            let first = run_cell(
                &mut crate::testing::session_state(&dialect),
                context_with(&host, "exec-code:0", tools.clone(), Some(&deferred)),
                &services,
                code,
            )
            .await;
            let failure = first.error().expect("the tool is unavailable");
            assert!(failure.message.ends_with(&expected), "{failure:?}");
            let rendered = crate::feedback::render(failure, "cell");
            assert!(rendered.contains(&expected), "{rendered}");

            host.kill_and_resume().await;
            let redriven = run_cell(
                &mut crate::testing::session_state(&dialect),
                context_with(&host, "exec-code:0", tools.clone(), Some(&deferred)),
                &services,
                code,
            )
            .await;
            assert_eq!(redriven.error(), first.error());
            assert_eq!(resolver.asked.load(Ordering::SeqCst), 1);

            let next = run_cell(
                &mut crate::testing::session_state(&dialect),
                context_with(&host, "exec-code:1", tools, Some(&deferred)),
                &services,
                "await web.fetch({})",
            )
            .await;
            assert!(
                next.error()
                    .unwrap()
                    .message
                    .contains("a different discovery answer")
            );
            assert_eq!(resolver.asked.load(Ordering::SeqCst), 2);
        }
    }
}

/// FIG-5766: a cell's response says what its committed transition did to
/// the session's bindings, by name. A binding the cell left alone is in no
/// list, a function bound to a name of its own, or held in data
/// (`K-SES-003`), is saved and so is added like any binding, and a cell
/// that failed committed nothing and reports nothing,
/// whatever it assigned before it failed.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_reports_the_bindings_its_committed_transition_changed() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let services = typescript_services();
    let mut state = typescript_state();

    let first = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &services,
        "const kept = [1, 2];\nlet count = 1;",
    )
    .await;
    assert!(first.error().is_none(), "{:?}", first.error());
    assert_eq!(
        *first.bindings,
        lash_core::BindingChanges {
            added: vec!["count".to_owned(), "kept".to_owned()],
            ..Default::default()
        }
    );

    let second = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", tools.clone()),
        &services,
        "const rows = [kept.length];\ncount = 2;\nconst increment = (value) => value + 1;\nconst held = { call: increment };",
    )
    .await;
    assert!(second.error().is_none(), "{:?}", second.error());
    assert_eq!(
        *second.bindings,
        lash_core::BindingChanges {
            added: vec!["held".to_owned(), "increment".to_owned(), "rows".to_owned()],
            changed: vec!["count".to_owned()],
            ..Default::default()
        }
    );

    let failed = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:2", tools.clone()),
        &services,
        "count = 3;\nconst late = 1;\nthrow new Error(\"stop\");",
    )
    .await;
    assert!(failed.error().is_some(), "the cell failed");
    assert_eq!(*failed.bindings, lash_core::BindingChanges::default());

    let after = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:3", tools),
        &services,
        "await control.finish(count);",
    )
    .await;
    assert_eq!(
        finish_of(&after),
        serde_json::json!(2),
        "the failed cell's assignment was not committed"
    );
    assert_eq!(*after.bindings, lash_core::BindingChanges::default());
}

/// FIG-1438: a later cell declares a session global again, with `let` or
/// `const` either way round, and that rebinds it: the cell runs, its
/// record names both as changed, and the next cell reads the new values.
#[tokio::test(flavor = "multi_thread")]
async fn a_later_cell_redeclares_a_session_global() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let services = typescript_services();
    let mut state = typescript_state();

    let first = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &services,
        "const total = 1;\nlet rows = [1];",
    )
    .await;
    assert!(first.error().is_none(), "{:?}", first.error());

    let again = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", tools.clone()),
        &services,
        "let total = 10;\nconst rows = [\"a\"];",
    )
    .await;
    assert!(again.error().is_none(), "{:?}", again.error());
    assert_eq!(
        *again.bindings,
        lash_core::BindingChanges {
            changed: vec!["rows".to_owned(), "total".to_owned()],
            ..Default::default()
        }
    );

    let read = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:2", tools),
        &services,
        "await control.finish({ total, rows });",
    )
    .await;
    assert_eq!(
        finish_of(&read),
        serde_json::json!({ "total": 10, "rows": ["a"] })
    );
}

/// `K-TASK-018`, the unjoined-task rule: a cell that ends with work it
/// started still running gets the kernel's typed error as its observation,
/// naming the line of the async code that is still running and how to
/// repair it.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_that_ends_with_an_unjoined_task_observes_the_typed_error() {
    let host = open_host().await;
    let response = run_cell(
        &mut typescript_state(),
        cell_context(
            &host,
            SESSION,
            TURN,
            "exec-code:0",
            Arc::new(CellTools::default()),
        ),
        &typescript_services(),
        "const work = async () => {\n  await sleep(60000);\n  return 1;\n};\nwork();",
    )
    .await;
    let failure = response.error().expect("the cell ends in the typed error");
    let Some(lash_core::CellDefect::TasksOutstanding {
        unfinished,
        unobserved,
    }) = &failure.defect
    else {
        panic!("expected the tasks-outstanding defect, got {failure:?}");
    };
    assert_eq!((unfinished.len(), unobserved.len()), (1, 0));
    assert_eq!(failure.kind, lash_core::CellFailureKind::Program);
    assert!(
        failure.message.starts_with(crate::CELL_TASKS_OUTSTANDING)
            && failure.message.contains("Still running (1)")
            && failure
                .message
                .contains("line 1 (`const work = async () => {`)")
            && failure.message.contains("Await every promise"),
        "{}",
        failure.message
    );
}

/// A session created in the Python dialect runs its cell as a kernel
/// program lowered by `lash-dialect-python`: one cell, end to end.
#[tokio::test(flavor = "multi_thread")]
async fn a_python_session_runs_a_cell_end_to_end() {
    let host = open_host().await;
    let dialect = CellDialect::python();
    let mut state = crate::testing::session_state(&dialect);
    let services = cell_services(&dialect, python_workers());
    let tools = Arc::new(CellTools::default());

    let response = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &services,
        "words = [\"a\", \"b\"]\nsaid = await echo.say({\"text\": \"py\"})\nprint(said)\ntotal = len(words) + 40",
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    assert_eq!(
        response
            .prints
            .iter()
            .map(|print| print.text.as_str())
            .collect::<Vec<_>>(),
        vec!["py"]
    );
    assert_eq!(tools.answered(), vec!["py"]);
    // The turn's phase commit adopts what the parked cell committed.
    crate::testing::commit_working(&mut state, &host).await;

    let read = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", tools),
        &services,
        "print(total)",
    )
    .await;
    assert!(read.error().is_none(), "{:?}", read.error());
    assert_eq!(
        read.prints
            .iter()
            .map(|print| print.text.as_str())
            .collect::<Vec<_>>(),
        vec!["42"],
        "the second cell reads the first cell's binding"
    );
}

/// FIG-5763: an unrelated throw keeps its kind and message even when its
/// text mentions a closure binding an earlier cell dropped (K-SES-003).
#[tokio::test(flavor = "multi_thread")]
async fn an_unrelated_error_keeps_its_cause_after_a_binding_is_dropped() {
    let host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let services = typescript_services();
    let mut state = typescript_state();
    let first = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &services,
        "const work = async () => 1;\nconst format = work();\nawait format;",
    )
    .await;
    assert!(first.error().is_none(), "{:?}", first.error());
    let second = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:1", tools.clone()),
        &services,
        "throw new Error(\"unsupported format in row 3\");",
    )
    .await;
    let failure = second.error().expect("the throw ends the cell");
    assert_eq!(failure.defect, None, "{failure:?}");
    assert_eq!(failure.kind, lash_core::CellFailureKind::Program);
    assert!(
        failure.message.contains("unsupported format in row 3"),
        "{}",
        failure.message
    );
    assert!(failure.message.contains("Error"), "{}", failure.message);

    // K-SES-003 requires both the unbound-variable kind and exact data.
    let dropped = std::collections::BTreeMap::from([(
        lash_kernel_doc::Name::new("format"),
        lash_kernel_dialect::NotSaved::Task.to_string(),
    )]);
    for (kind, binding, expected) in [
        ("unbound_variable", "format", true),
        ("unbound_variable", "other", false),
        ("other_error", "format", false),
    ] {
        let value = lash_kernel_doc::Datum::Error(Box::new(lash_kernel_doc::ErrorDatum {
            kind: kind.to_owned(),
            message: "original diagnostic".to_owned(),
            data: lash_kernel_doc::Datum::Text(binding.to_owned()),
        }));
        let observation = crate::feedback::CellObservation::of_run_error(
            lash_kernel_vm::RunError::Uncaught {
                error: value.clone(),
                site: None,
            },
            &dropped,
            None,
            &[],
        );
        if expected {
            assert_eq!(
                observation,
                crate::feedback::CellObservation::BindingNotCarried {
                    binding: "format".to_owned(),
                    why: lash_kernel_dialect::NotSaved::Task.to_string(),
                }
            );
        } else {
            assert_eq!(
                observation,
                crate::feedback::CellObservation::Uncaught(value)
            );
        }
    }

    // A dialect refusal mentioning that name keeps its original diagnosis.
    let refused = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:2", tools),
        &services,
        "format(1);",
    )
    .await;
    let failure = refused
        .error()
        .expect("the dialect refuses an unknown call");
    assert_eq!(failure.defect, None, "{failure:?}");
    assert!(
        !failure.message.contains(crate::SESSION_BINDING_NOT_CARRIED),
        "{}",
        failure.message
    );
}

// FIG-6711 makes the effect's display label opaque to the host.
// Require a label naming the unsettled work, without parsing coordinates.
fn assert_interrupted_effect_label(message: &str, callee: &str) {
    let label = message
        .split_once(&format!("effect `{callee}` at "))
        .and_then(|(_, rest)| rest.split_once(" started and never settled"))
        .map(|(label, _)| label)
        .expect("the interrupted effect has a display label");
    assert!(!label.is_empty(), "{message}");
}

/// FIG-5763 / ADR 0132 NR-2: an interrupted Once effect names the
/// admitted call and warns that its outside work may already have happened.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_once_effect_observation_names_the_effect() {
    let mut host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let response = crash_at_the_held_call_and_resume(
        &mut host,
        &tools,
        &typescript_services(),
        None,
        "await echo.say({ text: \"held\" });",
    )
    .await;
    let failure = response
        .error()
        .expect("the interrupted call ends the cell");
    assert!(
        failure.message.contains("tool_failed"),
        "{}",
        failure.message
    );
    assert!(failure.message.contains("echo.say"), "{}", failure.message);
    assert_interrupted_effect_label(&failure.message, "echo.say");
    assert!(
        failure
            .message
            .contains("outside work may already have happened")
            && failure
                .message
                .contains("check outside state before calling again"),
        "{}",
        failure.message
    );
    assert!(tools.answered().is_empty(), "Once never starts again");
}

/// FIG-6524: local methods and built-ins never enter deferred resolution.
async fn local_methods_stay_in_the_language(dialect: CellDialect, code: &str) {
    struct RejectMethods;
    resolves_only!(RejectMethods);
    #[async_trait::async_trait]
    impl lash_core::DeferredToolProvider for RejectMethods {
        async fn resolve(
            &self,
            _cx: &lash_core::DeferredResolveContext<'_>,
            paths: &[&str],
        ) -> std::collections::BTreeMap<String, lash_core::DeferredOutcome> {
            assert!(
                paths.is_empty(),
                "language operations reached the tool resolver: {paths:?}"
            );
            Default::default()
        }
    }
    struct ItemsTools;
    #[async_trait::async_trait]
    impl lash_core::ToolProvider for ItemsTools {
        fn tool_manifests(
            &self,
            _config: &lash_core::OwnerConfigView,
        ) -> Result<Vec<lash_core::ToolManifest>, lash_execution_contracts::plugin_error::PluginError>
        {
            Ok(vec![definition("tool:tools_sort", "tools.sort").manifest()])
        }
        fn resolve_contract(
            &self,
            name: &str,
            _config: &lash_core::OwnerConfigView,
        ) -> Option<Arc<lash_core::ToolContract>> {
            match name {
                "tools.sort" => Some(Arc::new(
                    definition("tool:tools_sort", "tools.sort").contract(),
                )),
                _ => None,
            }
        }
        async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
            panic!("a language method reached a tool: {}", call.name());
        }
    }
    let host = open_host().await;
    let services = cell_services(
        &dialect,
        if dialect.name() == "python" {
            python_workers()
        } else {
            Default::default()
        },
    );
    let deferred = Arc::new(RejectMethods) as Arc<dyn lash_core::DeferredToolProvider>;
    let response = run_cell(
        &mut crate::testing::session_state(&dialect),
        context_with(&host, "exec-code:0", Arc::new(ItemsTools), Some(&deferred)),
        &services,
        code,
    )
    .await;
    assert_eq!(
        finish_of(&response),
        serde_json::json!([{"key": 1}, {"key": 2}])
    );
    assert_eq!(response.prints.len(), 1, "the sorted records are logged");
}

#[tokio::test(flavor = "multi_thread")]
async fn typescript_local_sort_and_console_never_resolve_tools() {
    local_methods_stay_in_the_language(
        CellDialect::typescript(),
        r#"
        const items = [{key: 2}, {key: 1}];
        function order(items) { items.sort((a, b) => a.key - b.key); return items; }
        order(items).sort((a, b) => a.key - b.key);
        console.log(items);
        await control.finish(items);
    "#,
    )
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn python_local_sort_and_print_never_resolve_tools() {
    local_methods_stay_in_the_language(CellDialect::python(), "items = [{'key': 2}, {'key': 1}]\ndef order(items):\n    items.sort(key=lambda item: item['key'])\n    return items\norder(items).sort(key=lambda item: item['key'])\nprint(items)\nawait control.finish(items)\n").await;
}

/// OP-INTERNAL: neither a catalog perform nor a deferred provider's grant
/// admits an internal operation from authored cell code.
#[tokio::test(flavor = "multi_thread")]
async fn internal_operations_are_refused_in_static_and_deferred_cells() {
    fn private_definition() -> lash_core::ToolDefinition {
        lash_core::ToolDefinition::raw(
            "dataframe",
            "dataframe.run",
            "private",
            serde_json::json!({}),
            serde_json::json!({"type":"string"}),
        )
        .unwrap()
        .declared_contract(lash_kernel_doc::ContractId::new("run/1"))
        .with_execution(std::time::Duration::from_secs(30))
    }
    struct Dataframe;
    #[async_trait::async_trait]
    impl lash_core::ToolProvider for Dataframe {
        fn tool_manifests(
            &self,
            _: &lash_core::OwnerConfigView,
        ) -> Result<Vec<lash_core::ToolManifest>, lash_execution_contracts::plugin_error::PluginError>
        {
            Ok(vec![private_definition().manifest])
        }
        fn resolve_contract(
            &self,
            _: &str,
            _: &lash_core::OwnerConfigView,
        ) -> Option<Arc<lash_core::ToolContract>> {
            Some(Arc::new(private_definition().contract))
        }
        async fn execute(&self, _: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
            panic!("an internal operation must never be dispatched")
        }
    }
    struct GrantsDataframe;
    resolves_only!(GrantsDataframe);
    #[async_trait::async_trait]
    impl lash_core::DeferredToolProvider for GrantsDataframe {
        async fn resolve(
            &self,
            _: &lash_core::DeferredResolveContext<'_>,
            paths: &[&str],
        ) -> std::collections::BTreeMap<String, lash_core::DeferredOutcome> {
            paths
                .iter()
                .map(|path| {
                    (
                        (*path).to_owned(),
                        lash_core::DeferredOutcome::resolved(
                            lash_core::testing::test_tools_provider(),
                            private_definition(),
                            None,
                        ),
                    )
                })
                .collect()
        }
    }
    let builtin = lash_vm_runtime::lash_packages().unwrap();
    let package = lash_kernel_package::Package::build(
        "dataframe",
        "1",
        &lash_kernel_package::PackageSources {
            surface: "internal operation run(input?: Record{..Json}) -> Any contract \"run/1\"\n",
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    let held = Arc::new(
        lash_kernel_package::HeldPackages::install(
            builtin
                .in_order()
                .into_iter()
                .map(|package| (**package).clone())
                .chain([package]),
        )
        .unwrap(),
    );
    for deferred in [false, true] {
        let host = open_host().await;
        let mut services = typescript_services();
        services.workers = services.workers.with_packages(held.clone());
        let resolver = Arc::new(GrantsDataframe) as Arc<dyn lash_core::DeferredToolProvider>;
        let tools = if deferred {
            Arc::new(CellTools::default()) as Arc<dyn lash_core::ToolProvider>
        } else {
            Arc::new(Dataframe) as Arc<dyn lash_core::ToolProvider>
        };
        let response = run_cell(
            &mut typescript_state(),
            context_with(&host, "exec-code:0", tools, deferred.then_some(&resolver)),
            &services,
            if deferred {
                "await external.lookup({text: 'private'});"
            } else {
                "await dataframe.run({text: 'private'});"
            },
        )
        .await;
        let error = response.error().expect("the internal perform is refused");
        assert!(error.message.contains("dataframe.run"), "{error:?}");
        assert!(error.message.contains("internal operation"), "{error:?}");
        if !deferred {
            assert!(
                error.message.contains("main"),
                "the refusal names its authored unit: {error:?}"
            );
            let restored: lash_core::CellFailure =
                serde_json::from_value(serde_json::to_value(error).unwrap()).unwrap();
            assert!(
                matches!(restored.defect, Some(lash_core::CellDefect::InternalOperation { operation, unit: lash_kernel_doc::Unit::Main }) if operation.as_str() == "dataframe.run")
            );
        }
    }
}

/// Closed package namespaces: internal and unknown paths never reach the
/// resolver; an outside path still resolves and commits on SQLite.
#[tokio::test]
async fn package_namespace_candidates_are_decided_without_the_provider() {
    let package = lash_kernel_package::Package::build(
        "dataframe",
        "1",
        &lash_kernel_package::PackageSources {
            kernel: "function dataframe.col() -> Int\nkernel 1\ncharge 1\nbody { return 7 }\n",
            surface: "internal operation run(input?: Json) -> Any contract \"run/1\"\nfunction col() -> Int = dataframe.col\n",
            ..Default::default()
        },
        &[],
    )
    .unwrap();
    let builtin = lash_vm_runtime::lash_packages().unwrap();
    let held = Arc::new(
        lash_kernel_package::HeldPackages::install(
            builtin
                .in_order()
                .into_iter()
                .map(|package| (**package).clone())
                .chain([package]),
        )
        .unwrap(),
    );
    let view = held.view();
    let host = open_host().await;
    #[derive(Default)]
    struct ClaimsAny {
        paths: std::sync::Mutex<Vec<String>>,
    }
    resolves_only!(ClaimsAny);
    #[async_trait::async_trait]
    impl lash_core::DeferredToolProvider for ClaimsAny {
        async fn resolve(
            &self,
            _: &lash_core::DeferredResolveContext<'_>,
            paths: &[&str],
        ) -> std::collections::BTreeMap<String, lash_core::DeferredOutcome> {
            self.paths
                .lock()
                .unwrap()
                .extend(paths.iter().map(|path| (*path).to_owned()));
            paths
                .iter()
                .map(|path| {
                    (
                        (*path).to_owned(),
                        lash_core::DeferredOutcome::resolved(
                            lash_core::testing::test_tools_provider(),
                            definition(path, path),
                            None,
                        ),
                    )
                })
                .collect()
        }
    }
    let resolver = Arc::new(ClaimsAny::default());
    let deferred = resolver.clone() as Arc<dyn lash_core::DeferredToolProvider>;
    let context = context_with(
        &host,
        "exec-code:0",
        Arc::new(CellTools::default()),
        Some(&deferred),
    );
    let refusal = crate::deferred::journal_deferred_matches(
        BTreeSet::from(["dataframe.run".into()]),
        &context,
        &view,
    )
    .await
    .unwrap_err();
    assert!(
        matches!(refusal, crate::deferred::DeferredResolutionError::ProviderRegistry(lash_execution_contracts::PluginError::InternalOperation { operation }) if operation=="dataframe.run")
    );
    assert!(resolver.paths.lock().unwrap().is_empty());
    let context = context_with(
        &host,
        "exec-code:1",
        Arc::new(CellTools::default()),
        Some(&deferred),
    );
    let matches = crate::deferred::journal_deferred_matches(
        BTreeSet::from(["dataframe.nosuch".into(), "web.fetch".into()]),
        &context,
        &view,
    )
    .await
    .unwrap()
    .unwrap();
    assert!(matches!(
        matches["dataframe.nosuch"].outcome,
        lash_core::DeferredOutcome::Refused { .. }
    ));
    assert!(matches["web.fetch"].resolved().is_some());
    assert_eq!(*resolver.paths.lock().unwrap(), ["web.fetch"]);
    let mut services = typescript_services();
    services.workers = services.workers.with_packages(held.clone());
    services.kernel = super::KernelCarry::holding(Some(held.clone()), None);
    for (key, source) in [
        ("exec-code:2", "await dataframe.run({});"),
        ("exec-code:3", "await dataframe.nosuch({});"),
        ("exec-code:4", "await dataframe.sub.nosuch({});"),
    ] {
        let response = run_cell(
            &mut typescript_state(),
            context_with(&host, key, Arc::new(CellTools::default()), Some(&deferred)),
            &services,
            source,
        )
        .await;
        let error = response
            .error()
            .expect("the surface refuses the package path");
        assert!(error.message.contains("dataframe"), "{error:?}");
    }
    let mut state = typescript_state();
    let response = run_cell(
        &mut state,
        context_with(
            &host,
            "exec-code:5",
            Arc::new(CellTools::default()),
            Some(&deferred),
        ),
        &services,
        "await control.finish(dataframe.col());",
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    assert_eq!(finish_of(&response), serde_json::json!(7));
    let mut shadowed = typescript_state();
    let response = run_cell(
        &mut shadowed,
        context_with(
            &host,
            "exec-code:6",
            Arc::new(CellTools::default()),
            Some(&deferred),
        ),
        &services,
        "const dataframe = {col: 9};",
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    crate::testing::commit_working(&mut shadowed, &host).await;
    let response = run_cell(
        &mut shadowed,
        context_with(
            &host,
            "exec-code:7",
            Arc::new(CellTools::default()),
            Some(&deferred),
        ),
        &services,
        "await control.finish(dataframe.col);",
    )
    .await;
    assert!(response.error().is_none(), "{:?}", response.error());
    assert_eq!(
        finish_of(&response),
        serde_json::json!(9),
        "a session binding shadows its package namespace"
    );
    assert_eq!(
        *resolver.paths.lock().unwrap(),
        ["web.fetch"],
        "internal, unknown, nested, public and shadowed package paths never reach the provider"
    );
}
