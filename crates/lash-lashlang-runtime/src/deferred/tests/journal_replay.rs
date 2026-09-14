use super::*;
use lashlang::testing::ast_builders as b;

#[tokio::test]
async fn recorded_unavailable_masks_incompatible_surface_before_environment_validation() {
    // await web.now({})?
    let program = b::program(vec![b::module_call(
        &["web"],
        "now",
        vec![b::record(Vec::new())],
    )]);
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation(
            ["web"],
            "typescript.Runtime",
            "now",
            "surface:web.now",
            lashlang::TypeExpr::Any,
            lashlang::TypeExpr::Bool,
        )
        .expect("later ambient binding is valid before runtime built-ins merge");
    let surface = LashlangSurface {
        resources,
        ..LashlangSurface::default()
    };
    let mut record = DeferredResolutionRecord::default();
    record.record("web.now", Resolution::NotAvailable);
    let ctx = link_context(&mut record);

    let effective = resolve_and_build_deferred_environment(
        &program,
        &surface,
        &lash_core::ToolCatalog::default(),
        None,
        &mut record,
        &ctx,
    )
    .await
    .expect("recorded negative masks incompatible ambient schema before validation");

    assert!(matches!(
        record.get("web.now"),
        Some(Resolution::NotAvailable)
    ));
    assert!(!effective.resources.provides_module_operation("web", "now"));
    assert!(
        effective
            .resources
            .provides_module_operation("__typescript_runtime", "now")
    );
}
