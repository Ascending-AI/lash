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
    text = text.replace("Write one script inside standalone `<typescript>` and `</typescript>` lines.", "Call `execute_code` once with the complete TypeScript script in the required string argument `code`.")
        .replace("from inside a paired `<lashlang>` block", "from inside the `execute_code` program")
        .replace("across `<lashlang>` blocks", "across programs");
    // Fences in worked examples describe transport, not executable language.
    text = text
        .lines()
        .filter(|line| {
            !["<lashlang>", "</lashlang>", "<typescript>", "</typescript>"].contains(&line.trim())
        })
        .collect::<Vec<_>>()
        .join("\n");
    text
}

pub(super) fn finalization(dialect: &dyn RlmDialect, termination: &RlmTermination) -> String {
    transport_copy(dialect.finalization_copy(termination), dialect)
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
