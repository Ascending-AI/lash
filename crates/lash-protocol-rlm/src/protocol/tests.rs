use super::cell::{extract_cell, project_visible_assistant_prose_with_tags};
use super::*;

fn tags() -> crate::dialect::CellTags {
    crate::dialect::CellTags {
        open: "<typescript>",
        close: "</typescript>",
    }
}

fn extract_typescript_cell(
    text: &str,
) -> Result<Option<cell::CellExtraction>, CellExtractionError> {
    extract_cell(text, tags())
}

#[test]
fn cell_extraction_rejects_a_started_but_unclosed_block() {
    assert!(matches!(
        extract_typescript_cell("<typescript>\nfinish(1);"),
        Err(super::cell::CellExtractionError::UnclosedCell)
    ));
}

#[test]
fn rendered_history_cell_round_trips_through_extractor() {
    // History == emission: the cell text the history renderer emits for a prior
    // step (`render_cell_text`) extracts back to the exact prose + code
    // via the same grammar the protocol uses, and carries none of the
    // `--- history[...] ---` meta-format the model could imitate (the regression
    // for the observed glm-5.2 history echo).
    let code = "const loc = run();\nprint(loc);";
    let cell = crate::cell_scan::render_cell_text(tags(), "Found it.", code);
    assert!(!cell.contains("--- history["));
    assert!(!cell.contains("\nCode:\n"));
    let extraction = extract_typescript_cell(&cell)
        .expect("valid cell")
        .expect("renders a valid cell");
    assert_eq!(extraction.prose, "Found it.");
    assert_eq!(extraction.code, code);
}

#[test]
fn prose_of_only_markup_tag_lines_is_no_prose_but_prose_naming_a_tag_is_kept() {
    let prose = |text: &str| {
        let reply = format!("{text}\n<typescript>\nfinish(1);\n</typescript>");
        let cell = extract_typescript_cell(&reply)
            .ok()
            .flatten()
            .map(|cell| cell.prose);
        let visible = project_visible_assistant_prose_with_tags(&reply, tags());
        assert_eq!(cell.as_deref(), Some(visible.as_str()), "{text:?}");
        visible
    };
    assert_eq!(prose("<invoke_placeholder>"), "");
    assert_eq!(
        prose("<function_calls>\n<invoke name=\"tools.find\">\n</invoke>\n</function_calls>"),
        ""
    );
    assert_eq!(
        prose("Calling <invoke_placeholder> now."),
        "Calling <invoke_placeholder> now."
    );
    assert_eq!(
        prose("Searching.\n<invoke_placeholder>"),
        "Searching.\n<invoke_placeholder>"
    );
}
