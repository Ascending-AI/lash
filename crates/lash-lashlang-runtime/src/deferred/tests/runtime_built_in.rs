use super::*;

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
    let controller = Arc::new(FaultJournalController::new(JournalFault::None));
    let program = lashlang::parse("await triggers.list({})?\nawait web.fetch({})?").expect("parse");
    let surface = LashlangSurface {
        abilities: lashlang::LashlangAbilities::default().with_triggers(),
        ..LashlangSurface::default()
    };
    let mut first_record = DeferredResolutionRecord::default();
    let first_ctx = link_context_with_controller(
        &mut first_record,
        "exec-code:built-in-and-deferred",
        controller.clone(),
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

    let mut changed_surface = surface_with_shared_fetch_modules(&["web"]);
    changed_surface.abilities = changed_surface.abilities.with_triggers();
    let catalog = incompatible_shared_fetch_catalog();
    let mut replayed_record = DeferredResolutionRecord::default();
    let replay_ctx = link_context_with_controller(
        &mut replayed_record,
        "exec-code:built-in-and-deferred",
        controller,
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
