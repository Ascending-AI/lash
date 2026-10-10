use super::*;

/// The host's half of ADR 0063, as a marker list.
///
/// The substrate's walker can only see the fragments the RLM crate
/// contributes. A host adds its own: the Workbench injects three worked code
/// tutorials into every system prompt. Nothing downstream of the substrate
/// would ever catch a stale one — the served prompt is the only place the two
/// halves meet, so the assertion lives on the served prompt.
const HOST_FOREIGN_MARKERS: &[&str] = &[
    "<lash_vm>",
    "</lash_vm>",
    "lash_vm block",
    "lash_vm blocks",
    "lash_vm process",
    "bound in lash_vm",
    "re-print",
    "finish <value>",
];

/// `lash_vm_step` is the one identifier that legitimately carries the old
/// name: it is the `history` payload discriminant and the durable event-id
/// prefix. ADR 0063 carries the whole carve-out list.
fn strip_substrate_carve_outs(text: &str) -> String {
    text.replace("lash_vm_step", "«substrate carve-out»")
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
/// ADR 0096: the half that read the Lash VM prompt against the TypeScript
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

/// The effects the tutorials call, at the paths the real bindings produce.
/// `workbench.register_trigger` and the process controls carry the shipped
/// tools' own contracts.
fn workbench_effects() -> lash::vm::HostBoundary {
    let mut boundary = lash::vm::HostBoundary::new();
    let mut offer = |path: String, definition: Option<&lash::tools::ToolDefinition>| {
        let (id, input, output) = match definition {
            Some(definition) => {
                let contract = definition.contract();
                (
                    definition.manifest().id.clone(),
                    contract.input_schema.canonical().clone(),
                    contract.output_schema.canonical().clone(),
                )
            }
            None => (
                lash::tools::ToolId::from(format!("tool:{}", path.replace('.', "/"))),
                serde_json::json!({}),
                serde_json::json!({}),
            ),
        };
        boundary
            .offer_tool(
                &path,
                id,
                &input,
                &output,
                match path.as_str() {
                    "chat.reply" | "workbench_surface.terminal" => {
                        lash::tools::TurnControls::finish(lash::schema::JsonSchema::any())
                    }
                    "control.continue_as" => lash::tools::TurnControls::switch_agent_frame(),
                    _ => lash::tools::TurnControls::none(),
                },
            )
            .expect("a workbench tool is offered as an effect");
    };
    offer(
        "workbench.register_trigger".to_owned(),
        Some(&host_triggers::register_trigger_tool_definition()),
    );
    for (module, operations) in [
        ("agents", &["spawn"][..]),
        ("chat", &["reply"][..]),
        ("control", &["continue_as"][..]),
        ("inbox.work", &["list", "send", "delete"][..]),
        ("inbox.personal", &["list", "send", "delete"][..]),
        ("workbench_surface", &["terminal"][..]),
    ] {
        for operation in operations {
            offer(format!("{module}.{operation}"), None);
        }
    }
    for (operation, tool) in [
        ("start", lash::process_controls::ProcessControlTool::Start),
        ("await", lash::process_controls::ProcessControlTool::Await),
        ("cancel", lash::process_controls::ProcessControlTool::Cancel),
    ] {
        offer(
            format!("processes.{operation}"),
            Some(&lash::process_controls::process_tool_definition(tool)),
        );
    }
    boundary
}

/// Prompt copy that teaches code the language refuses is worse than no copy:
/// every program the prompt shows lowers against the Workbench's own
/// declared effects, as a worker would lower it.
#[test]
fn the_workbench_typescript_tutorials_lower() {
    let embedding =
        lash::vm::standard_worker_embedding(&lash::vm::WorkerTuning::default()).expect("embedding");
    let boundary = workbench_effects();
    let effects = boundary.signatures();
    let controls = boundary.controls();
    let lower = |program: &str| {
        embedding
            .lower(
                "typescript",
                program,
                &lash::dialect::Environment {
                    library: embedding.library(),
                    effects: &effects,
                    tool_roots: &Default::default(),
                    controls: &controls,
                    bindings: &Default::default(),
                    functions: &Default::default(),
                },
            )
            .expect("the TypeScript dialect is installed")
    };
    let programs = typescript_prompt_programs();
    assert_eq!(
        programs.len(),
        3,
        "the TypeScript prompt must carry all three tutorials"
    );
    let refused = programs
        .iter()
        .enumerate()
        .filter_map(|(index, program)| {
            lower(program)
                .err()
                .map(|error| format!("tutorial {}: {error}", index + 1))
        })
        .collect::<Vec<_>>();
    assert!(
        refused.is_empty(),
        "prompt programs that do not lower: {refused:#?}"
    );
    // The front end must be able to refuse, or an empty list proves nothing.
    assert!(
        lower("class Unsupported {} await chat.reply(1);").is_err(),
        "the control must be refused"
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
        .filter(|line| line.trim_start().starts_with("await chat.reply("))
        .collect::<Vec<_>>();
    assert!(
        finishes.len() >= 3,
        "every tutorial ends in a visible answer: {finishes:#?}"
    );
    let quoting = finishes
        .iter()
        .filter(|line| line.contains("subscription_id") || line.contains("handle"))
        .collect::<Vec<_>>();
    assert!(
        quoting.is_empty(),
        "tutorial answers that quote a registration identity: {quoting:#?}"
    );
}

// ADR 0096: the fixtures that resolved a recorded dialect from the session bag
// and refused a malformed one are gone with `RlmDialect`.

// The workbench, executed end to end.

// ADR 0096: the control fixture that served the same turn on the default
// dialect is gone with the second dialect.

// The laws below run through the workbench's chat route on the in-process
// durable workbench, whose engine runs every turn.

fn assert_no_lash_vm_words(prompts: &[String]) {
    let mut violations = prompts
        .iter()
        .flat_map(|prompt| foreign_words_in(prompt, HOST_FOREIGN_MARKERS))
        .collect::<Vec<_>>();
    violations.sort();
    violations.dedup();
    assert!(
        violations.is_empty(),
        "a TypeScript session was served Lash VM words: {violations:?}"
    );
}

/// The language of every code-block row of the rendered transcript.
fn transcript_code_languages(snapshot: &StateReadSnapshot) -> Vec<String> {
    snapshot
        .transcript
        .iter()
        .filter(|row| row.kind == crate::ChatRowKind::CodeBlock)
        .filter_map(|row| row.content.language.clone())
        .collect()
}

/// Everything the rendered transcript says back to the user: assistant rows
/// and the output of each executed cell.
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

/// A served turn reaches the model with the TypeScript prompt, carries none
/// of the retired language's words, and the rendered transcript labels its
/// executed cell `typescript`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_typescript_workbench_serves_typescript_turns_and_records_the_dialect() {
    let served_prompts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let provider = {
        let served_prompts = Arc::clone(&served_prompts);
        lash::testing::TestProvider::builder()
            .kind("workbench-harness")
            .complete(move |request: lash::provider::LlmRequest| {
                // The request's own rendering, so this fixture needs no
                // message-vocabulary types the facade does not export.
                served_prompts.lock_recover().push(format!("{request:?}"));
                async { Ok(text_response(&finish_cell("canonical answer"))) }
            })
            .build()
            .into_handle()
    };
    let workbench = Workbench::builder(provider).build().await;
    let state = &workbench.state;
    run_turn(state, "say the canonical answer").await;

    let prompts = served_prompts.lock_recover().clone();
    assert!(!prompts.is_empty(), "the turn must reach the provider");
    assert!(
        prompts
            .iter()
            .all(|prompt| prompt.contains("## TypeScript execution")),
        "every served prompt must be the TypeScript one: {prompts:#?}"
    );
    // The substrate's own walker covers the fragments the RLM crate
    // contributes; this covers the host's, where the workbench's worked
    // tutorials are injected.
    assert_no_lash_vm_words(&prompts);
    let projected = read_state(state, None).await.expect("project the session");
    assert_eq!(
        transcript_code_languages(&projected),
        vec!["typescript".to_string()],
        "the rendered transcript must label the executed cell"
    );
    workbench.shutdown().await;
}

/// The `code-failure` scenario renders a failed cell, recovers and
/// terminates. With the workbench's turn budget a scenario that cannot
/// commit re-asks the provider until the budget ends, so the watchdog is the
/// regression guard: a scenario that cannot terminate fails here.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_code_failure_scenario_renders_a_failed_cell_and_terminates() {
    let workbench =
        Workbench::builder(failure_provider::DevProviderScenario::CodeFailure.provider())
            .build()
            .await;
    let state = &workbench.state;
    tokio::time::timeout(
        Duration::from_secs(60),
        run_turn(state, "run the deterministic code failure"),
    )
    .await
    .expect("the code-failure scenario reaches a terminal state");

    let projected = read_state(state, None)
        .await
        .expect("project the code-failure session");
    let blocks = projected
        .transcript
        .iter()
        .filter(|row| row.kind == crate::ChatRowKind::CodeBlock)
        .map(|row| (row.content.language.clone(), row.content.success))
        .collect::<Vec<_>>();
    assert!(
        blocks
            .iter()
            .any(|block| *block == (Some("typescript".to_string()), Some(false))),
        "the scenario must render a failed cell: {blocks:?}"
    );
    let answers = transcript_answers(&projected);
    assert!(
        answers
            .iter()
            .any(|answer| answer.contains("session recovered after code failure")),
        "the scenario must recover and finish within the turn budget: {answers:?}"
    );
    workbench.shutdown().await;
}
