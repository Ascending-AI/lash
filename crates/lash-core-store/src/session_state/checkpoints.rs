//! Resident checkpoint adoption, body release and plugin-state capture.

use crate::facade_support::SessionGraphFacadeOps as _;

use super::{
    AcceptedExecutionRetention, ResidentCheckpointComponent, RuntimeSessionState,
    SessionPluginStateSource,
};

impl RuntimeSessionState {
    /// Durable reference for the head's plugin namespace map.
    pub fn plugin_state_ref(&self) -> Option<&crate::store::BlobRef> {
        self.checkpoint_components
            .component_ref(crate::store::PLUGIN_STATE_CHECKPOINT_COMPONENT)
    }

    /// The head's plugin state as resident state holds it, while its
    /// values are resident.
    pub fn plugin_state(&self) -> Option<&crate::PluginState> {
        self.checkpoint_components.plugin_state()
    }

    /// Record `snapshot` as the head's plugin state, or delete it. Only a
    /// namespace whose values changed writes its body at the next commit.
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

    /// The SessionExecution root the next checkpoint names, if any.
    pub fn execution_state_ref(&self) -> Option<crate::store::BlobRef> {
        self.checkpoint_components
            .execution_root()
            .map(|root| crate::store::BlobRef(root.to_hex()))
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

    /// Adopt the exact acknowledged postimage while keeping its validated
    /// component bodies available to this claim's next turn.
    ///
    /// # Errors
    /// The receipt differs from the complete prepared checkpoint or graph.
    pub fn adopt_retained_commit_result(
        &mut self,
        result: &crate::store::RuntimeCommitReceipt,
    ) -> Result<(), crate::StoreError> {
        let corrupt = |message: &str| crate::StoreError::StoredDataCorrupt {
            record_kind: "RuntimeCommitReceipt",
            message: message.to_owned(),
        };
        if self.head_revision.checked_add(1) != Some(result.head_revision) {
            return Err(corrupt(
                "acknowledged head does not follow its prepared basis",
            ));
        }
        if !self
            .checkpoint_components
            .manifest_matches_resident_commit_intent(&result.manifest)
        {
            return Err(corrupt(
                "acknowledged checkpoint differs from its prepared postimage",
            ));
        }
        if self.session_graph.leaf_node_id != result.committed_leaf_node_id {
            return Err(corrupt(
                "acknowledged leaf differs from its prepared postimage",
            ));
        }
        self.head_revision = result.head_revision;
        self.checkpoint_ref = Some(result.checkpoint_ref.clone());
        self.session_graph
            .apply_realized_node_timestamps(&result.realized_node_timestamps);
        self.session_graph.mark_all_durable();
        // Cold windows carry an anchor even when their first frame is the
        // graph root. Give the prepared graph that same canonical basis.
        if self.session_graph.anchor().is_none()
            && let Some(base) = self.session_graph.nodes.first()
            && base.parent_node_id.is_none()
            && base.frame_open().is_some()
        {
            let frame_node_id = crate::FrameNodeId::new(base.node_id.as_str())
                .map_err(|_| corrupt("acknowledged root frame has an invalid identity"))?;
            self.session_graph.data_mut().anchor =
                Some(crate::session_graph_window::WindowAnchor {
                    frame_node_id,
                    generation: 0,
                    external_parent: None,
                    previous_frame_node_id: None,
                });
        }
        self.retire_below_current_frame();
        self.agent_frames = self
            .session_graph
            .try_agent_frame_records(&self.session_id)?;
        self.checkpoint_components.adopt_manifest(&result.manifest);
        self.turn_index = result.manifest.turn_state.turn_index;
        self.token_usage = result.manifest.turn_state.token_usage.clone();
        self.last_prompt_usage = result.manifest.turn_state.last_prompt_usage.clone();
        self.take_run_view();
        Ok(())
    }

    /// The pending suffix of the resident graph, which the next commit
    /// carries: O(pending nodes), since durable nodes are the graph's prefix
    /// (FIG-6910).
    pub fn pending_graph_commit(&self) -> crate::GraphAppend {
        let nodes = self
            .session_graph
            .pending_nodes()
            .iter()
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
        I: IntoIterator<Item = lash_vocabulary::identity::NodeId>,
    {
        let node_ids = node_ids.into_iter().collect::<Vec<_>>();
        self.session_graph.mark_durable(&node_ids);
        self.retire_below_current_frame();
    }

    /// Whether the resident node `node_id` is durable.
    pub fn is_node_persisted(&self, node_id: &str) -> bool {
        self.session_graph.is_durable(node_id)
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
        if self.session_graph.retire_below_current_frame().is_none() {
            return;
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
    /// accepted component bodies stay resident, replaced at the next commit,
    /// so a same-frame restore rebuilds from them instead of from nothing
    /// (FIG-2521).
    pub fn discard_runtime_snapshots_retaining_accepted_execution(&mut self) {
        self.checkpoint_components
            .discard_known_bodies(true, AcceptedExecutionRetention::Resident);
    }

    /// Replaces the session's protocol execution state with `capture`, a
    /// session descriptor page (ADR 0145 §9), or clears it: a
    /// materialized frame switch clears it, since the new frame starts
    /// with no globals.
    ///
    /// Runtime-owned staging: the turn boundary and the explicit
    /// administrative restore path are the only callers, so this is not
    /// integrator surface (ADR 0051's "neither" class — it only mutates
    /// state the runtime owns). Downstream tests reach the same staging
    /// through `lash_core::testing::stage_execution_graph`.
    pub fn set_execution_graph(&mut self, capture: crate::plugin::ExecutionGraphCapture) {
        self.checkpoint_components.set_execution_graph(capture);
    }

    /// Stages the descriptor the protocol session was just restored to
    /// (FIG-2521) as the head's execution state. The root is staged as
    /// changed: a capture may carry appended seed globals the durable root
    /// does not.
    pub fn stage_restored_execution_graph(
        &mut self,
        restored: crate::plugin::HydratedExecutionGraph,
    ) {
        self.set_execution_graph(crate::plugin::ExecutionGraphCapture::Replace(restored));
    }

    /// The session descriptor the next checkpoint names; `None` when the
    /// session holds no execution state.
    ///
    /// # Errors
    ///
    /// [`crate::StoreError::IncompleteCheckpointComponentSet`] when resident
    /// state knows the root without its descriptor (the durable head
    /// supplies it), or a stored-data error when the last commit's result
    /// disagreed with its intent.
    pub fn execution_state_hydration(
        &self,
    ) -> Result<Option<crate::plugin::HydratedExecutionGraph>, crate::StoreError> {
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
        // A namespace whose values did not change keeps its body's
        // descriptor; ownership and receipt evidence change only the map.
        let snapshot = capture(plugins)?;
        if !snapshot.plugins.is_empty() {
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
}
