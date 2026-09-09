use super::cell::extract_cell;
use super::*;

fn tags() -> crate::dialect::CellTags {
    crate::dialect::CellTags {
        open: "<lashlang>",
        close: "</lashlang>",
    }
}

fn extract_lashlang_cell(text: &str) -> Result<Option<cell::CellExtraction>, CellExtractionError> {
    extract_cell(text, tags())
}

fn project_visible_assistant_prose(text: &str) -> String {
    cell::project_visible_assistant_prose_with_tags(text, tags())
}

fn prompt_lashlang_samples(prompt: &str) -> Vec<String> {
    let mut samples = Vec::new();
    let mut current = None::<String>;
    for line in prompt.lines() {
        match line.trim() {
            "<lashlang>" => {
                assert!(current.is_none(), "nested lashlang prompt sample");
                current = Some(String::new());
            }
            "</lashlang>" => {
                samples.push(current.take().expect("closing tag without opening tag"));
            }
            _ => {
                if let Some(sample) = &mut current {
                    sample.push_str(line);
                    sample.push('\n');
                }
            }
        }
    }
    assert!(current.is_none(), "unclosed lashlang prompt sample");
    samples
}

#[test]
fn every_rendered_prompt_sample_parses_and_links() {
    let surface = full_prompt_host_environment();
    for features in [
        RlmPromptFeatures::default(),
        RlmPromptFeatures {
            images: false,
            ..RlmPromptFeatures::default()
        },
    ] {
        let prompt = rlm_execution_section_for_host_environment(features, &surface);
        let samples = prompt_lashlang_samples(&prompt);
        assert!(
            !samples.is_empty(),
            "prompt must contain executable samples"
        );
        for (index, sample) in samples.iter().enumerate() {
            let program = lashlang::parse(sample).unwrap_or_else(|error| {
                panic!("prompt sample {index} did not parse: {error}\n{sample}")
            });
            lashlang::LinkedModule::link(program, surface.clone()).unwrap_or_else(|error| {
                panic!("prompt sample {index} did not link: {error}\n{sample}")
            });
        }
    }
}

#[test]
fn rendered_builtin_inventory_matches_enabled_runtime_registry() {
    for type_literals in [false, true] {
        let prompt = rlm_execution_section_for_host_environment(
            RlmPromptFeatures {
                type_literals,
                ..RlmPromptFeatures::default()
            },
            &full_prompt_host_environment(),
        );
        let builtins = prompt
            .split_once("### Builtins")
            .unwrap()
            .1
            .split("### ")
            .next()
            .unwrap();
        let names = builtins
            .split('`')
            .enumerate()
            .filter(|(i, _)| i % 2 == 1)
            .map(|(_, code)| code.split('(').next().unwrap())
            .collect::<std::collections::BTreeSet<_>>();
        for name in lashlang::builtin_names() {
            assert_eq!(
                names.contains(name),
                name != "validate" || type_literals,
                "{name}"
            );
        }
    }
}

fn prompt_host_environment(
    resources: lashlang::LashlangHostCatalog,
    abilities: lashlang::LashlangAbilities,
) -> lashlang::LashlangHostEnvironment {
    lashlang::LashlangHostEnvironment::new(resources, abilities)
}

fn prompt_host_environment_with_features(
    resources: lashlang::LashlangHostCatalog,
    abilities: lashlang::LashlangAbilities,
    language_features: lashlang::LashlangLanguageFeatures,
) -> lashlang::LashlangHostEnvironment {
    lashlang::LashlangHostEnvironment::new(resources, abilities)
        .with_language_features(language_features)
}

fn tool_resources() -> lashlang::LashlangHostCatalog {
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation(
            ["web"],
            "Web",
            "search",
            "search_web",
            lashlang::TypeExpr::Any,
            lashlang::TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["web"],
            "Web",
            "fetch",
            "fetch_url",
            lashlang::TypeExpr::Any,
            lashlang::TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["files"],
            "Files",
            "read",
            "read_file",
            lashlang::TypeExpr::Any,
            lashlang::TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
        .add_module_operation(
            ["module"],
            "Module",
            "operation",
            "module_operation",
            lashlang::TypeExpr::Any,
            lashlang::TypeExpr::Any,
        )
        .expect("host catalog operation must not conflict");
    resources
}

fn full_prompt_host_environment() -> lashlang::LashlangHostEnvironment {
    // Synthetic prompt surfaces deliberately model only known host contracts;
    // do not widen them with `any` escape hatches just to make samples link.
    prompt_host_environment(tool_resources(), lashlang::LashlangAbilities::all())
        .with_globals(["record"])
}

#[test]
fn execution_section_hides_processes_when_disabled() {
    let surface = prompt_host_environment(tool_resources(), lashlang::LashlangAbilities::default());
    let section =
        rlm_execution_section_for_host_environment(RlmPromptFeatures::default(), &surface);

    assert!(!section.contains("process name"));
    assert!(!section.contains("start name"));
    assert!(!section.contains("sleep for"));
    assert!(!section.contains("wait_signal"));
    assert!(!section.contains("signal_run"));
}

#[test]
fn execution_section_makes_paired_lashlang_tag_contract_explicit() {
    let section = rlm_execution_section_for_host_environment(
        RlmPromptFeatures::default(),
        &full_prompt_host_environment(),
    );

    assert!(section.contains("Put one program after any commentary"));
    assert!(section.contains("standalone `<lashlang>` and `</lashlang>` lines"));
    assert!(section.contains("even inside a multiline string"));
    assert!(section.contains("Markdown fences do not execute"));
}

#[test]
fn execution_section_claims_the_operator_ladder_and_new_builtin_semantics() {
    let section = rlm_execution_section_for_host_environment(
        RlmPromptFeatures::default(),
        &full_prompt_host_environment(),
    );
    assert!(section.contains("postfix calls/fields/indexing/result `?`"));
    assert!(section.contains("comparisons `== != < <= > >= in`"));
    assert!(section.contains("`sort(list)`"));
    assert!(section.contains("`unique(list)`"));
    assert!(section.contains("`replace(s, from, to)`"));
    assert!(section.contains("`min`, `max` (empty errors)"));
    assert!(section.contains("`sum(list)` (`sum([]) = 0`)"));
}

#[test]
fn execution_section_hides_label_annotations_when_disabled() {
    let section = rlm_execution_section_for_host_environment(
        RlmPromptFeatures::default(),
        &full_prompt_host_environment(),
    );

    assert!(!section.contains("@label"));
}

#[test]
fn execution_section_documents_static_label_annotations_when_enabled() {
    let surface = prompt_host_environment_with_features(
        tool_resources(),
        lashlang::LashlangAbilities::all(),
        lashlang::LashlangLanguageFeatures::default().with_label_annotations(),
    );
    let section =
        rlm_execution_section_for_host_environment(RlmPromptFeatures::default(), &surface);

    assert!(section.contains("@label(title: \"…\")"));
    assert!(section.contains("goes on the line before the one top-level statement"));
    assert!(section.contains("String literals only; never standalone or stacked"));
}

#[test]
fn execution_section_hides_sleep_and_signals_independently() {
    let surface = prompt_host_environment(
        tool_resources(),
        lashlang::LashlangAbilities::default().with_processes(),
    );
    let section =
        rlm_execution_section_for_host_environment(RlmPromptFeatures::default(), &surface);

    assert!(section.contains("process name"));
    assert!(!section.contains("sleep for"));
    assert!(!section.contains("wait_signal"));
    assert!(!section.contains("signal_run"));
}

#[test]
fn execution_section_distinguishes_foreground_finish_from_process_finish() {
    let surface = prompt_host_environment(
        tool_resources(),
        lashlang::LashlangAbilities::default().with_processes(),
    );
    let section =
        rlm_execution_section_for_host_environment(RlmPromptFeatures::default(), &surface);

    assert!(section.contains("`finish value` ends the turn"));
    assert!(section.contains("`finish value` / `fail value` complete the run"));
    assert!(!section.contains("cell-only"));
}

#[test]
fn execution_section_documents_foreground_signal_run_when_enabled() {
    let surface = prompt_host_environment(
        tool_resources(),
        lashlang::LashlangAbilities::default()
            .with_processes()
            .with_process_signals(),
    );
    let section =
        rlm_execution_section_for_host_environment(RlmPromptFeatures::default(), &surface);

    // Sending (`signal_run`) is documented as foreground-legal; receiving
    // (`wait_signal`) stays process-only.
    assert!(section.contains("signal_run(h, \"approve\", { ok: true })"));
    assert!(section.contains("sends from foreground or process code"));
    assert!(section.contains("wait_signal(\"approve\")"));
    assert!(section.contains("`wait_signal` is process-only"));
}

#[test]
fn execution_section_documents_unwrapped_process_await_for_finished_values() {
    let surface = prompt_host_environment(
        tool_resources(),
        lashlang::LashlangAbilities::default().with_processes(),
    );
    let section =
        rlm_execution_section_for_host_environment(RlmPromptFeatures::default(), &surface);

    assert!(section.contains("`(await h)?` unwraps the `{ ok, value }` wrapper"));
    assert!(section.contains("`results = await [h1, h2]`"));
    assert!(!section.contains("terminal result returned by `await handle`"));
}

#[test]
fn execution_section_shows_sleep_without_processes() {
    let surface = prompt_host_environment(
        tool_resources(),
        lashlang::LashlangAbilities::default().with_sleep(),
    );
    let section =
        rlm_execution_section_for_host_environment(RlmPromptFeatures::default(), &surface);

    assert!(section.contains("sleep for"));
    assert!(!section.contains("process name"));
}

#[test]
fn execution_section_hides_trigger_registry_language_when_disabled() {
    let surface = prompt_host_environment(
        tool_resources(),
        lashlang::LashlangAbilities::default()
            .with_processes()
            .with_sleep()
            .with_process_signals(),
    );
    let section =
        rlm_execution_section_for_host_environment(RlmPromptFeatures::default(), &surface);

    assert!(!section.contains("Trigger registry"));
    assert!(!section.contains("matching trigger occurrences"));
}

#[test]
fn execution_section_hides_trigger_registry_language_without_processes() {
    let surface = prompt_host_environment(
        tool_resources(),
        lashlang::LashlangAbilities::default().with_triggers(),
    );
    let section =
        rlm_execution_section_for_host_environment(RlmPromptFeatures::default(), &surface);

    assert!(!section.contains("Trigger registry"));
    assert!(!section.contains("triggers.register"));
}

#[test]
fn execution_section_lists_typed_operations_constructors_and_trigger_sources() {
    let mut resources = lashlang::LashlangHostCatalog::new();
    lashlang::add_trigger_resource_operations(&mut resources)
        .expect("trigger resource operations are unique");
    resources
        .add_trigger_source_constructor(
            ["timer", "Schedule"],
            lashlang::TypeExpr::Object(vec![
                lashlang::TypeField {
                    name: "expr".into(),
                    ty: lashlang::TypeExpr::Str,
                    optional: false,
                },
                lashlang::TypeField {
                    name: "tz".into(),
                    ty: lashlang::TypeExpr::Str,
                    optional: true,
                },
            ]),
            lashlang::NamedDataType::object(
                "timer.Tick",
                vec![lashlang::TypeField {
                    name: "fired_at".into(),
                    ty: lashlang::TypeExpr::Str,
                    optional: false,
                }],
            )
            .expect("valid timer tick type"),
        )
        .expect("valid timer trigger source");
    let surface = prompt_host_environment(
        resources,
        lashlang::LashlangAbilities::default()
            .with_processes()
            .with_triggers(),
    );

    let section =
        rlm_execution_section_for_host_environment(RlmPromptFeatures::default(), &surface);

    assert!(section.contains("### Host Surface"));
    assert!(section.contains("`await triggers.register("));
    assert!(section.contains("inputs: dict"));
    assert!(section.contains("name: str?"));
    assert!(section.contains("`type timer.Tick = { fired_at: str }`"));
    assert!(
        section.contains("`timer.Schedule({ expr: str, tz: str? }) -> TriggerSource<timer.Tick>`")
    );
    assert!(
        section.contains(
            "`timer.Schedule` can be passed to `triggers.register` and emits `timer.Tick`"
        )
    );
}

#[test]
fn execution_section_hides_module_examples_without_module_operations() {
    let surface = prompt_host_environment(
        lashlang::LashlangHostCatalog::new(),
        lashlang::LashlangAbilities::default(),
    );
    let section =
        rlm_execution_section_for_host_environment(RlmPromptFeatures::default(), &surface);

    assert!(!section.contains("await tools."));
    assert!(!section.contains("**Tools**"));
    assert!(!section.contains("Module operations"));
    assert!(section.contains("No module operations are available"));
}

#[test]
fn execution_section_does_not_advertise_unregistered_peer_capability() {
    let section = rlm_execution_section_for_host_environment(
        RlmPromptFeatures::default(),
        &full_prompt_host_environment(),
    );

    assert!(!section.contains("capability: \"peer\""));
    assert!(!section.contains("`peer`"));
}

#[test]
fn execution_section_keeps_tool_specific_examples_out_of_core_prompt() {
    let default_section = rlm_execution_section_for_host_environment(
        RlmPromptFeatures::default(),
        &full_prompt_host_environment(),
    );
    let label_surface = prompt_host_environment_with_features(
        tool_resources(),
        lashlang::LashlangAbilities::all(),
        lashlang::LashlangLanguageFeatures::default().with_label_annotations(),
    );
    let label_section =
        rlm_execution_section_for_host_environment(RlmPromptFeatures::default(), &label_surface);

    for tool_name in [
        "read_file",
        "exec_command",
        "files.edit",
        "files.glob",
        "files.write",
        "llm_query",
        "spawn_agent",
        "continue_as",
        "list_process_handles",
    ] {
        for section in [&default_section, &label_section] {
            assert!(
                !section.contains(tool_name),
                "core RLM prompt should not mention tool-specific example `{tool_name}`"
            );
        }
    }
    assert!(!default_section.contains("shell.exec"));
    assert!(!default_section.contains("exit_code"));
    assert!(!default_section.contains("full_output_path"));
    assert!(!default_section.contains("nonzero exit"));
}

#[test]
fn execution_section_can_disable_image_guidance() {
    let section = rlm_execution_section_for_host_environment(
        RlmPromptFeatures {
            images: false,
            ..RlmPromptFeatures::default()
        },
        &full_prompt_host_environment(),
    );

    assert!(!section.contains("Image"));
    assert!(!section.contains("image.size"));
    assert!(section.contains("### Language"));
    assert!(section.contains("### Builtins"));
    assert!(section.contains("### Type literals"));
}

#[test]
fn execution_section_mentions_while_and_bounded_loop_guidance() {
    let section = rlm_execution_section_for_host_environment(
        RlmPromptFeatures::default(),
        &full_prompt_host_environment(),
    );

    assert!(
        section.contains("Statements: `if cond { … }`, `for x in xs { … }`, `while cond { … }`")
    );
    assert!(section.contains("prefer bounded loops"));
}

#[test]
fn execution_section_documents_list_comprehensions() {
    let section = rlm_execution_section_for_host_environment(
        RlmPromptFeatures::default(),
        &full_prompt_host_environment(),
    );

    assert!(section.contains("[expr for x in xs if cond]"));
    assert!(section.contains("multiple for/if clauses execute left-to-right"));
    assert!(section.contains("Bindings are local"));
    assert!(!section.contains("Do not use comprehensions"));
}

#[test]
fn cell_extraction_returns_none_for_prose_only() {
    assert!(
        extract_lashlang_cell("plain prose")
            .expect("valid prose-only response")
            .is_none()
    );
    assert_eq!(
        project_visible_assistant_prose("plain prose"),
        "plain prose"
    );
    assert!(!contains_lashlang_cell("plain prose"));
}

#[test]
fn cell_extraction_rejects_a_started_but_unclosed_block() {
    assert!(matches!(
        extract_lashlang_cell("<lashlang>\nfinish 1"),
        Err(super::cell::CellExtractionError::UnclosedCell)
    ));
}

#[test]
fn cell_extraction_leaves_non_cell_markup_as_prose() {
    for text in [
        "<lashlang>",
        "</lashlang>\nfinish 1",
        "%%lashlang\nfinish 1",
    ] {
        assert!(
            extract_lashlang_cell(text)
                .expect("non-cell markup is not an extraction error")
                .is_none(),
            "non-cell markup should not parse: {text:?}"
        );
        assert!(!contains_lashlang_cell(text));
    }
}

#[test]
fn cell_extraction_uses_prose_before_start_tag_and_code_before_end_tag() {
    let text = "Before\n\n<lashlang>\nprint 1\nfinish 2\n</lashlang>\n  \n";
    let extraction = extract_lashlang_cell(text)
        .expect("valid cell")
        .expect("should extract");
    assert_eq!(extraction.prose, "Before");
    assert_eq!(extraction.code, "print 1\nfinish 2");
    assert_eq!(project_visible_assistant_prose(text), "Before");
}

#[test]
fn cell_extraction_accepts_indented_tag_lines() {
    let text = "Before\n  <lashlang>  \nfinish 1\n  </lashlang>  \n";
    let extraction = extract_lashlang_cell(text)
        .expect("valid cell")
        .expect("should extract");
    assert_eq!(extraction.prose, "Before");
    assert_eq!(extraction.code, "finish 1");
}

#[test]
fn inline_tag_text_is_plain_prose() {
    let text = "Use <lashlang> in documentation.";
    assert!(
        extract_lashlang_cell(text)
            .expect("valid prose-only response")
            .is_none()
    );
    assert_eq!(project_visible_assistant_prose(text), text);
}

#[test]
fn markdown_code_blocks_before_tags_remain_visible_prose() {
    let text = "Example:\n```python\nprint('x')\n```\n<lashlang>\nfinish 1\n</lashlang>";
    let extraction = extract_lashlang_cell(text)
        .expect("valid cell")
        .expect("should extract paired block");
    assert_eq!(extraction.code, "finish 1");
    assert_eq!(extraction.prose, "Example:\n```python\nprint('x')\n```");
    assert_eq!(
        project_visible_assistant_prose(text),
        "Example:\n```python\nprint('x')\n```"
    );
}

#[test]
fn markdown_code_blocks_inside_tags_are_lashlang_source() {
    let text =
        "<lashlang>\npayload = r\"\"\"```markdown\nbody\n```\"\"\"\nfinish payload\n</lashlang>";
    let extraction = extract_lashlang_cell(text)
        .expect("valid cell")
        .expect("should extract");
    assert_eq!(
        extraction.code,
        "payload = r\"\"\"```markdown\nbody\n```\"\"\"\nfinish payload"
    );
}

#[test]
fn rendered_history_cell_round_trips_through_extractor() {
    // History == emission: the cell text the history renderer emits for a prior
    // step (`render_lashlang_cell_text`) extracts back to the exact prose + code
    // via the same grammar the protocol uses, and carries none of the
    // `--- history[...] ---` meta-format the model could imitate (the regression
    // for the observed glm-5.2 history echo).
    let code = "loc = run()\nprint(loc)";
    let cell = crate::cell_scan::render_cell_text(tags(), "Found it.", code);
    assert!(!cell.contains("--- history["));
    assert!(!cell.contains("\nCode:\n"));
    let extraction = extract_lashlang_cell(&cell)
        .expect("valid cell")
        .expect("renders a valid cell");
    assert_eq!(extraction.prose, "Found it.");
    assert_eq!(extraction.code, code);
}

#[test]
fn cell_extraction_silently_drops_trailing_text_after_cell() {
    let text = "Before\n<lashlang>\nfinish 1\n</lashlang>\nafter";
    let extraction = extract_lashlang_cell(text)
        .expect("trailing prose is ignored")
        .expect("cell extracts");
    assert_eq!(extraction.prose, "Before");
    assert_eq!(extraction.code, "finish 1");
}

#[test]
fn standalone_close_tag_line_inside_multiline_source_is_the_cell_boundary() {
    let text = concat!(
        "<lashlang>\n",
        "payload = \"\"\"\n",
        "</lashlang>\n",
        "this text is outside the cell\n",
        "\"\"\"\n",
        "finish payload\n",
        "</lashlang>",
    );
    let extraction = extract_lashlang_cell(text)
        .expect("the first standalone closing-tag line owns the boundary")
        .expect("cell extracts");

    assert_eq!(extraction.code, "payload = \"\"\"");
}

#[test]
fn cell_extraction_accepts_only_the_first_of_multiple_cells() {
    let text = "<lashlang>\nprint 1\n</lashlang>\n<lashlang>\nfinish 2\n</lashlang>";
    let extraction = extract_lashlang_cell(text)
        .expect("suffix is discarded")
        .expect("first cell extracts");
    assert_eq!(extraction.code, "print 1");
}
