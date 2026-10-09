//! The laws of a session's cells on the kernel: each runs real cells
//! through the production entry, in a worker, on a claimed session actor
//! over a SQLite durable store.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use lash_vm_runtime::ToolDefinitionBindingExt as _;

use super::RlmExecutionState;
use crate::CellDialect;
use crate::testing::{DurableHost, cell_context, cell_services, python_workers, run_cell};

const SESSION: &str = "cell-laws";
const TURN: &str = "turn-1";

fn typescript_state() -> RlmExecutionState {
    let dialect = CellDialect::typescript();
    RlmExecutionState::new(dialect.name(), dialect.numbers())
}

fn typescript_services(resolver: Option<crate::SharedDeferredToolResolver>) -> super::CellServices {
    cell_services(
        &CellDialect::typescript(),
        lash_vm_client::service::Service::default(),
        resolver,
    )
}

/// FIG-5764: a corrupt fragment refuses the whole restore, and the host's
/// error retains the typed hash mismatch and the binding it names.
#[tokio::test]
async fn a_corrupt_fragment_reports_its_binding_as_a_typed_restore_cause() {
    use std::collections::BTreeSet;
    use std::error::Error as _;

    use super::snapshot::RlmSnapshotError;

    let fleet = lash_core::FleetFormat::current();
    let mut state = typescript_state();
    state
        .patch_globals(
            &lash_rlm_types::RlmGlobalsPatchPluginBody {
                set_default: serde_json::Map::from_iter([(
                    "notes".to_string(),
                    serde_json::json!(
                        "x".repeat(lash_core::plugin::EXECUTION_STATE_LEAF_MIN_BODY_BYTES)
                    ),
                )]),
            },
            &BTreeSet::new(),
        )
        .await
        .expect("bind a value stored as a leaf");
    let mut saved = state
        .hydrated_execution_state(fleet)
        .await
        .expect("capture the bindings");
    let (component, body) = saved.components.first_key_value().expect("the notes leaf");
    let component = component.clone();
    let mut corrupt = body.to_vec();
    corrupt.push(b' ');
    saved.components.insert(component.clone(), corrupt.into());

    let mut restored = typescript_state();
    let error = restored
        .restore_execution_state(&saved, fleet)
        .await
        .map_err(lash_core::SessionError::from)
        .expect_err("a corrupt fragment refuses the restore");
    let cause = error
        .source()
        .and_then(|source| source.downcast_ref::<RlmSnapshotError>())
        .expect("the public restore error retains its typed cause");
    assert!(matches!(
        cause,
        RlmSnapshotError::LeafHashMismatch {
            logical_key,
            component: expected,
            actual_component,
        } if logical_key == "notes" && expected == &component && actual_component != expected
    ));
    assert!(restored.bindings().names().is_empty());
}

async fn open_host() -> DurableHost {
    DurableHost::open(lash_core::AdmittedScope::turn(
        lash_core::SessionId::from(SESSION),
        lash_core::TurnId::from(TURN),
    ))
    .await
}

fn definition(id: &str, name: &str, module: &str, operation: &str) -> lash_core::ToolDefinition {
    lash_core::ToolDefinition::raw(
        id,
        name,
        "Echo the text back",
        lash_core::ToolDefinition::default_input_schema(),
        serde_json::json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_execution(std::time::Duration::from_secs(120))
    .with_tool_binding(lash_vm_runtime::ToolBinding::new([module], operation))
}

/// The tools a law's cells call: `echo.say` and `echo.await`, listed in the
/// catalog, and `web.fetch`, which is not listed and runs only under a grant. Each
/// answers its `text`. A call whose text is the held one, while the hold is
/// on, reports that it started and never returns: the node dies with it
/// running.
#[derive(Default)]
struct CellTools {
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
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        vec![
            definition("tool:echo", "echo", "echo", "say").manifest(),
            definition("tool:echo_await", "echo_await", "echo", "await").manifest(),
        ]
    }

    fn resolve_manifest_by_id(&self, id: &lash_core::ToolId) -> Option<lash_core::ToolManifest> {
        match id.as_str() {
            "tool:echo" => Some(definition("tool:echo", "echo", "echo", "say").manifest()),
            "tool:echo_await" => {
                Some(definition("tool:echo_await", "echo_await", "echo", "await").manifest())
            }
            "tool:web_fetch" => {
                Some(definition("tool:web_fetch", "web_fetch", "web", "fetch").manifest())
            }
            _ => None,
        }
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<lash_core::ToolContract>> {
        match name {
            "echo" | "tool:echo" => Some(Arc::new(
                definition("tool:echo", "echo", "echo", "say").contract(),
            )),
            "echo_await" | "tool:echo_await" => Some(Arc::new(
                definition("tool:echo_await", "echo_await", "echo", "await").contract(),
            )),
            "web_fetch" | "tool:web_fetch" => Some(Arc::new(
                definition("tool:web_fetch", "web_fetch", "web", "fetch").contract(),
            )),
            _ => None,
        }
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
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

/// Grants `web.fetch` and counts how often it was asked.
#[derive(Default)]
struct GrantsWebFetch {
    asked: AtomicUsize,
}

#[async_trait::async_trait]
impl crate::DeferredToolResolver for GrantsWebFetch {
    async fn resolve(
        &self,
        _cx: &crate::DeferredResolveContext<'_>,
        paths: &[&str],
    ) -> std::collections::BTreeMap<String, crate::Resolution> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        paths
            .iter()
            .map(|path| {
                let resolution = if *path == "web.fetch" {
                    crate::Resolution::Resolved(Box::new(
                        crate::ToolGrant::new(definition(
                            "tool:web_fetch",
                            "web_fetch",
                            "web",
                            "fetch",
                        ))
                        .with_source_id(lash_core::facade_support::PLUGIN_TOOL_SOURCE_ID),
                    ))
                } else {
                    crate::Resolution::NotAvailable
                };
                ((*path).to_string(), resolution)
            })
            .collect()
    }

    fn install_recorded_grant(
        &self,
        _path: &str,
        _grant: &crate::ToolGrant,
    ) -> Result<(), crate::RecordedGrantInstallError> {
        Ok(())
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
    let services = typescript_services(None);
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
        "rows.push({ n: 3 });\nfinish({ total, count: alias.length });",
    )
    .await;
    assert_eq!(
        finish_of(&read),
        serde_json::json!({ "total": 42, "count": 3 }),
        "the second cell reads the first cell's bindings, and `alias` is still `rows`"
    );

    let fleet = lash_core::FleetFormat::current();
    state
        .snapshot_execution_state(fleet)
        .await
        .expect("capture the session's bindings");
    state.acknowledge_execution_state_capture();
    let saved = state
        .hydrated_execution_state(fleet)
        .await
        .expect("the saved session state");
    let mut reloaded = typescript_state();
    reloaded
        .restore_execution_state(&saved, fleet)
        .await
        .expect("load the session's bindings");

    let after = run_cell(
        &mut reloaded,
        cell_context(&host, SESSION, TURN, "exec-code:2", tools),
        &services,
        "alias.push({ n: 4 });\nfinish(rows.length + total);",
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
        &typescript_services(None),
        "finish(await echo.await({ text: \"member\" }));",
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
        &typescript_services(None),
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
        &typescript_services(None),
        "const seen = [];\nfor (const word of [\"a\", \"b\", \"c\"]) {\n  seen.push(await echo.say({ text: word }));\n}\nfinish(seen);",
    )
    .await;
    assert_eq!(finish_of(&response), serde_json::json!(["a", "b", "c"]));
    assert_eq!(tools.answered(), vec!["a", "b", "c"]);
    assert_eq!(
        response
            .calls
            .iter()
            .map(|call| (call.operation.as_str(), call.outcome))
            .collect::<Vec<_>>(),
        vec![("echo.say", lash_core::ExecutedCallOutcome::Ok); 3]
    );
    assert_eq!(response.tool_calls.len(), 3);
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
        &typescript_services(None),
        "console.log(\"before\");\nconst first = await echo.say({ text: \"first\" });\nlet second = \"unset\";\ntry {\n  second = await echo.say({ text: \"held\" });\n} catch (error) {\n  second = \"interrupted\";\n}\nconsole.log(\"after\");\nfinish([first, second]);",
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
            .map(|call| call.outcome)
            .collect::<Vec<_>>(),
        vec![
            lash_core::ExecutedCallOutcome::Ok,
            lash_core::ExecutedCallOutcome::Err
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
        &typescript_services(Some(resolver.clone() as crate::SharedDeferredToolResolver)),
        "const first = await web.fetch({ text: \"first\" });\ntry {\n  await web.fetch({ text: \"held\" });\n} catch (error) {\n  console.log(\"interrupted\");\n}\nconst third = await web.fetch({ text: \"third\" });\nfinish([first, third]);",
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
        &typescript_services(None),
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
    let mut state = RlmExecutionState::new(dialect.name(), dialect.numbers());
    let services = cell_services(&dialect, python_workers(), None);
    let tools = Arc::new(CellTools::default());

    let response = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &services,
        "words = [\"a\", \"b\"]\nsaid = await echo_say({\"text\": \"py\"})\nprint(said)\ntotal = len(words) + 40",
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
    let services = typescript_services(None);
    let mut state = typescript_state();
    let first = run_cell(
        &mut state,
        cell_context(&host, SESSION, TURN, "exec-code:0", tools.clone()),
        &services,
        "const format = (value) => value;",
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
    let dropped = std::collections::BTreeSet::from([lash_kernel_doc::Name::new("format")]);
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
            lash_kernel_vm::RunError::Uncaught(value.clone()),
            &dropped,
        );
        if expected {
            assert_eq!(
                observation,
                crate::feedback::CellObservation::BindingNotCarried {
                    binding: "format".to_owned(),
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

/// FIG-5763 / ADR 0132 NR-2: an interrupted Once effect names the
/// admitted call and warns that its outside work may already have happened.
#[tokio::test(flavor = "multi_thread")]
async fn an_interrupted_once_effect_observation_names_the_effect() {
    let mut host = open_host().await;
    let tools = Arc::new(CellTools::default());
    let response = crash_at_the_held_call_and_resume(
        &mut host,
        &tools,
        &typescript_services(None),
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
    assert!(
        failure.message.contains("site") && failure.message.contains("occurrence 0"),
        "{}",
        failure.message
    );
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
