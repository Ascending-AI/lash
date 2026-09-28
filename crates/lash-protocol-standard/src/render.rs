use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;

use lash_core::facade_support::{ModelToolReturn, ModelToolReturnPart, ToolPresentationInput};
use lash_core::{
    RecordedRender, RuntimeErrorCode, ToolCallOutcome, ToolCallOutput, ToolId, ToolViewBlock,
};
use lash_render::{CutKind, CutReport, RenderParams, RenderParamsPatch, Rendered, ShownRange};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthoredViewPolicy {
    #[default]
    Prefer,
    Ignore,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRenderParams {
    pub value: RenderParams,
    pub authored_view: AuthoredViewPolicy,
    pub max_lines: usize,
    pub head_share_percent: u8,
    pub retain_full_output: bool,
}

impl Default for ToolRenderParams {
    fn default() -> Self {
        Self {
            value: RenderParams {
                max_chars: 16_000,
                ..RenderParams::default()
            },
            authored_view: AuthoredViewPolicy::Prefer,
            max_lines: 400,
            head_share_percent: 50,
            retain_full_output: true,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolRenderPatch {
    #[serde(default, skip_serializing_if = "RenderParamsPatch::is_empty")]
    pub value: RenderParamsPatch,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authored_view: Option<AuthoredViewPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_lines: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_share_percent: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retain_full_output: Option<bool>,
}

impl ToolRenderPatch {
    fn over(&self, under: &Self) -> Self {
        Self {
            value: self.value.over(&under.value),
            authored_view: self.authored_view.or(under.authored_view),
            max_lines: self.max_lines.or(under.max_lines),
            head_share_percent: self.head_share_percent.or(under.head_share_percent),
            retain_full_output: self.retain_full_output.or(under.retain_full_output),
        }
    }

    fn apply(&self, base: &ToolRenderParams) -> ToolRenderParams {
        ToolRenderParams {
            value: self.value.apply(&base.value),
            authored_view: self.authored_view.unwrap_or(base.authored_view),
            max_lines: self.max_lines.unwrap_or(base.max_lines),
            head_share_percent: self.head_share_percent.unwrap_or(base.head_share_percent),
            retain_full_output: self.retain_full_output.unwrap_or(base.retain_full_output),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StandardRenderConfig {
    #[serde(default)]
    pub defaults: ToolRenderPatch,
    #[serde(default)]
    pub per_tool: BTreeMap<ToolId, ToolRenderPatch>,
}

impl StandardRenderConfig {
    pub fn builtin() -> Self {
        Self {
            per_tool: BTreeMap::from([(
                ToolId::new("tool:batch"),
                ToolRenderPatch {
                    value: RenderParamsPatch {
                        max_depth: Some(6),
                        ..RenderParamsPatch::default()
                    },
                    ..ToolRenderPatch::default()
                },
            )]),
            ..Self::default()
        }
    }
}

pub(crate) fn without_nulls(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(mut entries) => {
            entries.retain(|_, value| !value.is_null());
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(key, value)| (key, without_nulls(value)))
                    .collect(),
            )
        }
        other => other,
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedStandardRenderConfig {
    pub defaults: ToolRenderParams,
    pub per_tool: BTreeMap<ToolId, ToolRenderParams>,
}

impl ResolvedStandardRenderConfig {
    pub fn for_tool(&self, id: &ToolId) -> &ToolRenderParams {
        self.per_tool.get(id).unwrap_or(&self.defaults)
    }
}

pub fn resolve(
    builtin: &StandardRenderConfig,
    host: &StandardRenderConfig,
    options: &StandardRenderConfig,
) -> Result<ResolvedStandardRenderConfig, String> {
    let defaults_patch = options
        .defaults
        .over(&host.defaults.over(&builtin.defaults));
    let defaults = defaults_patch.apply(&ToolRenderParams::default());
    let mut entries = builtin.per_tool.clone();
    for layer in [&host.per_tool, &options.per_tool] {
        for (id, patch) in layer {
            let merged = patch.over(entries.get(id).unwrap_or(&ToolRenderPatch::default()));
            entries.insert(id.clone(), merged);
        }
    }
    let per_tool = entries
        .into_iter()
        .map(|(id, patch)| (id, patch.apply(&defaults)))
        .collect::<BTreeMap<_, _>>();
    if defaults.head_share_percent > 100
        || per_tool
            .values()
            .any(|params| params.head_share_percent > 100)
    {
        return Err("head_share_percent must be within 0..=100".into());
    }
    Ok(ResolvedStandardRenderConfig { defaults, per_tool })
}

pub trait ToolOutputRenderer: Send + Sync {
    fn id(&self) -> &str;

    fn tool_output(
        &self,
        output: &ToolCallOutput,
        _tool: &ToolId,
        params: &ToolRenderParams,
    ) -> Rendered<Vec<ModelToolReturnPart>> {
        builtin_tool_output(output, params)
    }
}

pub struct BuiltinToolOutputRenderer;

impl ToolOutputRenderer for BuiltinToolOutputRenderer {
    fn id(&self) -> &str {
        "lash.tool.v1"
    }
}

#[derive(Clone)]
pub struct ToolOutputRendererSlot(pub Arc<dyn ToolOutputRenderer>);

impl Default for ToolOutputRendererSlot {
    fn default() -> Self {
        Self(Arc::new(BuiltinToolOutputRenderer))
    }
}

impl fmt::Debug for ToolOutputRendererSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("ToolOutputRendererSlot")
            .field(&self.0.id())
            .finish()
    }
}

impl PartialEq for ToolOutputRendererSlot {
    fn eq(&self, other: &Self) -> bool {
        self.0.id() == other.0.id()
    }
}

impl Eq for ToolOutputRendererSlot {}

fn builtin_tool_output(
    output: &ToolCallOutput,
    params: &ToolRenderParams,
) -> Rendered<Vec<ModelToolReturnPart>> {
    if !matches!(output.outcome, ToolCallOutcome::Success(_)) {
        let parts = ModelToolReturn::from_output(String::new(), String::new(), output).parts;
        return Rendered {
            body: parts,
            cuts: CutReport::default(),
        };
    }
    if params.authored_view == AuthoredViewPolicy::Prefer
        && let Some(view) = &output.view
    {
        let mut parts = Vec::with_capacity(view.blocks.len());
        for block in &view.blocks {
            match block {
                ToolViewBlock::Text { text, .. } => {
                    parts.push(ModelToolReturnPart::text(text.clone()))
                }
                ToolViewBlock::Attachment { source, .. } => {
                    parts.push(ModelToolReturnPart::Attachment(source.clone()))
                }
                ToolViewBlock::ResourceLink {
                    uri,
                    name,
                    mime_type,
                    ..
                } => {
                    let mime = mime_type
                        .as_deref()
                        .map(|value| format!(" {value}"))
                        .unwrap_or_default();
                    parts.push(ModelToolReturnPart::text(format!(
                        "[resource: {name} <{uri}>{mime}]"
                    )));
                }
            }
        }
        return Rendered {
            body: parts,
            cuts: CutReport::default(),
        };
    }
    let value = output.value_for_projection();
    let rendered = lash_render::render(&value, &params.value);
    let mut parts = vec![ModelToolReturnPart::text(rendered.body)];
    parts.extend(
        output
            .attachments()
            .into_iter()
            .map(ModelToolReturnPart::Attachment),
    );
    Rendered {
        body: parts,
        cuts: rendered.cuts,
    }
}

fn full_output(output: &ToolCallOutput, params: &ToolRenderParams) -> String {
    if params.authored_view == AuthoredViewPolicy::Prefer
        && let Some(view) = &output.view
    {
        return view
            .blocks
            .iter()
            .filter_map(|block| match block {
                ToolViewBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
    }
    match output.value_for_projection() {
        serde_json::Value::String(text) => text,
        value => serde_json::to_string_pretty(&value).unwrap_or_else(|_| "null".into()),
    }
}

fn text_stats(parts: &[ModelToolReturnPart]) -> (usize, usize) {
    let mut chars = 0;
    let mut lines = 0;
    for part in parts {
        if let ModelToolReturnPart::Text { text } = part {
            chars += text.chars().count();
            lines += text.chars().filter(|ch| *ch == '\n').count();
        }
    }
    (chars, if chars == 0 { 0 } else { lines + 1 })
}

fn head_tail(
    parts: Vec<ModelToolReturnPart>,
    params: &ToolRenderParams,
    notice: &str,
    cuts: &mut CutReport,
) -> Vec<ModelToolReturnPart> {
    let joined = parts
        .iter()
        .filter_map(|part| match part {
            ModelToolReturnPart::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    let chars = joined.chars().collect::<Vec<_>>();
    let original_lines = if chars.is_empty() {
        0
    } else {
        chars.iter().filter(|ch| **ch == '\n').count() + 1
    };
    let text_limit = params
        .value
        .max_chars
        .saturating_sub(notice.chars().count());
    let line_cut = original_lines > params.max_lines;
    let char_cut = chars.len() > text_limit;
    let mut keep = vec![true; chars.len()];
    if line_cut {
        let head_lines = params.max_lines * usize::from(params.head_share_percent) / 100;
        let tail_lines = params.max_lines.saturating_sub(head_lines);
        let mut line = 0;
        for (index, ch) in chars.iter().enumerate() {
            keep[index] = line < head_lines || line >= original_lines.saturating_sub(tail_lines);
            if *ch == '\n' {
                line += 1;
            }
        }
        *cuts.counts.entry(CutKind::Lines).or_default() += 1;
    }
    let selected = keep
        .iter()
        .enumerate()
        .filter_map(|(index, kept)| kept.then_some(index))
        .collect::<Vec<_>>();
    if selected.len() > text_limit {
        let head_chars = text_limit * usize::from(params.head_share_percent) / 100;
        let tail_chars = text_limit.saturating_sub(head_chars);
        for (position, index) in selected.iter().enumerate() {
            keep[*index] =
                position < head_chars || position >= selected.len().saturating_sub(tail_chars);
        }
    }
    if char_cut || selected.len() > text_limit {
        *cuts.counts.entry(CutKind::Chars).or_default() += 1;
    }
    let mut result = Vec::with_capacity(parts.len() + 1);
    let mut offset = 0;
    let mut inserted = false;
    for (block, part) in parts.into_iter().enumerate() {
        match part {
            ModelToolReturnPart::Text { text } => {
                let mut current = String::new();
                let start = offset;
                for ch in text.chars() {
                    if keep[offset] {
                        current.push(ch);
                    } else if !inserted {
                        current.push_str(notice);
                        inserted = true;
                    }
                    offset += 1;
                }
                if !current.is_empty() {
                    result.push(ModelToolReturnPart::text(current));
                }
                let local = text.chars().collect::<Vec<_>>();
                let mut line = 0;
                let mut range_start = None;
                let mut line_start = 0;
                for (position, ch) in local.iter().enumerate() {
                    if keep[start + position] && range_start.is_none() {
                        range_start = Some(position);
                        line_start = line;
                    }
                    if !keep[start + position]
                        && let Some(first) = range_start.take()
                    {
                        cuts.shown.push(ShownRange::Text {
                            block,
                            chars: first..position,
                            lines: line_start..line + 1,
                        });
                    }
                    if *ch == '\n' {
                        line += 1;
                    }
                }
                if let Some(first) = range_start {
                    cuts.shown.push(ShownRange::Text {
                        block,
                        chars: first..local.len(),
                        lines: line_start..line + 1,
                    });
                }
            }
            attachment => result.push(attachment),
        }
    }
    if !inserted {
        result.push(ModelToolReturnPart::text(notice));
    }
    result
}

pub async fn present(
    input: ToolPresentationInput,
    renderer: &ToolOutputRendererSlot,
) -> Result<ModelToolReturn, lash_core::RuntimeEffectControllerError> {
    render_present(input.previous, &input.context, renderer).await
}

async fn render_present(
    mut result: ModelToolReturn,
    ctx: &lash_core::facade_support::ToolResultProjectionContext,
    renderer: &ToolOutputRendererSlot,
) -> Result<ModelToolReturn, lash_core::RuntimeEffectControllerError> {
    let recorded = ctx
        .render
        .as_ref()
        .ok_or_else(|| renderer_unavailable(None, renderer.0.id()))?;
    if recorded.renderer_id != renderer.0.id() {
        return Err(renderer_unavailable(Some(recorded), renderer.0.id()));
    }
    let resolved: ResolvedStandardRenderConfig = serde_json::from_value(recorded.params.clone())
        .map_err(|error| {
            lash_core::RuntimeEffectControllerError::new(
                RuntimeErrorCode::RunShapeRefused,
                error.to_string(),
            )
        })?;
    let params = resolved.for_tool(&ctx.tool_id);
    let mut rendered = renderer.0.tool_output(&ctx.output, &ctx.tool_id, params);
    rendered.body.extend(
        result
            .attachment_notices
            .iter()
            .map(|notice| ModelToolReturnPart::text(notice.model_placeholder())),
    );
    let mut cuts = rendered.cuts;
    let (chars, lines) = text_stats(&rendered.body);
    cuts.original_chars = cuts.original_chars.max(chars);
    if !matches!(ctx.output.outcome, ToolCallOutcome::Success(_)) {
        result.parts = rendered.body;
        return Ok(result);
    }
    let needs_cut = !cuts.is_empty() || chars > params.value.max_chars || lines > params.max_lines;
    if !needs_cut {
        result.parts = rendered.body;
        return Ok(result);
    }
    let retention = if params.retain_full_output {
        ctx.artifacts
            .retain_text(
                &format!("tool-output:{}", ctx.call_id),
                &full_output(&ctx.output, params),
            )
            .await
            .map(|reference| format!("attachment {}", reference.id))
            .unwrap_or_else(|error| format!("retention failed: {error}"))
    } else {
        "not retained".to_string()
    };
    let mut notice = format!(
        "[output cut: showing head and tail of {} chars / {lines} lines; full output: {retention}]",
        cuts.original_chars
    );
    for _ in 0..8 {
        let mut draft = cuts.clone();
        let _ = head_tail(rendered.body.clone(), params, &notice, &mut draft);
        let shown = draft
            .shown
            .iter()
            .map(|range| match range {
                ShownRange::Text {
                    block,
                    chars,
                    lines,
                } => format!(
                    "block {block} chars {}..{} lines {}..{}",
                    chars.start, chars.end, lines.start, lines.end
                ),
                ShownRange::ValuePath(path) => format!("value {path}"),
            })
            .collect::<Vec<_>>()
            .join(", ");
        let next = format!(
            "[output cut: showing {shown} of {} chars / {lines} lines; full output: {retention}]",
            cuts.original_chars
        );
        if next == notice {
            break;
        }
        notice = next;
    }
    if notice.chars().count() > params.value.max_chars {
        notice = notice.chars().take(params.value.max_chars).collect();
    }
    result.parts = head_tail(rendered.body, params, &notice, &mut cuts);
    Ok(result)
}

fn renderer_unavailable(
    recorded: Option<&RecordedRender>,
    live: &str,
) -> lash_core::RuntimeEffectControllerError {
    lash_core::RuntimeEffectControllerError::new(
        RuntimeErrorCode::RecordedRendererUnavailable,
        format!(
            "standard renderer {} is unavailable; live renderer is {live}",
            recorded
                .map(|record| record.renderer_id.as_str())
                .unwrap_or("<missing>")
        ),
    )
    .retryable_uncommitted_derivation()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lash_core::facade_support::{ToolPresentationArtifacts, ToolResultProjectionContext};
    use lash_core::{
        AttachmentId, AttachmentRef, MediaType, PluginError, SessionId, ToolView, ToolViewMeta,
    };
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Artifacts {
        writes: AtomicUsize,
        text: std::sync::Mutex<Vec<String>>,
        fail: bool,
    }

    impl ToolPresentationArtifacts for Artifacts {
        fn retain_text<'a>(
            &'a self,
            _label: &'a str,
            text: &'a str,
        ) -> Pin<Box<dyn Future<Output = Result<AttachmentRef, PluginError>> + Send + 'a>> {
            Box::pin(async move {
                self.writes.fetch_add(1, Ordering::SeqCst);
                self.text.lock().expect("test mutex").push(text.to_string());
                if self.fail {
                    return Err(PluginError::Session("store unavailable".into()));
                }
                Ok(AttachmentRef::new(
                    AttachmentId::parse("full-output").expect("valid id"),
                    MediaType::parse("text/plain").expect("valid type"),
                    text.len() as u64,
                    None,
                    None,
                ))
            })
        }
    }

    fn context(
        output: ToolCallOutput,
        params: ToolRenderParams,
        artifacts: Arc<Artifacts>,
    ) -> ToolResultProjectionContext {
        ToolResultProjectionContext {
            session_id: SessionId::from("test"),
            call_id: "call".into(),
            tool_id: ToolId::new("tool:test"),
            tool_name: "display name".into(),
            render: Some(RecordedRender {
                renderer_id: "lash.tool.v1".into(),
                params: serde_json::to_value(ResolvedStandardRenderConfig {
                    defaults: params,
                    per_tool: BTreeMap::new(),
                })
                .expect("params"),
            }),
            args: serde_json::Value::Null,
            output,
            duration_ms: 0,
            artifacts,
        }
    }

    fn baseline(ctx: &ToolResultProjectionContext) -> ModelToolReturn {
        ModelToolReturn::from_output(ctx.call_id.clone(), ctx.tool_name.clone(), &ctx.output)
    }

    #[test]
    fn per_tool_params_overlay_fieldwise_and_batch_depth_is_built_in() {
        let id = ToolId::new("tool:test");
        let mut host = StandardRenderConfig::default();
        host.defaults.value.max_chars = Some(90);
        host.defaults.max_lines = Some(12);
        host.per_tool.insert(
            id.clone(),
            ToolRenderPatch {
                authored_view: Some(AuthoredViewPolicy::Ignore),
                ..ToolRenderPatch::default()
            },
        );
        let mut options = StandardRenderConfig::default();
        options.defaults.value.max_chars = Some(40);
        options.per_tool.insert(
            id.clone(),
            ToolRenderPatch {
                value: RenderParamsPatch {
                    max_depth: Some(8),
                    ..RenderParamsPatch::default()
                },
                ..ToolRenderPatch::default()
            },
        );
        let resolved =
            resolve(&StandardRenderConfig::builtin(), &host, &options).expect("valid patch");
        assert_eq!(resolved.defaults.value.max_chars, 40);
        assert_eq!(resolved.defaults.max_lines, 12);
        assert_eq!(resolved.for_tool(&id).value.max_depth, 8);
        assert_eq!(
            resolved.for_tool(&id).authored_view,
            AuthoredViewPolicy::Ignore
        );
        assert_eq!(
            resolved
                .for_tool(&ToolId::new("tool:batch"))
                .value
                .max_depth,
            6
        );
        assert!(
            resolve(
                &StandardRenderConfig::builtin(),
                &StandardRenderConfig {
                    defaults: ToolRenderPatch {
                        head_share_percent: Some(101),
                        ..ToolRenderPatch::default()
                    },
                    ..StandardRenderConfig::default()
                },
                &StandardRenderConfig::default()
            )
            .is_err()
        );
    }

    #[test]
    fn authored_empty_view_wins_and_ignore_renders_the_structured_value() {
        let output = ToolCallOutput::success(serde_json::json!({"answer": 42}))
            .with_view(ToolView { blocks: vec![] });
        let preferred = builtin_tool_output(&output, &ToolRenderParams::default());
        assert!(preferred.body.is_empty());
        let ignored = builtin_tool_output(
            &output,
            &ToolRenderParams {
                authored_view: AuthoredViewPolicy::Ignore,
                ..ToolRenderParams::default()
            },
        );
        assert!(
            matches!(&ignored.body[0], ModelToolReturnPart::Text { text } if text.contains("answer"))
        );
    }

    #[test]
    fn resource_links_and_media_keep_their_positions() {
        let source = lash_core::AttachmentSource::stored(AttachmentRef::new(
            AttachmentId::parse("image").expect("id"),
            MediaType::parse("image/png").expect("type"),
            3,
            None,
            None,
        ));
        let meta = ToolViewMeta::default();
        let output = ToolCallOutput::success(serde_json::json!({})).with_view(ToolView {
            blocks: vec![
                ToolViewBlock::Text {
                    text: "before".into(),
                    meta: meta.clone(),
                },
                ToolViewBlock::Attachment {
                    source: source.clone(),
                    meta: meta.clone(),
                },
                ToolViewBlock::ResourceLink {
                    uri: "file:///a".into(),
                    name: "a".into(),
                    title: None,
                    description: None,
                    mime_type: Some("text/plain".into()),
                    meta,
                },
            ],
        });
        assert_eq!(
            builtin_tool_output(&output, &ToolRenderParams::default()).body,
            vec![
                ModelToolReturnPart::text("before"),
                ModelToolReturnPart::Attachment(source),
                ModelToolReturnPart::text("[resource: a <file:///a> text/plain]")
            ]
        );
    }

    #[test]
    fn structured_only_mcp_projection_renders_channel_details_at_default_depth() {
        let channels = serde_json::json!({
            "channels": [{"name": "engineering", "topic": "Build the product"}]
        });
        let output = ToolCallOutput::success_tool_value(lash_core::ToolValue::untrusted_json(
            serde_json::json!({"structuredContent":channels,"content":[]}),
        ))
        .with_projection_value(channels);
        let rendered = builtin_tool_output(&output, &ToolRenderParams::default());
        let text = lash_core::facade_support::tool_result_text(&rendered.body);
        assert!(text.contains("engineering"), "{text}");
        assert!(text.contains("Build the product"), "{text}");
        assert!(!text.contains("[Object]"), "{text}");
    }

    #[tokio::test]
    async fn default_character_and_line_limits_include_the_cut_notice() {
        let artifacts = Arc::new(Artifacts::default());
        let text = "line of output\n".repeat(1_500);
        let ctx = context(
            ToolCallOutput::success(text),
            ToolRenderParams::default(),
            Arc::clone(&artifacts),
        );
        let result = render_present(baseline(&ctx), &ctx, &ToolOutputRendererSlot::default())
            .await
            .expect("presented");
        let (chars, lines) = text_stats(&result.parts);
        assert!(chars <= 16_000, "{chars}");
        assert!(lines <= 400, "{lines}");
        assert!(
            lash_core::facade_support::tool_result_text(&result.parts).contains("[output cut:")
        );
        assert_eq!(artifacts.writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cuts_share_the_cap_and_retain_complete_text_once() {
        let artifacts = Arc::new(Artifacts::default());
        let full = "ab".repeat(600);
        let source = lash_core::AttachmentSource::stored(AttachmentRef::new(
            AttachmentId::parse("image").expect("id"),
            MediaType::parse("image/png").expect("type"),
            3,
            None,
            None,
        ));
        let output = ToolCallOutput::success(serde_json::json!({})).with_view(ToolView {
            blocks: vec![
                ToolViewBlock::Text {
                    text: full[..600].into(),
                    meta: ToolViewMeta::default(),
                },
                ToolViewBlock::Attachment {
                    source: source.clone(),
                    meta: ToolViewMeta::default(),
                },
                ToolViewBlock::Text {
                    text: full[600..].into(),
                    meta: ToolViewMeta::default(),
                },
            ],
        });
        let ctx = context(
            output,
            ToolRenderParams {
                value: RenderParams {
                    max_chars: 220,
                    ..RenderParams::default()
                },
                ..ToolRenderParams::default()
            },
            Arc::clone(&artifacts),
        );
        let result = render_present(baseline(&ctx), &ctx, &ToolOutputRendererSlot::default())
            .await
            .expect("presented");
        let text = result
            .parts
            .iter()
            .filter_map(|part| match part {
                ModelToolReturnPart::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<String>();
        assert!(text.chars().count() <= 220);
        assert!(text.contains("[output cut: showing") && text.contains("attachment full-output"));
        assert!(text.contains("0..") && text.contains("chars"));
        assert_eq!(
            result
                .parts
                .iter()
                .filter(|part| matches!(part, ModelToolReturnPart::Attachment(_)))
                .count(),
            1
        );
        assert_eq!(artifacts.writes.load(Ordering::SeqCst), 1);
        assert_eq!(
            artifacts.text.lock().expect("test mutex").as_slice(),
            &[full]
        );
    }

    #[tokio::test]
    async fn disabled_and_failed_retention_make_no_false_claim() {
        let output =
            ToolCallOutput::success_tool_value(lash_core::ToolValue::String("x".repeat(900)));
        let mut params = ToolRenderParams {
            value: RenderParams {
                max_chars: 180,
                ..RenderParams::default()
            },
            ..ToolRenderParams::default()
        };
        params.retain_full_output = false;
        let disabled = Arc::new(Artifacts::default());
        let ctx = context(output.clone(), params.clone(), Arc::clone(&disabled));
        let result = render_present(baseline(&ctx), &ctx, &ToolOutputRendererSlot::default())
            .await
            .expect("presented");
        assert!(
            lash_core::facade_support::tool_result_text(&result.parts).contains("not retained")
        );
        assert_eq!(disabled.writes.load(Ordering::SeqCst), 0);

        params.retain_full_output = true;
        let failed = Arc::new(Artifacts {
            fail: true,
            ..Artifacts::default()
        });
        let ctx = context(output, params, Arc::clone(&failed));
        let result = render_present(baseline(&ctx), &ctx, &ToolOutputRendererSlot::default())
            .await
            .expect("presented");
        let text = lash_core::facade_support::tool_result_text(&result.parts);
        assert!(text.contains("retention failed"));
        assert!(!text.contains("attachment full-output"));
        assert_eq!(failed.writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn mismatched_renderer_refuses_before_render_or_retention() {
        let artifacts = Arc::new(Artifacts::default());
        let mut ctx = context(
            ToolCallOutput::success("hello"),
            ToolRenderParams::default(),
            Arc::clone(&artifacts),
        );
        ctx.render.as_mut().expect("record").renderer_id = "other".into();
        let error = render_present(baseline(&ctx), &ctx, &ToolOutputRendererSlot::default())
            .await
            .expect_err("unavailable");
        assert_eq!(error.code, RuntimeErrorCode::RecordedRendererUnavailable);
        assert!(
            error
                .journal_disposition(lash_core::RuntimeEffectKind::PresentToolResult)
                .is_retryable_derivation()
        );
        assert_eq!(artifacts.writes.load(Ordering::SeqCst), 0);
    }
}
