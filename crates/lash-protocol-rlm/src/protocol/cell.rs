//! Finalize / non-streaming executable-cell handling. All paired-tag grammar
//! lives in [`crate::cell_scan`]; this module only layers the
//! extraction-and-projection conveniences the driver needs.

pub(crate) use crate::cell_scan::malformed_cell_fence;
use crate::cell_scan::{complete_start_tag_span, first_cell_span};
use crate::dialect::CellTags;

pub(super) struct CellExtraction {
    pub(super) prose: String,
    pub(super) code: String,
    pub(super) cell_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellExtractionError {
    UnclosedCell,
}

pub(crate) fn project_visible_assistant_prose_with_tags(text: &str, tags: CellTags) -> String {
    let start = first_cell_span(text, tags)
        .map(|span| span.start_tag_start)
        .or_else(|| complete_start_tag_span(text, tags).map(|span| span.start_tag_start));
    start
        .map(|start| text[..start].trim_end().to_string())
        .unwrap_or_else(|| text.to_string())
}

pub(super) fn extract_cell(
    text: &str,
    tags: CellTags,
) -> Result<Option<CellExtraction>, CellExtractionError> {
    let Some(span) = first_cell_span(text, tags) else {
        return if complete_start_tag_span(text, tags).is_some() {
            Err(CellExtractionError::UnclosedCell)
        } else {
            Ok(None)
        };
    };
    let code = text[span.body_start..span.body_end].to_string();
    Ok(Some(CellExtraction {
        prose: text[..span.start_tag_start].trim_end().to_string(),
        code,
        cell_count: 1,
    }))
}
