//! Resident checkpoint component shapes: ADR 0056's commit states as types.
//!
//! `ResidentCheckpointComponent` is either `Unchanged` — the next commit
//! reuses its durable descriptor — or `Changed` — the next commit writes a
//! body whose payload is present by construction. An unchanged component
//! whose body was released holds `ResidentCheckpointComponentBody`'s payload
//! `None`.

use lash_kernel_doc::allocation::SharedBytes;

#[derive(Clone, Debug, serde::Serialize)]
pub(super) enum ResidentCheckpointComponentBody {
    PluginState {
        snapshot: Option<crate::PluginState>,
        map: PluginStateRecord,
    },
    Opaque(Option<SharedBytes>),
}

/// The head's namespace map as resident state keeps it, with which values
/// each entry's body was encoded from: a capture holding the same shared
/// values is unchanged without encoding them again.
type HeldAddresses = crate::source_map::SharedSourceMap<crate::checkpoint_graph::NodeId, ()>;

#[derive(Clone, Debug, Default, serde::Serialize)]
pub(super) struct PluginStateRecord {
    pub(super) map: crate::plugin_state::PluginStateMap,
    #[serde(skip)]
    pub(super) bodies:
        Option<crate::source_map::SharedSourceMap<String, crate::plugin_state::NamespaceBody>>,
    #[serde(skip)]
    pub(super) held_nodes: crate::source_map::SharedSourceMap<String, HeldAddresses>,
    #[serde(skip)]
    prepared_held_nodes: crate::source_map::SharedSourceMap<String, HeldAddresses>,
    #[serde(skip)]
    pub(super) encoded_from: Option<
        crate::source_map::SharedSourceMap<String, crate::plugin_state::NamespaceValuesWeak>,
    >,
    #[serde(skip)]
    generations: crate::source_map::SharedSourceMap<String, u64>,
}

impl PluginStateRecord {
    pub(super) fn empty_in(
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        use crate::source_map::SharedSourceMap;
        Ok(Self {
            map: crate::plugin_state::PluginStateMap::empty_in(budget)?,
            bodies: Some(SharedSourceMap::empty_in(budget)?),
            held_nodes: SharedSourceMap::empty_in(budget)?,
            prepared_held_nodes: SharedSourceMap::empty_in(budget)?,
            encoded_from: Some(SharedSourceMap::empty_in(budget)?),
            generations: SharedSourceMap::empty_in(budget)?,
        })
    }
    pub(super) fn clone_in(
        &self,
        budget: &lash_protocol_types::allocation::AllocationBudget,
    ) -> Result<Self, lash_protocol_types::allocation::AllocationError> {
        Ok(Self {
            map: self.map.clone_in(budget)?,
            bodies: self
                .bodies
                .as_ref()
                .map(|map| map.clone_in(budget))
                .transpose()?,
            held_nodes: self.held_nodes.clone_in(budget)?,
            prepared_held_nodes: self.prepared_held_nodes.clone_in(budget)?,
            encoded_from: self
                .encoded_from
                .as_ref()
                .map(|map| map.clone_in(budget))
                .transpose()?,
            generations: self.generations.clone_in(budget)?,
        })
    }
    pub(super) fn body(&self, plugin: &str) -> Option<&crate::plugin_state::NamespaceBody> {
        self.bodies.as_ref()?.get(plugin)
    }
    pub(super) fn body_iter(
        &self,
    ) -> impl Iterator<Item = (&String, &crate::plugin_state::NamespaceBody)> {
        self.bodies.as_ref().into_iter().flat_map(|map| map.iter())
    }
    pub(super) fn insert_body(
        &mut self,
        plugin: String,
        body: crate::plugin_state::NamespaceBody,
    ) -> Result<(), lash_protocol_types::allocation::AllocationError> {
        if let Some(bodies) = &mut self.bodies {
            bodies.insert(plugin, body)?;
        }
        Ok(())
    }
    pub(super) fn record_source(
        &mut self,
        plugin: String,
        values: &crate::plugin_state::NamespaceValues,
    ) -> Result<(), lash_protocol_types::allocation::AllocationError> {
        if let Some(encoded) = &mut self.encoded_from {
            encoded.insert(plugin, values.downgrade()?)?;
        }
        Ok(())
    }
    pub(super) fn prepare_adoption(
        &mut self,
    ) -> Result<(), lash_protocol_types::allocation::AllocationError> {
        let budget = self.map.plugins.allocation_budget();
        let mut held = self.held_nodes.clone();
        let mut generations = crate::source_map::SharedSourceMap::empty_in(budget)?;
        for (plugin, entry) in &self.map.plugins {
            generations.insert(plugin.clone(), entry.generation)?;
        }
        for (plugin, body) in self.body_iter() {
            // One sparse B-tree node per fixed-size address, plus source metadata.
            let allocation = budget.reserve(
                (body.graph().len() as u64)
                    .saturating_mul(1024)
                    .saturating_add(256),
            )?;
            let addresses = HeldAddresses::from_admitted(
                body.graph().keys().map(|id| (*id, ())).collect(),
                allocation,
                budget.clone(),
            );
            held.insert(plugin.clone(), addresses)?;
        }
        held.retain(|plugin, _| self.map.plugins.contains_key(plugin))?;
        self.prepared_held_nodes = held;
        self.generations = generations;
        Ok(())
    }
    fn adopt_bodies(&mut self) {
        self.held_nodes = self.prepared_held_nodes.clone();
    }
    fn release_bodies(&mut self) {
        self.bodies = None;
        self.encoded_from = None;
    }
    pub(super) fn generations(&self) -> crate::source_map::SharedSourceMap<String, u64> {
        self.generations.clone()
    }
    pub(super) fn known_values(
        &self,
        plugin: &str,
        values: &crate::plugin_state::NamespaceValues,
    ) -> Option<&crate::store::BlobRef> {
        self.encoded_from
            .as_ref()?
            .get(plugin)
            .filter(|encoded| encoded.matches(values))
            .and(self.map.plugins.get(plugin))
            .map(|entry| entry.values.as_ref())
    }
}

/// A body pending its next commit: ADR 0056's "present body" state, where the
/// payload is always present.
#[derive(Clone, Debug, serde::Serialize)]
pub(super) enum PendingCheckpointComponentBody {
    PluginState {
        snapshot: crate::PluginState,
        map: PluginStateRecord,
    },
    Opaque(SharedBytes),
}

impl PendingCheckpointComponentBody {
    /// The same body kept resident after its commit: the durable ref becomes
    /// authoritative and the payload stays a second copy until released.
    pub(super) fn into_resident(self) -> ResidentCheckpointComponentBody {
        match self {
            Self::PluginState { snapshot, map } => ResidentCheckpointComponentBody::PluginState {
                snapshot: Some(snapshot),
                map,
            },
            Self::Opaque(bytes) => ResidentCheckpointComponentBody::Opaque(Some(bytes)),
        }
    }
}

/// A resident checkpoint component in one of ADR 0056's commit states.
///
/// `Unchanged` always carries the durable ref the next commit reuses, and
/// `Changed` always carries the body it writes — the `(descriptor, body,
/// dirty)` combinations that used to surface as `StoredDataCorrupt` are not
/// representable.
#[derive(Clone, Debug, serde::Serialize)]
pub(super) enum ResidentCheckpointComponent {
    /// The next commit reuses `descriptor`; `body` is the still-resident copy
    /// or a released body (payload `None`).
    Unchanged {
        descriptor: crate::CheckpointComponentDescriptor,
        body: ResidentCheckpointComponentBody,
    },
    /// The next commit writes `body`; `descriptor` is the durable ref it
    /// supersedes when this is a local edit over a committed component.
    Changed {
        descriptor: Option<crate::CheckpointComponentDescriptor>,
        body: PendingCheckpointComponentBody,
    },
}

impl ResidentCheckpointComponent {
    pub(super) fn descriptor(&self) -> Option<&crate::CheckpointComponentDescriptor> {
        match self {
            Self::Unchanged { descriptor, .. } => Some(descriptor),
            Self::Changed { descriptor, .. } => descriptor.as_ref(),
        }
    }

    pub(super) fn plugin_state_snapshot(&self) -> Option<&crate::PluginState> {
        match self {
            Self::Unchanged {
                body: ResidentCheckpointComponentBody::PluginState { snapshot, .. },
                ..
            } => snapshot.as_ref(),
            Self::Changed {
                body: PendingCheckpointComponentBody::PluginState { snapshot, .. },
                ..
            } => Some(snapshot),
            _ => None,
        }
    }

    pub(super) fn plugin_record(&self) -> Option<&PluginStateRecord> {
        match self {
            Self::Unchanged {
                body: ResidentCheckpointComponentBody::PluginState { map, .. },
                ..
            }
            | Self::Changed {
                body: PendingCheckpointComponentBody::PluginState { map, .. },
                ..
            } => Some(map),
            _ => None,
        }
    }

    /// The resident bytes of an opaque component.
    pub(super) fn opaque_body(&self) -> Option<SharedBytes> {
        match self {
            Self::Unchanged {
                body: ResidentCheckpointComponentBody::Opaque(body),
                ..
            } => body.clone(),
            Self::Changed {
                body: PendingCheckpointComponentBody::Opaque(body),
                ..
            } => Some(body.clone()),
            _ => None,
        }
    }

    /// Adopts the committed manifest's durable ref. A changed body stays
    /// resident as the unchanged component's second copy.
    pub(super) fn adopt_descriptor(&mut self, descriptor: crate::CheckpointComponentDescriptor) {
        let mut body = match self {
            Self::Unchanged { body, .. } => {
                std::mem::replace(body, ResidentCheckpointComponentBody::Opaque(None))
            }
            Self::Changed { body, .. } => std::mem::replace(
                body,
                PendingCheckpointComponentBody::Opaque(SharedBytes::default()),
            )
            .into_resident(),
        };
        if let ResidentCheckpointComponentBody::PluginState { map, .. } = &mut body {
            map.adopt_bodies();
        }
        *self = Self::Unchanged { descriptor, body };
    }

    /// Releases the tool and plugin snapshot payloads. A changed typed body
    /// was the pending write's only copy; with the write discarded the
    /// component demotes to the durable ref it superseded, and reports whether
    /// anything durable remains to track.
    pub(super) fn release_typed_snapshot(&mut self) -> bool {
        let released = match self {
            Self::Unchanged { body, .. } => {
                match body {
                    ResidentCheckpointComponentBody::PluginState { snapshot, map } => {
                        *snapshot = None;
                        map.release_bodies();
                    }
                    ResidentCheckpointComponentBody::Opaque(_) => {}
                }
                return true;
            }
            Self::Changed { descriptor, body } => {
                let released = match body {
                    PendingCheckpointComponentBody::PluginState { map, .. } => {
                        map.release_bodies();
                        ResidentCheckpointComponentBody::PluginState {
                            snapshot: None,
                            map: std::mem::take(map),
                        }
                    }
                    PendingCheckpointComponentBody::Opaque(_) => return true,
                };
                (descriptor, released)
            }
        };
        match released.0.take() {
            Some(descriptor) => {
                *self = Self::Unchanged {
                    descriptor,
                    body: released.1,
                };
                true
            }
            None => false,
        }
    }

    /// Releases every resident payload of an unchanged component: once the
    /// durable ref is authoritative, the encoded bytes are a second resident
    /// copy. A changed body has not committed yet; a discarded pending write
    /// demotes to the durable ref it superseded, except a changed opaque
    /// body, which is the retry's only source and stays.
    pub(super) fn release_body(&mut self) -> bool {
        let released = match self {
            Self::Unchanged { body, .. } => {
                match body {
                    ResidentCheckpointComponentBody::PluginState { snapshot, map } => {
                        *snapshot = None;
                        map.release_bodies();
                    }
                    ResidentCheckpointComponentBody::Opaque(payload) => *payload = None,
                }
                return true;
            }
            Self::Changed { descriptor, body } => {
                let released = match body {
                    PendingCheckpointComponentBody::PluginState { map, .. } => {
                        map.release_bodies();
                        ResidentCheckpointComponentBody::PluginState {
                            snapshot: None,
                            map: std::mem::take(map),
                        }
                    }
                    PendingCheckpointComponentBody::Opaque(_) => return true,
                };
                (descriptor, released)
            }
        };
        match released.0.take() {
            Some(descriptor) => {
                *self = Self::Unchanged {
                    descriptor,
                    body: released.1,
                };
                true
            }
            None => false,
        }
    }
}

/// The resident plugin state a head's namespace map and its loaded values
/// bodies hold, and the record that keeps the map resident.
pub(super) fn assemble_plugin_state(
    entries: &std::collections::BTreeMap<String, ResidentCheckpointComponent>,
    map: crate::plugin_state::PluginStateMap,
    namespace_nodes: &std::collections::BTreeMap<
        crate::checkpoint_graph::NodeId,
        crate::checkpoint_graph::EncodedNode,
    >,
) -> Result<(crate::PluginState, PluginStateRecord), crate::StoreError> {
    let budget = map.plugins.allocation_budget().clone();
    let mut state = crate::PluginState::empty_in(&budget)?;
    let mut record = PluginStateRecord::empty_in(&budget)?;
    for (plugin, entry) in &map.plugins {
        let key = crate::plugin_state::namespace_component_key(plugin);
        let component = entries
            .get(&key)
            .ok_or_else(|| crate::StoreError::StoredDataCorrupt {
                record_kind: "HydratedSessionCheckpoint",
                message: format!("plugin `{plugin}` has no values body"),
            })?;
        if component
            .descriptor()
            .map(|descriptor| descriptor.blob_ref.as_ref())
            != Some(entry.values.as_ref())
        {
            return Err(crate::StoreError::StoredDataCorrupt {
                record_kind: "HydratedSessionCheckpoint",
                message: format!("plugin `{plugin}`'s values body is not its map entry's"),
            });
        }
        let bytes =
            component
                .opaque_body()
                .ok_or_else(|| crate::StoreError::StoredDataCorrupt {
                    record_kind: "HydratedSessionCheckpoint",
                    message: format!("plugin `{plugin}`'s values body was not hydrated"),
                })?;
        let root =
            crate::checkpoint_graph::NodeId::from_hex(entry.values.as_str()).ok_or_else(|| {
                crate::StoreError::StoredDataCorrupt {
                    record_kind: "plugin namespace",
                    message: "invalid map root".into(),
                }
            })?;
        let body = crate::plugin_state::NamespaceBody::load(
            namespace_nodes,
            root,
            crate::checkpoint_graph::DecodeLimits::within_total(1 << 32),
        )
        .map_err(|error| crate::StoreError::StoredDataCorrupt {
            record_kind: "plugin namespace",
            message: error.to_string(),
        })?;
        if body.bytes() != &bytes {
            return Err(crate::StoreError::StoredDataCorrupt {
                record_kind: "plugin namespace",
                message: "map root body differs".into(),
            });
        }
        let values = body.snapshot();
        record.insert_body(plugin.clone(), body.clone())?;
        record.record_source(plugin.clone(), &values)?;
        let mut namespace = entry.namespace(values)?;
        namespace.encoding = crate::plugin_state::NamespaceEncoding::recorded(body)?;
        state.plugins.insert(plugin.clone(), namespace)?;
    }
    if let Some(stray) = entries.keys().find_map(|key| {
        crate::plugin_state::namespace_component_plugin(key)
            .filter(|plugin| !map.plugins.contains_key(*plugin))
    }) {
        return Err(crate::StoreError::StoredDataCorrupt {
            record_kind: "HydratedSessionCheckpoint",
            message: format!("plugin `{stray}`'s values body has no map entry"),
        });
    }
    record.map = map;
    record.prepare_adoption()?;
    record.adopt_bodies();
    Ok((state, record))
}

#[cfg(test)]
#[path = "checkpoint_component_tests.rs"]
mod retention_tests;
