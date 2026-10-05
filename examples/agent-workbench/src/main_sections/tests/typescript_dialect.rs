use super::*;
use lash::SessionId;
use lash::TurnId;
use lash::rlm::RlmSendBuilderExt;

/// One turn through the workbench's own session-opening path.
///
/// Deliberately `state.session_builder(...)`, which is what `start_user_turn`
/// and every route use — opening `state.core.session(...)` directly would bypass
/// the very code this file exists to test.
pub(crate) async fn run_turn_through_the_workbench_open_path(
    state: &AppState,
    session_id: &SessionId,
    turn_id: &TurnId,
    text: &str,
) {
    // A turn route creates the session with the host's selection when it is
    // new, as `start_user_turn` does.
    state
        .ensure_session(session_id)
        .await
        .expect("create the session through the workbench path");
    // The previous turn's shift can still hold the session's execution lease a
    // moment after its terminal is observable, so the open goes through the
    // same bounded retry every route applies.
    let session = retry_session_open(
        "test.workbench_open_path",
        || {
            state
                .session_builder(SessionId::fixture(session_id.to_string()))
                .open()
        },
        |event, payload| state.trace_for_session(session_id, event, payload),
    )
    .await
    .expect("open through the workbench path");
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let ui_events = ChannelTurnEvents {
        turn_state: Arc::clone(&turn_state),
    };
    session
        .send(lash::TurnInput::text(text))
        .id(turn_id.clone())
        .require_finish()
        .expect("require finish")
        .output_into(&ui_events)
        .await
        .expect("run the turn");
}

/// The host's half of ADR 0063, as a marker list.
///
/// The substrate's walker can only see the fragments the RLM crate
/// contributes. A host adds its own: the Workbench injects three worked code
/// tutorials into every system prompt. Nothing downstream of the substrate
/// would ever catch a stale one — the served prompt is the only place the two
/// halves meet, so the assertion lives on the served prompt.
const HOST_FOREIGN_MARKERS: &[&str] = &[
    "<lashlang>",
    "</lashlang>",
    "lashlang block",
    "lashlang blocks",
    "lashlang process",
    "bound in lashlang",
    "re-print",
    "finish <value>",
];

/// `lashlang_step` is the one identifier that legitimately carries the old
/// name: it is the `history` payload discriminant and the durable event-id
/// prefix. ADR 0063 carries the whole carve-out list.
fn strip_substrate_carve_outs(text: &str) -> String {
    text.replace("lashlang_step", "«substrate carve-out»")
}

fn assert_no_lashlang_words(prompts: &[String]) {
    let mut violations = Vec::new();
    for prompt in prompts {
        let haystack = strip_substrate_carve_outs(prompt).to_lowercase();
        for marker in HOST_FOREIGN_MARKERS {
            if haystack.contains(marker) {
                violations.push((*marker).to_string());
            }
        }
    }
    violations.sort();
    violations.dedup();
    assert!(
        violations.is_empty(),
        "a TypeScript session was served Lashlang words: {violations:?}"
    );
}

/// Everything the rendered transcript says back to the user: assistant rows and
/// the output of each executed cell.
fn transcript_answers(snapshot: &StateReadSnapshot) -> Vec<String> {
    snapshot
        .transcript
        .iter()
        .filter(|row| row.suppressed.is_none())
        .flat_map(|row| {
            [Some(row.content.text.clone()), row.content.output.clone()]
                .into_iter()
                .flatten()
        })
        .collect()
}

pub(crate) fn transcript_code_languages(snapshot: &StateReadSnapshot) -> Vec<String> {
    snapshot
        .transcript
        .iter()
        .filter_map(|row| {
            (row.kind == lash::transcript::TranscriptRowKind::CodeBlock)
                .then(|| row.content.language.clone())
                .flatten()
        })
        .collect()
}

fn foreign_words_in(text: &str, markers: &[&str]) -> Vec<String> {
    let haystack = strip_substrate_carve_outs(text).to_lowercase();
    markers
        .iter()
        .filter(|marker| haystack.contains(&marker.to_lowercase()))
        .map(|marker| (*marker).to_string())
        .collect()
}

/// The host's counterpart to the substrate's prompt walker (ADR 0063).
///
/// The Workbench injects three worked programs into every system prompt.
/// The substrate's own walker cannot see a word of this, because none of it is
/// substrate copy.
///
/// ADR 0096: the half that read the Lashlang prompt against the TypeScript
/// marker list is gone with the second dialect, and with it the marker list's
/// own non-vacuity check.
#[test]
fn the_workbench_tutorials_are_written_in_the_session_dialect() {
    let prompt = workbench_prompt();
    let violations = foreign_words_in(prompt, HOST_FOREIGN_MARKERS);
    // Non-vacuity: the prompt must actually contain worked programs, or an
    // empty constant would pass every marker check.
    assert!(
        prompt.matches("<typescript>").count() >= 3,
        "the workbench prompt must carry its own worked programs"
    );
    assert!(
        violations.is_empty(),
        "the workbench tutorials carry foreign words: {violations:#?}"
    );
}

/// Every `<typescript>` program in the prompt, in prompt order.
fn typescript_prompt_programs() -> Vec<String> {
    let prompt = workbench_prompt();
    let mut programs = Vec::new();
    let mut rest = prompt;
    while let Some(open) = rest.find("<typescript>") {
        let body = &rest[open + "<typescript>".len()..];
        let close = body
            .find("</typescript>")
            .expect("every opened cell closes in the prompt");
        programs.push(body[..close].to_string());
        rest = &body[close..];
    }
    programs
}

/// The host surface the tutorials call, as the linker sees it.
///
/// The trigger sources and their event types come from the Workbench's own
/// declaration (`workbench_lashlang_resources`), so a change there is a change
/// here. The tool modules are stated at the paths the real bindings produce —
/// `with_tool_binding` writes the binding at the same path a TypeScript call
/// uses.
fn workbench_link_environment() -> lash::rlm::lang::LashlangHostEnvironment {
    let mut resources = workbench_lashlang_resources();
    lash::rlm::lang::add_trigger_resource_operations(&mut resources)
        .expect("trigger resource operations are unique");
    lash::rlm::lang::add_trigger_register_tool_binding(&mut resources)
        .expect("trigger register tool binding is unique");
    let modules: [(&[&str], &str, &[&str]); 5] = [
        (&["agents"], "Agents", &["spawn"]),
        (&["control"], "Control", &["continue_as"]),
        (&["inbox", "work"], "Inbox", &["list", "send", "delete"]),
        (&["inbox", "personal"], "Inbox", &["list", "send", "delete"]),
        // The `tool-value` scenario's own tool, installed by
        // `DevProviderScenario::tool_provider`.
        (&["workbench_surface"], "WorkbenchSurface", &["terminal"]),
    ];
    for (path, resource_type, operations) in modules {
        for operation in operations {
            resources
                .add_module_operation_contract(
                    path.iter().copied(),
                    resource_type,
                    *operation,
                    format!("tool:{}/{operation}", path.join("/")),
                    &lash::rlm::lang::OperationContract::new(
                        serde_json::json!({}),
                        serde_json::json!({}),
                    ),
                )
                .expect("workbench tutorial tool binding");
        }
    }
    // FIG-4177 (bf41d19ca5): create returns the immutable definition record
    // start accepts. Read that shape from the shipped start contract.
    let start = lash::process_controls::process_tool_definition(
        lash::process_controls::ProcessControlTool::Start,
    );
    let definition_schema =
        start.contract().input_schema.canonical()["properties"]["definition"].clone();
    resources
        .add_module_operation_contract(
            ["processes"],
            "Processes",
            "create",
            "tool:create_process",
            &lash::rlm::lang::OperationContract::new(
                serde_json::json!({
                    "type": "object",
                    "properties": { "source": { "type": "string" }, "dialect": { "type": "string" } },
                    "required": ["source", "dialect"],
                    "additionalProperties": false
                }),
                definition_schema,
            ),
        )
        .expect("link process create operation");
    add_process_control_operations(&mut resources);
    lash::rlm::lang::LashlangHostEnvironment::new(resources, workbench_lashlang_abilities())
}

/// The `processes` module the workbench's process-controls plugin binds.
///
/// The module is catalogue presence, not an ability bit (ADR 0095): a session
/// sees it only because `bootstrap` installs
/// `SessionProcessAdminPluginFactory`, so this fixture declares exactly the
/// operations that plugin binds, each carrying the shipped tool's own contract.
fn add_process_control_operations(resources: &mut lash::rlm::lang::LashlangHostCatalog) {
    for (operation, definition) in [
        (
            "start",
            lash::process_controls::process_tool_definition(
                lash::process_controls::ProcessControlTool::Start,
            ),
        ),
        (
            "signal",
            lash::process_controls::process_tool_definition(
                lash::process_controls::ProcessControlTool::Signal,
            ),
        ),
        (
            "emit",
            lash::process_controls::process_tool_definition(
                lash::process_controls::ProcessControlTool::Emit,
            ),
        ),
        (
            "get",
            lash::process_controls::process_tool_definition(
                lash::process_controls::ProcessControlTool::Get,
            ),
        ),
        (
            "list",
            lash::process_controls::process_tool_definition(
                lash::process_controls::ProcessControlTool::List,
            ),
        ),
        (
            "await",
            lash::process_controls::process_tool_definition(
                lash::process_controls::ProcessControlTool::Await,
            ),
        ),
        (
            "cancel",
            lash::process_controls::process_tool_definition(
                lash::process_controls::ProcessControlTool::Cancel,
            ),
        ),
    ] {
        let contract = definition.contract();
        resources
            .add_module_operation_contract(
                ["processes"],
                "Processes",
                operation,
                definition.manifest().id.to_string(),
                &lash::rlm::lang::OperationContract::new(
                    contract.input_schema.canonical().clone(),
                    contract.output_schema.canonical().clone(),
                ),
            )
            .expect("link process control operation");
    }
}

/// Prompt copy that teaches code the language refuses is worse than no copy.
///
/// Every program the prompt shows is linked against the Workbench's own
/// declared surface.
#[test]
fn the_workbench_typescript_tutorials_link() {
    let environment = workbench_link_environment();
    let programs = typescript_prompt_programs();
    assert_eq!(
        programs.len(),
        3,
        "the TypeScript prompt must carry all three tutorials"
    );
    let mut hits = Vec::new();
    for (index, program) in programs.iter().enumerate() {
        if let Err(error) = lash::typescript::link(program, &environment) {
            hits.push(format!("tutorial {}: {error}", index + 1));
        }
    }
    assert!(
        hits.is_empty(),
        "prompt programs that do not link: {hits:#?}"
    );

    // The linker must be able to reject, or an empty hit list proves nothing.
    assert!(
        lash::typescript::link("class Unsupported {} finish(1);", &environment).is_err(),
        "the control must be refused"
    );
}

/// Linking is not execution: the refusals that matter most to prompt copy fire
/// in the VM.
///
/// `"..." + handle` links cleanly and then finishes with `[object Object]` in
/// place of the handle's key (a plain object's string is its type tag), which
/// is the placeholder a model copying the old tutorial verbatim produced on the
/// workbench (FIG-3211). So every tutorial is *run*, not just linked, and the
/// control below proves this harness can still see that placeholder.
struct TutorialHost {
    environment: lash::rlm::lang::LashlangHostEnvironment,
}

/// The one subscription key the tutorial host hands back.
const TUTORIAL_SUBSCRIPTION_KEY: &str = "workbench-tutorial-subscription";

/// A registration handle shaped like the one the runtime returns.
///
/// `execute_trigger_command` (lash-lashlang-runtime) serializes the mutation
/// receipt and adds `type`/`id`. What this fixture depends on is only what the
/// ticket depends on: it is a plain record, and `subscription_key` is the
/// string-formed field the tutorials render.
fn tutorial_trigger_handle() -> serde_json::Value {
    serde_json::json!({
        "type": "trigger_handle",
        "id": TUTORIAL_SUBSCRIPTION_KEY,
        "subscription_key": TUTORIAL_SUBSCRIPTION_KEY,
        "subscription_id": "workbench-tutorial-subscription-id",
        "incarnation": "1",
        "revision": 1,
        "definition_fingerprint": "workbench-tutorial-fingerprint",
        "enabled": true,
        "disposition": "created"
    })
}

/// The two field names a handle record carries.
///
/// The runtime owns both and neither is on the `lash` facade, so an example
/// spells them itself rather than reaching past the facade for them;
/// `the_workbench_typescript_tutorials_run_without_a_dialect_refusal` is the guard
/// against that spelling drifting.
const HANDLE_MARKER_FIELD: &str = "__handle__";
const HANDLE_MARKER_KIND: &str = "lash";

/// The process the one process-starting tutorial starts.
const TUTORIAL_PROCESS_ID: &str = "workbench-tutorial-process";

/// The handle record the runtime hands back from a process start.
///
/// The id itself is minted, never hand-spelled: `lash::process::HandleId`
/// is the facade's own minting authority, so `await handle` refuses any
/// record whose id this module did not produce.
fn tutorial_process_handle() -> serde_json::Value {
    let id = lash::process::HandleId::process(&lash::ProcessId::fixture(TUTORIAL_PROCESS_ID));
    let mut record = serde_json::Map::new();
    record.insert(
        HANDLE_MARKER_FIELD.to_string(),
        serde_json::Value::String(HANDLE_MARKER_KIND.to_string()),
    );
    record.insert(
        "id".to_string(),
        serde_json::Value::String(id.as_str().to_string()),
    );
    serde_json::Value::Object(record)
}

impl TutorialHost {
    fn new() -> Self {
        Self {
            environment: workbench_link_environment(),
        }
    }

    /// Resolution goes through `resolve_lashlang_module_operation`, the same
    /// function `LashlangExecutionHost` uses, so a renamed or moved binding
    /// surfaces as an unanswered operation instead of falling into a default.
    fn resource_result(
        &self,
        call: &lash::rlm::lang::ResourceOperation,
    ) -> Result<lash::rlm::lang::Value, lash::rlm::lang::ExecutionHostError> {
        let lash::rlm::lang::Value::Resource(receiver) = &call.receiver else {
            return Err(lash::rlm::lang::ExecutionHostError::new(format!(
                "`{}` was called on something that is not a module authority",
                call.operation
            )));
        };
        let host_operation = lash::rlm::resolve_lashlang_module_operation(
            &self.environment,
            receiver,
            &call.operation,
        )?;
        let process_start = lash::process_controls::process_tool_definition(
            lash::process_controls::ProcessControlTool::Start,
        )
        .manifest()
        .id
        .to_string();
        if host_operation == lash::rlm::lang::REGISTER_TRIGGER_TOOL_ID {
            return Ok(lash::rlm::lang::from_json(tutorial_trigger_handle()));
        }
        if host_operation == lash::rlm::lang::TriggerHostOperation::List.host_operation() {
            return Ok(lash::rlm::lang::from_json(serde_json::json!([
                tutorial_trigger_handle()
            ])));
        }
        if host_operation == "tool:create_process" {
            // Creation now takes source text. Keep the tutorial law's check
            // of the process body against the host surface before mocking its
            // publication receipt, just as the inline body was link-checked.
            let Some(lash::rlm::lang::Value::String(source)) = call
                .args
                .first()
                .and_then(lash::rlm::lang::Value::as_record)
                .and_then(|input| input.get("source"))
            else {
                return Err(lash::rlm::lang::ExecutionHostError::new(
                    "the tutorial must create a process from source text",
                ));
            };
            lash::typescript::link(source.as_str(), &self.environment).map_err(|error| {
                lash::rlm::lang::ExecutionHostError::new(format!(
                    "the tutorial's process source does not link: {error}"
                ))
            })?;
            return Ok(lash::rlm::lang::from_json(serde_json::json!({
                "id": { "$lash_definition_id": format!("lash.definition:sha256:{}", "0".repeat(64)) },
                "signature": { "signature": "unknown" }
            })));
        }
        if host_operation == process_start {
            return Ok(lash::rlm::lang::from_json(tutorial_process_handle()));
        }
        Err(lash::rlm::lang::ExecutionHostError::new(format!(
            "the workbench tutorials reached an unanswered host operation `{host_operation}`"
        )))
    }
}

impl lash::rlm::lang::ExecutionHost for TutorialHost {
    async fn perform(
        &self,
        op: lash::rlm::lang::AbilityOp,
    ) -> Result<lash::rlm::lang::AbilityOutcome, lash::rlm::lang::ExecutionHostError> {
        match op {
            lash::rlm::lang::AbilityOp::ResourceOperation(call) => self
                .resource_result(&call)
                .map(lash::rlm::lang::AbilityOutcome::Value),
            // The one tutorial that awaits a process awaits a subagent branch,
            // whose declared output is `{ summary, key_metrics }`.
            lash::rlm::lang::AbilityOp::Await(_) => Ok(lash::rlm::lang::AbilityOutcome::Value(
                lash::rlm::lang::from_json(serde_json::json!({
                    "summary": "what the branch found",
                    "key_metrics": ["first metric", "second metric"]
                })),
            )),
            lash::rlm::lang::AbilityOp::Finish(value) => {
                Ok(lash::rlm::lang::AbilityOutcome::Value(value))
            }
            lash::rlm::lang::AbilityOp::Print(_) => Ok(lash::rlm::lang::AbilityOutcome::Unit),
            other => Err(lash::rlm::lang::ExecutionHostError::new(format!(
                "the workbench tutorials should not reach {other:?}"
            ))),
        }
    }
}

async fn run_tutorial(source: &str) -> Result<lash::rlm::lang::ExecutionOutcome, String> {
    let host = TutorialHost::new();
    let linked = lash::typescript::link(source, &host.environment)
        .map_err(|error| format!("does not link: {error}"))?;
    let compiled = lash::rlm::lang::testing::harness::compile_linked_main(&linked);
    lash::rlm::lang::execute(&compiled, &mut lash::rlm::lang::State::new(), &host)
        .await
        .map_err(|error| format!("{error:?}"))
}

/// Every tutorial the prompt ships runs to a finish, with no `TS_` refusal.
///
/// FIG-3211: the button-watcher tutorial ended in
/// `finish("… `" + handle + "` …")`, and `triggers.register` returns a plain
/// record, so the cell the prompt taught failed on its last line. Linking never
/// saw it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_workbench_typescript_tutorials_run_without_a_dialect_refusal() {
    let programs = typescript_prompt_programs();
    assert_eq!(
        programs.len(),
        3,
        "the TypeScript prompt must carry all three tutorials"
    );
    let mut hits = Vec::new();
    for (index, program) in programs.iter().enumerate() {
        match run_tutorial(program).await {
            Ok(lash::rlm::lang::ExecutionOutcome::Finished(_)) => {}
            Ok(other) => hits.push(format!("tutorial {}: {other:?}", index + 1)),
            Err(problem) => hits.push(format!("tutorial {}: {problem}", index + 1)),
        }
    }
    assert!(
        hits.is_empty(),
        "prompt programs the runtime refuses: {hits:#?}"
    );
}

/// The tutorials are what the model copies, so a registration key a tutorial
/// prints is a hash the agent then quotes at the user ("registration
/// derived/v3/83cc…a4a is active", FIG-5036). A tutorial's visible answer names
/// a registration by what it watches, never by its machine key.
#[test]
fn the_workbench_tutorials_never_print_a_registration_key() {
    let programs = typescript_prompt_programs();
    let finishes = programs
        .iter()
        .flat_map(|program| program.lines())
        .filter(|line| line.trim_start().starts_with("finish("))
        .collect::<Vec<_>>();
    assert!(
        finishes.len() >= 3,
        "every tutorial ends in a visible answer: {finishes:#?}"
    );
    let quoting = finishes
        .iter()
        .filter(|line| line.contains("subscription_key") || line.contains("handle"))
        .collect::<Vec<_>>();
    assert!(
        quoting.is_empty(),
        "tutorial answers that quote a registration identity: {quoting:#?}"
    );
}

// ADR 0096: the fixtures that resolved a recorded dialect from the session bag
// and refused a malformed one are gone with `RlmDialect`.

// The workbench, executed end to end.

/// A served turn must reach the model with the TypeScript prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_typescript_workbench_serves_typescript_turns_and_records_the_dialect() {
    let served_prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = {
        let served_prompts = Arc::clone(&served_prompts);
        lash::testing::TestProvider::builder()
            .kind("typescript-workbench-test")
            .complete(move |request: lash::provider::LlmRequest| {
                let served_prompts = Arc::clone(&served_prompts);
                async move {
                    // The request's own rendering, so this fixture needs no
                    // message-vocabulary types the facade does not export.
                    let rendered = format!("{request:?}");
                    served_prompts.lock_recover().push(rendered.clone());
                    Ok(text_response(
                        "<typescript>\nfinish(\"canonical answer\");\n</typescript>",
                    ))
                }
            })
            .build()
            .into_handle()
    };

    let double = crate::tests::test_double_backend(0).await;
    let state = queued_send_test_state(&double, provider).await;
    let session_id = state.current_session_id();

    run_turn_through_the_workbench_open_path(
        &state,
        &session_id,
        &TurnId::from("typescript-dialect-turn"),
        "say the canonical answer",
    )
    .await;

    let prompts = served_prompts.lock_recover().clone();
    assert!(
        !prompts.is_empty(),
        "the turn must have reached the provider"
    );
    assert!(
        prompts
            .iter()
            .all(|prompt| prompt.contains("## TypeScript execution")),
        "every served prompt must be the TypeScript one: {prompts:#?}"
    );

    // Nothing in the served prompt may carry the retired language's words. The
    // substrate's own walker covers the fragments the RLM crate contributes;
    // this covers the host's, which is where the Workbench's three worked
    // tutorials are injected.
    assert_no_lashlang_words(&prompts);

    // The rendered half: `/api/state` labels the executed code, read back from
    // the same projection the UI renders.
    let Json(projected) = app_state(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(session_id.clone()),
        }),
    )
    .await
    .expect("project the served session");
    assert_eq!(
        transcript_code_languages(&projected),
        vec!["typescript".to_string()],
        "the rendered transcript must label the executed cell"
    );
}

// ADR 0096: the control fixture that served the same turn on the default
// dialect is gone with the second dialect.

/// Every scripted development-provider reply is a cell the session can run.
///
/// A cell the session cannot execute does not fail the scenario — the turn
/// never reaches a terminal state, so the row hangs. The check therefore walks
/// tags first, then links what a judged row would actually run.
#[test]
fn every_scripted_dev_provider_reply_is_a_cell_of_the_hosts_dialect() {
    let scenarios = [
        failure_provider::DevProviderScenario::AuthFailureOnce,
        failure_provider::DevProviderScenario::RateLimitOnce,
        failure_provider::DevProviderScenario::PartialOutputFailure,
        failure_provider::DevProviderScenario::FailedProcess,
        failure_provider::DevProviderScenario::ExecBlocked,
        failure_provider::DevProviderScenario::ToolValue,
        failure_provider::DevProviderScenario::RenderedSurface,
        failure_provider::DevProviderScenario::CodeFailure,
        failure_provider::DevProviderScenario::RetryResetPartial,
        failure_provider::DevProviderScenario::TranscriptProjection,
    ];
    let environment = workbench_link_environment();
    let mut hits = Vec::new();
    let mut seen = 0usize;
    let open = "<typescript>";
    let close = "</typescript>";
    for scenario in scenarios {
        for call in 0..3 {
            let Some(text) = scenario.scripted_cell_for_test(call) else {
                continue;
            };
            seen += 1;
            let label = format!("{} call {call}", scenario.as_str());
            if !text.starts_with(open) || !text.trim_end().ends_with(close) {
                hits.push(format!("{label}: not a typescript cell: {text}"));
                continue;
            }
            let code = text
                .trim_start_matches(open)
                .trim_end()
                .trim_end_matches(close)
                .trim();
            if let Err(error) = lash::typescript::link(code, &environment) {
                hits.push(format!("{label}: {error}"));
            }
        }
    }
    assert!(
        seen >= 10,
        "every scenario must script at least one cell, saw {seen}"
    );
    assert!(
        hits.is_empty(),
        "scripted replies a session cannot run: {hits:#?}"
    );
}

/// The `code-failure` scenario, executed.
///
/// This scenario had no test at all, which is how it shipped scripting a cell
/// that could never commit; with the workbench's unbounded turn budget the
/// driver re-asked the provider forever, so the failure mode was a hang rather
/// than a red row. The timeout here is the regression guard: a scenario that
/// cannot terminate must fail this test rather than run out the harness.
///
/// ADR 0096: this ran once per dialect; TypeScript is the sole RLM language.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_code_failure_scenario_renders_a_failed_cell_and_terminates() {
    let provider = failure_provider::DevProviderScenario::CodeFailure.provider();
    let double = crate::tests::test_double_backend(0).await;
    let state = queued_send_test_state(&double, provider).await;
    let session_id = state.current_session_id();

    tokio::time::timeout(
        Duration::from_secs(60),
        run_turn_through_the_workbench_open_path(
            &state,
            &session_id,
            &TurnId::from("code-failure-turn"),
            "run the deterministic code failure",
        ),
    )
    .await
    .expect("the code-failure scenario never reached a terminal state");

    let Json(projected) = app_state(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(session_id.clone()),
        }),
    )
    .await
    .expect("project the code-failure session");
    let blocks = projected
        .transcript
        .iter()
        .filter(|row| row.kind == lash::transcript::TranscriptRowKind::CodeBlock)
        .map(|row| {
            (
                row.content.language.clone().unwrap(),
                row.content.success.unwrap(),
            )
        })
        .collect::<Vec<_>>();
    assert!(
        blocks
            .iter()
            .any(|(language, success)| language == "typescript" && !success),
        "the scenario must render a failed cell: {blocks:?}"
    );
    let answers = transcript_answers(&projected);
    assert!(
        answers
            .iter()
            .any(|answer| answer.contains("session recovered after code failure")),
        "the code-failure scenario must recover and reach a finish within the turn budget: transcript answers {answers:?}"
    );
}

/// A scripted provider that answers each call with the next cell in a list.
pub(crate) fn scripted_cells_provider(
    kind: &'static str,
    cells: Vec<String>,
) -> lash::provider::ProviderHandle {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    lash::testing::TestProvider::builder()
        .kind(kind)
        .complete(move |_request: lash::provider::LlmRequest| {
            let cells = cells.clone();
            let calls = Arc::clone(&calls);
            async move {
                let index = calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let cell = cells
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| panic!("the scripted provider ran out of cells at {index}"));
                Ok(text_response(&cell))
            }
        })
        .build()
        .into_handle()
}

/// Cell A binds, cell B reads, cell C rebinds and reads back, through the
/// production turn path.
///
/// This is the session model the prompt promises: top-level bindings persist
/// across cells and are listed under `=== BOUND VARIABLES ===` with their
/// values. The TypeScript lowerer resolved every name at parse against
/// source-local scopes, so cell B rejected with `TS_UNKNOWN_BINDING` for a name
/// the same prompt was showing it — and every crate-level test missed it,
/// because they pre-supply their bindings in the same source they compile.
///
/// ADR 0096: this ran once per dialect; the Lashlang half is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cell_reads_what_an_earlier_cell_bound_in_both_dialects() {
    let cells = vec![
        "<typescript>\nconst findings = { summary: \"first pass\" };\nfinish(\"bound\");\n</typescript>"
            .to_string(),
        "<typescript>\nfinish(findings.summary);\n</typescript>".to_string(),
        "<typescript>\nconst findings = { summary: \"second pass\" };\nfinish(findings.summary);\n</typescript>"
            .to_string(),
    ];
    let provider = scripted_cells_provider("session-globals", cells);
    let double = crate::tests::test_double_backend(0).await;
    let state = queued_send_test_state(&double, provider).await;
    let session_id = state.current_session_id();

    for (index, prompt) in ["bind it", "read it back", "rebind and read"]
        .into_iter()
        .enumerate()
    {
        run_turn_through_the_workbench_open_path(
            &state,
            &session_id,
            &TurnId::fixture(format!("session-globals-{index}")),
            prompt,
        )
        .await;
    }

    // The second turn's answer is the value the *first* turn bound, and the third turn's is
    // the rebound one.
    let Json(projected) = app_state(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(session_id.clone()),
        }),
    )
    .await
    .expect("project the session");
    let answers = transcript_answers(&projected);
    assert!(
        answers.iter().any(|answer| answer.contains("first pass")),
        "a cell must read the binding a previous cell made: {answers:#?}"
    );
    assert!(
        answers.iter().any(|answer| answer.contains("second pass")),
        "a cell must read back a rebound session global: {answers:#?}"
    );
}

/// The same session model across a *restart*: a second host process opens the
/// same store and the next cell still reads what the first process bound.
///
/// ADR 0096: this ran once per dialect; the Lashlang half is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rehydrated_session_still_reads_its_earlier_bindings_in_both_dialects() {
    let first = "<typescript>\nconst findings = { summary: \"survived\" };\nfinish(\"bound\");\n</typescript>";
    let second = "<typescript>\nfinish(findings.summary);\n</typescript>";
    // One double outlives both host processes: its stores are what the
    // second reopens.
    let double = crate::tests::test_double_backend(0).await;
    let session_id = {
        let state = queued_send_test_state(
            &double,
            scripted_cells_provider("session-globals-restart", vec![first.to_string()]),
        )
        .await;
        let session_id = state.current_session_id();
        run_turn_through_the_workbench_open_path(
            &state,
            &session_id,
            &TurnId::from("bind it"),
            "bind it",
        )
        .await;
        // The first process's shift outlives its answer while it closes the
        // run's scope (FIG-3979), and would admit an input sent meanwhile on
        // that process's driver.
        double.settle_session_shift(&session_id).await;
        session_id
    };

    // A second host process over the same durable store.
    let state = queued_send_test_state(
        &double,
        scripted_cells_provider("session-globals-restart", vec![second.to_string()]),
    )
    .await;
    run_turn_through_the_workbench_open_path(
        &state,
        &session_id,
        &TurnId::from("read after restart"),
        "read it back",
    )
    .await;

    let Json(projected) = app_state(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(session_id.clone()),
        }),
    )
    .await
    .expect("project the rehydrated session");
    let answers = transcript_answers(&projected);
    assert!(
        answers.iter().any(|answer| answer.contains("survived")),
        "a session must read its earlier binding after a restart: {answers:#?}"
    );
}

/// The negative control: a name neither the cell nor the session has must still
/// be refused, and the refusal must reach the model.
///
/// ADR 0096: this ran once per dialect; the Lashlang half is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_name_no_one_has_is_still_refused_in_both_dialects() {
    let cells = vec![
        "<typescript>\nfinish(nowhere);\n</typescript>".to_string(),
        "<typescript>\nfinish(\"recovered\");\n</typescript>".to_string(),
    ];
    let double = crate::tests::test_double_backend(0).await;
    let state = queued_send_test_state(
        &double,
        scripted_cells_provider("session-globals-unknown", cells),
    )
    .await;
    let session_id = state.current_session_id();
    run_turn_through_the_workbench_open_path(
        &state,
        &session_id,
        &TurnId::from("unknown-name-turn"),
        "read a name nobody has",
    )
    .await;

    let Json(projected) = app_state(
        State(state.clone()),
        Query(SessionQuery {
            session_id: Some(session_id.clone()),
        }),
    )
    .await
    .expect("project the session");
    let failures = projected
        .transcript
        .iter()
        .filter_map(|row| row.content.error.clone())
        .collect::<Vec<_>>();
    assert!(
        failures
            .iter()
            .any(|error| error.contains("TS_UNKNOWN_BINDING")),
        "a name nobody has must be refused: {failures:#?}"
    );
}
