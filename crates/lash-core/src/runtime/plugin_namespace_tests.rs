use crate::SessionId;
use lash_sansio::sync::MutexExt;
use std::sync::{Arc, Mutex};

#[tokio::test]
async fn plugin_context_host_exports_cannot_escape_namespaces() {
    #[derive(Clone)]
    struct Fixture {
        id: &'static str,
        hosts: Arc<Mutex<Vec<(SessionId, crate::PluginHost)>>>,
    }
    impl crate::plugin::PluginFactory for Fixture {
        fn id(&self) -> &'static str {
            self.id
        }
        fn build(
            &self,
            _: &crate::plugin::PluginSessionContext,
        ) -> Result<Arc<dyn crate::plugin::SessionPlugin>, crate::PluginError> {
            Ok(Arc::new(self.clone()))
        }
    }
    impl crate::plugin::SessionPlugin for Fixture {
        fn id(&self) -> &'static str {
            self.id
        }
        fn register(
            &self,
            reg: &mut crate::plugin::PluginRegistrar,
        ) -> Result<(), crate::PluginError> {
            let state = reg.state();
            state.set(self.id, serde_json::json!(self.id))?;
            let hosts = self.hosts.clone();
            reg.turn().before(Arc::new(move |ctx| {
                let hosts = hosts.clone();
                let state = state.clone();
                Box::pin(async move {
                    assert_eq!(state.keys(), vec![state.plugin_id().to_string()]);
                    for (id, host) in hosts.lock_recover().iter() {
                        let session = host.session(&SessionId::from(id)).unwrap();
                        assert!(session.export_state().plugins.is_empty());
                        let mut exported =
                            crate::RuntimeSessionState::new(crate::testing::mock_session_policy());
                        exported.refresh_plugin_states(&session);
                        assert!(
                            exported
                                .plugin_state()
                                .is_none_or(|state| state.plugins.is_empty()),
                            "public refresh must respect restricted namespace exports"
                        );
                    }
                    let snapshot = serde_json::to_string(&ctx.state.to_snapshot()).unwrap();
                    assert!(!snapshot.contains("neighbor-secret-key"));
                    Ok(Vec::new())
                })
            }));
            Ok(())
        }
        fn session_ready(
            &self,
            ctx: crate::plugin::SessionReadyContext,
        ) -> Result<(), crate::PluginError> {
            assert_eq!(ctx.state.keys(), vec![self.id.to_string()]);
            let session = ctx.host.session(&ctx.session_id).unwrap();
            assert!(session.export_state().plugins.is_empty());
            assert!(
                session
                    .host()
                    .session(&ctx.session_id)
                    .unwrap()
                    .export_state()
                    .plugins
                    .is_empty()
            );
            self.hosts.lock_recover().push((ctx.session_id, ctx.host));
            Ok(())
        }
    }
    struct ProtocolObserver(Arc<std::sync::atomic::AtomicUsize>);
    #[async_trait::async_trait]
    impl crate::plugin::ProtocolSessionPlugin for ProtocolObserver {
        async fn restore_session(
            &self,
            _ctx: crate::plugin::ProtocolSessionContext<'_>,
            state: crate::plugin::ProtocolSessionRestoreView,
        ) -> Result<(), crate::SessionError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let visible = format!("{state:?}");
            assert!(
                !visible.contains("neighbor-secret-key"),
                "protocol restore must not expose a decoded neighbor namespace: {visible}"
            );
            Ok(())
        }
    }
    struct Sessions;
    #[async_trait::async_trait]
    impl crate::plugin::SessionStateService for Sessions {}
    let hosts = Arc::new(Mutex::new(Vec::new()));
    let restores = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let mut factories = vec![
        crate::testing::test_standard_protocol_factory_with_runtime_state(
            Arc::new(ProtocolObserver(restores.clone())),
            None,
        ),
    ];
    factories.extend(vec![
        Arc::new(Fixture {
            id: "observer",
            hosts: hosts.clone(),
        }),
        Arc::new(Fixture {
            id: "neighbor-secret-key",
            hosts: hosts.clone(),
        }) as Arc<dyn crate::plugin::PluginFactory>,
    ]);
    let host = crate::PluginHost::new(factories);
    let parent = host.build_session("private-parent").unwrap();
    assert!(
        parent.export_state().plugins["neighbor-secret-key"]
            .values
            .contains_key("neighbor-secret-key")
    );
    let restricted = hosts.lock_recover()[0]
        .1
        .session(&SessionId::from("private-parent"))
        .unwrap();
    let child = restricted
        .fork_for_session("private-child", Default::default())
        .unwrap();
    assert!(child.export_state().plugins.is_empty());
    assert!(
        host.session(&SessionId::from("private-child"))
            .unwrap()
            .export_state()
            .plugins
            .contains_key("neighbor-secret-key")
    );
    assert!(
        child
            .host()
            .session(&SessionId::from("private-parent"))
            .unwrap()
            .export_state()
            .plugins
            .is_empty()
    );
    let mut runtime_state =
        crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded));
    runtime_state.capture_plugin_states(&child);
    assert!(
        runtime_state.plugin_state().unwrap().plugins["neighbor-secret-key"]
            .values
            .contains_key("neighbor-secret-key"),
        "restricted exports must not strip runtime checkpoint or fork state"
    );
    runtime_state.session_id = "private-child".into();
    let runtime_host = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::new(
        crate::testing::memory_store_backend().await,
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    ));
    let runtime_services = crate::RuntimeServices::new(
        child.clone(),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let restricted_runtime = crate::LashRuntime::from_embedded_state(
        crate::testing::mock_session_policy(),
        runtime_host,
        runtime_services,
        runtime_state.clone(),
        crate::testing::runtime_lease_owner(),
    )
    .await;
    assert!(matches!(
        restricted_runtime,
        Err(crate::SessionError::Plugin(crate::PluginError::Session(message)))
            if message == "plugin-facing session handles cannot construct a host runtime"
    ));
    assert_eq!(restores.load(std::sync::atomic::Ordering::SeqCst), 0);
    let runtime_host = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::new(
        crate::testing::memory_store_backend().await,
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
    ));
    let runtime_services = crate::RuntimeServices::new(
        host.session(&SessionId::from("private-child")).unwrap(),
        std::sync::Arc::clone(&runtime_host.core.durability.attachment_store),
        std::sync::Arc::clone(&runtime_host.core.durability.process_env_store),
    );
    let runtime = crate::LashRuntime::from_embedded_state(
        crate::testing::mock_session_policy(),
        runtime_host,
        runtime_services,
        runtime_state.clone(),
        crate::testing::runtime_lease_owner(),
    )
    .await
    .unwrap();
    assert_eq!(restores.load(std::sync::atomic::Ordering::SeqCst), 1);
    drop(runtime);
    parent
        .before_turn(crate::plugin::TurnHookContext {
            session_id: "private-parent".into(),
            state: crate::plugin::SessionReadView::from_persisted_state(&runtime_state)
                .expect("test runtime frame scope resolves"),
            sessions: Arc::new(Sessions),
            turn_context: Default::default(),
        })
        .await
        .unwrap();
}
