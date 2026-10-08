//! A durable run's plugin-state changes (FIG-5301).
//!
//! A session's run reads the head it started from, overlaid with what it
//! accepted since: its members' committed outcomes, its callbacks'
//! decisions and its cells' calls. The session head records one values
//! body per namespace by content address; a run's commits record only the
//! namespaces it changed, as rows of their own (`turn_namespaces`), and
//! write a namespace's values only when they are not durable yet. A phase
//! checkpoint carries no plugin state. `turn.commit` promotes the run's
//! state into the head; a cancel ends the run and its rows with it, leaving
//! the head as it was.
//!
//! A restore reinstalls the state the run started from with the run's rows
//! over it; a member outcome committed after the last rows publishes again
//! from its record, which the namespace's frontier applies once.
use super::*;
use lash_core_store::plugin_state::{NamespaceBody, NamespaceEntry};
use lash_durable::domain::TurnNamespace;

/// What a run's durable rows record, beside the state it started from.
#[derive(Debug)]
pub(super) struct RunRows {
    /// The state the run started from: its session head's, as the run's
    /// runtime opened it.
    base: PluginState,
    /// The content address of each base namespace's values, once measured.
    base_values: BTreeMap<String, crate::BlobRef>,
    /// The entry each namespace's run row records.
    rows: BTreeMap<String, NamespaceEntry>,
}

impl RunRows {
    fn base_values(&mut self, plugin: &str) -> Option<&crate::BlobRef> {
        if !self.base_values.contains_key(plugin) {
            let namespace = self.base.plugins.get(plugin)?;
            let values = NamespaceBody::encode(&namespace.values).values;
            self.base_values.insert(plugin.to_owned(), values);
        }
        self.base_values.get(plugin)
    }
}

/// Whether `publication` is `recorded`'s but for its owner segment. A run's
/// rows do not record the segment: it is the turn's, which every
/// preparation of the turn adopts on every namespace again, so a namespace
/// the run left alone is no change of the run's.
fn same_publication(publication: &StateFrontier, recorded: &StateFrontier) -> bool {
    publication.applied() == recorded.applied() && publication.recent() == recorded.recent()
}

/// Whether `namespace` is still `base`, the namespace the run started from.
fn is_base(namespace: &PluginNamespaceState, base: &PluginNamespaceState) -> bool {
    namespace.generation == base.generation
        && namespace.format_version == base.format_version
        && namespace.fork == base.fork
        && same_publication(&namespace.publication, &base.publication)
        && (Arc::ptr_eq(&namespace.values, &base.values) || namespace.values == base.values)
}

/// Whether the run's `row` records `namespace`.
fn is_recorded(namespace: &PluginNamespaceState, row: &NamespaceEntry) -> bool {
    namespace.generation == row.generation
        && namespace.format_version == row.format_version
        && namespace.fork == row.fork
        && same_publication(&namespace.publication, &row.publication)
}

/// A run's restore found a row whose values neither its body nor the state
/// the run started from hold.
fn unrecorded_values(plugin: &str) -> crate::plugin::PluginError {
    crate::plugin::PluginError::StoredDataCorrupt {
        record_kind: "turn namespace".into(),
        message: format!("plugin `{plugin}`'s run row names values no body holds"),
    }
}

impl crate::PluginSession {
    /// Begin a durable run over the state published now, the session head's
    /// as the run's runtime opened it: from here, [`Self::run_changes`]
    /// names what the run changed. Every namespace's frontier settles what
    /// the head holds, so the run keeps only the receipts it applies itself
    /// ([`StateFrontier`]).
    pub fn begin_run(&self) {
        let mut guard = self.state.lock_recover();
        let registry = &mut *guard;
        for namespace in registry.data.plugins.values_mut() {
            if !namespace.publication.recent().is_empty() {
                namespace.publication.settle();
                registry.source = None;
            }
        }
        registry.run = Some(RunRows {
            base: registry.data.clone(),
            base_values: BTreeMap::new(),
            rows: BTreeMap::new(),
        });
    }

    /// The namespaces whose published state the run's durable rows do not
    /// record yet, each with its values body when no row or base holds those
    /// values: what the run's next commit writes. An unchanged namespace
    /// costs nothing, however large. Empty outside a durable run.
    pub fn run_changes(&self) -> Vec<TurnNamespace> {
        let mut guard = self.state.lock_recover();
        let registry = &mut *guard;
        let Some(run) = registry.run.as_mut() else {
            return Vec::new();
        };
        let mut changes = Vec::new();
        for (plugin, namespace) in &registry.data.plugins {
            match (run.rows.get(plugin), run.base.plugins.get(plugin)) {
                (Some(row), _) if is_recorded(namespace, row) => continue,
                (None, Some(base)) if is_base(namespace, base) => continue,
                _ => {}
            }
            let body = NamespaceBody::encode(&namespace.values);
            let durable = run
                .rows
                .get(plugin)
                .is_some_and(|row| row.values == body.values)
                || run.base_values(plugin) == Some(&body.values);
            changes.push(TurnNamespace {
                plugin: plugin.clone(),
                entry: namespace.entry(body.values),
                body: (!durable).then_some(body.bytes),
            });
        }
        changes
    }

    /// Record that a commit wrote `written`, rows [`Self::run_changes`]
    /// named: the run's rows record them from now on.
    pub fn run_changes_committed(&self, written: &[TurnNamespace]) {
        let mut registry = self.state.lock_recover();
        if let Some(run) = registry.run.as_mut() {
            for namespace in written {
                run.rows
                    .insert(namespace.plugin.clone(), namespace.entry.clone());
            }
        }
    }

    /// Restore the run from its rows: the state it started from, with each
    /// row's namespace over it, replaces what preparing the run again
    /// published, so nothing the run committed runs again.
    ///
    /// # Errors
    ///
    /// [`crate::plugin::PluginError::StoredDataCorrupt`] for a row whose values neither its
    /// body nor the run's base hold, or a body that does not decode; and
    /// [`crate::plugin::PluginError::Session`] outside a durable run.
    pub fn restore_run(&self, rows: Vec<TurnNamespace>) -> Result<(), crate::plugin::PluginError> {
        let mut guard = self.state.lock_recover();
        let registry = &mut *guard;
        let Some(run) = registry.run.as_mut() else {
            return Err(crate::plugin::PluginError::Session(
                "a run restores only over the state it began from".into(),
            ));
        };
        let mut state = run.base.clone();
        for row in &rows {
            let values = match &row.body {
                Some(bytes) if crate::BlobRef::for_content(bytes) == row.entry.values => {
                    NamespaceBody::decode(&row.plugin, &row.entry.values, bytes)?
                }
                _ if run.base_values(&row.plugin) == Some(&row.entry.values) => run
                    .base
                    .plugins
                    .get(&row.plugin)
                    .map(|base| Arc::clone(&base.values))
                    .ok_or_else(|| unrecorded_values(&row.plugin))?,
                _ => return Err(unrecorded_values(&row.plugin)),
            };
            state
                .plugins
                .insert(row.plugin.clone(), row.entry.namespace(values));
        }
        run.rows = rows
            .into_iter()
            .map(|row| (row.plugin, row.entry))
            .collect();
        // The turn's preparation adopted its segment before the restore;
        // the restored namespaces are owned by it as the rest are.
        let segment = registry.segment;
        registry.hydrate_live(&state);
        if registry.segment < segment {
            registry.segment = segment;
        }
        let segment = registry.segment;
        for namespace in registry.data.plugins.values_mut() {
            namespace.publication.owner_segment = segment;
        }
        registry.source = None;
        Ok(())
    }
}
