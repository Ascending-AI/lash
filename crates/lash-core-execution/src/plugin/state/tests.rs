use super::*;
use crate::SessionId;
use crate::plugin::PluginSessionRequest;

#[tokio::test]
async fn an_equal_format_different_revision_redrive_parks_before_callbacks_or_effects() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let callbacks = Arc::new(AtomicUsize::new(0));
    let effects = Arc::new(AtomicUsize::new(0));
    let mut declaration = crate::plugin::PluginDeclaration::initial("revision-probe");
    declaration.behavior_revision = crate::plugin::BehaviorRevision::new(2).unwrap();
    let counted_callbacks = callbacks.clone();
    let counted_effects = effects.clone();
    let host = crate::PluginHost::new(vec![Arc::new(crate::plugin::StaticPluginFactory::new(
        declaration,
        crate::plugin::PluginSpec::new().with_before_turn(Arc::new(move |_| {
            counted_callbacks.fetch_add(1, Ordering::SeqCst);
            let effects = counted_effects.clone();
            Box::pin(async move {
                effects.fetch_add(1, Ordering::SeqCst);
                Ok(Vec::new())
            })
        })),
    ))]);
    let recorded = crate::store::plugin_writers::PluginAdmission::from_plugins(
        host.factories()
            .iter()
            .map(|factory| {
                let declaration = factory.declaration();
                crate::store::plugin_writers::AdmittedPlugin {
                    plugin: factory.id().into(),
                    behavior_revision: crate::plugin::BehaviorRevision::ONE,
                    writer: declaration.format_version,
                }
            })
            .collect(),
    );
    let session = host
        .build_session(PluginSessionRequest::creation(
            "revision-redrive",
            Default::default(),
        ))
        .unwrap();
    let identity = &session.contributions.before_turn_hooks[0].identity;
    assert_eq!(identity.key, "before_turn:0");
    assert_eq!(identity.owner.plugin, "revision-probe");
    assert_eq!(identity.owner.behavior_revision.get(), 2);
    let rebuilt = host
        .isolated_registry()
        .build_session(PluginSessionRequest::creation(
            "revision-redrive",
            Default::default(),
        ))
        .unwrap();
    assert_eq!(
        identity,
        &rebuilt.contributions.before_turn_hooks[0].identity
    );
    session.adopt_plugin_admission(recorded);
    let state = crate::RuntimeSessionState::new(crate::SessionPolicy::new(
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ));
    let result = session
        .dispatch(None)
        .before_turn(crate::plugin::TurnHookContext {
            session_id: "revision-redrive".into(),
            plugin_config: Default::default(),
            state: state.read_view(),
            sessions: Arc::new(crate::testing::MockSessionManager::default()),
            turn_context: Default::default(),
        })
        .await;
    assert_eq!(
        callbacks.load(Ordering::SeqCst),
        0,
        "the substituted revision must never enter a callback"
    );
    assert_eq!(effects.load(Ordering::SeqCst), 0);
    let error = result
        .unwrap_err()
        .into_turn_failure(crate::RuntimeErrorCode::Plugin);
    assert!(
        crate::store::ParkReason::of_error(&error).is_some(),
        "the refusal must park unfinished work"
    );
    assert_eq!(
        error.code,
        crate::RuntimeErrorCode::PluginRevisionUnavailable
    );
    let park = crate::store::ParkReason::of_error(&error).unwrap();
    let park_bytes = serde_json::to_vec(&park).unwrap();
    let replayed: crate::store::ParkReason = serde_json::from_slice(&park_bytes).unwrap();
    assert_eq!(park, replayed);
    let controller = crate::RuntimeEffectControllerError::from(error);
    let plugin = crate::PluginError::RuntimeEffectController(controller.clone());
    let bytes = serde_json::to_vec(&plugin).unwrap();
    let plugin: crate::PluginError = serde_json::from_slice(&bytes).unwrap();
    let replayed = crate::RuntimeEffectControllerError::from(plugin);
    assert_eq!(controller.cause, replayed.cause);
    assert_eq!(controller.code, replayed.code);
}

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
        .resolve_creation_plugin_config(
            None,
            &options,
            None,
            true,
            &crate::store::plugin_writers::PluginAdmission::default(),
        )
        .unwrap_err();
    assert!(matches!(
        error,
        crate::plugin::CreationConfigError::Format(_)
    ));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(rmp_serde::to_vec_named(&config).unwrap(), config_bytes);
}

/// A factory whose declared [`crate::plugin::PluginDeclaration`] names
/// another plugin id than its own [`PluginFactory::id`](crate::PluginFactory::id).
#[derive(Clone)]
struct DeclaredAs {
    declared: &'static str,
    calls: Arc<std::sync::atomic::AtomicUsize>,
}

impl crate::PluginFactory for DeclaredAs {
    fn id(&self) -> &'static str {
        "registered"
    }

    fn declaration(&self) -> crate::plugin::PluginDeclaration {
        crate::plugin::PluginDeclaration::initial(self.declared)
    }

    fn build(
        &self,
        _: &crate::PluginSessionContext,
    ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Arc::new(self.clone()))
    }
}

impl crate::SessionPlugin for DeclaredAs {
    fn id(&self) -> &'static str {
        "registered"
    }

    fn register(&self, _: &mut crate::PluginRegistrar) -> Result<(), crate::PluginError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }

    fn session_ready(
        &self,
        _: crate::plugin::SessionReadyContext,
    ) -> Result<(), crate::PluginError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// A readable writer at its native format with no converter: it can admit
/// and write old stamps, but nothing migrates them to what it reads.
#[derive(Clone)]
struct NoMigrateProbe(Arc<std::sync::atomic::AtomicUsize>);

impl crate::PluginFactory for NoMigrateProbe {
    fn id(&self) -> &'static str {
        "no-migrate"
    }

    fn declaration(&self) -> crate::plugin::PluginDeclaration {
        let mut declaration = crate::plugin::PluginDeclaration::initial(self.id());
        declaration.format_version = crate::FormatVersion::new(2).unwrap();
        declaration.writable_formats = vec![crate::FormatVersion::ONE, declaration.format_version];
        declaration
    }

    fn build(
        &self,
        _: &crate::PluginSessionContext,
    ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(Arc::new(self.clone()))
    }
}

impl crate::SessionPlugin for NoMigrateProbe {
    fn id(&self) -> &'static str {
        "no-migrate"
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

/// The remaining negative legs of the materialization boundary (FIG-4859):
/// every corrupt or misowned record is refused by its own typed outcome
/// before a single factory build, registration or readiness callback runs.
#[test]
fn plugin_state_refusals_are_distinct_typed_and_pre_callback() {
    let calls = || Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let inactive =
        serde_json::json!({"generation": 7, "format_version": 99, "values": {"opaque": 5}});

    // A missing or zero format stamp never produces a `PluginState`: the
    // component codec refuses the body as corrupt durable data, before any
    // host exists to run callbacks.
    for body in [
        serde_json::json!({"format-probe": {"generation": 7, "values": {"native": 17}}}),
        serde_json::json!({"format-probe": {"generation": 7, "format_version": 0, "values": {"native": 17}}}),
    ] {
        let mut checkpoint = crate::HydratedSessionCheckpoint::default();
        checkpoint.components.insert(
            crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT.into(),
            crate::HydratedCheckpointComponent::changed(rmp_serde::to_vec_named(&body).unwrap()),
        );
        let error = checkpoint
            .decode_component::<PluginState>(crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
            .expect_err("a namespace without a readable stamp is corrupt");
        assert!(
            matches!(error, crate::StoreError::StoredDataCorrupt { .. }),
            "missing and zero stamps refuse typed: {error}"
        );
        assert_eq!(
            checkpoint.component_body(crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT),
            Some(&*rmp_serde::to_vec_named(&body).unwrap()),
            "the stored body is not rewritten on refusal"
        );
    }

    // A payload that deserializes but violates the store's invariants is
    // `PluginError::State` at any stamp the plugin admits — including its
    // native one, whose path ran no value validation before.
    let calls_probe = calls();
    let host = crate::PluginHost::new(vec![Arc::new(FormatProbe(calls_probe.clone()))]);
    for stamp in [1_u32, 2] {
        for malformed in [
            serde_json::json!({"bad/key": 1}),
            serde_json::json!({"wide": "x".repeat(VALUE_LIMIT + 1)}),
        ] {
            let snapshot: PluginState = serde_json::from_value(serde_json::json!({
                "format-probe": {"generation": 7, "format_version": stamp, "values": malformed},
                "inactive": inactive,
            }))
            .unwrap();
            let bytes = rmp_serde::to_vec_named(&snapshot).unwrap();
            let result = host.build_session(PluginSessionRequest::rematerialization(
                "malformed",
                &snapshot,
                Default::default(),
            ));
            assert!(
                matches!(result, Err(crate::PluginError::State(_))),
                "a malformed payload at stamp {stamp} refuses typed: {:?}",
                result.err()
            );
            assert_eq!(calls_probe.load(std::sync::atomic::Ordering::SeqCst), 0);
            assert_eq!(
                rmp_serde::to_vec_named(&snapshot).unwrap(),
                bytes,
                "the stored bytes are preserved"
            );
            assert_eq!(snapshot.plugins["inactive"].format_version.get(), 99);
        }
    }

    // A recorded stamp the plugin can write but has no converter for is
    // `PluginError::Format`: distinct from a too-new stamp only by
    // direction, and refused before the factory builds.
    let calls_no_migrate = calls();
    let host = crate::PluginHost::new(vec![Arc::new(NoMigrateProbe(calls_no_migrate.clone()))]);
    let snapshot: PluginState = serde_json::from_value(serde_json::json!({
        "no-migrate": {"generation": 7, "format_version": 1, "values": {"old": 17}},
        "inactive": inactive,
    }))
    .unwrap();
    let bytes = rmp_serde::to_vec_named(&snapshot).unwrap();
    let result = host.build_session(PluginSessionRequest::rematerialization(
        "unreadable-past",
        &snapshot,
        Default::default(),
    ));
    let Err(crate::PluginError::Format(refusal)) = result else {
        panic!("a missing converter is a typed format refusal");
    };
    assert_eq!(refusal.plugin, "no-migrate");
    assert_eq!(refusal.namespace, crate::FormatNamespace::State);
    assert_eq!(refusal.stored.get(), 1);
    assert_eq!(refusal.readable.get(), 2);
    let mut config = crate::PluginConfig::default();
    config.insert_versioned(
        "no-migrate",
        crate::FormatVersion::ONE,
        serde_json::json!({"old": 17}),
    );
    let config_bytes = rmp_serde::to_vec_named(&config).unwrap();
    let result = host.build_session(PluginSessionRequest::creation(
        "unreadable-config",
        crate::plugin::SessionAuthorityContext {
            plugin_config: crate::AdmittedPluginConfig::new(config.clone(), 3),
            ..Default::default()
        },
    ));
    assert!(
        matches!(
            result,
            Err(crate::PluginError::Format(crate::FormatRefusal {
                namespace: crate::FormatNamespace::Config,
                ..
            }))
        ),
        "config faces the same typed refusal: {:?}",
        result.err()
    );
    assert_eq!(
        calls_no_migrate.load(std::sync::atomic::Ordering::SeqCst),
        0
    );
    assert_eq!(rmp_serde::to_vec_named(&snapshot).unwrap(), bytes);
    assert_eq!(rmp_serde::to_vec_named(&config).unwrap(), config_bytes);

    // A factory whose declaration names another owner is `PluginError::
    // Declaration` before its build, registration or readiness runs.
    let calls_owner = calls();
    let host = crate::PluginHost::new(vec![Arc::new(DeclaredAs {
        declared: "another",
        calls: calls_owner.clone(),
    })]);
    let snapshot: PluginState = serde_json::from_value(serde_json::json!({
        "registered": {"generation": 7, "format_version": 1, "values": {"value": 1}},
        "inactive": inactive,
    }))
    .unwrap();
    let bytes = rmp_serde::to_vec_named(&snapshot).unwrap();
    for request in [
        PluginSessionRequest::creation("wrong-owner-create", Default::default()),
        PluginSessionRequest::rematerialization(
            "wrong-owner-reopen",
            &snapshot,
            Default::default(),
        ),
    ] {
        let result = host.build_session(request);
        let Err(crate::PluginError::Declaration(
            crate::plugin::PluginDeclarationError::IdMismatch { factory, declared },
        )) = result
        else {
            panic!("a wrong plugin owner is a typed declaration refusal");
        };
        assert_eq!(factory, "registered");
        assert_eq!(declared, "another");
    }
    assert_eq!(calls_owner.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(rmp_serde::to_vec_named(&snapshot).unwrap(), bytes);
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
    state
        .state
        .lock_recover()
        .initialize(state.owner(), None)
        .unwrap();
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
        state
            .state
            .lock_recover()
            .initialize(state.owner(), None)
            .unwrap();
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
    registry
        .lock_recover()
        .initialize(state.owner(), Some(&recorded))
        .unwrap();
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
    registry
        .lock_recover()
        .initialize(state.owner(), Some(&snapshot))
        .unwrap();
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
    registry
        .lock_recover()
        .initialize(state.owner(), Some(snapshot))
        .unwrap();
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

/// A fleet record a law moves the way a finalize would.
struct FleetRecord(std::sync::Mutex<crate::store::plugin_writers::PluginWriterRanges>);

impl FleetRecord {
    fn permitting(min: u32, max: u32) -> Self {
        Self(std::sync::Mutex::new(Self::ranges(min, max)))
    }

    fn ranges(min: u32, max: u32) -> crate::store::plugin_writers::PluginWriterRanges {
        crate::store::plugin_writers::PluginWriterRanges::from_rows([(
            "format-probe".to_string(),
            i64::from(min),
            i64::from(max),
        )])
        .unwrap()
    }

    /// Move the probe's range, keeping every other plugin's.
    fn finalize(&self, min: u32, max: u32) {
        let mut ranges = self.0.lock_recover();
        let moved = Self::ranges(min, max)
            .iter()
            .map(|(plugin, range)| (plugin.to_string(), range))
            .collect();
        *ranges = ranges.clone().with(moved);
    }
}

impl crate::store::FleetFormatStore for FleetRecord {
    fn fleet_format(&self) -> crate::store::FleetFormat {
        crate::store::FleetFormat::current()
    }

    fn plugin_writers(&self) -> crate::store::PluginWriterRangesFuture<'_> {
        let ranges = self.0.lock_recover().clone();
        Box::pin(async move { Ok(ranges) })
    }

    fn provision_plugin_writers<'a>(
        &'a self,
        registrations: &'a [crate::store::plugin_writers::PluginWriterRegistration],
    ) -> crate::store::PluginWriterRangesFuture<'a> {
        let mut ranges = self.0.lock_recover();
        let provisioned = ranges.provisioned(registrations, false);
        *ranges = ranges.clone().with(provisioned);
        let ranges = ranges.clone();
        Box::pin(async move { Ok(ranges) })
    }
}

/// FIG-4747: a session writes plugin state in the format its admission
/// recorded. A finalize that widens the fleet's range changes what the next
/// admission chooses; a retry that adopts the recorded admission again
/// writes the bytes it wrote before.
#[tokio::test]
async fn a_session_writes_state_in_its_admissions_recorded_format_across_a_finalize() {
    let host = crate::PluginHost::new(vec![Arc::new(FormatProbe(Arc::new(
        std::sync::atomic::AtomicUsize::new(0),
    )))]);
    let fleet = FleetRecord::permitting(1, 1);
    let stored: PluginState = serde_json::from_value(serde_json::json!({
        "format-probe": {"generation": 7, "format_version": 1, "values": {"old": 17}}
    }))
    .unwrap();
    let session = host
        .isolated_registry()
        .build_session(PluginSessionRequest::rematerialization(
            "admitted",
            &stored,
            Default::default(),
        ))
        .unwrap();
    let native = session.export_state();
    assert_eq!(native.plugins["format-probe"].format_version.get(), 2);
    // No admission adopted: the native format, as before any Run.
    assert_eq!(session.committed_state().unwrap(), native);

    // Admitted inside the window: the fleet permits format 1 only.
    let recorded = host.admit_plugins(&fleet).await.unwrap();
    // The record is the whole composition in hook order, the host's own
    // plugins first, each with its revision and chosen writer.
    assert_eq!(
        recorded
            .plugins()
            .iter()
            .map(|plugin| plugin.plugin.as_str())
            .collect::<Vec<_>>(),
        host.factories()
            .iter()
            .map(|factory| factory.id())
            .collect::<Vec<_>>()
    );
    let probe = recorded.plugins().last().unwrap();
    assert_eq!(
        (
            probe.plugin.as_str(),
            probe.behavior_revision.get(),
            probe.writer.get()
        ),
        ("format-probe", 1, 1)
    );
    session.adopt_plugin_admission(recorded.clone());
    let before = session.committed_state().unwrap();
    assert_eq!(before.plugins["format-probe"].format_version.get(), 1);
    assert_eq!(
        before.plugins["format-probe"].values,
        BTreeMap::from([("old".to_string(), serde_json::json!(17))])
    );
    // The committed form is the live state: adopting it back changes nothing.
    session.require_hydrated_state(&before).unwrap();
    session.hydrate_state(&before).unwrap();
    assert_eq!(session.export_state(), native);

    // Finalize widens the range. A new admission chooses the native format.
    fleet.finalize(1, 2);
    let after = host.admit_plugins(&fleet).await.unwrap();
    assert_eq!(after.writer("format-probe").unwrap().get(), 2);

    // A retry of the admitted work adopts its record again and writes the
    // same bytes, whatever the fleet permits now.
    session.adopt_plugin_admission(recorded);
    assert_eq!(
        rmp_serde::to_vec_named(&session.committed_state().unwrap()).unwrap(),
        rmp_serde::to_vec_named(&before).unwrap()
    );

    // Work admitted after the finalize writes the native format.
    session.adopt_plugin_admission(after);
    assert_eq!(session.committed_state().unwrap(), native);

    // A plugin that writes nothing the fleet permits is not admitted.
    fleet.finalize(3, 3);
    let refused = host.admit_plugins(&fleet).await.unwrap_err();
    assert!(
        matches!(
            refused,
            crate::StoreError::Incompatible {
                refusal: crate::compat::CompatRefusal::PluginWriterUnwritable { ref plugin, .. }
            } if plugin == "format-probe"
        ),
        "{refused:?}"
    );
}

#[tokio::test]
async fn recorded_effect_state_is_atomic_owner_checked_and_idempotent() {
    let host = crate::PluginHost::empty();
    let session = host
        .isolated_registry()
        .build_session(PluginSessionRequest::creation(
            "effect-owner",
            Default::default(),
        ))
        .unwrap();
    let first = PluginStateStore::bind(&session.owner, "first", Arc::clone(&session.state));
    let second = PluginStateStore::bind(&session.owner, "second", Arc::clone(&session.state));
    let base = session.export_state();
    let effect = super::effect::record_effect(
        Arc::clone(&session),
        crate::RuntimeEffectKind::LanguageRuntimeValue,
        crate::EffectAddress::new(
            crate::ExecutionScope::turn("effect-owner", "run"),
            "state-effect",
        )
        .unwrap(),
        async {
            first.set("a", serde_json::json!(1)).unwrap();
            first.set("b", serde_json::json!(2)).unwrap();
            second.set("c", serde_json::json!(3)).unwrap();
            assert!(first.set("invalid key", Value::Null).is_err());
            Ok(crate::RuntimeEffectOutcome::LanguageRuntimeValue { value: Value::Null })
        },
    )
    .await
    .unwrap();
    let after = session.export_state();
    let bytes = rmp_serde::to_vec_named(&effect).unwrap();
    let effect: crate::RuntimeEffectOutcome = rmp_serde::from_slice(&bytes).unwrap();
    let cold = host
        .isolated_registry()
        .build_session(PluginSessionRequest::rematerialization(
            "effect-owner",
            &base,
            Default::default(),
        ))
        .unwrap();
    cold.restore_effect_state(effect.clone()).unwrap();
    assert_eq!(cold.export_state(), after);
    cold.restore_effect_state(effect.clone()).unwrap();
    assert_eq!(
        cold.export_state(),
        after,
        "duplicate delivery advances nothing"
    );

    let later = PluginStateStore::bind(&cold.owner, "first", Arc::clone(&cold.state));
    later.set("later", Value::Bool(true)).unwrap();
    let with_later = cold.export_state();
    cold.restore_effect_state(effect.clone()).unwrap();
    assert_eq!(
        cold.export_state(),
        with_later,
        "duplicate delivery preserves subsequent writes"
    );

    let other = host
        .isolated_registry()
        .build_session(PluginSessionRequest::rematerialization(
            "another-owner",
            &base,
            Default::default(),
        ))
        .unwrap();
    let refusal = other.restore_effect_state(effect.clone()).unwrap_err();
    assert_eq!(
        refusal.cause,
        Some(crate::RuntimeErrorCause::PluginStateEffectOwnerMismatch)
    );
    assert_eq!(other.export_state(), base);

    let divergent = host
        .isolated_registry()
        .build_session(PluginSessionRequest::rematerialization(
            "effect-owner",
            &base,
            Default::default(),
        ))
        .unwrap();
    PluginStateStore::bind(&divergent.owner, "second", Arc::clone(&divergent.state))
        .set("other", Value::Bool(true))
        .unwrap();
    let before_refusal = divergent.export_state();
    let refusal = divergent.restore_effect_state(effect).unwrap_err();
    assert_eq!(
        refusal.cause,
        Some(crate::RuntimeErrorCause::PluginStateEffectReplayMismatch {
            plugin: "second".into(),
        })
    );
    assert_eq!(
        divergent.export_state(),
        before_refusal,
        "first namespace must not publish alone"
    );
    let wire: crate::RuntimeError = rmp_serde::from_slice(
        &rmp_serde::to_vec_named(&refusal.clone().into_runtime_error()).unwrap(),
    )
    .unwrap();
    assert_eq!(wire.cause, refusal.cause);
}

#[tokio::test]
async fn recorded_effect_state_preserves_terminal_failure_and_excludes_nested_batches() {
    let host = crate::PluginHost::empty();
    let session = host
        .isolated_registry()
        .build_session(PluginSessionRequest::creation(
            "effect-owner",
            Default::default(),
        ))
        .unwrap();
    let store = PluginStateStore::bind(&session.owner, "state", Arc::clone(&session.state));
    let base = session.export_state();
    let terminal =
        crate::RuntimeEffectControllerError::new(crate::RuntimeErrorCode::Plugin, "refused");
    let effect = super::effect::record_effect(
        Arc::clone(&session),
        crate::RuntimeEffectKind::LanguageRuntimeValue,
        crate::EffectAddress::new(
            crate::ExecutionScope::turn("effect-owner", "run"),
            "terminal",
        )
        .unwrap(),
        async {
            store.set("accepted", Value::Bool(true)).unwrap();
            Err(terminal.clone())
        },
    )
    .await
    .unwrap();
    let after = session.export_state();
    session.hydrate_state(&base).unwrap();
    let result = session.restore_effect_state(effect).unwrap_err();
    assert_eq!(result.code, terminal.code);
    assert_eq!(result.message, terminal.message);
    assert_eq!(session.export_state(), after);
    let outer = super::effect::record_effect(
        Arc::clone(&session),
        crate::RuntimeEffectKind::LanguageRuntimeValue,
        crate::EffectAddress::new(crate::ExecutionScope::turn("effect-owner", "run"), "outer")
            .unwrap(),
        async {
            let inner = super::effect::record_effect(
                Arc::clone(&session),
                crate::RuntimeEffectKind::LanguageRuntimeValue,
                crate::EffectAddress::new(
                    crate::ExecutionScope::turn("effect-owner", "run"),
                    "inner",
                )
                .unwrap(),
                async {
                    store.set("nested", Value::Bool(true)).unwrap();
                    Ok(crate::RuntimeEffectOutcome::LanguageRuntimeValue { value: Value::Null })
                },
            )
            .await
            .unwrap();
            session.restore_effect_state(inner).unwrap();
            Ok(crate::RuntimeEffectOutcome::LanguageRuntimeValue { value: Value::Null })
        },
    )
    .await
    .unwrap();
    assert!(matches!(
        outer,
        crate::RuntimeEffectOutcome::LanguageRuntimeValue { .. }
    ));
}

#[test]
fn recorded_transition_keeps_typed_refusal_and_publishes_neither_namespace() {
    #[derive(Clone)]
    struct Converter {
        id: &'static str,
        refuse: bool,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }
    impl crate::PluginFactory for Converter {
        fn id(&self) -> &'static str {
            self.id
        }
        fn declaration(&self) -> crate::plugin::PluginDeclaration {
            let mut declaration = crate::plugin::PluginDeclaration::initial(self.id);
            declaration.format_version = crate::FormatVersion::new(2).unwrap();
            declaration.writable_formats = vec![declaration.format_version];
            declaration
        }
        fn migrate_format(
            &self,
            from: crate::FormatVersion,
            namespace: crate::FormatNamespace,
            mut value: Value,
        ) -> Result<Value, crate::FormatRefusal> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.refuse {
                return Err(crate::FormatRefusal {
                    plugin: self.id.into(),
                    namespace,
                    stored: from,
                    readable: self.declaration().format_version,
                });
            }
            value
                .as_object_mut()
                .unwrap()
                .insert("converted".into(), Value::Bool(true));
            Ok(value)
        }
        fn build(
            &self,
            _: &crate::PluginSessionContext,
        ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
            panic!("transition cannot materialize a factory")
        }
    }
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host = crate::PluginHost::new(vec![
        Arc::new(Converter {
            id: "first",
            refuse: false,
            calls: Arc::clone(&calls),
        }),
        Arc::new(Converter {
            id: "second",
            refuse: true,
            calls: Arc::clone(&calls),
        }),
    ]);
    let base = PluginState {
        plugins: ["first", "second", "inactive"]
            .into_iter()
            .map(|id| (id.into(), PluginNamespaceState::default()))
            .collect(),
    };
    let request = crate::plugin::PluginTransitionRequest {
        id: crate::plugin::PluginTransitionId(
            crate::EffectAddress::new(
                crate::ExecutionScope::turn("transition-owner", "run"),
                "plugin-transition",
            )
            .unwrap(),
        ),
        owner: crate::RuntimeOwner::Session("transition-owner".into()),
        base: crate::store::SessionHeadRef {
            generation: 0,
            revision: 7,
            leaf: None,
            checkpoint: Some(super::state_ref(&base)),
        },
        target: crate::store::plugin_writers::PluginAdmission::from_plugins(
            host.factories()
                .iter()
                .map(|factory| crate::store::plugin_writers::AdmittedPlugin {
                    plugin: factory.id().into(),
                    behavior_revision: factory.declaration().behavior_revision,
                    writer: factory.declaration().format_version,
                })
                .collect(),
        ),
    };
    let record = host.transition_plugins(request, &base, &crate::PluginConfig::default());
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(
        record.namespaces["first"]
            .as_ref()
            .unwrap()
            .values
            .contains_key("converted")
    );
    assert_eq!(
        record.namespaces["inactive"].as_ref().unwrap(),
        &base.plugins["inactive"]
    );
    let record: crate::plugin::PluginTransitionRecord =
        rmp_serde::from_slice(&rmp_serde::to_vec_named(&record).unwrap()).unwrap();
    assert!(
        matches!(record.candidate(), Err(crate::PluginError::Format(ref refusal)) if refusal.plugin == "second")
    );
    let session = crate::PluginHost::empty()
        .build_session(PluginSessionRequest::rematerialization(
            "transition-owner",
            &base,
            Default::default(),
        ))
        .unwrap();
    let before = session.export_state();
    assert!(session.adopt_plugin_transition(&record).is_err());
    assert_eq!(session.export_state(), before);
    assert!(session.plugin_admission().is_none());
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "replaying refusal calls no converter"
    );
}

#[tokio::test]
async fn abandoned_effect_state_drops_accepted_tail_and_invalidates_guards() {
    let session = crate::PluginHost::empty()
        .build_session(PluginSessionRequest::creation(
            "effect-owner",
            Default::default(),
        ))
        .unwrap();
    let store = PluginStateStore::bind(&session.owner, "state", Arc::clone(&session.state));
    let base = session.export_state();
    let address = || {
        crate::EffectAddress::new(
            crate::ExecutionScope::turn("effect-owner", "run"),
            "abandoned",
        )
        .unwrap()
    };
    let result = super::effect::record_effect(
        Arc::clone(&session),
        crate::RuntimeEffectKind::LanguageRuntimeValue,
        address(),
        async {
            store.set("tail", Value::Bool(true)).unwrap();
            Err(crate::RuntimeEffectControllerError::new(
                crate::RuntimeErrorCode::PluginSessionManager,
                "lost owner",
            )
            .retryable_uncommitted_derivation())
        },
    )
    .await;
    assert!(result.is_err());
    assert_eq!(session.export_state(), base);
    assert!(store.generation() > 0);
    let (written, receiver) = tokio::sync::oneshot::channel();
    let mut body = Box::pin(super::effect::record_effect(
        Arc::clone(&session),
        crate::RuntimeEffectKind::LanguageRuntimeValue,
        address(),
        async {
            store.set("tail", Value::Bool(true)).unwrap();
            written.send(()).unwrap();
            std::future::pending().await
        },
    ));
    tokio::select! { _ = &mut body => panic!("body must suspend"), _ = receiver => {} }
    drop(body);
    assert_eq!(session.export_state(), base);
    assert!(matches!(
        store.apply_guarded(0, vec![]),
        Err(PluginStateError::GenerationConflict { .. })
    ));
}
