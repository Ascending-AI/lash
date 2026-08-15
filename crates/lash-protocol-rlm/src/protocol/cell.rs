//! Finalize / non-streaming executable-cell handling. All paired-tag grammar
//! lives in [`crate::cell_scan`]; this module only layers the
//! extraction-and-projection conveniences the driver needs.

use crate::cell_scan::{complete_start_tag_span, first_cell_span};
use crate::dialect::CellTags;

pub(super) struct CellExtraction {
    pub(super) prose: String,
    pub(super) code: String,
    pub(super) lashlang_cell_count: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CellExtractionError {
    UnclosedCell,
}

const LASHLANG_TAGS: CellTags = CellTags {
    open: "<lashlang>",
    close: "</lashlang>",
};

const TYPESCRIPT_TAGS: CellTags = CellTags {
    open: "<typescript>",
    close: "</typescript>",
};

/// Every registered dialect's tag set, so extraction can *recognize* a cell it
/// must not execute.
///
/// A scanner that knows only the active dialect's tags reads a foreign cell as
/// prose. That is not a cosmetic miss: with `FinishRequired` the driver asks
/// the model to finish, the model answers with the cell it was told to write,
/// and the turn re-prompts forever — the execution fence never fires because
/// extraction never yields a cell to fence. The list is asserted against the
/// dialect registry, so a third dialect cannot be forgotten here.
const REGISTERED_CELL_TAGS: &[(&str, CellTags)] = &[
    (crate::dialect::lashlang::LANGUAGE_ID, LASHLANG_TAGS),
    ("typescript", TYPESCRIPT_TAGS),
];

/// The registered-but-inactive dialect this text writes a cell in, if any.
pub(crate) fn foreign_dialect_cell(
    text: &str,
    active: CellTags,
) -> Option<(&'static str, CellTags)> {
    REGISTERED_CELL_TAGS
        .iter()
        .find(|(_, tags)| {
            tags.open != active.open
                && (first_cell_span(text, *tags).is_some()
                    || complete_start_tag_span(text, *tags).is_some())
        })
        .map(|(language, tags)| (*language, *tags))
}

pub fn contains_lashlang_cell(text: &str) -> bool {
    first_cell_span(text, LASHLANG_TAGS).is_some()
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
        lashlang_cell_count: 1,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The recognition list must cover every dialect the registry can activate,
    /// or a session running the missing one silently reads foreign cells as
    /// prose again.
    #[test]
    fn every_registered_dialect_is_recognizable() {
        let registered = crate::dialect::registered_language_ids();
        let known = REGISTERED_CELL_TAGS
            .iter()
            .map(|(language, _)| *language)
            .collect::<std::collections::BTreeSet<_>>();
        for language in registered {
            assert!(
                known.contains(language),
                "`{language}` is registered but its cell tags are unknown to extraction"
            );
        }
    }

    #[test]
    fn a_foreign_cell_is_named_and_the_active_one_is_not() {
        let typescript = CellTags {
            open: "<typescript>",
            close: "</typescript>",
        };
        assert_eq!(
            foreign_dialect_cell("<typescript>\nfinish(1);\n</typescript>", LASHLANG_TAGS)
                .map(|(language, _)| language),
            Some("typescript")
        );
        assert_eq!(
            foreign_dialect_cell("<lashlang>\nfinish 1\n</lashlang>", typescript)
                .map(|(language, _)| language),
            Some("lashlang")
        );
        assert_eq!(
            foreign_dialect_cell("<lashlang>\nfinish 1\n</lashlang>", LASHLANG_TAGS),
            None
        );
        // An unclosed foreign cell is still a foreign cell: the model wrote the
        // wrong tag, and saying so beats an unclosed-cell diagnostic about a
        // dialect this session does not run.
        assert_eq!(
            foreign_dialect_cell("<typescript>\nfinish(1);", LASHLANG_TAGS)
                .map(|(language, _)| language),
            Some("typescript")
        );
    }
}
