use std::sync::Arc;

use lash_render::{CutKind, RenderParams, Rendered, render, truncate_chars};
use lash_rlm_types::RlmRenderPatch;

pub trait CodeRenderer: Send + Sync {
    fn id(&self) -> &str;

    fn print(&self, value: &lashlang::Value, params: &RenderParams) -> Rendered<String> {
        render(value, params)
    }

    fn variable_preview(&self, value: &lashlang::Value, params: &RenderParams) -> Rendered<String> {
        render(value, params)
    }
}

pub struct BuiltinCodeRenderer;

impl CodeRenderer for BuiltinCodeRenderer {
    fn id(&self) -> &str {
        "lash.ax.v1"
    }

    fn variable_preview(&self, value: &lashlang::Value, params: &RenderParams) -> Rendered<String> {
        let mut rendered = render(value, params);
        if matches!(value, lashlang::Value::String(_)) {
            rendered.body =
                serde_json::to_string(&rendered.body).unwrap_or_else(|_| "\"\"".to_string());
            rendered.cuts.original_chars = rendered.body.chars().count();
        }
        rendered
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
        Self {
            print: RenderParams::default(),
            preview: RenderParams::preview(),
        }
    }
}

impl ResolvedRlmRender {
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
    value: &lashlang::Value,
    params: &RenderParams,
    history_index: usize,
    print_index: usize,
    typed: serde_json::Value,
) -> Option<lash_sansio::Observation> {
    let rendered = truncate_chars(renderer.print(value, params), params.max_chars);
    if rendered.body.is_empty() {
        return None;
    }
    let projected_chars = rendered.body.chars().count();
    let projection = lash_sansio::TextProjectionMetadata {
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
            "[cut: {} chars rendered within {}; {}; narrow with history[{history_index}].output[{print_index}].<path>]\n{}",
            rendered.cuts.original_chars, params.max_chars, counts, rendered.body
        )
    };
    Some(lash_sansio::Observation {
        text,
        value: typed,
        projection,
    })
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
        let value = lashlang::Value::String("abcdef".into());
        let params = RenderParams {
            max_chars: 3,
            ..RenderParams::default()
        };
        let observation = rendered_print(
            &BuiltinCodeRenderer,
            &value,
            &params,
            4,
            2,
            serde_json::json!("abcdef"),
        )
        .expect("nonempty print");
        assert_eq!(observation.value, serde_json::json!("abcdef"));
        assert_eq!(
            observation.text,
            "[cut: 6 chars rendered within 3; chars 1; narrow with history[4].output[2].<path>]\nabc\n...[truncated 3 chars]"
        );
        assert_eq!(observation.projection.original_chars, 6);
        assert!(observation.projection.truncated);
    }

    #[test]
    fn render_options_overlay_host_fieldwise() {
        let host = RlmRenderPatch {
            print: lash_render::RenderParamsPatch {
                max_chars: Some(123),
                max_depth: Some(4),
                ..Default::default()
            },
            preview: lash_render::RenderParamsPatch {
                layout: Some(lash_render::Layout::Pretty),
                ..Default::default()
            },
        };
        let options = RlmRenderPatch {
            print: lash_render::RenderParamsPatch {
                max_depth: Some(1),
                ..Default::default()
            },
            ..Default::default()
        };
        let resolved = ResolvedRlmRender::resolve(&host, &options);
        assert_eq!(resolved.print.max_chars, 123);
        assert_eq!(resolved.print.max_depth, 1);
        assert_eq!(resolved.preview.layout, lash_render::Layout::Pretty);
        assert_eq!(resolved.preview.max_chars, 1000);
    }

    #[test]
    fn run_override_merges_render_fields_over_session_and_host() {
        let session = lash_core::ProtocolTurnOptions::typed(lash_rlm_types::RlmCreateExtras {
            render: Some(RlmRenderPatch {
                print: lash_render::RenderParamsPatch {
                    max_chars: Some(5),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        })
        .expect("session options");
        let turn = lash_core::ProtocolTurnOptions::typed(lash_rlm_types::RlmCreateExtras {
            render: Some(RlmRenderPatch {
                print: lash_render::RenderParamsPatch {
                    max_depth: Some(1),
                    ..Default::default()
                },
                ..Default::default()
            }),
            ..Default::default()
        })
        .expect("turn options");
        let merged = lash_core::RunOverrides {
            protocol_turn_options: Some(turn),
            ..Default::default()
        }
        .over(lash_core::RunOverrides {
            protocol_turn_options: Some(session),
            ..Default::default()
        })
        .protocol_turn_options
        .expect("merged options");
        let options = crate::rlm_support::decode_rlm_options(&merged).expect("RLM options");
        let resolved = ResolvedRlmRender::resolve(
            &RlmRenderPatch {
                print: lash_render::RenderParamsPatch {
                    max_chars: Some(9),
                    max_depth: Some(4),
                    ..Default::default()
                },
                ..Default::default()
            },
            &options.render.expect("render patch"),
        );
        assert_eq!(resolved.print.max_chars, 5);
        assert_eq!(resolved.print.max_depth, 1);
    }
}
