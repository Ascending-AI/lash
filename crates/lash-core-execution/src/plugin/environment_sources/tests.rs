use super::super::packages::{PackageProvider, operation_environment_fragment};
use super::*;
use lash_core_store::environment::{AssemblyRefusal, assemble};
use lash_kernel_value::names::Segment;
use lash_vocabulary::{ToolContract, ToolDefinition, ToolManifest};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Default)]
struct Calls {
    manifests: AtomicUsize,
    contracts: AtomicUsize,
    packages: AtomicUsize,
    holds: AtomicUsize,
}
impl Calls {
    fn take(&self) -> [usize; 4] {
        [
            &self.manifests,
            &self.contracts,
            &self.packages,
            &self.holds,
        ]
        .map(|n| n.swap(0, Ordering::SeqCst))
    }
}
struct Provider {
    calls: Arc<Calls>,
    definition: Mutex<ToolDefinition>,
    mode: SourceMode,
    during_snapshot: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    projection_members: Mutex<BTreeSet<super::super::ToolProviderKey>>,
}
impl Provider {
    fn new(name: &str, mode: SourceMode) -> Arc<Self> {
        Arc::new(Self {
            calls: Arc::new(Calls::default()),
            definition: Mutex::new(
                ToolDefinition::raw(
                    format!("{name}-tool"),
                    format!("{name}.work"),
                    "fixture",
                    serde_json::json!({"type":"object"}),
                    serde_json::json!({"type":"null"}),
                )
                .unwrap()
                .with_execution(std::time::Duration::from_secs(1)),
            ),
            mode,
            during_snapshot: Mutex::new(None),
            projection_members: Mutex::new(BTreeSet::new()),
        })
    }
}
#[async_trait::async_trait]
impl crate::ToolProvider for Provider {
    fn environment_mode(&self) -> SourceMode {
        self.mode
    }
    fn environment_namespace_dependencies(&self) -> BTreeSet<String> {
        ["state".into()].into()
    }
    fn tool_manifests(&self, _: &OwnerConfigView) -> Result<Vec<ToolManifest>, PluginError> {
        self.calls.manifests.fetch_add(1, Ordering::SeqCst);
        if let Some(hook) = self.during_snapshot.lock_recover().take() {
            hook();
        }
        Ok(vec![self.definition.lock_recover().manifest.clone()])
    }
    fn resolve_contract(&self, _: &str, _: &OwnerConfigView) -> Option<Arc<ToolContract>> {
        self.calls.contracts.fetch_add(1, Ordering::SeqCst);
        Some(Arc::new(self.definition.lock_recover().contract.clone()))
    }
    async fn execute(&self, _: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        unreachable!()
    }
}
#[async_trait::async_trait]
impl PackageProvider for Provider {
    fn environment_tool_sources(
        &self,
        publisher: &ToolProviderHandle,
    ) -> BTreeSet<ToolProviderHandle> {
        self.projection_members
            .lock_recover()
            .iter()
            .map(|key| ToolProviderHandle::new(publisher.registry(), publisher.owner(), *key))
            .collect()
    }

    fn environment_mode(&self) -> SourceMode {
        self.mode
    }
    fn roots(&self, _: &OwnerConfigView) -> BTreeSet<Segment> {
        unreachable!("shared observation supplies roots")
    }
    fn advertise(
        &self,
        _: &OwnerConfigView,
        _: &lash_kernel_value::package::PublicationContext,
    ) -> Result<Vec<super::super::packages::PackageContribution>, PluginError> {
        unreachable!("shared observation supplies packages")
    }
    fn environment_fragment(
        &self,
        config: &OwnerConfigView,
        domain: &PublicationDomain,
        scope: &PublicationScope,
        key: &ToolProviderHandle,
        tools: Option<&[crate::SurfaceEntry]>,
    ) -> Result<EnvironmentFragment, PluginError> {
        self.calls.packages.fetch_add(1, Ordering::SeqCst);
        let tools = tools.unwrap();
        let roots = tools
            .iter()
            .map(|entry| entry.definition.manifest.operation.package.as_slice()[0].clone())
            .collect();
        operation_environment_fragment(tools, roots, config, domain, scope, key)
    }
    async fn hold(
        &self,
        _: &lash_core_store::artifact_referrer::ReferrerClaim,
        _: &[&lash_kernel_value::package::PackageAdvertisement],
    ) -> Result<(), lash_execution_contracts::process::linking::EnginePortError> {
        self.calls.holds.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
fn key(name: &str) -> ToolProviderHandle {
    ToolProviderHandle::new(
        super::super::RegistryKind::Session,
        name,
        crate::tool_provider_key!("tools"),
    )
}
fn fixtures(mode: SourceMode) -> (EnvironmentSources, Arc<Provider>, Arc<Provider>) {
    let a = Provider::new("a", mode);
    let b = Provider::new("b", mode);
    let registry = Arc::new(
        crate::ToolRegistry::from_tool_registrations(
            vec![(key("a"), a.clone()), (key("b"), b.clone())],
            None,
            BTreeMap::new(),
            Vec::new(),
        )
        .unwrap(),
    );
    let mut sources = EnvironmentSources::default();
    for (name, provider) in [("a", a.clone()), ("b", b.clone())] {
        sources
            .register(
                Arc::new(ProviderSource {
                    key: key(name),
                    tools: [key(name)].into(),
                    subscriptions: Mutex::new(Vec::new()),
                    registry: registry.clone(),
                    packages: [(key(name), provider as Arc<dyn PackageProvider>)].into(),
                    domain: PublicationDomain("fixture".into()),
                    scope: PublicationScope::Host,
                    namespaces: None,
                    revision: Mutex::new(SourceRevision {
                        incarnation: uuid::Uuid::new_v4(),
                        counter: 0,
                    }),
                }),
                Arc::new(|_| {}),
            )
            .unwrap();
    }
    (sources, a, b)
}
fn frontier(snapshots: &[SourceSnapshot]) -> InputFrontier {
    snapshots
        .iter()
        .map(|snapshot| (snapshot.source.clone(), snapshot.revision.clone()))
        .collect()
}
fn notice(source: &str, frontier: &InputFrontier, advance: u64) -> EnvironmentChange {
    let source = provider_source_id(&key(source));
    let mut revision = frontier[&source].clone();
    revision.counter += advance;
    EnvironmentChange::SourceRevision { source, revision }
}

/// ENV-DELTA: one enumeration supplies both projections, with no callback or
/// hold on clean B and structural reuse of B's prepared fragment.
#[test]
fn env_delta_collector_shares_tools_and_packages_and_never_asks_clean_b() {
    let (sources, a, b) = fixtures(SourceMode::Notifying);
    let config = OwnerConfigView::default();
    let initial = sources.collect(&InputFrontier::new(), [], &config).unwrap();
    let before = assemble(None, initial.clone()).unwrap();
    assert_eq!(a.calls.take(), [1, 1, 1, 0]);
    assert_eq!(b.calls.take(), [1, 1, 1, 0]);
    let frontier = frontier(&initial);
    let delta = sources
        .collect(&frontier, [notice("a", &frontier, 1)], &config)
        .unwrap();
    assert_eq!(delta.len(), 1);
    assert_eq!(
        delta[0].fragment.tools[0].link,
        delta[0].fragment.routes[0].route
    );
    assert_eq!(a.calls.take(), [1, 1, 1, 0]);
    assert_eq!(b.calls.take(), [0; 4]);
    let after = assemble(Some(&before), delta).unwrap();
    let b = provider_source_id(&key("b"));
    assert!(Arc::ptr_eq(&before.fragments()[&b], &after.fragments()[&b]));
    assert!(sources.collect(&frontier, [], &config).unwrap().is_empty());
}

/// ENV-DELTA: duplicate/older notices cannot regress an adopted revision.
#[test]
fn notices_coalesce_and_never_regress_the_frontier() {
    let (sources, a, b) = fixtures(SourceMode::Notifying);
    let config = OwnerConfigView::default();
    let initial = sources.collect(&InputFrontier::new(), [], &config).unwrap();
    let old = frontier(&initial);
    a.calls.take();
    b.calls.take();
    let delta = sources
        .collect(
            &old,
            [
                notice("a", &old, 1),
                notice("a", &old, 3),
                notice("a", &old, 1),
            ],
            &config,
        )
        .unwrap();
    assert_eq!(delta.len(), 1);
    assert_eq!(a.calls.take(), [1, 1, 1, 0]);
    assert_eq!(b.calls.take(), [0; 4]);
    let mut adopted = old.clone();
    adopted.extend(frontier(&delta));
    assert!(
        sources
            .collect(
                &adopted,
                [notice("a", &old, 0), notice("a", &old, 1)],
                &config
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(a.calls.take(), [0; 4]);
}

/// ENV-DELTA: an A replacement colliding with clean B is a typed refusal.
#[test]
fn replacement_collision_preserves_the_published_candidate() {
    let (sources, a, _) = fixtures(SourceMode::Notifying);
    let config = OwnerConfigView::default();
    let initial = sources.collect(&InputFrontier::new(), [], &config).unwrap();
    let before = assemble(None, initial.clone()).unwrap();
    let frontier = frontier(&initial);
    a.definition.lock_recover().manifest.operation =
        lash_kernel_value::names::QualifiedOperation::from_effect(
            &lash_kernel_value::EffectName::new("b.work").unwrap(),
        )
        .unwrap();
    let delta = sources
        .collect(&frontier, [notice("a", &frontier, 1)], &config)
        .unwrap();
    assert!(matches!(
        assemble(Some(&before), delta),
        Err(AssemblyRefusal::OperationCollision { .. })
    ));
    // Explicit config validation must settle the same collision as an owner
    // refusal, preserving its typed assembly cause across the plugin boundary.
    let error = sources
        .initial_body(&config)
        .unwrap_err()
        .into_plugin_error();
    let PluginError::Runtime(error) = error else {
        panic!("assembly refusal lost its owner")
    };
    let Some(crate::RunShapeRefusal::Owner { refusal }) = error.run_shape_refusal() else {
        panic!("assembly refusal lost its recorded shape cause")
    };
    assert!(matches!(
        refusal.owner_refusal::<lash_execution_contracts::config_error::CoreConfigRefusal>(),
        Some(lash_execution_contracts::config_error::CoreConfigRefusal::EnvironmentAssemblyRefused { refusal })
            if matches!(refusal.as_ref(), AssemblyRefusal::OperationCollision { .. })
    ));
    assert_eq!(before.fragments().len(), 2);
}

/// ENV-DELTA: RefreshOnly ignores external notices; explicit refresh and
/// committed config/namespace dependency events affect only their consumers.
#[test]
fn refresh_only_and_dependency_commits_have_explicit_scope() {
    let (sources, a, b) = fixtures(SourceMode::RefreshOnly);
    let config = OwnerConfigView::default();
    let initial = sources.collect(&InputFrontier::new(), [], &config).unwrap();
    let frontier = frontier(&initial);
    a.calls.take();
    b.calls.take();
    assert!(
        sources
            .collect(&frontier, [notice("a", &frontier, 1)], &config)
            .unwrap()
            .is_empty()
    );
    assert_eq!(a.calls.take(), [0; 4]);
    let id = provider_source_id(&key("a"));
    sources
        .collect(
            &frontier,
            [EnvironmentChange::Refresh {
                sources: Some([id.clone()].into()),
            }],
            &config,
        )
        .unwrap();
    assert_eq!(a.calls.take(), [1, 1, 1, 0]);
    assert_eq!(b.calls.take(), [0; 4]);
    assert_eq!(
        sources.config_committed(["a"]),
        EnvironmentChange::ConfigSelection {
            sources: [id].into()
        }
    );
    sources
        .collect(&frontier, [sources.config_committed(["a"])], &config)
        .unwrap();
    assert_eq!(a.calls.take(), [1, 1, 1, 0]);
    assert_eq!(b.calls.take(), [0; 4]);
    assert_eq!(
        sources.namespace_committed(["state"]),
        EnvironmentChange::ConfigSelection {
            sources: [provider_source_id(&key("a")), provider_source_id(&key("b"))].into()
        }
    );
    let (immutable, a, b) = fixtures(SourceMode::Immutable);
    let initial = immutable
        .collect(&InputFrontier::new(), [], &config)
        .unwrap();
    a.calls.take();
    b.calls.take();
    assert!(
        immutable
            .collect(
                &self::frontier(&initial),
                [EnvironmentChange::Refresh { sources: None }],
                &config
            )
            .unwrap()
            .is_empty()
    );
    assert_eq!(a.calls.take(), [0; 4]);
    assert_eq!(b.calls.take(), [0; 4]);
}

/// ENV-DELTA: republication installs new immutable keys and one mailbox notice;
/// stale-incarnation mail reconciles to the current snapshot without regression.
#[test]
fn published_packages_keep_old_keys_and_reconcile_stale_incarnations() {
    fn fragment(boolean: bool) -> EnvironmentFragment {
        let definition = ToolDefinition::raw(
            "a-tool",
            "a.work",
            "fixture",
            serde_json::json!({"type":"object"}),
            serde_json::json!({"type": if boolean { "boolean" } else { "null" }}),
        )
        .unwrap()
        .with_execution(std::time::Duration::from_secs(1));
        let tools = vec![crate::SurfaceEntry::advertised(key("a"), definition, None)];
        let mut fragment = operation_environment_fragment(
            &tools,
            [Segment::new("a").unwrap()].into(),
            &OwnerConfigView::default(),
            &PublicationDomain("fixture".into()),
            &PublicationScope::Host,
            &key("a"),
        )
        .unwrap();
        fragment.tools = tools;
        fragment
    }
    let source = Arc::new(PublishedEnvironmentSource::new(
        SourceId("published".into()),
        fragment(false),
    ));
    let mailbox = Arc::new(Mutex::new(Vec::new()));
    let target = mailbox.clone();
    let sink: EnvironmentChangeSink = Arc::new(move |change| target.lock_recover().push(change));
    let mut sources = EnvironmentSources::default();
    sources.register(source.clone(), sink.clone()).unwrap();
    let config = OwnerConfigView::default();
    let initial = sources.collect(&InputFrontier::new(), [], &config).unwrap();
    let old = frontier(&initial);
    let key = initial[0].fragment.package_roots[0].publication.key();
    source.publish(fragment(false)).unwrap();
    assert!(mailbox.lock_recover().is_empty());
    source.publish(fragment(true)).unwrap();
    let notices = std::mem::take(&mut *mailbox.lock_recover());
    assert_eq!(notices.len(), 1);
    let delta = sources.collect(&old, notices, &config).unwrap();
    assert_eq!(delta.len(), 1);
    assert_ne!(key, delta[0].fragment.package_roots[0].publication.key());
    assert_eq!(key, initial[0].fragment.package_roots[0].publication.key());
    let before = assemble(None, initial).unwrap();
    let after = assemble(Some(&before), delta.clone()).unwrap();
    assert_ne!(before.id(), after.id());
    let stale = EnvironmentChange::SourceRevision {
        source: SourceId("published".into()),
        revision: SourceRevision {
            incarnation: uuid::Uuid::new_v4(),
            counter: 100,
        },
    };
    assert!(
        sources
            .collect(&frontier(&delta), [stale], &config)
            .unwrap()
            .is_empty()
    );
    drop(sources);
    source.publish(fragment(false)).unwrap();
    assert!(
        mailbox.lock_recover().is_empty(),
        "collector drop ends subscriptions even while the writer lives"
    );
}

/// ENV-DELTA: a namespace commit during enumeration never escapes as a mixed
/// snapshot; only sources declaring that namespace receive its change notice.
#[test]
fn coherent_namespace_observation_refuses_a_racing_publication() {
    let state = Arc::new(Mutex::new(
        super::super::state::PluginStateRegistry::default(),
    ));
    state
        .lock_recover()
        .data
        .plugins
        .insert("state".into(), Default::default());
    let provider = Provider::new("a", SourceMode::Notifying);
    let registry =
        Arc::new(crate::ToolRegistry::from_tool_provider(key("a"), provider.clone()).unwrap());
    let source = Arc::new(ProviderSource {
        key: key("a"),
        tools: [key("a")].into(),
        subscriptions: Mutex::new(Vec::new()),
        registry,
        packages: BTreeMap::new(),
        domain: PublicationDomain("fixture".into()),
        scope: PublicationScope::Host,
        namespaces: Some(state.clone()),
        revision: Mutex::new(SourceRevision {
            incarnation: uuid::Uuid::new_v4(),
            counter: 0,
        }),
    });
    let mailbox = Arc::new(Mutex::new(Vec::new()));
    let target = mailbox.clone();
    let sink: EnvironmentChangeSink = Arc::new(move |change| target.lock_recover().push(change));
    let mut sources = EnvironmentSources::default();
    sources.register(source, sink).unwrap();
    state
        .lock_recover()
        .environment
        .namespace_committed(&["unrelated".into()]);
    assert!(mailbox.lock_recover().is_empty());
    let publishing = state.clone();
    *provider.during_snapshot.lock_recover() = Some(Arc::new(move || {
        let mut state = publishing.lock_recover();
        let namespace = state.data.plugins.get_mut("state").unwrap();
        let original = namespace.values.clone();
        namespace.values = Arc::new([("flag".into(), serde_json::json!(true))].into());
        namespace.generation += 1;
        namespace.values = original;
        namespace.generation += 1;
        state.environment.namespace_committed(&["state".into()]);
    }));
    assert!(matches!(
        sources.collect(&InputFrontier::new(), [], &OwnerConfigView::default()),
        Err(SourceCollectionError::ChangedDuringSnapshot(_))
    ));
    assert_eq!(mailbox.lock_recover().len(), 1);
    assert_eq!(
        mailbox.lock_recover()[0],
        EnvironmentChange::ConfigSelection {
            sources: [provider_source_id(&key("a"))].into()
        }
    );
    let snapshots = sources
        .collect(&InputFrontier::new(), [], &OwnerConfigView::default())
        .unwrap();
    assert_eq!(
        snapshots[0].fragment.config["namespace:state"],
        namespace_identity(&lash_core_store::plugin_state::PluginNamespaceState::default())
    );
    assert!(
        snapshots[0].fragment.config["namespace:state"]
            .get("generation")
            .is_none(),
        "generation is an observation fence, not content identity"
    );
    let original = assemble(None, snapshots).unwrap();
    {
        let mut state = state.lock_recover();
        let namespace = state.data.plugins.get_mut("state").unwrap();
        namespace.values_mut().insert(
            "marker".into(),
            serde_json::json!("<namespace-value-marker>"),
        );
        namespace.generation += 1;
    }
    let changed = assemble(
        None,
        sources
            .collect(&InputFrontier::new(), [], &OwnerConfigView::default())
            .unwrap(),
    )
    .unwrap();
    assert_ne!(
        changed.id(),
        original.id(),
        "namespace values affect environment identity"
    );
    assert!(
        !serde_json::to_string(&changed)
            .unwrap()
            .contains("<namespace-value-marker>"),
        "environment publication must not copy namespace values"
    );
    state
        .lock_recover()
        .data
        .plugins
        .get_mut("state")
        .unwrap()
        .generation += 1;
    let unchanged = assemble(
        None,
        sources
            .collect(&InputFrontier::new(), [], &OwnerConfigView::default())
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        unchanged.id(),
        changed.id(),
        "generation alone is not content"
    );
}

/// ENV-DELTA: one composite publisher projects multiple tool registrations
/// without re-enumeration or losing the route's original provider identity.
#[test]
fn composite_package_membership_preserves_each_provider_once() {
    let a = Provider::new("a", SourceMode::RefreshOnly);
    let b = Provider::new("b", SourceMode::RefreshOnly);
    *a.projection_members.lock_recover() = [
        crate::tool_provider_key!("a"),
        crate::tool_provider_key!("b"),
    ]
    .into();
    let mut reg = super::super::PluginRegistrar::new("test_protocol");
    crate::testing::register_inventory_protocol(&mut reg).unwrap();
    let tool_a = a.clone();
    let tool_b = b.clone();
    reg.tools()
        .provider(crate::tool_provider_key!("a"), move |_, _| {
            Ok(tool_a.clone())
        })
        .unwrap();
    reg.tools()
        .provider(crate::tool_provider_key!("b"), move |_, _| {
            Ok(tool_b.clone())
        })
        .unwrap();
    reg.package_provider(crate::tool_provider_key!("a"), a.clone())
        .unwrap();
    let factory = super::super::PluginRecipeFactory::new(
        super::super::PluginDeclaration::initial("test_protocol"),
        super::super::PluginRecipe::from_registrar("test_protocol", reg),
    );
    let session = crate::testing::test_plugin_host(vec![Arc::new(factory)])
        .build_session(super::super::PluginSessionRequest::creation(
            "composite",
            super::super::SessionAuthorityContext::fixture(),
        ))
        .unwrap();
    let sources = session.environment_sources(Arc::new(|_| {})).unwrap();
    a.calls.take();
    b.calls.take();
    let snapshots = sources
        .collect(&InputFrontier::new(), [], &OwnerConfigView::default())
        .unwrap();
    assert_eq!(a.calls.take(), [1, 1, 1, 0]);
    assert_eq!(b.calls.take(), [1, 1, 0, 0]);
    let fragment = &snapshots
        .iter()
        .find(|snapshot| !snapshot.fragment.tools.is_empty())
        .unwrap()
        .fragment;
    assert_eq!(fragment.tools.len(), 2);
    for entry in &fragment.tools {
        assert_eq!(
            &fragment
                .routes
                .iter()
                .find(|route| route.route.tool == *entry.tool())
                .unwrap()
                .route,
            &entry.link
        );
    }
    assemble(None, snapshots).unwrap();
}
