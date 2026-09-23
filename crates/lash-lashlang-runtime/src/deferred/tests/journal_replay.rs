use super::*;
use lashlang::testing::ast_builders as b;

#[tokio::test]
async fn recorded_unavailable_masks_incompatible_surface_before_environment_validation() {
    // await web.now({})?
    let program = b::program(vec![b::module_call(
        &["web"],
        lash_typescript::TYPESCRIPT_RUNTIME_NOW_OPERATION,
        vec![b::record(Vec::new())],
    )]);
    let mut resources = lashlang::LashlangHostCatalog::new();
    resources
        .add_module_operation(
            ["web"],
            lash_typescript::TYPESCRIPT_RUNTIME_RESOURCE_TYPE,
            lash_typescript::TYPESCRIPT_RUNTIME_NOW_OPERATION,
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
    let ctx = link_context(&mut record).await;

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
    assert!(
        !effective
            .resources
            .provides_module_operation("web", lash_typescript::TYPESCRIPT_RUNTIME_NOW_OPERATION)
    );
    assert!(effective.resources.provides_module_operation(
        lash_typescript::TYPESCRIPT_RUNTIME_MODULE_PATH,
        lash_typescript::TYPESCRIPT_RUNTIME_NOW_OPERATION,
    ));
}
