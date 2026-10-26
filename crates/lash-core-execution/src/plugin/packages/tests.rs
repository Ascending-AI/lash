use super::*;
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
        ToolDefinition::any_output(),
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
        signature: lash_kernel_value::Signature {
            params: Vec::new(),
            result: lash_kernel_value::Type::Json,
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
    assert!(
        !requirements.providers.contains(
            &super::super::ToolProviderHandle::from_name(
                super::super::RegistryKind::Session,
                &grant.provider
            )
            .unwrap()
        )
    );
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
    let mut competing = context.clone();
    competing.publisher = PublisherInstance("competitor".into());
    assert!(
        matches!(
            validate_advertisement(&snapshot, &competing, &child("service.read.child")),
            Err(AdmissionRefusal::PathConflict { .. })
        ),
        "the first prior path refusal precedes its ownership refusal"
    );
    assert!(validate_advertisement(&snapshot, &context, &child("service.reader")).is_ok());
    assert!(matches!(
        validate_advertisement(&NamespaceSnapshot::default(), &context, &child("other")),
        Err(AdmissionRefusal::NotOwner { .. })
    ));
}
