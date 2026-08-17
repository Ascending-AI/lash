//! Single source of truth for the RLM paired executable-cell grammar.
//!
//! Two shapes are executable cells, and nothing else is.
//!
//! The *block* shape: visible assistant prose, then a line whose trimmed
//! content is exactly `<lashlang>`, then Lashlang source, then a later line
//! whose trimmed content is exactly `</lashlang>`.
//!
//! The *inline* shape: one **complete** line — terminated, or ended by the
//! response itself — whose trimmed content starts with `<lashlang>` and ends
//! with `</lashlang>`, with the source between them. A model that answers a
//! one-statement turn on one line has written something the grammar can read
//! unambiguously — refusing it cost a judged frontier row twelve billed calls
//! for a semantically fine reply (FIG-1475) — so it is read rather than
//! refused.
//!
//! The line must be complete because a streamed prefix of a longer line looks
//! exactly like a finished inline cell: `<lashlang>print 1</lashlang> ok` is
//! prose, and reading it as a cell the instant the closing tag arrives would
//! let the provider's chunk boundaries decide what executed. Streaming passes
//! `allow_eof: false` until the response ends, mirroring the block shape's own
//! end-tag rule (ADR 0047: the mask owns the cell boundary, not the transport).
//!
//! Two residues follow from the shape, both deliberate:
//!
//! * The close is **greedy** — the cell ends at the *last* closing tag on the
//!   line, since that tag is what makes the line an inline cell at all. So
//!   `<lashlang>print "</lashlang>"</lashlang>` reads the string correctly,
//!   while two inline cells written on one line concatenate into a single
//!   program rather than one cell plus prose. Both halves still run, in order,
//!   which is what the reply asked for; a non-greedy close would instead
//!   truncate the string case, and truncating source is the worse failure.
//! * A line that *begins* with the open tag and *ends* with the closing tag is
//!   a cell even if a human would read it as a sentence — `<lashlang> is like
//!   </lashlang>` executes. Prose that merely names the tags mid-sentence, or
//!   trails anything after the closing tag, stays prose.
//!
//! Markdown code blocks, inline tag mentions, and retired `%%lashlang` markers
//! are plain text.

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

/// Locate the first complete executable cell for `tags` in `text`, if any.
///
/// Either shape counts, whichever starts first. `text` here is a whole model
/// reply rather than a stream buffer, so its last line is complete by
/// construction and an unterminated inline cell is a cell — the streaming
/// scanner reaches the same answer only once it knows the response ended.
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
        if let Some(span) = inline_cell_span(text, pos, line, tags) {
            return Some(span);
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

/// Returns the byte length of a suffix that could still become a cell's first
/// line once more streamed text arrives.
///
/// A line that has *already* opened with the tag is held too, not only a prefix
/// of the tag: until its newline arrives it may yet close as an inline cell, and
/// flushing it early would show the user source the mask exists to suppress.
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
            let opens_an_unfinished_line =
                trimmed.starts_with(tags.open) && !trimmed.contains('\n');
            (tags.open.starts_with(trimmed) || opens_an_unfinished_line).then_some(suffix.len())
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

/// Where a streamed cell begins, and in which shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamedCellStart {
    /// A standalone open-tag line. The body arrives in later text.
    Block(StartTagSpan),
    /// A whole cell on one line: there is no later body to wait for.
    Inline(CellSpan),
}

/// The first cell start in streamed text, in either shape.
///
/// One scan rather than two calls, because precedence is positional: a buffer
/// holding an open-tag line whose body has not closed yet must not be
/// reinterpreted because some *later* line happens to be a complete inline
/// cell.
///
/// `allow_eof` is reserved for genuine response EOF, exactly as in
/// [`complete_end_tag_span`]: only then is an unterminated last line known to be
/// the whole line. Passing `true` at a chunk boundary would hand the provider's
/// framing the power to decide whether a cell executed.
pub(crate) fn complete_cell_start(
    text: &str,
    allow_eof: bool,
    tags: CellTags,
) -> Option<StreamedCellStart> {
    let mut pos = 0;
    while pos <= text.len() {
        let line = logical_line(text, pos);
        if line.has_terminator && line.text.trim() == tags.open {
            return Some(StreamedCellStart::Block(StartTagSpan {
                start_tag_start: pos,
                body_start: line.next_pos,
                body_end: text.len(),
            }));
        }
        // An unterminated line decides nothing until the response ends.
        let line_is_whole = line.has_terminator || allow_eof;
        if line_is_whole && let Some(span) = inline_cell_span(text, pos, line, tags) {
            return Some(StreamedCellStart::Inline(span));
        }
        if !line.has_terminator {
            break;
        }
        pos = line.next_pos;
    }
    None
}

/// Whether some line writes a fence for `tags` in a position the grammar
/// refuses.
///
/// Whitespace-insensitive so a dirtied tag (`<lashlang >`) is recognized as an
/// attempted fence rather than read as prose. Deliberately anchored at the line
/// start: a tag *mentioned* mid-sentence is prose, and a reply that opens a
/// line with the tag was trying to write a cell. Callers use this only after
/// extraction found no cell, so a well-formed cell never reaches it.
///
/// It cannot tell an attempted cell from prose that happens to begin a line with
/// the tag, because nothing can. Callers must therefore ask it only where the
/// turn *requires* a cell; on a turn where prose is a legal answer, a line
/// opening with the tag is as likely to be the answer as a mistake.
pub(crate) fn malformed_cell_fence(text: &str, tags: CellTags) -> bool {
    text.lines().any(|line| {
        let squeezed = line
            .chars()
            .filter(|character| !character.is_whitespace())
            .collect::<String>();
        squeezed.starts_with(tags.open) || squeezed.starts_with(tags.close)
    })
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

/// The inline-shape cell `line` is, if it is one.
///
/// `line_start` is `line`'s own byte offset in `text`, so the returned span is
/// addressed in `text` like every other span here. Callers decide whether an
/// unterminated `line` may be read at all; this only decides shape.
///
/// The body runs to the *last* closing tag on the line — see the module doc for
/// why greedy is the better residue.
fn inline_cell_span(
    text: &str,
    line_start: usize,
    line: LogicalLine<'_>,
    tags: CellTags,
) -> Option<CellSpan> {
    let trimmed = line.text.trim();
    if !trimmed.starts_with(tags.open) || !trimmed.ends_with(tags.close) {
        return None;
    }
    // The open tag must not be the closing tag's own prefix, and the two tags
    // must not overlap: `<x></x>` is the shortest inline cell there is.
    if trimmed.len() < tags.open.len() + tags.close.len() {
        return None;
    }
    let indent = line.text.len() - line.text.trim_start().len();
    let body_start = line_start + indent + tags.open.len();
    let body_end = line_start + indent + trimmed.len() - tags.close.len();
    debug_assert!(text.is_char_boundary(body_start) && text.is_char_boundary(body_end));
    Some(CellSpan {
        start_tag_start: line_start,
        body_start,
        body_end,
        end_tag_start: body_end,
        end_tag_end: line.next_pos,
    })
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

    /// The FIG-1475 shape: a whole cell on one line.
    #[test]
    fn one_line_cell_extracts_its_source() {
        assert_eq!(code("<lashlang>finish 1</lashlang>"), Some("finish 1"));
        assert_eq!(code("  <lashlang>finish 1</lashlang>  "), Some("finish 1"));
        assert_eq!(
            code("Checking.\n<lashlang>finish 1</lashlang>"),
            Some("finish 1")
        );
        assert_eq!(code("<lashlang></lashlang>"), Some(""));
    }

    #[test]
    fn one_line_cell_span_addresses_prose_and_tags() {
        let text = "Before\n<lashlang>finish 1</lashlang>\nAfter";
        let span = first_lashlang_cell_span(text).expect("inline cell");
        assert_eq!(&text[..span.start_tag_start].trim_end(), &"Before");
        assert_eq!(&text[span.body_start..span.body_end], "finish 1");
        assert_eq!(&text[span.end_tag_start..], "</lashlang>\nAfter");
        assert_eq!(&text[span.end_tag_end..], "After");
    }

    /// A block-shape cell still wins when both could match, because the block
    /// starts first.
    #[test]
    fn block_shape_takes_precedence_over_a_later_one_line_cell() {
        assert_eq!(
            code("<lashlang>\nfinish 1\n</lashlang>\n<lashlang>finish 2</lashlang>"),
            Some("finish 1")
        );
    }

    /// Prose that merely *names* both tags on one line is not a cell: the line
    /// has to start with the open tag and end with the close tag.
    #[test]
    fn one_line_tag_mentions_are_still_prose() {
        assert!(first_lashlang_cell_span("Use <lashlang> and </lashlang> tags.").is_none());
        assert!(first_lashlang_cell_span("<lashlang> and </lashlang> are the tags.").is_none());
        assert!(first_lashlang_cell_span("<lashlang>finish 1</lashlang> — done.").is_none());
    }

    /// The inline close is greedy, and both documented residues follow from it.
    #[test]
    fn one_line_cell_closes_at_the_last_closing_tag() {
        // A closing tag inside a string survives, which non-greedy would truncate.
        assert_eq!(
            code("<lashlang>print \"</lashlang>\"</lashlang>"),
            Some("print \"</lashlang>\"")
        );
        // Two cells written on one line concatenate into one program, in order.
        assert_eq!(
            code("<lashlang>print 1</lashlang><lashlang>print 2</lashlang>"),
            Some("print 1</lashlang><lashlang>print 2")
        );
    }

    /// A sentence shaped exactly like a cell is a cell. Nothing can tell them
    /// apart, and the shape is what the grammar promises to read.
    #[test]
    fn a_sentence_shaped_like_a_one_line_cell_is_a_cell() {
        assert_eq!(code("<lashlang> is like </lashlang>"), Some(" is like "));
    }

    #[test]
    fn malformed_fences_are_recognized_as_attempted_cells() {
        for text in [
            "<lashlang>finish 1",
            "<lashlang>finish 1\n</lashlang>",
            "<lashlang >\nfinish 1\n</lashlang>",
            "</lashlang>\nfinish 1",
            "<lashlang>",
        ] {
            assert!(
                malformed_cell_fence(text, tags()),
                "{text:?} writes a fence the grammar refuses"
            );
        }
    }

    #[test]
    fn ordinary_prose_carries_no_malformed_fence() {
        for text in [
            "Use <lashlang> here.",
            "plain prose",
            "%%lashlang\nfinish 1",
            "```lashlang\nfinish 1\n```",
        ] {
            assert!(
                !malformed_cell_fence(text, tags()),
                "{text:?} is prose, not an attempted cell"
            );
        }
    }

    #[test]
    fn streamed_cell_start_reports_the_first_shape_positionally() {
        assert!(matches!(
            complete_cell_start("Plan.\n<lashlang>\nfinish 1", false, tags()),
            Some(StreamedCellStart::Block(span)) if span.start_tag_start == 6
        ));
        assert!(matches!(
            complete_cell_start("Plan.\n<lashlang>finish 1</lashlang>\n", false, tags()),
            Some(StreamedCellStart::Inline(span)) if span.start_tag_start == 6
        ));
        assert!(matches!(
            complete_cell_start(
                "<lashlang>\nprint 1\n<lashlang>finish 2</lashlang>\n",
                false,
                tags()
            ),
            Some(StreamedCellStart::Block(_)),
        ));
        assert_eq!(
            complete_cell_start("Use <lashlang> here.\n", false, tags()),
            None
        );
    }

    /// Streaming may not read an inline cell out of a line that has not ended:
    /// `<lashlang>print 1</lashlang> ok` is prose, and every prefix of it that
    /// stops after the closing tag looks like a finished cell.
    #[test]
    fn an_unterminated_line_is_only_an_inline_cell_at_response_eof() {
        let prefix = "<lashlang>print 1</lashlang>";
        assert_eq!(complete_cell_start(prefix, false, tags()), None);
        assert!(matches!(
            complete_cell_start(prefix, true, tags()),
            Some(StreamedCellStart::Inline(_))
        ));
        // The whole line, once it ends, is prose in both readings — which is what
        // the streamed answer has to agree with.
        let whole = "<lashlang>print 1</lashlang> ok";
        assert_eq!(complete_cell_start(whole, true, tags()), None);
        assert_eq!(complete_cell_start(whole, false, tags()), None);
        assert!(first_lashlang_cell_span(whole).is_none());
    }

    /// A line that has opened with the tag is held until it ends, so an inline
    /// cell's source cannot reach the user as prose.
    #[test]
    fn an_opened_line_is_held_until_it_ends() {
        assert_eq!(
            possible_start_tag_suffix_len("Plan.\n<lashlang>fin", tags()),
            "<lashlang>fin".len()
        );
        assert_eq!(
            possible_start_tag_suffix_len("Plan.\n<lashlang> here\n", tags()),
            0
        );
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
