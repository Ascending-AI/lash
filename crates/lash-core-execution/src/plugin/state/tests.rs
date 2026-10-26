use super::*;
use crate::plugin::PluginSessionRequest;

/// PLUGIN-VIEW-CANNOT-CAPTURE-FOREIGN-NAMESPACE: every public capture keeps
/// the restriction of the host handed to a plugin, including trait forwards.
#[test]
fn plugin_view_cannot_capture_foreign_namespace() {
    use crate::plugin::ExecutionState;
    use crate::plugin::SessionAuthorityContext;
    use lash_core_store::session_state::SessionPluginStateSource;
    use lash_execution_contracts::plugin_error::PluginError;
    use lash_protocol_types::sync::MutexExt;

    #[derive(Clone)]
    struct Probe {
        id: &'static str,
        handles: Arc<std::sync::Mutex<BTreeMap<String, Arc<crate::PluginSession>>>>,
    }
    impl crate::plugin::PluginMetadata for Probe {
        fn plugin_declaration(&self) -> crate::plugin::PluginDeclaration {
            crate::plugin::PluginDeclaration::initial(self.id)
        }
    }
    impl crate::PluginFactory for Probe {
        fn id(&self) -> &'static str {
            self.id
        }
        fn initialize_state(
            &self,
            _: &lash_vocabulary::identity::RuntimeOwner,
            _: &crate::PluginConfig,
        ) -> Result<BTreeMap<String, Value>, PluginError> {
            Ok(BTreeMap::from([("secret".into(), Value::from(self.id))]))
        }
        fn recipe(
            self: Arc<Self>,
        ) -> std::result::Result<
            crate::plugin::PluginRecipe,
            lash_execution_contracts::plugin_error::PluginError,
        > {
            let mut reg = crate::plugin::PluginRegistrar::<Probe>::new(
                crate::plugin::PluginFactory::id(self.as_ref()),
            );
            Probe::declare(&mut reg)?;
            let id = self.id();
            Ok(crate::plugin::PluginRecipe::new(
                id,
                reg,
                move |_ctx, _state| Ok(Arc::new(self.as_ref().clone())),
            ))
        }
    }
    impl crate::SessionPlugin for Probe {
        fn id(&self) -> &'static str {
            self.id
        }

        fn session_ready(
            &self,
            ctx: crate::plugin::SessionReadyContext,
        ) -> Result<(), PluginError> {
            assert_eq!(ctx.state.get("secret"), Some(Value::from(self.id)));
            let session = ctx.host.session(ctx.owner.session_id().unwrap()).unwrap();
            self.handles.lock_recover().insert(self.id.into(), session);
            Ok(())
        }
    }
    impl Probe {
        fn declare(
            _reg: &mut crate::plugin::PluginRegistrar<Self>,
        ) -> std::result::Result<(), lash_execution_contracts::plugin_error::PluginError> {
            Ok(())
        }
    }
    let handles = Arc::new(std::sync::Mutex::new(BTreeMap::new()));
    let host = crate::PluginHost::new(
        ["plugin-a", "plugin-b"]
            .into_iter()
            .map(|id| {
                Arc::new(Probe {
                    id,
                    handles: handles.clone(),
                }) as Arc<dyn crate::PluginFactory>
            })
            .collect(),
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(Arc::new(crate::SystemClock)),
    );
    let record = host.transition_plugins(
        transition_request("capture-owner", &host),
        &Default::default(),
        &Default::default(),
    );
    let (state, config) = record.candidate().unwrap();
    let fleet = crate::FleetFormat::current();
    let bytes = crate::plugin::PluginNativeView {
        request: record.request,
        source: record.source,
        generation: None,
        state: state.clone(),
        config: config.clone(),
    }
    .encode(fleet)
    .unwrap();
    let session = host
        .defer_session(PluginSessionRequest::creation(
            "capture-owner",
            SessionAuthorityContext::fixture(),
        ))
        .unwrap();
    session.adopt_native_view(&bytes, fleet).unwrap();
    session.materialize().unwrap();
    let view = handles.lock_recover()["plugin-a"].clone();
    let address = crate::EffectAddress::new(
        crate::ExecutionScope::turn("capture-owner", "capture"),
        "state",
    )
    .unwrap();
    let staged =
        crate::plugin::EffectPublication::begin(session.clone(), address).stage(Vec::new());

    let mut leaks = Vec::new();
    for (route, result) in [
        ("export", Ok(view.export_state())),
        ("committed", view.committed_state()),
        ("staged", view.committed_state_with(&staged)),
    ] {
        match result {
            Ok(state) if state.plugins.contains_key("plugin-b") => leaks.push(route),
            Err(error) => assert!(matches!(error, PluginError::Session(_)), "{route}: {error}"),
            _ => {}
        }
    }
    for (route, namespaces) in [
        ("prompt", view.committed_namespaces()),
        (
            "execution-state",
            ExecutionState::committed_namespaces(view.as_ref()),
        ),
    ] {
        if namespaces.contains_key("plugin-b") {
            leaks.push(route);
        }
    }
    assert!(
        ExecutionState::owner_state(view.as_ref(), "plugin-b")
            .values
            .is_empty()
    );
    for (route, result) in [
        (
            "source-export",
            SessionPluginStateSource::export_plugin_state(view.as_ref()),
        ),
        (
            "source-capture",
            SessionPluginStateSource::capture_plugin_state(view.as_ref()),
        ),
        (
            "arc-source-export",
            SessionPluginStateSource::export_plugin_state(&view),
        ),
        (
            "arc-source-capture",
            SessionPluginStateSource::capture_plugin_state(&view),
        ),
    ] {
        if result.is_ok_and(|state| state.plugins.contains_key("plugin-b")) {
            leaks.push(route);
        }
    }
    for (route, result) in [
        (
            "native",
            view.native_view(fleet).map_err(|error| {
                crate::RuntimeEffectControllerError::from(error).into_runtime_error()
            }),
        ),
        (
            "source-native",
            SessionPluginStateSource::capture_plugin_admission(view.as_ref(), &config, fleet),
        ),
        (
            "arc-source-native",
            SessionPluginStateSource::capture_plugin_admission(&view, &config, fleet),
        ),
    ] {
        if let Ok(Some(bytes)) = result {
            let state = crate::plugin::PluginNativeView::decode(&bytes, fleet)
                .unwrap()
                .state;
            if state.plugins.contains_key("plugin-b") {
                leaks.push(route);
            }
        }
    }
    assert!(matches!(
        view.build_recorded_head(),
        Err(PluginError::Session(_))
    ));
    assert!(matches!(
        view.fork_for_session("foreign-capture", SessionAuthorityContext::fixture()),
        Err(PluginError::Session(_))
    ));
    let full = session.committed_state().unwrap();
    assert_eq!(full, state);
    assert_eq!(session.committed_state_with(&staged).unwrap(), state);
    assert!(session.committed_namespaces().contains_key("plugin-b"));
    assert_eq!(
        SessionPluginStateSource::capture_plugin_state(session.as_ref()).unwrap(),
        state
    );
    assert!(session.native_view(fleet).unwrap().is_some());
    let native = SessionPluginStateSource::capture_plugin_admission(&session, &config, fleet)
        .unwrap()
        .unwrap();
    assert_eq!(
        crate::plugin::PluginNativeView::decode(&native, fleet)
            .unwrap()
            .state,
        state
    );
    staged.discard();

    session.begin_run();
    let mut changed = state.clone();
    let foreign = changed.plugins.get_mut("plugin-b").unwrap();
    foreign.generation += 1;
    foreign.values = Arc::new(BTreeMap::from([(
        "secret".into(),
        Value::from("changed-b"),
    )]));
    session.hydrate_state(&changed).unwrap();
    let full_changes = session.run_changes();
    let foreign_change = full_changes
        .iter()
        .find(|change| change.plugin == "plugin-b")
        .unwrap();
    assert!(
        foreign_change.values.body().is_some(),
        "the runtime captures the changed values"
    );
    for (route, changes) in [
        ("run-changes", view.run_changes()),
        (
            "execution-run-changes",
            ExecutionState::run_changes(view.as_ref()),
        ),
    ] {
        if changes.iter().any(|change| change.plugin == "plugin-b") {
            leaks.push(route);
        }
    }
    assert!(
        leaks.is_empty(),
        "plugin A captured plugin B through {leaks:?}"
    );
}

#[derive(Clone)]
struct FormatProbe(Arc<std::sync::atomic::AtomicUsize>);

impl crate::PluginFactory for FormatProbe {
    fn id(&self) -> &'static str {
        "format-probe"
    }

    fn migrate_format(
        &self,
        from: lash_kernel_value::providers::FormatVersion,
        namespace: crate::FormatNamespace,
        mut value: Value,
    ) -> Result<Value, crate::FormatRefusal> {
        if from == lash_kernel_value::providers::FormatVersion::ONE {
            let value = value.as_object_mut().unwrap();
            if let Some(old) = value.remove("old") {
                value.insert("native".into(), old);
            }
            Ok(Value::Object(value.clone()))
        } else if from == crate::plugin::PluginMetadata::plugin_declaration(self).format_version {
            Ok(value)
        } else {
            Err(crate::FormatRefusal {
                plugin: "format-probe".into(),
                namespace,
                stored: from,
                readable: crate::plugin::PluginMetadata::plugin_declaration(self).format_version,
            })
        }
    }

    fn encode_format(
        &self,
        to: lash_kernel_value::providers::FormatVersion,
        namespace: crate::FormatNamespace,
        value: &Value,
    ) -> Result<Value, crate::FormatRefusal> {
        let mut value = value.clone();
        if to == lash_kernel_value::providers::FormatVersion::ONE {
            let object = value.as_object_mut().unwrap();
            if let Some(native) = object.remove("native") {
                object.insert("old".into(), native);
            }
            Ok(value)
        } else if to == crate::plugin::PluginMetadata::plugin_declaration(self).format_version {
            Ok(value)
        } else {
            Err(crate::FormatRefusal {
                plugin: "format-probe".into(),
                namespace,
                stored: to,
                readable: crate::plugin::PluginMetadata::plugin_declaration(self).format_version,
            })
        }
    }

    fn recipe(
        self: Arc<Self>,
    ) -> Result<crate::plugin::PluginRecipe, lash_execution_contracts::plugin_error::PluginError>
    {
        let id = crate::PluginFactory::id(self.as_ref());
        Ok(crate::plugin::PluginRecipe::new(
            id,
            crate::PluginRegistrar::<Self>::new(id),
            move |_, _| {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Arc::clone(&self))
            },
        ))
    }
}

impl FormatProbe {
    pub fn declaration() -> crate::plugin::PluginDeclaration {
        let mut declaration = crate::plugin::PluginDeclaration::initial("format-probe");
        declaration.format_version = lash_kernel_value::providers::FormatVersion::new(2).unwrap();
        declaration.writable_formats = vec![
            lash_kernel_value::providers::FormatVersion::ONE,
            declaration.format_version,
        ];
        declaration
    }
}
impl crate::plugin::PluginMetadata for FormatProbe {
    fn plugin_declaration(&self) -> crate::plugin::PluginDeclaration {
        Self::declaration()
    }
}

impl crate::SessionPlugin for FormatProbe {
    fn id(&self) -> &'static str {
        "format-probe"
    }

    fn session_ready(
        &self,
        _: crate::plugin::SessionReadyContext,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

#[test]
fn plugin_formats_refuse_before_callbacks_and_preserve_bytes() {
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host = crate::PluginHost::new(
        vec![Arc::new(FormatProbe(calls.clone()))],
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
    );
    let snapshot: PluginState = serde_json::from_value(serde_json::json!({
        "format-probe": {"generation": 7, "format_version": 4294967295_u32, "fork": "copy", "publication": {"owner_segment": 0, "settled": null, "recent": []}, "values": {"old": 17}}
    }))
    .unwrap();
    let bytes = rmp_serde::to_vec_named(&snapshot).unwrap();
    let result = host.build_session(PluginSessionRequest::rematerialization(
        "unreadable",
        &snapshot,
        crate::plugin::SessionAuthorityContext::fixture(),
    ));
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert!(
        result.is_err(),
        "an unreadable stamp must refuse materialization"
    );
    assert_eq!(rmp_serde::to_vec_named(&snapshot).unwrap(), bytes);
    let refusal = result.err().unwrap();
    let lash_execution_contracts::plugin_error::PluginError::Format(expected) = refusal else {
        panic!("typed format refusal");
    };
    assert_eq!(expected.stored.get(), u32::MAX);
    assert_eq!(expected.namespace, crate::FormatNamespace::State);
    let plugin: lash_execution_contracts::plugin_error::PluginError = rmp_serde::from_slice(
        &rmp_serde::to_vec_named(
            &lash_execution_contracts::plugin_error::PluginError::Format(expected.clone()),
        )
        .unwrap(),
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
        lash_execution_contracts::plugin_error::PluginError::Runtime(runtime)
            .into_turn_failure(crate::RuntimeErrorCode::Plugin)
            .cause,
        cause
    );
    assert_eq!(
        lash_execution_contracts::plugin_error::PluginError::RuntimeEffectController(controller)
            .into_turn_failure(crate::RuntimeErrorCode::Plugin)
            .cause,
        cause
    );

    let mut config = crate::PluginConfig::default();
    config.insert_versioned(
        "format-probe",
        lash_kernel_value::providers::FormatVersion::new(u32::MAX).unwrap(),
        serde_json::json!({"old": 17}),
    );
    let config_bytes = rmp_serde::to_vec_named(&config).unwrap();
    let result = host.build_session(PluginSessionRequest::creation(
        "bad-config",
        crate::plugin::SessionAuthorityContext {
            plugin_config: crate::AdmittedPluginConfig::new(config.clone(), 3),
        },
    ));
    assert!(matches!(
        result,
        Err(lash_execution_contracts::plugin_error::PluginError::Format(
            crate::FormatRefusal {
                namespace: crate::FormatNamespace::Config,
                ..
            }
        ))
    ));
    let options = crate::PluginOptions {
        plugins: config.namespaces().clone(),
    };
    let error = host
        .resolve_creation_plugin_config(
            None,
            &options,
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

    fn recipe(
        self: Arc<Self>,
    ) -> Result<crate::plugin::PluginRecipe, lash_execution_contracts::plugin_error::PluginError>
    {
        let id = crate::PluginFactory::id(self.as_ref());
        Ok(crate::plugin::PluginRecipe::new(
            id,
            crate::PluginRegistrar::<Self>::new(crate::plugin::PluginFactory::id(self.as_ref())),
            move |_, _state| {
                self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Arc::clone(&self))
            },
        ))
    }
}

impl crate::plugin::PluginMetadata for DeclaredAs {
    fn plugin_declaration(&self) -> crate::plugin::PluginDeclaration {
        crate::plugin::PluginDeclaration::initial(self.declared)
    }
}

impl crate::SessionPlugin for DeclaredAs {
    fn id(&self) -> &'static str {
        "registered"
    }

    fn session_ready(
        &self,
        _: crate::plugin::SessionReadyContext,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
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

    fn recipe(
        self: Arc<Self>,
    ) -> Result<crate::plugin::PluginRecipe, lash_execution_contracts::plugin_error::PluginError>
    {
        let id = crate::PluginFactory::id(self.as_ref());
        Ok(crate::plugin::PluginRecipe::new(
            id,
            crate::PluginRegistrar::<Self>::new(crate::plugin::PluginFactory::id(self.as_ref())),
            move |_, _state| {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(Arc::clone(&self))
            },
        ))
    }
}

impl NoMigrateProbe {
    pub fn declaration() -> crate::plugin::PluginDeclaration {
        let mut declaration = crate::plugin::PluginDeclaration::initial("no-migrate");
        declaration.format_version = lash_kernel_value::providers::FormatVersion::new(2).unwrap();
        declaration.writable_formats = vec![
            lash_kernel_value::providers::FormatVersion::ONE,
            declaration.format_version,
        ];
        declaration
    }
}
impl crate::plugin::PluginMetadata for NoMigrateProbe {
    fn plugin_declaration(&self) -> crate::plugin::PluginDeclaration {
        Self::declaration()
    }
}

impl crate::SessionPlugin for NoMigrateProbe {
    fn id(&self) -> &'static str {
        "no-migrate"
    }

    fn session_ready(
        &self,
        _: crate::plugin::SessionReadyContext,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Ok(())
    }
}

/// The remaining negative legs of the materialization boundary (FIG-4859):
/// every corrupt or misowned record is refused by its own typed outcome
/// before a single owner binder or readiness callback runs.
#[test]
fn plugin_state_refusals_are_distinct_typed_and_pre_callback() {
    let calls = || Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let inactive = serde_json::json!({"generation": 7, "format_version": 99, "fork": "copy", "publication": {"owner_segment": 0, "settled": null, "recent": []}, "values": {"opaque": 5}});

    // A missing or zero format stamp never produces a `PluginState`: the
    // component codec refuses the body as corrupt durable data, before any
    // host exists to run callbacks.
    for body in [
        serde_json::json!({"format-probe": {"generation": 7, "values": {"native": 17}}}),
        serde_json::json!({"format-probe": {"generation": 7, "format_version": 0, "fork": "copy", "publication": {"owner_segment": 0, "settled": null, "recent": []}, "values": {"native": 17}}}),
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
    let host = crate::PluginHost::new(
        vec![Arc::new(FormatProbe(calls_probe.clone()))],
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
    );
    for stamp in [1_u32, 2] {
        for malformed in [
            serde_json::json!({"bad/key": 1}),
            serde_json::json!({"wide": "x".repeat(lash_core_store::plugin_state::PLUGIN_STATE_VALUE_LIMIT + 1)}),
        ] {
            let snapshot: PluginState = serde_json::from_value(serde_json::json!({
                "format-probe": {"generation": 7, "format_version": stamp, "fork": "copy", "publication": {"owner_segment": 0, "settled": null, "recent": []}, "values": malformed},
                "inactive": inactive,
            }))
            .unwrap();
            let bytes = rmp_serde::to_vec_named(&snapshot).unwrap();
            let result = host.build_session(PluginSessionRequest::rematerialization(
                "malformed",
                &snapshot,
                crate::plugin::SessionAuthorityContext::fixture(),
            ));
            assert!(
                matches!(
                    result,
                    Err(lash_execution_contracts::plugin_error::PluginError::State(
                        _
                    ))
                ),
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
    let host = crate::PluginHost::new(
        vec![Arc::new(NoMigrateProbe(calls_no_migrate.clone()))],
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
    );
    let snapshot: PluginState = serde_json::from_value(serde_json::json!({
        "no-migrate": {"generation": 7, "format_version": 1, "fork": "copy", "publication": {"owner_segment": 0, "settled": null, "recent": []}, "values": {"old": 17}},
        "inactive": inactive,
    }))
    .unwrap();
    let bytes = rmp_serde::to_vec_named(&snapshot).unwrap();
    let result = host.build_session(PluginSessionRequest::rematerialization(
        "unreadable-past",
        &snapshot,
        crate::plugin::SessionAuthorityContext::fixture(),
    ));
    let Err(lash_execution_contracts::plugin_error::PluginError::Format(refusal)) = result else {
        panic!("a missing converter is a typed format refusal");
    };
    assert_eq!(refusal.plugin, "no-migrate");
    assert_eq!(refusal.namespace, crate::FormatNamespace::State);
    assert_eq!(refusal.stored.get(), 1);
    assert_eq!(refusal.readable.get(), 2);
    let mut config = crate::PluginConfig::default();
    config.insert_versioned(
        "no-migrate",
        lash_kernel_value::providers::FormatVersion::ONE,
        serde_json::json!({"old": 17}),
    );
    let config_bytes = rmp_serde::to_vec_named(&config).unwrap();
    let result = host.build_session(PluginSessionRequest::creation(
        "unreadable-config",
        crate::plugin::SessionAuthorityContext {
            plugin_config: crate::AdmittedPluginConfig::new(config.clone(), 3),
        },
    ));
    assert!(
        matches!(
            result,
            Err(lash_execution_contracts::plugin_error::PluginError::Format(
                crate::FormatRefusal {
                    namespace: crate::FormatNamespace::Config,
                    ..
                }
            ))
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
    let host = crate::PluginHost::new(
        vec![Arc::new(DeclaredAs {
            declared: "another",
            calls: calls_owner.clone(),
        })],
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
    );
    let snapshot: PluginState = serde_json::from_value(serde_json::json!({
        "registered": {"generation": 7, "format_version": 1, "fork": "copy", "publication": {"owner_segment": 0, "settled": null, "recent": []}, "values": {"value": 1}},
        "inactive": inactive,
    }))
    .unwrap();
    let bytes = rmp_serde::to_vec_named(&snapshot).unwrap();
    for request in [
        PluginSessionRequest::creation(
            "wrong-owner-create",
            crate::plugin::SessionAuthorityContext::fixture(),
        ),
        PluginSessionRequest::rematerialization(
            "wrong-owner-reopen",
            &snapshot,
            crate::plugin::SessionAuthorityContext::fixture(),
        ),
    ] {
        let result = host.build_session(request);
        let Err(lash_execution_contracts::plugin_error::PluginError::Declaration(
            lash_execution_contracts::declaration_error::PluginDeclarationError::IdMismatch {
                factory,
                declared,
            },
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
    let host = crate::PluginHost::new(
        vec![Arc::new(FormatProbe(calls))],
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
    );
    let snapshot: PluginState = serde_json::from_value(serde_json::json!({
        "format-probe": {"generation": 7, "format_version": 1, "fork": "copy", "publication": {"owner_segment": 0, "settled": null, "recent": []}, "values": {"old": 17}}
    }))
    .unwrap();
    let before = rmp_serde::to_vec_named(&snapshot).unwrap();
    let mut config = crate::PluginConfig::default();
    config.insert_versioned(
        "format-probe",
        lash_kernel_value::providers::FormatVersion::ONE,
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
    let host = crate::PluginHost::new(
        vec![Arc::new(FormatProbe(Arc::new(
            std::sync::atomic::AtomicUsize::new(0),
        )))],
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
    );
    let native: PluginState = serde_json::from_value(serde_json::json!({
        "format-probe": {"generation": 8, "format_version": 2, "fork": "copy", "publication": {"owner_segment": 0, "settled": null, "recent": []}, "values": {"native": 18}},
        "inactive": {"generation": 7, "format_version": 99, "fork": "copy", "publication": {"owner_segment": 0, "settled": null, "recent": []}, "values": {"opaque": 5}}
    }))
    .unwrap();
    let mut config = crate::PluginConfig::default();
    config.insert_versioned(
        "format-probe",
        lash_kernel_value::providers::FormatVersion::new(2).unwrap(),
        serde_json::json!({"native": 18}),
    );
    config.insert_versioned(
        "inactive",
        lash_kernel_value::providers::FormatVersion::new(99).unwrap(),
        serde_json::json!({"opaque": 5}),
    );
    for version in [1, 2] {
        let writer = lash_kernel_value::providers::FormatVersion::new(version).unwrap();
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
        let options = crate::PluginOptions {
            plugins: encoded_config.namespaces().clone(),
        };
        let options: crate::PluginOptions =
            rmp_serde::from_slice(&rmp_serde::to_vec_named(&options).unwrap()).unwrap();
        assert_eq!(options.plugins["format-probe"].format_version, writer);
    }
    assert!(
        host.encode_state(
            &native,
            &BTreeMap::from([(
                "format-probe".into(),
                lash_kernel_value::providers::FormatVersion::new(3).unwrap()
            )])
        )
        .is_err()
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
                format_version: lash_kernel_value::providers::FormatVersion::ONE,
                generation: 17,
                publication: Default::default(),
                fork: Default::default(),
                values: values.into(),
                encoding: Default::default(),
            },
        )]),
    };
    let host = crate::PluginHost::empty(
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
    );
    let parent = host
        .build_session(PluginSessionRequest::rematerialization(
            "parent",
            &durable,
            crate::plugin::SessionAuthorityContext::fixture(),
        ))
        .unwrap();
    let child = parent
        .fork_for_session("child", crate::plugin::SessionAuthorityContext::fixture())
        .unwrap();
    assert_eq!(child.export_state(), parent.export_state());
    assert_eq!(
        child.export_state().plugins["absent-plugin"],
        durable.plugins["absent-plugin"]
    );
    let batch = |value| lash_core_store::tool_run::StateCommandBatch {
        plugin: String::from("canonical"),
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
        let StateResolutionOutcome::Applied { changes } = batch(value).reduce(
            &BTreeMap::new(),
            &mut |_, _, _, _| unreachable!(),
            lash_core_store::tool_run::StateCommandLimits::PUBLISHED,
        ) else {
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
    let host = crate::PluginHost::new(
        vec![Arc::new(FormatProbe(Arc::new(
            std::sync::atomic::AtomicUsize::new(0),
        )))],
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
    );
    let fleet = FleetRecord::permitting(1, 1);
    let stored: PluginState = serde_json::from_value(serde_json::json!({
        "format-probe": {"generation": 7, "format_version": 1, "fork": "copy", "publication": {"owner_segment": 0, "settled": null, "recent": []}, "values": {"old": 17}}
    }))
    .unwrap();
    let session = host
        .isolated_registry()
        .defer_session(PluginSessionRequest::rematerialization(
            "admitted",
            &stored,
            crate::plugin::SessionAuthorityContext::fixture(),
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
    // plugins first, each with its chosen writer.
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
        (probe.plugin.as_str(), probe.writer.get()),
        ("format-probe", 1)
    );
    session.adopt_plugin_admission(recorded.clone());
    let before = session.committed_state().unwrap();
    assert_eq!(before.plugins["format-probe"].format_version.get(), 1);
    assert_eq!(
        *before.plugins["format-probe"].values,
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

        fn migrate_format(
            &self,
            from: lash_kernel_value::providers::FormatVersion,
            namespace: crate::FormatNamespace,
            mut value: Value,
        ) -> Result<Value, crate::FormatRefusal> {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            if self.refuse {
                return Err(crate::FormatRefusal {
                    plugin: self.id.into(),
                    namespace,
                    stored: from,
                    readable: crate::plugin::PluginMetadata::plugin_declaration(self)
                        .format_version,
                });
            }
            value
                .as_object_mut()
                .unwrap()
                .insert("converted".into(), Value::Bool(true));
            Ok(value)
        }
        fn recipe(
            self: Arc<Self>,
        ) -> std::result::Result<
            crate::plugin::PluginRecipe,
            lash_execution_contracts::plugin_error::PluginError,
        > {
            let id = self.id();
            Ok(crate::plugin::PluginRecipe::empty(
                id,
                move |_ctx, _state| panic!("transition cannot materialize a factory"),
            ))
        }
    }

    impl crate::plugin::PluginMetadata for Converter {
        fn plugin_declaration(&self) -> crate::plugin::PluginDeclaration {
            let mut declaration = crate::plugin::PluginDeclaration::initial(self.id);
            declaration.format_version =
                lash_kernel_value::providers::FormatVersion::new(2).unwrap();
            declaration.writable_formats = vec![declaration.format_version];
            declaration
        }
    }
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host = crate::PluginHost::new(
        vec![
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
        ],
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
    );
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
        owner: lash_vocabulary::identity::RuntimeOwner::Session("transition-owner".into()),
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
                    writer: factory.plugin_declaration().format_version,
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
        matches!(record.candidate(), Err(lash_execution_contracts::plugin_error::PluginError::Format(ref refusal)) if refusal.plugin == "second")
    );
    let session = crate::PluginHost::empty(
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
    )
    .build_session(PluginSessionRequest::rematerialization(
        "transition-owner",
        &base,
        crate::plugin::SessionAuthorityContext::fixture(),
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
                crate::ExecutionScope::turn(
                    lash_kernel_value::identity::SessionId::fixture(owner),
                    "run",
                ),
                "plugin-transition",
            )
            .unwrap(),
        ),
        owner: lash_vocabulary::identity::RuntimeOwner::Session(
            lash_kernel_value::identity::SessionId::fixture(owner),
        ),
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
    }
}

#[tokio::test]
async fn pure_initialization_precedes_read_only_binding_and_readiness() {
    #[derive(Clone)]
    struct ReadOnly(Arc<std::sync::atomic::AtomicUsize>);
    impl crate::PluginFactory for ReadOnly {
        fn id(&self) -> &'static str {
            "read-only"
        }

        fn initialize_state(
            &self,
            _: &lash_vocabulary::identity::RuntimeOwner,
            _: &crate::PluginConfig,
        ) -> Result<BTreeMap<String, Value>, lash_execution_contracts::plugin_error::PluginError>
        {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(BTreeMap::from([("initial".into(), serde_json::json!(17))]))
        }
        fn recipe(
            self: Arc<Self>,
        ) -> Result<crate::plugin::PluginRecipe, lash_execution_contracts::plugin_error::PluginError>
        {
            let id = crate::PluginFactory::id(self.as_ref());
            Ok(crate::plugin::PluginRecipe::new(
                id,
                crate::PluginRegistrar::<Self>::new(crate::plugin::PluginFactory::id(
                    self.as_ref(),
                )),
                move |_, state| {
                    assert_eq!(state.get("initial"), Some(serde_json::json!(17)));
                    Ok(Arc::clone(&self))
                },
            ))
        }
    }

    impl ReadOnly {
        pub fn declaration() -> crate::plugin::PluginDeclaration {
            crate::plugin::PluginDeclaration::initial("read-only")
        }
    }
    impl crate::plugin::PluginMetadata for ReadOnly {
        fn plugin_declaration(&self) -> crate::plugin::PluginDeclaration {
            Self::declaration()
        }
    }
    impl crate::SessionPlugin for ReadOnly {
        fn id(&self) -> &'static str {
            "read-only"
        }

        fn session_ready(
            &self,
            ctx: crate::plugin::SessionReadyContext,
        ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
            assert_eq!(ctx.state.get("initial"), Some(serde_json::json!(17)));
            Ok(())
        }
    }
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let host = crate::PluginHost::new(
        vec![Arc::new(ReadOnly(calls.clone()))],
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(std::sync::Arc::new(crate::SystemClock)),
    );
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
                crate::plugin::SessionAuthorityContext::fixture(),
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
