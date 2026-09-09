use crate::dialect::RlmDialect;
use lash_rlm_types::RlmTermination;

/// Only transport prose changes. Runtime, language and standard library copy
/// comes verbatim from the dialect's authoritative teaching.
pub(super) fn execution_section(
    dialect: &dyn RlmDialect,
    features: crate::protocol::RlmPromptFeatures,
    catalog: &lash_core::ToolCatalog,
) -> String {
    let original = dialect
        .render_execution_section(features, catalog)
        .expect("validated dialect catalog");
    let mut text = original;
    if let Some(start) = text.find("### Response shape") {
        let end = text[start + 4..]
            .find("\n### ")
            .map(|offset| start + 4 + offset)
            .unwrap_or(text.len());
        text.replace_range(start..end, "### Tool transport\n\nCall `execute_code` once with a JSON object containing only the string `code`. Put the complete program in `code`. Host operations and `finish` run inside that program.\n");
    }
    text = text
        .replace("a paired `<lashlang>` block", "the `execute_code` program")
        .replace("across `<lashlang>` blocks", "across programs");
    if let Some(start) = text.find("### Example cell")
        && let Some(close) = text[start..].find(dialect.cell_tags().close)
    {
        let end = start + close + dialect.cell_tags().close.len();
        let example = transport_copy(&text[start..end], dialect)
            .replace("### Example cell", "### Example execute_code call");
        text.replace_range(start..end, &example);
    }
    // Other worked examples retain their existing language teaching.
    text.lines()
        .filter(|line| {
            !["<lashlang>", "</lashlang>", "<typescript>", "</typescript>"].contains(&line.trim())
        })
        .collect::<Vec<_>>()
        .join("\n")
}

pub(super) fn finalization(dialect: &dyn RlmDialect, termination: &RlmTermination) -> String {
    transport_copy(&dialect.finalization_copy(termination), dialect)
}

/// Preserve the dialect's finish and workflow teaching while replacing the
/// response transport vocabulary and the wrappers of worked examples.
pub(super) fn transport_copy(original: &str, dialect: &dyn RlmDialect) -> String {
    let tags = dialect.cell_tags();
    let pair = format!("`{}...{}`", tags.open, tags.close);
    let mut text = original
        .replace(&format!("paired {pair} block"), "`execute_code` call")
        .replace(&format!("`{}` block", tags.open), "`execute_code` call")
        .replace("Lashlang block", "`execute_code` call")
        .replace("TypeScript block", "`execute_code` call")
        .replace("a <lashlang> block", "an `execute_code` call")
        .replace("the block", "the code call")
        .replace("no block", "no code call")
        .replace("A block without", "A program without")
        .replace(
            "inside a `execute_code` call",
            "inside the `code` argument of an `execute_code` call",
        )
        .replace("a `execute_code`", "an `execute_code`");
    let mut lines = Vec::new();
    let mut program: Option<Vec<&str>> = None;
    for line in text.lines() {
        if line.trim() == tags.open {
            program = Some(Vec::new());
        } else if line.trim() == tags.close {
            if let Some(program) = program.take() {
                lines.push(format!(
                    "execute_code({})",
                    serde_json::json!({"code":program.join("\n")})
                ));
            }
        } else if let Some(program) = &mut program {
            program.push(line);
        } else {
            lines.push(line.to_string());
        }
    }
    text = lines.join("\n");
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transport_rewrite_preserves_finish_teaching_and_runtime_sections() {
        let dialect = crate::dialect::LashlangDialect::prompt_only(
            lash_lashlang_runtime::LashlangSurface::default(),
        );
        let native = finalization(&dialect, &RlmTermination::FinishRequired { schema: None });
        assert!(native.contains("Use `finish null` only when null is intentional."));
        assert!(!native.contains("<lashlang>"));
        let catalog = lash_core::ToolCatalog::default();
        let features = crate::protocol::RlmPromptFeatures::default();
        let cell = dialect
            .render_execution_section(features, &catalog)
            .unwrap();
        let native = execution_section(&dialect, features, &catalog);
        let teaching = cell
            .split("### `print` vs `finish`")
            .nth(1)
            .unwrap()
            .split("### Response shape")
            .next()
            .unwrap();
        assert!(native.contains(teaching));
    }
}

#[cfg(test)]
mod drift_tests {
    use super::*;
    #[test]
    fn native_prompts_pin_both_dialects_and_replacement_needles() {
        let lashlang = crate::dialect::LashlangDialect::prompt_only(
            lash_lashlang_runtime::LashlangSurface::default(),
        );
        let typescript = crate::dialect::typescript_test_dialect();
        let mut corpus = String::new();
        for dialect in [&lashlang as &dyn RlmDialect, &typescript] {
            let catalog = lash_core::ToolCatalog::default();
            let features = crate::protocol::RlmPromptFeatures::default();
            let execution = dialect
                .render_execution_section(features, &catalog)
                .unwrap();
            assert!(execution.contains(&crate::dialect::cell_response_shape(
                dialect.cell_tags(),
                dialect.prompt_vocabulary()
            )));
            corpus.push_str(&execution);
            let mut native = execution_section(dialect, features, &catalog);
            assert!(!native.contains("### Response shape"));
            assert!(!native.contains("Example cell"));
            if dialect.language_id() == "typescript" {
                assert!(
                    native.contains(
                        r#"execute_code({"code":"const total = 1 + 2;\nfinish(total);"})"#
                    )
                );
            }
            assert!(!native.contains("Markdown code fences"));
            assert!(!native.contains(dialect.cell_tags().open));
            assert!(!native.contains(dialect.cell_tags().close));
            for termination in [
                RlmTermination::Natural,
                RlmTermination::FinishRequired { schema: None },
            ] {
                corpus.push_str(&dialect.finalization_copy(&termination));
                native.push_str(&finalization(dialect, &termination));
            }
            corpus.push_str(&dialect.turn_limit_final_copy(4));
            corpus.push_str(&dialect.output_limit_cell_copy(None));
            corpus.push_str(&dialect.finish_required_copy(false));
            corpus.push_str(&dialect.finish_required_copy(true));
            insta::assert_snapshot!(format!("native_prompt_{}", dialect.language_id()), native);
        }
        for needle in [
            "### Response shape",
            "across `<lashlang>` blocks",
            "paired `<lashlang>...</lashlang>` block",
            "paired `<typescript>...</typescript>` block",
            "`<lashlang>` block",
            "`<typescript>` block",
            "Lashlang block",
            "TypeScript block",
            "a <lashlang> block",
            "the block",
            "no block",
            "A block without",
        ] {
            assert!(
                corpus.contains(needle),
                "native prompt rewrite needle disappeared: {needle}"
            );
        }
        // These two needles are introduced by the preceding replacements.
        let intermediate = "inside a paired `<lashlang>...</lashlang>` block".replace(
            "paired `<lashlang>...</lashlang>` block",
            "`execute_code` call",
        );
        assert!(intermediate.contains("inside a `execute_code` call"));
        assert!(intermediate.contains("a `execute_code`"));
    }
}

#[cfg(test)]
mod prompt_diet_tests {
    use super::*;
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
        let durable = crate::dialect::typescript::typescript_process_prompt(
            &lashlang::LashlangAbilities::all(),
        );
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
            language_features: lashlang::LashlangLanguageFeatures::default()
                .with_label_annotations(),
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
            if !typescript {
                assert!(
                    !prompt
                        .split(|c: char| !c.is_alphanumeric() && c != '_')
                        .any(|word| word == "run")
                );
            }
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
}
