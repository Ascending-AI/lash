use super::*;
use pretty_assertions::assert_eq;

#[derive(Clone, Copy, Default)]
pub(super) enum Registration {
    #[default]
    None,
    Remove,
    Admission,
}

#[derive(Clone, Default)]
pub(super) struct MockPlugin {
    pub(super) writes_on_ready: bool,
    pub(super) registration: Registration,
    pub(super) ready_values:
        Arc<Mutex<std::collections::BTreeMap<String, Option<serde_json::Value>>>>,
    pub(super) handles: Arc<Mutex<std::collections::BTreeMap<String, PluginStateStore>>>,
}
impl PluginFactory for MockPlugin {
    fn id(&self) -> &'static str {
        "mock-state"
    }

    fn declaration(&self) -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial(PluginFactory::id(self))
    }
    fn initialize_state(
        &self,
        _: &crate::RuntimeOwner,
        _: &crate::PluginConfig,
    ) -> Result<std::collections::BTreeMap<String, serde_json::Value>, PluginError> {
        Ok(if self.writes_on_ready {
            std::collections::BTreeMap::from([("ready".into(), serde_json::json!(true))])
        } else {
            Default::default()
        })
    }
    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(self.clone()))
    }
}
impl SessionPlugin for MockPlugin {
    fn id(&self) -> &'static str {
        "mock-state"
    }
    fn register(&self, registrar: &mut PluginRegistrar) -> Result<(), PluginError> {
        let state = registrar.state();
        if !matches!(self.registration, Registration::None) {
            assert!(matches!(
                state.remove("counter"),
                Err(PluginStateError::WriteScopeRequired { .. })
            ));
            assert!(matches!(
                state.set("accepted", serde_json::json!(true)),
                Err(PluginStateError::WriteScopeRequired { .. })
            ));
            assert_eq!(state.generation(), 5, "registration is read-only");
        }
        self.handles
            .lock_recover()
            .insert(owner_key(state.owner()), state.clone());
        if self.writes_on_ready {
            registrar.turn().before(
                crate::hook_key!("failing-writer"),
                Arc::new(move |_| {
                    let state = state.clone();
                    Box::pin(async move {
                        state.set("failed-hook", serde_json::json!(true))?;
                        Err(PluginError::Session(
                            "deliberate hook failure after accepted write".into(),
                        ))
                    })
                }),
            )?;
        }
        Ok(())
    }
    fn session_ready(&self, context: SessionReadyContext) -> Result<(), PluginError> {
        self.ready_values
            .lock_recover()
            .insert(owner_key(&context.owner), context.state.get("counter"));
        let registered = self.handles.lock_recover()[&owner_key(&context.owner)].clone();
        assert_eq!(registered.generation(), context.state.generation());
        assert_eq!(
            registered.get("counter"),
            context.state.get("counter"),
            "ready must observe hydrated state through the captured registrar handle"
        );
        if self.writes_on_ready {
            assert!(matches!(
                context.state.set("ready", serde_json::json!(true)),
                Err(PluginStateError::WriteScopeRequired { .. })
            ));
            assert_eq!(context.state.get("ready"), Some(serde_json::json!(true)));
        }
        Ok(())
    }
}
impl MockPlugin {
    pub(super) fn host(&self) -> crate::PluginHost {
        let mut factories = crate::testing::test_standard_protocol_factories();
        factories.push(Arc::new(self.clone()));
        crate::PluginHost::new(factories)
    }
    pub(super) fn state(&self, id: &str) -> PluginStateStore {
        self.handles.lock_recover()[id].clone()
    }
}

/// The fixture's key for a plugin session: the session id the laws look it up
/// by, or the owner's own spelling for a process.
pub(super) fn owner_key(owner: &crate::RuntimeOwner) -> String {
    match owner {
        crate::RuntimeOwner::Session(session_id) => session_id.to_string(),
        crate::RuntimeOwner::Process(_) => owner.to_string(),
    }
}
