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
