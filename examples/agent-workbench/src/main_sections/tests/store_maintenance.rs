use super::*;

// Coverage for `/api/admin/store-maintenance`: the two levers that bound
// session-store growth, and — the point of the route — the destruction it
// refuses to perform.

#[test]
fn store_maintenance_is_absent_from_the_workbench_ui() {
    assert!(
        !ui::INDEX_HTML.contains("/api/admin/store-maintenance"),
        "store maintenance is operator-only: it must never be one click away"
    );
}
