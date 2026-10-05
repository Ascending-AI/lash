//! Resident checkpoint adoption, body release and plugin-state capture.

use crate::facade_support::SessionGraphFacadeOps as _;

use super::{
    AcceptedExecutionRetention, ResidentCheckpointComponent, RuntimeSessionState,
    SessionPluginStateSource,
};

impl RuntimeSessionState {
    /// Durable reference for the well-known tool-state component.
    pub fn tool_state_ref(&self) -> Option<&crate::store::BlobRef> {
        self.checkpoint_components
            .component_ref(crate::store::TOOL_STATE_CHECKPOINT_COMPONENT)
    }

    /// Generation carried by the typed tool-state view.
    pub fn tool_state_generation(&self) -> Option<u64> {
        self.checkpoint_components.tool_state_generation()
    }

    /// Typed resident view of the well-known tool-state component.
    pub fn tool_state_snapshot(&self) -> Option<&crate::ToolState> {
        self.checkpoint_components.tool_state_snapshot()
    }

    /// Replace or explicitly delete the well-known tool-state component.
    pub fn set_tool_state_snapshot(&mut self, snapshot: Option<crate::ToolState>) {
        self.checkpoint_components.set_tool_state_snapshot(snapshot);
    }

    /// Whether the captured tool catalog differs from its durable component.
    /// Released bodies are unchanged; a repeated stamp of the same catalog
    /// also remains unchanged, even though it supplies a pending body.
    pub fn tool_state_is_dirty(&self) -> Result<bool, crate::StoreError> {
        let Some(snapshot) = self.tool_state_snapshot() else {
            return Ok(false);
        };
        let bytes = crate::store::encode_checkpoint_component(
            crate::store::TOOL_STATE_CHECKPOINT_COMPONENT,
            snapshot,
        )?;
        Ok(self.tool_state_ref() != Some(&crate::store::BlobRef::for_content(&bytes)))
    }

    /// Durable reference for the well-known plugin-snapshot component.
    pub fn plugin_state_ref(&self) -> Option<&crate::store::BlobRef> {
        self.checkpoint_components
            .component_ref(crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
    }

    /// Typed resident view of the well-known plugin-snapshot component.
    pub fn plugin_state(&self) -> Option<&crate::PluginState> {
        self.checkpoint_components.plugin_state()
    }

    /// Replace or explicitly delete the well-known plugin-snapshot component.
    pub fn set_plugin_state(&mut self, snapshot: Option<crate::PluginState>) {
        self.checkpoint_components.set_plugin_state(snapshot);
    }

    pub fn plugin_state_is_dirty(&self) -> bool {
        self.checkpoint_components
            .component(crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
            .is_some_and(|component| {
                matches!(component, ResidentCheckpointComponent::Changed { .. })
            })
    }

    /// Durable reference for the well-known execution-state component.
    pub fn execution_state_ref(&self) -> Option<&crate::store::BlobRef> {
        self.checkpoint_components
            .component_ref(crate::store::EXECUTION_STATE_CHECKPOINT_COMPONENT)
    }

    /// Advances resident state to a store's committed head revision and realized timestamps, adopts
    /// durable artifact references, and clears transient snapshots so protocol and store
    /// implementors cannot reuse stale bytes.
    ///
    /// # Panics
    /// Panics in every build profile if the receipt does not advance resident
    /// state. Replay callers refresh state instead of adopting an old receipt.
    pub fn apply_persisted_commit_result(&mut self, result: crate::store::RuntimeCommitReceipt) {
        assert!(
            result.head_revision > self.head_revision,
            "adopted head revision must advance"
        );
        self.head_revision = result.head_revision;
        self.checkpoint_ref = Some(result.checkpoint_ref);
        self.session_graph
            .apply_realized_node_timestamps(&result.realized_node_timestamps);
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
        let committed_components_match = self
            .checkpoint_components
            .manifest_matches_resident_commit_intent(&result.manifest);
        self.checkpoint_components.adopt_manifest(&result.manifest);
        self.checkpoint_components.discard_known_bodies(
            committed_components_match,
            AcceptedExecutionRetention::DurableHead,
        );
    }

    pub fn pending_graph_commit(&self) -> crate::GraphAppend {
        let nodes = self
            .session_graph
            .nodes
            .iter()
            .filter(|node| !self.persisted_node_ids.contains(&node.node_id))
            .map(|node| node.as_ref().clone())
            .collect::<Vec<_>>();
        if nodes.is_empty() {
            crate::GraphAppend::PreserveHead
        } else {
            crate::GraphAppend::Extend { nodes }
        }
    }

    /// Record the nodes an accepted commit receipt made durable, then retire
    /// the durable nodes below the current frame
    /// ([`Self::retire_below_current_frame`]).
    pub fn mark_node_ids_persisted<I>(&mut self, node_ids: I)
    where
        I: IntoIterator<Item = crate::NodeId>,
    {
        self.persisted_node_ids.extend(node_ids);
        self.retire_below_current_frame();
    }

    /// Keep the resident graph to the current frame (ADR 0112 §9).
    ///
    /// When the current `FrameOpen` is not the window base, the graph is
    /// rebuilt from the base's records at and after that `FrameOpen`, under
    /// an anchor derived from the old one: the base generation plus the path
    /// offset, the `FrameOpen`'s parent, and that parent's frame. Only nodes
    /// that are both durable and below the current `FrameOpen` are dropped;
    /// a pending node stays until it is durable, and the trim waits while a
    /// pending node still hangs off a node it would drop. When the base is
    /// already current, this does nothing.
    pub fn retire_below_current_frame(&mut self) {
        let Some(retired) = self
            .session_graph
            .retire_below_current_frame(&self.persisted_node_ids)
        else {
            return;
        };
        for node_id in &retired {
            self.persisted_node_ids.remove(node_id);
        }
        self.agent_frames = self.session_graph.agent_frame_records(&self.session_id);
    }

    /// Clears in-memory tool, plugin, and execution-state snapshots for protocol implementors after
    /// their durable references have become authoritative.
    pub fn discard_runtime_snapshots(&mut self) {
        self.checkpoint_components
            .discard_known_bodies(true, AcceptedExecutionRetention::DurableHead);
    }

    /// [`Self::discard_runtime_snapshots`] for a session no store backs: the
    /// accepted execution bodies stay resident, replaced at the next commit,
    /// so a same-frame restore rebuilds from them instead of from nothing
    /// (FIG-2521).
    pub fn discard_runtime_snapshots_retaining_accepted_execution(&mut self) {
        self.checkpoint_components
            .discard_known_bodies(true, AcceptedExecutionRetention::Resident);
    }

    /// Updates execution state snapshot state for protocol and process-engine implementors while
    /// materializing or restoring protocol session state.
    pub fn set_execution_state_snapshot(
        &mut self,
        execution_state_snapshot: Option<std::sync::Arc<[u8]>>,
    ) {
        // A materialized frame-switch outcome passes `None` here to clear the checkpoint. Clear
        // the durable ref with the resident body: every store interprets an absent body with a
        // present ref as an unchanged component, which would otherwise restore the old frame.
        self.checkpoint_components
            .set_execution_state_snapshot(execution_state_snapshot);
    }

    /// Replaces the complete protocol-owned execution-state root and leaf set.
    ///
    /// Runtime-owned staging: the turn boundary and the explicit administrative
    /// restore path are the only callers, so this is not integrator surface
    /// (ADR 0051's "neither" class — it only mutates state the runtime owns).
    /// Downstream tests reach the same staging through
    /// `lash_core::testing::stage_execution_state_components`.
    pub fn set_execution_state_components(
        &mut self,
        snapshot: crate::plugin::ExecutionStateCapture,
    ) -> Result<(), crate::StoreError> {
        self.checkpoint_components
            .set_execution_state_components(snapshot)
    }

    /// Stages a complete execution capture the protocol session was just
    /// restored to, over the resident set this state already holds (FIG-2521).
    ///
    /// The executor treats every restored leaf as its persisted baseline, so
    /// the next commit references them as unchanged. A leaf the resident set
    /// holds — durably, or as a body still waiting for its first commit —
    /// keeps exactly that bookkeeping; only a leaf the set never held is
    /// staged with its body. The run is staged as changed: a capture may
    /// carry appended seed globals the durable root does not.
    pub fn stage_restored_execution_state(
        &mut self,
        restored: crate::plugin::HydratedExecutionState,
    ) -> Result<(), crate::StoreError> {
        let leaves = restored
            .components
            .into_iter()
            .map(|(key, body)| {
                let change = if self.checkpoint_components.holds_execution_state_leaf(&key) {
                    crate::LeafChange::Unchanged
                } else {
                    crate::LeafChange::Changed(body)
                };
                (key, change)
            })
            .collect();
        self.set_execution_state_components(crate::ExecutionStateCapture::Replace {
            root: restored.root,
            leaves,
        })
    }

    /// Exposes execution state snapshot to protocol and process-engine implementors while
    /// materializing or restoring protocol session state. Returns `None` when no execution state
    /// snapshot is present.
    pub fn execution_state_snapshot(&self) -> Option<std::sync::Arc<[u8]>> {
        self.checkpoint_components.execution_state_snapshot()
    }

    /// Returns the fully hydrated protocol-owned execution-state root and leaves.
    pub fn execution_state_hydration(
        &self,
    ) -> Result<Option<crate::plugin::HydratedExecutionState>, crate::StoreError> {
        self.checkpoint_components.execution_state_hydration()
    }

    /// Refreshes exported plugin state while respecting the session handle's
    /// namespace permissions. Plugin-facing handles expose no namespaces.
    ///
    /// # Errors
    /// The source's typed refusal when a namespace cannot be written in the
    /// format its admission recorded; the state keeps its last capture.
    pub fn refresh_plugin_states(
        &mut self,
        plugins: &dyn SessionPluginStateSource,
    ) -> Result<(), crate::RuntimeError> {
        self.refresh_plugin_states_with(plugins, |source| source.export_plugin_state())
    }

    /// Captures every plugin namespace as the runtime commits it.
    ///
    /// # Errors
    /// As [`Self::refresh_plugin_states`].
    pub fn capture_plugin_states(
        &mut self,
        plugins: &dyn SessionPluginStateSource,
        fleet: crate::store::FleetFormat,
    ) -> Result<(), crate::RuntimeError> {
        // A native checkpoint is a cold-open record of the session. The
        // running Run's overrides belong only to its recorded admission.
        let config = crate::store::persisted_session_config_from_state(self).plugin_config;
        let native = plugins.capture_plugin_admission(&config, fleet)?;
        self.refresh_plugin_states_with(plugins, |source| source.capture_plugin_state())?;
        if let Some(bytes) = native {
            self.set_plugin_admission_snapshot(bytes);
        }
        Ok(())
    }

    fn refresh_plugin_states_with(
        &mut self,
        plugins: &dyn SessionPluginStateSource,
        capture: fn(
            &dyn SessionPluginStateSource,
        ) -> Result<crate::PluginState, crate::RuntimeError>,
    ) -> Result<(), crate::RuntimeError> {
        // A `PreservePersisted` open (FIG-3353) never reconciled its registry,
        // so refreshing tool state here would overwrite the durable surface
        // with whatever the sources happen to advertise. The loaded snapshot
        // rides the next commit forward untouched.
        if !self.preserve_tool_state_snapshot {
            let generation = plugins.tool_state_generation();
            if self.tool_state_ref().is_none() || self.tool_state_generation() != Some(generation) {
                let snapshot = plugins.export_tool_state();
                self.set_tool_state_snapshot(Some(snapshot));
            }
        }

        // Ownership and receipt evidence can change without a values-generation
        // change. Compare the complete recorded namespace checkpoint.
        let snapshot = capture(plugins)?;
        if !snapshot.plugins.is_empty() && !self.matches_plugin_checkpoint(&snapshot)? {
            self.set_plugin_state(Some(snapshot));
        }
        // The config a commit writes is the recorded one too (FIG-4747): the
        // sticky config under a run view and the view the state runs under
        // are written in the formats the source's admission recorded, never
        // in the native format a load decoded them to.
        if let Some(config) = plugins.committed_plugin_config(&self.authority.plugin_config)? {
            self.authority.plugin_config = config;
        }
        if let Some(view) = self.authority.run_view.as_deref_mut()
            && let Some(config) = plugins.committed_plugin_config(&view.sticky.plugin_config)?
        {
            view.sticky.plugin_config = config;
        }
        Ok(())
    }

    /// Released resident bytes still have an authoritative content address.
    /// Compare the complete namespace, including ownership and receipts,
    /// rather than treating a released body as an uncommitted write.
    fn matches_plugin_checkpoint(
        &self,
        snapshot: &crate::PluginState,
    ) -> Result<bool, crate::RuntimeError> {
        if let Some(resident) = self.plugin_state() {
            return Ok(resident == snapshot);
        }
        let Some(recorded) = self.plugin_state_ref() else {
            return Ok(false);
        };
        let bytes = crate::store::encode_checkpoint_component(
            crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT,
            snapshot,
        )
        .map_err(|error| {
            crate::RuntimeError::new(
                crate::RuntimeErrorCode::RuntimeStoreCorrupt,
                format!("failed to encode captured plugin checkpoint: {error}"),
            )
        })?;
        Ok(*recorded == crate::store::BlobRef::for_content(&bytes))
    }
}
