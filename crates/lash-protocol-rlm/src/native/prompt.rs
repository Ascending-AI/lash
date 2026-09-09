use crate::dialect::RlmDialect;
use lash_rlm_types::RlmTermination;

/// Only transport prose changes. Runtime, language and standard library copy
/// comes verbatim from the dialect's authoritative teaching.
pub(crate) fn execution_section(
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
        text.replace_range(start..end, "### Tool transport\n\nEach response makes one `execute_code` call with `{\"code\": \"<complete program>\"}`. Tool calls and `finish` run inside the program; prose before the call is commentary.\n");
    }
    text = text
        .replace(
            "a paired `<typescript>` block",
            "the `execute_code` program",
        )
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
        .replace("in a block", "in an `execute_code` call")
        .replace(
            "response's block calls",
            "response's `execute_code` call runs",
        )
        .replace("the block", "the `execute_code` call")
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
