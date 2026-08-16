//! Single source of truth for the RLM paired executable-cell grammar.
//!
//! A model response may contain visible assistant prose followed by a
//! line whose trimmed content is exactly `<lashlang>`, then Lashlang
//! source, then a later line whose trimmed content is exactly
//! `</lashlang>`. Markdown code blocks, inline tag mentions, and retired
//! `%%lashlang` markers are plain text.

use crate::dialect::CellTags;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct StartTagSpan {
    /// Byte offset of the first byte of the start tag line.
    pub(crate) start_tag_start: usize,
    /// Byte offset immediately after the start tag line terminator.
    pub(crate) body_start: usize,
    /// Byte offset at the currently buffered text end.
    pub(crate) body_end: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct CellSpan {
    /// Byte offset of the first byte of the start tag line.
    pub(crate) start_tag_start: usize,
    /// Byte offset immediately after the start tag line terminator.
    pub(crate) body_start: usize,
    /// Byte offset immediately after the executable source. This excludes
    /// the line terminator that separates the last source line from the
    /// closing tag.
    pub(crate) body_end: usize,
    /// Byte offset of the first byte of the closing tag line.
    pub(crate) end_tag_start: usize,
    /// Byte offset immediately after the closing tag line terminator, or
    /// EOF if the closing tag is the final line.
    pub(crate) end_tag_end: usize,
}

/// Locate the first complete executable block for `tags` in `text`, if any.
pub(crate) fn first_cell_span(text: &str, tags: CellTags) -> Option<CellSpan> {
    let mut pos = 0;
    while pos <= text.len() {
        let line = logical_line(text, pos);
        if line.text.trim() == tags.open && line.has_terminator {
            let body_start = line.next_pos;
            if let Some(end) = first_end_tag_after(text, body_start, true, tags) {
                return Some(CellSpan {
                    start_tag_start: pos,
                    body_start,
                    body_end: source_body_end(text, body_start, end.line_start),
                    end_tag_start: end.line_start,
                    end_tag_end: end.next_pos,
                });
            }
        }
        if line.has_terminator {
            if line.next_pos <= pos {
                break;
            }
            pos = line.next_pos;
        } else {
            break;
        }
    }
    None
}

/// Render one canonical paired lashlang block. `prose` is trimmed at the
/// end, and `code` is emitted as the exact executable source between the
/// tags.
pub(crate) fn render_cell_text(tags: CellTags, prose: &str, code: &str) -> String {
    let prose = prose.trim_end();
    let mut rendered = String::new();
    if !prose.is_empty() {
        rendered.push_str(prose);
        rendered.push('\n');
    }
    rendered.push_str(tags.open);
    rendered.push('\n');
    rendered.push_str(code);
    rendered.push('\n');
    rendered.push_str(tags.close);
    rendered
}

/// Returns the byte length of a suffix that could still become a start tag
/// line once more streamed text arrives.
pub(crate) fn possible_start_tag_suffix_len(text: &str, tags: CellTags) -> usize {
    text.char_indices()
        .filter_map(|(idx, _)| {
            if idx > 0 && !text[..idx].ends_with('\n') {
                return None;
            }
            let suffix = &text[idx..];
            let trimmed = suffix.trim_start_matches([' ', '\t']);
            if trimmed.is_empty() {
                return Some(suffix.len());
            }
            tags.open.starts_with(trimmed).then_some(suffix.len())
        })
        .next()
        .unwrap_or(0)
}

/// Locate a start tag only after its line has completed. Streaming uses
/// this to avoid suppressing an incomplete line like `<lashlang> here`
/// before the model has emitted the rest of the line.
pub(crate) fn complete_start_tag_span(text: &str, tags: CellTags) -> Option<StartTagSpan> {
    let mut pos = 0;
    while pos < text.len() {
        let line = logical_line(text, pos);
        if !line.has_terminator {
            return None;
        }
        if line.text.trim() == tags.open {
            return Some(StartTagSpan {
                start_tag_start: pos,
                body_start: line.next_pos,
                body_end: text.len(),
            });
        }
        pos = line.next_pos;
    }
    None
}

/// Locate a closing tag line inside a cell body. `allow_eof` is reserved for
/// genuine response EOF; an ordinary provider chunk boundary is not EOF.
pub(crate) fn complete_end_tag_span(
    text: &str,
    allow_eof: bool,
    tags: CellTags,
) -> Option<CellSpan> {
    first_end_tag_after(text, 0, allow_eof, tags).map(|end| CellSpan {
        start_tag_start: 0,
        body_start: 0,
        body_end: source_body_end(text, 0, end.line_start),
        end_tag_start: end.line_start,
        end_tag_end: end.next_pos,
    })
}

#[derive(Debug, Clone, Copy)]
struct LogicalLine<'a> {
    text: &'a str,
    next_pos: usize,
    has_terminator: bool,
}

#[derive(Debug, Clone, Copy)]
struct EndTagLine {
    line_start: usize,
    next_pos: usize,
}

fn logical_line(text: &str, pos: usize) -> LogicalLine<'_> {
    let line_end = text[pos..]
        .find('\n')
        .map(|rel| pos + rel)
        .unwrap_or(text.len());
    LogicalLine {
        text: &text[pos..line_end],
        next_pos: if line_end < text.len() {
            line_end + 1
        } else {
            line_end
        },
        has_terminator: line_end < text.len(),
    }
}

fn first_end_tag_after(
    text: &str,
    start: usize,
    allow_eof: bool,
    tags: CellTags,
) -> Option<EndTagLine> {
    let mut pos = start;
    while pos <= text.len() {
        let line = logical_line(text, pos);
        if line.text.trim() == tags.close && (line.has_terminator || allow_eof) {
            return Some(EndTagLine {
                line_start: pos,
                next_pos: line.next_pos,
            });
        }
        if !line.has_terminator {
            break;
        }
        pos = line.next_pos;
    }
    None
}

fn source_body_end(text: &str, body_start: usize, end_tag_start: usize) -> usize {
    if end_tag_start <= body_start {
        return end_tag_start;
    }
    if text[..end_tag_start].ends_with("\r\n") {
        end_tag_start - 2
    } else if text[..end_tag_start].ends_with('\n') {
        end_tag_start - 1
    } else {
        end_tag_start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags() -> CellTags {
        CellTags {
            open: "<lashlang>",
            close: "</lashlang>",
        }
    }

    fn first_lashlang_cell_span(text: &str) -> Option<CellSpan> {
        first_cell_span(text, tags())
    }

    fn render_lashlang_cell_text(prose: &str, code: &str) -> String {
        render_cell_text(tags(), prose, code)
    }

    #[test]
    fn scanner_and_renderer_use_the_supplied_dialect_tags() {
        let tags = CellTags {
            open: "<typescript>",
            close: "</typescript>",
        };
        let rendered = render_cell_text(tags, "Checking.", "finish(1)");
        assert_eq!(
            rendered,
            "Checking.\n<typescript>\nfinish(1)\n</typescript>"
        );
        let span = first_cell_span(&rendered, tags).expect("custom dialect cell");
        assert_eq!(&rendered[span.body_start..span.body_end], "finish(1)");
        assert!(first_cell_span(&rendered, self::tags()).is_none());
    }

    fn code(text: &str) -> Option<&str> {
        let span = first_lashlang_cell_span(text)?;
        Some(&text[span.body_start..span.body_end])
    }

    #[test]
    fn prose_only_has_no_cell() {
        assert!(first_lashlang_cell_span("plain prose").is_none());
    }

    #[test]
    fn inline_tag_mentions_are_prose() {
        assert!(first_lashlang_cell_span("Use <lashlang> here.").is_none());
        assert!(first_lashlang_cell_span("Use </lashlang> here.").is_none());
    }

    #[test]
    fn start_only_incomplete_block_has_no_cell() {
        assert!(first_lashlang_cell_span("<lashlang>\nfinish 1").is_none());
    }

    #[test]
    fn end_without_start_has_no_cell() {
        assert!(first_lashlang_cell_span("</lashlang>\nfinish 1").is_none());
    }

    #[test]
    fn empty_block_extracts_empty_source() {
        assert_eq!(code("<lashlang>\n</lashlang>"), Some(""));
    }

    #[test]
    fn prose_plus_complete_block_extracts_code_and_prose_offsets() {
        let text = "Before\n\n<lashlang>\nprint 1\nfinish 2\n</lashlang>";
        let span = first_lashlang_cell_span(text).expect("complete block");
        assert_eq!(&text[..span.start_tag_start].trim_end(), &"Before");
        assert_eq!(&text[span.body_start..span.body_end], "print 1\nfinish 2");
        assert_eq!(&text[span.end_tag_start..span.end_tag_end], "</lashlang>");
    }

    #[test]
    fn indented_tag_lines_parse() {
        assert_eq!(
            code("Before\n  <lashlang>  \nfinish 1\n  </lashlang>  \nAfter"),
            Some("finish 1")
        );
    }

    #[test]
    fn markdown_fences_inside_block_are_source() {
        assert_eq!(
            code(
                "<lashlang>\npayload = r\"\"\"```markdown\nbody\n```\"\"\"\nfinish payload\n</lashlang>"
            ),
            Some("payload = r\"\"\"```markdown\nbody\n```\"\"\"\nfinish payload")
        );
    }

    #[test]
    fn raw_triple_strings_containing_backticks_are_source() {
        assert_eq!(
            code("<lashlang>\npayload = r\"\"\"```\nvalue\n```\"\"\"\nfinish payload\n</lashlang>"),
            Some("payload = r\"\"\"```\nvalue\n```\"\"\"\nfinish payload")
        );
    }

    #[test]
    fn suffix_text_after_end_tag_is_ignored_by_span() {
        let text = "<lashlang>\nfinish 1\n</lashlang>\nTrailing prose.";
        let span = first_lashlang_cell_span(text).expect("complete block");
        assert_eq!(&text[span.body_start..span.body_end], "finish 1");
        assert_eq!(&text[span.end_tag_end..], "Trailing prose.");
    }

    #[test]
    fn second_block_is_ignored_after_first_close() {
        let text = "<lashlang>\nfinish 1\n</lashlang>\n<lashlang>\nfinish 2\n</lashlang>";
        assert_eq!(code(text), Some("finish 1"));
    }

    #[test]
    fn retired_percent_marker_is_plain_prose() {
        assert!(first_lashlang_cell_span("%%lashlang\nfinish 1").is_none());
    }

    #[test]
    fn canonical_renderer_round_trips_empty_code() {
        let rendered = render_lashlang_cell_text("", "");
        let span = first_lashlang_cell_span(&rendered).expect("complete block");
        assert_eq!(&rendered[span.body_start..span.body_end], "");
    }
}
