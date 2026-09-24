use super::cell::extract_cell;
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

fn project_visible_assistant_prose(text: &str) -> String {
    cell::project_visible_assistant_prose_with_tags(text, tags())
}

#[test]
fn cell_extraction_rejects_a_started_but_unclosed_block() {
    assert!(matches!(
        extract_typescript_cell("<typescript>\nfinish(1);"),
        Err(super::cell::CellExtractionError::UnclosedCell)
    ));
}

#[test]
fn cell_extraction_leaves_non_cell_markup_as_prose() {
    for text in [
        "<typescript>",
        "</typescript>\nfinish(1);",
        "%%typescript\nfinish(1);",
    ] {
        assert!(
            extract_typescript_cell(text)
                .expect("non-cell markup is not an extraction error")
                .is_none(),
            "non-cell markup should not parse: {text:?}"
        );
    }
}

#[test]
fn cell_extraction_uses_prose_before_start_tag_and_code_before_end_tag() {
    let text = "Before\n\n<typescript>\nprint(1);\nfinish(2);\n</typescript>\n  \n";
    let extraction = extract_typescript_cell(text)
        .expect("valid cell")
        .expect("should extract");
    assert_eq!(extraction.prose, "Before");
    assert_eq!(extraction.code, "print(1);\nfinish(2);");
    assert_eq!(project_visible_assistant_prose(text), "Before");
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
fn standalone_close_tag_line_inside_multiline_source_is_the_cell_boundary() {
    let text = concat!(
        "<typescript>\n",
        "const payload = `\n",
        "</typescript>\n",
        "this text is outside the cell\n",
        "`;\n",
        "finish(payload);\n",
        "</typescript>",
    );
    let extraction = extract_typescript_cell(text)
        .expect("the first standalone closing-tag line owns the boundary")
        .expect("cell extracts");

    assert_eq!(extraction.code, "const payload = `");
}
