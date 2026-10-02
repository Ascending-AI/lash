use super::*;
use crate::plugin::PluginSessionRequest;

#[derive(Clone)]
struct FormatProbe(Arc<std::sync::atomic::AtomicUsize>);

impl crate::PluginFactory for FormatProbe {
    fn id(&self) -> &'static str {
        "format-probe"
    }

    fn declaration(&self) -> crate::plugin::PluginDeclaration {
        let mut declaration = crate::plugin::PluginDeclaration::initial(self.id());
        declaration.format_version = crate::FormatVersion::new(2).unwrap();
        declaration.writable_formats = vec![crate::FormatVersion::ONE, declaration.format_version];
        declaration
    }

    fn migrate_format(
        &self,
        from: crate::FormatVersion,
        namespace: crate::FormatNamespace,
        mut value: Value,
    ) -> Result<Value, crate::FormatRefusal> {
        if from == crate::FormatVersion::ONE {
            let value = value.as_object_mut().unwrap();
            if let Some(old) = value.remove("old") {
                value.insert("native".into(), old);
            }
            Ok(Value::Object(value.clone()))
        } else if from == self.declaration().format_version {
            Ok(value)
        } else {
            Err(crate::FormatRefusal {
                plugin: "format-probe".into(),
                namespace,
                stored: from,
                readable: self.declaration().format_version,
            })
        }
    }

    fn encode_format(
        &self,
        to: crate::FormatVersion,
        namespace: crate::FormatNamespace,
        value: &Value,
    ) -> Result<Value, crate::FormatRefusal> {
        let mut value = value.clone();
        if to == crate::FormatVersion::ONE {
            let object = value.as_object_mut().unwrap();
            if let Some(native) = object.remove("native") {
                object.insert("old".into(), native);
            }
            Ok(value)
        } else if to == self.declaration().format_version {
            Ok(value)
        } else {
            Err(crate::FormatRefusal {
                plugin: "format-probe".into(),
                namespace,
                stored: to,
                readable: self.declaration().format_version,
            })
        }
    }

    fn register_config(
        &self,
        _: &mut crate::ConfigRegistrar,
    ) -> Result<(), crate::ConfigRegistrationError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn build(
        &self,
        _: &crate::PluginSessionContext,
    ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Arc::new(self.clone()))
    }
}

impl crate::SessionPlugin for FormatProbe {
    fn id(&self) -> &'static str {
        "format-probe"
    }

    fn register(&self, _: &mut crate::PluginRegistrar) -> Result<(), crate::PluginError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn session_ready(
        &self,
        _: crate::plugin::SessionReadyContext,
    ) -> Result<(), crate::PluginError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn plugin_formats_refuse_before_callbacks_and_preserve_bytes() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host = crate::PluginHost::new(vec![Arc::new(FormatProbe(calls.clone()))]);
    let snapshot: PluginState = serde_json::from_value(serde_json::json!({
        "format-probe": {"generation": 7, "format_version": 4294967295_u32, "values": {"old": 17}}
    }))
    .unwrap();
    let bytes = rmp_serde::to_vec_named(&snapshot).unwrap();
    let result = host.build_session(PluginSessionRequest::rematerialization(
        "unreadable",
        &snapshot,
        Default::default(),
    ));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        result.is_err(),
        "an unreadable stamp must refuse materialization"
    );
    assert_eq!(rmp_serde::to_vec_named(&snapshot).unwrap(), bytes);
    let refusal = result.err().unwrap();
    let crate::PluginError::Format(expected) = refusal else {
        panic!("typed format refusal");
    };
    assert_eq!(expected.stored.get(), u32::MAX);
    assert_eq!(expected.namespace, crate::FormatNamespace::State);
    let plugin: crate::PluginError = rmp_serde::from_slice(
        &rmp_serde::to_vec_named(&crate::PluginError::Format(expected.clone())).unwrap(),
    )
    .unwrap();
    let controller = crate::RuntimeEffectControllerError::from(plugin);
    let runtime = controller.clone().into_runtime_error();
    let cause = Some(crate::RuntimeErrorCause::PluginFormat {
        refusal: Box::new(expected),
    });
    assert_eq!(runtime.cause, cause);
    assert!(runtime.is_terminal());
    assert_eq!(
        crate::PluginError::Runtime(runtime)
            .into_turn_failure(crate::RuntimeErrorCode::Plugin)
            .cause,
        cause
    );
    assert_eq!(
        crate::PluginError::RuntimeEffectController(controller)
            .into_turn_failure(crate::RuntimeErrorCode::Plugin)
            .cause,
        cause
    );

    let mut config = crate::PluginConfig::default();
    config.insert_versioned(
        "format-probe",
        crate::FormatVersion::new(u32::MAX).unwrap(),
        serde_json::json!({"old": 17}),
    );
    let config_bytes = rmp_serde::to_vec_named(&config).unwrap();
    let result = host.build_session(PluginSessionRequest::creation(
        "bad-config",
        crate::plugin::SessionAuthorityContext {
            plugin_config: crate::AdmittedPluginConfig::new(config.clone(), 3),
            ..Default::default()
        },
    ));
    assert!(matches!(
        result,
        Err(crate::PluginError::Format(crate::FormatRefusal {
            namespace: crate::FormatNamespace::Config,
            ..
        }))
    ));
    let options = crate::PluginOptions {
        plugins: config.namespaces().clone(),
    };
    let error = host
        .resolve_creation_plugin_config(None, &options, None, true)
        .unwrap_err();
    assert!(matches!(
        error,
        crate::plugin::CreationConfigError::Format(_)
    ));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(rmp_serde::to_vec_named(&config).unwrap(), config_bytes);
}

#[test]
fn plugin_formats_migrate_on_decode_and_replay_identically() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host = crate::PluginHost::new(vec![Arc::new(FormatProbe(calls))]);
    let snapshot: PluginState = serde_json::from_value(serde_json::json!({
        "format-probe": {"generation": 7, "format_version": 1, "values": {"old": 17}}
    }))
    .unwrap();
    let before = rmp_serde::to_vec_named(&snapshot).unwrap();
    let mut config = crate::PluginConfig::default();
    config.insert_versioned(
        "format-probe",
        crate::FormatVersion::ONE,
        serde_json::json!({"old": 17}),
    );
    let mut results = Vec::new();
    for owner in ["first-decode", "replay-decode"] {
        let session = host
            .isolated_registry()
            .build_session(PluginSessionRequest::rematerialization(
                owner,
                &snapshot,
                crate::plugin::SessionAuthorityContext {
                    plugin_config: crate::AdmittedPluginConfig::new(config.clone(), 3),
                    ..Default::default()
                },
            ))
            .unwrap();
        let decoded = session.export_state();
        assert_eq!(
            decoded.plugins["format-probe"].values.get("native"),
            Some(&serde_json::json!(17))
        );
        assert!(!decoded.plugins["format-probe"].values.contains_key("old"));
        let decoded_config = session.admitted_plugin_config();
        assert_eq!(
            decoded_config.config.get("format-probe"),
            Some(&serde_json::json!({"native": 17}))
        );
        assert_eq!(
            decoded_config
                .config
                .namespace("format-probe")
                .unwrap()
                .format_version
                .get(),
            2
        );
        assert_eq!(decoded_config.revision, 3);
        assert_eq!(decoded.plugins["format-probe"].generation, 8);
        session.hydrate_state(&snapshot).unwrap();
        assert_eq!(session.export_state(), decoded);
        results.push(rmp_serde::to_vec_named(&(decoded, decoded_config)).unwrap());
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(rmp_serde::to_vec_named(&snapshot).unwrap(), before);
}

#[test]
fn plugin_formats_stamp_every_state_write() {
    let state = store();
    state.set("value", serde_json::json!(17)).unwrap();
    let encoded = serde_json::to_value(&state.state.lock_recover().data).unwrap();
    assert_eq!(encoded["mock"]["format_version"], serde_json::json!(1));
    let host = crate::PluginHost::new(vec![Arc::new(FormatProbe(Arc::new(
        std::sync::atomic::AtomicUsize::new(0),
    )))]);
    let native: PluginState = serde_json::from_value(serde_json::json!({
        "format-probe": {"generation": 8, "format_version": 2, "values": {"native": 18}},
        "inactive": {"generation": 7, "format_version": 99, "values": {"opaque": 5}}
    }))
    .unwrap();
    let mut config = crate::PluginConfig::default();
    config.insert_versioned(
        "format-probe",
        crate::FormatVersion::new(2).unwrap(),
        serde_json::json!({"native": 18}),
    );
    config.insert_versioned(
        "inactive",
        crate::FormatVersion::new(99).unwrap(),
        serde_json::json!({"opaque": 5}),
    );
    for version in [1, 2] {
        let writer = crate::FormatVersion::new(version).unwrap();
        let writers = BTreeMap::from([("format-probe".into(), writer)]);
        let encoded_state = host.encode_state(&native, &writers).unwrap();
        let encoded_config = host.encode_config(&config, &writers).unwrap();
        let key = if version == 1 { "old" } else { "native" };
        assert_eq!(
            encoded_state.plugins["format-probe"].values[key],
            serde_json::json!(18)
        );
        assert_eq!(encoded_state.plugins["format-probe"].format_version, writer);
        assert_eq!(
            encoded_state.plugins["inactive"],
            native.plugins["inactive"]
        );
        assert_eq!(
            encoded_config.get("format-probe").unwrap()[key],
            serde_json::json!(18)
        );
        assert_eq!(
            encoded_config
                .namespace("format-probe")
                .unwrap()
                .format_version,
            writer
        );
        assert_eq!(
            encoded_config.namespace("inactive"),
            config.namespace("inactive")
        );
        let init = crate::SessionPluginInit::captured(
            encoded_state.clone(),
            Default::default(),
            Default::default(),
        )
        .unwrap();
        let init: crate::SessionPluginInit =
            rmp_serde::from_slice(&rmp_serde::to_vec_named(&init).unwrap()).unwrap();
        assert_eq!(init.plugin_state, encoded_state);
        let options = crate::PluginOptions {
            plugins: encoded_config.namespaces().clone(),
        };
        let options: crate::PluginOptions =
            rmp_serde::from_slice(&rmp_serde::to_vec_named(&options).unwrap()).unwrap();
        assert_eq!(options.plugins["format-probe"].format_version, writer);
        let environment = crate::ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::new(encoded_config, 3),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
        );
        let environment: crate::ProcessExecutionEnvSpec =
            rmp_serde::from_slice(&rmp_serde::to_vec_named(&environment).unwrap()).unwrap();
        assert_eq!(
            environment
                .plugin_config
                .config
                .namespace("format-probe")
                .unwrap()
                .format_version,
            writer
        );
    }
    assert!(
        host.encode_state(
            &native,
            &BTreeMap::from([("format-probe".into(), crate::FormatVersion::new(3).unwrap())])
        )
        .is_err()
    );
}

fn store() -> PluginStateStore {
    PluginStateStore::bind(
        &crate::RuntimeOwner::Session(SessionId::from("session")),
        "mock",
        Arc::new(Mutex::new(PluginStateRegistry::default())),
    )
}

#[test]
fn materialization_preserves_recorded_config_and_creation_kind() {
    use crate::plugin::*;

    #[derive(Clone)]
    struct ContextProbe(Arc<Mutex<Vec<PluginSessionContext>>>);
    impl PluginFactory for ContextProbe {
        fn id(&self) -> &'static str {
            "context-probe"
        }

        fn declaration(&self) -> crate::plugin::PluginDeclaration {
            crate::plugin::PluginDeclaration::initial(PluginFactory::id(self))
        }
        fn build(&self, ctx: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
            self.0.lock_recover().push(ctx.clone());
            Ok(Arc::new(self.clone()))
        }
    }
    impl SessionPlugin for ContextProbe {
        fn id(&self) -> &'static str {
            "context-probe"
        }
        fn register(&self, _: &mut PluginRegistrar) -> Result<(), PluginError> {
            Ok(())
        }
    }
    let contexts = Arc::new(Mutex::new(Vec::new()));
    let host = PluginHost::new(vec![Arc::new(ContextProbe(contexts.clone()))]);
    let created = host
        .build_session(PluginSessionRequest::creation(
            "created",
            Default::default(),
        ))
        .unwrap();
    let snapshot = created.export_state();
    let mut recorded = crate::plugin::PluginConfig::default();
    recorded.insert("context-probe", serde_json::json!({"recorded": 7}));
    let recorded = crate::plugin::AdmittedPluginConfig::new(recorded, 3);
    let restored = host
        .build_session(PluginSessionRequest {
            parent_session_id: Some("parent".into()),
            ..PluginSessionRequest::rematerialization(
                "restored",
                &snapshot,
                SessionAuthorityContext {
                    plugin_config: recorded.clone(),
                    ..Default::default()
                },
            )
        })
        .unwrap();
    let contexts = contexts.lock_recover();
    assert_eq!(
        contexts[0].materialization,
        PluginSessionMaterialization::Creation
    );
    assert_eq!(
        contexts[0].plugin_config,
        crate::plugin::AdmittedPluginConfig::default()
    );
    assert!(contexts[0].is_root_session());
    assert_eq!(
        contexts[1].materialization,
        PluginSessionMaterialization::Rematerialization
    );
    assert_eq!(contexts[1].plugin_config, recorded);
    assert_eq!(contexts[1].parent_session_id, Some("parent".into()));
    assert!(!restored.forked_plugins());
    restored.require_hydrated_state(&snapshot).unwrap();
}

#[test]
fn materialization_uses_spawn_capture_after_parent_changes_and_unregisters() {
    use crate::plugin::*;

    let host = PluginHost::empty();
    let durable = PluginState {
        plugins: BTreeMap::from([(
            "absent-plugin".into(),
            PluginNamespaceState {
                format_version: lash_core_ids::FormatVersion::ONE,
                generation: 17,
                values: BTreeMap::from([("value".into(), serde_json::json!("at-spawn"))]),
            },
        )]),
    };
    let parent = host
        .build_session(PluginSessionRequest {
            tool_catalog_overlay: ToolCatalogContribution::remove_tools(["hidden-at-spawn"]),
            tool_snapshot: Some(crate::ToolState::new(42, BTreeMap::new())),
            ..PluginSessionRequest::rematerialization(
                "parent",
                &durable,
                SessionAuthorityContext::default(),
            )
        })
        .unwrap();
    let init = parent.capture_fork_init().unwrap();
    let mut later = parent.export_state();
    let namespace = later.plugins.get_mut("absent-plugin").unwrap();
    namespace.generation += 1;
    namespace
        .values
        .insert("value".into(), serde_json::json!("after-spawn"));
    parent.hydrate_state(&later).unwrap();
    host.unregister_session(&"parent".into()).unwrap();
    drop(parent);
    let child = host
        .isolated_registry()
        .build_session(PluginSessionRequest {
            parent_session_id: Some("parent".into()),
            tool_catalog_overlay: init.tool_catalog_overlay.clone(),
            tool_snapshot: Some(init.tool_state.clone()),
            materialization: PluginSessionMaterializationRequest::Creation {
                config: SessionAuthorityContext::default(),
                seed_snapshot: Some(&init.plugin_state),
            },
            owner: crate::RuntimeOwner::Session("child".into()),
        })
        .unwrap();
    assert!(child.forked_plugins());
    let captured = child.capture_fork_init().unwrap();
    assert_eq!(captured.plugin_state, init.plugin_state);
    assert_eq!(
        captured.tool_catalog_overlay.remove,
        init.tool_catalog_overlay.remove
    );
    assert_eq!(captured.tool_state.generation, init.tool_state.generation);
    assert_eq!(captured.tool_state.entries(), init.tool_state.entries());
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
                format_version: lash_core_ids::FormatVersion::ONE,
                generation: 17,
                values,
            },
        )]),
    };
    let host = crate::PluginHost::empty();
    let parent = host
        .build_session(PluginSessionRequest::rematerialization(
            "parent",
            &durable,
            crate::plugin::SessionAuthorityContext::default(),
        ))
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

        fn declaration(&self) -> crate::plugin::PluginDeclaration {
            crate::plugin::PluginDeclaration::initial(crate::plugin::PluginFactory::id(self))
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
    let initial = host
        .build_session(PluginSessionRequest::creation(
            "initial",
            Default::default(),
        ))
        .unwrap();
    let state = initial.export_state();
    assert_eq!(state.plugins["ready-plugin"].generation, 2);
    let rebuilt = host
        .build_session(PluginSessionRequest::rematerialization(
            "rebuilt",
            &state,
            crate::plugin::SessionAuthorityContext::default(),
        ))
        .unwrap();
    assert_eq!(
        *observed.lock_recover(),
        vec![("register".into(), 1), ("register".into(), 3)]
    );
    assert_eq!(rebuilt.export_state().plugins["ready-plugin"].generation, 4);
}

#[test]
fn live_hydration_adopts_the_recorded_head_over_an_uncommitted_tail() {
    let state = store();
    state.state.lock_recover().initialize(None).unwrap();
    state.set("value", serde_json::json!(1)).unwrap();
    let recorded = state.state.lock_recover().data.clone();
    state.set("value", serde_json::json!(2)).unwrap();
    state.state.lock_recover().hydrate_live(&recorded);
    assert_eq!(state.generation(), 3);
    assert_eq!(state.get("value"), Some(serde_json::json!(1)));
    assert!(matches!(
        state.apply_guarded(2, vec![]),
        Err(PluginStateError::GenerationConflict { actual: 3, .. })
    ));

    let mut diverged = recorded.clone();
    let namespace = diverged.plugins.get_mut("mock").unwrap();
    namespace
        .values
        .insert("value".into(), serde_json::json!(3));
    state.set("value", serde_json::json!(4)).unwrap();
    state.state.lock_recover().hydrate_live(&diverged);
    assert_eq!(
        (state.generation(), state.get("value")),
        (5, Some(serde_json::json!(3))),
        "an equal-generation tail yields to the recorded head's values"
    );

    state
        .state
        .lock_recover()
        .hydrate_live(&PluginState::default());
    assert_eq!(
        (state.generation(), state.keys()),
        (6, Vec::<String>::new()),
        "a bound namespace the head does not carry retains its acceptance token"
    );
}

/// A runtime rematerialized from a head, then handed an accepted write, is
/// re-hydrated from that same head when the bound turn left its plugin
/// state unchanged: the tail is dropped all the same (FIG-4600).
#[test]
fn live_hydration_from_the_unchanged_source_head_drops_the_uncommitted_tail() {
    let recorded = {
        let state = store();
        state.state.lock_recover().initialize(None).unwrap();
        state.set("value", serde_json::json!(1)).unwrap();
        state.state.lock_recover().data.clone()
    };
    let registry = Arc::new(Mutex::new(PluginStateRegistry::registering(Some(
        &recorded,
    ))));
    let state = PluginStateStore::bind(
        &crate::RuntimeOwner::Session(SessionId::from("session")),
        "mock",
        registry.clone(),
    );
    state.set("registered", serde_json::json!(true)).unwrap();
    registry.lock_recover().initialize(Some(&recorded)).unwrap();
    registry.lock_recover().hydrate_live(&recorded);
    assert_eq!(
        (state.generation(), state.get("registered")),
        (2, Some(serde_json::json!(true))),
        "a state still equal to its hydration keeps its registration writes"
    );

    state.set("value", serde_json::json!(2)).unwrap();
    assert!(state.remove("registered").is_ok());
    registry.lock_recover().hydrate_live(&recorded);
    assert_eq!(
        (state.generation(), state.get("value"), state.keys().len()),
        (5, Some(serde_json::json!(1)), 2),
        "the head the runtime was built from does not carry the accepted writes"
    );

    state.remove("value").unwrap();
    registry.lock_recover().hydrate_live(&recorded);
    assert_eq!(
        (state.generation(), state.get("value")),
        (7, Some(serde_json::json!(1))),
        "a removal is a tail as a set is"
    );
}

#[test]
fn state_handle_debug_does_not_expose_other_namespaces() {
    let registry = Arc::new(Mutex::new(PluginStateRegistry::default()));
    let first = PluginStateStore::bind(
        &crate::RuntimeOwner::Session(SessionId::from("session")),
        "first",
        registry.clone(),
    );
    let second = PluginStateStore::bind(
        &crate::RuntimeOwner::Session(SessionId::from("session")),
        "private-neighbor",
        registry,
    );
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
                format_version: lash_core_ids::FormatVersion::ONE,
                generation: 5,
                values: BTreeMap::from([("seed".into(), Value::Bool(true))]),
            },
        )]),
    };
    let registry = Arc::new(Mutex::new(PluginStateRegistry::registering(Some(
        &snapshot,
    ))));
    let state = PluginStateStore::bind(
        &crate::RuntimeOwner::Session(SessionId::from("rebuilt")),
        "mock",
        registry.clone(),
    );
    state.remove("seed").unwrap();
    registry.lock_recover().initialize(Some(&snapshot)).unwrap();
    assert_eq!(
        state.get("seed"),
        None,
        "register remove must delete the durable key"
    );
    assert_eq!(state.generation(), 6);
}

fn hydration_fixture(
    snapshot: &PluginState,
) -> (PluginStateStore, Arc<Mutex<PluginStateRegistry>>) {
    let registry = Arc::new(Mutex::new(PluginStateRegistry::registering(Some(snapshot))));
    let state = PluginStateStore::bind(
        &crate::RuntimeOwner::Session(SessionId::from("session")),
        "mock",
        registry.clone(),
    );
    state.set("registered", Value::Bool(true)).unwrap();
    registry.lock_recover().initialize(Some(snapshot)).unwrap();
    (state, registry)
}

fn hydration_head() -> PluginState {
    PluginState {
        plugins: BTreeMap::from([(
            "mock".into(),
            PluginNamespaceState {
                format_version: lash_core_ids::FormatVersion::ONE,
                generation: 5,
                values: BTreeMap::from([("seed".into(), Value::Bool(true))]),
            },
        )]),
    }
}

fn assert_hydration_predicate_sees_ready_writes(
    predicate: impl Fn(&PluginStateRegistry, &PluginState) -> bool,
) {
    let snapshot = hydration_head();
    for mutation in 0..5 {
        let (state, registry) = hydration_fixture(&snapshot);
        assert!(predicate(&registry.lock_recover(), &snapshot));
        let before = state.generation();
        assert!(state.set("invalid/key", Value::Null).is_err());
        assert!(state.apply_guarded(before + 1, vec![]).is_err());
        state.remove("absent").unwrap();
        assert!(
            predicate(&registry.lock_recover(), &snapshot),
            "refusals and absent removes leave hydration intact"
        );
        match mutation {
            0 => {
                state.set("tail", Value::Bool(true)).unwrap();
            }
            1 => {
                state.remove("seed").unwrap();
            }
            2 => {
                state.apply(vec![]).unwrap();
            }
            3 => {
                state.apply_guarded(before, vec![]).unwrap();
            }
            4 => {
                state.set("registered", Value::Bool(true)).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            !predicate(&registry.lock_recover(), &snapshot),
            "accepted mutation {mutation} invalidates hydration"
        );
    }
}

#[test]
fn hydration_law_was_hydrated_from_sees_ready_writes() {
    assert_hydration_predicate_sees_ready_writes(|registry, snapshot| {
        registry.was_hydrated_from(snapshot)
    });
}

#[test]
fn hydration_law_matches_ref_sees_ready_writes() {
    assert_hydration_predicate_sees_ready_writes(|registry, snapshot| {
        registry.matches_ref(&state_ref(snapshot))
    });
}

#[test]
fn hydration_law_live_adoption_sees_ready_writes() {
    let snapshot = hydration_head();
    let (state, registry) = hydration_fixture(&snapshot);
    state.remove("registered").unwrap();
    registry.lock_recover().hydrate_live(&snapshot);
    assert_eq!(
        state.get("registered"),
        Some(Value::Bool(true)),
        "adoption restores the registration edit after a Ready write"
    );
    assert!(registry.lock_recover().was_hydrated_from(&snapshot));
    assert!(registry.lock_recover().matches_ref(&state_ref(&snapshot)));
}

#[test]
fn hydration_law_live_adoption_equals_a_cold_rebuild() {
    let snapshot = hydration_head();
    let (state, registry) = hydration_fixture(&snapshot);
    let newcomer = PluginStateStore::bind(state.owner(), "newcomer", registry.clone());
    state.set("tail", Value::Bool(true)).unwrap();
    newcomer.set("tail", Value::Bool(true)).unwrap();
    for head in [snapshot.clone(), PluginState::default(), snapshot] {
        let (_, cold) = hydration_fixture(&head);
        PluginStateStore::bind(state.owner(), "newcomer", cold.clone());
        registry.lock_recover().hydrate_live(&head);
        assert_eq!(
            registry.lock_recover().data,
            cold.lock_recover().data,
            "live checkpoint includes the same registration edits and bound namespaces as a cold rebuild"
        );
        state.set("tail", Value::Bool(true)).unwrap();
    }
}

#[test]
fn hydration_law_generations_never_reuse_a_guard_token() {
    let snapshot = hydration_head();
    let (state, registry) = hydration_fixture(&snapshot);
    let mut stale = Vec::new();
    for head in [snapshot.clone(), PluginState::default(), snapshot] {
        let accepted = state.set("tail", Value::Bool(true)).unwrap();
        stale.push(accepted);
        registry.lock_recover().hydrate_live(&head);
        let adopted = state.generation();
        assert!(
            adopted > accepted,
            "changing content invalidates every previously observed generation"
        );
        for expected in &stale {
            assert_eq!(
                state.apply_guarded(*expected, vec![]),
                Err(PluginStateError::GenerationConflict {
                    expected: *expected,
                    actual: adopted
                })
            );
        }
        registry.lock_recover().hydrate_live(&head);
        assert_eq!(
            state.generation(),
            adopted,
            "repeating the same hydration is idempotent"
        );
        assert_eq!(state.apply_guarded(adopted, vec![]).unwrap(), adopted + 1);
    }
}
