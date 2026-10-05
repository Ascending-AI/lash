use super::*;

type ParentObservation = (lash_core::RuntimeOwner, Option<lash_core::SessionId>);

struct ChildParentProbe(Arc<Mutex<Vec<ParentObservation>>>);

impl lash_core::facade_support::PluginFactory for ChildParentProbe {
    fn id(&self) -> &'static str {
        "fig4669-parent-probe"
    }

    fn build(
        &self,
        context: &lash_core::facade_support::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::facade_support::SessionPlugin>, lash_core::PluginError> {
        self.0
            .lock()
            .expect("observations")
            .push((context.owner.clone(), context.parent_session_id.clone()));
        lash_core::plugin::StaticPluginFactory::new(
            lash_core::plugin::PluginMetadata::plugin_declaration(self),
            lash_core::facade_support::PluginSpec::new(),
        )
        .build(context)
    }
}

impl lash_core::plugin::PluginDefinition for ChildParentProbe {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("fig4669-parent-probe")
    }
}

pub(super) async fn fig4669_child_parent_survives_every_runtime_reopen(
    tier: Tier,
    replay: bool,
    seed: u64,
) {
    let double = double(tier, replay, seed).await.expect("store tier");
    let backend = double.double.lash_backend();
    let observations = Arc::new(Mutex::new(Vec::new()));
    let probe: Arc<dyn lash_core::facade_support::PluginFactory> =
        Arc::new(ChildParentProbe(Arc::clone(&observations)));
    let parent = lash_core::testing::runtime_helpers::TestRuntime::new(
        &backend,
        lash_core::testing::runtime_helpers::mock_provider(Vec::new()),
    )
    .plugins(vec![Arc::clone(&probe)])
    .build()
    .await;
    let child = parent
        .session_lifecycle_service()
        .expect("lifecycle")
        .create_session(
            lash_core::SessionCreateRequest::child_session(
                "root",
                lash_core::SessionStartPoint::Empty,
                lash_core::PluginOptions::default(),
            )
            .with_session_id("fig4669-child"),
        )
        .await
        .expect("create ordinary child without subagent metadata");
    let reopened =
        lash_core::testing::runtime_helpers::reopen_session_runtime(&parent, &child.session_id)
            .await;
    let store = lash_core::store::SessionStore::new(
        backend.session_store_factory(),
        child.session_id.clone(),
    )
    .expect("session view");
    let host = lash_core::facade_support::RuntimeHostConfig::new(
        backend,
        lash_core::CommitBudget::bounded(1024 * 1024, 512),
        lash_core::QueuedWorkBatchingConfig::new(1),
    );
    // The embedded caller supplies fresh plugin state; parentage still comes from the catalog.
    let mut embedded_state = reopened.state().clone();
    embedded_state.set_plugin_state(None);
    let rebuilt = lash_core::runtime::EmbeddedRuntimeBuilder::new(
        host,
        lash_core::testing::runtime_lease_owner(),
    )
    .with_initial_state(embedded_state)
    .with_store(store)
    .with_plugin_host(lash_core::testing::test_plugin_host(vec![probe]))
    .build()
    .await
    .expect("embedded reopen");
    drop(rebuilt);
    let child_observations = observations
        .lock()
        .expect("observations")
        .iter()
        .filter(|(owner, _)| *owner == lash_core::RuntimeOwner::Session(child.session_id.clone()))
        .map(|(_, parent)| parent.clone())
        .collect::<Vec<_>>();
    assert_eq!(
        child_observations,
        vec![Some(lash_core::SessionId::from("root")); 3],
        "creation, environment reopen and embedded reopen must all read the relation"
    );
}
