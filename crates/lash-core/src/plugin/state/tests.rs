use super::*;

fn store() -> PluginStateStore {
    PluginStateStore::bind(
        &SessionId::from("session"),
        "mock",
        Arc::new(Mutex::new(PluginStateRegistry::default())),
    )
}

#[test]
fn state_keys_values_and_batches_are_atomic() {
    let state = store();
    fn assert_traits<T: Clone + Send + Sync + 'static>() {}
    assert_traits::<PluginStateStore>();
    assert_eq!(state.generation(), 0);
    for (key, reason) in [
        ("".to_string(), KeyRejection::Empty),
        ("x".repeat(129), KeyRejection::TooLong),
        (
            "a/b".to_string(),
            KeyRejection::IllegalCharacter { at: 1, byte: b'/' },
        ),
    ] {
        assert!(
            matches!(state.set(&key, Value::Null), Err(PluginStateError::InvalidKey { reason: found, .. }) if found == reason)
        );
        assert!(state.remove(&key).is_err());
    }
    assert_eq!(state.set(&"x".repeat(128), Value::Null).unwrap(), 1);
    assert_eq!(state.set("a._-09AZ", Value::Null).unwrap(), 2);
    assert_eq!(
        state.set("a._-09AZ", Value::Null).unwrap(),
        3,
        "identical writes bump"
    );
    assert_eq!(state.remove("missing").unwrap(), 3);
    assert_eq!(
        state.apply(vec![]).unwrap(),
        4,
        "accepted batches bump exactly once"
    );
    let before = state.state.lock_recover().data.clone();
    assert!(
        state
            .apply(vec![
                PluginStateEdit::Set {
                    key: "valid".into(),
                    value: Value::Bool(true)
                },
                PluginStateEdit::Set {
                    key: "illegal/".into(),
                    value: Value::Null
                }
            ])
            .is_err()
    );
    assert_eq!(state.state.lock_recover().data, before);
    assert!(
        matches!(state.set("huge", Value::String("x".repeat(VALUE_LIMIT - 1))), Err(PluginStateError::ValueTooLarge { bytes, limit: VALUE_LIMIT, .. }) if bytes == VALUE_LIMIT + 1)
    );
    assert_eq!(state.state.lock_recover().data, before);
    assert!(matches!(
        state.apply(
            (0..5)
                .map(|i| PluginStateEdit::Set {
                    key: format!("value{i}"),
                    value: Value::String("x".repeat(VALUE_LIMIT - 2))
                })
                .collect()
        ),
        Err(PluginStateError::StoreTooLarge { .. })
    ));
    assert_eq!(state.state.lock_recover().data, before);
    let generation = state
        .apply_guarded(
            4,
            vec![
                PluginStateEdit::Remove {
                    key: "a._-09AZ".into(),
                },
                PluginStateEdit::Set {
                    key: "valid".into(),
                    value: serde_json::json!([1, 2]),
                },
            ],
        )
        .unwrap();
    assert_eq!(generation, 5);
    let mut copy = state.get("valid").unwrap();
    copy.as_array_mut().unwrap().clear();
    assert_eq!(
        state.get("valid"),
        Some(serde_json::json!([1, 2])),
        "reads are owned"
    );
    assert!(matches!(
        state.get_as::<String>("valid"),
        Err(PluginStateError::Decode { .. })
    ));
    assert_eq!(state.get_as::<String>("missing").unwrap(), None);
}

#[test]
fn state_encoding_errors_leave_generation_and_values_unchanged() {
    struct Reject;
    impl Serialize for Reject {
        fn serialize<S: serde::Serializer>(&self, _: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("rejected"))
        }
    }
    let state = store();
    assert!(
        matches!(state.set_as("bad", &Reject), Err(PluginStateError::Encode { key, .. }) if key == "bad")
    );
    assert_eq!(state.generation(), 0);
    assert!(state.keys().is_empty());
}

#[test]
fn guarded_state_winner_is_atomic() {
    let state = store();
    let barrier = Arc::new(std::sync::Barrier::new(8));
    let workers = (0..8)
        .map(|i| {
            let state = state.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                state.apply_guarded(
                    0,
                    vec![
                        PluginStateEdit::Set {
                            key: "winner".into(),
                            value: serde_json::json!(i),
                        },
                        PluginStateEdit::Set {
                            key: format!("worker{i}"),
                            value: Value::Bool(true),
                        },
                    ],
                )
            })
        })
        .collect::<Vec<_>>();
    let results = workers
        .into_iter()
        .map(|w| w.join().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(results.iter().filter(|r| r.is_ok()).count(), 1);
    assert_eq!(state.generation(), 1);
    let winner = state.get_as::<u64>("winner").unwrap().unwrap();
    assert_eq!(
        state.keys(),
        vec!["winner".to_string(), format!("worker{winner}")]
    );
    assert!(
        results
            .into_iter()
            .filter_map(Result::err)
            .all(|e| matches!(
                e,
                PluginStateError::GenerationConflict {
                    expected: 0,
                    actual: 1
                }
            ))
    );
}

#[test]
fn fork_preserves_absent_namespaces_and_canonical_order() {
    let mut values = BTreeMap::new();
    values.insert("v".into(), serde_json::json!({"nested": {"z": 1, "a": 2}}));
    let durable = PluginState {
        plugins: BTreeMap::from([(
            "absent-plugin".into(),
            PluginNamespaceState {
                generation: 17,
                values,
            },
        )]),
    };
    let host = crate::PluginHost::empty();
    let parent = host
        .rematerialize_session(
            "parent",
            &durable,
            crate::plugin::RecordedSessionConfig::new(Default::default()),
        )
        .unwrap();
    let child = parent
        .fork_for_session("child", Default::default())
        .unwrap();
    assert_eq!(child.export_state(), parent.export_state());
    assert_eq!(
        child.export_state().plugins["absent-plugin"],
        durable.plugins["absent-plugin"]
    );
    let state = store();
    state
        .set("v", serde_json::json!({"z": {"b": 1, "a": 2}, "a": 3}))
        .unwrap();
    let first = rmp_serde::to_vec_named(&state.state.lock_recover().data).unwrap();
    let other = store();
    other
        .set("v", serde_json::json!({"a": 3, "z": {"a": 2, "b": 1}}))
        .unwrap();
    assert_eq!(
        first,
        rmp_serde::to_vec_named(&other.state.lock_recover().data).unwrap()
    );
}

#[test]
fn readiness_runs_after_hydration_and_its_writes_survive() {
    #[derive(Clone)]
    struct ReadyPlugin(Arc<Mutex<Vec<(String, u64)>>>, Arc<Mutex<Vec<u64>>>);
    impl crate::plugin::PluginFactory for ReadyPlugin {
        fn id(&self) -> &'static str {
            "ready-plugin"
        }
        fn build(
            &self,
            _: &crate::plugin::PluginSessionContext,
        ) -> Result<Arc<dyn crate::plugin::SessionPlugin>, crate::PluginError> {
            Ok(Arc::new(self.clone()))
        }
    }
    impl crate::plugin::SessionPlugin for ReadyPlugin {
        fn id(&self) -> &'static str {
            "ready-plugin"
        }
        fn register(
            &self,
            reg: &mut crate::plugin::PluginRegistrar,
        ) -> Result<(), crate::PluginError> {
            let token = reg.state().set("value", serde_json::json!("register"))?;
            self.1.lock_recover().push(token);
            Ok(())
        }
        fn session_ready(
            &self,
            ctx: crate::plugin::SessionReadyContext,
        ) -> Result<(), crate::PluginError> {
            assert_eq!(
                self.1.lock_recover().last().copied(),
                Some(ctx.state.generation()),
                "accepted registration generation must survive hydration"
            );
            self.0.lock_recover().push((
                ctx.state.get_as::<String>("value")?.unwrap(),
                ctx.state.generation(),
            ));
            ctx.state.set("value", serde_json::json!("ready"))?;
            Ok(())
        }
    }
    let observed = Arc::new(Mutex::new(Vec::new()));
    let host = crate::PluginHost::new(vec![Arc::new(ReadyPlugin(
        observed.clone(),
        Arc::new(Mutex::new(Vec::new())),
    ))]);
    let initial = host.build_session("initial").unwrap();
    let state = initial.export_state();
    assert_eq!(state.plugins["ready-plugin"].generation, 2);
    let rebuilt = host
        .rematerialize_session(
            "rebuilt",
            &state,
            crate::plugin::RecordedSessionConfig::new(Default::default()),
        )
        .unwrap();
    assert_eq!(
        *observed.lock_recover(),
        vec![("register".into(), 1), ("register".into(), 3)]
    );
    assert_eq!(rebuilt.export_state().plugins["ready-plugin"].generation, 4);
}

#[tokio::test]
async fn checkpoint_component_changes_iff_mediated_generation_moves() {
    let host = crate::PluginHost::empty();
    let plugins = host.build_session("generation-gate").unwrap();
    let handle = PluginStateStore::bind(
        &SessionId::from("generation-gate"),
        "mock",
        plugins.state.clone(),
    );
    let store: Arc<dyn crate::RuntimePersistence> = Arc::new(crate::InMemorySessionStore::new());
    let mut state = crate::RuntimeSessionState {
        session_id: "generation-gate".into(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    state.refresh_plugin_states(&plugins);
    let receipt = crate::testing::store_fixtures::commit_runtime_state_for_test(
        &store,
        crate::RuntimeCommit::persisted_state_for_test(&state, &[]),
        "generation-gate",
    )
    .await
    .unwrap();
    state.apply_persisted_commit_result(receipt);
    state.refresh_plugin_states(&plugins);
    assert!(matches!(
        crate::RuntimeCommit::persisted_state_for_test(&state, &[])
            .checkpoint
            .components[crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT],
        crate::HydratedCheckpointComponent::Unchanged { .. }
    ));
    handle.set("value", serde_json::json!(1)).unwrap();
    state.refresh_plugin_states(&plugins);
    assert!(
        matches!(
            crate::RuntimeCommit::persisted_state_for_test(&state, &[])
                .checkpoint
                .components[crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT],
            crate::HydratedCheckpointComponent::Changed { .. }
        ),
        "accepted write must make the checkpoint component Changed"
    );
}

#[tokio::test]
async fn write_after_capture_survives_commit_receipt_adoption() {
    let plugins = crate::PluginHost::empty()
        .build_session("capture-race")
        .unwrap();
    let handle = PluginStateStore::bind(
        &SessionId::from("capture-race"),
        "mock",
        plugins.state.clone(),
    );
    let store: Arc<dyn crate::RuntimePersistence> = Arc::new(crate::InMemorySessionStore::new());
    let mut state = crate::RuntimeSessionState {
        session_id: "capture-race".into(),
        ..crate::RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    handle.set("value", serde_json::json!(1)).unwrap();
    state.refresh_plugin_states(&plugins);
    let captured = crate::RuntimeCommit::persisted_state_for_test(&state, &[]);
    handle.set("value", serde_json::json!(2)).unwrap();
    let receipt = crate::testing::store_fixtures::commit_runtime_state_for_test(
        &store,
        captured,
        "capture-race",
    )
    .await
    .unwrap();
    state.apply_persisted_commit_result(receipt);
    let loaded = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        loaded.plugin_state().unwrap().plugins["mock"].values["value"],
        serde_json::json!(1)
    );
    state.refresh_plugin_states(&plugins);
    let next = crate::RuntimeCommit::persisted_state_for_test(&state, &[]);
    assert!(matches!(
        next.checkpoint.components[crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT],
        crate::HydratedCheckpointComponent::Changed { .. }
    ));
    let receipt = crate::testing::store_fixtures::commit_runtime_state_for_test(
        &store,
        next,
        "capture-race-next",
    )
    .await
    .unwrap();
    state.apply_persisted_commit_result(receipt);
    let loaded = crate::store::load_persisted_session_state(store.as_ref())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        loaded.plugin_state().unwrap().plugins["mock"].values["value"],
        serde_json::json!(2)
    );
}

#[test]
fn live_hydration_refuses_generation_rewind_and_preserves_bound_namespaces() {
    let state = store();
    state.state.lock_recover().initialize(None).unwrap();
    state.set("value", serde_json::json!(1)).unwrap();
    let old = state.state.lock_recover().data.clone();
    state.set("value", serde_json::json!(2)).unwrap();
    assert!(state.state.lock_recover().hydrate_live(&old).is_err());
    assert_eq!(state.generation(), 2);
    assert_eq!(state.get("value"), Some(serde_json::json!(2)));
    assert!(matches!(
        state.apply_guarded(1, vec![]),
        Err(PluginStateError::GenerationConflict { actual: 2, .. })
    ));
}

#[test]
fn state_handle_debug_does_not_expose_other_namespaces() {
    let registry = Arc::new(Mutex::new(PluginStateRegistry::default()));
    let first = PluginStateStore::bind(&SessionId::from("session"), "first", registry.clone());
    let second = PluginStateStore::bind(&SessionId::from("session"), "private-neighbor", registry);
    second
        .set("secret", serde_json::json!("neighbor-value"))
        .unwrap();
    let rendered = format!("{first:?}");
    assert!(!rendered.contains("private-neighbor"));
    assert!(!rendered.contains("neighbor-value"));
    assert_eq!(first.get("secret"), None);
}

#[test]
fn register_remove_rebuilt_generation_five() {
    let snapshot = PluginState {
        plugins: BTreeMap::from([(
            "mock".into(),
            PluginNamespaceState {
                generation: 5,
                values: BTreeMap::from([("seed".into(), Value::Bool(true))]),
            },
        )]),
    };
    let registry = Arc::new(Mutex::new(PluginStateRegistry::registering(Some(
        &snapshot,
    ))));
    let state = PluginStateStore::bind(&SessionId::from("rebuilt"), "mock", registry.clone());
    state.remove("seed").unwrap();
    registry.lock_recover().initialize(Some(&snapshot)).unwrap();
    assert_eq!(
        state.get("seed"),
        None,
        "register remove must delete the durable key"
    );
    assert_eq!(state.generation(), 6);
}

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
    let restricted_runtime = crate::LashRuntime::from_embedded_state(
        crate::testing::mock_session_policy(),
        crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )),
        crate::RuntimeServices::new(child.clone()),
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
    let runtime = crate::LashRuntime::from_embedded_state(
        crate::testing::mock_session_policy(),
        crate::EmbeddedRuntimeHost::new(crate::RuntimeHostConfig::in_memory(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        )),
        crate::RuntimeServices::new(host.session(&SessionId::from("private-child")).unwrap()),
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
