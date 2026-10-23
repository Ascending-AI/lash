use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct ServicePackage(Arc<AtomicBool>);
#[async_trait::async_trait]
impl PackageProvider for ServicePackage {
    async fn hold(
        &self,
        _: &ReferrerClaim,
        _: &[&PackageAdvertisement],
    ) -> Result<(), EnginePortError> {
        Ok(())
    }
    fn roots(&self, _: &OwnerConfigView) -> BTreeSet<Segment> {
        [Segment::new("service").unwrap()].into()
    }
    fn advertise(
        &self,
        _: &OwnerConfigView,
        context: &PublicationContext,
    ) -> Result<Vec<PackageContribution>, PluginError> {
        if !self.0.load(Ordering::SeqCst) {
            return Ok(Vec::new());
        }
        let offer = OperationOffer {
            requirement: lash_vocabulary::OperationRequirement {
                signature: lash_vocabulary::Signature {
                    params: Vec::new(),
                    result: lash_vocabulary::Type::Null,
                },
                contract: lash_vocabulary::ContractId::new("service.read"),
            },
            intents: Default::default(),
            controls: Default::default(),
            mode: OperationMode::Read,
        };
        let advertisement = PackageAdvertisement::Enumerated(Package {
            path: PackagePath::parse("service").unwrap(),
            members: [(
                Segment::new("read").unwrap(),
                Member {
                    visibility: Visibility::Internal,
                    kind: MemberKind::Operation(offer),
                },
            )]
            .into(),
        });
        Ok(vec![PackageContribution {
            publication: operation_publication(context, &advertisement).unwrap(),
            advertisement,
            routes: Vec::new(),
        }])
    }
}

/// L-STANDARD: service-package-only changes cannot add or suppress model tools.
/// The accepted advertisement and its round write still change together.
#[test]
fn service_packages_leave_the_provider_catalog_unchanged() {
    let enabled = Arc::new(AtomicBool::new(false));
    let mut reg = super::super::PluginRegistrar::new("test_protocol");
    crate::testing::register_inventory_protocol(&mut reg).unwrap();
    reg.package_provider(
        crate::tool_provider_key!("services"),
        Arc::new(ServicePackage(Arc::clone(&enabled))),
    )
    .unwrap();
    let factory = super::super::PluginRecipeFactory::new(
        super::super::PluginDeclaration::initial("test_protocol"),
        super::super::PluginRecipe::from_registrar("test_protocol", reg),
    );
    let session = crate::testing::test_plugin_host(vec![Arc::new(factory)])
        .build_session(super::super::PluginSessionRequest::creation(
            "packages",
            super::super::SessionAuthorityContext::fixture(),
        ))
        .unwrap();
    let before = session.pin_resolved_tool_surface().unwrap();
    enabled.store(true, Ordering::SeqCst);
    let after = session.pin_resolved_tool_surface().unwrap();
    let manifests = |catalog: &crate::ToolCatalog| {
        catalog
            .entries()
            .iter()
            .map(|entry| entry.manifest.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(manifests(&before.catalog), manifests(&after.catalog));
    assert!(before.surface.packages().snapshot.advertisements.is_empty());
    assert_eq!(after.surface.packages().snapshot.advertisements.len(), 1);
    let restored: crate::RecordedSurface =
        serde_json::from_slice(&serde_json::to_vec(&*after.surface).unwrap()).unwrap();
    enabled.store(false, Ordering::SeqCst);
    assert_eq!(restored.packages(), after.surface.packages());
}

struct ControlledPackages {
    available: AtomicBool,
    holding: tokio::sync::Notify,
    release: tokio::sync::Notify,
    holds: AtomicUsize,
    store: std::sync::Mutex<
        Option<Arc<dyn lash_execution_contracts::module_artifacts::ModuleArtifactStore>>,
    >,
}
#[async_trait::async_trait]
impl PackageProvider for ControlledPackages {
    fn roots(&self, _: &OwnerConfigView) -> BTreeSet<Segment> {
        [Segment::new("service").unwrap()].into()
    }
    fn advertise(
        &self,
        config: &OwnerConfigView,
        context: &PublicationContext,
    ) -> Result<Vec<PackageContribution>, PluginError> {
        if !self.available.load(Ordering::SeqCst) {
            return Err(PluginError::Package(Box::new(
                EnginePortError::Unavailable {
                    message: "configured publication cannot rebuild".into(),
                },
            )));
        }
        ServicePackage(Arc::new(AtomicBool::new(true))).advertise(config, context)
    }
    async fn hold(
        &self,
        claim: &ReferrerClaim,
        _: &[&PackageAdvertisement],
    ) -> Result<(), EnginePortError> {
        self.holds.fetch_add(1, Ordering::SeqCst);
        let store = self.store.lock().unwrap().clone();
        if let Some(store) = store {
            store
                .publish_module_artifact(claim, "partial-provider", b"partial")
                .await
                .map_err(|error| package_store_error(error.into()))?;
        }
        self.holding.notify_one();
        self.release.notified().await;
        Ok(())
    }
}
fn controlled_session(provider: Arc<ControlledPackages>) -> Arc<super::super::PluginSession> {
    let mut reg = super::super::PluginRegistrar::new("test_protocol");
    crate::testing::register_inventory_protocol(&mut reg).unwrap();
    reg.package_provider(crate::tool_provider_key!("services"), provider)
        .unwrap();
    let factory = super::super::PluginRecipeFactory::new(
        super::super::PluginDeclaration::initial("test_protocol"),
        super::super::PluginRecipe::from_registrar("test_protocol", reg),
    );
    crate::testing::test_plugin_host(vec![Arc::new(factory)])
        .build_session(super::super::PluginSessionRequest::creation(
            "packages",
            super::super::SessionAuthorityContext::fixture(),
        ))
        .unwrap()
}
fn controlled_packages() -> Arc<ControlledPackages> {
    Arc::new(ControlledPackages {
        available: AtomicBool::new(true),
        holding: Default::default(),
        release: Default::default(),
        holds: AtomicUsize::new(0),
        store: Default::default(),
    })
}
/// E1: a committed package configuration that cannot rebuild must not
/// silently pair its old advertisement with new work.
#[test]
fn a_failed_package_rebuild_refuses_new_work_with_its_typed_cause() {
    let provider = controlled_packages();
    let session = controlled_session(provider.clone());
    let previous = session.pin_resolved_tool_surface().unwrap();
    provider.available.store(false, Ordering::SeqCst);
    let error = match session.pin_resolved_tool_surface() {
        Ok(_) => panic!("stale advertisement served"),
        Err(error) => error,
    };
    assert!(
        matches!(&error, PluginError::Package(cause) if matches!(**cause, EnginePortError::Unavailable {..}))
    );
    assert!(matches!(
        crate::session::ExecutionEnvironmentSyncError::of_plugin_error(
            crate::sansio::ExecutionEnvironmentSyncFailureKind::ToolSurface,
            error,
        ),
        crate::session::ExecutionEnvironmentSyncError::Live(_)
    ));
    assert_eq!(previous.surface.packages().snapshot.advertisements.len(), 1);
}
/// L-PIN: cancelling at a provider's hold boundary publishes no round
/// provenance; ending its original owner fences a later completion.
#[tokio::test]
async fn cancelling_package_retention_keeps_the_original_referrer_fence() {
    use lash_durable::packages::PackageStore;
    let provider = controlled_packages();
    let session = controlled_session(provider.clone());
    let surface = session.pin_resolved_tool_surface().unwrap();
    let packages = surface.surface.packages();
    let stores = lash_sqlite_store::SqliteStoreSet::memory().await.unwrap();
    let store = stores.open_store().await.unwrap();
    let claim = live_session_claim(&stores, "cancelled-preparation").await;
    *provider.store.lock().unwrap() = Some(store.clone());
    let composition = session.composition();
    let mut holding =
        Box::pin(composition.retain_packages(&claim, packages, store.as_ref(), false));
    tokio::select! {
        result = &mut holding => panic!("hold completed before release: {result:?}"),
        _ = provider.holding.notified() => {},
    }
    drop(holding);
    let key = packages.snapshot.advertisements[0].publication.key();
    assert!(store.publication(&key).await.unwrap().is_none());
    use lash_execution_contracts::module_artifacts::ModuleArtifactStore;
    assert_eq!(
        store.get_module_artifact("partial-provider").await.unwrap(),
        Some(b"partial".to_vec())
    );
    // Ending the cancelled turn cannot sever its session's partial preparation.
    store
        .end_package_referrer(
            &lash_core_store::artifact_referrer::ResolvedArtifactCleanup {
                referrer: crate::ArtifactReferrer::Execution(
                    crate::ExecutionScope::turn("cancelled-preparation", "cancelled-turn")
                        .journal_identity()
                        .unwrap(),
                ),
                carries: Vec::new(),
            },
        )
        .await
        .unwrap();
    assert!(
        store
            .get_module_artifact("partial-provider")
            .await
            .unwrap()
            .is_some()
    );
    let mut delayed =
        Box::pin(composition.retain_packages(&claim, packages, store.as_ref(), false));
    tokio::select! {
        result = &mut delayed => panic!("hold completed before release: {result:?}"),
        _ = provider.holding.notified() => {},
    }
    use lash_core_store::store::SessionCatalogStore;
    store
        .delete_session(&crate::SessionId::fixture("cancelled-preparation"))
        .await
        .unwrap();
    store
        .end_package_referrer(
            &lash_core_store::artifact_referrer::ResolvedArtifactCleanup {
                referrer: claim.referrer(),
                carries: Vec::new(),
            },
        )
        .await
        .unwrap();
    provider.release.notify_one();
    assert!(matches!(
        delayed.await,
        Err(EnginePortError::PackageRefused(
            lash_durable::packages::PackageRefusal::SessionNotLive(_)
        ))
    ));
    assert!(
        store
            .get_module_artifact("partial-provider")
            .await
            .unwrap()
            .is_none()
    );
    assert!(store.publication(&key).await.unwrap().is_none());
}

/// L-DEFERRED-PIN: a terminal target needs its retained executor, while a
/// first use still requires a registered resolver for the original grant.
#[test]
fn a_pinned_target_needs_its_executor_without_rediscovery() {
    use lash_core_store::tool_surface::RouteSource;
    let mut reg = super::super::PluginRegistrar::new("test_protocol");
    crate::testing::register_inventory_protocol(&mut reg).unwrap();
    reg.tools()
        .provider(
            lash_vocabulary::providers::ToolProviderKey::new("executor").unwrap(),
            |_, _| Ok(Arc::new(crate::testing::EmptyToolProvider)),
        )
        .unwrap();
    let factory = super::super::PluginRecipeFactory::new(
        super::super::PluginDeclaration::initial("test_protocol"),
        super::super::PluginRecipe::from_registrar("test_protocol", reg),
    );
    let session = crate::testing::test_plugin_host(vec![Arc::new(factory)])
        .build_session(super::super::PluginSessionRequest::creation(
            "pinned-target",
            super::super::SessionAuthorityContext::fixture(),
        ))
        .unwrap();
    let composition = session.composition();
    let provider = composition
        .tool_registry()
        .keys()
        .find(|provider| provider.key() == "executor")
        .unwrap()
        .clone();
    let definition = ToolDefinition::raw(
        "tool:pinned",
        "service.run",
        "",
        serde_json::json!({}),
        serde_json::json!({}),
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(10));
    let grant = DeferredGrant {
        provider: ProviderName {
            owner: "resolver_plugin".into(),
            key: "resolver".into(),
        },
        context: PublicationContext {
            domain: PublicationDomain("test".into()),
            scope: PublicationScope::Host,
            publisher: PublisherInstance("resolver_plugin/resolver".into()),
            root: Segment::new("service").unwrap(),
        },
        path: PackagePath::parse("service").unwrap(),
        visibility: Visibility::Public,
        signature: lash_vocabulary::Signature {
            params: Vec::new(),
            result: lash_vocabulary::Type::Json,
        },
        config: ProviderConfig {
            component: None,
            format: super::super::FormatVersion::ONE,
            value: serde_json::Value::Null,
            payload: None,
        },
        resolution_policy: Default::default(),
    };
    let route = OperationRoute {
        tool: definition.manifest.id.clone(),
        provider,
        policy: definition.manifest.execution_policy,
        projection: definition.manifest.argument_projection.clone(),
        payload: None,
        source: RouteSource::Resolved {
            definition: Box::new(definition.clone()),
        },
    };
    let name = QualifiedName {
        package: grant.path.clone(),
        member: Segment::new("run").unwrap(),
        type_member: None,
    };
    assert!(
        composition
            .validate_deferred_grant(&grant, super::super::RegistryKind::Session)
            .is_err()
    );
    let requirements = composition
        .validate_resolved_route(
            &grant,
            &name,
            &definition,
            &route,
            super::super::RegistryKind::Session,
        )
        .unwrap();
    assert_eq!(requirements.providers.len(), 1);
    assert!(!requirements.providers.contains(&grant.provider));
    assert_eq!(requirements.callbacks.len(), 1);
    // L-DEFERRED-PIN: choosing another owner cannot cross registries, invent
    // an executor, or authorize a name outside the pinned deferred path.
    let invalid_routes = [
        OperationRoute {
            provider: super::super::ToolProviderHandle::new(
                super::super::RegistryKind::Process,
                "test_protocol",
                crate::tool_provider_key!("executor"),
            ),
            ..route.clone()
        },
        OperationRoute {
            provider: super::super::ToolProviderHandle::new(
                super::super::RegistryKind::Session,
                "unregistered",
                crate::tool_provider_key!("executor"),
            ),
            ..route.clone()
        },
    ];
    for invalid in invalid_routes {
        assert!(matches!(
            composition.validate_resolved_route(
                &grant,
                &name,
                &definition,
                &invalid,
                super::super::RegistryKind::Session
            ),
            Err(EnginePortError::Refused(
                AdmissionRefusal::NotVisible { .. }
            ))
        ));
    }
    let outside = QualifiedName {
        package: PackagePath::parse("another_scope").unwrap(),
        ..name.clone()
    };
    assert!(matches!(
        composition.validate_resolved_route(
            &grant,
            &outside,
            &definition,
            &route,
            super::super::RegistryKind::Session
        ),
        Err(EnginePortError::Refused(
            AdmissionRefusal::NotVisible { .. }
        ))
    ));
}

/// A2: sessions only add disjoint paths; a package member is a leaf,
/// and a second configured instance cannot claim an owned root.
#[test]
fn scoped_advertisements_refuse_foreign_roots_and_member_path_collisions() {
    let context = PublicationContext {
        domain: PublicationDomain("store".into()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("one".into()),
        root: Segment::new("service").unwrap(),
    };
    let parent = PackageAdvertisement::Enumerated(Package {
        path: PackagePath::parse("service").unwrap(),
        members: [(
            Segment::new("read").unwrap(),
            Member {
                visibility: Visibility::Public,
                kind: MemberKind::Export {
                    link: LinkId::from_bytes([1; 32]),
                    export: ExportName {
                        name: Name::new("read"),
                        member: None,
                    },
                },
            },
        )]
        .into(),
    });
    let snapshot = NamespaceSnapshot {
        advertisements: vec![AcceptedAdvertisement {
            publication: PublicationRecord {
                context: context.clone(),
                path: PackagePath::parse("service").unwrap(),
                content: PublishedContent::Package(match &parent {
                    PackageAdvertisement::Enumerated(package) => package.identity(),
                    _ => unreachable!(),
                }),
                code_links: Default::default(),
                imported_publications: Vec::new(),
            },
            advertisement: parent,
        }],
    };
    let child = |path| {
        PackageAdvertisement::Enumerated(Package {
            path: PackagePath::parse(path).unwrap(),
            members: Default::default(),
        })
    };
    assert!(matches!(
        validate_advertisement(&snapshot, &context, &child("service.read.child")),
        Err(AdmissionRefusal::PathConflict { .. })
    ));
    let foreign = PublicationContext {
        publisher: PublisherInstance("two".into()),
        ..context.clone()
    };
    assert!(matches!(
        validate_advertisement(&snapshot, &foreign, &child("service.extra")),
        Err(AdmissionRefusal::NotOwner { .. })
    ));
    let session = PublicationContext {
        scope: PublicationScope::Session(crate::SessionId::fixture("another-scope")),
        ..context.clone()
    };
    assert!(matches!(
        validate_advertisement(&snapshot, &session, &child("service.extra")),
        Err(AdmissionRefusal::NotOwner { .. })
    ));
    assert!(validate_advertisement(&snapshot, &context, &child("service.reader")).is_ok());
    assert!(matches!(
        validate_advertisement(&NamespaceSnapshot::default(), &context, &child("other")),
        Err(AdmissionRefusal::NotOwner { .. })
    ));
}

struct ConfiguredPackages(&'static str);

#[async_trait::async_trait]
impl PackageProvider for ConfiguredPackages {
    fn roots(&self, config: &OwnerConfigView) -> BTreeSet<Segment> {
        config
            .decode::<std::collections::BTreeMap<String, BTreeSet<Segment>>>("test_protocol")
            .unwrap()
            .unwrap()
            .remove(self.0)
            .unwrap()
    }

    fn advertise(
        &self,
        _: &OwnerConfigView,
        context: &PublicationContext,
    ) -> Result<Vec<PackageContribution>, PluginError> {
        if context.root == Segment::new("unused").unwrap() {
            return Ok(Vec::new());
        }
        let advertisement = PackageAdvertisement::Enumerated(Package {
            path: Vec1::new(context.root.clone()),
            members: Default::default(),
        });
        Ok(vec![PackageContribution {
            publication: operation_publication(context, &advertisement).unwrap(),
            advertisement,
            routes: Vec::new(),
        }])
    }

    async fn hold(
        &self,
        _: &ReferrerClaim,
        _: &[&PackageAdvertisement],
    ) -> Result<(), EnginePortError> {
        Ok(())
    }
}

fn configured_host() -> super::super::PluginHost {
    let mut reg = super::super::PluginRegistrar::new("test_protocol");
    crate::testing::register_inventory_protocol(&mut reg).unwrap();
    reg.package_provider(
        crate::tool_provider_key!("primary"),
        Arc::new(ConfiguredPackages("primary")),
    )
    .unwrap();
    reg.package_provider(
        crate::tool_provider_key!("secondary"),
        Arc::new(ConfiguredPackages("secondary")),
    )
    .unwrap();
    let factory = super::super::PluginRecipeFactory::new(
        super::super::PluginDeclaration::initial("test_protocol"),
        super::super::PluginRecipe::from_registrar("test_protocol", reg),
    );
    crate::testing::test_plugin_host(vec![Arc::new(factory)])
}

fn root_config(primary: &str, secondary: &str) -> super::super::AdmittedPluginConfig {
    let mut config = super::super::PluginConfig::default();
    config.insert_versioned(
        "test_protocol",
        super::super::FormatVersion::ONE,
        serde_json::json!({"primary": [primary, "unused"], "secondary": [secondary]}),
    );
    super::super::AdmittedPluginConfig::new(config, 1)
}

fn configured_session(host: &super::super::PluginHost) -> Arc<super::super::PluginSession> {
    host.build_session(super::super::PluginSessionRequest::creation(
        "configured-roots",
        super::super::SessionAuthorityContext::fixture(),
    ))
    .unwrap()
}

fn advertised_roots(packages: &RecordedPackages) -> BTreeSet<Segment> {
    packages
        .snapshot
        .advertisements
        .iter()
        .map(|advertisement| advertisement.publication.context.root.clone())
        .collect()
}

/// FIG-7177: each run claims only its own config-derived roots, even when
/// another run assigns a previously claimed root to a different provider. An idle claim
/// contributes no advertisement, so it exposes no Imports.
#[test]
fn package_roots_are_owned_independently_for_each_runs_config() {
    let session = configured_session(&configured_host());
    for (primary, secondary) in [("alpha", "beta"), ("beta", "gamma")] {
        let surface = session
            .build_tool_surface(&root_config(primary, secondary))
            .unwrap();
        let packages = surface.surface.packages();
        assert_eq!(
            advertised_roots(packages),
            [
                Segment::new(primary).unwrap(),
                Segment::new(secondary).unwrap()
            ]
            .into(),
        );
        let primary_advertisement = packages
            .snapshot
            .advertisements
            .iter()
            .find(|advertisement| {
                advertisement.publication.context.publisher
                    == PublisherInstance("test_protocol/primary".into())
            })
            .unwrap();
        assert_eq!(
            primary_advertisement.publication.path,
            Vec1::new(Segment::new(primary).unwrap())
        );
    }
}

/// FIG-7177: reconstructing providers and the recorded run config preserves
/// ownership, even when the reopened session's current config is different.
#[tokio::test]
async fn reopening_a_recorded_run_recomputes_its_package_roots() {
    let session = configured_session(&configured_host());
    let config = root_config("alpha", "beta");
    let before = session.build_tool_surface(&config).unwrap();
    let stores = lash_sqlite_store::SqliteStoreSet::memory().await.unwrap();
    let store = stores.open_store().await.unwrap();
    let claim = live_session_claim(&stores, "configured-roots").await;
    session
        .composition()
        .retain_packages(&claim, before.surface.packages(), store.as_ref(), false)
        .await
        .unwrap();
    let changed = session
        .build_tool_surface(&root_config("new", "other"))
        .unwrap();
    session
        .composition()
        .retain_packages(&claim, changed.surface.packages(), store.as_ref(), false)
        .await
        .unwrap();
    let recorded_config = serde_json::to_vec(&config).unwrap();
    let snapshot = session.capture_state();
    drop(session);
    let reopened = configured_host()
        .build_session(super::super::PluginSessionRequest::rematerialization(
            "configured-roots",
            &snapshot,
            super::super::SessionAuthorityContext {
                plugin_config: root_config("current", "other"),
            },
        ))
        .unwrap();
    let restored = serde_json::from_slice(&recorded_config).unwrap();
    let after = reopened.build_tool_surface(&restored).unwrap();
    use lash_durable::packages::PackageStore;
    let keys = |packages: &RecordedPackages| {
        packages
            .snapshot
            .advertisements
            .iter()
            .map(|a| a.publication.key())
            .collect::<BTreeSet<_>>()
    };
    assert!(
        store
            .unheld_publications(&claim, &keys(before.surface.packages()))
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .unheld_publications(&claim, &keys(changed.surface.packages()))
            .await
            .unwrap()
            .is_empty()
    );
    let (result, sql) = lash_core_ids::operational_metrics::sql::collect(
        "recorded-package-reuse",
        "sqlite",
        reopened.composition().retain_packages(
            &claim,
            after.surface.packages(),
            store.as_ref(),
            true,
        ),
    )
    .await;
    result.unwrap();
    assert!(sql.statements > 0);
    assert_eq!(sql.rows_affected, 0);
    assert_eq!(sql.shapes.keys().filter(|s| mutating_sql(s)).count(), 0);
    for key in keys(before.surface.packages()).union(&keys(changed.surface.packages())) {
        assert!(store.publication(key).await.unwrap().is_some());
    }
    assert_eq!(before.surface.packages(), after.surface.packages());
    assert_eq!(
        advertised_roots(after.surface.packages()),
        [
            Segment::new("alpha").unwrap(),
            Segment::new("beta").unwrap()
        ]
        .into()
    );
}

/// FIG-7177: distinct provider identities cannot own the same root derived
/// from this run's config; an empty advertisement does not waive ownership.
#[test]
fn config_derived_root_collisions_are_refused_as_not_owner() {
    let session = configured_session(&configured_host());
    for root in ["alpha", "unused"] {
        let error = match session.build_tool_surface(&root_config(root, root)) {
            Ok(_) => panic!("duplicate root accepted"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            PluginError::Package(cause) if matches!(&*cause,
                EnginePortError::Refused(AdmissionRefusal::NotOwner { path })
                if *path == Vec1::new(Segment::new(root).unwrap()))
        ));
    }
}

async fn live_session_claim(stores: &lash_sqlite_store::SqliteStoreSet, id: &str) -> ReferrerClaim {
    use lash_core_store::store::SessionCatalogStore;
    let id = crate::SessionId::fixture(id);
    stores
        .session_store_factory()
        .admit_session(&crate::testing::store_fixtures::root_session_request(&id))
        .await
        .unwrap();
    ReferrerClaim::unguarded(crate::ArtifactReferrer::Session(id)).unwrap()
}

fn mutating_sql(sql: &str) -> bool {
    let sql = sql.trim_start().to_ascii_lowercase();
    ["insert ", "update ", "delete ", "replace "]
        .iter()
        .any(|prefix| sql.starts_with(prefix))
}

/// L-PIN: exact publication facts survive a cold reopen without callback or DML.
#[tokio::test]
async fn an_identical_session_snapshot_requires_no_second_hold() {
    use lash_durable::packages::PackageStore;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("packages.db");
    let provider = controlled_packages();
    let session = controlled_session(provider.clone());
    let surface = session.pin_resolved_tool_surface().unwrap();
    let stores = lash_sqlite_store::SqliteStoreSet::open(
        &path,
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await
    .unwrap();
    let claim = live_session_claim(&stores, "reopened-packages").await;
    let store = stores.open_store().await.unwrap();
    provider.release.notify_one();
    session
        .composition()
        .retain_packages(&claim, surface.surface.packages(), store.as_ref(), false)
        .await
        .unwrap();
    assert_eq!(provider.holds.load(Ordering::SeqCst), 1);
    drop(store);
    drop(stores);
    let stores = lash_sqlite_store::SqliteStoreSet::open(
        &path,
        lash_sqlite_store::SqliteSynchronous::Normal,
    )
    .await
    .unwrap();
    let store = stores.open_store().await.unwrap();
    let mut duplicate = surface.surface.packages().clone();
    duplicate.snapshot.advertisements.reverse();
    duplicate
        .snapshot
        .advertisements
        .extend(duplicate.snapshot.advertisements.clone());
    let (result, sql) = lash_core_ids::operational_metrics::sql::collect(
        "identical-session-retention",
        "sqlite",
        session
            .composition()
            .retain_packages(&claim, &duplicate, store.as_ref(), false),
    )
    .await;
    result.unwrap();
    assert_eq!(provider.holds.load(Ordering::SeqCst), 1);
    assert!(sql.statements > 0, "retention observation must execute");
    assert_eq!(sql.rows_affected, 0, "{sql:?}");
    assert_eq!(
        sql.shapes.keys().filter(|s| mutating_sql(s)).count(),
        0,
        "{sql:?}"
    );
    let original = &duplicate.snapshot.advertisements[0].publication;
    let keys: BTreeSet<_> = duplicate
        .snapshot
        .advertisements
        .iter()
        .map(|a| a.publication.key())
        .collect();
    assert!(
        store
            .unheld_publications(&claim, &keys)
            .await
            .unwrap()
            .is_empty()
    );
    let mut changed = original.key();
    changed.domain = PublicationDomain("different-domain".into());
    let mut scope = original.key();
    scope.scope = PublicationScope::Session(crate::SessionId::fixture("other-session"));
    let mut publisher = original.key();
    publisher.publisher = PublisherInstance("other-publisher".into());
    let mut content = original.key();
    content.content_digest[0] ^= 1;
    let missing: BTreeSet<_> = [changed, scope, publisher, content].into();
    assert_eq!(
        store.unheld_publications(&claim, &missing).await.unwrap(),
        missing
    );
    let deferred = PackageAdvertisement::Deferred(Box::new(DeferredPackage {
        path: original.path.clone(),
        visibility: Visibility::Public,
        provider: ProviderName {
            owner: "test_protocol".into(),
            key: "services".into(),
        },
        config: ProviderConfig {
            component: None,
            format: super::super::FormatVersion::ONE,
            value: serde_json::json!({"tenant":"one"}),
            payload: None,
        },
        resolution_policy: Default::default(),
    }));
    let first = operation_publication(&original.context, &deferred).unwrap();
    store.publish(&claim, &first).await.unwrap();
    let PackageAdvertisement::Deferred(mut changed) = deferred else {
        unreachable!()
    };
    changed.config.value = serde_json::json!({"tenant":"two"});
    let second =
        operation_publication(&original.context, &PackageAdvertisement::Deferred(changed)).unwrap();
    assert_ne!(first.key(), second.key());
    assert_eq!(
        store
            .unheld_publications(&claim, &[first.key(), second.key()].into())
            .await
            .unwrap(),
        [second.key()].into()
    );
    // Empty snapshots still prove the session owner exists.
    let absent = ReferrerClaim::unguarded(crate::ArtifactReferrer::Session(
        crate::SessionId::fixture("absent"),
    ))
    .unwrap();
    assert!(matches!(
        store.unheld_publications(&absent, &BTreeSet::new()).await,
        Err(lash_durable::DurableError::Package(
            lash_durable::packages::PackageRefusal::SessionNotLive(_)
        ))
    ));
}
