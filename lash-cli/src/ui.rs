use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

use crate::app::{
    App, DisplayBlock, SuggestionKind, TASK_TRAY_TWO_COL_MIN_INNER_WIDTH, TaskSnapshot,
};
use crate::markdown;
use crate::theme;

fn input_height(app: &App, frame_width: u16) -> u16 {
    if app.has_prompt() {
        prompt_height(app, frame_width)
    } else {
        // Inner width = frame width; prefix handled by wrap_line
        let inner_w = frame_width as usize;
        let visual_lines = input_visual_lines(&app.input, inner_w);
        (visual_lines as u16 + 2).min(12)
    }
}

/// Exact history viewport height based on the same layout math used in draw().
pub fn history_viewport_height(app: &App, frame_width: u16, frame_height: u16) -> usize {
    let strike_h = if app.running { 1 } else { 0 };
    let task_tray_h = app.task_tray_height(frame_width);
    let queued_h: u16 = if app.has_queued_message() { 1 } else { 0 };
    let input_h = input_height(app, frame_width);
    let overhead = 1 + strike_h + task_tray_h + queued_h + input_h;
    frame_height.saturating_sub(overhead) as usize
}

pub fn draw(frame: &mut Frame, app: &App) {
    // Paint entire frame with FORM bg so no terminal background bleeds through
    frame.render_widget(Block::default().style(theme::history_bg()), frame.area());

    let strike_h = if app.running { 1 } else { 0 };
    let task_tray_h = app.task_tray_height(frame.area().width);
    let queued_h: u16 = if app.has_queued_message() { 1 } else { 0 };
    let input_h = input_height(app, frame.area().width);

    let chunks = Layout::vertical([
        Constraint::Length(1),           // [0] status bar
        Constraint::Min(3),              // [1] history
        Constraint::Length(strike_h),    // [2] strike zone (only when running)
        Constraint::Length(task_tray_h), // [3] task tray
        Constraint::Length(queued_h),    // [4] queued message bar
        Constraint::Length(input_h),     // [5] input (dynamic height)
    ])
    .split(frame.area());

    draw_status_bar(frame, app, chunks[0]);
    draw_history(frame, app, chunks[1]);
    if app.running {
        draw_strike_zone(frame, app, chunks[2]);
    }
    draw_task_tray(frame, app, chunks[3]);
    draw_queued_message(frame, app, chunks[4]);
    if app.has_prompt() {
        draw_prompt(frame, app, chunks[5]);
    } else {
        draw_input(frame, app, chunks[5]);
        draw_suggestions(frame, app, chunks[5]);
    }
    draw_session_picker(frame, app, chunks[1]); // overlay on history area
    draw_skill_picker(frame, app, chunks[1]); // overlay on history area
}

fn draw_status_bar(frame: &mut Frame, app: &App, area: Rect) {
    let width = area.width as usize;
    if width == 0 {
        return;
    }

    let is_plan = app.mode == crate::app::Mode::Plan;

    // ── Left-side segment widths (progressive: name → model → mode) ──
    let name_w = " lash".chars().count();
    let sep_w = theme::STATUS_SEP.chars().count();
    let model_w = app.model.chars().count();
    let left_1 = name_w;
    let left_2 = name_w + sep_w + model_w;
    let left_3 = left_2 + if is_plan { sep_w + 4 } else { 0 };

    // ── Right-side: token components ──
    let display_input_tokens = app.token_usage.input_tokens;
    let display_output_tokens = app.token_usage.output_tokens + app.live_output_tokens_estimate;
    let display_total_tokens = display_input_tokens + display_output_tokens;
    let has_usage = display_total_tokens > 0;
    let ctx_pct: Option<u64> = app.context_window.and_then(|ctx_win| {
        if app.last_input_tokens > 0 && ctx_win > 0 {
            Some((app.last_input_tokens as f64 / ctx_win as f64 * 100.0) as u64)
        } else {
            None
        }
    });

    let in_out = if has_usage {
        format!(
            "{} in \u{b7} {} out",
            crate::app::format_tokens(display_input_tokens),
            crate::app::format_tokens(display_output_tokens),
        )
    } else {
        String::new()
    };
    let total = if has_usage {
        format!("{} total", crate::app::format_tokens(display_total_tokens),)
    } else {
        String::new()
    };
    let ctx = ctx_pct.map(|p| format!("{}% ctx", p)).unwrap_or_default();

    // Build right-side variants from longest to shortest
    let join_parts = |parts: &[&str]| -> String {
        parts
            .iter()
            .filter(|s| !s.is_empty())
            .copied()
            .collect::<Vec<_>>()
            .join(" \u{b7} ")
    };
    let r0 = join_parts(&[&in_out, &total, &ctx]);
    let r1 = join_parts(&[&total, &ctx]);
    let r2 = ctx.clone();
    let right_variants = [r0, r1, r2, String::new()];

    let right_w = |idx: usize| -> usize {
        let s = &right_variants[idx];
        if s.is_empty() {
            0
        } else {
            s.chars().count() + 1 // +1 trailing space
        }
    };

    // ── Progressive fit (drop lowest priority first) ──
    let left_w = |level: usize| match level {
        3 => left_3,
        2 => left_2,
        _ => left_1,
    };
    let check = |ll: usize, ri: usize| -> bool {
        let lw = left_w(ll);
        let rw = right_w(ri);
        if rw > 0 {
            lw + 1 + rw <= width
        } else {
            lw <= width
        }
    };

    let mut left_level = 3usize;
    let mut right_idx = 0usize;

    if !check(left_level, right_idx) {
        right_idx = 1;
    }
    if !check(left_level, right_idx) {
        right_idx = 2;
    }
    if !check(left_level, right_idx) {
        right_idx = 3;
    }
    if !check(left_level, right_idx) {
        left_level = 2;
    }
    if !check(left_level, right_idx) {
        left_level = 1;
    }

    // ── Build spans ──
    let mut spans: Vec<Span> = vec![Span::styled(" lash", theme::app_title())];
    if left_level >= 2 {
        spans.push(Span::styled(theme::STATUS_SEP, theme::status_separator()));
        spans.push(Span::styled(&app.model, theme::model_name()));
    }
    if left_level >= 3 && is_plan {
        spans.push(Span::styled(theme::STATUS_SEP, theme::status_separator()));
        spans.push(Span::styled(
            "PLAN",
            Style::default()
                .fg(theme::SODIUM)
                .add_modifier(Modifier::BOLD),
        ));
    }

    // Padding between left and right
    let actual_left: usize = spans.iter().map(|s| s.width()).sum();
    let total_right = right_w(right_idx);
    let pad = width.saturating_sub(actual_left + total_right);
    if pad > 0 {
        spans.push(Span::styled(" ".repeat(pad), theme::bar_bg()));
    }

    // Token spans
    if right_idx < 3 && !right_variants[right_idx].is_empty() {
        let token_text = &right_variants[right_idx];
        if let Some(pct) = ctx_pct {
            let ctx_color = if pct >= 80 {
                theme::SODIUM
            } else {
                theme::CHALK_DIM
            };
            if right_idx == 2 {
                // ctx only
                spans.push(Span::styled(
                    token_text.clone(),
                    Style::default().fg(ctx_color),
                ));
            } else {
                let ctx_suffix = format!(" \u{b7} {}% ctx", pct);
                if let Some(prefix) = token_text.strip_suffix(&ctx_suffix) {
                    spans.push(Span::styled(
                        prefix.to_string(),
                        Style::default().fg(theme::CHALK_DIM),
                    ));
                    spans.push(Span::styled(ctx_suffix, Style::default().fg(ctx_color)));
                } else {
                    // text IS just the ctx (no prefix)
                    spans.push(Span::styled(
                        token_text.clone(),
                        Style::default().fg(ctx_color),
                    ));
                }
            }
        } else {
            spans.push(Span::styled(
                token_text.clone(),
                Style::default().fg(theme::CHALK_DIM),
            ));
        }
        spans.push(Span::styled(" ", theme::bar_bg()));
    }

    let bar = Paragraph::new(Line::from(spans)).style(theme::bar_bg());
    frame.render_widget(bar, area);
}

fn draw_history(frame: &mut Frame, app: &App, area: Rect) {
    let viewport_height = area.height as usize;
    let viewport_width = area.width as usize;

    // scroll_offset is already clamped by the main loop before draw()
    let scroll = app.scroll_offset;

    // Find the first visible block via binary search on the height cache
    let (first_idx, skip_lines) = find_visible_block_readonly(app, scroll, viewport_width);

    let mut lines: Vec<Line> = Vec::with_capacity(viewport_height + skip_lines + 20);

    // Render only blocks from first_idx until we have enough lines
    for idx in first_idx..app.blocks.len() {
        render_block(
            &app.blocks,
            idx,
            app.expand_level,
            &mut lines,
            viewport_width,
            viewport_height,
        );
        if lines.len() >= viewport_height + skip_lines {
            break;
        }
    }

    // Render live streaming LLM text (pending_text accumulates TextDelta events).
    // Use a uniform 2-space indent on every line — no marker or empty-line
    // special-casing — so that re-rendering partial markdown each frame
    // produces a stable line count (avoids flicker / vertical oscillation).
    // The ■ marker appears once the text is committed as an AssistantText block.
    if !app.pending_text.is_empty() && lines.len() < viewport_height + skip_lines {
        let rendered =
            markdown::render_markdown_compact(&app.pending_text, viewport_width.saturating_sub(2));
        for line in rendered {
            let mut spans = vec![Span::raw("  ")];
            spans.extend(line.spans);
            lines.push(Line::from(spans));
            if lines.len() >= viewport_height + skip_lines {
                break;
            }
        }
    }

    // Render live streaming tool output (e.g. bash)
    if !app.streaming_output.is_empty() && lines.len() < viewport_height + skip_lines {
        for line in &app.streaming_output {
            lines.push(Line::from(vec![
                Span::styled("\u{2502} ", theme::code_chrome()),
                Span::styled(line.as_str(), theme::code_content()),
            ]));
            if lines.len() >= viewport_height + skip_lines {
                break;
            }
        }
    }

    // Render active delegate indicator
    if let Some((ref name, ref task, ref started)) = app.active_delegate
        && lines.len() < viewport_height + skip_lines
    {
        render_delegate_line(
            app.tick,
            name,
            task,
            started.elapsed(),
            &mut lines,
            viewport_width,
        );
    }

    // Bottom-align content when it doesn't fill the viewport (chat-style).
    // Use visual row count (accounting for line wrapping) not logical line count.
    if scroll == 0 {
        let total_visual: usize = lines
            .iter()
            .map(|line| {
                let w = line.width();
                if viewport_width == 0 || w == 0 {
                    1
                } else {
                    w.div_ceil(viewport_width)
                }
            })
            .sum();
        let pad_count = viewport_height.saturating_sub(total_visual);
        if pad_count > 0 {
            let mut padded = Vec::with_capacity(pad_count + lines.len());
            for _ in 0..pad_count {
                padded.push(Line::from(""));
            }
            padded.extend(lines);
            lines = padded;
        }
    }

    // Use Paragraph's built-in scroll to skip visual rows correctly.
    // Manual .skip()/.take() on logical Lines is wrong because skip_lines
    // is in visual-row space (from the height cache) but Lines can wrap.
    let paragraph = Paragraph::new(lines)
        .wrap(Wrap { trim: false })
        .style(theme::history_bg())
        .block(Block::default().borders(Borders::NONE))
        .scroll((skip_lines as u16, 0));

    frame.render_widget(paragraph, area);
}

/// Read-only version of find_visible_block that uses the pre-computed height cache.
/// The cache MUST be pre-warmed via `ensure_height_cache_pub` before calling draw().
fn find_visible_block_readonly(app: &App, scroll_offset: usize, _width: usize) -> (usize, usize) {
    if app.height_cache_snapshot().is_empty() {
        return (0, 0);
    }
    let cache = app.height_cache_snapshot();
    let idx = cache.partition_point(|&cumulative| cumulative <= scroll_offset);
    if idx >= app.blocks.len() {
        return (app.blocks.len(), 0);
    }
    let block_start = if idx == 0 { 0 } else { cache[idx - 1] };
    let skip = scroll_offset - block_start;
    (idx, skip)
}

fn truncate_to_display_width(text: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    if UnicodeWidthStr::width(text) <= max_width {
        return text.to_string();
    }
    let target = max_width.saturating_sub(1);
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if width + w > target {
            break;
        }
        out.push(ch);
        width += w;
    }
    out.push('\u{2026}');
    out
}

/// Build a ghost fold summary for a group of tool calls starting at `idx`.
/// `idx` should point to the first (non-continuation) ToolCall in a group.
/// Scans forward through contiguous ToolCall/CodeBlock/CodeOutput blocks,
/// collecting tool names and durations.
/// Returns `(summary_string, any_failed)`.
fn build_ghost_fold_summary(blocks: &[DisplayBlock], idx: usize) -> (String, bool) {
    let mut groups: Vec<(String, usize)> = Vec::new();
    let mut total_ms = 0u64;
    let mut any_failed = false;

    // Include the block at idx itself
    if let DisplayBlock::ToolCall {
        name,
        success,
        duration_ms,
        ..
    } = &blocks[idx]
    {
        total_ms += *duration_ms;
        if !*success {
            any_failed = true;
        }
        groups.push((name.clone(), 1));
    }

    // Scan forward from idx+1
    for b in &blocks[idx + 1..] {
        match b {
            DisplayBlock::CodeBlock { .. } | DisplayBlock::CodeOutput { .. } => continue,
            DisplayBlock::ToolCall {
                name,
                success,
                duration_ms,
                ..
            } => {
                total_ms += *duration_ms;
                if !*success {
                    any_failed = true;
                }
                if let Some((last_name, count)) = groups.last_mut()
                    && last_name == name
                {
                    *count += 1;
                } else {
                    groups.push((name.clone(), 1));
                }
            }
            _ => break,
        }
    }

    let total_tools: usize = groups.iter().map(|(_, c)| c).sum();
    let group_text = groups
        .iter()
        .map(|(name, count)| {
            if *count > 1 {
                format!("{}\u{00d7}{}", name, count)
            } else {
                name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(", ");
    let summary = format!(
        "+ {} tool{} \u{b7} {} \u{b7} {}",
        total_tools,
        if total_tools != 1 { "s" } else { "" },
        group_text,
        crate::util::format_duration_ms(total_ms)
    );
    (summary, any_failed)
}

/// Build a ghost fold summary for a group of code blocks starting at `idx`.
/// `idx` should point to the first (non-continuation) CodeBlock in a group.
/// Scans forward through contiguous CodeBlock/ToolCall/CodeOutput blocks.
fn build_code_fold_summary(blocks: &[DisplayBlock], idx: usize) -> String {
    let mut block_count = 0usize;
    let mut line_count = 0usize;

    for b in &blocks[idx..] {
        match b {
            DisplayBlock::CodeBlock { code, .. } => {
                block_count += 1;
                line_count += code.lines().count().max(1);
            }
            DisplayBlock::ToolCall { .. } | DisplayBlock::CodeOutput { .. } => continue,
            _ => break,
        }
    }

    format!(
        "\u{25b6} {} code block{} \u{b7} {} line{}",
        block_count,
        if block_count != 1 { "s" } else { "" },
        line_count,
        if line_count != 1 { "s" } else { "" }
    )
}

fn render_block<'a>(
    blocks: &'a [DisplayBlock],
    idx: usize,
    expand_level: u8,
    lines: &mut Vec<Line<'a>>,
    viewport_width: usize,
    viewport_height: usize,
) {
    let block = &blocks[idx];
    match block {
        DisplayBlock::UserInput(text) => {
            // Blank line before user input to separate turns (skip for first block / after Splash)
            if idx > 0 && !matches!(blocks[idx - 1], DisplayBlock::Splash) {
                lines.push(Line::from(""));
            }
            // Circle marker on first line only, continuation lines get 2-space indent.
            let marker_style = Style::default().fg(theme::SODIUM);
            let text_style = theme::user_input();
            let prefix_w = 2; // "● " or "  " is 2 columns
            let cap = viewport_width.saturating_sub(prefix_w);
            let mut is_first = true;

            for line in text.lines() {
                if cap == 0 || line.is_empty() {
                    let prefix = if is_first {
                        Span::styled("\u{25CF} ", marker_style)
                    } else {
                        Span::raw("  ")
                    };
                    is_first = false;
                    lines.push(Line::from(vec![
                        prefix,
                        Span::styled(line.to_string(), text_style),
                    ]));
                } else {
                    let mut seg_start = 0;
                    let mut col = 0;
                    for (byte_idx, ch) in line.char_indices() {
                        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
                        if col + w > cap && col > 0 {
                            let prefix = if is_first {
                                Span::styled("\u{25CF} ", marker_style)
                            } else {
                                Span::raw("  ")
                            };
                            is_first = false;
                            lines.push(Line::from(vec![
                                prefix,
                                Span::styled(line[seg_start..byte_idx].to_string(), text_style),
                            ]));
                            seg_start = byte_idx;
                            col = w;
                        } else {
                            col += w;
                        }
                    }
                    let prefix = if is_first {
                        Span::styled("\u{25CF} ", marker_style)
                    } else {
                        Span::raw("  ")
                    };
                    is_first = false;
                    lines.push(Line::from(vec![
                        prefix,
                        Span::styled(line[seg_start..].to_string(), text_style),
                    ]));
                }
            }
        }
        DisplayBlock::AssistantText(text) => {
            let prefix_w = 2; // "■ " or "  " is 2 columns
            let rendered =
                markdown::render_markdown_compact(text, viewport_width.saturating_sub(prefix_w));
            if rendered.is_empty() {
                return;
            }
            // Breathing line before assistant response (separates from user turn / tool blocks)
            if idx > 0
                && !matches!(
                    blocks[idx - 1],
                    DisplayBlock::AssistantText(_) | DisplayBlock::Splash
                )
            {
                lines.push(Line::from(""));
            }
            // Square marker on the first non-empty line, plain indent on others,
            // empty lines pass through with no prefix.
            let mut marker_placed = false;
            for line in rendered {
                let is_empty = line.spans.iter().all(|s| s.content.trim().is_empty());
                if is_empty {
                    lines.push(Line::from(""));
                } else if !marker_placed {
                    marker_placed = true;
                    let mut spans = vec![Span::styled("\u{25A0} ", theme::assistant_bar())];
                    spans.extend(line.spans);
                    lines.push(Line::from(spans));
                } else {
                    let mut spans = vec![Span::raw("  ")];
                    spans.extend(line.spans);
                    lines.push(Line::from(spans));
                }
            }
        }
        DisplayBlock::CodeBlock { code, continuation } => {
            match expand_level {
                0 => {
                    if !*continuation {
                        let summary = truncate_to_display_width(
                            &build_code_fold_summary(blocks, idx),
                            viewport_width,
                        );
                        lines.push(Line::from(Span::styled(summary, theme::code_header())));
                    }
                }
                1 => {
                    let line_count = code.lines().count().max(1);
                    let summary = truncate_to_display_width(
                        &format!(
                            "\u{25b6} code \u{b7} {} line{}",
                            line_count,
                            if line_count != 1 { "s" } else { "" }
                        ),
                        viewport_width,
                    );
                    lines.push(Line::from(Span::styled(summary, theme::code_header())));
                }
                _ => {
                    // Full: render code with scribe border
                    for line in code.lines() {
                        lines.push(Line::from(vec![
                            Span::styled("\u{2502} ", theme::code_scribe()),
                            Span::styled(line, theme::code_content()),
                        ]));
                    }
                }
            }
        }
        DisplayBlock::ToolCall {
            name,
            success,
            duration_ms,
            continuation,
        } => {
            match expand_level {
                0 => {
                    if *continuation {
                        // Absorbed into the ghost fold summary — hidden
                    } else {
                        // Ghost fold: build summary scanning forward
                        let (summary, any_failed) = build_ghost_fold_summary(blocks, idx);
                        let summary = truncate_to_display_width(&summary, viewport_width);
                        lines.push(Line::from(Span::styled(
                            summary,
                            if any_failed {
                                theme::tool_failure()
                            } else {
                                theme::tool_success()
                            },
                        )));
                    }
                }
                _ => {
                    // Level 1 and 2: individual tool call lines
                    let icon = if *success { "+" } else { "\u{00d7}" };
                    let style = if *success {
                        theme::tool_success()
                    } else {
                        theme::tool_failure()
                    };
                    lines.push(Line::from(Span::styled(
                        format!(
                            "{} {} \u{b7} {}",
                            icon,
                            name,
                            crate::util::format_duration_ms(*duration_ms)
                        ),
                        style,
                    )));
                }
            }
        }
        DisplayBlock::CodeOutput { output, error } => {
            if expand_level >= 2 && !output.is_empty() {
                for line in output.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("\u{2502} ", theme::code_chrome()),
                        Span::styled(line, theme::system_output()),
                    ]));
                }
            }
            if let Some(err) = error {
                if expand_level >= 2 {
                    for line in err.lines() {
                        lines.push(Line::from(vec![
                            Span::styled("\u{2502} ", theme::code_chrome()),
                            Span::styled(line, theme::error()),
                        ]));
                    }
                } else {
                    // Level 0 and 1: show first meaningful error line only
                    let summary = err
                        .lines()
                        .find(|l| {
                            !l.trim().is_empty()
                                && !l.trim_start().starts_with("File ")
                                && !l.trim_start().starts_with("Traceback")
                        })
                        .unwrap_or(err.lines().next().unwrap_or("error"));
                    lines.push(Line::from(Span::styled(
                        format!("\u{2716} {}", summary.trim()),
                        theme::error(),
                    )));
                }
            }
        }
        DisplayBlock::ShellOutput {
            command,
            output,
            error,
        } => {
            lines.push(Line::from(Span::styled(
                format!("$ {}", command),
                theme::code_chrome(),
            )));
            if !output.is_empty() {
                for line in output.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("\u{2502} ", theme::code_chrome()),
                        Span::styled(line, theme::system_output()),
                    ]));
                }
            }
            if let Some(err) = error {
                for line in err.lines() {
                    lines.push(Line::from(vec![
                        Span::styled("\u{2502} ", theme::code_chrome()),
                        Span::styled(line, theme::error()),
                    ]));
                }
            }
        }
        DisplayBlock::Error(msg) => {
            lines.push(Line::from(Span::styled(
                format!("Error: {}", msg),
                theme::error(),
            )));
        }
        DisplayBlock::SystemMessage(text) => {
            for line in text.lines() {
                lines.push(Line::from(Span::styled(line, theme::system_message())));
            }
        }
        DisplayBlock::SubAgentResult {
            task,
            usage,
            tool_calls,
            iterations,
            success,
            is_last,
        } => {
            let connector = if *is_last {
                "\u{2514}\u{2500}"
            } else {
                "\u{251c}\u{2500}"
            };
            let status_connector = if *is_last { "   " } else { "\u{2502}  " };

            // Truncate task to fit: connector(2) + space(1) + task + sep(3) + stats(~30)
            let stats_str = format!(
                " \u{b7} {} turns \u{b7} {} tool uses \u{b7} {} tokens",
                iterations,
                tool_calls,
                crate::app::format_tokens(usage.total()),
            );
            let max_task_w = viewport_width.saturating_sub(3 + stats_str.chars().count());
            let task_display: String = task.chars().take(max_task_w).collect();
            let truncated = task.chars().count() > max_task_w;
            let task_final = if truncated {
                format!("{}\u{2026}", task_display)
            } else {
                task_display
            };

            lines.push(Line::from(vec![
                Span::styled(format!("{} ", connector), Style::default().fg(theme::ASH)),
                Span::styled(task_final, Style::default().fg(theme::CHALK_MID)),
                Span::styled(stats_str, Style::default().fg(theme::CHALK_DIM)),
            ]));

            let (status_label, status_style) = if *success {
                ("\u{2514}\u{2500} Done", Style::default().fg(theme::LICHEN))
            } else {
                ("\u{2514}\u{2500} Failed", Style::default().fg(theme::ERROR))
            };
            lines.push(Line::from(vec![
                Span::styled(status_connector, Style::default().fg(theme::ASH)),
                Span::styled(status_label, status_style),
            ]));
        }
        DisplayBlock::PlanContent(content) => {
            render_plan_block(content, lines, viewport_width);
        }
        DisplayBlock::Splash => {
            let chalk = theme::assistant_text();
            let sodium = Style::default().fg(theme::SODIUM);

            let content_width = 30;
            let content_height = 8;
            let cx = viewport_width.saturating_sub(content_width) / 2;
            let cy = viewport_height.saturating_sub(content_height) / 2;
            let pad = " ".repeat(cx);

            for _ in 0..cy {
                lines.push(Line::from(""));
            }

            let logo: &[(&str, &str)] = &[
                ("██       ████   ██████  ", "  ██"),
                ("██      ██  ██  ██     ", "█  ██"),
                ("██      ██████  ██████", "██████"),
                ("██      ██  ██      █", " ██  ██"),
                ("██████  ██  ██  ████", "  ██  ██"),
            ];
            for &(before, after) in logo {
                lines.push(Line::from(vec![
                    Span::styled(format!("{}{}", pad, before), chalk),
                    Span::styled("██", sodium),
                    Span::styled(after, chalk),
                ]));
            }
            lines.push(Line::from(Span::styled(
                format!("{}                   ██", pad),
                sodium,
            )));
            lines.push(Line::from(vec![
                Span::styled(format!("{}──────────", pad), sodium),
                Span::styled("──────────", Style::default().fg(theme::ASH_MID)),
                Span::styled("──────────", Style::default().fg(theme::ASH)),
            ]));
            lines.push(Line::from(""));

            // Bottom padding to fill viewport (so bottom-align logic sees full viewport)
            let rendered = cy + content_height;
            for _ in rendered..viewport_height {
                lines.push(Line::from(""));
            }
        }
    }
}

/// Render plan content as a bordered block with markdown inside.
fn render_plan_block<'a>(content: &'a str, lines: &mut Vec<Line<'a>>, viewport_width: usize) {
    let title = " PLAN ";
    let title_w = UnicodeWidthStr::width(title);
    let fill_w = viewport_width.saturating_sub(3 + title_w); // ┌─ + title + ┐

    // Top border: ┌─ PLAN ─────────────────────────┐
    lines.push(Line::from(vec![
        Span::styled("\u{250c}\u{2500}", Style::default().fg(theme::ASH)),
        Span::styled(
            title.to_string(),
            Style::default()
                .fg(theme::SODIUM)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("\u{2500}".repeat(fill_w), Style::default().fg(theme::ASH)),
        Span::styled("\u{2510}", Style::default().fg(theme::ASH)),
    ]));

    // Content: render markdown, hard-wrap each rendered line to fit inside the box.
    // We draw both left and right borders per visual row.
    let inner_w = viewport_width.saturating_sub(4); // "│ " + content + " │"
    let md_lines = markdown::render_markdown(content, inner_w.max(1));
    for line in md_lines {
        let raw = line
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect::<String>();

        let segments = if raw.is_empty() {
            vec![(0usize, 0usize)]
        } else {
            wrap_line(&raw, 0, inner_w.max(1))
        };

        for (start, end) in segments {
            let chunk = if raw.is_empty() {
                String::new()
            } else {
                truncate_to_display_width(&raw[start..end], inner_w)
            };
            let pad = inner_w.saturating_sub(UnicodeWidthStr::width(chunk.as_str()));
            lines.push(Line::from(vec![
                Span::styled("\u{2502} ", Style::default().fg(theme::ASH)),
                Span::styled(chunk, theme::assistant_text()),
                Span::raw(" ".repeat(pad)),
                Span::styled(" \u{2502}", Style::default().fg(theme::ASH)),
            ]));
        }
    }

    // Bottom border: └──────────────────────────────┘
    let bottom_fill = viewport_width.saturating_sub(2); // minus └ and ┘
    lines.push(Line::from(Span::styled(
        format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(bottom_fill)),
        Style::default().fg(theme::ASH),
    )));
}

// ── Delegate running indicator ───────────────────────────────────────

/// Render an animated line showing a running delegate sub-agent.
/// Format: `  ╲─ delegate_deep · Do a thorough investigation of th…   12.3s`
fn render_delegate_line<'a>(
    tick: usize,
    name: &'a str,
    task: &'a str,
    elapsed: std::time::Duration,
    lines: &mut Vec<Line<'a>>,
    viewport_width: usize,
) {
    const ANGLES: &[char] = &['\u{2572}', '\u{2500}', '\u{2571}', '\u{2502}']; // ╲ ─ ╱ │
    let idx = (tick / 2) % ANGLES.len();
    let trail_idx = (idx + ANGLES.len() - 1) % ANGLES.len();
    let lead = ANGLES[idx];
    let trail = ANGLES[trail_idx];

    // Format elapsed time
    let secs = elapsed.as_secs_f64();
    let time_str = if secs >= 60.0 {
        format!("{:.0}m{:.0}s", (secs / 60.0).floor(), secs % 60.0)
    } else {
        format!("{:.1}s", secs)
    };

    // Fixed-width parts: "  " + trail + lead + "─ " + name + " · " + ... + "  " + time
    // prefix: 2 + 1 + 1 + 2 = 6, separator: 3, time suffix: 2 + time_str.len()
    let prefix_w = 6;
    let name_w = name.len();
    let sep_w = 3; // " · "
    let time_w = 2 + time_str.len(); // "  " + time
    let fixed_w = prefix_w + name_w + sep_w + time_w;

    let task_max = viewport_width.saturating_sub(fixed_w);
    let task_chars: String = task.chars().take(task_max).collect();
    let task_display = if task.chars().count() > task_max {
        // Replace last char with ellipsis
        let mut truncated: String = task.chars().take(task_max.saturating_sub(1)).collect();
        truncated.push('\u{2026}');
        truncated
    } else {
        task_chars
    };

    lines.push(Line::from(vec![
        Span::raw("  "),
        Span::styled(trail.to_string(), Style::default().fg(theme::ASH_TEXT)),
        Span::styled(
            format!("{}\u{2500} ", lead),
            Style::default()
                .fg(theme::SODIUM)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(name, Style::default().fg(theme::CHALK_MID)),
        Span::styled(" \u{b7} ", Style::default().fg(theme::ASH)),
        Span::styled(task_display, Style::default().fg(theme::ASH_TEXT)),
        Span::styled(
            format!("  {}", time_str),
            Style::default().fg(theme::ASH_MID),
        ),
    ]));
}

// ── Strike zone ─────────────────────────────────────────────────────
// The brand slash rotating in place — a tight 2-char animation where
// the lead character glows sodium and the trail fades to ash. Status
// text follows on the same line. One line, focused, no noise.

fn draw_strike_zone(frame: &mut Frame, app: &App, area: Rect) {
    // Suppress the normal spinner when a delegate is running — the delegate
    // line in the history area serves as the activity indicator.
    if app.active_delegate.is_some() {
        let paragraph = Paragraph::new(Line::from("")).style(theme::history_bg());
        frame.render_widget(paragraph, area);
        return;
    }

    // Four angles of the slash mark rotating in place
    const ANGLES: &[char] = &['╲', '─', '╱', '│'];
    // Divide tick by 2 → 200ms per angle, 800ms full rotation
    let idx = (app.tick / 2) % ANGLES.len();
    let trail_idx = (idx + ANGLES.len() - 1) % ANGLES.len();

    let lead = ANGLES[idx];
    let trail = ANGLES[trail_idx];

    let status = app.status_text.as_deref().unwrap_or("");

    let mut spans = vec![
        Span::raw("  "),
        Span::styled(trail.to_string(), Style::default().fg(theme::ASH_TEXT)),
        Span::styled(
            lead.to_string(),
            Style::default()
                .fg(theme::SODIUM)
                .add_modifier(Modifier::BOLD),
        ),
    ];

    if !status.is_empty() {
        spans.push(Span::styled(
            format!("  {}", status),
            Style::default().fg(theme::ASH_MID),
        ));
    }

    let paragraph = Paragraph::new(Line::from(spans)).style(theme::history_bg());
    frame.render_widget(paragraph, area);
}

fn draw_task_tray(frame: &mut Frame, app: &App, area: Rect) {
    if app.task_tray.is_empty() {
        return;
    }

    let width = area.width as usize;
    let total = app.task_tray.len();
    let completed = app
        .task_tray
        .iter()
        .filter(|t| t.status == "completed" || t.status == "cancelled")
        .count();
    let remaining = total - completed;
    let ratio_str = if remaining == 0 {
        format!("{} done", total)
    } else {
        format!("{} left \u{b7} {} done", remaining, completed)
    };

    // Top border: ┌─ TASKS ─ 3 left · 6 done ──────────┐
    let title = format!(" TASKS \u{2500} {} ", ratio_str);
    let title_w = UnicodeWidthStr::width(title.as_str());
    let fill_w = width.saturating_sub(3 + title_w); // ┌─ + title + ┐

    let top_line = Line::from(vec![
        Span::styled("\u{250c}\u{2500}", Style::default().fg(theme::ASH)),
        Span::styled(
            title,
            Style::default()
                .fg(theme::SODIUM)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("\u{2500}".repeat(fill_w), Style::default().fg(theme::ASH)),
        Span::styled("\u{2510}", Style::default().fg(theme::ASH)),
    ]);

    // Build pills for non-completed tasks
    let active_tasks: Vec<&TaskSnapshot> = app
        .task_tray
        .iter()
        .filter(|t| t.status != "completed" && t.status != "cancelled")
        .collect();

    let inner_w = width.saturating_sub(4); // "│ " + content + " │"
    let two_col = active_tasks.len() >= 4 && inner_w >= TASK_TRAY_TWO_COL_MIN_INNER_WIDTH;

    let format_task_cell = |task: &TaskSnapshot, cell_w: usize| -> (String, Style) {
        let (marker, marker_color, text_color) = match task.status.as_str() {
            "in_progress" => ("\u{25c6}", theme::SODIUM, theme::SODIUM),
            "pending" if task.is_blocked => ("\u{25cb}", theme::ASH, theme::ASH_TEXT),
            "pending" => ("\u{25cb}", theme::ASH_MID, theme::CHALK_DIM),
            _ => ("\u{25cb}", theme::ASH_MID, theme::CHALK_DIM),
        };
        let label = if task.is_blocked && task.status == "pending" {
            format!("{} (blocked)", task.label)
        } else {
            task.label.clone()
        };
        let raw_text = if task.owner.is_empty() {
            label
        } else {
            format!("{} \u{00b7} {}", label, task.owner)
        };

        let body_w = cell_w.saturating_sub(2); // marker + space
        let body = truncate_to_display_width(&raw_text, body_w);
        let mut cell = format!("{} {}", marker, body);
        let pad = cell_w.saturating_sub(UnicodeWidthStr::width(cell.as_str()));
        if pad > 0 {
            cell.push_str(&" ".repeat(pad));
        }

        let style =
            Style::default()
                .fg(text_color)
                .add_modifier(if marker_color == theme::SODIUM {
                    Modifier::BOLD
                } else {
                    Modifier::empty()
                });
        (cell, style)
    };

    // Breathing line above the tray so it doesn't collide with content above
    let mut lines = vec![Line::from(""), top_line];

    if active_tasks.is_empty() {
        // All tasks completed — single row
        let dismiss_text = match app.task_dismiss_remaining() {
            Some(s) if s > 0 => format!("all completed \u{2500} {}s", s),
            Some(_) => "all completed".to_string(),
            None => "all completed".to_string(),
        };
        let dismiss_w = UnicodeWidthStr::width(dismiss_text.as_str());
        lines.push(Line::from(vec![
            Span::styled("\u{2502} ", Style::default().fg(theme::ASH)),
            Span::styled(dismiss_text, Style::default().fg(theme::LICHEN)),
            Span::raw(" ".repeat(inner_w.saturating_sub(dismiss_w))),
            Span::styled(" \u{2502}", Style::default().fg(theme::ASH)),
        ]));
    } else if two_col {
        // Two-column mode on wide terminals, preserving vertical list order down the first column.
        let gap_w = 3usize;
        let left_w = inner_w.saturating_sub(gap_w) / 2;
        let right_w = inner_w.saturating_sub(gap_w + left_w);
        let rows = active_tasks.len().div_ceil(2);

        for row in 0..rows {
            let left_idx = row;
            let right_idx = row + rows;
            let mut spans: Vec<Span> =
                vec![Span::styled("\u{2502} ", Style::default().fg(theme::ASH))];

            let (left_cell, left_style) = format_task_cell(active_tasks[left_idx], left_w);
            spans.push(Span::styled(left_cell, left_style));
            spans.push(Span::raw(" ".repeat(gap_w)));

            if right_idx < active_tasks.len() {
                let (right_cell, right_style) = format_task_cell(active_tasks[right_idx], right_w);
                spans.push(Span::styled(right_cell, right_style));
            } else {
                spans.push(Span::raw(" ".repeat(right_w)));
            }
            spans.push(Span::styled(" \u{2502}", Style::default().fg(theme::ASH)));
            lines.push(Line::from(spans));
        }
    } else {
        // Default: one long vertically aligned list.
        for task in &active_tasks {
            let (cell, style) = format_task_cell(task, inner_w);
            lines.push(Line::from(vec![
                Span::styled("\u{2502} ", Style::default().fg(theme::ASH)),
                Span::styled(cell, style),
                Span::styled(" \u{2502}", Style::default().fg(theme::ASH)),
            ]));
        }
    }

    // Progress bar (2+ tasks) — scribe-line style: heavy ━ for done, light ─ for remaining.
    // Ratio already shown in the title so the bar is purely visual.
    if total >= 2 {
        let bar_w = inner_w;
        let filled = if total > 0 {
            (completed * bar_w) / total
        } else {
            0
        };
        let empty = bar_w.saturating_sub(filled);

        let mut progress_spans = vec![Span::styled("\u{2502} ", Style::default().fg(theme::ASH))];
        if filled > 0 {
            progress_spans.push(Span::styled(
                "\u{2501}".repeat(filled),
                Style::default().fg(theme::SODIUM),
            ));
        }
        if empty > 0 {
            progress_spans.push(Span::styled(
                "\u{2500}".repeat(empty),
                Style::default().fg(theme::ASH),
            ));
        }
        progress_spans.push(Span::styled(" \u{2502}", Style::default().fg(theme::ASH)));

        lines.push(Line::from(progress_spans));
    }

    // Bottom border
    let bottom_fill = width.saturating_sub(2);
    lines.push(Line::from(Span::styled(
        format!("\u{2514}{}\u{2518}", "\u{2500}".repeat(bottom_fill)),
        Style::default().fg(theme::ASH),
    )));

    let paragraph = Paragraph::new(lines).style(theme::history_bg());
    frame.render_widget(paragraph, area);
}

/// Draw the queued message bar (1 line above the input, shown while agent is running).
fn draw_queued_message(frame: &mut Frame, app: &App, area: Rect) {
    let msg = match &app.queued_message {
        Some(m) => m,
        None => return,
    };
    if area.height == 0 {
        return;
    }

    let max_w = area.width.saturating_sub(6) as usize; // " ┆ " prefix + padding
    let collapsed: String = msg
        .chars()
        .map(|c| if c == '\n' { ' ' } else { c })
        .collect();
    let truncated: String = collapsed.chars().take(max_w).collect();
    let display = if collapsed.chars().count() > max_w {
        format!("{}\u{2026}", truncated)
    } else {
        truncated
    };

    let line = Line::from(vec![
        Span::styled(" \u{2506} ", Style::default().fg(theme::ASH)),
        Span::styled(display, Style::default().fg(theme::CHALK_DIM)),
        Span::styled("  \u{232b}", Style::default().fg(theme::ASH)),
    ]);
    frame.render_widget(
        Paragraph::new(line).style(Style::default().bg(theme::FORM_RAISED)),
        area,
    );
}

fn draw_input(frame: &mut Frame, app: &App, area: Rect) {
    let mut lines = Vec::new();

    // Multi-line input rendering with manual character-level wrapping.
    // We pre-wrap each logical line using wrap_line() so that rendering and
    // cursor positioning use identical wrapping logic (no Paragraph::wrap).
    let total_width = area.width as usize;
    let prefix_w = 2; // "❯ " or "  "
    let input_lines: Vec<&str> = app.input.split('\n').collect();
    for (i, logical_line) in input_lines.iter().enumerate() {
        let segments = wrap_line(logical_line, prefix_w, total_width);
        for (j, &(seg_start, seg_end)) in segments.iter().enumerate() {
            let seg_text = &logical_line[seg_start..seg_end];
            if j == 0 {
                // First visual line of this logical line — gets prefix
                if i == 0 {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{} ", theme::PROMPT_CHAR), theme::prompt()),
                        Span::styled(seg_text, theme::user_input()),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled("  ", Style::default().fg(theme::ASH)),
                        Span::styled(seg_text, theme::user_input()),
                    ]));
                }
            } else {
                // Wrapped continuation line — no prefix
                lines.push(Line::from(Span::styled(seg_text, theme::user_input())));
            }
        }
    }

    // Position cursor accounting for visual wrapping
    let clamped_cursor = app.cursor_pos.min(app.input.len());
    let (vis_row, vis_col) = input_cursor_position(&app.input, clamped_cursor, area.width as usize);
    let cursor_abs_row = vis_row;
    let content_h = area.height.saturating_sub(2) as usize; // inside borders

    // Compute scroll offset to keep the cursor visible
    let scroll_offset = if content_h > 0 && cursor_abs_row >= content_h {
        cursor_abs_row - content_h + 1
    } else {
        0
    };

    let input = Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::TOP | Borders::BOTTOM)
                .border_style(theme::input_border()),
        )
        .scroll((scroll_offset as u16, 0));
    frame.render_widget(input, area);

    // Session/cwd on the bottom border, right-aligned
    let session_w = UnicodeWidthStr::width(app.session_name.as_str())
        + 3 // " · "
        + UnicodeWidthStr::width(app.cwd.as_str())
        + 2; // padding
    if session_w + 2 <= area.width as usize {
        let x = area.x + area.width - session_w as u16;
        let y = area.y + area.height - 1;
        let session_area = Rect::new(x, y, session_w as u16, 1);
        let session_line = Line::from(vec![
            Span::styled(" ", Style::default().fg(theme::ASH)),
            Span::styled(
                app.session_name.as_str(),
                Style::default().fg(theme::ASH_MID),
            ),
            Span::styled(" \u{b7} ", Style::default().fg(theme::ASH)),
            Span::styled(app.cwd.as_str(), Style::default().fg(theme::ASH_TEXT)),
            Span::styled(" ", Style::default().fg(theme::ASH)),
        ]);
        frame.render_widget(
            Paragraph::new(session_line).style(theme::history_bg()),
            session_area,
        );
    }

    let cursor_x = area.x + vis_col as u16;
    let cursor_y = area.y + 1 + (cursor_abs_row - scroll_offset) as u16;
    frame.set_cursor_position((cursor_x, cursor_y));
}

/// Draw the autocomplete popup above the input area (slash commands or @path).
fn draw_suggestions(frame: &mut Frame, app: &App, input_area: Rect) {
    if app.suggestions.is_empty() {
        return;
    }

    let is_path = app.suggestion_kind == SuggestionKind::Path;
    let count = app.suggestions.len() as u16;
    let height = count + 2; // +2 for top/bottom border
    let base_width: u16 = if is_path { 50 } else { 40 };
    let width = base_width.min(input_area.width);
    let y = input_area.y.saturating_sub(height);
    let height = height.min(frame.area().bottom().saturating_sub(y));

    let popup_area = Rect::new(input_area.x, y, width, height);

    // Clear the area behind the popup
    frame.render_widget(Clear, popup_area);

    // Column width for the left side (name/path)
    let name_col: usize = if is_path { 34 } else { 12 };

    let items: Vec<Line> = app
        .suggestions
        .iter()
        .enumerate()
        .map(|(i, (cmd, desc))| {
            if i == app.suggestion_idx {
                Line::from(vec![
                    Span::styled(
                        format!(" {:<w$}", cmd, w = name_col),
                        Style::default()
                            .fg(theme::SODIUM)
                            .bg(theme::FORM_RAISED)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        format!(" {}", desc),
                        Style::default().fg(theme::CHALK_MID).bg(theme::FORM_RAISED),
                    ),
                ])
            } else {
                Line::from(vec![
                    Span::styled(
                        format!(" {:<w$}", cmd, w = name_col),
                        Style::default().fg(theme::CHALK_DIM),
                    ),
                    Span::styled(format!(" {}", desc), Style::default().fg(theme::ASH_MID)),
                ])
            }
        })
        .collect();

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::ASH));
    let paragraph = Paragraph::new(items)
        .block(block)
        .style(Style::default().bg(theme::FORM_DEEP));
    frame.render_widget(paragraph, popup_area);
}

/// Draw the session picker popup centered in the history area.
fn draw_session_picker(frame: &mut Frame, app: &App, history_area: Rect) {
    if app.session_picker.is_empty() {
        return;
    }

    let max_visible = 15u16;
    let count = app.session_picker.len() as u16;
    let visible = count.min(max_visible);
    let height = visible + 2; // +2 for borders
    let width = 80u16.min(history_area.width.saturating_sub(4));

    // Center in history area, clamped to frame
    let x = history_area.x + (history_area.width.saturating_sub(width)) / 2;
    let y = history_area.y + (history_area.height.saturating_sub(height)) / 2;
    let height = height.min(frame.area().bottom().saturating_sub(y));
    let popup_area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup_area);

    // If we have more items than visible, scroll the view to keep selection visible
    let scroll_offset = if app.session_picker_idx >= visible as usize {
        app.session_picker_idx - visible as usize + 1
    } else {
        0
    };

    // Layout: " > 8h ago  First user message truncated...  5 msgs "
    let time_col = 10; // fixed width for relative time
    let count_col = 8; // fixed width for "99 msgs"
    // inner_w = width - 2 (borders) - 3 (prefix " > " or "   ") - 2 (gaps around message)
    let inner_w = (width as usize).saturating_sub(2 + 3 + time_col + count_col + 2);

    let items: Vec<Line> = app
        .session_picker
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible as usize)
        .map(|(i, session)| {
            let selected = i == app.session_picker_idx;
            let prefix = if selected { " > " } else { "   " };

            let time_str = session.relative_time();
            let msg_count = format!("{} msgs", session.message_count);

            // Truncate the first message to fit, collapsing newlines
            let preview: String = session
                .first_message
                .chars()
                .map(|c| if c == '\n' { ' ' } else { c })
                .take(inner_w)
                .collect();
            let preview = if session.first_message.chars().count() > inner_w {
                format!("{}\u{2026}", &preview[..preview.len().min(preview.len())])
            } else {
                preview
            };

            let (time_style, msg_style, count_style, prefix_style) = if selected {
                let bg = theme::FORM_RAISED;
                (
                    Style::default().fg(theme::SODIUM).bg(bg),
                    Style::default().fg(theme::CHALK).bg(bg),
                    Style::default().fg(theme::ASH_MID).bg(bg),
                    Style::default()
                        .fg(theme::SODIUM)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                (
                    Style::default().fg(theme::ASH_MID),
                    Style::default().fg(theme::CHALK_DIM),
                    Style::default().fg(theme::ASH),
                    Style::default().fg(theme::CHALK_DIM),
                )
            };

            Line::from(vec![
                Span::styled(prefix, prefix_style),
                Span::styled(format!("{:<tw$}", time_str, tw = time_col), time_style),
                Span::styled(format!("{:<mw$}", preview, mw = inner_w), msg_style),
                Span::styled(format!("{:>cw$} ", msg_count, cw = count_col), count_style),
            ])
        })
        .collect();

    let title = format!(" Sessions ({}) ", app.session_picker.len());
    let block = Block::default()
        .title(Span::styled(title, Style::default().fg(theme::SODIUM)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::ASH));
    let paragraph = Paragraph::new(items)
        .block(block)
        .style(Style::default().bg(theme::FORM_DEEP));
    frame.render_widget(paragraph, popup_area);
}

/// Draw the skill picker popup centered in the history area.
fn draw_skill_picker(frame: &mut Frame, app: &App, history_area: Rect) {
    if app.skill_picker.is_empty() {
        return;
    }

    let max_visible = 15u16;
    let count = app.skill_picker.len() as u16;
    let visible = count.min(max_visible);
    let height = visible + 2; // +2 for borders
    let width = 60u16.min(history_area.width.saturating_sub(4));

    // Center in history area, clamped to frame
    let x = history_area.x + (history_area.width.saturating_sub(width)) / 2;
    let y = history_area.y + (history_area.height.saturating_sub(height)) / 2;
    let height = height.min(frame.area().bottom().saturating_sub(y));
    let popup_area = Rect::new(x, y, width, height);

    frame.render_widget(Clear, popup_area);

    // Scroll to keep selection visible
    let scroll_offset = if app.skill_picker_idx >= visible as usize {
        app.skill_picker_idx - visible as usize + 1
    } else {
        0
    };

    let name_col = 20usize;
    let desc_w = (width as usize).saturating_sub(2 + 3 + name_col + 1); // borders + prefix + name + gap

    let items: Vec<Line> = app
        .skill_picker
        .iter()
        .enumerate()
        .skip(scroll_offset)
        .take(visible as usize)
        .map(|(i, (name, desc))| {
            let selected = i == app.skill_picker_idx;
            let prefix = if selected { " > " } else { "   " };

            let desc_truncated: String = desc.chars().take(desc_w).collect();

            let (name_style, desc_style, prefix_style) = if selected {
                let bg = theme::FORM_RAISED;
                (
                    Style::default()
                        .fg(theme::SODIUM)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                    Style::default().fg(theme::CHALK).bg(bg),
                    Style::default()
                        .fg(theme::SODIUM)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                (
                    Style::default().fg(theme::CHALK_DIM),
                    Style::default().fg(theme::ASH_MID),
                    Style::default().fg(theme::CHALK_DIM),
                )
            };

            Line::from(vec![
                Span::styled(prefix, prefix_style),
                Span::styled(format!("{:<w$}", name, w = name_col), name_style),
                Span::styled(format!(" {}", desc_truncated), desc_style),
            ])
        })
        .collect();

    let title = format!(" Skills ({}) ", app.skill_picker.len());
    let block = Block::default()
        .title(Span::styled(title, Style::default().fg(theme::SODIUM)))
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::ASH));
    let paragraph = Paragraph::new(items)
        .block(block)
        .style(Style::default().bg(theme::FORM_DEEP));
    frame.render_widget(paragraph, popup_area);
}

/// Calculate the height needed for the inline prompt area.
fn prompt_height(app: &App, frame_width: u16) -> u16 {
    let prompt = match &app.prompt {
        Some(p) => p,
        None => return 3, // fallback
    };

    let has_options = !prompt.options.is_empty();
    let inner_w = frame_width.saturating_sub(2) as usize; // border padding

    // Question lines via markdown rendering
    let q_lines = if prompt.question.is_empty() {
        1
    } else {
        markdown::markdown_height(&prompt.question, inner_w)
    };

    let option_count = if has_options {
        prompt.options.len() + 1 // +1 for "Other"
    } else {
        0
    };

    let extra_h: u16 = if prompt.editing_extra || !has_options {
        1 // single input line
    } else {
        0
    };

    let content_h = q_lines as u16
        + if option_count > 0 || extra_h > 0 { 1 } else { 0 } // blank after question (only if more content follows)
        + option_count as u16
        + extra_h
        + 1; // help bar
    let max_h = 20u16; // cap height
    (content_h + 2).min(max_h) // +2 for borders
}

/// Draw the agent prompt inline in the input area (replaces normal input).
fn draw_prompt(frame: &mut Frame, app: &App, area: Rect) {
    let prompt = match &app.prompt {
        Some(p) => p,
        None => return,
    };

    let has_options = !prompt.options.is_empty();
    let inner_w = area.width.saturating_sub(2) as usize;

    let mut lines: Vec<Line> = Vec::new();

    // Question text via markdown rendering
    if !prompt.question.is_empty() {
        let md_lines = markdown::render_markdown(&prompt.question, inner_w);
        // Prefix each line with a left margin
        for md_line in md_lines {
            let mut prefixed = vec![Span::styled(" ", Style::default())];
            prefixed.extend(md_line.spans);
            lines.push(Line::from(prefixed));
        }
    }

    // Blank separator before options/input
    lines.push(Line::from(""));

    // Option list
    if has_options {
        for (i, opt) in prompt.options.iter().enumerate() {
            let selected = i == prompt.selected_idx;
            let marker = if selected { "\u{203a}" } else { " " };
            let max_opt_w = inner_w.saturating_sub(6); // " › 1. " prefix
            let label = format!("{}. {}", i + 1, opt);
            let truncated: String = label.chars().take(max_opt_w).collect();

            if selected {
                lines.push(Line::from(vec![
                    Span::styled(
                        format!(" {} ", marker),
                        Style::default().fg(theme::SODIUM).bg(theme::FORM_RAISED),
                    ),
                    Span::styled(
                        truncated,
                        Style::default()
                            .fg(theme::SODIUM)
                            .bg(theme::FORM_RAISED)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
            } else {
                lines.push(Line::from(Span::styled(
                    format!(" {} {}", marker, truncated),
                    Style::default().fg(theme::CHALK_DIM),
                )));
            }
        }

        // "Other" option — always last
        let other_selected = prompt.selected_idx == prompt.options.len();
        let marker = if other_selected { "\u{203a}" } else { " " };
        if other_selected {
            lines.push(Line::from(vec![
                Span::styled(
                    format!(" {} ", marker),
                    Style::default().fg(theme::SODIUM).bg(theme::FORM_RAISED),
                ),
                Span::styled(
                    "Other",
                    Style::default()
                        .fg(theme::SODIUM)
                        .bg(theme::FORM_RAISED)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
        } else {
            lines.push(Line::from(Span::styled(
                format!(" {} Other", marker),
                Style::default().fg(theme::CHALK_DIM),
            )));
        }
    }

    // Text input line (shown when editing extra or freeform)
    if prompt.editing_extra || !has_options {
        let label = if has_options { "context" } else { "answer" };
        let text_w = inner_w.saturating_sub(4); // " ❯ " prefix
        let text_display: String = prompt.extra_text.chars().take(text_w).collect();
        lines.push(Line::from(vec![
            Span::styled(format!(" {} ", theme::PROMPT_CHAR), theme::prompt()),
            Span::styled(text_display, Style::default().fg(theme::CHALK)),
            Span::styled("\u{2588}", Style::default().fg(theme::SODIUM)),
            Span::styled(format!("  {}", label), Style::default().fg(theme::ASH)),
        ]));
    }

    // Help bar
    let help = if has_options {
        Line::from(vec![
            Span::styled(" \u{2191}\u{2193}", theme::help_key()),
            Span::styled(" select  ", theme::help_desc()),
            Span::styled("\u{23ce}", theme::help_key()),
            Span::styled(" submit  ", theme::help_desc()),
            Span::styled("\u{21e7}\u{21e5}", theme::help_key()),
            Span::styled(" context  ", theme::help_desc()),
            Span::styled("Esc", theme::help_key()),
            Span::styled(" cancel", theme::help_desc()),
        ])
    } else {
        Line::from(vec![
            Span::styled(" \u{23ce}", theme::help_key()),
            Span::styled(" submit  ", theme::help_desc()),
            Span::styled("Esc", theme::help_key()),
            Span::styled(" cancel", theme::help_desc()),
        ])
    };
    lines.push(help);

    let content_h = area.height.saturating_sub(2) as usize;
    let scroll_offset = if lines.len() > content_h {
        lines.len() - content_h
    } else {
        0
    };

    let title = " Agent Question ";
    let block = Block::default()
        .title(Span::styled(
            title,
            Style::default()
                .fg(theme::SODIUM)
                .add_modifier(Modifier::BOLD),
        ))
        .borders(Borders::TOP | Borders::BOTTOM)
        .border_style(Style::default().fg(theme::ASH_LIGHT));
    let paragraph = Paragraph::new(lines)
        .block(block)
        .style(Style::default().bg(theme::FORM_DEEP))
        .scroll((scroll_offset as u16, 0));
    frame.render_widget(paragraph, area);
}

/// Wrap a single logical line into visual line segments, returning byte-offset ranges.
/// `prefix_width` columns are reserved on the first visual line (for the prompt prefix).
/// Continuation (wrapped) visual lines use the full `total_width`.
/// Uses character-level wrapping with proper Unicode width calculation.
fn wrap_line(text: &str, prefix_width: usize, total_width: usize) -> Vec<(usize, usize)> {
    if total_width == 0 {
        return vec![(0, text.len())];
    }
    let first_cap = total_width.saturating_sub(prefix_width);
    let mut result = Vec::new();
    let mut line_start = 0;
    let mut col = 0;
    let mut capacity = first_cap;

    for (byte_idx, ch) in text.char_indices() {
        let w = UnicodeWidthChar::width(ch).unwrap_or(0);
        if col + w > capacity && col > 0 {
            result.push((line_start, byte_idx));
            line_start = byte_idx;
            col = w;
            capacity = total_width;
        } else {
            col += w;
        }
    }
    result.push((line_start, text.len()));
    result
}

/// Count total visual lines the input text occupies, accounting for wrapping.
/// `width` is the total rendering width (prefix is accounted for internally).
fn input_visual_lines(input: &str, width: usize) -> usize {
    if width == 0 {
        return 1;
    }
    let prefix_w = 2; // "❯ " or "  "
    let mut total = 0;
    for line in input.split('\n') {
        total += wrap_line(line, prefix_w, width).len();
    }
    total.max(1)
}

/// Compute the visual (row, col) of the cursor in the input, accounting for wrapping.
/// `full_width` is the total rendering width; the 2-column prefix is handled internally.
fn input_cursor_position(input: &str, cursor_pos: usize, full_width: usize) -> (usize, usize) {
    let prefix_w = 2usize; // "❯ " or "  "
    let mut vis_row = 0usize;
    let mut byte_offset = 0usize;

    for logical_line in input.split('\n') {
        let line_end = byte_offset + logical_line.len();
        let segments = wrap_line(logical_line, prefix_w, full_width);

        if cursor_pos <= line_end {
            let cursor_in_line = cursor_pos - byte_offset;
            for (i, &(seg_start, seg_end)) in segments.iter().enumerate() {
                let is_last = i == segments.len() - 1;
                if cursor_in_line >= seg_start && (cursor_in_line < seg_end || is_last) {
                    let text_before = &logical_line[seg_start..cursor_in_line];
                    let col = UnicodeWidthStr::width(text_before);
                    return (vis_row, if i == 0 { col + prefix_w } else { col });
                }
                vis_row += 1;
            }
            return (vis_row, prefix_w);
        }

        vis_row += segments.len();
        byte_offset = line_end + 1; // +1 for '\n'
    }

    (vis_row.saturating_sub(1), prefix_w)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::App;

    fn line_text(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    #[test]
    fn plan_block_lines_stay_within_box_width() {
        let mut lines = Vec::new();
        let content = "A long paragraph that should wrap inside the plan box without escaping the right border.";
        let width = 32usize;
        render_plan_block(content, &mut lines, width);

        assert!(lines.len() >= 3, "expected top/content/bottom lines");

        // Top and bottom borders.
        assert!(line_text(&lines[0]).starts_with("\u{250c}"));
        assert!(line_text(lines.last().expect("bottom line")).starts_with("\u{2514}"));

        // Every rendered line should fit the viewport width exactly.
        for line in &lines {
            assert_eq!(line.width(), width, "line exceeded/fell short of box width");
        }

        // Middle content rows must have both left and right borders.
        for line in &lines[1..lines.len() - 1] {
            let text = line_text(line);
            assert!(text.starts_with("\u{2502} "), "missing left plan border");
            assert!(text.ends_with(" \u{2502}"), "missing right plan border");
        }
    }

    #[test]
    fn history_viewport_height_respects_dynamic_input_and_trays() {
        let mut app = App::new("model".into(), "session".into(), None);
        app.blocks.clear(); // avoid splash-specific influence on expectations
        app.input = "line1\nline2\nline3".into();
        app.queued_message = Some("queued".into());
        app.task_tray.push(crate::app::TaskSnapshot {
            id: "t1".into(),
            label: "Task".into(),
            status: "pending".into(),
            owner: String::new(),
            is_blocked: false,
        });

        let fw = 100u16;
        let fh = 40u16;
        let expected_overhead = 1u16 // status bar
            + if app.running { 1 } else { 0 }
            + app.task_tray_height(fw)
            + 1u16 // queued bar
            + input_height(&app, fw);

        let got = history_viewport_height(&app, fw, fh);
        assert_eq!(got, fh.saturating_sub(expected_overhead) as usize);
    }
}
