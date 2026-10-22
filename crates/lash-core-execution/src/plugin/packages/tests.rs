use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

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
    fn roots(&self) -> BTreeSet<Segment> {
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
}
#[async_trait::async_trait]
impl PackageProvider for ControlledPackages {
    fn roots(&self) -> BTreeSet<Segment> {
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
        _: &ReferrerClaim,
        _: &[&PackageAdvertisement],
    ) -> Result<(), EnginePortError> {
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
    let claim = ReferrerClaim::unguarded(crate::ArtifactReferrer::HostPin(
        crate::HostArtifactPin::mint(),
    ))
    .unwrap();
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
    store
        .end_package_referrer(
            &lash_core_store::artifact_referrer::ResolvedArtifactCleanup {
                referrer: claim.referrer().clone(),
                carries: Vec::new(),
            },
        )
        .await
        .unwrap();
    provider.release.notify_one();
    assert!(
        composition
            .retain_packages(&claim, packages, store.as_ref(), false)
            .await
            .is_err()
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
