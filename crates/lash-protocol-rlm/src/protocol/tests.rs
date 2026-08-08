use super::cell::extract_lashlang_cell;
use super::*;

#[test]
fn rlm_execution_section_default_prompt_is_golden() {
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(
            "rlm_execution_section_default",
            rlm_execution_section_for_host_environment(RlmPromptFeatures::default(), &full_prompt_host_environment())
        );
    });
}

#[test]
fn rlm_execution_section_no_images_prompt_is_golden() {
    insta::with_settings!({ snapshot_path => "../snapshots" }, {
        insta::assert_snapshot!(
            "rlm_execution_section_no_images",
            rlm_execution_section_for_host_environment(
                RlmPromptFeatures {
                    images: false,
                    ..RlmPromptFeatures::default()
                },
                &full_prompt_host_environment()
            )
        );
    });
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
fn rendered_builtin_inventory_exactly_matches_runtime_registry() {
    for features in [
        RlmPromptFeatures::default(),
        RlmPromptFeatures {
            images: false,
            ..RlmPromptFeatures::default()
        },
    ] {
        let prompt =
            rlm_execution_section_for_host_environment(features, &full_prompt_host_environment());
        let line = prompt
            .lines()
            .find(|line| {
                line.starts_with("Available builtins (generated from the runtime registry):")
            })
            .expect("generated builtin inventory line");
        let rendered = line
            .split_once(": ")
            .expect("builtin inventory separator")
            .1
            .trim_end_matches('.')
            .split(", ")
            .map(|name| name.trim_matches('`'))
            .collect::<Vec<_>>();
        assert_eq!(rendered, lashlang::builtin_names().collect::<Vec<_>>());
    }
}

#[test]
fn rendered_builtin_bullets_are_bidirectionally_complete() {
    let registry = lashlang::builtin_names().collect::<std::collections::BTreeSet<_>>();
    for features in [
        RlmPromptFeatures::default(),
        RlmPromptFeatures {
            images: false,
            ..RlmPromptFeatures::default()
        },
    ] {
        let prompt =
            rlm_execution_section_for_host_environment(features, &full_prompt_host_environment());
        let builtins = prompt
            .split_once("### Builtins")
            .expect("builtins section")
            .1
            .split_once("### Working with context")
            .expect("section after builtins")
            .0;
        let mut counts = std::collections::BTreeMap::<&str, usize>::new();
        for line in builtins.lines().filter(|line| line.starts_with("- `")) {
            let lead = line
                .strip_prefix("- `")
                .and_then(|line| line.split_once('`'))
                .map(|(signature, _)| signature.split('(').next().expect("signature name"))
                .expect("builtin bullet lead");
            assert!(
                registry.contains(lead),
                "prompt bullet lead `{lead}` is absent from the runtime registry"
            );
            *counts.entry(lead).or_default() += 1;
        }
        for name in &registry {
            assert_eq!(
                counts.get(name),
                Some(&1),
                "builtin `{name}` must have exactly one prompt bullet"
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
    resources.add_module_operation(
        ["web"],
        "Web",
        "search",
        "search_web",
        lashlang::TypeExpr::Any,
        lashlang::TypeExpr::Any,
    );
    resources.add_module_operation(
        ["web"],
        "Web",
        "fetch",
        "fetch_url",
        lashlang::TypeExpr::Any,
        lashlang::TypeExpr::Any,
    );
    resources.add_module_operation(
        ["files"],
        "Files",
        "read",
        "read_file",
        lashlang::TypeExpr::Any,
        lashlang::TypeExpr::Any,
    );
    resources.add_module_operation(
        ["module"],
        "Module",
        "operation",
        "module_operation",
        lashlang::TypeExpr::Any,
        lashlang::TypeExpr::Any,
    );
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

    assert!(section.contains("Use plain prose only for direct conversational replies"));
    assert!(
        section
            .contains("Executable code must be inside paired `<lashlang>` and `</lashlang>` tags")
    );
    assert!(section.contains("tag lines must be standalone after trimming"));
    assert!(
        section.contains("When action is needed, place the Lashlang block after any visible prose")
    );
    assert!(!section.contains("exactly one Lashlang block"));
    assert!(!section.contains("NEVER have multiple `<lashlang>` blocks"));
    assert!(!section.contains("Any text after it is ignored"));
    assert!(
        section.contains("Only `finish` once you have observed and verified the relevant results")
    );
    assert!(!section.contains("### Persistence"));
    assert!(!section.contains("Every message before the final answer"));
    assert!(!section.contains("Prose-only does not end the turn"));
}

#[test]
fn execution_section_claims_the_operator_ladder_and_new_builtin_semantics() {
    let section = rlm_execution_section_for_host_environment(
        RlmPromptFeatures::default(),
        &full_prompt_host_environment(),
    );
    assert!(section.contains("postfix calls/fields/indexing/result `?`"));
    assert!(section.contains("comparisons `== != < <= > >= in`"));
    assert!(section.contains("`sort(list)` — stable ascending sort"));
    assert!(section.contains(
        "`unique(list)` — stable typed-equality deduplication that keeps first occurrences"
    ));
    assert!(section.contains("`replace(s, from, to)` — literal Rust-style text replacement"));
    assert!(section.contains("`min(list)` — least item from a non-empty list"));
    assert!(section.contains("`sum(list)` — numeric total; `sum([])` is `0`"));
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

    assert!(section.contains("@label(title: \"Label\")"));
    assert!(section.contains("@label(title: \"Label\", description: \"Details\")"));
    assert!(section.contains("Execution labels"));
    assert!(section.contains("prefix annotation, not a standalone statement"));
    assert!(section.contains("must appear immediately before the one statement"));
    assert!(section.contains("Do not emit `@label(...)` by itself"));
    assert!(section.contains("paths = await files.glob"));
    assert!(section.contains("important Lashlang phases"));
    assert!(section.contains("At top level, label meaningful setup"));
    assert!(section.contains("string literals"));
    assert!(!section.contains("process-map"));
    assert!(!section.contains("visual process statement"));
    assert!(!section.contains("color:"));
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
    assert!(section.contains("signal_run(handle, \"name\", payload)"));
    assert!(section.contains("foreground turn as well as inside a process body"));
    assert!(section.contains("wait_signal(\"name\")"));
    assert!(section.contains("only valid inside a process body"));
}

#[test]
fn execution_section_documents_unwrapped_process_await_for_finished_values() {
    let surface = prompt_host_environment(
        tool_resources(),
        lashlang::LashlangAbilities::default().with_processes(),
    );
    let section =
        rlm_execution_section_for_host_environment(RlmPromptFeatures::default(), &surface);

    assert!(section.contains("`await handle` waits and returns a result wrapper"));
    assert!(section.contains("`result = (await handle)?`"));
    assert!(section.contains("then read `result.field`"));
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
    lashlang::add_trigger_resource_operations(&mut resources);
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
    let section = rlm_execution_section_for_host_environment(
        RlmPromptFeatures::default(),
        &full_prompt_host_environment(),
    );

    for tool_name in [
        "read_file",
        "exec_command",
        "files.edit",
        "files.write",
        "llm_query",
        "spawn_agent",
        "continue_as",
        "list_process_handles",
    ] {
        assert!(
            !section.contains(tool_name),
            "core RLM prompt should not mention tool-specific example `{tool_name}`"
        );
    }
    assert!(!section.contains("shell.exec"));
    assert!(!section.contains("exit_code"));
    assert!(!section.contains("full_output_path"));
    assert!(!section.contains("nonzero exit"));
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

    assert!(section.contains("statement `if`/`for`/`while`"));
    assert!(section.contains("Prefer bounded `while` loops where possible"));
}

#[test]
fn execution_section_documents_list_comprehensions() {
    let section = rlm_execution_section_for_host_environment(
        RlmPromptFeatures::default(),
        &full_prompt_host_environment(),
    );

    assert!(section.contains("[expr for name in iterable]"));
    assert!(section.contains("multiple `for`/`if` clauses run left-to-right like Python"));
    assert!(section.contains("Comprehension bindings are local"));
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
fn cell_extraction_requires_complete_paired_block() {
    for text in [
        "<lashlang>",
        "<lashlang>\nfinish 1",
        "</lashlang>\nfinish 1",
        "%%lashlang\nfinish 1",
    ] {
        assert!(
            extract_lashlang_cell(text)
                .expect("incomplete or retired form is not an extraction error")
                .is_none(),
            "incomplete or retired form should not parse: {text:?}"
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
    let cell = crate::cell_scan::render_lashlang_cell_text("Found it.", code);
    assert!(!cell.contains("--- history["));
    assert!(!cell.contains("\nCode:\n"));
    let extraction = extract_lashlang_cell(&cell)
        .expect("valid cell")
        .expect("renders a valid cell");
    assert_eq!(extraction.prose, "Found it.");
    assert_eq!(extraction.code, code);
}

#[test]
fn cell_extraction_rejects_trailing_text_after_cell() {
    let text = "Before\n<lashlang>\nfinish 1\n</lashlang>\nafter";
    assert!(matches!(
        extract_lashlang_cell(text),
        Err(super::cell::CellExtractionError::TrailingText)
    ));
}

#[test]
fn cell_extraction_rejects_multiple_cells() {
    let text = "<lashlang>\nprint 1\n</lashlang>\n<lashlang>\nfinish 2\n</lashlang>";
    assert!(matches!(
        extract_lashlang_cell(text),
        Err(super::cell::CellExtractionError::MultipleCells)
    ));
}
