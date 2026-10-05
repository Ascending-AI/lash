use super::*;
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
        crate::plugin::PluginSpec::new().with_before_turn(
            crate::hook_key!("probe"),
            Arc::new(move |_| {
                counted_callbacks.fetch_add(1, Ordering::SeqCst);
                let effects = counted_effects.clone();
                Box::pin(async move {
                    effects.fetch_add(1, Ordering::SeqCst);
                    Ok(Default::default())
                })
            }),
        ),
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
    let identity = &session.capabilities().contributions.before_turn_hooks[0].identity;
    assert_eq!(identity.key, "before_turn:probe");
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
        &rebuilt.capabilities().contributions.before_turn_hooks[0].identity
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
        "format-probe": {"generation": 7, "format_version": 4294967295_u32, "publication": {"applied": null, "owner_segment": 0, "receipts": {}}, "values": {"old": 17}}
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
    let inactive = serde_json::json!({"generation": 7, "format_version": 99, "publication": {"applied": null, "owner_segment": 0, "receipts": {}}, "values": {"opaque": 5}});

    // A missing or zero format stamp never produces a `PluginState`: the
    // component codec refuses the body as corrupt durable data, before any
    // host exists to run callbacks.
    for body in [
        serde_json::json!({"format-probe": {"generation": 7, "values": {"native": 17}}}),
        serde_json::json!({"format-probe": {"generation": 7, "format_version": 0, "publication": {"applied": null, "owner_segment": 0, "receipts": {}}, "values": {"native": 17}}}),
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
            serde_json::json!({"wide": "x".repeat(lash_core_store::plugin_state::PLUGIN_STATE_VALUE_LIMIT + 1)}),
        ] {
            let snapshot: PluginState = serde_json::from_value(serde_json::json!({
                "format-probe": {"generation": 7, "format_version": stamp, "publication": {"applied": null, "owner_segment": 0, "receipts": {}}, "values": malformed},
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
        "no-migrate": {"generation": 7, "format_version": 1, "publication": {"applied": null, "owner_segment": 0, "receipts": {}}, "values": {"old": 17}},
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
        "registered": {"generation": 7, "format_version": 1, "publication": {"applied": null, "owner_segment": 0, "receipts": {}}, "values": {"value": 1}},
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
fn plugin_formats_convert_only_in_recorded_transition() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host = crate::PluginHost::new(vec![Arc::new(FormatProbe(calls))]);
    let snapshot: PluginState = serde_json::from_value(serde_json::json!({
        "format-probe": {"generation": 7, "format_version": 1, "publication": {"applied": null, "owner_segment": 0, "receipts": {}}, "values": {"old": 17}}
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
            .defer_session(PluginSessionRequest::rematerialization(
                owner,
                &snapshot,
                crate::plugin::SessionAuthorityContext {
                    plugin_config: crate::AdmittedPluginConfig::new(config.clone(), 3),
                    ..Default::default()
                },
            ))
            .unwrap();
        let request = transition_request(owner, &host);
        let record = host.transition_plugins(request, &snapshot, &config);
        session.adopt_plugin_transition(&record).unwrap();
        session.materialize().unwrap();
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
        let bytes = session
            .native_view(crate::FleetFormat::current())
            .unwrap()
            .unwrap();
        session
            .adopt_native_view(&bytes, crate::FleetFormat::current())
            .unwrap();
        assert_eq!(session.export_state(), decoded);
        results.push(rmp_serde::to_vec_named(&(decoded, decoded_config)).unwrap());
    }
    assert_eq!(results[0], results[1]);
    assert_eq!(rmp_serde::to_vec_named(&snapshot).unwrap(), before);
}

#[test]
fn plugin_formats_stamp_every_state_write() {
    let host = crate::PluginHost::new(vec![Arc::new(FormatProbe(Arc::new(
        std::sync::atomic::AtomicUsize::new(0),
    )))]);
    let native: PluginState = serde_json::from_value(serde_json::json!({
        "format-probe": {"generation": 8, "format_version": 2, "publication": {"applied": null, "owner_segment": 0, "receipts": {}}, "values": {"native": 18}},
        "inactive": {"generation": 7, "format_version": 99, "publication": {"applied": null, "owner_segment": 0, "receipts": {}}, "values": {"opaque": 5}}
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
                publication: Default::default(),
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
fn fork_preserves_absent_namespaces_and_canonical_order() {
    let mut values = BTreeMap::new();
    values.insert("v".into(), serde_json::json!({"nested": {"z": 1, "a": 2}}));
    let durable = PluginState {
        plugins: BTreeMap::from([(
            "absent-plugin".into(),
            PluginNamespaceState {
                format_version: lash_core_ids::FormatVersion::ONE,
                generation: 17,
                publication: Default::default(),
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
    let batch = |value| lash_core_store::tool_run::StateCommandBatch {
        plugin: crate::store::plugin_writers::PluginRevision::new(
            "canonical",
            crate::plugin::BehaviorRevision::ONE,
        ),
        origin: StateCommandOrigin::ToolAttempt {
            call_id: crate::ToolCallId::fixture("canonical"),
            attempt: lash_core_store::tool_run::AttemptOrdinal::FIRST,
        },
        commands: vec![StateCommand::Set {
            key: "v".into(),
            value,
        }],
    };
    let resolved = |value| {
        let StateResolutionOutcome::Applied { changes } =
            batch(value).reduce(&BTreeMap::new(), &mut |_, _, _, _| unreachable!())
        else {
            panic!("a valid set resolves");
        };
        rmp_serde::to_vec_named(&changes).unwrap()
    };
    assert_eq!(
        resolved(serde_json::json!({"z": {"b": 1, "a": 2}, "a": 3})),
        resolved(serde_json::json!({"a": 3, "z": {"a": 2, "b": 1}})),
        "a resolved value is canonical, whatever order its object keys were written in"
    );
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

#[tokio::test]
async fn a_session_writes_state_in_its_admissions_recorded_format_across_a_finalize() {
    let host = crate::PluginHost::new(vec![Arc::new(FormatProbe(Arc::new(
        std::sync::atomic::AtomicUsize::new(0),
    )))]);
    let fleet = FleetRecord::permitting(1, 1);
    let stored: PluginState = serde_json::from_value(serde_json::json!({
        "format-probe": {"generation": 7, "format_version": 1, "publication": {"applied": null, "owner_segment": 0, "receipts": {}}, "values": {"old": 17}}
    }))
    .unwrap();
    let session = host
        .isolated_registry()
        .defer_session(PluginSessionRequest::rematerialization(
            "admitted",
            &stored,
            Default::default(),
        ))
        .unwrap();
    let record = host.transition_plugins(
        transition_request("admitted", &host),
        &stored,
        &Default::default(),
    );
    session.adopt_plugin_transition(&record).unwrap();
    session.materialize().unwrap();
    let native = session.export_state();
    assert_eq!(native.plugins["format-probe"].format_version.get(), 2);
    // The initialization transition chooses the native formats before a fleet Run.
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
        base: crate::plugin::PluginTransitionBase::Session {
            head: crate::store::SessionHeadRef {
                generation: 0,
                revision: 7,
                leaf: None,
                checkpoint: Some(super::state_ref(&base)),
            },
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

fn transition_request(
    owner: &str,
    host: &crate::PluginHost,
) -> crate::plugin::PluginTransitionRequest {
    crate::plugin::PluginTransitionRequest {
        id: crate::plugin::PluginTransitionId(
            crate::EffectAddress::new(
                crate::ExecutionScope::turn(crate::SessionId::fixture(owner), "run"),
                "plugin-transition",
            )
            .unwrap(),
        ),
        owner: crate::RuntimeOwner::Session(crate::SessionId::fixture(owner)),
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
                    let declaration = factory.declaration();
                    crate::store::plugin_writers::AdmittedPlugin {
                        plugin: factory.id().into(),
                        behavior_revision: declaration.behavior_revision,
                        writer: declaration.format_version,
                    }
                })
                .collect(),
        ),
    }
}

/// L19: a transition's journaled outcome retains numeric publication receipt
/// keys through the engine's internally tagged JSON outcome decoder.
#[test]
fn a_journaled_transition_retains_the_completed_state_frontier() {
    let host = crate::PluginHost::empty();
    let mut namespace = PluginNamespaceState::default();
    namespace.publication.applied = Some(crate::tool_run::PublicationOrdinal(1));
    namespace.publication.receipts.insert(
        crate::tool_run::PublicationOrdinal(1),
        crate::BlobRef::for_content(b"completed resolution"),
    );
    let base = PluginState {
        plugins: [("retained".into(), namespace)].into(),
    };
    let record = host.transition_plugins(
        transition_request("frontier-reopen", &host),
        &base,
        &crate::PluginConfig::default(),
    );
    let outcome = crate::RuntimeEffectOutcome::TransitionPlugins {
        record: Box::new(record),
    };
    let journal = serde_json::to_value(&outcome).unwrap();
    let served: crate::RuntimeEffectOutcome = serde_json::from_value(journal.clone()).unwrap();
    assert_eq!(serde_json::to_value(&served).unwrap(), journal);
    let crate::RuntimeEffectOutcome::TransitionPlugins { record } = served else {
        panic!("transition outcome");
    };
    assert_eq!(
        record.candidate().unwrap().0.plugins["retained"],
        base.plugins["retained"]
    );
    let binary: crate::RuntimeEffectOutcome =
        rmp_serde::from_slice(&rmp_serde::to_vec_named(&outcome).unwrap()).unwrap();
    assert_eq!(serde_json::to_value(binary).unwrap(), journal);
}

#[tokio::test]
async fn pure_initialization_precedes_read_only_registration_and_readiness() {
    #[derive(Clone)]
    struct ReadOnly(Arc<std::sync::atomic::AtomicUsize>);
    impl crate::PluginFactory for ReadOnly {
        fn id(&self) -> &'static str {
            "read-only"
        }
        fn declaration(&self) -> crate::plugin::PluginDeclaration {
            crate::plugin::PluginDeclaration::initial(crate::PluginFactory::id(self))
        }
        fn initialize_state(
            &self,
            _: &crate::RuntimeOwner,
            _: &crate::PluginConfig,
        ) -> Result<BTreeMap<String, Value>, crate::PluginError> {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(BTreeMap::from([("initial".into(), serde_json::json!(17))]))
        }
        fn build(
            &self,
            _: &crate::PluginSessionContext,
        ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
            Ok(Arc::new(self.clone()))
        }
    }
    impl crate::SessionPlugin for ReadOnly {
        fn id(&self) -> &'static str {
            "read-only"
        }
        fn register(
            &self,
            registrar: &mut crate::PluginRegistrar,
        ) -> Result<(), crate::PluginError> {
            let state = registrar.state();
            assert_eq!(state.get("initial"), Some(serde_json::json!(17)));
            Ok(())
        }
        fn session_ready(
            &self,
            ctx: crate::plugin::SessionReadyContext,
        ) -> Result<(), crate::PluginError> {
            assert_eq!(ctx.state.get("initial"), Some(serde_json::json!(17)));
            Ok(())
        }
    }
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host = crate::PluginHost::new(vec![Arc::new(ReadOnly(calls.clone()))]);
    let request = transition_request("read-only-owner", &host);
    let record = host.transition_plugins(request, &Default::default(), &Default::default());
    let view = crate::plugin::PluginNativeView {
        generation: None,
        request: record.request.clone(),
        source: record.source.clone(),
        state: record.candidate().unwrap().0,
        config: record.candidate().unwrap().1,
    }
    .encode(crate::FleetFormat::current())
    .unwrap();
    for callback in ["initial-construction", "cold-construction"] {
        let session = host
            .isolated_registry()
            .defer_session(PluginSessionRequest::creation(
                "read-only-owner",
                Default::default(),
            ))
            .unwrap();
        session
            .adopt_native_view(&view, crate::FleetFormat::current())
            .unwrap();
        session.materialize().unwrap();
        assert_eq!(
            session.export_state().plugins["read-only"].generation,
            0,
            "{callback}: construction publishes nothing"
        );
    }
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}
