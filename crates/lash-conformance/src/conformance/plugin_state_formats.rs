use super::*;
use pretty_assertions::assert_eq;

#[derive(Clone)]
pub(super) struct FormatPlugin(pub(super) Arc<std::sync::atomic::AtomicUsize>);

#[expect(
    clippy::unwrap_used,
    reason = "format law uses known versions and exact object fixtures"
)]
impl PluginFactory for FormatPlugin {
    fn id(&self) -> &'static str {
        "format-state"
    }

    fn migrate_format(
        &self,
        from: lash_core::FormatVersion,
        namespace: lash_core::FormatNamespace,
        mut value: serde_json::Value,
    ) -> Result<serde_json::Value, lash_core::FormatRefusal> {
        if from != lash_core::FormatVersion::ONE {
            return Err(lash_core::FormatRefusal {
                plugin: "format-state".into(),
                namespace,
                stored: from,
                readable: crate::plugin::PluginMetadata::plugin_declaration(self).format_version,
            });
        }
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let map = value.as_object_mut().unwrap();
        let old = map.remove("count").unwrap();
        map.insert("total".into(), old);
        Ok(value)
    }
    fn encode_format(
        &self,
        to: lash_core::FormatVersion,
        namespace: lash_core::FormatNamespace,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, lash_core::FormatRefusal> {
        let mut value = value.clone();
        if to == lash_core::FormatVersion::ONE {
            let map = value.as_object_mut().unwrap();
            let total = map.remove("total").unwrap();
            map.insert("count".into(), total);
        } else if to != crate::plugin::PluginMetadata::plugin_declaration(self).format_version {
            return Err(lash_core::FormatRefusal {
                plugin: "format-state".into(),
                namespace,
                stored: to,
                readable: crate::plugin::PluginMetadata::plugin_declaration(self).format_version,
            });
        }
        Ok(value)
    }
    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Arc::new(self.clone()))
    }
}

#[expect(
    clippy::unwrap_used,
    reason = "format law uses known versions and exact object fixtures"
)]
impl crate::plugin::PluginDefinition for FormatPlugin {
    fn declaration() -> lash_core::plugin::PluginDeclaration {
        let mut declaration = lash_core::plugin::PluginDeclaration::initial("format-state");
        declaration.format_version = lash_core::FormatVersion::new(2).unwrap();
        declaration.writable_formats =
            vec![lash_core::FormatVersion::ONE, declaration.format_version];
        declaration
    }
}
impl SessionPlugin for FormatPlugin {
    fn id(&self) -> &'static str {
        "format-state"
    }
    fn register(&self, _: &mut PluginRegistrar) -> Result<(), PluginError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
    fn session_ready(&self, _: SessionReadyContext) -> Result<(), PluginError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// A commit whose plugin namespaces are stamped outside the fleet record's
/// writer range is refused typed and leaves the session's head where it was
/// (FIG-4746).
#[expect(
    clippy::unwrap_used,
    reason = "format law asserts every persisted fixture and commit"
)]
async fn assert_plugin_write_refused(
    store: &Arc<dyn RuntimeStore>,
    state: &RuntimeSessionState,
    session_id: &str,
) {
    let head = |store: Arc<dyn RuntimeStore>| async move {
        store
            .load_session_head_meta(&SessionId::fixture(session_id))
            .await
            .unwrap()
            .map(|head| (head.head_revision, head.checkpoint_ref))
    };
    let before = head(Arc::clone(store)).await;
    let refused = crate::testing::store_fixtures::commit_runtime_state_for_test(
        store,
        RuntimeCommit::persisted_state_for_test(state),
        "plugin-state-law",
    )
    .await;
    assert!(
        matches!(
            refused,
            Err(crate::StoreError::Incompatible {
                refusal: lash_core::compat::CompatRefusal::PluginWriterOutsideRange { .. }
            })
        ),
        "a plugin format outside the fleet's writer range must be refused: {refused:?}"
    );
    assert_eq!(head(Arc::clone(store)).await, before);
}

/// FIG-4745's refusal and migration at a real persisted checkpoint boundary.
#[expect(
    clippy::unwrap_used,
    reason = "format law asserts every persisted fixture and commit"
)]
pub(super) async fn plugin_format_boundary(
    store: Arc<dyn RuntimeStore>,
    session_id: &str,
    version: u32,
) {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(FormatPlugin(calls.clone())));
    let host = crate::PluginHost::new(factories);
    let mut state = RuntimeSessionState {
        session_id: crate::SessionId::fixture(session_id),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            crate::MaxToolCalls::new(1024),
        ))
    };
    // Include all bound namespaces so migration alone must dirty the component.
    let seed = host
        .build_session(PluginSessionRequest::creation(
            "format-seed",
            Default::default(),
        ))
        .unwrap();
    let mut snapshot = seed.export_state();
    snapshot.plugins.insert(
        "format-state".into(),
        lash_core::PluginNamespaceState {
            format_version: lash_core::FormatVersion::new(version).unwrap(),
            generation: 7,
            publication: Default::default(),
            fork: Default::default(),
            values: std::sync::Arc::new(std::collections::BTreeMap::from([(
                "count".into(),
                serde_json::json!(17),
            )])),
        },
    );
    state.set_plugin_state(Some(snapshot));
    state.authority.plugin_config.insert_versioned(
        "format-state",
        lash_core::FormatVersion::new(version).unwrap(),
        serde_json::json!({"count": 17}),
    );
    // The fleet record permits what the plugin's registration provisions
    // (FIG-4746): the fixture's stored format stands for a build that wrote
    // it, so that build's registration is what the store is provisioned from.
    let newest = version.max(2);
    let permitted = store
        .provision_plugin_writers(&[crate::store::plugin_writers::PluginWriterRegistration {
            plugin: "format-state".into(),
            native: lash_core::FormatVersion::new(newest).unwrap(),
            writable: (1..=newest)
                .map(|format| lash_core::FormatVersion::new(format).unwrap())
                .collect(),
        }])
        .await
        .unwrap()
        .permitted_writer("format-state")
        .unwrap();
    if !permitted.contains(version) {
        // Inside a rollback window the fleet permits only the oldest format,
        // so no build can have published this one: the store refuses it and
        // publishes nothing.
        assert_plugin_write_refused(&store, &state, session_id).await;
        return;
    }
    commit(&store, &mut state).await;
    let mut durable =
        crate::conformance::helpers::load_window_state(&store, &SessionId::fixture(session_id))
            .await
            .unwrap()
            .unwrap();
    let original = durable.plugin_state().unwrap().clone();
    let bytes = rmp_serde::to_vec_named(&original).unwrap();
    let original_head = durable.head_revision;
    calls.store(0, std::sync::atomic::Ordering::SeqCst);
    let record = support::transition(
        &host,
        session_id,
        &original,
        &durable.authority.plugin_config,
    )
    .await;
    if version > 2 {
        assert!(matches!(record.candidate(), Err(PluginError::Format(_))));
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
        let after =
            crate::conformance::helpers::load_window_state(&store, &SessionId::fixture(session_id))
                .await
                .unwrap()
                .unwrap();
        assert_eq!(after.head_revision, original_head);
        assert_eq!(
            rmp_serde::to_vec_named(after.plugin_state().unwrap()).unwrap(),
            bytes
        );
        return;
    }
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "the recorded state and config each convert once"
    );
    let recorded = rmp_serde::to_vec_named(&record).unwrap();
    let request = PluginSessionRequest::rematerialization(
        SessionId::fixture(session_id),
        &original,
        SessionAuthorityContext {
            plugin_config: durable.admitted_plugin_config(),
            ..Default::default()
        },
    );
    let decoded = host
        .isolated_registry()
        .defer_session(request.clone())
        .unwrap();
    decoded.adopt_plugin_transition(&record).unwrap();
    decoded.materialize().unwrap();
    let replay = host.isolated_registry().defer_session(request).unwrap();
    let replay_record = rmp_serde::from_slice(&recorded).unwrap();
    replay.adopt_plugin_transition(&replay_record).unwrap();
    replay.materialize().unwrap();
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        8,
        "two capability reconstructions run build/register/ready, with no converter replay"
    );
    assert_eq!(decoded.export_state(), replay.export_state());
    assert_eq!(
        decoded.admitted_plugin_config(),
        replay.admitted_plugin_config()
    );
    // Materialization publishes nothing.
    let before_commit =
        crate::conformance::helpers::load_window_state(&store, &SessionId::fixture(session_id))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(before_commit.head_revision, original_head);
    assert_eq!(
        rmp_serde::to_vec_named(before_commit.plugin_state().unwrap()).unwrap(),
        bytes
    );
    durable.refresh_plugin_states(&decoded).unwrap();
    let pending = RuntimeCommit::persisted_state_for_test(&durable);
    assert!(matches!(
        pending.checkpoint.components[crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT],
        crate::HydratedCheckpointComponent::Changed { .. }
    ));
    durable.authority.plugin_config = decoded.admitted_plugin_config().config.as_ref().clone();
    if !permitted.contains(2) {
        // Inside a rollback window the migrated namespace cannot be written
        // back in the plugin's native format.
        assert_plugin_write_refused(&store, &durable, session_id).await;
        // A session admitted inside the window writes the format its
        // admission chose from the fleet record (FIG-4747): the oldest one,
        // which the build beside it still reads. The commit lands, in the
        // stored shape.
        let admission = host.admit_plugins(store.as_ref()).await.unwrap();
        assert_eq!(admission.writer("format-state").unwrap().get(), 1);
        decoded.adopt_plugin_admission(admission);
        durable.refresh_plugin_states(&decoded).unwrap();
        commit(&store, &mut durable).await;
        let after =
            crate::conformance::helpers::load_window_state(&store, &SessionId::fixture(session_id))
                .await
                .unwrap()
                .unwrap();
        let namespace = &after.plugin_state().unwrap().plugins["format-state"];
        assert_eq!(namespace.format_version.get(), 1);
        assert_eq!(namespace.values["count"], serde_json::json!(17));
        let config = after
            .authority
            .plugin_config
            .namespace("format-state")
            .unwrap();
        assert_eq!(config.format_version.get(), 1);
        assert_eq!(config.value["count"], serde_json::json!(17));
        return;
    }
    // A finalized fleet admits the native format, and the admission chooses
    // it.
    let admission = host.admit_plugins(store.as_ref()).await.unwrap();
    assert_eq!(admission.writer("format-state").unwrap().get(), 2);
    decoded.adopt_plugin_admission(admission);
    durable.refresh_plugin_states(&decoded).unwrap();
    commit(&store, &mut durable).await;
    let after =
        crate::conformance::helpers::load_window_state(&store, &SessionId::fixture(session_id))
            .await
            .unwrap()
            .unwrap();
    let namespace = &after.plugin_state().unwrap().plugins["format-state"];
    assert_eq!(namespace.format_version.get(), 2);
    assert_eq!(namespace.generation, 8);
    assert_eq!(namespace.values["total"], serde_json::json!(17));
    assert_eq!(
        after
            .authority
            .plugin_config
            .namespace("format-state")
            .unwrap()
            .format_version
            .get(),
        2
    );
    assert_eq!(
        after.authority.plugin_config.get("format-state").unwrap()["total"],
        serde_json::json!(17)
    );
}
