use crate::SessionId;
use crate::plugin::PluginSessionRequest;

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

        fn initialize_state(
            &self,
            _: &crate::RuntimeOwner,
            _: &crate::PluginConfig,
        ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, crate::PluginError>
        {
            Ok(std::collections::BTreeMap::from([(
                self.id.into(),
                serde_json::json!(self.id),
            )]))
        }
        fn recipe(
            self: Arc<Self>,
        ) -> std::result::Result<lash_core::plugin::PluginRecipe, lash_core::plugin::PluginError>
        {
            let mut reg = lash_core::plugin::PluginRegistrar::<Fixture>::new(
                lash_core::plugin::PluginFactory::id(self.as_ref()),
            );
            Fixture::declare(&mut reg)?;
            let id = self.id();
            Ok(lash_core::plugin::PluginRecipe::new(
                id,
                reg,
                move |_ctx, _state| Ok(Arc::new(self.as_ref().clone())),
            ))
        }
    }

    impl crate::plugin::PluginMetadata for Fixture {
        fn plugin_declaration(&self) -> crate::plugin::PluginDeclaration {
            crate::plugin::PluginDeclaration::initial(self.id)
        }
    }
    impl crate::plugin::SessionPlugin for Fixture {
        fn id(&self) -> &'static str {
            self.id
        }

        fn session_ready(
            &self,
            ctx: crate::plugin::SessionReadyContext,
        ) -> Result<(), crate::PluginError> {
            assert_eq!(ctx.state.keys(), vec![self.id.to_string()]);
            let session_id = ctx.owner.session_id().unwrap().clone();
            let session = ctx.host.session(&session_id).unwrap();
            assert!(session.export_state().plugins.is_empty());
            assert!(
                session
                    .host()
                    .session(&session_id)
                    .unwrap()
                    .export_state()
                    .plugins
                    .is_empty()
            );
            self.hosts.lock_recover().push((session_id, ctx.host));
            Ok(())
        }
    }
    impl Fixture {
        fn declare(
            reg: &mut lash_core::plugin::PluginRegistrar<Self>,
        ) -> std::result::Result<(), lash_core::plugin::PluginError> {
            reg.turn()
                .before(crate::hook_key!("turn-before-1"), move |this, state| {
                    let state = state.clone();
                    let hosts = this.hosts.clone();
                    Ok(Arc::new(move |ctx| {
                        let hosts = hosts.clone();
                        let state = state.clone();
                        Box::pin(async move {
                            assert_eq!(state.keys(), vec![state.plugin_id().to_string()]);
                            for (id, host) in hosts.lock_recover().iter() {
                                let session = host.session(&SessionId::from(id)).unwrap();
                                assert!(session.export_state().plugins.is_empty());
                                let mut exported = crate::RuntimeSessionState::fixture(
                                    crate::testing::mock_session_policy(),
                                );
                                exported
                                    .refresh_plugin_states(&session)
                                    .expect("the live plugin state is captured");
                                assert!(
                                    exported
                                        .plugin_state()
                                        .is_none_or(|state| state.plugins.is_empty()),
                                    "public refresh must respect restricted namespace exports"
                                );
                            }
                            let snapshot = serde_json::to_string(&ctx.state.to_snapshot()).unwrap();
                            assert!(!snapshot.contains("neighbor-secret-key"));
                            Ok(Default::default())
                        })
                    }))
                })?;
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
    impl crate::core_internal::RuntimeService for Sessions {}

    #[async_trait::async_trait]
    impl crate::plugin::SessionReadService for Sessions {
        async fn committed_turn(
            &self,
            _session_id: &crate::SessionId,
            _turn_id: &crate::TurnId,
        ) -> Result<
            Option<crate::transcript::CommittedTurnWithCalls>,
            crate::plugin::CommittedTurnReadError,
        > {
            Err(crate::PluginError::Session("operation is absent from this fixture".into()).into())
        }

        async fn snapshot_current(
            &self,
        ) -> Result<lash_core::SessionSnapshot, lash_core::PluginError> {
            Err(lash_core::PluginError::Session(
                "operation is absent from this fixture".into(),
            ))
        }
        async fn snapshot_session(
            &self,
            _session_id: &lash_core::SessionId,
        ) -> Result<lash_core::SessionSnapshot, lash_core::PluginError> {
            Err(lash_core::PluginError::Session(
                "operation is absent from this fixture".into(),
            ))
        }
        async fn tool_catalog(
            &self,
            _session_id: &lash_core::SessionId,
        ) -> Result<Vec<serde_json::Value>, lash_core::PluginError> {
            Err(lash_core::PluginError::Session(
                "operation is absent from this fixture".into(),
            ))
        }
    }
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
    let host = crate::PluginHost::new(
        factories,
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
    );
    let parent = host
        .defer_session(PluginSessionRequest::creation(
            "private-parent",
            crate::plugin::SessionAuthorityContext::fixture(),
        ))
        .unwrap();
    let transition = |id: &str, state: &crate::PluginState| {
        host.transition_plugins(
            crate::plugin::PluginTransitionRequest {
                id: crate::plugin::PluginTransitionId(
                    crate::EffectAddress::new(
                        crate::ExecutionScope::turn(crate::SessionId::fixture(id), "run"),
                        "plugin-transition",
                    )
                    .unwrap(),
                ),
                owner: crate::RuntimeOwner::Session(crate::SessionId::fixture(id)),
                base: crate::plugin::PluginTransitionBase::Session {
                    head: crate::store::SessionHeadRef {
                        generation: 0,
                        revision: 0,
                        leaf: None,
                        checkpoint: None,
                    },
                },
                target: crate::store::plugin_writers::PluginAdmission::from_plugins(
                    host.factories()
                        .iter()
                        .map(|factory| {
                            let declaration = factory.plugin_declaration();
                            crate::store::plugin_writers::AdmittedPlugin {
                                plugin: factory.id().into(),
                                writer: declaration.format_version,
                            }
                        })
                        .collect(),
                ),
            },
            state,
            &Default::default(),
        )
    };
    parent
        .adopt_plugin_transition(&transition("private-parent", &parent.export_state()))
        .unwrap();
    parent.materialize().unwrap();
    assert!(
        parent.export_state().plugins["neighbor-secret-key"]
            .values
            .contains_key("neighbor-secret-key")
    );
    let restricted = hosts.lock_recover()[0]
        .1
        .session(&SessionId::from("private-parent"))
        .unwrap();
    // Fork and checkpoint capture belong to the runtime owner; the plugin
    // observes the resulting child only through its restricted host.
    let owned_child = parent
        .fork_for_session(
            "private-child",
            crate::plugin::SessionAuthorityContext::fixture(),
        )
        .unwrap();
    let child = restricted
        .host()
        .session(&SessionId::from("private-child"))
        .unwrap();
    owned_child
        .adopt_plugin_transition(&transition("private-child", &owned_child.export_state()))
        .unwrap();
    assert!(
        child
            .native_view(lash_core_store::store::FleetFormat::current())
            .unwrap()
            .is_none()
    );
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
    let mut runtime_state = crate::RuntimeSessionState::fixture(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
        crate::NoProgressBudget::bounded(12),
    ));
    runtime_state
        .capture_plugin_states(&owned_child, lash_core_store::store::FleetFormat::current())
        .expect("the live plugin state is captured");
    assert!(
        runtime_state.plugin_state().unwrap().plugins["neighbor-secret-key"]
            .values
            .contains_key("neighbor-secret-key"),
        "restricted exports must not strip runtime checkpoint or fork state"
    );
    runtime_state.session_id = "private-child".into();
    let runtime_host = crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::new(
        crate::testing::sqlite_memory_store_backend().await,
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
        crate::ExecutionBudgets::recommended(),
        crate::runtime::DeltaCoalescing::recommended(),
        crate::DataRetentionConfig::standard(),
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
        crate::testing::sqlite_memory_store_backend().await,
        crate::CommitBudget::bounded(1024 * 1024, 512),
        crate::QueuedWorkBatchingConfig::new(1),
        crate::ExecutionBudgets::recommended(),
        crate::runtime::DeltaCoalescing::recommended(),
        crate::DataRetentionConfig::standard(),
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
        .dispatch(None)
        .before_turn(crate::plugin::TurnHookContext {
            session_id: "private-parent".into(),
            state: crate::plugin::SessionReadView::from_persisted_state(&runtime_state),
            sessions: Arc::new(Sessions),
            turn_context: Default::default(),
            plugin_config: Default::default(),
            trace: Default::default(),
        })
        .await
        .unwrap();
}
