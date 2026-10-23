//! Laws of the session globals a prompt lists (FIG-1438): each names the
//! cell whose committed transition last bound it, the listing keeps to its
//! recorded budget and counts the rest, and a continuation pages the rest
//! of exactly the snapshot it was taken from.

use lash_core::{BindingChanges, CellRecord, SessionHistoryRecord};
use lash_rlm_types::{RlmGlobalsPatchPluginBody, RlmProtocolEvent};

use super::{SessionGlobalKind, StaleGlobalsPage};
use crate::dialect::{DialectSession, test_dialect_services};

fn cell(id: &str, bindings: BindingChanges) -> SessionHistoryRecord {
    SessionHistoryRecord::Protocol(crate::rlm_protocol_event(
        RlmProtocolEvent::RlmTrajectoryEntry(std::sync::Arc::new(CellRecord {
            id: id.to_string(),
            language: "typescript".to_string(),
            code: String::new(),
            bindings,
            result: lash_core::CellOutcome::Completed,
            ..CellRecord::default()
        })),
        lash_core::FleetFormat::current().writer_version(lash_core::surface_format!(
            crate::RLM_PROTOCOL_EVENT_VERSION
        )),
    ))
}

fn names(names: &[&str]) -> Vec<String> {
    names.iter().map(|name| name.to_string()).collect()
}

fn session(max_bound_variable_bytes: usize) -> DialectSession {
    let mut services = test_dialect_services();
    services.presentation.max_bound_variable_bytes = max_bound_variable_bytes;
    DialectSession::new(crate::dialect::CellDialect::typescript(), services)
}

async fn bind(session: &mut DialectSession, globals: &[(&str, serde_json::Value)]) {
    session
        .patch_globals(&RlmGlobalsPatchPluginBody {
            set_default: globals
                .iter()
                .map(|(name, value)| (name.to_string(), value.clone()))
                .collect(),
        })
        .await
        .expect("bind session globals");
}

async fn listing(
    session: &mut DialectSession,
    provenance: std::collections::BTreeMap<String, String>,
) -> (std::sync::Arc<str>, super::SessionGlobals) {
    session
        .prepare_bound_variables_prompt(lash_render::RenderParams::preview(), provenance, None)
        .await
        .expect("render the session's globals")
        .render()
        .map(|(text, facts, _)| (text, facts))
        .expect("presentable bindings")
}

/// A global names the cell whose committed record last added, changed or
/// left it, read from the cell records of the frame: a later change moves
/// it, a removal forgets it, and a global no record names has none.
#[tokio::test]
async fn a_global_names_the_cell_that_last_bound_it() {
    let events = [
        cell(
            "lash_vm_step_t_0",
            BindingChanges {
                added: names(&["rows", "scratch"]),
                ..Default::default()
            },
        ),
        cell(
            "lash_vm_step_t_1",
            BindingChanges {
                changed: names(&["rows"]),
                removed: names(&["scratch"]),
                ..Default::default()
            },
        ),
        cell(
            "lash_vm_step_t_2",
            BindingChanges {
                not_carried: names(&["pending"]),
                ..Default::default()
            },
        ),
    ];
    let projection = lash_core::facade_support::ChronologicalProjection::from_turn_view(
        &events,
        &Default::default(),
    );
    let provenance =
        crate::dialect::SessionDialect::prompt_only(crate::dialect::CellDialect::typescript())
            .projection_index(&projection)
            .expect("valid cell records")
            .provenance;
    assert_eq!(
        provenance.into_iter().collect::<Vec<_>>(),
        [
            ("pending".to_string(), "lash_vm_step_t_2".to_string()),
            ("rows".to_string(), "lash_vm_step_t_1".to_string()),
        ]
    );

    let mut session = session(usize::MAX);
    bind(
        &mut session,
        &[
            ("rows", serde_json::json!([1, 2, 3])),
            ("seeded", serde_json::json!("by the host")),
        ],
    )
    .await;
    let (text, globals) = listing(
        &mut session,
        crate::dialect::SessionDialect::prompt_only(crate::dialect::CellDialect::typescript())
            .projection_index(&projection)
            .expect("valid cell records")
            .provenance,
    )
    .await;
    insta::assert_snapshot!(text, @r#"
    These variables are already bound in TypeScript. Access them directly in `<typescript>` cells; do not recreate them manually.
    Never write a field name you haven't seen in the key sets below — guessed field names silently produce zeros rather than errors. If a name is not listed, it does not exist on that value.

    Available variables:
    - `rows` = [1,2,3] — from cell `lash_vm_step_t_1`
    - `seeded` = "by the host"
    "#);
    assert!(
        text.contains("- `rows` = [1,2,3] — from cell `lash_vm_step_t_1`\n"),
        "{text}"
    );
    assert!(text.ends_with("- `seeded` = \"by the host\""), "{text}");
    let rows = globals.get("rows").expect("the rows global");
    assert_eq!(rows.cell.as_deref(), Some("lash_vm_step_t_1"));
    assert_eq!(
        rows.kind,
        SessionGlobalKind::Data(serde_json::json!([1, 2, 3]))
    );
    assert_eq!(globals.get("seeded").expect("seeded").cell, None);
}

/// The listing keeps to its recorded budget: it shows the globals that fit,
/// in name order, and counts the rest. The continuation it leaves pages
/// exactly the omitted remainder, and a continuation of other globals is
/// refused.
#[tokio::test]
async fn paging_returns_exactly_the_globals_the_listing_left_out() {
    let mut session = session(64);
    let values = (0..20)
        .map(|index| (format!("v{index:02}"), serde_json::json!(index)))
        .collect::<Vec<_>>();
    let values = values
        .iter()
        .map(|(name, value)| (name.as_str(), value.clone()))
        .collect::<Vec<_>>();
    bind(&mut session, &values).await;
    let (text, globals) = listing(&mut session, Default::default()).await;
    insta::assert_snapshot!(text, @r"
    These variables are already bound in TypeScript. Access them directly in `<typescript>` cells; do not recreate them manually.
    Never write a field name you haven't seen in the key sets below — guessed field names silently produce zeros rather than errors. If a name is not listed, it does not exist on that value.

    Available variables:
    - `v00` = 0
    - `v01` = 1
    - `v02` = 2
    - `v03` = 3
    - `v04` = 4
    15 more variables are not listed here; each is still in scope under its name, and `console.log(name)` shows one.
    ");

    let listed = globals.listed().len();
    assert!(0 < listed && listed < 20, "{text}");
    assert_eq!(globals.omitted(), 20 - listed);
    assert!(
        text.contains(&format!(
            "{} more variables are not listed here; each is still in scope under its name, and `console.log(name)` shows one.",
            20 - listed
        )),
        "{text}"
    );
    let last = &globals.listed()[listed - 1].name;
    let first_omitted = &globals.entries()[listed].name;
    assert!(text.contains(&format!("- `{last}` = ")), "{text}");
    assert!(!text.contains(&format!("- `{first_omitted}` = ")), "{text}");

    let from = globals.continuation().expect("a listing that omits");
    assert_eq!(from.offset(), listed);
    let rest = globals.page(from, usize::MAX).expect("a current page");
    assert_eq!(rest.entries, &globals.entries()[listed..]);
    assert_eq!(rest.next, None);
    let first = globals.page(from, 3).expect("a current page");
    assert_eq!(first.entries, &globals.entries()[listed..listed + 3]);
    let second = globals
        .page(first.next.expect("more after the first page"), usize::MAX)
        .expect("a current page");
    assert_eq!(second.entries, &globals.entries()[listed + 3..]);

    bind(&mut session, &[("w", serde_json::json!("new"))]).await;
    let (_, changed) = listing(&mut session, Default::default()).await;
    assert_eq!(changed.page(from, 1), Err(StaleGlobalsPage));

    // A change beyond the read budget still changes the prompt cut, even
    // when the names, kinds and extents of both globals are identical.
    let mut first = self::session(64);
    bind(
        &mut first,
        &[
            ("a", serde_json::json!(1)),
            ("z", serde_json::json!("x".repeat(1 << 20))),
        ],
    )
    .await;
    let (_, first) = listing(&mut first, Default::default()).await;
    let mut other = session_with(&[
        ("a", serde_json::json!(1)),
        ("z", serde_json::json!("y".repeat(1 << 20))),
    ])
    .await;
    let (text, other) = listing(&mut other, Default::default()).await;
    assert!(
        text.contains("preview exceeds the binding read budget"),
        "{text}"
    );
    assert_eq!(
        other.page(
            first.continuation().expect("the large global is omitted"),
            1
        ),
        Err(StaleGlobalsPage)
    );

    let (_, all) = listing(&mut session_with(&values).await, Default::default()).await;
    assert_eq!(all.omitted(), 0);
    assert_eq!(all.continuation(), None);
}

async fn session_with(values: &[(&str, serde_json::Value)]) -> DialectSession {
    let mut session = session(usize::MAX);
    bind(&mut session, values).await;
    session
}
