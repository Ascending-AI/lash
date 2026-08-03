use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};

use lash_core::plugin::{
    PluginError, PluginFactory, PluginRegistrar, PluginSessionContext, SessionPlugin,
    ToolResultProjectionContext,
};
use lash_core::{
    ToolCallOutcome, ToolValue, facade_support::ModelToolReturn,
    facade_support::ModelToolReturnPart, facade_support::PluginStack,
};

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
}

impl Default for ToolOutputBudgetConfig {
    fn default() -> Self {
        Self {
            mode: ToolOutputBudgetMode::Bytes,
            limit: DEFAULT_TOOL_OUTPUT_BUDGET_LIMIT_BYTES,
            max_lines: DEFAULT_TOOL_OUTPUT_BUDGET_MAX_LINES,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ProjectionDirection {
    Head,
    Tail,
}

/// Which end of the output a windowed truncation keeps.
///
/// `Head` keeps the leading lines (the common case); `Tail` keeps the
/// trailing lines (used for streaming command output where the end is
/// the interesting part). This is the public mirror of the budget
/// plugin's internal `ProjectionDirection`, exported so other plugins
/// (e.g. rolling-history) can share the one canonical truncation core
/// instead of forking it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruncationDirection {
    Head,
    Tail,
}

impl From<ProjectionDirection> for TruncationDirection {
    fn from(direction: ProjectionDirection) -> Self {
        match direction {
            ProjectionDirection::Head => TruncationDirection::Head,
            ProjectionDirection::Tail => TruncationDirection::Tail,
        }
    }
}

/// The unit reported in the `...N <unit> truncated...` marker when a
/// windowed truncation hits its byte budget (rather than its line cap).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TruncationUnit {
    /// Report removed *characters*, labelled `bytes`.
    Bytes,
    /// Report an approximate removed *token* count, labelled `tokens`.
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

/// Parameters for [`truncate_windowed`], the single canonical
/// head/tail-window + byte-cap truncation implementation shared by the
/// tool-output-budget projector and the rolling-history compaction
/// plugin.
#[derive(Clone, Copy, Debug)]
pub struct WindowedTruncation<'a> {
    /// Maximum number of lines retained in the preview window.
    pub max_lines: usize,
    /// Maximum number of bytes retained in the preview window.
    pub max_bytes: usize,
    /// Which end of the output to keep.
    pub direction: TruncationDirection,
    /// The unit reported in the byte-budget truncation marker.
    pub unit: TruncationUnit,
    /// Trailing hint text appended to (Head) / prepended to (Tail) the
    /// preview, explaining the truncation and where the full output is.
    pub hint: &'a str,
}

/// The canonical head/tail-window + byte-cap truncation core.
///
/// Returns `text` unchanged when it already fits within `max_lines` and
/// `max_bytes`. Otherwise keeps a preview window from the configured end
/// and wraps it with a `...N <unit> truncated...` marker plus the
/// caller-supplied `hint`.
///
/// A single line that is itself larger than `max_bytes` is truncated at
/// a UTF-8 char boundary rather than dropped, so over-long lines never
/// silently disappear and the function never panics on multi-byte text.
pub fn truncate_windowed(text: &str, opts: &WindowedTruncation) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let total_bytes = text.len();
    if lines.len() <= opts.max_lines && total_bytes <= opts.max_bytes {
        return text.to_string();
    }

    let mut preview_lines: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let mut hit_budget = false;

    let mut push_line = |line: &str, bytes: &mut usize, hit_budget: &mut bool| -> bool {
        // `separator` accounts for the `\n` re-joined between lines; the
        // first retained line carries no separator.
        let separator = usize::from(!preview_lines.is_empty());
        let remaining = opts.max_bytes.saturating_sub(*bytes + separator);
        if line.len() + separator <= opts.max_bytes.saturating_sub(*bytes) {
            preview_lines.push(line.to_string());
            *bytes += line.len() + separator;
            true
        } else if preview_lines.is_empty() && remaining > 0 {
            // A lone line longer than the whole budget: truncate it at a
            // char boundary instead of dropping it entirely.
            let cut = char_floor(line, remaining);
            if cut == 0 {
                *hit_budget = true;
                return false;
            }
            preview_lines.push(line[..cut].to_string());
            *bytes += cut;
            *hit_budget = true;
            false
        } else {
            *hit_budget = true;
            false
        }
    };

    match opts.direction {
        TruncationDirection::Head => {
            for line in lines.iter().take(opts.max_lines) {
                if !push_line(line, &mut bytes, &mut hit_budget) {
                    break;
                }
            }
        }
        TruncationDirection::Tail => {
            for line in lines.iter().rev().take(opts.max_lines) {
                if !push_line(line, &mut bytes, &mut hit_budget) {
                    break;
                }
            }
            preview_lines.reverse();
        }
    }

    let preview = preview_lines.join("\n");
    let (removed, unit) = if hit_budget {
        let removed = match opts.unit {
            TruncationUnit::Bytes => {
                u64::try_from(text.chars().count().saturating_sub(preview.chars().count()))
                    .unwrap_or(u64::MAX)
            }
            TruncationUnit::Tokens => {
                approx_tokens_from_byte_count(total_bytes.saturating_sub(preview.len()))
            }
        };
        (removed, opts.unit.label())
    } else {
        (
            u64::try_from(lines.len().saturating_sub(preview_lines.len())).unwrap_or(u64::MAX),
            "lines",
        )
    };
    let hint = opts.hint;
    match opts.direction {
        TruncationDirection::Head => {
            format!("{preview}\n\n...{removed} {unit} truncated...\n\n{hint}")
        }
        TruncationDirection::Tail => {
            format!("...{removed} {unit} truncated...\n\n{hint}\n\n{preview}")
        }
    }
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

pub struct ToolOutputBudgetPluginFactory {
    config: ToolOutputBudgetConfig,
}

impl ToolOutputBudgetPluginFactory {
    pub fn new(config: ToolOutputBudgetConfig) -> Self {
        Self { config }
    }
}

impl Default for ToolOutputBudgetPluginFactory {
    fn default() -> Self {
        Self::new(ToolOutputBudgetConfig::default())
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
            config: self.config.clone(),
        }))
    }
}

struct ToolOutputBudgetPlugin {
    config: ToolOutputBudgetConfig,
}

impl SessionPlugin for ToolOutputBudgetPlugin {
    fn id(&self) -> &'static str {
        "tool_output_budget"
    }

    fn register(&self, reg: &mut PluginRegistrar) -> Result<(), PluginError> {
        register_projector(reg, &self.config)
    }
}

fn register_projector(
    reg: &mut PluginRegistrar,
    config: &ToolOutputBudgetConfig,
) -> Result<(), PluginError> {
    let config = config.clone();
    reg.tool_results().projector(Arc::new(move |ctx| {
        let config = config.clone();
        Box::pin(async move { Ok(project_tool_result(&config, ctx)) })
    }))
}

fn project_tool_result(
    config: &ToolOutputBudgetConfig,
    ctx: ToolResultProjectionContext,
) -> ModelToolReturn {
    let parts = project_model_parts(config, &ctx);
    ModelToolReturn {
        call_id: ctx.call_id.clone(),
        tool_name: ctx.tool_name.clone(),
        parts,
    }
}

/// Project a tool result into the rendered model-facing text using the
/// canonical projector — including the structure-aware `batch` path that
/// recurses into each child result with the correct per-child truncation
/// direction.
///
/// Exposed so other plugins (e.g. rolling-history) can reuse the one
/// canonical batch/structured projection instead of flattening batched
/// output to an opaque string and tail-truncating it (which cuts JSON
/// mid-structure and loses per-child windowing). Attachments are rendered
/// inline using the same `[Attachment: …]` placeholder the budget plugin
/// uses internally.
pub fn project_tool_result_text(
    config: &ToolOutputBudgetConfig,
    ctx: ToolResultProjectionContext,
) -> String {
    render_model_return_parts(&project_tool_result(config, ctx).parts)
}

fn project_model_parts(
    config: &ToolOutputBudgetConfig,
    ctx: &ToolResultProjectionContext,
) -> Vec<ModelToolReturnPart> {
    if ctx.tool_name == "batch" {
        let value = project_batch_value(config, ctx);
        return vec![ModelToolReturnPart::text(render_projected_model_value(
            &value,
        ))];
    }

    match &ctx.output.outcome {
        ToolCallOutcome::Success(value) => project_tool_value_parts(config, ctx, value),
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
    }
}

fn render_projected_model_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(text) => text.clone(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "null".to_string()),
    }
}

fn project_tool_value_parts(
    config: &ToolOutputBudgetConfig,
    ctx: &ToolResultProjectionContext,
    value: &ToolValue,
) -> Vec<ModelToolReturnPart> {
    let mut parts = Vec::new();
    match value {
        ToolValue::String(text) => {
            parts.push(ModelToolReturnPart::text(project_text(text, config, ctx)))
        }
        ToolValue::Attachment(reference) => {
            parts.push(ModelToolReturnPart::Attachment(reference.clone()));
        }
        ToolValue::Null
        | ToolValue::Bool(_)
        | ToolValue::Number(_)
        | ToolValue::Array(_)
        | ToolValue::Object(_) => {
            push_projected_tool_value_parts(value, &mut parts, config, ctx);
        }
    }
    parts
}

fn push_projected_tool_value_parts(
    value: &ToolValue,
    parts: &mut Vec<ModelToolReturnPart>,
    config: &ToolOutputBudgetConfig,
    ctx: &ToolResultProjectionContext,
) {
    match value {
        ToolValue::Null => push_text_part(parts, "null"),
        ToolValue::Bool(value) => push_text_part(parts, value.to_string()),
        ToolValue::Number(value) => push_text_part(parts, value.to_string()),
        ToolValue::String(text) => push_text_part(
            parts,
            serde_json::to_string(&project_text(text, config, ctx))
                .unwrap_or_else(|_| "\"\"".to_string()),
        ),
        ToolValue::Attachment(reference) => {
            parts.push(ModelToolReturnPart::Attachment(reference.clone()));
        }
        ToolValue::Array(items) => {
            push_text_part(parts, "[");
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    push_text_part(parts, ",");
                }
                push_projected_tool_value_parts(item, parts, config, ctx);
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
                push_projected_tool_value_parts(value, parts, config, ctx);
            }
            push_text_part(parts, "}");
        }
    }
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

fn project_text(
    text: &str,
    config: &ToolOutputBudgetConfig,
    ctx: &ToolResultProjectionContext,
) -> String {
    if !needs_truncation(text, config) {
        return text.to_string();
    }
    truncate_text(
        text,
        config,
        tool_projection_direction(&ctx.tool_name),
        Some(ctx),
    )
}

fn needs_truncation(text: &str, config: &ToolOutputBudgetConfig) -> bool {
    if text.lines().count() > config.max_lines {
        return true;
    }
    match config.mode {
        ToolOutputBudgetMode::Bytes => text.len() > config.limit,
        ToolOutputBudgetMode::Tokens => approx_token_count(text) > config.limit,
    }
}

fn truncate_text(
    text: &str,
    config: &ToolOutputBudgetConfig,
    direction: ProjectionDirection,
    ctx: Option<&ToolResultProjectionContext>,
) -> String {
    truncate_text_with_hint(text, config, direction, truncation_hint(ctx, text))
}

fn truncate_text_with_hint(
    text: &str,
    config: &ToolOutputBudgetConfig,
    direction: ProjectionDirection,
    hint: String,
) -> String {
    if text.is_empty() {
        return String::new();
    }
    let max_bytes = match config.mode {
        ToolOutputBudgetMode::Bytes => config.limit,
        ToolOutputBudgetMode::Tokens => approx_bytes_for_tokens(config.limit),
    };
    if max_bytes == 0 {
        return format_truncation_marker(
            config.mode,
            removed_units(config.mode, text.len(), text.chars().count()),
        );
    }
    if !needs_truncation(text, config) {
        return text.to_string();
    }
    truncate_windowed(
        text,
        &WindowedTruncation {
            max_lines: config.max_lines,
            max_bytes,
            direction: direction.into(),
            unit: match config.mode {
                ToolOutputBudgetMode::Bytes => TruncationUnit::Bytes,
                ToolOutputBudgetMode::Tokens => TruncationUnit::Tokens,
            },
            hint: &hint,
        },
    )
}

fn format_truncation_marker(mode: ToolOutputBudgetMode, removed: u64) -> String {
    match mode {
        ToolOutputBudgetMode::Bytes => format!("…{removed} chars truncated…"),
        ToolOutputBudgetMode::Tokens => format!("…{removed} tokens truncated…"),
    }
}

fn removed_units(mode: ToolOutputBudgetMode, removed_bytes: usize, removed_chars: usize) -> u64 {
    match mode {
        ToolOutputBudgetMode::Bytes => u64::try_from(removed_chars).unwrap_or(u64::MAX),
        ToolOutputBudgetMode::Tokens => approx_tokens_from_byte_count(removed_bytes),
    }
}

fn approx_token_count(text: &str) -> usize {
    text.len()
        .saturating_add(APPROX_BYTES_PER_TOKEN.saturating_sub(1))
        / APPROX_BYTES_PER_TOKEN
}

fn approx_bytes_for_tokens(tokens: usize) -> usize {
    tokens.saturating_mul(APPROX_BYTES_PER_TOKEN)
}

fn approx_tokens_from_byte_count(bytes: usize) -> u64 {
    let bytes = bytes as u64;
    bytes.saturating_add((APPROX_BYTES_PER_TOKEN as u64).saturating_sub(1))
        / (APPROX_BYTES_PER_TOKEN as u64)
}

fn tool_projection_direction(tool_name: &str) -> ProjectionDirection {
    match tool_name {
        "exec_command" | "write_stdin" => ProjectionDirection::Tail,
        _ => ProjectionDirection::Head,
    }
}

fn truncation_hint(ctx: Option<&ToolResultProjectionContext>, text: &str) -> String {
    let output_path = ctx
        .and_then(existing_tool_output_path)
        .or_else(|| ctx.and_then(|ctx| spill_tool_output(&ctx.tool_name, &ctx.args, text)));
    match output_path {
        Some(path) => format!(
            "The tool output was truncated. Full output saved to: {}\nUse `read_file` with `offset`/`limit` or `grep` to inspect specific sections instead of reading the whole file at once.",
            path.display()
        ),
        None => "The tool output was truncated. Use `read_file` with `offset`/`limit` or `grep` to inspect specific sections instead of reading the whole file at once.".to_string(),
    }
}

fn existing_tool_output_path(ctx: &ToolResultProjectionContext) -> Option<PathBuf> {
    ctx.output
        .value_for_projection()
        .get("full_output_path")
        .and_then(|value| value.as_str())
        .filter(|value| !value.trim().is_empty())
        .map(PathBuf::from)
}

fn spill_tool_output(
    tool_name: &str,
    args: &serde_json::Value,
    full_output: &str,
) -> Option<PathBuf> {
    let dir = std::env::temp_dir().join("lash-tool-output");
    if fs::create_dir_all(&dir).is_err() {
        return None;
    }

    let mut hasher = Sha256::new();
    hasher.update(tool_name.as_bytes());
    hasher.update(args.to_string().as_bytes());
    hasher.update(full_output.as_bytes());
    let digest = format!("{:x}", hasher.finalize());
    let stem = tool_name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    let path = dir.join(format!("{stem}-{}.txt", &digest[..12]));
    if write_if_changed(&path, full_output).is_err() {
        return None;
    }
    Some(path)
}

fn write_if_changed(path: &Path, content: &str) -> std::io::Result<()> {
    let should_write = match fs::read_to_string(path) {
        Ok(existing) => existing != content,
        Err(_) => true,
    };
    if should_write {
        fs::write(path, content)?;
    }
    Ok(())
}

fn project_batch_value(
    config: &ToolOutputBudgetConfig,
    ctx: &ToolResultProjectionContext,
) -> serde_json::Value {
    let value = ctx.output.value_for_projection();
    let Some(map) = value.as_object() else {
        return project_json_value(&value, config, ctx);
    };

    let mut projected = serde_json::Map::new();

    let results = map
        .get("results")
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .enumerate()
                .map(|(index, item)| project_batch_child_value(index, item, config, ctx))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    projected.insert("results".to_string(), serde_json::Value::Array(results));
    serde_json::Value::Object(projected)
}

fn project_batch_child_value(
    index: usize,
    item: &serde_json::Value,
    config: &ToolOutputBudgetConfig,
    ctx: &ToolResultProjectionContext,
) -> serde_json::Value {
    let Some(map) = item.as_object() else {
        return project_json_value(item, config, ctx);
    };

    let tool_name = map
        .get("tool")
        .and_then(|value| value.as_str())
        .or_else(|| batch_child_tool_name(&ctx.args, index))
        .unwrap_or("tool")
        .to_string();
    let success = map
        .get("success")
        .and_then(|value| value.as_bool())
        .unwrap_or(false);
    let duration_ms = map
        .get("duration_ms")
        .and_then(|value| value.as_u64())
        .unwrap_or_default();
    let child_value = if success {
        map.get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    } else {
        map.get("error").cloned().unwrap_or(serde_json::Value::Null)
    };
    let child_args = batch_child_args(&ctx.args, index);

    let projected_child = if tool_name == "batch" || !success {
        project_json_value(&child_value, config, ctx)
    } else {
        let model_return = project_tool_result(
            config,
            ToolResultProjectionContext {
                session_id: ctx.session_id.clone(),
                call_id: format!("{}.{}", ctx.call_id, index),
                tool_name: tool_name.clone(),
                args: child_args,
                output: lash_core::ToolCallOutput::success(child_value.clone()),
                duration_ms,
            },
        );
        let rendered = render_model_return_parts(&model_return.parts);
        rendered
            .parse::<serde_json::Value>()
            .unwrap_or(serde_json::Value::String(rendered))
    };

    let mut projected = serde_json::Map::new();
    if let Some(value) = map.get("index") {
        projected.insert("index".to_string(), value.clone());
    }
    projected.insert("tool".to_string(), serde_json::json!(tool_name));
    projected.insert("success".to_string(), serde_json::json!(success));
    projected.insert("duration_ms".to_string(), serde_json::json!(duration_ms));
    projected.insert(
        if success {
            "result".to_string()
        } else {
            "error".to_string()
        },
        projected_child,
    );
    serde_json::Value::Object(projected)
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

fn project_json_value(
    value: &serde_json::Value,
    config: &ToolOutputBudgetConfig,
    ctx: &ToolResultProjectionContext,
) -> serde_json::Value {
    match value {
        serde_json::Value::String(text) => {
            serde_json::Value::String(project_text(text, config, ctx))
        }
        serde_json::Value::Array(items) => serde_json::Value::Array(
            items
                .iter()
                .map(|item| project_json_value(item, config, ctx))
                .collect(),
        ),
        serde_json::Value::Object(map) => serde_json::Value::Object(
            map.iter()
                .map(|(key, value)| (key.clone(), project_json_value(value, config, ctx)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn batch_child_tool_name(batch_args: &serde_json::Value, index: usize) -> Option<&str> {
    batch_args
        .get("tool_calls")
        .and_then(|value| value.as_array())
        .and_then(|items| items.get(index))
        .and_then(|value| value.get("tool"))
        .and_then(|value| value.as_str())
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
    use serde_json::json;

    #[test]
    fn windowed_truncation_truncates_over_long_single_line_instead_of_dropping_it() {
        // A single line longer than the whole byte budget must be cut at a
        // char boundary, not dropped (which would leave an empty preview).
        let line = "x".repeat(1000);
        let got = truncate_windowed(
            &line,
            &WindowedTruncation {
                max_lines: 400,
                max_bytes: 64,
                direction: TruncationDirection::Head,
                unit: TruncationUnit::Bytes,
                hint: "hint",
            },
        );
        let preview = got.split("\n\n...").next().expect("preview");
        assert!(!preview.is_empty(), "preview must not be empty: {got:?}");
        assert!(preview.len() <= 64);
        assert!(preview.chars().all(|c| c == 'x'));
        assert!(got.contains("bytes truncated"));
    }

    #[test]
    fn windowed_truncation_never_splits_a_multibyte_char() {
        // Budget lands mid-way through a 3-byte char; must back off to a
        // boundary rather than panic or emit invalid UTF-8.
        let line = "★".repeat(100); // each '★' is 3 bytes
        let got = truncate_windowed(
            &line,
            &WindowedTruncation {
                max_lines: 400,
                max_bytes: 10, // not a multiple of 3
                direction: TruncationDirection::Head,
                unit: TruncationUnit::Bytes,
                hint: "hint",
            },
        );
        let preview = got.split("\n\n...").next().expect("preview");
        assert!(!preview.is_empty());
        assert!(preview.chars().all(|c| c == '★'));
        assert_eq!(preview.len() % 3, 0, "must cut on a char boundary");
        assert!(preview.len() <= 10);
    }

    #[test]
    fn windowed_truncation_returns_input_unchanged_when_within_budget() {
        let text = "a\nb\nc";
        let got = truncate_windowed(
            text,
            &WindowedTruncation {
                max_lines: 400,
                max_bytes: 1024,
                direction: TruncationDirection::Head,
                unit: TruncationUnit::Bytes,
                hint: "hint",
            },
        );
        assert_eq!(got, text);
    }

    #[test]
    fn truncates_strings_with_terminal_style_marker() {
        let config = ToolOutputBudgetConfig {
            mode: ToolOutputBudgetMode::Tokens,
            limit: 5,
            max_lines: DEFAULT_TOOL_OUTPUT_BUDGET_MAX_LINES,
        };
        let got = project_text(
            "this is an example of a long output that should be truncated",
            &config,
            &ToolResultProjectionContext {
                session_id: "root".to_string(),
                call_id: "call".to_string(),
                tool_name: "grep".to_string(),
                args: json!({}),
                output: lash_core::ToolCallOutput::success(json!("unused")),
                duration_ms: 1,
            },
        );
        assert!(got.contains("tokens truncated"));
        assert!(got.contains("Full output saved to:"));
    }

    #[test]
    fn truncation_hint_reuses_existing_full_output_path() {
        let config = ToolOutputBudgetConfig {
            limit: 512,
            ..ToolOutputBudgetConfig::default()
        };
        let projected = project_tool_result(
            &config,
            ToolResultProjectionContext {
                session_id: "root".to_string(),
                call_id: "call".to_string(),
                tool_name: "exec_command".to_string(),
                args: json!({}),
                output: lash_core::ToolCallOutput::success(json!({
                    "output": "x".repeat(20_000),
                    "full_output_path": "/tmp/existing-shell-output.log",
                })),
                duration_ms: 1,
            },
        );
        let output = render_model_return_parts(&projected.parts);
        assert!(output.contains("Full output saved to: /tmp/existing-shell-output.log"));
    }

    #[test]
    fn model_projection_can_collapse_large_structured_payload_to_string() {
        let config = ToolOutputBudgetConfig {
            mode: ToolOutputBudgetMode::Bytes,
            limit: 40,
            max_lines: DEFAULT_TOOL_OUTPUT_BUDGET_MAX_LINES,
        };
        let projected = project_tool_result(
            &config,
            ToolResultProjectionContext {
                session_id: "root".to_string(),
                call_id: "call".to_string(),
                tool_name: "search_tools".to_string(),
                args: json!({}),
                output: lash_core::ToolCallOutput::success(json!({
                    "results": [{"output": "x".repeat(200)}]
                })),
                duration_ms: 1,
            },
        );
        assert!(render_model_return_parts(&projected.parts).contains("bytes truncated"));
    }

    #[test]
    fn batch_model_projection_preserves_projected_child_payloads() {
        let projected = project_tool_result(
            &ToolOutputBudgetConfig::default(),
            ToolResultProjectionContext {
                session_id: "root".to_string(),
                call_id: "call".to_string(),
                tool_name: "batch".to_string(),
                args: json!({}),
                output: lash_core::ToolCallOutput::success(json!({
                    "results": [
                        {"tool": "read_file", "success": true, "duration_ms": 1, "result": "very long child payload"},
                        {"tool": "grep", "success": false, "duration_ms": 1, "error": "boom"}
                    ]
                })),
                duration_ms: 1,
            },
        );
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

    #[test]
    fn batch_history_projection_recursively_projects_child_payloads() {
        let projected = project_tool_result(
            &ToolOutputBudgetConfig {
                limit: 8,
                ..ToolOutputBudgetConfig::default()
            },
            ToolResultProjectionContext {
                session_id: "root".to_string(),
                call_id: "call".to_string(),
                tool_name: "batch".to_string(),
                args: json!({}),
                output: lash_core::ToolCallOutput::success(json!({
                    "results": [
                        {"tool": "read_file", "success": true, "duration_ms": 1, "result": "child payload"},
                        {"tool": "grep", "success": false, "duration_ms": 1, "error": "boom"}
                    ]
                })),
                duration_ms: 1,
            },
        );
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
}
