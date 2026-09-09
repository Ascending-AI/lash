
use crate::dialect::RlmDialect;
use crate::native::prompt::execution_section;
use lash_lashlang_runtime::{LashlangSurface, ToolBinding, ToolDefinitionBindingExt};

fn catalog() -> lash_core::ToolCatalog {
    lash_core::ToolCatalog::from_tool_definitions((0..7).map(|index| {
            lash_core::ToolDefinition::raw(format!("tool:probe{index}"), format!("probe{index}"),
                "Return a STRING containing record-looking text, not a structured record{{type_literal_hint}}.",
                serde_json::json!({"type":"object","properties":{"id":{"type":"string","description":"Record identifier"}},"required":["id"]}),
                serde_json::json!({"type":"string"}))
                .with_tool_binding(ToolBinding::new(["probe"], format!("op{index}")))
        }).collect())
}

fn dialect(typescript: bool, enabled: bool) -> Box<dyn RlmDialect> {
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
    if typescript {
        Box::new(crate::dialect::typescript::TypescriptDialect::prompt_only(
            surface,
        ))
    } else {
        Box::new(crate::dialect::lashlang::LashlangDialect::prompt_only(
            surface,
        ))
    }
}

fn system(dialect: &dyn RlmDialect, native: bool, enabled: bool) -> String {
    let catalog = catalog();
    let features = crate::protocol::RlmPromptFeatures {
        images: enabled,
        type_literals: enabled,
        decomposition: enabled,
    };
    let mut execution = if native {
        execution_section(dialect, features, &catalog)
    } else {
        dialect
            .render_execution_section(features, &catalog)
            .unwrap()
    };
    if !dialect.renders_tool_catalogue_inline() {
        execution.push_str(&format!(
            "\n\n### Tools\n\nAwait these documented operations:\n\n{}",
            crate::tool_catalog::rlm_prompt_tool_docs(&catalog, dialect, features)
        ));
    }
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
    for typescript in [true, false] {
        for native in [false, true] {
            let off = system(dialect(typescript, false).as_ref(), native, false);
            let on = system(dialect(typescript, true).as_ref(), native, true);
            let size = off.chars().count();
            println!(
                "prompt diet typescript={typescript} native={native}: off={size} on={}",
                on.chars().count()
            );
            assert!(
                size <= if typescript { 6000 } else { 7000 },
                "{size}: {off}"
            );
            assert!(on.len() > off.len());
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
    let durable =
        crate::dialect::typescript::typescript_process_prompt(&lashlang::LashlangAbilities::all());
    assert!(durable.chars().count() <= 900, "{}", durable.len());
}

#[test]
fn mode_independent_header_and_guidance_are_identical() {
    let standard = lash_core::PromptTemplate::default().render(&lash_sansio::PromptContext {
        execution_prompt: "Use direct tool calls.".into(),
        ..Default::default()
    });
    for typescript in [false, true] {
        for native in [false, true] {
            let prompt = system(dialect(typescript, false).as_ref(), native, false);
            assert_eq!(prompt.lines().next(), standard.lines().next());
            assert_eq!(
                prompt.split_once("## Guidance").unwrap().1,
                standard.split_once("## Guidance").unwrap().1
            );
        }
    }
}

#[test]
fn dialect_execution_headings_have_a_body_in_both_channels() {
    for (typescript, heading) in [
        (true, "## TypeScript execution"),
        (false, "## Lashlang execution"),
    ] {
        for native in [false, true] {
            let prompt = system(dialect(typescript, false).as_ref(), native, true);
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
                "Call `execute_code` once"
            } else {
                "paired"
            };
            assert!(prompt.split_once(heading).unwrap().1.contains(transport));
        }
    }
}

#[test]
fn durable_primitives_gate_independently() {
    for mask in 0..16 {
        let abilities = lashlang::LashlangAbilities {
            processes: mask & 1 != 0,
            sleep: mask & 2 != 0,
            process_signals: mask & 4 != 0,
            triggers: mask & 8 != 0,
        };
        let text = crate::dialect::typescript::typescript_process_prompt(&abilities);
        for (needle, expected) in [
            ("run", abilities.processes),
            ("defineProcess", abilities.processes),
            ("sleep(", abilities.sleep),
            (
                "waitSignal",
                abilities.processes && abilities.process_signals,
            ),
            ("registerTrigger", abilities.processes && abilities.triggers),
            ("signals?", abilities.processes && abilities.process_signals),
        ] {
            assert_eq!(
                text.contains(needle),
                expected,
                "mask={mask}, {needle}: {text}"
            );
        }
    }
}

#[test]
fn labels_without_processes_render_a_complete_sentence() {
    let surface = LashlangSurface {
        language_features: lashlang::LashlangLanguageFeatures::default().with_label_annotations(),
        ..Default::default()
    };
    let dialect = crate::dialect::lashlang::LashlangDialect::prompt_only(surface);
    let prompt = system(&dialect, false, true);
    let labels = prompt
        .lines()
        .find(|line| line.starts_with("- Execution labels:"))
        .unwrap();
    assert!(labels.contains("At top level, label meaningful setup, resource calls, submissions, branches, and loops."), "{labels}");
    assert!(!labels.contains(",s"), "{labels}");
    assert!(!labels.contains("process"), "{labels}");
}

#[test]
fn toolbench_shaped_prompt_has_no_process_vocabulary() {
    for typescript in [false, true] {
        let surface = LashlangSurface {
            language_features: lashlang::LashlangLanguageFeatures::default()
                .with_label_annotations(),
            ..Default::default()
        };
        let dialect: Box<dyn RlmDialect> = if typescript {
            Box::new(crate::dialect::typescript::TypescriptDialect::prompt_only(
                surface,
            ))
        } else {
            Box::new(crate::dialect::lashlang::LashlangDialect::prompt_only(
                surface,
            ))
        };
        let prompt = system(dialect.as_ref(), false, true);
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
    for dialect in [dialect(false, false), dialect(true, false)] {
        let tool =
            crate::control_tools::continue_as_tool_definition_for(dialect.prompt_vocabulary());
        assert!(tool.manifest().description.chars().count() <= 350);
        let catalog = lash_core::ToolCatalog::from_tool_definitions(vec![tool]);
        assert!(
            crate::tool_catalog::rlm_prompt_tool_docs(
                &catalog,
                dialect.as_ref(),
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
                dialect.as_ref(),
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
