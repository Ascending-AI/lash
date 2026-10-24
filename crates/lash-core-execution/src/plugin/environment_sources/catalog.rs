//! The catalog's removal projection has its own inputs, even for owners that
//! register no provider. Its output is part of the recorded body, never replayed.
use super::*;
use crate::plugin::{PluginSession, ToolCatalogContext};

pub const SOURCE: &str = "catalog";

type CatalogWitness = (SourceSnapshot, NamespaceSlices);
pub(super) struct CatalogSource {
    session: Arc<PluginSession>,
    owners: BTreeSet<String>,
    revision: Mutex<SourceRevision>,
    witness: Mutex<Option<CatalogWitness>>,
}
impl CatalogSource {
    fn namespaces(&self) -> NamespaceSlices {
        let state = self.session.state.lock_recover();
        self.owners
            .iter()
            .filter_map(|owner| {
                state
                    .data
                    .plugins
                    .get(owner)
                    .map(|namespace| (owner.clone(), namespace.clone()))
            })
            .collect()
    }
}
impl EnvironmentSource for CatalogSource {
    fn descriptor(&self) -> SourceDescriptor {
        SourceDescriptor {
            source: SourceId(SOURCE.into()),
            mode: SourceMode::Notifying,
        }
    }
    fn config_dependencies(&self) -> BTreeSet<String> {
        self.owners.clone()
    }
    fn namespace_dependencies(&self) -> BTreeSet<String> {
        self.owners.clone()
    }
    fn subscribe(&self, sink: &EnvironmentChangeSink) {
        self.session.state.lock_recover().environment.register(
            &SourceId(SOURCE.into()),
            self.owners.clone(),
            self.owners.clone(),
            sink,
        );
    }
    fn snapshot(&self, config: &OwnerConfigView) -> Result<SourceSnapshot, SourceCollectionError> {
        let namespaces = self.namespaces();
        let mut fragment = EnvironmentFragment::default();
        for owner in &self.owners {
            if let Some(value) = config.environment_config_slice(owner).map_err(|error| {
                PluginError::ProviderConfig {
                    owner: owner.clone(),
                    message: error.to_string(),
                }
            })? {
                fragment.config.insert(owner.clone(), value);
            }
        }
        for (owner, namespace) in &namespaces {
            fragment
                .config
                .insert(format!("namespace:{owner}"), namespace_identity(namespace));
        }
        let mut overlay = self.session.tool_catalog_overlay.clone();
        overlay.remove.sort();
        overlay.remove.dedup();
        fragment.config.insert(
            "catalog:overlay".into(),
            serde_json::to_value(overlay)
                .map_err(|error| PluginError::Session(error.to_string()))?,
        );
        let mut revision = self.revision.lock_recover();
        revision.counter = revision
            .counter
            .checked_add(1)
            .ok_or_else(|| PluginError::Session("catalog revision exhausted".into()))?;
        let snapshot = SourceSnapshot {
            source: SourceId(SOURCE.into()),
            revision: revision.clone(),
            fragment,
        };
        *self.witness.lock_recover() = Some((snapshot.clone(), namespaces));
        Ok(snapshot)
    }
}
impl CatalogSource {
    pub(super) fn project(
        &self,
        tools: Vec<crate::ToolManifest>,
        recorded: Option<&EnvironmentFragment>,
        config: &OwnerConfigView,
    ) -> Result<SourceSnapshot, SourceCollectionError> {
        let witness = self
            .witness
            .lock_recover()
            .clone()
            .ok_or_else(|| SourceCollectionError::UnknownSource(SourceId(SOURCE.into())))?;
        let (mut snapshot, namespaces) =
            if recorded.is_some_and(|f| f.config == witness.0.fragment.config) {
                witness
            } else {
                // A run override observes its own view without replacing the base
                // head. Re-establish this view's witness before deriving it again.
                self.snapshot(config)?;
                self.witness
                    .lock_recover()
                    .clone()
                    .ok_or_else(|| SourceCollectionError::UnknownSource(SourceId(SOURCE.into())))?
            };
        if namespaces != self.namespaces() {
            return Err(SourceCollectionError::ChangedDuringSnapshot(
                snapshot.source,
            ));
        }
        let mut contributions = crate::plugin::session_obj::collect_owned_sync(
            self.session.composition().tool_catalog_contributors(),
            ToolCatalogContext {
                owner: self.session.owner.clone(),
                tools,
                config: config.clone(),
                extensions: self.session.extensions.clone(),
            },
            |hook, ctx| hook(ctx),
        )?
        .into_iter()
        .map(|owned| owned.value)
        .collect::<Vec<_>>();
        contributions.push(self.session.tool_catalog_overlay.clone());
        let removed: BTreeSet<_> = contributions.into_iter().flat_map(|c| c.remove).collect();
        snapshot.fragment.model_removals = removed;
        if namespaces != self.namespaces() {
            return Err(SourceCollectionError::ChangedDuringSnapshot(
                snapshot.source,
            ));
        }
        Ok(snapshot)
    }
}
impl PluginSession {
    pub(super) fn register_catalog_source(
        self: &Arc<Self>,
        sources: &mut EnvironmentSources,
        sink: EnvironmentChangeSink,
    ) -> Result<(), SourceCollectionError> {
        if self.composition().tool_catalog_contributors().is_empty()
            && self.tool_catalog_overlay.is_empty()
        {
            return Ok(());
        }
        let owners = self
            .state
            .lock_recover()
            .data
            .plugins
            .keys()
            .cloned()
            .chain(
                self.admitted_plugin_config()
                    .config
                    .namespaces()
                    .keys()
                    .cloned(),
            )
            .chain(
                self.composition()
                    .tool_catalog_contributors()
                    .iter()
                    .map(|hook| hook.identity.owner.clone()),
            )
            .collect();
        let catalog = Arc::new(CatalogSource {
            session: self.clone(),
            owners,
            revision: Mutex::new(SourceRevision {
                incarnation: uuid::Uuid::new_v4(),
                counter: 0,
            }),
            witness: Mutex::new(None),
        });
        sources.register(catalog.clone(), sink)?;
        sources.catalog = Some(catalog);
        Ok(())
    }
}
