
/// One turn through the workbench's own session-opening path.
///
/// Deliberately `state.session_builder(...)`, which is what `run_user_turn` and
/// every route use — opening `state.core.session(...)` directly would bypass
/// the very code this file exists to test.
async fn run_turn_through_the_workbench_open_path(
    state: &AppState,
    session_id: &str,
    turn_id: &str,
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
/// The substrate's walker (`dialect::prompt_walker_tests`) can only see the
/// fragments the RLM crate contributes. A host adds its own: the Workbench
/// injects three worked code tutorials into every system prompt, and they were
/// written when Lashlang was the only dialect. Nothing downstream of the
/// substrate would ever have caught that — the served prompt is the only place
/// the two halves meet, so the assertion lives on the served prompt.
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

/// `lashlang_step` is the one identifier that legitimately crosses dialects:
/// it is the `history` payload discriminant and the durable event-id prefix.
/// ADR 0063 carries the whole carve-out list.
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

fn transcript_code_languages(snapshot: &StateReadSnapshot) -> Vec<String> {
    snapshot
        .transcript
        .iter()
        .filter_map(|row| match row {
            TranscriptRow::CodeBlock { language, .. } => Some(language.clone()),
            _ => None,
        })
        .collect()
}

/// The Lashlang direction of the same rule.
const HOST_FOREIGN_MARKERS_LASHLANG: &[&str] = &[
    "<typescript>",
    "</typescript>",
    "console.log(",
    "definePro",
    "registerTrigger",
];

fn foreign_markers_for(dialect: lash::rlm::RlmDialect) -> &'static [&'static str] {
    match dialect {
        lash::rlm::RlmDialect::Lashlang => HOST_FOREIGN_MARKERS_LASHLANG,
        lash::rlm::RlmDialect::Typescript => HOST_FOREIGN_MARKERS,
    }
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
/// The Workbench injects three worked programs into every system prompt. They
/// were written when Lashlang was the only dialect and were injected
/// unconditionally, so a TypeScript session read `## TypeScript execution` and
/// then three complete `<lashlang>` programs to copy — the substrate's own
/// walker cannot see a word of this, because none of it is substrate copy.
#[test]
fn the_workbench_tutorials_are_written_in_the_session_dialect() {
    let mut violations = Vec::new();
    for dialect in lash::rlm::RlmDialect::ALL {
        let prompt = workbench_prompt(dialect);
        for word in foreign_words_in(prompt, foreign_markers_for(dialect)) {
            violations.push(format!("{} prompt carries `{word}`", dialect.language_id()));
        }
        // Non-vacuity: each prompt must actually contain worked programs in its
        // own dialect, or an empty constant would pass every marker check.
        let own_tag = format!("<{}>", dialect.language_id());
        assert!(
            prompt.matches(&own_tag).count() >= 3,
            "the {} prompt must carry its own worked programs",
            dialect.language_id()
        );
    }
    assert!(
        violations.is_empty(),
        "the workbench tutorials mix dialects: {violations:#?}"
    );

    // And the marker lists must be able to fire, or the assertion above is
    // decoration: each prompt read against the *other* dialect's list trips.
    assert!(!foreign_words_in(
        workbench_prompt(lash::rlm::RlmDialect::Lashlang),
        HOST_FOREIGN_MARKERS
    )
    .is_empty());
    assert!(!foreign_words_in(
        workbench_prompt(lash::rlm::RlmDialect::Typescript),
        HOST_FOREIGN_MARKERS_LASHLANG
    )
    .is_empty());
}

/// Every `<typescript>` program in the prompt, in prompt order.
fn typescript_prompt_programs() -> Vec<String> {
    let prompt = workbench_prompt(lash::rlm::RlmDialect::Typescript);
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
/// `with_tool_binding` writes the same binding under both dialect keys, so
/// a TypeScript call path is the Lashlang one.
fn workbench_link_environment() -> lashlang::LashlangHostEnvironment {
    let mut resources = workbench_lashlang_resources();
    lashlang::add_trigger_resource_operations(&mut resources);
    let modules: [(&[&str], &str, &[&str]); 5] = [
        (&["agents"], "Agents", &["spawn"]),
        (&["web"], "Web", &["search", "fetch"]),
        (&["inbox", "work"], "Inbox", &["list", "send", "delete"]),
        (&["inbox", "personal"], "Inbox", &["list", "send", "delete"]),
        // The `tool-value` scenario's own tool, installed by
        // `DevProviderScenario::tool_provider`.
        (&["workbench_surface"], "WorkbenchSurface", &["terminal"]),
    ];
    for (path, resource_type, operations) in modules {
        for operation in operations {
            resources
                .add_module_operation_binding(
                    path.iter().copied(),
                    resource_type,
                    *operation,
                    format!("tool:{}/{operation}", path.join("/")),
                    lashlang::ResourceOperationBinding {
                        input_ty: lashlang::TypeExpr::Any,
                        output_ty: lashlang::TypeExpr::Any,
                        output_from_input: None,
                    },
                )
                .expect("workbench tutorial tool binding");
        }
    }
    lashlang::LashlangHostEnvironment::new(resources, workbench_lashlang_abilities())
}

/// Prompt copy that teaches code the dialect refuses is worse than no copy.
///
/// Every program the TypeScript prompt shows is linked against the Workbench's
/// own declared surface. This is the check that keeps the twin honest: the
/// Lashlang tutorials it was translated from use `await handle` result
/// wrappers, `format`/`join`/`len`, module authorities as process parameters,
/// and record-joined handles, and each of those has a different answer here.
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
    assert!(hits.is_empty(), "prompt programs that do not link: {hits:#?}");

    // The linker must be able to reject, or an empty hit list proves nothing.
    assert!(
        lash_typescript::link("class Unsupported {} finish(1);", &environment).is_err(),
        "the control must be refused"
    );
}

/// The tutorials follow the dialect the turn resolved, not this process's
/// configuration: a store that outlived a config change runs its recorded
/// dialect, and copy keyed on configuration teaches the other one.
#[test]
fn the_tutorials_follow_the_turns_resolved_options() {
    let typescript = lash_core::ProtocolTurnOptions::typed(
        lash_rlm_types::RlmCreateExtras {
            dialect: Some(lash::rlm::RlmDialect::Typescript),
            ..Default::default()
        },
    )
    .expect("typed options");
    assert_eq!(
        tutorial_dialect(&typescript),
        lash::rlm::RlmDialect::Typescript
    );
    assert!(workbench_prompt(tutorial_dialect(&typescript)).contains("<typescript>"));

    // Absent options are how every pre-dialect session reads.
    assert_eq!(
        tutorial_dialect(&lash_core::ProtocolTurnOptions::default()),
        lash::rlm::RlmDialect::Lashlang
    );
}

// The workbench's TypeScript branch, driven end to end.
//
// Every other fixture here builds a Lashlang `AppState`, so nothing reached the
// branch that a `LASH_RUNBOOK_DIALECT=typescript` deployment actually runs. The
// dialect field exists precisely so a test can set it; these do.

/// A served turn must reach the model with the TypeScript prompt, and the
/// session must record `typescript` durably.
///
/// The first version of this fix applied the ambient dialect only on opens that
/// create, and those two call sites open and drop without running a turn. A
/// dialect becomes durable at the session's first commit, so the pin evaporated
/// with the handle and the first real turn — opening with no dialect, finding
/// nothing recorded — committed `lashlang` permanently. Asserting the prompt
/// alone would not have caught it either; the durable read is what pins the
/// mechanism.
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

    let mut state = queued_send_test_state(data_dir.path(), provider).await;
    state.rlm_dialect = lash::rlm::RlmDialect::Typescript;
    let session_id = state.current_session_id();

    run_turn_through_the_workbench_open_path(
        &state,
        &session_id,
        "typescript-dialect-turn",
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

    // Nothing in the served prompt may be written in the other dialect. The
    // substrate's own walker covers the fragments the RLM crate contributes;
    // this covers the host's, which is where the Workbench's three worked
    // tutorials are injected.
    assert_no_lashlang_words(&prompts);

    // The durable half. A prompt can be right for one turn and still leave the
    // session recorded as Lashlang, which is the shape that shipped.
    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("reopen the served session");
    assert_eq!(
        session.read_view().protocol_turn_options().payload["dialect"],
        serde_json::json!("typescript"),
        "the served session must have recorded its dialect durably"
    );
    drop(session);

    // The rendered half: `/api/state` labels the executed code with the dialect
    // the session recorded. A host that labelled from its own configuration
    // would be right here and wrong in the one case the label exists for — a
    // store that outlived a config change — so the label is read back from the
    // same projection the UI renders.
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
        "the rendered transcript must label the executed cell with the recorded dialect"
    );
}

/// The same fixture on the default dialect, so the assertions above cannot pass
/// by the workbench being TypeScript for everyone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_lashlang_workbench_still_serves_lashlang_turns() {
    let data_dir = tempfile::tempdir().expect("temp dir");
    let served_prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = {
        let served_prompts = Arc::clone(&served_prompts);
        lash::testing::TestProvider::builder()
            .kind("lashlang-workbench-test")
            .complete(move |request: lash::provider::LlmRequest| {
                let served_prompts = Arc::clone(&served_prompts);
                async move {
                    // The request's own rendering, so this fixture needs no
                    // message-vocabulary types the facade does not export.
                    served_prompts.lock_recover().push(format!("{request:?}"));
                    Ok(text_response(
                        "<lashlang>\nfinish \"canonical answer\"\n</lashlang>",
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
        "lashlang-dialect-turn",
        "say the canonical answer",
    )
    .await;

    let prompts = served_prompts.lock_recover().clone();
    assert!(!prompts.is_empty(), "the turn must have reached the provider");
    assert!(
        prompts
            .iter()
            .all(|prompt| !prompt.contains("## TypeScript execution")),
        "the default workbench must not serve the TypeScript prompt: {prompts:#?}"
    );

    let session = state
        .core
        .session(session_id.clone())
        .open()
        .await
        .expect("reopen the served session");
    assert_eq!(
        session.read_view().protocol_turn_options().payload["dialect"],
        serde_json::json!("lashlang"),
    );
    drop(session);

    // The label's control: the same projection on the default dialect must say
    // `lashlang`, or the TypeScript assertion above proves only that the field
    // is constant.
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
        vec!["lashlang".to_string()],
    );
}

/// Every scripted development-provider reply, in both dialects.
///
/// The dev provider boots nine of the twenty-one TypeScript judged rows. Its
/// ten replies were Lashlang cells regardless of the configured dialect, and a
/// cell the session cannot execute does not fail the scenario — the turn never
/// reaches a terminal state, so the row hangs. The check therefore walks tags
/// first, then links what a judged row would actually run.
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
    for scenario in scenarios {
        for dialect in lash::rlm::RlmDialect::ALL {
            let open = format!("<{}>", dialect.language_id());
            let close = format!("</{}>", dialect.language_id());
            for call in 0..3 {
                let Some(text) = scenario.scripted_cell_for_test(dialect, call) else {
                    continue;
                };
                seen += 1;
                let label = format!("{} call {call} ({})", scenario.as_str(), dialect.language_id());
                if !text.starts_with(&open) || !text.trim_end().ends_with(&close) {
                    hits.push(format!("{label}: not a {} cell: {text}", dialect.language_id()));
                    continue;
                }
                let code = text
                    .trim_start_matches(&open)
                    .trim_end()
                    .trim_end_matches(&close)
                    .trim();
                match dialect {
                    lash::rlm::RlmDialect::Typescript => {
                        if let Err(error) = lash_typescript::link(code, &environment) {
                            hits.push(format!("{label}: {error}"));
                        }
                    }
                    lash::rlm::RlmDialect::Lashlang => match lashlang::parse(code) {
                        Ok(program) => {
                            // The deliberate code failure is a *runtime* failure
                            // by construction: it must link and then fail while
                            // executing, which is what the scenario renders.
                            if let Err(error) =
                                lashlang::LinkedModule::link(program, environment.clone())
                            {
                                hits.push(format!("{label}: {error}"));
                            }
                        }
                        Err(error) => hits.push(format!("{label}: {error}")),
                    },
                }
            }
        }
    }
    assert!(seen >= 20, "every scenario must script at least one cell in each dialect, saw {seen}");
    assert!(hits.is_empty(), "scripted replies a session cannot run: {hits:#?}");
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
        for dialect in lash::rlm::RlmDialect::ALL {
            let last = scenario
                .scripted_cell_for_test(dialect, 1)
                .unwrap_or_else(|| panic!("{} scripts a second call", scenario.as_str()));
            assert_ne!(
                scenario,
                failure_provider::DevProviderScenario::ToolValue,
                "ToolValue terminates through its tool's control, not a scripted finish"
            );
            let finish = match dialect {
                lash::rlm::RlmDialect::Typescript => "finish(",
                lash::rlm::RlmDialect::Lashlang => "finish ",
            };
            assert!(
                last.contains(finish),
                "{} ({}) must terminate on its retry: {last}",
                scenario.as_str(),
                dialect.language_id()
            );
        }
    }
}

/// The `code-failure` scenario, executed, in both dialects.
///
/// This scenario had no test at all, which is how it shipped scripting
/// `fail "..."` at cell top level — process-only in Lashlang, unspellable in
/// TypeScript. The cell could never commit, and with the workbench's unbounded
/// turn budget the driver re-asked the provider forever, so the failure mode
/// was a hang rather than a red row. The timeout here is the regression guard:
/// a scenario that cannot terminate must fail this test rather than run out the
/// harness.
///
/// **Reproducing the hang takes two reverts, not one.** Restoring the shipped
/// cell alone is not enough: the same fix also added the `call == 0` retry
/// branch, and that branch on its own makes the scenario terminate (the bad
/// cell fails, the retry finishes, and this test passes in ~0.15s). Both
/// changes have to come out — the shipped `fail "..."` cell as the *only*
/// scripted reply — for the 60s timeout to fire with "never reached a terminal
/// state". With only the cell reverted, the fixture that goes red instead is
/// `every_scripted_dev_provider_reply_is_a_cell_of_the_hosts_dialect`, which
/// link-verifies the cell in both dialects.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_code_failure_scenario_renders_a_failed_cell_and_terminates() {
    for dialect in lash::rlm::RlmDialect::ALL {
        let data_dir = tempfile::tempdir().expect("temp dir");
        let provider = failure_provider::DevProviderScenario::CodeFailure.provider(dialect);
        let mut state = queued_send_test_state(data_dir.path(), provider).await;
        state.rlm_dialect = dialect;
        let session_id = state.current_session_id();

        tokio::time::timeout(
            Duration::from_secs(60),
            run_turn_through_the_workbench_open_path(
                &state,
                &session_id,
                "code-failure-turn",
                "run the deterministic code failure",
            ),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the {} code-failure scenario never reached a terminal state",
                dialect.language_id()
            )
        });

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
                .any(|(language, success)| language == dialect.language_id() && !success),
            "the {} scenario must render a failed cell of its own dialect: {blocks:?}",
            dialect.language_id()
        );
        let answers = transcript_answers(&projected);
        assert!(
            answers
                .iter()
                .any(|answer| answer.contains("session recovered after code failure")),
            "the {} code-failure scenario must recover and reach a finish within the turn budget: transcript answers {answers:?}",
            dialect.language_id()
        );
    }
}

/// A scripted provider that answers each call with the next cell in a list.
fn scripted_cells_provider(kind: &'static str, cells: Vec<String>) -> lash::provider::ProviderHandle {
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

/// Cell A binds, cell B reads, cell C rebinds and reads back — in both
/// dialects, through the production turn path.
///
/// This is the session model the prompt promises: top-level bindings persist
/// across cells and are listed under `=== BOUND VARIABLES ===` with their
/// values. Lashlang has always delivered it because it resolves names at link,
/// where the live session globals are known. The TypeScript lowerer resolved
/// every name at parse against source-local scopes, so cell B rejected with
/// `TS_UNKNOWN_BINDING` for a name the same prompt was showing it — and every
/// crate-level test missed it, because they pre-supply their bindings in the
/// same source they compile.
///
/// The test is deliberately parameterized over both dialects and driven
/// through `session_builder(...).turn(...)`, the path a served turn uses: any
/// future dialect that resolves names its own way fails here loudly rather than
/// shipping as a demo that cannot hold state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cell_reads_what_an_earlier_cell_bound_in_both_dialects() {
    for (dialect, cells) in [
        (
            lash::rlm::RlmDialect::Lashlang,
            vec![
                "<lashlang>\nfindings = { summary: \"first pass\" }\nfinish \"bound\"\n</lashlang>"
                    .to_string(),
                "<lashlang>\nfinish findings.summary\n</lashlang>".to_string(),
                "<lashlang>\nfindings = { summary: \"second pass\" }\nfinish findings.summary\n</lashlang>"
                    .to_string(),
            ],
        ),
        (
            lash::rlm::RlmDialect::Typescript,
            vec![
                "<typescript>\nconst findings = { summary: \"first pass\" };\nfinish(\"bound\");\n</typescript>"
                    .to_string(),
                "<typescript>\nfinish(findings.summary);\n</typescript>".to_string(),
                "<typescript>\nconst findings = { summary: \"second pass\" };\nfinish(findings.summary);\n</typescript>"
                    .to_string(),
            ],
        ),
    ] {
        let data_dir = tempfile::tempdir().expect("temp dir");
        let provider = scripted_cells_provider("session-globals", cells);
        let mut state = queued_send_test_state(data_dir.path(), provider).await;
        state.rlm_dialect = dialect;
        let session_id = state.current_session_id();

        for (index, prompt) in ["bind it", "read it back", "rebind and read"]
            .into_iter()
            .enumerate()
        {
            run_turn_through_the_workbench_open_path(
                &state,
                &session_id,
                &format!("session-globals-{}-{index}", dialect.language_id()),
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
            "{} must read the binding a previous cell made: {answers:#?}",
            dialect.language_id()
        );
        assert!(
            answers.iter().any(|answer| answer.contains("second pass")),
            "{} must read back a rebound session global: {answers:#?}",
            dialect.language_id()
        );
    }
}

/// The same session model across a *restart*: a second host process opens the
/// same store and the next cell still reads what the first process bound.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rehydrated_session_still_reads_its_earlier_bindings_in_both_dialects() {
    for (dialect, first, second) in [
        (
            lash::rlm::RlmDialect::Lashlang,
            "<lashlang>\nfindings = { summary: \"survived\" }\nfinish \"bound\"\n</lashlang>",
            "<lashlang>\nfinish findings.summary\n</lashlang>",
        ),
        (
            lash::rlm::RlmDialect::Typescript,
            "<typescript>\nconst findings = { summary: \"survived\" };\nfinish(\"bound\");\n</typescript>",
            "<typescript>\nfinish(findings.summary);\n</typescript>",
        ),
    ] {
        let data_dir = tempfile::tempdir().expect("temp dir");
        let session_id = {
            let mut state = queued_send_test_state(
                data_dir.path(),
                scripted_cells_provider("session-globals-restart", vec![first.to_string()]),
            )
            .await;
            state.rlm_dialect = dialect;
            let session_id = state.current_session_id();
            run_turn_through_the_workbench_open_path(&state, &session_id, "bind it", "bind it")
                .await;
            session_id
        };

        // A second host process over the same durable store.
        let mut state = queued_send_test_state(
            data_dir.path(),
            scripted_cells_provider("session-globals-restart", vec![second.to_string()]),
        )
        .await;
        state.rlm_dialect = dialect;
        run_turn_through_the_workbench_open_path(
            &state,
            &session_id,
            "read after restart",
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
            "{} must read its earlier binding after a restart: {answers:#?}",
            dialect.language_id()
        );
    }
}

/// The negative control, per dialect: a name neither the cell nor the session
/// has must still be refused, and the refusal must reach the model.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_name_no_one_has_is_still_refused_in_both_dialects() {
    for (dialect, cells, expected) in [
        (
            lash::rlm::RlmDialect::Lashlang,
            vec![
                "<lashlang>\nfinish nowhere\n</lashlang>".to_string(),
                "<lashlang>\nfinish \"recovered\"\n</lashlang>".to_string(),
            ],
            "nowhere",
        ),
        (
            lash::rlm::RlmDialect::Typescript,
            vec![
                "<typescript>\nfinish(nowhere);\n</typescript>".to_string(),
                "<typescript>\nfinish(\"recovered\");\n</typescript>".to_string(),
            ],
            "TS_UNKNOWN_BINDING",
        ),
    ] {
        let data_dir = tempfile::tempdir().expect("temp dir");
        let mut state = queued_send_test_state(
            data_dir.path(),
            scripted_cells_provider("session-globals-unknown", cells),
        )
        .await;
        state.rlm_dialect = dialect;
        let session_id = state.current_session_id();
        run_turn_through_the_workbench_open_path(
            &state,
            &session_id,
            "unknown-name-turn",
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
            failures.iter().any(|error| error.contains(expected)),
            "{} must refuse a name nobody has: {failures:#?}",
            dialect.language_id()
        );
    }
}
