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
use lash_durable::domain::{RunValuesWrite, TurnNamespace, TurnNamespaceWrite};

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
    bodies: BTreeMap<String, NamespaceBody>,
}

impl RunRows {
    fn base_values(
        &mut self,
        plugin: &str,
    ) -> Result<Option<&crate::BlobRef>, lash_protocol_types::allocation::AllocationError> {
        if !self.base_values.contains_key(plugin) {
            let Some(namespace) = self.base.plugins.get(plugin) else {
                return Ok(None);
            };
            let values = namespace.body()?.values().clone();
            self.base_values.insert(plugin.to_owned(), values);
        }
        Ok(self.base_values.get(plugin))
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
        && (namespace.values.ptr_eq(&base.values) || namespace.values == base.values)
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
fn unrecorded_values(plugin: &str) -> lash_execution_contracts::plugin_error::PluginError {
    lash_execution_contracts::plugin_error::PluginError::StoredDataCorrupt {
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
    pub fn begin_run(&self) -> Result<(), lash_protocol_types::allocation::AllocationError> {
        let mut guard = self.state.lock_recover();
        let registry = &mut *guard;
        let mut data = registry.data.clone();
        for (id, current) in &registry.data.plugins {
            if !current.publication.recent().is_empty() {
                let mut namespace = current.clone();
                namespace.publication.settle();
                data.plugins.insert_ref(id, namespace)?;
            }
        }
        if data != registry.data {
            registry.source = None;
        }
        registry.data = data;
        registry.run = Some(RunRows {
            base: registry.data.clone(),
            base_values: BTreeMap::new(),
            rows: BTreeMap::new(),
            bodies: BTreeMap::new(),
        });
        Ok(())
    }

    /// The namespaces whose published state the run's durable rows do not
    /// record yet, each with its values body when no row or base holds those
    /// values: what the run's next commit writes. An unchanged namespace
    /// costs nothing, however large. Empty outside a durable run and on
    /// plugin-facing handles, as with [`Self::export_state`].
    pub fn run_changes(
        &self,
    ) -> Result<Vec<TurnNamespaceWrite>, lash_protocol_types::allocation::AllocationError> {
        if !self.host.export_plugin_namespaces {
            return Ok(Vec::new());
        }
        let mut guard = self.state.lock_recover();
        let registry = &mut *guard;
        let Some(run) = registry.run.as_mut() else {
            return Ok(Vec::new());
        };
        let mut changes = Vec::new();
        for (plugin, namespace) in &registry.data.plugins {
            match (run.rows.get(plugin), run.base.plugins.get(plugin)) {
                (Some(row), _) if is_recorded(namespace, row) => continue,
                (None, Some(base)) if is_base(namespace, base) => continue,
                _ => {}
            }
            let body = namespace.body()?;
            let values = if run.base_values(plugin)? == Some(body.values()) {
                RunValuesWrite::Base
            } else if run
                .rows
                .get(plugin)
                .is_some_and(|row| row.values.as_ref() == body.values())
            {
                RunValuesWrite::Held
            } else {
                let previous = match run.bodies.get(plugin) {
                    Some(body) => Some(body.clone()),
                    None => run
                        .base
                        .plugins
                        .get(plugin)
                        .map(PluginNamespaceState::body)
                        .transpose()?,
                };
                RunValuesWrite::Body(body.publication_after(previous.as_ref(), true)?)
            };
            changes.push(TurnNamespaceWrite {
                plugin: plugin.clone(),
                entry: namespace.entry(body.values())?,
                values,
            });
        }
        Ok(changes)
    }

    /// Record that a commit wrote `written`, rows [`Self::run_changes`]
    /// named: the run's rows record them from now on.
    pub fn run_changes_committed(&self, written: &[TurnNamespaceWrite]) {
        let mut registry = self.state.lock_recover();
        if let Some(run) = registry.run.as_mut() {
            for namespace in written {
                match &namespace.values {
                    RunValuesWrite::Body(body) => {
                        run.bodies.insert(namespace.plugin.clone(), body.clone());
                    }
                    RunValuesWrite::Base => {
                        run.bodies.remove(&namespace.plugin);
                    }
                    RunValuesWrite::Held => {}
                }
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
    /// [`lash_execution_contracts::plugin_error::PluginError::StoredDataCorrupt`] for a row whose values neither its
    /// body nor the run's base hold, or a body that does not decode; and
    /// [`lash_execution_contracts::plugin_error::PluginError::Session`] outside a durable run.
    pub fn restore_run(
        &self,
        rows: Vec<TurnNamespace>,
    ) -> Result<(), lash_execution_contracts::plugin_error::PluginError> {
        let mut guard = self.state.lock_recover();
        let registry = &mut *guard;
        let Some(run) = registry.run.as_mut() else {
            return Err(
                lash_execution_contracts::plugin_error::PluginError::Session(
                    "a run restores only over the state it began from".into(),
                ),
            );
        };
        let mut state = run.base.clone();
        for row in &rows {
            let values = match &row.body {
                Some(body) if body.root().to_hex() == row.entry.values.as_str() => body.snapshot(),
                Some(_) => return Err(unrecorded_values(&row.plugin)),
                None if run.base_values(&row.plugin)? == Some(row.entry.values.as_ref()) => run
                    .base
                    .plugins
                    .get(&row.plugin)
                    .map(|base| base.values.clone())
                    .ok_or_else(|| unrecorded_values(&row.plugin))?,
                _ => return Err(unrecorded_values(&row.plugin)),
            };
            let mut namespace = row.entry.namespace(values)?;
            if let Some(body) = &row.body {
                namespace.encoding =
                    lash_core_store::plugin_state::NamespaceEncoding::recorded(body.clone())?;
            } else if let Some(base) = run.base.plugins.get(&row.plugin) {
                namespace.encoding = base.encoding.clone();
            }
            state.plugins.insert_ref(&row.plugin, namespace)?;
        }
        self.host.retired_plugins.check_state(&state)?;
        run.bodies = rows
            .iter()
            .filter_map(|row| row.body.clone().map(|body| (row.plugin.clone(), body)))
            .collect();
        run.rows = rows
            .into_iter()
            .map(|row| (row.plugin, row.entry))
            .collect();
        // The turn's preparation adopted its segment before the restore;
        // the restored namespaces are owned by it as the rest are.
        let segment = registry.segment;
        registry.hydrate_live(&state)?;
        if registry.segment < segment {
            registry.segment = segment;
        }
        let segment = registry.segment;
        let mut data = registry.data.clone();
        for (id, current) in &registry.data.plugins {
            let mut namespace = current.clone();
            namespace.publication.owner_segment = segment;
            data.plugins.insert_ref(id, namespace)?;
        }
        registry.data = data;
        registry.source = None;
        Ok(())
    }
}
