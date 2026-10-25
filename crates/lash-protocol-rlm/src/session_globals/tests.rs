//! Laws of the session globals a prompt lists (FIG-1438): the listing keeps
//! to its recorded budget and counts the rest, and a continuation pages the
//! rest of exactly the snapshot it was taken from.

use lash_rlm_types::RlmGlobalsPatchPluginBody;

use super::StaleGlobalsPage;
use crate::dialect::{DialectSession, test_dialect_services};

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

async fn listing(session: &mut DialectSession) -> (std::sync::Arc<str>, super::SessionGlobals) {
    session
        .prepare_bound_variables_prompt(lash_render::RenderParams::preview(), None)
        .await
        .expect("render the session's globals")
        .render()
        .map(|(text, facts, _)| (text, facts))
        .expect("presentable bindings")
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
    let (text, globals) = listing(&mut session).await;
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
    let (_, changed) = listing(&mut session).await;
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
    let (_, first) = listing(&mut first).await;
    let mut other = session_with(&[
        ("a", serde_json::json!(1)),
        ("z", serde_json::json!("y".repeat(1 << 20))),
    ])
    .await;
    let (text, other) = listing(&mut other).await;
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

    let (_, all) = listing(&mut session_with(&values).await).await;
    assert_eq!(all.omitted(), 0);
    assert_eq!(all.continuation(), None);
}

async fn session_with(values: &[(&str, serde_json::Value)]) -> DialectSession {
    let mut session = session(usize::MAX);
    bind(&mut session, values).await;
    session
}
