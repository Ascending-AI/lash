use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use lash_core::plugin::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin,
    ToolPresentationInput, ToolResultProjectionContext,
};
use lash_core::{
    ToolCallOutcome, ToolValue, facade_support::ModelToolReturn,
    facade_support::ModelToolReturnPart, facade_support::PluginStack,
};

/// Boxed future for the recursive projection helpers: retaining a truncated
/// output as a session artifact is async, and the JSON walker recurses.
type ProjectionFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

const APPROX_BYTES_PER_TOKEN: usize = 4;
pub const DEFAULT_TOOL_OUTPUT_BUDGET_LIMIT_BYTES: usize = 16 * 1024;
pub const DEFAULT_TOOL_OUTPUT_BUDGET_MAX_LINES: usize = 400;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolOutputBudgetMode {
    Bytes,
    Tokens,
}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct ToolOutputBudgetConfig {
    pub mode: ToolOutputBudgetMode,
    pub limit: usize,
    pub max_lines: usize,
    /// Percentage of the retained budget given to the head of a truncated
    /// output; the tail keeps the remainder. 50 splits evenly, 100 keeps only
    /// the head, and 0 keeps only the tail. Values above 100 are refused when
    /// the plugin factory is constructed.
    pub head_share_percent: u8,
    /// `false` retains nothing beyond the truncated preview (today's default).
    /// When `true`, a truncated output's full text is retained once as a
    /// durable session attachment through the presentation boundary's journaled
    /// artifact capability, and the truncation hint names the attachment.
    pub retain_full_output: bool,
}

impl Default for ToolOutputBudgetConfig {
    fn default() -> Self {
        Self {
            mode: ToolOutputBudgetMode::Bytes,
            limit: DEFAULT_TOOL_OUTPUT_BUDGET_LIMIT_BYTES,
            max_lines: DEFAULT_TOOL_OUTPUT_BUDGET_MAX_LINES,
            head_share_percent: 50,
            retain_full_output: false,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum TruncationUnit {
    Bytes,
    Tokens,
}

impl TruncationUnit {
    fn label(self) -> &'static str {
        match self {
            TruncationUnit::Bytes => "bytes",
            TruncationUnit::Tokens => "tokens",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct WindowedTruncation<'a> {
    /// Maximum number of lines retained across both preview windows.
    pub max_lines: usize,
    /// Maximum number of bytes retained across both preview windows.
    pub max_bytes: usize,
    /// Percentage of the retained budget given to the head window.
    pub head_share_percent: u8,
    /// The unit reported in the byte-budget truncation marker.
    pub unit: TruncationUnit,
    /// Hint text emitted between the marker and the tail window, explaining
    /// the truncation and where the full output is.
    pub hint: &'a str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Budget {
    pub max_bytes: usize,
    pub max_lines: usize,
    pub head_share_percent: u8,
    pub unit: TruncationUnit,
}

impl Budget {
    pub fn from_config(config: &ToolOutputBudgetConfig) -> Self {
        let (max_bytes, unit) = match config.mode {
            ToolOutputBudgetMode::Bytes => (config.limit, TruncationUnit::Bytes),
            ToolOutputBudgetMode::Tokens => (
                config.limit.saturating_mul(APPROX_BYTES_PER_TOKEN),
                TruncationUnit::Tokens,
            ),
        };
        Self {
            max_bytes,
            max_lines: config.max_lines,
            head_share_percent: config.head_share_percent.min(100),
            unit,
        }
    }
}

impl From<&ToolOutputBudgetConfig> for Budget {
    fn from(config: &ToolOutputBudgetConfig) -> Self {
        Self::from_config(config)
    }
}

impl From<ToolOutputBudgetConfig> for Budget {
    fn from(config: ToolOutputBudgetConfig) -> Self {
        Self::from_config(&config)
    }
}

/// The canonical head+tail-window + byte-cap truncation core.
///
/// Returns `text` unchanged when it already fits within `max_lines` and
/// `max_bytes`. Otherwise keeps a head and a tail preview window — split by
/// `head_share_percent` — wrapped around a single
/// `...N <unit> truncated...` marker plus the caller-supplied `hint`.
/// Keeping both ends matters because failures cluster at the start of build
/// output and at the end of test runs; no single direction is safe for
/// every tool.
///
/// A line that is itself larger than its window's byte budget is truncated
/// at a UTF-8 char boundary rather than dropped, so over-long lines never
/// silently disappear and the function never panics on multi-byte text.
pub(crate) fn truncate_windowed(text: &str, opts: &WindowedTruncation) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total_bytes = text.len();
    if lines.len() <= opts.max_lines && total_bytes <= opts.max_bytes {
        return text.to_string();
    }

    let share = usize::from(opts.head_share_percent.min(100));
    let head_max_bytes = opts.max_bytes.saturating_mul(share) / 100;
    let head_max_lines = opts.max_lines.saturating_mul(share) / 100;

    let head = collect_window(
        lines.iter().take(head_max_lines).copied(),
        head_max_bytes,
        End::Head,
    );
    let tail = collect_window(
        lines
            .iter()
            .rev()
            .take(opts.max_lines.saturating_sub(head_max_lines))
            .copied(),
        opts.max_bytes.saturating_sub(head_max_bytes),
        End::Tail,
    );

    let head_preview = head.lines.join("\n");
    let tail_line_count = tail.lines.len();
    let mut tail_lines = tail.lines;
    tail_lines.reverse();
    let tail_preview = tail_lines.join("\n");

    let retained_bytes = head_preview.len().saturating_add(tail_preview.len());
    let removed_bytes = total_bytes.saturating_sub(retained_bytes);
    let (removed, unit) = if head.hit_budget || tail.hit_budget {
        let removed = match opts.unit {
            TruncationUnit::Bytes => u64::try_from(removed_bytes).unwrap_or(u64::MAX),
            TruncationUnit::Tokens => approx_tokens_from_byte_count(removed_bytes),
        };
        (removed, opts.unit.label())
    } else {
        let removed_lines = lines
            .len()
            .saturating_sub(head.lines.len())
            .saturating_sub(tail_line_count);
        (u64::try_from(removed_lines).unwrap_or(u64::MAX), "lines")
    };

    let marker = truncation_marker(removed, unit);
    [head_preview, marker, opts.hint.to_string(), tail_preview]
        .into_iter()
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// One retained window of a truncated output.
struct Window {
    /// Retained lines in collection order (reversed for a tail window).
    lines: Vec<String>,
    /// Whether the byte budget, rather than the line cap, stopped the window.
    hit_budget: bool,
}

/// Which end of the output a window collects from. Over-long lines are cut
/// at a char boundary on the outer edge: a head window keeps the line's
/// start, a tail window keeps its end.
#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Head,
    Tail,
}

fn collect_window<'a>(lines: impl Iterator<Item = &'a str>, max_bytes: usize, end: End) -> Window {
    let mut kept: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let mut hit_budget = false;
    for line in lines {
        // `separator` accounts for the `\n` re-joined between lines; the
        // first retained line carries no separator.
        let separator = usize::from(!kept.is_empty());
        let remaining = max_bytes.saturating_sub(bytes + separator);
        if line.len() + separator <= max_bytes.saturating_sub(bytes) {
            kept.push(line.to_string());
            bytes += line.len() + separator;
        } else if kept.is_empty() && remaining > 0 {
            // A lone line longer than the whole window budget: truncate it
            // at a char boundary instead of dropping it entirely.
            let kept_text = match end {
                End::Head => &line[..char_floor(line, remaining)],
                End::Tail => &line[char_ceil(line, line.len().saturating_sub(remaining))..],
            };
            if kept_text.is_empty() {
                hit_budget = true;
                break;
            }
            kept.push(kept_text.to_string());
            hit_budget = true;
            break;
        } else {
            hit_budget = true;
            break;
        }
    }
    Window {
        lines: kept,
        hit_budget,
    }
}

fn truncation_marker(removed: u64, unit: &str) -> String {
    format!("...{removed} {unit} truncated...")
}

/// Largest byte offset `<= max` that lands on a UTF-8 char boundary.
fn char_floor(text: &str, max: usize) -> usize {
    if max >= text.len() {
        return text.len();
    }
    let mut cut = max;
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    cut
}

/// Smallest byte offset `>= min` that lands on a UTF-8 char boundary.
fn char_ceil(text: &str, min: usize) -> usize {
    let mut cut = min;
    while cut < text.len() && !text.is_char_boundary(cut) {
        cut += 1;
    }
    cut
}

pub struct ToolOutputBudgetPluginFactory {
    budget: Budget,
    retain_full_output: bool,
}

impl ToolOutputBudgetPluginFactory {
    pub fn new(config: ToolOutputBudgetConfig) -> Result<Self, PluginError> {
        if config.head_share_percent > 100 {
            return Err(PluginError::Registration(format!(
                "tool_output_budget head_share_percent must be within 0..=100, got {}",
                config.head_share_percent
            )));
        }
        let budget = Budget::from(&config);
        Ok(Self {
            budget,
            retain_full_output: config.retain_full_output,
        })
    }
}

impl Default for ToolOutputBudgetPluginFactory {
    fn default() -> Self {
        // The built-in config is always valid, so construction cannot fail.
        let config = ToolOutputBudgetConfig::default();
        Self {
            budget: Budget::from(&config),
            retain_full_output: config.retain_full_output,
        }
    }
}

pub fn tool_output_budget_stack() -> PluginStack {
    let mut stack = PluginStack::new();
    stack.push(Arc::new(ToolOutputBudgetPluginFactory::default()));
    stack
}

impl PluginFactory for ToolOutputBudgetPluginFactory {
    fn id(&self) -> &'static str {
        "tool_output_budget"
    }

    fn build(&self, _ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(ToolOutputBudgetPlugin {
            budget: self.budget,
            retain_full_output: self.retain_full_output,
        }))
    }
}

struct ToolOutputBudgetPlugin {
    budget: Budget,
    retain_full_output: bool,
}

impl SessionPlugin for ToolOutputBudgetPlugin {
    fn id(&self) -> &'static str {
        "tool_output_budget"
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        register_presentation_step(reg, self.budget, self.retain_full_output);
        Ok(())
    }
}

fn register_presentation_step(reg: &mut PluginRegistrar, budget: Budget, retain_full_output: bool) {
    reg.tool_results().presentation_step(Arc::new(move |input| {
        Box::pin(async move { present_tool_result(&budget, retain_full_output, input).await })
    }));
}

async fn present_tool_result(
    budget: &Budget,
    retain_full_output: bool,
    input: ToolPresentationInput,
) -> Result<ModelToolReturn, PluginError> {
    let ctx = &input.context;
    let parts = project_model_parts(budget, retain_full_output, ctx).await?;
    Ok(ModelToolReturn {
        call_id: ctx.call_id.clone(),
        tool_name: ctx.tool_name.clone(),
        parts,
        attachment_notices: input.previous.attachment_notices,
    })
}

async fn project_model_parts(
    budget: &Budget,
    retain_full_output: bool,
    ctx: &ToolResultProjectionContext,
) -> Result<Vec<ModelToolReturnPart>, PluginError> {
    if ctx.tool_name == "batch" {
        let value = project_batch_value(budget, retain_full_output, ctx).await?;
        return Ok(vec![ModelToolReturnPart::text(
            render_projected_model_value(&value),
        )]);
    }

    Ok(match &ctx.output.outcome {
        ToolCallOutcome::Success(value) => {
            project_tool_value_parts(budget, retain_full_output, ctx, value).await
        }
        ToolCallOutcome::Failure(failure) => {
            let mut parts = vec![ModelToolReturnPart::text(
                lash_core::session_model::format_tool_output_content(&ctx.output),
            )];
            if let Some(raw) = &failure.raw {
                parts.extend(
                    raw.attachments()
                        .into_iter()
                        .map(ModelToolReturnPart::Attachment),
                );
            }
            parts
        }
        ToolCallOutcome::Cancelled(cancellation) => {
            let mut parts = vec![ModelToolReturnPart::text(
                lash_core::session_model::format_tool_output_content(&ctx.output),
            )];
            if let Some(raw) = &cancellation.raw {
                parts.extend(
                    raw.attachments()
                        .into_iter()
                        .map(ModelToolReturnPart::Attachment),
                );
            }
            parts
        }
    })
}

fn render_projected_model_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
    }
}

async fn project_tool_value_parts(
    budget: &Budget,
    retain_full_output: bool,
    ctx: &ToolResultProjectionContext,
    value: &ToolValue,
) -> Vec<ModelToolReturnPart> {
    let mut parts = Vec::new();
    match value {
        ToolValue::String(text) => parts.push(ModelToolReturnPart::text(
            project_text(text, budget, ctx, retain_full_output).await,
        )),
        ToolValue::Attachment(reference) => {
            parts.push(ModelToolReturnPart::Attachment(reference.clone()));
        }
        ToolValue::UntrustedJson(value) => parts.push(ModelToolReturnPart::text(
            project_text(
                &render_projected_model_value(value),
                budget,
                ctx,
                retain_full_output,
            )
            .await,
        )),
        ToolValue::Null
        | ToolValue::Bool(_)
        | ToolValue::Number(_)
        | ToolValue::Array(_)
        | ToolValue::Object(_) => {
            push_projected_tool_value_parts(value, &mut parts, budget, retain_full_output, ctx)
                .await;
        }
    }
    parts
}

fn push_projected_tool_value_parts<'a>(
    value: &'a ToolValue,
    parts: &'a mut Vec<ModelToolReturnPart>,
    budget: &'a Budget,
    retain_full_output: bool,
    ctx: &'a ToolResultProjectionContext,
) -> ProjectionFuture<'a, ()> {
    Box::pin(async move {
        match value {
            ToolValue::Null => push_text_part(parts, "null"),
            ToolValue::Bool(value) => push_text_part(parts, value.to_string()),
            ToolValue::Number(value) => push_text_part(parts, value.to_string()),
            ToolValue::String(text) => push_text_part(
                parts,
                serde_json::to_string(&project_text(text, budget, ctx, retain_full_output).await)
                    .unwrap_or_else(|_| "\"\"".to_string()),
            ),
            ToolValue::Attachment(reference) => {
                parts.push(ModelToolReturnPart::Attachment(reference.clone()));
            }
            ToolValue::UntrustedJson(value) => push_text_part(
                parts,
                project_text(
                    &serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
                    budget,
                    ctx,
                    retain_full_output,
                )
                .await,
            ),
            ToolValue::Array(items) => {
                push_text_part(parts, "[");
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        push_text_part(parts, ",");
                    }
                    push_projected_tool_value_parts(item, parts, budget, retain_full_output, ctx)
                        .await;
                }
                push_text_part(parts, "]");
            }
            ToolValue::Object(map) => {
                push_text_part(parts, "{");
                for (index, (key, value)) in map.iter().enumerate() {
                    if index > 0 {
                        push_text_part(parts, ",");
                    }
                    push_text_part(
                        parts,
                        serde_json::to_string(key).unwrap_or_else(|_| "\"\"".to_string()),
                    );
                    push_text_part(parts, ":");
                    push_projected_tool_value_parts(value, parts, budget, retain_full_output, ctx)
                        .await;
                }
                push_text_part(parts, "}");
            }
        }
    })
}

fn push_text_part(parts: &mut Vec<ModelToolReturnPart>, text: impl Into<String>) {
    let text = text.into();
    if text.is_empty() {
        return;
    }
    if let Some(ModelToolReturnPart::Text { text: existing }) = parts.last_mut() {
        existing.push_str(&text);
    } else {
        parts.push(ModelToolReturnPart::text(text));
    }
}

async fn project_text(
    text: &str,
    budget: &Budget,
    ctx: &ToolResultProjectionContext,
    retain_full_output: bool,
) -> String {
    if !needs_truncation(text, budget) {
        return text.to_string();
    }
    let hint = truncation_hint(ctx, text, retain_full_output).await;
    truncate_text_with_hint(text, budget, hint)
}

fn needs_truncation(text: &str, budget: &Budget) -> bool {
    text.lines().count() > budget.max_lines || text.len() > budget.max_bytes
}

fn truncate_text_with_hint(text: &str, budget: &Budget, hint: String) -> String {
    if text.is_empty() {
        return String::new();
    }
    if budget.max_bytes == 0 {
        return format_zero_budget_marker(budget.unit, text);
    }
    truncate_windowed(
        text,
        &WindowedTruncation {
            max_lines: budget.max_lines,
            max_bytes: budget.max_bytes,
            head_share_percent: budget.head_share_percent,
            unit: budget.unit,
            hint: &hint,
        },
    )
}

fn format_zero_budget_marker(unit: TruncationUnit, text: &str) -> String {
    let removed = match unit {
        TruncationUnit::Bytes => u64::try_from(text.len()).unwrap_or(u64::MAX),
        TruncationUnit::Tokens => approx_tokens_from_byte_count(text.len()),
    };
    truncation_marker(removed, unit.label())
}

fn approx_tokens_from_byte_count(bytes: usize) -> u64 {
    let bytes = bytes as u64;
    bytes.saturating_add((APPROX_BYTES_PER_TOKEN as u64).saturating_sub(1))
        / (APPROX_BYTES_PER_TOKEN as u64)
}

/// The truncation hint: a tool-supplied `full_output_path` wins; with
/// `retain_full_output` the full text is journaled once as a session artifact
/// and the hint names the recorded attachment — never a filesystem path.
async fn truncation_hint(
    ctx: &ToolResultProjectionContext,
    text: &str,
    retain_full_output: bool,
) -> String {
    if let Some(path) = existing_tool_output_path(ctx) {
        return format!(
            "The tool output was truncated. Full output saved to: {}\nUse the shell tool or host-provided file access to inspect specific sections instead of reading the whole file at once.",
            path.display()
        );
    }
    if retain_full_output {
        let label = retained_output_label(ctx);
        if let Ok(reference) = ctx.artifacts.retain_text(&label, text).await {
            return format!(
                "The tool output was truncated. Full output retained as attachment {} ({}); read it with host-provided attachment access to inspect specific sections instead of reading the whole output at once.",
                reference.id,
                reference.label.as_deref().unwrap_or(label.as_str()),
            );
        }
    }
    "The tool output was truncated. Re-run the tool with narrower arguments, or use the shell tool or host-provided file access to inspect a smaller section.".to_string()
}

fn retained_output_label(ctx: &ToolResultProjectionContext) -> String {
    format!("{} full tool output", ctx.tool_name)
}

fn existing_tool_output_path(ctx: &ToolResultProjectionContext) -> Option<PathBuf> {
    ctx.output
        .value_for_projection()
        .get("full_output_path")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
}

async fn project_batch_value(
    budget: &Budget,
    retain_full_output: bool,
    ctx: &ToolResultProjectionContext,
) -> Result<serde_json::Value, PluginError> {
    let value = ctx.output.value_for_projection();
    let Some(map) = value.as_object() else {
        return Ok(project_json_value(&value, budget, retain_full_output, ctx).await);
    };

    let mut projected = serde_json::Map::new();

    if let Some(items) = map.get("results").and_then(|value| value.as_array()) {
        let mut results = Vec::with_capacity(items.len());
        for item in items {
            results.push(project_batch_child_value(item, budget, retain_full_output, ctx).await?);
        }
        projected.insert("results".to_string(), serde_json::Value::Array(results));
    } else {
        projected.insert("results".to_string(), serde_json::Value::Array(Vec::new()));
    }
    Ok(serde_json::Value::Object(projected))
}

async fn project_batch_child_value(
    item: &serde_json::Value,
    budget: &Budget,
    retain_full_output: bool,
    ctx: &ToolResultProjectionContext,
) -> Result<serde_json::Value, PluginError> {
    let row = serde_json::from_value::<lash_protocol_standard::BatchResultRow>(item.clone())
        .map_err(|error| PluginError::Session(format!("invalid batch result row: {error}")))?;
    let child_value = row.value().clone();
    let child_args = batch_child_args(&ctx.args, row.index);

    let projected_child = if row.tool == "batch" || !row.success {
        project_json_value(&child_value, budget, retain_full_output, ctx).await
    } else {
        let child_ctx = ToolResultProjectionContext {
            session_id: ctx.session_id.clone(),
            call_id: format!("{}.{}", ctx.call_id, row.index),
            tool_name: row.tool.clone(),
            args: child_args,
            output: lash_core::ToolCallOutput::success(child_value.clone()),
            duration_ms: row.duration_ms,
            artifacts: Arc::clone(&ctx.artifacts),
        };
        let parts = Box::pin(project_model_parts(budget, retain_full_output, &child_ctx)).await?;
        let rendered = render_model_return_parts(&parts);
        rendered
            .parse::<serde_json::Value>()
            .unwrap_or(serde_json::Value::String(rendered))
    };

    let mut projected = serde_json::Map::new();
    projected.insert("index".to_string(), serde_json::json!(row.index));
    projected.insert("tool".to_string(), serde_json::json!(row.tool));
    projected.insert("success".to_string(), serde_json::json!(row.success));
    projected.insert(
        "duration_ms".to_string(),
        serde_json::json!(row.duration_ms),
    );
    projected.insert(
        if row.success {
            "result".to_string()
        } else {
            "error".to_string()
        },
        projected_child,
    );
    Ok(serde_json::Value::Object(projected))
}

fn render_model_return_parts(parts: &[ModelToolReturnPart]) -> String {
    let mut rendered = String::new();
    for part in parts {
        match part {
            ModelToolReturnPart::Text { text } => rendered.push_str(text),
            ModelToolReturnPart::Attachment(source) => {
                rendered.push_str("[Attachment: ");
                match source {
                    lash_core::AttachmentSource::Stored { attachment_ref } => rendered.push_str(
                        attachment_ref
                            .label
                            .as_deref()
                            .unwrap_or_else(|| attachment_ref.id.as_str()),
                    ),
                    lash_core::AttachmentSource::Inline { media_type, .. } => {
                        rendered.push_str(media_type.as_str())
                    }
                    lash_core::AttachmentSource::ExternalUrl { url, .. } => rendered.push_str(url),
                    lash_core::AttachmentSource::ProviderFile { id, .. } => rendered.push_str(id),
                }
                rendered.push(']');
            }
        }
    }
    rendered
}

fn project_json_value<'a>(
    value: &'a serde_json::Value,
    budget: &'a Budget,
    retain_full_output: bool,
    ctx: &'a ToolResultProjectionContext,
) -> ProjectionFuture<'a, serde_json::Value> {
    Box::pin(async move {
        match value {
            serde_json::Value::String(text) => {
                serde_json::Value::String(project_text(text, budget, ctx, retain_full_output).await)
            }
            serde_json::Value::Array(items) => {
                let mut projected = Vec::with_capacity(items.len());
                for item in items {
                    projected.push(project_json_value(item, budget, retain_full_output, ctx).await);
                }
                serde_json::Value::Array(projected)
            }
            serde_json::Value::Object(map) => {
                let mut projected = serde_json::Map::with_capacity(map.len());
                for (key, value) in map {
                    projected.insert(
                        key.clone(),
                        project_json_value(value, budget, retain_full_output, ctx).await,
                    );
                }
                serde_json::Value::Object(projected)
            }
            other => other.clone(),
        }
    })
}

fn batch_child_args(batch_args: &serde_json::Value, index: usize) -> serde_json::Value {
    batch_args
        .get("tool_calls")
        .and_then(|value| value.as_array())
        .and_then(|items| items.get(index))
        .and_then(|value| value.get("parameters"))
        .cloned()
        .unwrap_or_else(|| serde_json::Value::Object(Default::default()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_sansio::SessionId;
    use serde_json::json;

    fn test_context(
        tool_name: &str,
        args: serde_json::Value,
        output: serde_json::Value,
    ) -> ToolResultProjectionContext {
        test_context_with_artifacts(
            tool_name,
            args,
            output,
            Arc::new(lash_core::plugin::NoPresentationArtifacts),
        )
    }

    fn test_context_with_artifacts(
        tool_name: &str,
        args: serde_json::Value,
        output: serde_json::Value,
        artifacts: Arc<dyn lash_core::plugin::ToolPresentationArtifacts>,
    ) -> ToolResultProjectionContext {
        ToolResultProjectionContext {
            session_id: SessionId::from("root"),
            call_id: "call".to_string(),
            tool_name: tool_name.to_string(),
            args,
            output: lash_core::ToolCallOutput::success(output),
            duration_ms: 1,
            artifacts,
        }
    }

    async fn present_tool_result(
        budget: &Budget,
        ctx: ToolResultProjectionContext,
    ) -> Result<ModelToolReturn, PluginError> {
        let parts = project_model_parts(budget, false, &ctx).await?;
        Ok(ModelToolReturn {
            call_id: ctx.call_id.clone(),
            tool_name: ctx.tool_name.clone(),
            parts,
            attachment_notices: Vec::new(),
        })
    }

    /// A `ToolPresentationArtifacts` test double: records each retention and
    /// answers a deterministic content reference.
    #[derive(Default)]
    struct RecordingArtifacts {
        retained: std::sync::Mutex<Vec<(String, String)>>,
    }

    impl RecordingArtifacts {
        fn retained(&self) -> Vec<(String, String)> {
            self.retained.lock().unwrap().clone()
        }
    }

    impl lash_core::plugin::ToolPresentationArtifacts for RecordingArtifacts {
        fn retain_text<'a>(
            &'a self,
            label: &'a str,
            text: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<lash_core::AttachmentRef, PluginError>> + Send + 'a>>
        {
            Box::pin(async move {
                self.retained
                    .lock()
                    .unwrap()
                    .push((label.to_string(), text.to_string()));
                Ok(lash_core::AttachmentRef::new(
                    lash_core::AttachmentId::parse("att-retained").expect("attachment id"),
                    lash_core::MediaType::parse("text/plain").expect("media type"),
                    text.len() as u64,
                    None,
                    Some(label.to_string()),
                ))
            })
        }
    }

    #[test]
    fn windowed_truncation_truncates_over_long_single_line_instead_of_dropping_it() {
        // A single line longer than the whole byte budget must be cut at a
        // char boundary on both ends, not dropped.
        let line = "x".repeat(1000);
        let got = truncate_windowed(
            &line,
            &WindowedTruncation {
                max_lines: 400,
                max_bytes: 64,
                head_share_percent: 50,
                unit: TruncationUnit::Bytes,
                hint: "hint",
            },
        );
        let head = got.split("\n\n...").next().expect("head preview");
        let tail = got.rsplit("\n\n").next().expect("tail preview");
        assert_eq!(head.len(), 32, "head gets half the budget: {got:?}");
        assert_eq!(tail.len(), 32, "tail gets the other half: {got:?}");
        assert!(got.contains("bytes truncated"));
    }

    #[test]
    fn windowed_truncation_never_splits_a_multibyte_char() {
        // Both budgets land mid-way through a 3-byte char; each window must
        // back off to a boundary rather than panic or emit invalid UTF-8.
        let line = "★".repeat(100); // each '★' is 3 bytes
        let got = truncate_windowed(
            &line,
            &WindowedTruncation {
                max_lines: 400,
                max_bytes: 10, // 5 bytes per window, not a multiple of 3
                head_share_percent: 50,
                unit: TruncationUnit::Bytes,
                hint: "hint",
            },
        );
        let head = got.split("\n\n...").next().expect("head preview");
        let tail = got.rsplit("\n\n").next().expect("tail preview");
        for (name, window) in [("head", head), ("tail", tail)] {
            assert!(!window.is_empty(), "{name} must not be empty: {got:?}");
            assert!(
                window.chars().all(|c| c == '★'),
                "{name} keeps only whole chars: {got:?}"
            );
            assert_eq!(
                window.len() % 3,
                0,
                "{name} must cut on a char boundary: {got:?}"
            );
            assert!(window.len() <= 5);
        }
    }

    #[test]
    fn windowed_truncation_keeps_both_ends_around_one_marker() {
        // Failures cluster at the start of builds and at the end of test
        // runs; either error line must survive regardless of tool name.
        let text = "error: first failure is at the top\n".to_string()
            + &"ok\n".repeat(50)
            + "test result: FAILED";
        let got = truncate_windowed(
            &text,
            &WindowedTruncation {
                max_lines: 4,
                max_bytes: 80,
                head_share_percent: 50,
                unit: TruncationUnit::Bytes,
                hint: "hint",
            },
        );
        assert_eq!(
            got.matches("truncated").count(),
            1,
            "exactly one marker: {got}"
        );
        assert!(got.contains("error: first failure is at the top"), "{got}");
        assert!(got.contains("test result: FAILED"), "{got}");
    }

    #[test]
    fn windowed_truncation_marker_counts_the_real_omitted_amount() {
        let text = "a".repeat(100);
        let got = truncate_windowed(
            &text,
            &WindowedTruncation {
                max_lines: 400,
                max_bytes: 40,
                head_share_percent: 50,
                unit: TruncationUnit::Bytes,
                hint: "hint",
            },
        );
        assert!(got.contains("...60 bytes truncated..."), "{got}");
    }

    #[test]
    fn windowed_truncation_head_share_zero_keeps_tail_only() {
        let text = "start\n".to_string() + &"filler\n".repeat(20) + "end";
        let got = truncate_windowed(
            &text,
            &WindowedTruncation {
                max_lines: 2,
                max_bytes: 1024,
                head_share_percent: 0,
                unit: TruncationUnit::Bytes,
                hint: "hint",
            },
        );
        assert!(got.ends_with("end"), "{got}");
        assert!(!got.contains("start"), "{got}");
    }

    #[test]
    fn windowed_truncation_head_share_hundred_keeps_head_only() {
        let text = "start\n".to_string() + &"filler\n".repeat(20) + "end";
        let got = truncate_windowed(
            &text,
            &WindowedTruncation {
                max_lines: 2,
                max_bytes: 1024,
                head_share_percent: 100,
                unit: TruncationUnit::Bytes,
                hint: "hint",
            },
        );
        assert!(got.starts_with("start"), "{got}");
        assert!(!got.contains("\nend"), "{got}");
    }

    #[test]
    fn windowed_truncation_returns_input_unchanged_when_within_budget() {
        let text = "a\nb\nc";
        let got = truncate_windowed(
            text,
            &WindowedTruncation {
                max_lines: 400,
                max_bytes: 1024,
                head_share_percent: 50,
                unit: TruncationUnit::Bytes,
                hint: "hint",
            },
        );
        assert_eq!(got, text);
    }

    #[tokio::test]
    async fn truncates_strings_with_terminal_style_marker() {
        let config = ToolOutputBudgetConfig {
            mode: ToolOutputBudgetMode::Tokens,
            limit: 5,
            max_lines: DEFAULT_TOOL_OUTPUT_BUDGET_MAX_LINES,
            head_share_percent: 50,
            retain_full_output: false,
        };
        let got = project_text(
            "this is an example of a long output that should be truncated",
            &Budget::from(&config),
            &test_context("grep", json!({}), json!("unused")),
            false,
        )
        .await;
        assert!(got.contains("tokens truncated"));
        assert!(got.contains("Re-run the tool with narrower arguments"));
        assert!(!got.contains("Full output saved to:"));
    }

    #[tokio::test]
    async fn default_config_truncates_without_retaining_an_artifact() {
        let artifacts = Arc::new(RecordingArtifacts::default());
        let ctx =
            test_context_with_artifacts("grep", json!({}), json!("unused"), artifacts.clone());
        let text = "x".repeat(DEFAULT_TOOL_OUTPUT_BUDGET_LIMIT_BYTES + 1);

        let got = project_text(
            &text,
            &Budget::from(ToolOutputBudgetConfig::default()),
            &ctx,
            false,
        )
        .await;

        assert!(!got.contains("Full output saved to:"), "{got}");
        assert!(!got.contains("retained as attachment"), "{got}");
        assert!(got.contains("Re-run the tool with narrower arguments"));
        assert!(artifacts.retained().is_empty());
    }

    #[tokio::test]
    async fn retain_full_output_journals_a_durable_artifact_and_names_it() {
        let artifacts = Arc::new(RecordingArtifacts::default());
        let ctx = test_context_with_artifacts(
            "grep",
            json!({"query": "needle"}),
            json!("unused"),
            artifacts.clone(),
        );
        let config = ToolOutputBudgetConfig {
            mode: ToolOutputBudgetMode::Bytes,
            limit: 4,
            max_lines: DEFAULT_TOOL_OUTPUT_BUDGET_MAX_LINES,
            head_share_percent: 50,
            retain_full_output: true,
        };

        let got = project_text("full output", &Budget::from(&config), &ctx, true).await;

        let retained = artifacts.retained();
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].0, "grep full tool output");
        assert_eq!(retained[0].1, "full output");
        assert!(got.contains("retained as attachment att-retained"), "{got}");
        assert!(got.contains("grep full tool output"), "{got}");
        assert!(!got.contains("saved to:"), "{got}");
    }

    #[tokio::test]
    async fn a_tool_supplied_full_output_path_wins_over_retention() {
        let artifacts = Arc::new(RecordingArtifacts::default());
        let ctx = test_context_with_artifacts(
            "exec_command",
            json!({}),
            json!({
                "output": "x".repeat(20_000),
                "full_output_path": "/tmp/existing-shell-output.log",
            }),
            artifacts.clone(),
        );
        let config = ToolOutputBudgetConfig {
            limit: 512,
            head_share_percent: 50,
            retain_full_output: true,
            ..ToolOutputBudgetConfig::default()
        };
        let projected = present_tool_result(&Budget::from(&config), ctx)
            .await
            .expect("project tool result");
        let output = render_model_return_parts(&projected.parts);
        assert!(output.contains("Full output saved to: /tmp/existing-shell-output.log"));
        assert!(output.contains("Use the shell tool or host-provided file access"));
        assert!(!output.contains("read_file"));
        assert!(!output.contains("grep"));
        assert!(artifacts.retained().is_empty());
    }

    #[tokio::test]
    async fn truncation_hint_without_retention_names_only_surviving_access_surfaces() {
        let ctx = test_context("read_file", json!({}), json!("unused"));
        let hint = truncation_hint(&ctx, "full output", false).await;

        assert!(hint.contains("Re-run the tool with narrower arguments"));
        assert!(hint.contains("shell tool or host-provided file access"));
        assert!(!hint.contains("read_file"));
        assert!(!hint.contains("grep"));
    }

    #[tokio::test]
    async fn model_projection_can_collapse_large_structured_payload_to_string() {
        let config = ToolOutputBudgetConfig {
            mode: ToolOutputBudgetMode::Bytes,
            limit: 40,
            max_lines: DEFAULT_TOOL_OUTPUT_BUDGET_MAX_LINES,
            head_share_percent: 50,
            retain_full_output: false,
        };
        let projected = present_tool_result(
            &Budget::from(&config),
            test_context(
                "search_tools",
                json!({}),
                json!({
                    "results": [{"output": "x".repeat(200)}]
                }),
            ),
        )
        .await
        .expect("project tool result");
        assert!(render_model_return_parts(&projected.parts).contains("bytes truncated"));
    }

    #[tokio::test]
    async fn batch_model_projection_preserves_projected_child_payloads() {
        let projected = present_tool_result(
            &Budget::from(ToolOutputBudgetConfig::default()),
            test_context(
                "batch",
                json!({}),
                json!({
                    "results": [
                        {"index": 0, "tool": "read_file", "success": true, "duration_ms": 1, "result": "very long child payload"},
                        {"index": 1, "tool": "grep", "success": false, "duration_ms": 1, "error": "boom"}
                    ]
                }),
            ),
        )
        .await
        .expect("project batch result");
        let projected_value: serde_json::Value =
            serde_json::from_str(&render_model_return_parts(&projected.parts)).unwrap();
        let results = projected_value
            .get("results")
            .and_then(|value| value.as_array())
            .expect("results");
        assert_eq!(results.len(), 2);
        assert_eq!(
            results[0].get("result"),
            Some(&json!("very long child payload"))
        );
        assert_eq!(results[1].get("error"), Some(&json!("boom")));
    }

    #[tokio::test]
    async fn batch_history_projection_recursively_projects_child_payloads() {
        let projected = present_tool_result(
            &Budget::from(ToolOutputBudgetConfig {
                limit: 8,
                ..ToolOutputBudgetConfig::default()
            }),
            test_context(
                "batch",
                json!({}),
                json!({
                    "results": [
                        {"index": 0, "tool": "read_file", "success": true, "duration_ms": 1, "result": "child payload"},
                        {"index": 1, "tool": "grep", "success": false, "duration_ms": 1, "error": "boom"}
                    ]
                }),
            ),
        )
        .await
        .expect("project batch result");
        let projected_value: serde_json::Value =
            serde_json::from_str(&render_model_return_parts(&projected.parts)).unwrap();
        let details = projected_value
            .get("results")
            .and_then(|value| value.as_array())
            .expect("results");
        assert_eq!(details.len(), 2);
        let child_result = details[0]
            .get("result")
            .and_then(|value| value.as_str())
            .unwrap_or_default();
        assert!(child_result.contains("truncated"));
        assert_eq!(details[1].get("error"), Some(&json!("boom")));
    }

    #[tokio::test]
    async fn batch_projection_decode_names_missing_required_row_field() {
        let error = present_tool_result(
            &Budget::from(ToolOutputBudgetConfig::default()),
            test_context(
                "batch",
                json!({}),
                json!({
                    "results": [{
                        "index": 0,
                        "tool": "read_file",
                        "duration_ms": 1,
                        "result": "payload"
                    }]
                }),
            ),
        )
        .await
        .expect_err("row without success must fail");

        assert!(
            error.to_string().contains("missing field `success`"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn zero_budget_returns_marker_only() {
        let byte_config = ToolOutputBudgetConfig {
            mode: ToolOutputBudgetMode::Bytes,
            limit: 0,
            max_lines: DEFAULT_TOOL_OUTPUT_BUDGET_MAX_LINES,
            head_share_percent: 50,
            retain_full_output: false,
        };
        let token_config = ToolOutputBudgetConfig {
            mode: ToolOutputBudgetMode::Tokens,
            limit: 0,
            max_lines: DEFAULT_TOOL_OUTPUT_BUDGET_MAX_LINES,
            head_share_percent: 50,
            retain_full_output: false,
        };
        let ctx = test_context("read_file", json!({}), json!("unused"));

        let byte_result =
            project_text("hello world", &Budget::from(&byte_config), &ctx, false).await;
        assert_eq!(byte_result, "...11 bytes truncated...");

        let token_result =
            project_text("hello world", &Budget::from(&token_config), &ctx, false).await;
        assert_eq!(token_result, "...3 tokens truncated...");
    }

    #[tokio::test]
    async fn byte_mode_vs_token_mode_equivalence_at_same_effective_max_bytes() {
        // limit: 10 tokens == limit: 40 bytes (10 * 4 = 40)
        let token_config = ToolOutputBudgetConfig {
            mode: ToolOutputBudgetMode::Tokens,
            limit: 10,
            max_lines: 100,
            head_share_percent: 50,
            retain_full_output: false,
        };
        let byte_config = ToolOutputBudgetConfig {
            mode: ToolOutputBudgetMode::Bytes,
            limit: 40,
            max_lines: 100,
            head_share_percent: 50,
            retain_full_output: false,
        };

        let token_budget = Budget::from(&token_config);
        let byte_budget = Budget::from(&byte_config);

        assert_eq!(token_budget.max_bytes, 40);
        assert_eq!(byte_budget.max_bytes, 40);
        assert_eq!(token_budget.max_lines, byte_budget.max_lines);

        let ctx = test_context("read_file", json!({}), json!("unused"));

        // Text within budget (<= 40 bytes) passes through untouched in both modes
        let short_text = "short text well under forty bytes";
        assert_eq!(
            project_text(short_text, &byte_budget, &ctx, false).await,
            short_text
        );
        assert_eq!(
            project_text(short_text, &token_budget, &ctx, false).await,
            short_text
        );

        // Text exactly at budget (40 bytes)
        let exact_text = "a".repeat(40);
        assert_eq!(
            project_text(&exact_text, &byte_budget, &ctx, false).await,
            exact_text
        );
        assert_eq!(
            project_text(&exact_text, &token_budget, &ctx, false).await,
            exact_text
        );

        // A 41-byte single-line input exceeds the effective byte budget in
        // both modes; max_lines must not be the reason either result truncates.
        let boundary_text = "a".repeat(41);
        let byte_boundary_projected = project_text(&boundary_text, &byte_budget, &ctx, false).await;
        let token_boundary_projected =
            project_text(&boundary_text, &token_budget, &ctx, false).await;
        assert_ne!(byte_boundary_projected, boundary_text);
        assert_ne!(token_boundary_projected, boundary_text);

        // Text exceeding budget (100 bytes): preview portions must be identical
        let long_text = "a".repeat(100);
        let byte_projected = project_text(&long_text, &byte_budget, &ctx, false).await;
        let token_projected = project_text(&long_text, &token_budget, &ctx, false).await;

        let byte_head = byte_projected.split("\n\n...").next().expect("head");
        let token_head = token_projected.split("\n\n...").next().expect("head");
        assert_eq!(byte_head, token_head);
        assert_eq!(byte_head.len(), 20);
        let byte_tail = byte_projected.rsplit("\n\n").next().expect("tail");
        let token_tail = token_projected.rsplit("\n\n").next().expect("tail");
        assert_eq!(byte_tail, token_tail);
        assert_eq!(byte_tail.len(), 20);

        assert!(byte_projected.contains("...60 bytes truncated..."));
        // 60 bytes / 4 = 15 tokens
        assert!(token_projected.contains("...15 tokens truncated..."));
    }

    #[test]
    fn factory_refuses_a_head_share_above_100_percent() {
        let config = ToolOutputBudgetConfig {
            head_share_percent: 101,
            ..ToolOutputBudgetConfig::default()
        };
        let error = ToolOutputBudgetPluginFactory::new(config)
            .err()
            .expect("head_share_percent above 100 must be refused");
        assert!(error.to_string().contains("head_share_percent"), "{error}");

        for share in [0u8, 50, 100] {
            assert!(
                ToolOutputBudgetPluginFactory::new(ToolOutputBudgetConfig {
                    head_share_percent: share,
                    ..ToolOutputBudgetConfig::default()
                })
                .is_ok(),
                "share {share} must be accepted"
            );
        }
    }

    #[tokio::test]
    async fn truncated_output_keeps_failure_lines_at_both_ends() {
        // A failure at the end of a long run must survive the same way a
        // failure at the start does, for any tool name.
        let tail_failure = "Compiling crate v1\n".to_string()
            + &"Checking dep\n".repeat(60)
            + "error[E0308]: mismatched types";
        let head_failure = "error: cannot find -lssl\n".to_string() + &"ok\n".repeat(60);
        let budget = Budget::from(&ToolOutputBudgetConfig {
            mode: ToolOutputBudgetMode::Bytes,
            limit: 120,
            max_lines: 6,
            head_share_percent: 50,
            retain_full_output: false,
        });
        let ctx = test_context("run", json!({}), json!("unused"));

        let tail_projected = project_text(&tail_failure, &budget, &ctx, false).await;
        assert!(
            tail_projected.contains("error[E0308]: mismatched types"),
            "{tail_projected}"
        );
        assert!(
            tail_projected.contains("Compiling crate v1"),
            "{tail_projected}"
        );

        let head_projected = project_text(&head_failure, &budget, &ctx, false).await;
        assert!(
            head_projected.contains("error: cannot find -lssl"),
            "{head_projected}"
        );
    }
}
