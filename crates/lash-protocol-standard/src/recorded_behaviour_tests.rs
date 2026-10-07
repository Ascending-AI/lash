//! FIG-4398 laws: a session runs under the standard-protocol behaviour it
//! recorded at creation — its discovery operation and its `batch` choice and
//! maximum — never under the configuration of the deployment that opens,
//! redrives or resumes it (ADR 0105 §1).

use super::*;

/// A recorded namespace under `render` options, with the default behaviour.
fn recorded_under(render: Option<StandardRenderConfig>) -> StandardRecordedConfig {
    StandardRecordedConfig {
        render,
        behaviour: StandardProtocolConfig::default().recorded_behaviour(),
    }
}

fn defaults(max_lines: Option<usize>, head_share_percent: Option<u8>) -> StandardRenderConfig {
    StandardRenderConfig {
        defaults: render::ToolRenderPatch {
            max_lines,
            head_share_percent,
            ..render::ToolRenderPatch::default()
        },
        ..StandardRenderConfig::default()
    }
}

fn standard_owner() -> StandardConfigOwner {
    StandardConfigOwner {
        behaviour: StandardProtocolConfig::default().recorded_behaviour(),
    }
}

/// FIG-4652: the owner lays a run's render options over the session's,
/// field by field and tool by tool, and nothing else of the namespace moves.
#[test]
fn run_options_apply_over_the_recorded_render_field_by_field() {
    let tool = lash_core::ToolId::new("tool:a");
    let mut session = defaults(Some(10), Some(40));
    session.per_tool.insert(
        tool.clone(),
        render::ToolRenderPatch {
            max_lines: Some(3),
            ..render::ToolRenderPatch::default()
        },
    );
    let recorded = recorded_under(Some(session));
    let mut stated = defaults(None, Some(60));
    stated.per_tool.insert(
        tool.clone(),
        render::ToolRenderPatch {
            head_share_percent: Some(25),
            ..render::ToolRenderPatch::default()
        },
    );
    let applied = standard_owner()
        .apply_run_options(
            &recorded,
            StandardRunOptions {
                render: Some(stated),
            },
        )
        .expect("the run's render options apply");
    let render = applied.render.clone().expect("render options");
    assert_eq!(render.defaults.max_lines, Some(10), "the session's stays");
    assert_eq!(
        render.defaults.head_share_percent,
        Some(60),
        "the run's wins"
    );
    assert_eq!(
        (
            render.per_tool[&tool].max_lines,
            render.per_tool[&tool].head_share_percent
        ),
        (Some(3), Some(25))
    );
    assert_eq!(
        applied,
        StandardRecordedConfig {
            render: applied.render.clone(),
            ..recorded.clone()
        },
        "the behaviour stays as recorded"
    );
    assert_eq!(
        standard_owner()
            .apply_run_options(&recorded, StandardRunOptions::default())
            .expect("empty options apply"),
        recorded
    );
}

/// FIG-4652: the driver reads the run's namespace as the recorded type. A
/// render it refuses is a typed refusal in the protocol's own type, and a
/// namespace it cannot read is corruption, not a refused shape.
#[test]
fn a_refused_render_is_typed_and_an_unreadable_namespace_is_corruption() {
    let driver = StandardProtocolDriver {
        config: StandardProtocolConfig::default(),
    };
    let namespace = |recorded: &StandardRecordedConfig| {
        lash_core::ProtocolTurnOptions::typed(recorded).expect("the namespace encodes")
    };
    driver
        .resolve_render(&namespace(&recorded_under(Some(defaults(
            Some(10),
            Some(50),
        )))))
        .expect("a head share within range resolves")
        .expect("the standard protocol records a render");

    let refused = driver
        .resolve_render(&namespace(&recorded_under(Some(defaults(None, Some(101))))))
        .expect_err("a head share over 100 is refused");
    let lash_core::RenderFault::Refused(refusal) = refused else {
        panic!("the render is refused: {refused:?}");
    };
    assert_eq!(
        serde_json::from_value::<StandardRenderRefusal>(refusal.refusal.clone())
            .expect("the protocol's own refusal type"),
        StandardRenderRefusal::HeadShareOutOfRange {
            head_share_percent: 101
        }
    );
    assert_eq!(
        refusal.message,
        StandardRenderRefusal::HeadShareOutOfRange {
            head_share_percent: 101
        }
        .to_string()
    );

    let corrupt = driver
        .resolve_render(&lash_core::ProtocolTurnOptions::from_payload(
            serde_json::json!({ "render": { "defaults": {} } }),
        ))
        .expect_err("a namespace without its behaviour is unreadable");
    assert!(
        matches!(
            &corrupt,
            lash_core::RenderFault::RecordedCorrupt(error)
                if error.owner == STANDARD_PROTOCOL_PLUGIN_ID
        ),
        "{corrupt:?}"
    );
}
