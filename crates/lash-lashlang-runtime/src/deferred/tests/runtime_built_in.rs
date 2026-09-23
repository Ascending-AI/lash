use super::*;
use lashlang::testing::ast_builders as b;

struct EmptyResolver {
    calls: AtomicUsize,
    batches: Mutex<Vec<Vec<String>>>,
}

#[async_trait]
impl DeferredToolResolver for EmptyResolver {
    async fn resolve(&self, paths: &[&str]) -> BTreeMap<String, Resolution> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.batches
            .lock_recover()
            .push(paths.iter().map(|path| (*path).to_string()).collect());
        BTreeMap::new()
    }
}

#[tokio::test]
async fn runtime_built_in_survives_empty_deferred_resolution_and_premerge_masking() {
    let resolver = Arc::new(EmptyResolver {
        calls: AtomicUsize::new(0),
        batches: Mutex::new(Vec::new()),
    });
    let shared: SharedDeferredToolResolver = resolver.clone();
    let effect_host = fault_journal_host(JournalFault::None).await;
    // await triggers.list({})?
    // await web.fetch({})?
    let program = b::program(vec![
        b::module_call(&["triggers"], "list", vec![b::record(Vec::new())]),
        b::module_call(&["web"], "fetch", vec![b::record(Vec::new())]),
    ]);
    let surface = LashlangSurface {
        abilities: lashlang::LashlangAbilities::default(),
        ..LashlangSurface::default()
    };
    let mut first_record = DeferredResolutionRecord::default();
    let first_ctx = link_context_with_host(
        &mut first_record,
        "exec-code:built-in-and-deferred",
        effect_host.clone(),
    );

    let effective = resolve_and_build_deferred_environment(
        &program,
        &surface,
        &lash_core::ToolCatalog::default(),
        Some(&shared),
        &mut first_record,
        &first_ctx,
    )
    .await
    .expect("runtime built-in remains ambient while missing path is deferred");

    assert!(
        effective
            .resources
            .provides_module_operation("triggers", "list")
    );
    assert!(
        !effective
            .resources
            .provides_module_operation("web", "fetch")
    );
    assert!(first_record.get("triggers.list").is_none());
    assert!(matches!(
        first_record.get("web.fetch"),
        Some(Resolution::NotAvailable)
    ));
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        *resolver.batches.lock_recover(),
        vec![vec!["web.fetch".to_string()]]
    );

    let changed_surface = surface_with_shared_fetch_modules(&["web"]);
    let catalog = incompatible_shared_fetch_catalog();
    let mut replayed_record = DeferredResolutionRecord::default();
    let replay_ctx = link_context_with_host(
        &mut replayed_record,
        "exec-code:built-in-and-deferred",
        effect_host,
    );

    let replayed = resolve_and_build_deferred_environment(
        &program,
        &changed_surface,
        &catalog,
        Some(&shared),
        &mut replayed_record,
        &replay_ctx,
    )
    .await
    .expect("journaled path is masked before catalog merge on replay");

    assert!(
        replayed
            .resources
            .provides_module_operation("triggers", "list")
    );
    assert!(!replayed.resources.provides_module_operation("web", "fetch"));
    assert!(
        replayed
            .resources
            .provides_module_operation("catalog", "fetch")
    );
    assert!(replayed_record.get("triggers.list").is_none());
    assert!(matches!(
        replayed_record.get("web.fetch"),
        Some(Resolution::NotAvailable)
    ));
    assert_eq!(resolver.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn retained_negative_masks_duplicate_catalog_claimants_before_live_validation() {
    let program = web_fetch_program();
    let duplicate_catalog = lash_core::ToolCatalog::from_tool_definitions(vec![
        lash_core::ToolDefinition::raw(
            "tool:first_fetch",
            "first_fetch",
            "First fetch",
            lash_core::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "boolean" }),
        )
        .with_tool_binding(ToolBinding::new(["web"], "fetch")),
        lash_core::ToolDefinition::raw(
            "tool:second_fetch",
            "second_fetch",
            "Second fetch",
            lash_core::ToolDefinition::default_input_schema(),
            serde_json::json!({ "type": "boolean" }),
        )
        .with_tool_binding(ToolBinding::new(["web"], "fetch")),
    ]);

    let mut retained = DeferredResolutionRecord::default();
    let retained_ctx = link_context_with_host(
        &mut retained,
        "exec-code:retained-negative",
        fault_journal_host(JournalFault::None).await,
    );
    retained
        .resolutions
        .insert("web.fetch".to_string(), Resolution::NotAvailable);
    let effective = resolve_and_build_deferred_environment(
        &program,
        &LashlangSurface::default(),
        &duplicate_catalog,
        None,
        &mut retained,
        &retained_ctx,
    )
    .await
    .expect("retained negative masks duplicate claimants before validation");

    assert!(
        !effective
            .resources
            .provides_module_operation("web", "fetch")
    );
    assert!(matches!(
        retained.get("web.fetch"),
        Some(Resolution::NotAvailable)
    ));

    let mut fresh = DeferredResolutionRecord::default();
    let fresh_ctx = link_context_with_host(
        &mut fresh,
        "exec-code:fresh-duplicate",
        fault_journal_host(JournalFault::None).await,
    );
    let error = resolve_and_build_deferred_environment(
        &program,
        &LashlangSurface::default(),
        &duplicate_catalog,
        None,
        &mut fresh,
        &fresh_ctx,
    )
    .await
    .expect_err("fresh duplicate claimants must still fail validation");

    assert!(matches!(error, DeferredResolutionError::Ambient(_)));
}
