//! Finalize / non-streaming Lashlang cell handling. All paired-tag grammar
//! lives in [`crate::cell_scan`]; this module only layers the
//! extraction-and-projection conveniences the driver needs.

use crate::cell_scan::{complete_lashlang_start_tag_span, first_lashlang_cell_span};

pub(super) struct CellExtraction {
    pub(super) prose: String,
    pub(super) code: String,
    pub(super) lashlang_cell_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CellExtractionError {
    UnclosedCell,
    MultipleCells,
}

impl CellExtractionError {
    pub(super) fn message(self) -> &'static str {
        match self {
            Self::UnclosedCell => {
                "Model response started a `<lashlang>` block but did not close it. Retry with a complete paired block. A line whose trimmed content is exactly `</lashlang>` closes the cell, including inside multiline source text; construct that content without a standalone delimiter line."
            }
            Self::MultipleCells => {
                "Model response contained multiple `<lashlang>...</lashlang>` blocks. The blocks were discarded; reply with exactly one paired block containing all work for this step."
            }
        }
    }
}

pub fn contains_lashlang_cell(text: &str) -> bool {
    first_lashlang_cell_span(text).is_some()
}

pub fn project_visible_assistant_prose(text: &str) -> String {
    let start = first_lashlang_cell_span(text)
        .map(|span| span.start_tag_start)
        .or_else(|| complete_lashlang_start_tag_span(text).map(|span| span.start_tag_start));
    start
        .map(|start| text[..start].trim_end().to_string())
        .unwrap_or_else(|| text.to_string())
}

/// Normalize the assistant text to the single executable-cell boundary.
///
/// Keep exactly one complete cell and discard anything after it.
///
/// The literal delimiter parsed by [`first_lashlang_cell_span`] is the only
/// authority that closes a cell.
pub(super) fn normalize_cell_boundary(text: &str) -> String {
    if let Some(span) = first_lashlang_cell_span(text) {
        let trailing = &text[span.end_tag_end..];
        if first_lashlang_cell_span(trailing).is_some() {
            return text.to_string();
        }
        return text[..span.end_tag_end].to_string();
    }

    text.to_string()
}

pub(super) fn extract_lashlang_cell(
    text: &str,
) -> Result<Option<CellExtraction>, CellExtractionError> {
    let Some(span) = first_lashlang_cell_span(text) else {
        return if complete_lashlang_start_tag_span(text).is_some() {
            Err(CellExtractionError::UnclosedCell)
        } else {
            Ok(None)
        };
    };
    let trailing = &text[span.end_tag_end..];
    if first_lashlang_cell_span(trailing).is_some() {
        return Err(CellExtractionError::MultipleCells);
    }
    let code = text[span.body_start..span.body_end].to_string();
    Ok(Some(CellExtraction {
        prose: text[..span.start_tag_start].trim_end().to_string(),
        code,
        lashlang_cell_count: 1,
    }))
}
