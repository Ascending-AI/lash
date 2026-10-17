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
    pub(super) writes_on_before: bool,
    pub(super) registration: Registration,
    pub(super) ready_values:
        Arc<Mutex<std::collections::BTreeMap<String, Option<serde_json::Value>>>>,
    pub(super) handles: Arc<Mutex<std::collections::BTreeMap<String, PluginStateView>>>,
}
impl PluginFactory for MockPlugin {
    fn id(&self) -> &'static str {
        "mock-state"
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
    fn recipe(
        self: Arc<Self>,
    ) -> std::result::Result<
        lash_core::plugin::PluginRecipe,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        let mut reg = lash_core::plugin::PluginRegistrar::<Self>::new(
            lash_core::plugin::PluginFactory::id(self.as_ref()),
        );
        if self.writes_on_ready {
            reg.turn()
                .before(crate::hook_key!("failing-writer"), |_, _| {
                    Ok(Arc::new(move |_| {
                        Box::pin(async move {
                            Err(
                                lash_execution_contracts::plugin_error::PluginError::Session(
                                    "deliberate hook failure before any command".into(),
                                ),
                            )
                        })
                    }))
                })?;
        }
        if self.writes_on_before {
            reg.turn()
                .before(crate::hook_key!("ingress-writer"), |_, _| {
                    Ok(Arc::new(|_| {
                        Box::pin(async {
                            Ok(crate::plugin::TurnContributions {
                                state: StateCommands::new().set("counter", serde_json::json!(17)),
                                ..Default::default()
                            })
                        })
                    }))
                })?;
        }
        let id = lash_core::plugin::PluginFactory::id(self.as_ref());
        Ok(lash_core::plugin::PluginRecipe::new(
            id,
            reg,
            move |_, state| {
                let state = state.clone();
                if !matches!(self.registration, Registration::None) {
                    assert_eq!(state.generation(), 5, "registration reads published state");
                }
                self.handles.lock_recover().insert(
                    owner_key(state.owner().ok_or_else(|| {
                        PluginError::Registration(
                            "the fixture requires a materialized owner".into(),
                        )
                    })?),
                    state,
                );

                Ok(Arc::new(self.as_ref().clone()))
            },
        ))
    }
}

impl MockPlugin {
    pub fn declaration() -> lash_core::plugin::PluginDeclaration {
        lash_core::plugin::PluginDeclaration::initial("mock-state")
    }
}
impl crate::plugin::PluginMetadata for MockPlugin {
    fn plugin_declaration(&self) -> crate::plugin::PluginDeclaration {
        Self::declaration()
    }
}
impl SessionPlugin for MockPlugin {
    fn id(&self) -> &'static str {
        "mock-state"
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
            "ready must observe hydrated state through the captured registrar view"
        );
        if self.writes_on_ready {
            assert_eq!(context.state.get("ready"), Some(serde_json::json!(true)));
        }
        Ok(())
    }
}
impl MockPlugin {
    pub(super) fn host(&self) -> crate::PluginHost {
        let mut factories = crate::testing::test_standard_protocol_factories();
        factories.push(Arc::new(self.clone()));
        crate::PluginHost::new(
            factories,
            lash_core::ExecutionBudgets::recommended(),
            lash_core::trace::TraceRuntime::new(std::sync::Arc::new(
                lash_core::facade_support::SystemClock,
            )),
        )
    }
    pub(super) fn state(&self, id: &str) -> PluginStateView {
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
