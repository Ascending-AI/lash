use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::theme;

/// Pad a string with trailing spaces to reach a target display width.
fn pad_display(s: &str, target: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    let padding = target.saturating_sub(w);
    format!("{}{}", s, " ".repeat(padding))
}

/// Truncate a string to fit within `max_w` display columns, appending `…` if truncated.
/// Uses Unicode character widths for correct handling of CJK and other wide characters.
fn truncate_display(s: &str, max_w: usize) -> String {
    let w = UnicodeWidthStr::width(s);
    if w <= max_w {
        return s.to_string();
    }
    // Need to truncate — reserve 1 column for '…'
    let target = max_w.saturating_sub(1);
    let mut result = String::new();
    let mut col = 0;
    for ch in s.chars() {
        let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
        if col + cw > target {
            break;
        }
        result.push(ch);
        col += cw;
    }
    result.push('\u{2026}');
    result
}

/// Parse markdown text and return styled ratatui Lines.
/// `max_width` constrains table rendering so borders are never wider than the viewport.
/// Used for finalized AssistantText blocks only (not streaming).
pub fn render_markdown(text: &str, max_width: usize) -> Vec<Line<'static>> {
    let mut renderer = MdRenderer::new(max_width);
    let opts = Options::ENABLE_TABLES;
    let parser = Parser::new_ext(text, opts);
    for event in parser {
        renderer.process(event);
    }
    renderer.flush_line();
    renderer.lines
}

fn is_blank_line(line: &Line<'_>) -> bool {
    line.spans.iter().all(|s| s.content.trim().is_empty())
}

/// Normalize rendered markdown lines for chat-style blocks:
/// - trim leading/trailing blank lines
/// - collapse multiple blank lines to a single blank line
pub fn compact_lines(lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    let mut out: Vec<Line<'static>> = Vec::with_capacity(lines.len());
    let mut prev_blank = true;

    for line in lines {
        let blank = is_blank_line(&line);
        if blank {
            if !prev_blank {
                out.push(Line::from(""));
            }
        } else {
            out.push(line);
        }
        prev_blank = blank;
    }

    while out.last().is_some_and(is_blank_line) {
        out.pop();
    }

    out
}

/// Render markdown and apply chat-style line compaction.
pub fn render_markdown_compact(text: &str, max_width: usize) -> Vec<Line<'static>> {
    compact_lines(render_markdown(text, max_width))
}

struct MdRenderer {
    lines: Vec<Line<'static>>,
    spans: Vec<Span<'static>>,
    style_stack: Vec<Style>,
    max_width: usize,
    in_code_block: bool,
    in_list: bool,
    in_item: bool,
    // Table buffering: collect all rows, then render with aligned columns
    in_table: bool,
    in_table_head: bool,
    table_rows: Vec<Vec<String>>, // rows of cells (text content)
    table_head: Vec<String>,      // header row
    current_cell: String,         // accumulator for current cell text
}

impl MdRenderer {
    fn new(max_width: usize) -> Self {
        Self {
            lines: Vec::new(),
            spans: Vec::new(),
            style_stack: vec![theme::assistant_text()],
            max_width,
            in_code_block: false,
            in_list: false,
            in_item: false,
            in_table: false,
            in_table_head: false,
            table_rows: Vec::new(),
            table_head: Vec::new(),
            current_cell: String::new(),
        }
    }

    fn current_style(&self) -> Style {
        self.style_stack
            .last()
            .copied()
            .unwrap_or(theme::assistant_text())
    }

    fn push_style(&mut self, style: Style) {
        self.style_stack.push(style);
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        }
    }

    fn flush_line(&mut self) {
        if !self.spans.is_empty() {
            let spans = std::mem::take(&mut self.spans);
            self.lines.push(Line::from(spans));
        }
    }

    fn blank_line(&mut self) {
        self.lines.push(Line::from(""));
    }

    /// Render the buffered table as aligned columns with box-drawing borders.
    /// Column widths are constrained to `self.max_width` so borders never wrap.
    fn flush_table(&mut self) {
        let head = std::mem::take(&mut self.table_head);
        let rows = std::mem::take(&mut self.table_rows);

        if head.is_empty() && rows.is_empty() {
            return;
        }

        // Compute column count and natural display widths (using Unicode width)
        let col_count = head
            .len()
            .max(rows.iter().map(|r| r.len()).max().unwrap_or(0));
        let mut widths = vec![0usize; col_count];
        for (i, cell) in head.iter().enumerate() {
            widths[i] = widths[i].max(UnicodeWidthStr::width(cell.as_str()));
        }
        for row in &rows {
            for (i, cell) in row.iter().enumerate() {
                widths[i] = widths[i].max(UnicodeWidthStr::width(cell.as_str()));
            }
        }

        // Constrain total table width to max_width
        if self.max_width > 0 && col_count > 0 {
            // border_overhead: leading │ + (space + content + space + │) per column
            let border_overhead = 1 + col_count * 3;
            let available = self.max_width.saturating_sub(border_overhead);
            let total: usize = widths.iter().sum();
            if total > available && available > 0 {
                let min_col = 3usize;
                // Shrink each column proportionally, with a minimum of min_col
                let mut new_widths = vec![0usize; col_count];
                for (i, &w) in widths.iter().enumerate() {
                    new_widths[i] = ((w as u64 * available as u64) / total as u64) as usize;
                    new_widths[i] = new_widths[i].max(min_col);
                }
                // If rounding pushed us over, trim the widest columns
                let mut new_total: usize = new_widths.iter().sum();
                while new_total > available {
                    // Find widest column and shrink by 1
                    if let Some(max_idx) = new_widths
                        .iter()
                        .enumerate()
                        .filter(|(_, w)| **w > min_col)
                        .max_by_key(|(_, w)| **w)
                        .map(|(i, _)| i)
                    {
                        new_widths[max_idx] -= 1;
                        new_total -= 1;
                    } else {
                        break; // all columns at minimum
                    }
                }
                widths = new_widths;
            }
        }

        let chrome = theme::code_chrome();
        let head_style = theme::assistant_text().add_modifier(Modifier::BOLD);
        let cell_style = theme::assistant_text();

        // Helper: build a row line with display-width-padded, truncated cells
        let build_row = |cells: &[String], style: Style, widths: &[usize]| -> Line<'static> {
            let mut spans: Vec<Span<'static>> = Vec::new();
            spans.push(Span::styled("\u{2502}", chrome)); // │
            for (i, w) in widths.iter().enumerate() {
                let text = cells.get(i).map(|s| s.as_str()).unwrap_or("");
                let display = truncate_display(text, *w);
                spans.push(Span::styled(
                    format!(" {} ", pad_display(&display, *w)),
                    style,
                ));
                spans.push(Span::styled("\u{2502}", chrome)); // │
            }
            Line::from(spans)
        };

        // Helper: build a separator line (─ is 1 display column wide)
        let build_sep = |left: &str, mid: &str, right: &str, widths: &[usize]| -> Line<'static> {
            let mut s = left.to_string();
            for (i, w) in widths.iter().enumerate() {
                // +2 for the space padding on each side of the cell
                for _ in 0..(w + 2) {
                    s.push('\u{2500}'); // ─
                }
                if i < widths.len() - 1 {
                    s.push_str(mid);
                }
            }
            s.push_str(right);
            Line::from(Span::styled(s, chrome))
        };

        // Top border: ┌──┬──┐
        self.lines
            .push(build_sep("\u{250c}", "\u{252c}", "\u{2510}", &widths));

        // Header row
        if !head.is_empty() {
            self.lines.push(build_row(&head, head_style, &widths));
            // Header separator: ├──┼──┤
            self.lines
                .push(build_sep("\u{251c}", "\u{253c}", "\u{2524}", &widths));
        }

        // Data rows
        for row in &rows {
            self.lines.push(build_row(row, cell_style, &widths));
        }

        // Bottom border: └──┴──┘
        self.lines
            .push(build_sep("\u{2514}", "\u{2534}", "\u{2518}", &widths));
        self.blank_line();
    }

    fn process(&mut self, event: Event<'_>) {
        match event {
            // ── Table events ──
            Event::Start(Tag::Table(_)) => {
                self.flush_line();
                self.in_table = true;
                self.table_head.clear();
                self.table_rows.clear();
            }
            Event::End(TagEnd::Table) => {
                self.in_table = false;
                self.flush_table();
            }
            Event::Start(Tag::TableHead) => {
                self.in_table_head = true;
            }
            Event::End(TagEnd::TableHead) => {
                self.in_table_head = false;
            }
            Event::Start(Tag::TableRow) => {
                if !self.in_table_head {
                    self.table_rows.push(Vec::new());
                }
            }
            Event::End(TagEnd::TableRow) => {}
            Event::Start(Tag::TableCell) => {
                self.current_cell.clear();
            }
            Event::End(TagEnd::TableCell) => {
                let cell = std::mem::take(&mut self.current_cell);
                if self.in_table_head {
                    self.table_head.push(cell);
                } else if let Some(row) = self.table_rows.last_mut() {
                    row.push(cell);
                }
            }

            // ── Heading ──
            Event::Start(Tag::Heading { .. }) => {
                self.flush_line();
                self.push_style(theme::heading());
            }
            Event::End(TagEnd::Heading(_)) => {
                self.flush_line();
                self.pop_style();
                self.blank_line();
            }

            // ── Paragraph ──
            Event::Start(Tag::Paragraph) => {}
            Event::End(TagEnd::Paragraph) => {
                if !self.in_table {
                    self.flush_line();
                    self.blank_line();
                }
            }

            // ── Inline formatting ──
            Event::Start(Tag::Strong) => {
                let base = self.current_style();
                self.push_style(base.add_modifier(Modifier::BOLD));
            }
            Event::End(TagEnd::Strong) => {
                self.pop_style();
            }
            Event::Start(Tag::Emphasis) => {
                let base = self.current_style();
                self.push_style(base.add_modifier(Modifier::ITALIC));
            }
            Event::End(TagEnd::Emphasis) => {
                self.pop_style();
            }
            Event::Code(code) => {
                if self.in_table {
                    self.current_cell.push_str(&code);
                } else {
                    self.spans
                        .push(Span::styled(code.to_string(), theme::inline_code()));
                }
            }

            // ── Code blocks ──
            Event::Start(Tag::CodeBlock(_)) => {
                self.flush_line();
                self.in_code_block = true;
            }
            Event::End(TagEnd::CodeBlock) => {
                self.in_code_block = false;
                self.blank_line();
            }

            // ── Lists ──
            Event::Start(Tag::List(_)) => {
                self.in_list = true;
            }
            Event::End(TagEnd::List(_)) => {
                self.in_list = false;
                self.blank_line();
            }
            Event::Start(Tag::Item) => {
                self.flush_line();
                self.in_item = true;
            }
            Event::End(TagEnd::Item) => {
                self.flush_line();
                self.in_item = false;
            }

            // ── Text ──
            Event::Text(text) => {
                if self.in_table {
                    self.current_cell.push_str(&text);
                } else if self.in_code_block {
                    for line in text.lines() {
                        self.lines.push(Line::from(vec![
                            Span::styled("\u{2502} ", theme::code_chrome()),
                            Span::styled(line.to_string(), theme::code_content()),
                        ]));
                    }
                } else if self.in_item && self.spans.is_empty() {
                    self.spans
                        .push(Span::styled("  \u{2022} ", self.current_style()));
                    self.spans
                        .push(Span::styled(text.to_string(), self.current_style()));
                } else {
                    self.spans
                        .push(Span::styled(text.to_string(), self.current_style()));
                }
            }
            Event::SoftBreak => {
                if !self.in_table {
                    // Preserve model-emitted line breaks in chat output instead of
                    // collapsing them into spaces.
                    self.flush_line();
                }
            }
            Event::HardBreak => {
                self.flush_line();
            }

            // ── Horizontal rule (---) ──
            Event::Rule => {
                self.flush_line();
                self.lines.push(Line::from(Span::styled(
                    "\u{2500}".repeat(40),
                    theme::code_chrome(),
                )));
                self.blank_line();
            }

            // ── Links ──
            Event::Start(Tag::Link { .. }) => {}
            Event::End(TagEnd::Link) => {}

            _ => {}
        }
    }
}

/// Count the visual height of rendered markdown, accounting for line wrapping at `width`.
pub fn markdown_height(text: &str, width: usize) -> usize {
    let lines = render_markdown(text, width);
    if width == 0 {
        return lines.len();
    }
    lines
        .iter()
        .map(|line| {
            let w = line.width();
            if w == 0 { 1 } else { w.div_ceil(width) }
        })
        .sum::<usize>()
        .max(1)
}

/// Count visual height after chat-style compaction.
pub fn markdown_height_compact(text: &str, width: usize) -> usize {
    if text.trim().is_empty() {
        return 0;
    }
    let lines = render_markdown_compact(text, width);
    if width == 0 {
        return lines.len();
    }
    lines
        .iter()
        .map(|line| {
            let w = line.width();
            if w == 0 { 1 } else { w.div_ceil(width) }
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── pad_display ──

    #[test]
    fn pad_display_already_correct() {
        assert_eq!(pad_display("abc", 3), "abc");
    }

    #[test]
    fn pad_display_under_width() {
        assert_eq!(pad_display("ab", 5), "ab   ");
    }

    #[test]
    fn pad_display_over_width() {
        // Over target width — no padding added (saturating_sub → 0)
        assert_eq!(pad_display("abcdef", 3), "abcdef");
    }

    #[test]
    fn pad_display_cjk() {
        // CJK char is 2 columns, so "世" = width 2, target 5 → 3 spaces
        let result = pad_display("\u{4e16}", 5);
        assert_eq!(result, "\u{4e16}   ");
    }

    // ── render_markdown ──

    #[test]
    fn render_plain_text() {
        let lines = render_markdown("hello world", 80);
        // Plain text → paragraph with text + blank line after
        assert!(!lines.is_empty());
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("hello world"));
    }

    #[test]
    fn render_heading() {
        let lines = render_markdown("# Title", 80);
        // Should have a bold styled line and a blank line
        assert!(lines.len() >= 2);
        let text: String = lines[0].spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("Title"));
    }

    #[test]
    fn render_bullet_list() {
        let lines = render_markdown("- item one\n- item two", 80);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains("\u{2022}"));
        assert!(all_text.contains("item one"));
        assert!(all_text.contains("item two"));
    }

    #[test]
    fn render_code_block() {
        let lines = render_markdown("```\nfn main() {}\n```", 80);
        // Code block lines have "│ " prefix
        let has_code_chrome = lines
            .iter()
            .any(|l| l.spans.iter().any(|s| s.content.contains('\u{2502}')));
        assert!(has_code_chrome);
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains("fn main()"));
    }

    #[test]
    fn render_bold_and_italic() {
        let lines = render_markdown("**bold** and *italic*", 80);
        // Just verify it doesn't crash and produces output
        assert!(!lines.is_empty());
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains("bold"));
        assert!(all_text.contains("italic"));
    }

    #[test]
    fn render_soft_break_as_newline() {
        let lines = render_markdown("line one\nline two", 80);
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert!(rendered.iter().any(|line| line.contains("line one")));
        assert!(rendered.iter().any(|line| line.contains("line two")));
        assert!(
            !rendered
                .iter()
                .any(|line| line.contains("line one") && line.contains("line two"))
        );
    }

    #[test]
    fn render_table() {
        let lines = render_markdown("| A | B |\n|---|---|\n| 1 | 2 |", 80);
        // Table should have box-drawing characters
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains('\u{250c}')); // ┌
        assert!(all_text.contains('\u{2514}')); // └
        assert!(all_text.contains("A"));
        assert!(all_text.contains("1"));
    }

    #[test]
    fn render_table_truncated() {
        // Table with long content forced into narrow width
        let lines = render_markdown(
            "| Name | Description |\n|---|---|\n| short | A very long description that should be truncated |",
            30,
        );
        let all_text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect();
        assert!(all_text.contains('\u{250c}')); // ┌
        assert!(all_text.contains('\u{2026}')); // … (truncation marker)
        // Each line should fit within max_width
        for line in &lines {
            assert!(
                line.width() <= 30,
                "line width {} > 30: {:?}",
                line.width(),
                line
            );
        }
    }

    // ── markdown_height ──

    #[test]
    fn markdown_height_simple() {
        let h = markdown_height("hello", 80);
        assert!(h >= 1);
    }

    #[test]
    fn markdown_height_multiline() {
        let h = markdown_height("line1\n\nline2", 80);
        assert!(h >= 2);
    }

    #[test]
    fn markdown_height_wrapping() {
        // Very narrow width forces wrapping
        let h_narrow = markdown_height("a long line of text", 5);
        let h_wide = markdown_height("a long line of text", 200);
        assert!(h_narrow > h_wide);
    }

    #[test]
    fn compact_lines_trims_outer_blank_rows() {
        let lines = vec![
            Line::from(""),
            Line::from("hi"),
            Line::from(""),
            Line::from(""),
        ];
        let compact = compact_lines(lines);
        assert_eq!(compact.len(), 1);
        let text: String = compact[0]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text, "hi");
    }

    #[test]
    fn compact_lines_collapses_blank_runs() {
        let lines = vec![
            Line::from("a"),
            Line::from(""),
            Line::from(""),
            Line::from("b"),
        ];
        let compact = compact_lines(lines);
        assert_eq!(compact.len(), 3);
        let text: Vec<String> = compact
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text, vec!["a".to_string(), "".to_string(), "b".to_string()]);
    }
}
