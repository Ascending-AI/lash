use super::*;
use lash::SessionId;
use lash::TurnId;

/// One turn through the workbench's own session-opening path.
///
/// Deliberately `state.session_builder(...)`, which is what `run_user_turn` and
/// every route use — opening `state.core.session(...)` directly would bypass
/// the very code this file exists to test.
pub(crate) async fn run_turn_through_the_workbench_open_path(
    state: &AppState,
    session_id: &SessionId,
    turn_id: &TurnId,
    text: &str,
) {
    let session = state
        .session_builder(session_id.to_string())
        .session_spec(lash::SessionSpec::inherit().turn_budget(lash::TurnBudget::bounded(8)))
        .open()
        .await
        .expect("open through the workbench path");
    let turn_state = Arc::new(Mutex::new(TurnStreamState::default()));
    let ui_events = ChannelTurnEvents {
        turn_state: Arc::clone(&turn_state),
    };
    session
        .turn(lash::TurnInput::text(text))
        .turn_id(turn_id.to_string())
        .require_finish()
        .expect("require finish")
        .stream_to(&ui_events)
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
        .flat_map(|row| match row {
            TranscriptRow::Message { message } => vec![message.text.clone()],
            TranscriptRow::CodeBlock { output, .. } => vec![output.clone()],
            TranscriptRow::Reasoning { .. } => Vec::new(),
        })
        .collect()
}

pub(crate) fn transcript_code_languages(snapshot: &StateReadSnapshot) -> Vec<String> {
    snapshot
        .transcript
        .iter()
        .filter_map(|row| match row {
            TranscriptRow::CodeBlock { language, .. } => Some(language.clone()),
            _ => None,
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
fn workbench_link_environment() -> lashlang::LashlangHostEnvironment {
    let mut resources = workbench_lashlang_resources();
    lashlang::add_trigger_resource_operations(&mut resources)
        .expect("trigger resource operations are unique");
    let modules: [(&[&str], &str, &[&str]); 4] = [
        (&["agents"], "Agents", &["spawn"]),
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
                    &lashlang::OperationContract::new(serde_json::json!({}), serde_json::json!({})),
                )
                .expect("workbench tutorial tool binding");
        }
    }
    lashlang::LashlangHostEnvironment::new(resources, workbench_lashlang_abilities())
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
        if let Err(error) = lash_typescript::link(program, &environment) {
            hits.push(format!("tutorial {}: {error}", index + 1));
        }
    }
    assert!(
        hits.is_empty(),
        "prompt programs that do not link: {hits:#?}"
    );

    // The linker must be able to reject, or an empty hit list proves nothing.
    assert!(
        lash_typescript::link("class Unsupported {} finish(1);", &environment).is_err(),
        "the control must be refused"
    );
}

// ADR 0096: the fixtures that resolved a recorded dialect from the session bag
// and refused a malformed one are gone with `RlmDialect`.

// The workbench, driven end to end.

/// A served turn must reach the model with the TypeScript prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_typescript_workbench_serves_typescript_turns_and_records_the_dialect() {
    let data_dir = tempfile::tempdir().expect("temp dir");
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

    let state = queued_send_test_state(data_dir.path(), provider).await;
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
            if let Err(error) = lash_typescript::link(code, &environment) {
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

/// Every multi-shot scenario must terminate.
///
/// `code-failure` shipped without one: its only reply was a cell that could
/// never commit, so with the workbench's unbounded turn budget the driver
/// re-asked the provider forever. A retry branch that finishes is what makes
/// each of these a *scenario* rather than a loop.
#[test]
fn every_dev_provider_scenario_reaches_a_finish() {
    // `ToolValue` is excluded, and the exclusion carries its reason: its cell
    // does not finish, it calls a tool whose result *is* the terminal
    // (`ToolControl::Finish`), so the turn ends on the tool's control rather
    // than on a `finish` in the cell. It is single-shot for that reason, not
    // an oversight — the shape `code-failure` got wrong was a scenario that
    // could not terminate at all, and this one terminates through the other
    // seam. `RenderedSurface` is likewise single-shot and does finish, so it
    // is covered by the tag/link walk instead.
    for scenario in [
        failure_provider::DevProviderScenario::AuthFailureOnce,
        failure_provider::DevProviderScenario::RateLimitOnce,
        failure_provider::DevProviderScenario::PartialOutputFailure,
        failure_provider::DevProviderScenario::ExecBlocked,
        failure_provider::DevProviderScenario::CodeFailure,
        failure_provider::DevProviderScenario::RetryResetPartial,
    ] {
        let last = scenario
            .scripted_cell_for_test(1)
            .unwrap_or_else(|| panic!("{} scripts a second call", scenario.as_str()));
        assert_ne!(
            scenario,
            failure_provider::DevProviderScenario::ToolValue,
            "ToolValue terminates through its tool's control, not a scripted finish"
        );
        assert!(
            last.contains("finish("),
            "{} must terminate on its retry: {last}",
            scenario.as_str()
        );
    }
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
    let data_dir = tempfile::tempdir().expect("temp dir");
    let provider = failure_provider::DevProviderScenario::CodeFailure.provider();
    let state = queued_send_test_state(data_dir.path(), provider).await;
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
        .filter_map(|row| match row {
            TranscriptRow::CodeBlock {
                language, success, ..
            } => Some((language.clone(), *success)),
            _ => None,
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
    let data_dir = tempfile::tempdir().expect("temp dir");
    let provider = scripted_cells_provider("session-globals", cells);
    let state = queued_send_test_state(data_dir.path(), provider).await;
    let session_id = state.current_session_id();

    for (index, prompt) in ["bind it", "read it back", "rebind and read"]
        .into_iter()
        .enumerate()
    {
        run_turn_through_the_workbench_open_path(
            &state,
            &session_id,
            &TurnId::from(format!("session-globals-{index}")),
            prompt,
        )
        .await;
    }

    // The second turn's answer is the value the *first* turn bound, and the
    // third turn's is the rebound one. Read from the rendered transcript,
    // which is what a host and a judged row see.
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
    let data_dir = tempfile::tempdir().expect("temp dir");
    let session_id = {
        let state = queued_send_test_state(
            data_dir.path(),
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
        session_id
    };

    // A second host process over the same durable store.
    let state = queued_send_test_state(
        data_dir.path(),
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
    let data_dir = tempfile::tempdir().expect("temp dir");
    let state = queued_send_test_state(
        data_dir.path(),
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
        .filter_map(|row| match row {
            TranscriptRow::CodeBlock { error, .. } => error.clone(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        failures
            .iter()
            .any(|error| error.contains("TS_UNKNOWN_BINDING")),
        "a name nobody has must be refused: {failures:#?}"
    );
}
