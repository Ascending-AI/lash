/// FIG-5020: every durable facade feed has a bounded page and an explicit
/// continuation, including an empty page; time cutoffs are validated instants.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn facade_change_feeds_share_bounded_pages_and_typed_cutoffs() -> crate::Result<()> {
    let core = super::explicit_ephemeral_facets(crate::LashCore::standard_builder(
        super::sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(super::mock_provider(), super::mock_llm_profile_spec())
    .build(crate::tests::host::runtime_lease_owner())?;
    let limit = std::num::NonZeroUsize::new(1).unwrap();
    let turns: crate::ChangePage<
        crate::persistence::TurnChange,
        crate::persistence::TurnChangeCursor,
    > = core
        .turns_changed_since(crate::persistence::TurnChangeCursor::initial(), limit)
        .await?;
    assert!(turns.changes.is_empty());
    assert_eq!(turns.next, crate::persistence::TurnChangeCursor::initial());
    assert_eq!(turns.retained_after, Some(turns.next));
    // Standing-fault pages retain the last item as their continuation; an
    // empty page retains the caller's cursor instead of restarting the feed.
    for label in ["fault-a", "fault-b"] {
        core.session(
            lash_vocabulary::identity::SessionId::parse(label).expect("nonblank host identity"),
        )
        .create(crate::SessionCreation::root(super::mock_session_spec()))
        .await?;
        let record = crate::SessionFaultRecord::new(
            crate::SessionFaultOrigin::DriveAdmission,
            &crate::runtime::RuntimeError::new(
                crate::runtime::RuntimeErrorCode::RuntimeStoreCorrupt,
                "corrupt",
            ),
        );
        core.store_factory
            .record_session_fault(
                &lash_vocabulary::identity::SessionId::parse(label).unwrap(),
                &record,
                1,
            )
            .await?;
    }
    let first = core.session_faults(None, limit).await?;
    assert_eq!(first.changes.len(), 1);
    assert_eq!(first.next.as_ref().unwrap().as_str(), "fault-a");
    let second = core.session_faults(first.next.as_ref(), limit).await?;
    assert_eq!(second.changes.len(), 1);
    assert_eq!(second.next.as_ref().unwrap().as_str(), "fault-b");
    let after = lash_vocabulary::identity::SessionId::parse("last-fault").unwrap();
    let faults: crate::ChangePage<
        crate::SessionFault,
        Option<lash_vocabulary::identity::SessionId>,
    > = core.session_faults(Some(&after), limit).await?;
    assert!(faults.changes.is_empty());
    assert_eq!(faults.next, Some(after));
    assert_eq!(faults.retained_after, None);
    Ok(())
}

mod builder_packages {
    use super::super::*;
    use lash_vocabulary::tool_contract::{ToolContract, ToolDefinition, ToolManifest};

    struct CountingTools {
        definition: ToolDefinition,
        enumerations: AtomicUsize,
    }
    #[async_trait]
    impl ToolProvider for CountingTools {
        fn tool_manifests(
            &self,
            _: &lash_core::OwnerConfigView,
        ) -> std::result::Result<Vec<ToolManifest>, crate::plugins::PluginError> {
            self.enumerations.fetch_add(1, Ordering::SeqCst);
            Ok(vec![self.definition.manifest.clone()])
        }
        fn resolve_contract(
            &self,
            name: &str,
            _: &lash_core::OwnerConfigView,
        ) -> Option<Arc<ToolContract>> {
            (name == self.definition.manifest.tool())
                .then(|| Arc::new(self.definition.contract.clone()))
        }
        async fn execute(&self, _: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
            unreachable!("the namespace law only captures definitions")
        }
    }

    /// BUILDER-ROOT-OWNER: multiple runners share one package publisher while
    /// their original callback identities and single observations stay intact.
    #[tokio::test]
    async fn one_namespace_preserves_each_builder_runner_and_observes_it_once() {
        let members: Vec<_> = [
            crate::tool_provider_key!("left"),
            crate::tool_provider_key!("right"),
        ]
        .into_iter()
        .map(|key| {
            let name = key.as_str();
            let definition = ToolDefinition::raw(
                format!("tool:builder-{name}"),
                format!("tools.{name}"),
                name,
                serde_json::json!({"type":"object"}),
                serde_json::json!({"type":"null"}),
            )
            .unwrap()
            .with_execution(std::time::Duration::from_secs(1));
            (
                key,
                Arc::new(CountingTools {
                    definition,
                    enumerations: AtomicUsize::new(0),
                }),
            )
        })
        .collect();
        let backend = sqlite_memory_store_backend().await;
        let mut builder = explicit_ephemeral_facets(LashCore::standard_builder(backend.clone()))
            .serve_test_llm_profile(mock_provider(), mock_llm_profile_spec());
        for (key, provider) in &members {
            builder = builder.tools(*key, provider.clone());
        }
        let core = builder
            .build(crate::tests::host::runtime_lease_owner())
            .unwrap();
        let id = SessionId::from("builder-root-owner");
        core.session(id.clone())
            .create(crate::SessionCreation::root(mock_session_spec()))
            .await
            .unwrap();
        let session = core.session(id.clone()).open().await.unwrap();
        for (_, provider) in &members {
            provider.enumerations.store(0, Ordering::SeqCst);
        }
        let output = session
            .send(crate::TurnInput::text("capture the builder packages"))
            .output()
            .await
            .expect("one builder namespace must have one publisher");
        assert!(output.is_success(), "{output:?}");
        let environments = backend.durable().environments();
        let head = environments.environment_head(&id).await.unwrap().unwrap();
        let body = environments
            .environment_owner(&head.owner)
            .await
            .unwrap()
            .unwrap()
            .body
            .unwrap();
        let fragments: Vec<_> = body
            .fragments()
            .values()
            .map(|fragment| fragment.fragment())
            .collect();
        let packages: Vec<_> = fragments
            .iter()
            .flat_map(|fragment| &fragment.package_roots)
            .filter(|package| package.publication.path.to_string() == "tools")
            .collect();
        assert_eq!(packages.len(), 1);
        let lash_vocabulary::package::PackageAdvertisement::Enumerated(package) =
            &packages[0].advertisement
        else {
            panic!("builder operations are enumerated")
        };
        assert_eq!(package.path.to_string(), "tools");
        assert_eq!(package.members.len(), members.len());
        for (key, provider) in &members {
            assert_eq!(
                provider.enumerations.load(Ordering::SeqCst),
                1,
                "package projection re-enumerated {key:?}"
            );
            let entry = fragments
                .iter()
                .flat_map(|fragment| &fragment.tools)
                .find(|entry| entry.tool() == &provider.definition.manifest.id)
                .unwrap();
            let expected = crate::plugins::ToolProviderHandle::new(
                lash_vocabulary::claims::RegistryKind::Session,
                key.as_str(),
                *key,
            );
            assert_eq!(entry.provider(), &expected);
            let route = fragments
                .iter()
                .flat_map(|fragment| &fragment.routes)
                .find(|candidate| candidate.route.tool == provider.definition.manifest.id)
                .unwrap();
            assert_eq!(route.route.provider, expected);
        }
        let factories = core
            .plugin_factories
            .iter()
            .filter(|factory| members.iter().any(|(key, _)| factory.id() == key.as_str()))
            .cloned()
            .collect();
        let authoring = lash_core::testing::test_plugin_host(factories)
            .build_session(lash_core::plugin::PluginSessionRequest::creation(
                id.clone(),
                lash_core::plugin::SessionAuthorityContext::fixture(),
            ))
            .unwrap();
        for (_, provider) in &members {
            provider.enumerations.store(0, Ordering::SeqCst);
        }
        let publication = &packages[0].publication;
        let authored = authoring
            .composition()
            .advertise_packages(
                &lash_core::OwnerConfigView::default(),
                &Default::default(),
                &publication.context.domain,
                publication.context.scope.clone(),
            )
            .expect("explicit namespace authoring admits declared runners");
        let authored_package = authored
            .snapshot
            .advertisements
            .iter()
            .find(|accepted| accepted.publication.path == publication.path)
            .unwrap();
        assert_eq!(authored_package, packages[0]);
        for (key, provider) in &members {
            assert_eq!(provider.enumerations.load(Ordering::SeqCst), 1);
            let route = authored
                .routes
                .iter()
                .find(|candidate| candidate.route.tool == provider.definition.manifest.id)
                .unwrap();
            assert_eq!(
                route.route.provider,
                crate::plugins::ToolProviderHandle::new(
                    lash_vocabulary::claims::RegistryKind::Session,
                    key.as_str(),
                    *key,
                )
            );
        }
        drop(session);
        core.shutdown().await.unwrap();
    }
}
