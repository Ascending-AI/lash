impl ToolRegistry {
    /// Builds a `ToolRegistry` from tool provider data for protocol and process-engine implementors
    /// while preparing or executing plugin and tool work.
    pub fn from_tool_provider(provider: Arc<dyn ToolProvider>) -> Result<Self, ReconfigureError> {
        let registry = Self::empty();
        registry.upsert_source(Arc::new(ToolProviderSource::new(
            PLUGIN_TOOL_SOURCE_ID,
            provider,
        )))?;
        Ok(registry)
    }

    #[cfg(test)]
    pub(crate) fn from_tool_providers(
        providers: Vec<Arc<dyn ToolProvider>>,
    ) -> Result<Self, ReconfigureError> {
        Self::from_tool_providers_with_hidden_tools(providers, BTreeSet::new())
    }

    pub(crate) fn from_tool_providers_with_hidden_tools(
        providers: Vec<Arc<dyn ToolProvider>>,
        hidden_tool_names: BTreeSet<String>,
    ) -> Result<Self, ReconfigureError> {
        let registry = Self::empty_with_hidden_tools(hidden_tool_names);
        registry.upsert_source(Arc::new(ToolProviderGroupSource::new(
            PLUGIN_TOOL_SOURCE_ID,
            providers,
        )))?;
        Ok(registry)
    }

    pub(crate) fn empty() -> Self {
        Self::empty_with_hidden_tools(BTreeSet::new())
    }

    fn empty_with_hidden_tools(hidden_tool_names: BTreeSet<String>) -> Self {
        Self {
            sources: Arc::new(RwLock::new(BTreeMap::new())),
            state: Arc::new(RwLock::new(ToolRegistryState {
                generation: 0,
                tools: BTreeMap::new(),
                next_live_source_id: 0,
            })),
            hidden_tool_names: Arc::new(hidden_tool_names),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.state
            .read_recover()
            .generation
    }

    pub(crate) fn export_state(&self) -> ToolState {
        let state = self
            .state
            .read_recover();
        ToolState::new(state.generation, export_tool_state_entries(&state.tools))
    }

    pub(crate) fn apply_state(&self, next: ToolState) -> Result<u64, ReconfigureError> {
        let current_generation = self.generation();
        if next.generation != current_generation {
            return Err(ReconfigureError::GenerationMismatch {
                expected: next.generation,
                actual: current_generation,
            });
        }

        let rebound = {
            let sources = self.sources.read_recover();
            reconcile_tool_state_entries(
                next.entries(),
                &sources,
                ReconcileMode::SnapshotSurface,
                None,
                &self.hidden_tool_names,
            )?
        };

        let mut state = self
            .state
            .write_recover();
        if state.generation != next.generation {
            return Err(ReconfigureError::GenerationMismatch {
                expected: next.generation,
                actual: state.generation,
            });
        }
        state.tools = rebound.tools;
        state.generation += 1;
        Ok(state.generation)
    }

    /// Restore a persisted [`ToolState`] snapshot onto a freshly-built registry.
    ///
    /// Unlike [`apply_state`](Self::apply_state) — which applies an incremental
    /// *delta* expected at the current generation and bumps it by one — a
    /// restore rebuilds from the current live source surface and overlays the
    /// snapshot's per-id membership. A byte-equivalent surface remains at `G`;
    /// a new tool, refreshed manifest, rebound orphan, or superseded orphan
    /// bumps to `G + 1` so persistence captures the served surface. Cold
    /// rebuilds can therefore restore a session whose catalog reached
    /// generation `G ≥ 2` onto a base registry at generation 1 without the
    /// generation fence used by [`apply_state`](Self::apply_state).
    ///
    /// Restore is tolerant of missing sources: a persisted tool that no current
    /// source resolves becomes an orphaned entry (kept with its last-known
    /// manifest, excluded from the catalog as a non-member, rebound when its
    /// source returns) instead of failing the whole restore. Tool id is the
    /// registry identity; the live manifest wins on rebind, with persisted Tool
    /// Catalog membership preserved per id. Newly advertised ids are members by
    /// default. Consequently an opt-out does not transfer when a provider
    /// replaces a tool with a new id, even if it reuses the same name. Multiple
    /// sources resolving the same id or advertised name still fail because
    /// execution authority and model-facing names must both be unambiguous.
    pub(crate) fn restore_state(
        &self,
        snapshot: ToolState,
    ) -> Result<ToolRestoreReport, ReconfigureError> {
        let rebound = {
            let sources = self.sources.read_recover();
            reconcile_tool_state_entries(
                snapshot.entries(),
                &sources,
                ReconcileMode::LiveSurface,
                None,
                &self.hidden_tool_names,
            )?
        };

        let mut state = self
            .state
            .write_recover();
        state.tools = rebound.tools;
        state.generation = reconciled_generation(snapshot.generation(), rebound.changed)?;
        Ok(ToolRestoreReport {
            generation: state.generation,
            orphaned: rebound.orphaned,
        })
    }

    pub(crate) fn compose_session_catalog(
        &self,
        include_base_tools: bool,
        context_providers: Vec<Arc<dyn ToolProvider>>,
    ) -> Result<Self, ReconfigureError> {
        let registry = if include_base_tools {
            self.refresh_sources()?;
            self.fork_with_state(self.export_state())?
        } else {
            Self::empty_with_hidden_tools((*self.hidden_tool_names).clone())
        };
        registry.upsert_overlay_source(Arc::new(ToolProviderGroupSource::new(
            "context",
            context_providers,
        )))?;
        Ok(registry)
    }

    pub(crate) fn upsert_source(
        &self,
        source: Arc<dyn ToolSourceExecutor>,
    ) -> Result<u64, ReconfigureError> {
        self.reconcile_source(source, SourceReconcilePolicy::RejectExternalConflicts)
    }

    pub(crate) fn remove_source_id(&self, source_id: &str) -> Result<u64, ReconfigureError> {
        {
            let mut sources = self.sources.write_recover();
            if sources.remove(source_id).is_none() {
                return Err(ReconfigureError::UnknownSource(source_id.to_string()));
            }
        }
        let mut state = self
            .state
            .write_recover();
        state
            .tools
            .retain(|_, entry| entry.binding.source_id() != Some(source_id));
        state.generation += 1;
        Ok(state.generation)
    }

    fn upsert_overlay_source(
        &self,
        source: Arc<dyn ToolSourceExecutor>,
    ) -> Result<u64, ReconfigureError> {
        self.reconcile_source(source, SourceReconcilePolicy::OverlayReplacingConflicts)
    }

    fn reconcile_source(
        &self,
        source: Arc<dyn ToolSourceExecutor>,
        policy: SourceReconcilePolicy,
    ) -> Result<u64, ReconfigureError> {
        let source_id = source.id().to_string();
        let mut sources = self
            .sources
            .read_recover()
            .iter()
            .map(|(id, source)| (id.clone(), Arc::clone(source)))
            .collect::<BTreeMap<_, _>>();
        sources.insert(source_id.clone(), Arc::clone(&source));
        let snapshot = self.export_state();
        let preferred_source_id = (policy == SourceReconcilePolicy::OverlayReplacingConflicts)
            .then_some(source_id.as_str());
        let reconciled = reconcile_tool_state_entries(
            snapshot.entries(),
            &sources,
            ReconcileMode::LiveSurface,
            preferred_source_id,
            &self.hidden_tool_names,
        )?;

        self.sources
            .write_recover()
            .insert(source_id, source);
        let mut state = self
            .state
            .write_recover();
        state.tools = reconciled.tools;
        if reconciled.changed {
            state.generation = reconciled_generation(state.generation, true)?;
        }
        Ok(state.generation)
    }

    pub(crate) fn refresh_sources(&self) -> Result<u64, ReconfigureError> {
        let sources = self
            .sources
            .read_recover()
            .iter()
            .map(|(id, source)| (id.clone(), Arc::clone(source)))
            .collect::<BTreeMap<_, _>>();
        let snapshot = self.export_state();
        let reconciled = reconcile_tool_state_entries(
            snapshot.entries(),
            &sources,
            ReconcileMode::LiveSurface,
            None,
            &self.hidden_tool_names,
        )?;
        let mut state = self
            .state
            .write_recover();
        state.tools = reconciled.tools;
        if reconciled.changed {
            state.generation = reconciled_generation(state.generation, true)?;
        }
        Ok(state.generation)
    }

    pub(crate) fn fork_with_state(&self, snapshot: ToolState) -> Result<Self, ReconfigureError> {
        let sources = self
            .sources
            .read_recover()
            .iter()
            .map(|(k, v)| (k.clone(), Arc::clone(v)))
            .collect::<BTreeMap<_, _>>();
        let rebound = reconcile_tool_state_entries(
            snapshot.entries(),
            &sources,
            ReconcileMode::LiveSurface,
            None,
            &self.hidden_tool_names,
        )?;
        let generation = reconciled_generation(snapshot.generation.max(1), rebound.changed)?;
        Ok(Self {
            sources: Arc::new(RwLock::new(sources)),
            state: Arc::new(RwLock::new(ToolRegistryState {
                generation,
                tools: rebound.tools,
                next_live_source_id: 0,
            })),
            hidden_tool_names: Arc::clone(&self.hidden_tool_names),
        })
    }
}

fn reconciled_generation(generation: u64, changed: bool) -> Result<u64, ReconfigureError> {
    if !changed {
        return Ok(generation);
    }
    generation.checked_add(1).ok_or_else(|| {
        ReconfigureError::Validation("tool registry generation overflow".to_string())
    })
}
