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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

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
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
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
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
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
/// FIG-7408: direct sleep losers must instead be cancelled when the race
/// settles, so the advertised timeout can finish without orphaning effects.
#[tokio::test(flavor = "multi_thread")]
async fn a_live_race_loser_blocks_control_until_it_settles() {
    for (program, expected) in [
        (
            r#"const call = sleep(5000);
const ms = 0;
const winner = await Promise.race([call, sleep(ms)]);
let cancelled = false;
try { await call; } catch (error) { cancelled = error.name === "cancelled"; }
await control.finish({ timedOut: winner === undefined, cancelled });"#,
            serde_json::json!({ "timedOut": true, "cancelled": true }),
        ),
        (
            r#"const call = tools.release({});
const ms = 5000;
const winner = await Promise.race([call, sleep(ms)]);
await control.finish(winner);"#,
            serde_json::json!("release"),
        ),
        (
            r#"const call = tools.slow({});
const ms = 0;
const winner = await Promise.race([call, sleep(ms)]);
let refusal = null;
try { await control.finish(winner === undefined); }
catch (error) { refusal = error.name; }
await tools.release({});
const settled = await call;
await control.finish({ timedOut: winner === undefined, settled, refusal });"#,
            serde_json::json!({
                "timedOut": true,
                "settled": "slow",
                "refusal": "control_refused"
            }),
        ),
        (
            r#"const call = (async () => {
    await tools.slow({});
    return "wrapped";
})();
const winner = await Promise.race([call, sleep(0)]);
await tools.release({});
const settled = await call;
await control.finish({ timedOut: winner === undefined, settled });"#,
            serde_json::json!({ "timedOut": true, "settled": "wrapped" }),
        ),
    ] {
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
            program,
        )
        .await;
        assert!(
            response.error().is_none(),
            "{program}: {:?}",
            response.error()
        );
        assert_eq!(finish_of(&response), expected, "{program}");
    }
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
        .bound_variable_values(host.session_pages().as_ref())
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
async fn a_parked_cells_bindings_are_previewed_before_the_turn_commits_their_pages() {
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
        .bound_variable_values(host.session_pages().as_ref())
        .await
        .expect("the preview reads the carry under the parked run's lease")
        .into_iter()
        .collect();
    assert_eq!(
        bindings["answer"],
        crate::SessionGlobalKind::Data(serde_json::json!("release"))
    );
    assert_eq!(
        bindings["kept"],
        crate::SessionGlobalKind::Unpreviewed {
            kind: "list".into(),
            extent: 2000,
        }
    );
}

/// FIG-5764: a corrupt descriptor refuses the whole restore, and the
/// host's error retains the typed refusal.
#[tokio::test]
async fn a_corrupt_descriptor_refuses_the_restore_with_a_typed_cause() {
    use std::error::Error as _;

    use super::snapshot::SessionOpenError;

    let mut state = typescript_state();
    state
        .patch_globals(&lash_rlm_types::RlmGlobalsPatchPluginBody {
            set_default: serde_json::Map::from_iter([(
                "notes".to_string(),
                serde_json::json!("x".repeat(1_000)),
            )]),
        })
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
    let dialect = CellDialect::typescript();
    let mut state = CodeModeExecutionState::new(dialect.name(), dialect.numbers());
    state
        .patch_globals(&lash_rlm_types::RlmGlobalsPatchPluginBody {
            set_default: serde_json::Map::from_iter([(
                "notes".to_string(),
                serde_json::json!("x".repeat(1_000)),
            )]),
        })
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
        lash_vocabulary::identity::SessionId::from(SESSION),
        lash_vocabulary::identity::TurnId::from(TURN),
    ))
    .await
}

fn definition(id: &str, operation: &str) -> lash_vocabulary::tool_contract::ToolDefinition {
    typed_definition(
        id,
        operation,
        lash_vocabulary::tool_contract::ToolDefinition::default_input_schema(),
    )
}

fn typed_definition(
    id: &str,
    operation: &str,
    input: serde_json::Value,
) -> lash_vocabulary::tool_contract::ToolDefinition {
    lash_vocabulary::tool_contract::ToolDefinition::raw(
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
fn echo_say() -> lash_vocabulary::tool_contract::ToolDefinition {
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
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
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
        id: &lash_vocabulary::tool_contract::ToolId,
        _config: &lash_core::OwnerConfigView,
    ) -> Option<lash_vocabulary::tool_contract::ToolManifest> {
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
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
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
    code: &str,
) -> lash_core::ExecResponse {
    tools.hold.store(true, Ordering::SeqCst);
    {
        let mut state = typescript_state();
        let cell = run_cell(
            &mut state,
            cell_context(host, SESSION, TURN, "exec-code:0", tools.clone()),
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
        cell_context(host, SESSION, TURN, "exec-code:0", tools.clone()),
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
        lash_vocabulary::names::Name::new("format"),
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

/// HELD-CELL-MEMBERSHIP: the executor hands the consuming turn the identity
/// it actually registered, including a pure cell that issues no tool call.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_cell_hands_off_its_registered_execution_for_atomic_retirement() {
    use lash_core::core_internal::RuntimeExecutionContextRuntimeOps as _;
    let host = open_host().await;
    let handoff = lash_core::CellEndHandoff::held();
    let ctx = cell_context(
        &host,
        SESSION,
        TURN,
        "membership:0",
        Arc::new(crate::testing::NoTools),
    )
    .with_cell_end(handoff.clone());
    let expected = lash_core_store::resolutions::ExecutionId::cell(
        &lash_vocabulary::identity::SessionId::from(SESSION),
        TURN,
        "membership:0",
    );
    let mut state = typescript_state();
    let response = run_cell(&mut state, ctx, &typescript_services(), "const answer = 1;").await;
    assert!(response.error().is_none(), "{:?}", response.error());
    let execution = handoff
        .take_execution()
        .expect("the registered cell hands off its identity");
    assert_eq!(execution, expected);
    assert!(
        handoff.take_execution().is_none(),
        "the obligation transfers once"
    );
    let context = host.context();
    let resolutions = context.backend().stores().execution_resolutions();
    let live = resolutions.requirements(context.actor()).await.unwrap();
    assert!(
        live.machine
            .kernels
            .contains(&lash_kernel_doc::KERNEL_VERSION),
        "the registered cell contributes its machine debt: {live:?}"
    );
    let mut tx = context.begin().await.unwrap();
    tx.end_execution(execution);
    context
        .commit(tx, lash_durable::CommitLabel::new("law.consume_cell"))
        .await
        .unwrap();
    assert!(
        resolutions
            .requirements(context.actor())
            .await
            .unwrap()
            .is_empty(),
        "the consuming commit retires the handed-off source"
    );
}

/// HELD-CELL-SETUP: a refusal before registration creates no execution
/// identity for the turn to retire, and contributes no source debt.
#[tokio::test(flavor = "multi_thread")]
async fn a_cell_refused_before_registration_hands_off_no_execution() {
    use lash_core::core_internal::RuntimeExecutionContextRuntimeOps as _;
    let host = open_host().await;
    let handoff = lash_core::CellEndHandoff::held();
    let ctx = cell_context(
        &host,
        SESSION,
        TURN,
        "membership:refused",
        Arc::new(crate::testing::NoTools),
    )
    .with_cell_end(handoff.clone());
    let mut state = typescript_state();
    let response = run_cell(&mut state, ctx, &typescript_services(), "const = ;").await;
    assert!(response.error().is_some());
    assert!(handoff.take_execution().is_none());
    let context = host.context();
    assert!(
        context
            .backend()
            .stores()
            .execution_resolutions()
            .requirements(context.actor())
            .await
            .unwrap()
            .is_empty()
    );
}

/// FIG-7135 / FIG-6000: registration cannot advance the actor underneath
/// a held model-response draft; first park commits both at one revision.
#[tokio::test(flavor = "multi_thread")]
async fn a_held_response_registers_its_cell_in_the_first_park() {
    use lash_core::core_internal::RuntimeExecutionContextRuntimeOps as _;
    let host = open_host().await;
    let services = typescript_services();
    let _pool = services.workers.pool().expect("ready worker");
    let context = host.context();
    let held = lash_core::HeldResponse::holding(
        context.begin().await.expect("the model response draft"),
        |_| false,
        |_| false,
    );
    let ctx = cell_context(
        &host,
        SESSION,
        TURN,
        "held-registration:0",
        Arc::new(CellTools::default()),
    )
    .with_held_response(Some(held.clone()));
    let response = run_cell(
        &mut typescript_state(),
        ctx,
        &services,
        r#"await echo.say({ text: "held response" });"#,
    )
    .await;
    assert!(
        response.error().is_none(),
        "the first park must commit without a stale response draft: {:?}",
        response.error()
    );
    assert_eq!(
        held.committed_as(),
        Some(lash_durable::CommitLabel::MODEL_DONE_CELL_SNAPSHOT_ADMIT),
        "registration must retain the first-park coalescing boundary"
    );
    assert_eq!(
        response.calls.len(),
        1,
        "one admitted call, without redriving the cell"
    );
}
