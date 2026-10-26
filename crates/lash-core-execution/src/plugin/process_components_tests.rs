use super::*;
/// Closed package namespaces: the stored environment decides before a
/// provider is consulted, and the typed cause survives host boundaries.
#[tokio::test]
async fn an_internal_dynamic_path_is_refused_before_provider_resolution() {
    struct Resolver(std::sync::atomic::AtomicUsize);
    #[async_trait::async_trait]
    impl crate::ToolProvider for Resolver {
        fn tool_manifests(
            &self,
            _: &crate::OwnerConfigView,
        ) -> Result<Vec<lash_vocabulary::tool_contract::ToolManifest>, PluginError> {
            Ok(Vec::new())
        }
        fn resolve_contract(
            &self,
            _: &str,
            _: &crate::OwnerConfigView,
        ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
            None
        }
        async fn execute(&self, _: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
            panic!("this law resolves only")
        }
    }
    #[async_trait::async_trait]
    impl crate::DeferredToolProvider for Resolver {
        async fn resolve(
            &self,
            cx: &crate::DeferredResolveContext<'_>,
            paths: &[&str],
        ) -> BTreeMap<String, crate::DeferredOutcome> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert!(
                matches!(paths, ["outside.lookup"] | ["outside.alias"]),
                "no package path reaches the provider"
            );
            BTreeMap::from([(
                paths[0].to_owned(),
                crate::DeferredOutcome::resolved(
                    cx.provider.clone(),
                    lash_vocabulary::tool_contract::ToolDefinition::raw(
                        "outside",
                        if paths[0] == "outside.alias" {
                            "dataframe.col"
                        } else {
                            "outside.lookup"
                        },
                        "outside",
                        serde_json::json!({}),
                        lash_vocabulary::ToolDefinition::any_output(),
                    )
                    .unwrap()
                    .with_execution(std::time::Duration::from_secs(1)),
                    None,
                ),
            )])
        }
    }
    const KEY: crate::ProcessConfigKey<()> =
        crate::ProcessConfigKey::new("namespace-law", FormatVersion::ONE);
    let resolver = Arc::new(Resolver(std::sync::atomic::AtomicUsize::new(0)));
    let granted = lash_vocabulary::tool_contract::ToolDefinition::raw(
            "host:call", "tools.call_tool", "Dispatches a host-selected path.",
            serde_json::json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}),
        lash_vocabulary::ToolDefinition::any_output(),
        ).unwrap().with_execution(std::time::Duration::from_secs(30));
    let effect = lash_vocabulary::OperationUse {
        link: lash_vocabulary::LinkId::from_bytes([5; 32]),
        slot: lash_vocabulary::ImportSlot(0),
    };
    let registration = crate::DeferredToolProviderRegistration::new(resolver.clone())
        .with_dispatch_grant(crate::DispatchGrant::new(granted.clone(), |input| {
            Ok(crate::DispatchTarget {
                path: input["path"].as_str().unwrap().to_owned(),
                args: serde_json::json!({}),
            })
        }))
        .unwrap();
    let handle = KEY.tool_provider(crate::tool_provider_key!("deferred"));
    let mut components = ProcessComponentRegistry::new();
    components
        .register(
            Arc::new(
                StaticPluginFactory::new(
                    PluginDeclaration::initial(KEY.component()),
                    super::PluginSpec::new().with_deferred_provider(
                        crate::tool_provider_key!("deferred"),
                        registration.clone(),
                    ),
                )
                .as_component(KEY.format())
                .unwrap(),
            ),
            KEY,
        )
        .unwrap();
    let env = Arc::new(
        crate::ProcessEnv::builder(crate::MaxToolCalls::new(10))
            .component(KEY, &())
            .execution({
                use lash_vocabulary::package::*;
                let route = registration.dispatch_link(handle, granted.id()).unwrap();
                let mut execution = crate::testing::operation_environment(
                    lash_vocabulary::claims::RegistryKind::Process,
                    [(effect, granted.clone(), route)],
                )
                .unwrap();
                execution.publications.push(PublicationRecord {
                    context: PublicationContext {
                        domain: PublicationDomain("fixture".into()),
                        scope: PublicationScope::Host,
                        publisher: PublisherInstance("fixture".into()),
                        root: lash_vocabulary::Segment::new("dataframe").unwrap(),
                    },
                    path: lash_vocabulary::PackagePath::parse("dataframe").unwrap(),
                    content: PublishedContent::Package(lash_vocabulary::PackageId::from_bytes(
                        [4; 32],
                    )),
                    code_links: Default::default(),
                    imported_publications: Vec::new(),
                });
                execution
            })
            .build()
            .unwrap(),
    );
    let env =
        Arc::new(crate::ProcessEnv::from_store_bytes(&env.to_store_bytes().unwrap()).unwrap());
    let composition = components.compose(&env, None).unwrap();
    let owner = crate::ExecutionOwner::Process {
        process_id: crate::ProcessIdMint::sequential_id_for_testing(1),
    };
    let refused = composition
        .resolve_dispatch(
            &env,
            &owner,
            &effect,
            &serde_json::json!({"path":"dataframe.run"}),
        )
        .await
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(&refused, crate::DispatchRefusal::PackageNamespace { path } if path == "dataframe.run")
    );
    assert_eq!(resolver.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    for path in [
        "dataframe.nosuch",
        "dataframe.sub.nosuch",
        "dataframe",
        "dataframe.col",
        "dataframe.public",
    ] {
        assert!(
            matches!(composition.resolve_dispatch(&env,&owner,&effect,&serde_json::json!({"path":path})).await.unwrap(),Err(crate::DispatchRefusal::PackageNamespace { path: refused }) if refused==path)
        );
    }
    assert_eq!(resolver.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    let outside = composition
        .resolve_dispatch(
            &env,
            &owner,
            &effect,
            &serde_json::json!({"path":"outside.lookup"}),
        )
        .await
        .unwrap();
    assert!(outside.unwrap().resolved().is_some());
    assert_eq!(resolver.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        matches!(composition.resolve_dispatch(&env, &owner, &effect, &serde_json::json!({"path":"outside.alias"})).await.unwrap(), Err(crate::DispatchRefusal::PackageNamespace { path }) if path == "dataframe.col")
    );
    assert_eq!(resolver.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    let failure = refused.failure();
    assert_eq!(failure.code, "package_namespace");
    assert!(
        matches!(failure.cause.as_deref(), Some(crate::ToolFailureCause::PackageNamespace { path }) if path == "dataframe.run")
    );
    let refused = PluginError::PackageNamespace {
        path: "dataframe.run".into(),
    };
    let controller = crate::RuntimeEffectControllerError::from(refused.clone());
    assert!(matches!(
        controller.cause,
        Some(crate::RuntimeErrorCause::PluginOperation { .. })
    ));
    let failure = lash_execution_contracts::plugin_error::ToolIntentCommandFailure::from(&refused);
    let restored: lash_execution_contracts::plugin_error::ToolIntentCommandFailure =
        serde_json::from_slice(&serde_json::to_vec(&failure).unwrap()).unwrap();
    assert!(
        matches!(restored, lash_execution_contracts::plugin_error::ToolIntentCommandFailure::PackageNamespace { path } if path == "dataframe.run")
    );
}
