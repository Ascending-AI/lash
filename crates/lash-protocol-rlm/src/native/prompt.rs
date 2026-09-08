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
        .replace(
            "from inside a paired `<lashlang>` block",
            "from inside the `execute_code` program",
        )
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
            "from inside a paired `<lashlang>` block",
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
