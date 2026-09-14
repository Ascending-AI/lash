use crate::dialect::TypescriptDialect;

use crate::native::prompt::execution_section;
use lash_lashlang_runtime::{LashlangSurface, ToolBinding, ToolDefinitionBindingExt};

fn catalog() -> lash_core::ToolCatalog {
    lash_core::ToolCatalog::from_tool_definitions(catalog_definitions())
}

fn catalog_definitions() -> Vec<lash_core::ToolDefinition> {
    ((0..7).map(|index| {
            lash_core::ToolDefinition::raw(format!("tool:probe{index}"), format!("probe{index}"),
                "Return a STRING containing record-looking text, not a structured record{{type_literal_hint}}.",
                serde_json::json!({"type":"object","properties":{"id":{"type":"string","description":"Record identifier"}},"required":["id"]}),
                serde_json::json!({"type":"string"}))
                .with_tool_binding(ToolBinding::new(["probe"], format!("op{index}")))
        })).collect()
}

/// A catalogue carrying the process control surface.
///
/// FIG-2999: the process authoring block is gated by catalogue presence, not
/// by an ability, so a fixture that wants it declares a `processes.*` tool.
fn process_catalog() -> lash_core::ToolCatalog {
    let mut tools = catalog_definitions();
    tools.push(
        lash_core::ToolDefinition::raw(
            "tool:process-controls/start",
            "processes_start",
            "Start a process",
            serde_json::json!({
                "type": "object",
                "properties": { "definition": { "type": "object" } },
                "required": ["definition"],
                "additionalProperties": false
            }),
            serde_json::json!({
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"],
                "additionalProperties": false
            }),
        )
        .with_tool_binding(ToolBinding::new(["processes"], "start")),
    );
    lash_core::ToolCatalog::from_tool_definitions(tools)
}

fn dialect(enabled: bool) -> TypescriptDialect {
    let surface = LashlangSurface {
        abilities: if enabled {
            lashlang::LashlangAbilities::all()
        } else {
            lashlang::LashlangAbilities::default()
        },
        language_features: if enabled {
            lashlang::LashlangLanguageFeatures::default().with_label_annotations()
        } else {
            lashlang::LashlangLanguageFeatures::default()
        },
        ..LashlangSurface::default()
    };
    crate::dialect::TypescriptDialect::prompt_only(surface)
}

fn system(dialect: &TypescriptDialect, native: bool, enabled: bool) -> String {
    system_with(dialect, native, enabled, catalog())
}

fn system_with(
    dialect: &TypescriptDialect,
    native: bool,
    enabled: bool,
    catalog: lash_core::ToolCatalog,
) -> String {
    let features = crate::protocol::RlmPromptFeatures {
        images: enabled,
        type_literals: enabled,
        decomposition: enabled,
    };
    let execution = if native {
        execution_section(dialect, features, &catalog)
    } else {
        dialect
            .render_execution_section(features, &catalog)
            .unwrap()
    };
    let prompt = lash_core::PromptTemplate::default().render(&lash_sansio::PromptContext {
        execution_prompt: execution.into(),
        ..Default::default()
    });
    crate::execution_prompt::render_system_prompt(&prompt, dialect)
        .unwrap()
        .to_string()
}

#[test]
fn prompt_diet_sizes_and_capability_gates() {
    for native in [false, true] {
        {
            let off = system(&dialect(false), native, false);
            let on = system(&dialect(true), native, true);
            let size = off.chars().count();
            println!(
                "prompt diet native={native}: off={size} on={}",
                on.chars().count()
            );
            assert!(size <= 3100, "{size}: {off}");
            assert!(on.len() > off.len());
            assert!(on.chars().count() <= 7200);
            for forbidden in [
                "defineProcess",
                "waitSignal",
                "registerTrigger",
                "Background processes",
                "wait_signal",
                "signal_run",
                "Type { ... }",
                "### Type literals",
                "@label",
                "Image",
                "continuation tool",
            ] {
                assert!(!off.contains(forbidden), "disabled {forbidden}: {off}");
            }
            assert_eq!(off.matches("### Tools\n").count(), 1);
            assert_eq!(off.matches("Return a STRING").count(), 7);
            assert!(!off.contains("### Host Surface"));
        }
    }
    let durable = crate::dialect::typescript::typescript_process_prompt(true);
    // Raised from 900 with FIG-2986, by the smallest amount the ruled change
    // forces: `inputs?: (event: unknown) => Record<string, unknown>` is 21
    // characters longer than `inputs: Record<string, unknown>`, and the prose
    // has to say the arrow is erased or a model writes logic inside it, which
    // costs 13 more. 900 + 21 + 13 = 934, the prompt's exact length, so this is
    // as tight a ratchet as the old one was.
    assert!(durable.chars().count() <= 935, "{}", durable.len());
}

#[test]
fn mode_independent_header_and_guidance_are_identical() {
    let standard = lash_core::PromptTemplate::default().render(&lash_sansio::PromptContext {
        execution_prompt: "Use direct tool calls.".into(),
        ..Default::default()
    });
    for native in [false, true] {
        {
            let prompt = system(&dialect(false), native, false);
            assert_eq!(prompt.lines().next(), standard.lines().next());
            assert_eq!(
                prompt.split_once("## Guidance").unwrap().1,
                standard.split_once("## Guidance").unwrap().1
            );
        }
    }
}

#[test]
fn execution_heading_has_a_body_in_both_channels() {
    {
        let heading = "## TypeScript execution";
        for native in [false, true] {
            let prompt = system(&dialect(false), native, true);
            assert_eq!(prompt.lines().filter(|line| *line == heading).count(), 1);
            assert!(!prompt.lines().any(|line| line == "## Execution"));
            let lines: Vec<_> = prompt.lines().filter(|line| !line.is_empty()).collect();
            for pair in lines.windows(2) {
                assert!(
                    !(pair[0].starts_with('#') && pair[1].starts_with('#')),
                    "bodiless heading: {} before {}",
                    pair[0],
                    pair[1]
                );
            }
            let transport = if native {
                "Each response makes one `execute_code` call"
            } else {
                "standalone"
            };
            assert!(prompt.split_once(heading).unwrap().1.contains(transport));
        }
    }
}

#[test]
fn the_process_block_follows_the_catalogue_and_sleep_follows_its_ability() {
    // FIG-2999: `processes`, `process_signals` and `triggers` are gone as
    // abilities. The authoring block rides on the rendered catalogue, and
    // `sleep` is the one remaining engine ability.
    for process_surface in [false, true] {
        let text = crate::dialect::typescript::typescript_process_prompt(process_surface);
        for needle in [
            "async",
            "waitSignal",
            "Captures are by value",
            "A started handle outlives the turn",
        ] {
            assert_eq!(text.contains(needle), process_surface, "{needle}: {text}");
        }
        for retired in ["defineProcess", "registerTrigger", "signals?", "wake("] {
            assert!(!text.contains(retired), "{retired}: {text}");
        }
    }

    for sleep in [false, true] {
        let surface = LashlangSurface {
            abilities: lashlang::LashlangAbilities { sleep },
            ..LashlangSurface::default()
        };
        let dialect = crate::dialect::TypescriptDialect::prompt_only(surface);
        let text = dialect
            .render_execution_section(crate::protocol::RlmPromptFeatures::default(), &catalog())
            .unwrap();
        assert_eq!(
            text.contains("`await sleep(ms)` pauses the program."),
            sleep,
            "{text}"
        );
    }
}

#[test]
fn child_lifecycle_copy_is_present_once_on_every_process_channel() {
    for native in [false, true] {
        {
            let prompt = system_with(&dialect(true), native, true, process_catalog());
            for fact in [
                "A started handle outlives the turn",
                "Stop cancels only the awaited handle",
                "cancel is a request the child sees at its next step or wake",
            ] {
                assert_eq!(
                    prompt.matches(fact).count(),
                    1,
                    "native={native}, fact={fact}: {prompt}"
                );
            }
        }
    }

    for native in [false, true] {
        {
            let prompt = system(&dialect(false), native, false);
            assert!(!prompt.contains("A started handle outlives the turn"));
        }
    }
}

#[test]
fn toolbench_shaped_prompt_has_no_process_vocabulary() {
    {
        let surface = LashlangSurface {
            language_features: lashlang::LashlangLanguageFeatures::default()
                .with_label_annotations(),
            ..Default::default()
        };
        let dialect = crate::dialect::TypescriptDialect::prompt_only(surface);
        let prompt = system(&dialect, false, true);
        for forbidden in [",s.", "process", "defineProcess", "waitSignal"] {
            assert!(!prompt.contains(forbidden), "{forbidden}: {prompt}");
        }
        assert!(
            !prompt
                .split(|c: char| !c.is_alphanumeric() && c != '_')
                .any(|word| word == "run")
        );
        if let Ok(directory) = std::env::var("LASH_PROMPT_CAPTURE_DIR") {
            std::fs::write(
                std::path::Path::new(&directory)
                    .join(format!("fig2750-fix1-{}.txt", dialect.language_id())),
                prompt,
            )
            .unwrap();
        }
    }
}

#[test]
fn continuation_docs_are_short_and_gated() {
    {
        let dialect = dialect(false);
        let tool =
            crate::control_tools::continue_as_tool_definition_for(dialect.prompt_vocabulary());
        assert!(tool.manifest().description.chars().count() <= 350);
        let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);
        assert!(
            crate::tool_catalog::rlm_prompt_tool_docs(
                &catalog,
                &dialect,
                crate::protocol::RlmPromptFeatures {
                    decomposition: false,
                    ..Default::default()
                }
            )
            .is_empty()
        );
        assert!(
            crate::tool_catalog::rlm_prompt_tool_docs(
                &catalog,
                &dialect,
                crate::protocol::RlmPromptFeatures::default()
            )
            .contains("Terminal action")
        );
    }
}

#[test]
fn removed_guardrails_still_have_repair_hints() {
    let host = lashlang::LashlangHostEnvironment::new(
        lashlang::LashlangHostCatalog::default(),
        lashlang::LashlangAbilities::all(),
    );
    for (source, repair) in [
        ("class A {}", "Use functions and plain objects"),
        (
            "function* g() { yield 1; }",
            "build the whole list and return it",
        ),
        (
            "await Promise.race([1, 2]);",
            "Promise.all/Promise.allSettled",
        ),
    ] {
        let error = lash_typescript::link(source, &host)
            .expect_err("unsupported construct")
            .to_string();
        let hint = error.split_once("hint:").expect("repair hint").1;
        assert!(hint.contains(repair), "{source}: {error}");
    }
}

#[test]
fn prompt_section_order_and_termination_have_one_owner() {
    for native in [false, true] {
        {
            let dialect = dialect(false);
            let prompt = system(&dialect, native, false);
            let headings = prompt
                .lines()
                .filter(|line| line.starts_with('#') && !line.starts_with("### `await "))
                .collect::<Vec<_>>();
            let transport = if native {
                "### Tool transport"
            } else {
                "### Response shape"
            };
            let expected = vec![
                "## TypeScript execution",
                transport,
                if native {
                    "### Example execute_code call"
                } else {
                    "### Example cell"
                },
                "### Host API",
                "### Tools",
                "## Guidance",
            ];
            assert_eq!(headings, expected, "native={native}");
            assert_eq!(
                prompt.matches("only those listed under **Tools**").count(),
                1
            );
            assert_eq!(prompt.matches("ends the turn").count(), 1);
            for termination in [
                lash_rlm_types::RlmTermination::Natural,
                lash_rlm_types::RlmTermination::FinishRequired { schema: None },
            ] {
                let policy = dialect.finalization_copy(&termination);
                assert!(!policy.contains("standalone"));
                assert!(!policy.contains("only those listed"));
                assert!(!policy.contains("Return exactly"));
            }
        }
    }
}

#[test]
fn each_host_capability_gates_its_own_vocabulary() {
    // TypeScript is the only RLM surface, so what the execution section gates
    // is the host ability set. Images, type literals and label annotations were
    // syntax features of the retired surface with no TypeScript spelling: they
    // contribute no vocabulary to gate any more (FIG-3021). Decomposition gates
    // the continuation tool's own docs, which
    // `continuation_docs_are_short_and_gated` covers against a catalogue that
    // actually carries that tool. FIG-2999 leaves `sleep` as the only ability;
    // the process vocabulary is gated by the catalogue instead, so it is
    // exercised here on the same axis.
    for native in [false, true] {
        for capability in 0..2 {
            for enabled in [false, true] {
                let features = crate::protocol::RlmPromptFeatures {
                    images: false,
                    type_literals: false,
                    decomposition: false,
                };
                let mut surface = LashlangSurface::default();
                let mut catalog = catalog();
                let needles: &[&str] = match capability {
                    0 => {
                        surface.abilities.sleep = enabled;
                        &["`await sleep(ms)` pauses the program."]
                    }
                    _ => {
                        if enabled {
                            catalog = process_catalog();
                        }
                        &[
                            "### Processes",
                            "Captures are by value",
                            "A started handle outlives the turn",
                        ]
                    }
                };
                let dialect = crate::dialect::TypescriptDialect::prompt_only(surface);
                let text = if native {
                    execution_section(&dialect, features, &catalog)
                } else {
                    dialect
                        .render_execution_section(features, &catalog)
                        .unwrap()
                };
                for needle in needles {
                    assert_eq!(
                        text.contains(needle),
                        enabled,
                        "capability={capability} needle={needle}: {text}"
                    );
                }
            }
        }
    }
}

#[test]
fn tool_signatures_cover_every_operation_parameter_and_return_shape() {
    use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};
    let catalog = lash_core::ToolCatalog::from_tool_definitions(["first", "second"].map(|operation| lash_core::ToolDefinition::raw(operation, operation, format!("Description for {operation}"), serde_json::json!({"type":"object","properties":{"required_id":{"type":"string"},"optional_limit":{"type":"integer"}},"required":["required_id"],"additionalProperties":false}), serde_json::json!({"type":"object","properties":{"answer":{"type":"string"}},"required":["answer"],"additionalProperties":false})).with_tool_binding(ToolBinding::new(["lookup"], operation))).to_vec());
    {
        let dialect = dialect(false);
        let docs =
            crate::tool_catalog::rlm_prompt_tool_docs(&catalog, &dialect, Default::default());
        for operation in ["first", "second"] {
            assert!(docs.contains(&format!("Description for {operation}")));
            assert!(
                docs.contains(&format!("{operation}(input:"))
                    || docs.contains(&format!("lookup.{operation}({{"))
            );
        }
        for needle in ["required_id", "optional_limit", "answer"] {
            assert_eq!(docs.matches(needle).count(), 2, "{docs}");
        }
        assert!(!docs.contains("Parameters:"));
        assert!(!docs.contains("Return fields:"));
        assert!(!docs.contains("declare"), "{docs}");
        assert_eq!(
            docs.lines()
                .filter(|line| line.starts_with("`lookup."))
                .count(),
            2
        );
        assert!(docs.contains("optional_limit?: number"));
    }
}

#[test]
fn typescript_capabilities_gate_in_both_assembled_channels() {
    for native in [false, true] {
        for sleep in [false, true] {
            for process_surface in [false, true] {
                let dialect = crate::dialect::TypescriptDialect::prompt_only(LashlangSurface {
                    abilities: lashlang::LashlangAbilities { sleep },
                    ..Default::default()
                });
                let catalog = if process_surface {
                    process_catalog()
                } else {
                    catalog()
                };
                let prompt = system_with(&dialect, native, false, catalog);
                for (needle, enabled) in [
                    ("### Processes", process_surface),
                    ("Captures are by value", process_surface),
                    ("waitSignal", process_surface),
                    ("await sleep(ms)", sleep),
                ] {
                    assert_eq!(
                        prompt.contains(needle),
                        enabled,
                        "sleep={sleep}, process_surface={process_surface}, native={native}, {needle}"
                    );
                }
                // These retired surface syntaxes must never enter TypeScript
                // copy, and neither may the deleted special forms (FIG-2999).
                for needle in [
                    "@label",
                    "Type {",
                    "### Type literals",
                    "sleep for",
                    "wait_signal",
                    "defineProcess",
                    "registerTrigger",
                ] {
                    assert!(!prompt.contains(needle), "{needle}: {prompt}");
                }
            }
        }
    }
}

#[test]
fn wrapup_nested_return_rows_and_plain_signatures() {
    use lash_lashlang_runtime::{ToolBinding, ToolDefinitionBindingExt};
    let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![lash_core::ToolDefinition::raw("get", "get", "Read a nested record.", serde_json::json!({"type":"object","properties":{},"additionalProperties":false}), serde_json::json!({"type":"object","properties":{"outer":{"type":"object","properties":{"inner":{"type":"string"}},"required":["inner"]}},"required":["outer"]})).with_tool_binding(ToolBinding::new(["kv"], "get"))]);
    {
        let dialect = dialect(false);
        let docs =
            crate::tool_catalog::rlm_prompt_tool_docs(&catalog, &dialect, Default::default());
        assert!(docs.contains("Return fields:"), "{docs}");
        assert!(docs.contains("outer.inner"), "{docs}");
        assert_eq!(docs.matches("Read a nested record.").count(), 1);
        assert!(docs.starts_with("`kv.get({}): Promise<"), "{docs}");
        for forbidden in [
            "declare",
            "namespace",
            "const ",
            "function ",
            "/**",
            "await ",
            " -> ",
        ] {
            assert!(!docs.contains(forbidden), "{forbidden}: {docs}");
        }
    }
}

#[test]
fn opening_line_names_exactly_the_available_sections() {
    {
        for host_surface in [false, true] {
            let mut surface = LashlangSurface::default();
            if host_surface {
                surface
                    .resources
                    .add_module_operation(
                        ["host"],
                        "host",
                        "read",
                        "read",
                        lashlang::TypeExpr::Str,
                        lashlang::TypeExpr::Str,
                    )
                    .unwrap();
            }
            let dialect = crate::dialect::TypescriptDialect::prompt_only(surface);
            let text = dialect
                .render_execution_section(Default::default(), &catalog())
                .unwrap();
            let expected = if host_surface {
                "Use prose for conversation; use a paired `<typescript>` block for action or computation. Call tools as `await module.operation({ ... })`, only those listed under **Tools** or **Host Surface**."
            } else {
                "Use prose for conversation; use a paired `<typescript>` block for action or computation. Call tools as `await module.operation({ ... })`, only those listed under **Tools**."
            };
            assert_eq!(text.lines().next(), Some(expected));
            assert_eq!(text.contains("### Host Surface"), host_surface);
        }
    }
}

#[test]
fn print_finish_has_one_short_verification_cue() {
    let text = system(&dialect(false), false, false);
    // The retired surface carried the cue in its own `print` vs `finish`
    // section; TypeScript states it once inside the host API paragraph.
    assert_eq!(
        text.matches("do not finish an unexamined whole tool result.")
            .count(),
        1
    );
}
