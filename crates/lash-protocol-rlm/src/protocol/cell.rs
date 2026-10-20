//! Finalize / non-streaming executable-cell handling. All paired-tag grammar
//! lives in [`crate::cell_scan`]; this module only layers the
//! extraction-and-projection conveniences the driver needs.
//!
//! The prose before a cell is what the reply says to its reader. Prose made
//! only of markup tag lines says nothing: a model that reaches for its own
//! tool-call syntax leaks lines like `<invoke_placeholder>` there (FIG-6888).
//! Such prose is no prose, so it is never committed as an assistant entry,
//! shown to hosts, or replayed to the model as a pattern to repeat.

pub(crate) use crate::cell_scan::malformed_cell_fence;
use crate::cell_scan::{complete_start_tag_span, first_cell_span};
use crate::dialect::CellTags;

pub(super) struct CellExtraction {
    pub(super) prose: String,
    pub(super) code: String,
    pub(super) cell_count: usize,
    pub(super) discarded_after_cell: Option<crate::cell_scan::DiscardedAfterCell>,
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
        .map(|start| prose_before_cell(&text[..start]))
        .unwrap_or_else(|| text.to_string())
}

/// The prose a cell's reply wrote before its start tag: nothing when it is
/// only markup tag lines.
fn prose_before_cell(text: &str) -> String {
    let prose = text.trim_end();
    if prose.lines().all(|line| {
        let line = line.trim();
        line.is_empty() || is_markup_tag(line)
    }) {
        String::new()
    } else {
        prose.to_string()
    }
}

/// Whether `line` is exactly one opening, closing or self-closing markup tag,
/// such as `<invoke_placeholder>`, `</function_calls>` or `<invoke name="x">`.
fn is_markup_tag(line: &str) -> bool {
    let Some(inner) = line
        .strip_prefix('<')
        .and_then(|rest| rest.strip_suffix('>'))
    else {
        return false;
    };
    if inner.contains(['<', '>']) {
        return false;
    }
    let inner = inner.strip_prefix('/').unwrap_or(inner);
    let inner = inner.strip_suffix('/').unwrap_or(inner);
    let name_end = inner
        .find(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | ':')))
        .unwrap_or(inner.len());
    let (name, attributes) = inner.split_at(name_end);
    name.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
        && (attributes.is_empty() || attributes.starts_with(char::is_whitespace))
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
        prose: prose_before_cell(&text[..span.start_tag_start]),
        code,
        cell_count: 1,
        discarded_after_cell: crate::cell_scan::discarded_tail(&text[span.end_tag_end..]),
    }))
}
