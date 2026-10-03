use super::*;

#[derive(Default)]
pub(super) struct Counts {
    pub converters: Arc<AtomicUsize>,
    pub initializers: AtomicUsize,
    pub builds: AtomicUsize,
    pub registers: AtomicUsize,
    pub ready: AtomicUsize,
    pub callbacks: AtomicUsize,
    pub providers: AtomicUsize,
}

#[derive(Clone)]
pub(super) struct Probe {
    pub counts: Arc<Counts>,
    pub convert: bool,
    pub refuse: bool,
}

impl crate::PluginFactory for Probe {
    fn id(&self) -> &'static str {
        if self.convert {
            "convert-probe"
        } else {
            "publish-probe"
        }
    }
    fn declaration(&self) -> crate::plugin::PluginDeclaration {
        if self.convert {
            Converter(self.counts.converters.clone(), false).declaration()
        } else {
            crate::plugin::PluginDeclaration::initial(crate::PluginFactory::id(self))
        }
    }
    fn initialize_state(
        &self,
        _: &crate::RuntimeOwner,
        _: &crate::PluginConfig,
    ) -> Result<BTreeMap<String, serde_json::Value>, crate::PluginError> {
        assert!(!self.convert, "only the missing namespace initializes");
        self.counts.initializers.fetch_add(1, Ordering::SeqCst);
        Ok(BTreeMap::from([(
            "initialized".into(),
            serde_json::json!(17),
        )]))
    }
    fn migrate_format(
        &self,
        from: crate::FormatVersion,
        namespace: crate::FormatNamespace,
        value: serde_json::Value,
    ) -> Result<serde_json::Value, crate::FormatRefusal> {
        assert!(self.convert);
        Converter(self.counts.converters.clone(), self.refuse)
            .migrate_format(from, namespace, value)
    }
    fn encode_format(
        &self,
        to: crate::FormatVersion,
        namespace: crate::FormatNamespace,
        value: &serde_json::Value,
    ) -> Result<serde_json::Value, crate::FormatRefusal> {
        if self.convert {
            Converter(self.counts.converters.clone(), false).encode_format(to, namespace, value)
        } else {
            assert_eq!(to, crate::FormatVersion::ONE);
            Ok(value.clone())
        }
    }
    fn build(
        &self,
        context: &crate::PluginSessionContext,
    ) -> Result<Arc<dyn crate::SessionPlugin>, crate::PluginError> {
        if self.convert {
            assert_eq!(
                context.plugin_config.config.get("convert-probe").unwrap(),
                &serde_json::json!({"native":17})
            );
        }
        self.counts.builds.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(self.clone()))
    }
}

impl crate::SessionPlugin for Probe {
    fn id(&self) -> &'static str {
        crate::PluginFactory::id(self)
    }
    fn register(&self, registrar: &mut crate::PluginRegistrar) -> Result<(), crate::PluginError> {
        self.counts.registers.fetch_add(1, Ordering::SeqCst);
        let calls = self.counts.clone();
        registrar.turn().before(
            crate::hook_key!("publication-probe"),
            Arc::new(move |_| {
                let calls = calls.clone();
                Box::pin(async move {
                    calls.callbacks.fetch_add(1, Ordering::SeqCst);
                    panic!("no ordinary callback runs during transition publication")
                })
            }),
        )?;
        Ok(())
    }
    fn session_ready(
        &self,
        context: crate::plugin::SessionReadyContext,
    ) -> Result<(), crate::PluginError> {
        self.counts.ready.fetch_add(1, Ordering::SeqCst);
        let key = if self.convert {
            "native"
        } else {
            "initialized"
        };
        assert_eq!(context.state.get(key), Some(serde_json::json!(17)));
        assert_eq!(context.state.generation(), if self.convert { 8 } else { 0 });
        Ok(())
    }
}

#[derive(Clone)]
pub(super) enum Stores {
    Memory(Arc<dyn crate::StoreSet>),
    File(std::path::PathBuf),
}

pub(super) struct StoreLifetime {
    stores: Weak<dyn crate::StoreSet>,
    factory: Weak<dyn crate::RuntimeStore>,
}

type PreviousStore = Arc<Mutex<Option<StoreLifetime>>>;
impl Stores {
    pub(super) async fn open(&self, previous: &PreviousStore) -> Arc<dyn crate::StoreSet> {
        match self {
            Self::Memory(stores) => stores.clone(),
            Self::File(path) => {
                assert!(
                    previous.lock_recover().as_ref().is_none_or(|old| old
                        .stores
                        .upgrade()
                        .is_none()
                        && old.factory.upgrade().is_none()),
                    "the file store set closes before cold reopen"
                );
                let stores: Arc<dyn crate::StoreSet> =
                    Arc::new(lash_sqlite_store::SqliteStoreSet::open(path).await.unwrap());
                let factory: Arc<dyn crate::RuntimeStore> = stores.session_store_factory();
                *previous.lock_recover() = Some(StoreLifetime {
                    stores: Arc::downgrade(&stores),
                    factory: Arc::downgrade(&factory),
                });
                stores
            }
        }
    }
}

pub(super) async fn assert_unpublished(
    store: &crate::store::SessionStore,
    initial: &crate::RuntimeSessionState,
) {
    let current =
        crate::store::load_session_window_state(store, crate::store::WindowSelector::Current)
            .await
            .unwrap()
            .unwrap()
            .state;
    assert_eq!(current.head_revision, initial.head_revision);
    assert_eq!(current.checkpoint_ref, initial.checkpoint_ref);
    assert!(current.plugin_admission_snapshot().is_none());
    assert_eq!(current.plugin_state(), initial.plugin_state());
    assert_eq!(
        current.authority.plugin_config,
        initial.authority.plugin_config
    );
}

/// Seed a later checkpoint with different native values so recovery must honor
/// its admitted resume head rather than the transition's older receipt head.
pub(super) async fn advance_inactive_namespace(
    raw: &Arc<dyn crate::RuntimeStore>,
    store: &crate::store::SessionStore,
    fence: &crate::store::ShiftFence,
) -> crate::store::SessionHeadRef {
    let mut state =
        crate::store::load_session_window_state(store, crate::store::WindowSelector::Current)
            .await
            .unwrap()
            .unwrap()
            .state;
    let mut view =
        crate::plugin::PluginNativeView::decode(&state.plugin_admission_snapshot().unwrap())
            .unwrap();
    let inactive = view.state.plugins.get_mut("inactive").unwrap();
    inactive.generation = 10;
    inactive
        .values
        .insert("retained".into(), serde_json::json!(99));
    let mut written = state.plugin_state().unwrap().clone();
    written.plugins.insert("inactive".into(), inactive.clone());
    state.set_plugin_state(Some(written));
    state.set_plugin_admission_snapshot(view.encode().unwrap());
    let mut commit = crate::RuntimeCommit::persisted_state_for_test(&state);
    commit.shift_fence = Some(Box::new(fence.clone()));
    let receipt = raw.commit_runtime_state(commit).await.unwrap();
    crate::store::SessionHeadRef {
        generation: 0,
        revision: receipt.head_revision,
        checkpoint: Some(receipt.checkpoint_ref),
        leaf: receipt.committed_leaf_node_id,
    }
}
