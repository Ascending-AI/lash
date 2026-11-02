use std::sync::Arc;

use lash_render::{CutKind, RenderParams, Rendered, render, render_preview, truncate_chars};
use lash_rlm_types::RlmRenderPatch;

pub trait CodeRenderer: Send + Sync {
    fn id(&self) -> &str;

    fn print(&self, value: &serde_json::Value, params: &RenderParams) -> Rendered<String> {
        render(value, params)
    }

    /// Return the final preview and cut report, including the character cut.
    /// The truncation notice lies outside `params.max_chars`.
    fn variable_preview(
        &self,
        value: &serde_json::Value,
        params: &RenderParams,
    ) -> Rendered<String> {
        render_preview(value, params, false)
    }
}

pub struct BuiltinCodeRenderer;

impl CodeRenderer for BuiltinCodeRenderer {
    fn id(&self) -> &str {
        "lash.ax.v1"
    }

    fn variable_preview(
        &self,
        value: &serde_json::Value,
        params: &RenderParams,
    ) -> Rendered<String> {
        render_preview(value, params, true)
    }
}

#[derive(Clone)]
pub struct CodeRendererSlot(pub Arc<dyn CodeRenderer>);

impl Default for CodeRendererSlot {
    fn default() -> Self {
        Self(Arc::new(BuiltinCodeRenderer))
    }
}

impl std::fmt::Debug for CodeRendererSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CodeRendererSlot")
            .field(&self.0.id())
            .finish()
    }
}

impl PartialEq for CodeRendererSlot {
    fn eq(&self, other: &Self) -> bool {
        self.0.id() == other.0.id()
    }
}

impl Eq for CodeRendererSlot {}

#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedRlmRender {
    pub print: RenderParams,
    pub preview: RenderParams,
}

impl Default for ResolvedRlmRender {
    fn default() -> Self {
        Self::standard()
    }
}

impl ResolvedRlmRender {
    /// Standard print and preview presets from `RenderParams`; no workload
    /// measurement establishes these cuts for every host.
    pub fn standard() -> Self {
        Self {
            print: RenderParams::default(),
            preview: RenderParams::preview(),
        }
    }
}

impl ResolvedRlmRender {
    /// A complete base, explicitly stating every resolved field. An empty
    /// `RlmRenderPatch` instead inherits this base.
    pub fn as_patch(&self) -> RlmRenderPatch {
        RlmRenderPatch {
            print: (&self.print).into(),
            preview: (&self.preview).into(),
        }
    }

    pub fn resolve(host: &RlmRenderPatch, options: &RlmRenderPatch) -> Self {
        let base = Self::default();
        Self {
            print: options.print.apply(&host.print.apply(&base.print)),
            preview: options.preview.apply(&host.preview.apply(&base.preview)),
        }
    }
}

pub(crate) fn rendered_print(
    renderer: &dyn CodeRenderer,
    value: &serde_json::Value,
    params: &RenderParams,
) -> lash_protocol_types::CellPrint {
    let rendered = truncate_chars(renderer.print(value, params), params.max_chars);
    let projected_chars = rendered.body.chars().count();
    let projection = lash_protocol_types::TextProjectionMetadata {
        truncated: !rendered.cuts.is_empty(),
        original_chars: rendered.cuts.original_chars,
        projected_chars,
        limit_chars: params.max_chars,
    };
    let text = if rendered.cuts.is_empty() {
        rendered.body
    } else {
        let counts = rendered
            .cuts
            .counts
            .iter()
            .filter(|(_, count)| **count != 0)
            .map(|(kind, count)| format!("{} {count}", cut_name(*kind)))
            .collect::<Vec<_>>()
            .join(", ");
        format!(
            "[cut: {} chars rendered within {}; {}]\n{}",
            rendered.cuts.original_chars, params.max_chars, counts, rendered.body
        )
    };
    lash_protocol_types::CellPrint {
        text,
        value: value.clone(),
        projection,
    }
}

fn cut_name(kind: CutKind) -> &'static str {
    match kind {
        CutKind::Array => "array",
        CutKind::Depth => "depth",
        CutKind::Item => "item",
        CutKind::Stack => "stack",
        CutKind::Chars => "chars",
        CutKind::Lines => "lines",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn print_keeps_typed_value_and_places_cut_header_outside_body_budget() {
        let value = serde_json::Value::String("abcdef".into());
        let params = RenderParams {
            max_chars: 3,
            ..RenderParams::default()
        };
        let observation = rendered_print(&BuiltinCodeRenderer, &value, &params);
        assert_eq!(observation.value, serde_json::json!("abcdef"));
        assert_eq!(
            observation.text,
            "[cut: 6 chars rendered within 3; chars 1]\nabc\n...[truncated 3 chars]"
        );
        assert_eq!(observation.projection.original_chars, 6);
        assert!(observation.projection.truncated);
    }
}
