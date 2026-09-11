use super::*;

impl ToolRegistry {
    /// Builds a `ToolRegistry` from tool provider data for protocol and process-engine implementors
    /// while preparing or executing plugin and tool work.
    pub fn from_tool_provider(provider: Arc<dyn ToolProvider>) -> Result<Self, ReconfigureError> {
        let registry = Self::empty();
        registry.upsert_source(Arc::new(ToolProviderSource::new(
            PLUGIN_TOOL_SOURCE_ID,
            vec![provider],
        )))?;
        Ok(registry)
    }

    /// Build a registry from one leaf provider plus completed first-party
    /// orchestrating definitions. The two registration lanes must have
    /// disjoint tool ids.
    #[doc(hidden)]
    pub fn from_tool_provider_with_orchestrating_tools(
        provider: Arc<dyn ToolProvider>,
        orchestrating_tools: Vec<crate::tool_provider::orchestration::OrchestratingToolDef>,
    ) -> Result<Self, ReconfigureError> {
        Self::from_tool_registrations(
            vec![(PLUGIN_TOOL_SOURCE_ID.to_string(), vec![provider])],
            orchestrating_tools,
        )
    }

    #[cfg(test)]
    pub(crate) fn from_tool_providers(
        providers: Vec<Arc<dyn ToolProvider>>,
    ) -> Result<Self, ReconfigureError> {
        Self::from_tool_provider_sources(vec![(PLUGIN_TOOL_SOURCE_ID.to_string(), providers)])
    }

    #[cfg(test)]
    pub(crate) fn from_tool_provider_sources(
        sources: Vec<(String, Vec<Arc<dyn ToolProvider>>)>,
    ) -> Result<Self, ReconfigureError> {
        Self::from_tool_registrations(sources, Vec::new())
    }

    pub(crate) fn from_tool_registrations(
        sources: Vec<(String, Vec<Arc<dyn ToolProvider>>)>,
        orchestrating_tools: Vec<crate::tool_provider::orchestration::OrchestratingToolDef>,
    ) -> Result<Self, ReconfigureError> {
        let registry = Self::empty();
        for (source_id, providers) in sources {
            registry.upsert_source(Arc::new(ToolProviderSource::new(source_id, providers)))?;
        }
        for definition in orchestrating_tools {
            registry.upsert_source(Arc::new(OrchestratingToolSource::new(definition)))?;
        }
        Ok(registry)
    }

    pub(crate) fn empty() -> Self {
        Self {
            inner: Arc::new(RwLock::new(ToolRegistryInner {
                source_revision: 0,
                state_revision: 0,
                sources: BTreeMap::new(),
                state: ToolRegistryState {
                    generation: 0,
                    surface: ToolSurface::default(),
                    next_live_source_id: 0,
                },
            })),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.inner.read_recover().state.generation
    }

    pub(crate) fn is_orchestrating_tool(&self, tool_id: &ToolId) -> bool {
        self.inner
            .read_recover()
            .state
            .surface
            .get(tool_id)
            .is_some_and(|entry| entry.registration_kind() == ToolRegistrationKind::Orchestrating)
    }

    pub(crate) fn export_state(&self) -> ToolState {
        let authority = self.inner.read_recover();
        ToolState::new(
            authority.state.generation,
            export_tool_state_entries(&authority.state.surface),
        )
    }

    pub(crate) fn apply_state(&self, next: ToolState) -> Result<u64, ReconfigureError> {
        loop {
            let (source_revision, state_revision, current_generation, sources) = {
                let authority = self.inner.read_recover();
                (
                    authority.source_revision,
                    authority.state_revision,
                    authority.state.generation,
                    authority.sources.clone(),
                )
            };
            if next.generation != current_generation {
                return Err(ReconfigureError::GenerationMismatch {
                    expected: next.generation,
                    actual: current_generation,
                });
            }

            let rebound = match reconcile_tool_state_entries(
                next.entries(),
                &sources,
                ReconcileMode::SnapshotSurface,
                None,
            ) {
                Ok(rebound) => rebound,
                Err(error) => {
                    let inner = self.inner.read_recover();
                    if inner.source_revision != source_revision
                        || inner.state_revision != state_revision
                    {
                        continue;
                    }
                    if inner.state.generation != next.generation {
                        return Err(ReconfigureError::GenerationMismatch {
                            expected: next.generation,
                            actual: inner.state.generation,
                        });
                    }
                    return Err(error);
                }
            };

            let mut authority = self.inner.write_recover();
            if authority.source_revision != source_revision
                || authority.state_revision != state_revision
            {
                continue;
            }
            if authority.state.generation != next.generation {
                return Err(ReconfigureError::GenerationMismatch {
                    expected: next.generation,
                    actual: authority.state.generation,
                });
            }
            let generation = reconciled_generation(authority.state.generation, true)?;
            let next_state_revision = checked_state_revision(authority.state_revision)?;
            authority.state.surface = rebound.surface;
            authority.state.surface.debug_assert_invariant();
            authority.state.generation = generation;
            authority.state_revision = next_state_revision;
            return Ok(generation);
        }
    }

    /// Restore a persisted [`ToolState`] snapshot onto a freshly-built registry.
    ///
    /// Unlike [`apply_state`](Self::apply_state) — which applies an incremental
    /// *delta* expected at the current generation and bumps it by one — a
    /// restore rebuilds from the current live source surface and overlays the
    /// snapshot's per-id host curation. A byte-equivalent surface remains at `G`;
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
    /// Host curation is preserved per id. The live source also re-derives the
    /// registration lane, so snapshots written before lane persistence remain
    /// resumable. Newly advertised ids are members by default. Consequently an
    /// opt-out does not transfer when a provider replaces a tool with a new id,
    /// even if it reuses the same name. Multiple
    /// sources resolving the same id or advertised name still fail because
    /// execution authority and model-facing names must both be unambiguous.
    pub(crate) fn restore_state(
        &self,
        snapshot: ToolState,
    ) -> Result<ToolRestoreReport, ReconfigureError> {
        loop {
            let (source_revision, state_revision, state_generation, sources) = {
                let authority = self.inner.read_recover();
                (
                    authority.source_revision,
                    authority.state_revision,
                    authority.state.generation,
                    authority.sources.clone(),
                )
            };
            let rebound = match reconcile_tool_state_entries(
                snapshot.entries(),
                &sources,
                ReconcileMode::LiveSurface,
                None,
            ) {
                Ok(rebound) => rebound,
                Err(error) => {
                    if self.reconciliation_inputs_changed(
                        source_revision,
                        state_revision,
                        state_generation,
                    ) {
                        continue;
                    }
                    return Err(error);
                }
            };

            let mut authority = self.inner.write_recover();
            if authority.source_revision != source_revision
                || authority.state_revision != state_revision
                || authority.state.generation != state_generation
            {
                continue;
            }
            let generation = reconciled_generation(snapshot.generation(), rebound.changed)?;
            let next_state_revision = checked_state_revision(authority.state_revision)?;
            authority.state.surface = rebound.surface;
            authority.state.surface.debug_assert_invariant();
            authority.state.generation = generation;
            authority.state_revision = next_state_revision;
            return Ok(ToolRestoreReport {
                generation,
                orphaned: rebound.orphaned,
            });
        }
    }

    pub(crate) fn compose_session_catalog(
        &self,
        include_base_tools: bool,
        context_providers: Vec<Arc<dyn ToolProvider>>,
    ) -> Result<Self, ReconfigureError> {
        let registry = if include_base_tools {
            self.refresh_sources()?;
            self.pin_current_surface()
        } else {
            Self::empty()
        };
        registry.upsert_overlay_source(Arc::new(ToolProviderSource::new(
            "context",
            context_providers,
        )))?;
        Ok(registry)
    }

    /// Admit each live source once, compose the session overlay, and freeze
    /// the resulting surface for one model request.
    pub(crate) fn pin_session_surface(
        &self,
        include_base_tools: bool,
        context_providers: Vec<Arc<dyn ToolProvider>>,
    ) -> Result<Self, ReconfigureError> {
        let registry = self.compose_session_catalog(include_base_tools, context_providers)?;
        Ok(registry.pin_current_surface())
    }

    pub(crate) fn upsert_source(
        &self,
        source: Arc<dyn ToolSourceExecutor>,
    ) -> Result<u64, ReconfigureError> {
        self.reconcile_source(source)
    }

    pub(crate) fn remove_source_id(&self, source_id: &str) -> Result<u64, ReconfigureError> {
        let source_key = ToolSourceKey::Leaf(source_id.to_string());
        let mut authority = self.inner.write_recover();
        if !authority.sources.contains_key(&source_key) {
            return Err(ReconfigureError::UnknownSource(source_id.to_string()));
        }
        let source_revision = checked_source_revision(authority.source_revision)?;
        let mut surface = authority.state.surface.clone();
        let previous = export_tool_state_entries(&surface);
        let removed_ids = surface
            .by_id
            .iter()
            .filter(|(_, entry)| entry.binding.source_key() == Some(&source_key))
            .map(|(id, _)| id.clone())
            .collect::<Vec<_>>();
        for id in removed_ids {
            surface.remove(&id);
        }
        surface.debug_assert_invariant();
        let changed = export_tool_state_entries(&surface) != previous;
        let generation = reconciled_generation(authority.state.generation, changed)?;
        let state_revision = changed
            .then(|| checked_state_revision(authority.state_revision))
            .transpose()?;

        let retired = authority.sources.remove(&source_key);
        authority.source_revision = source_revision;
        if let Some(state_revision) = state_revision {
            authority.state.surface = surface;
            authority.state.generation = generation;
            authority.state_revision = state_revision;
        }
        drop(authority);
        drop(retired);
        Ok(generation)
    }

    fn upsert_overlay_source(
        &self,
        source: Arc<dyn ToolSourceExecutor>,
    ) -> Result<u64, ReconfigureError> {
        let source_key = source.source_key();
        let manifests = source
            .advertised_tools()
            .into_iter()
            .map(|manifest| manifest_with_compact_contract(source.as_ref(), manifest))
            .collect::<Vec<_>>();
        validate_unique_manifests(&manifests)?;

        loop {
            let (source_revision, state_revision, mut next_state) = {
                let authority = self.inner.read_recover();
                (
                    authority.source_revision,
                    authority.state_revision,
                    authority.state.clone(),
                )
            };
            let rebuilt = (|| {
                let curated = next_state
                    .surface
                    .by_id
                    .iter()
                    .map(|(id, entry)| (id.clone(), entry.member))
                    .collect::<BTreeMap<_, _>>();
                let previous = export_tool_state_entries(&next_state.surface);
                for manifest in manifests.iter().cloned() {
                    let id = manifest.id.clone();
                    insert_advertised_entry(
                        &mut next_state.surface,
                        &source_key,
                        source.registration_kind(),
                        manifest,
                        Some(&source_key),
                    )?;
                    if let Some(member) = curated.get(&id)
                        && let Some(entry) = next_state.surface.get_mut(&id)
                    {
                        entry.member = *member;
                    }
                }
                next_state.surface.debug_assert_invariant();
                Ok::<_, ReconfigureError>(
                    export_tool_state_entries(&next_state.surface) != previous,
                )
            })();
            let changed = match rebuilt {
                Ok(changed) => changed,
                Err(error) => {
                    if self.reconciliation_inputs_changed(
                        source_revision,
                        state_revision,
                        next_state.generation,
                    ) {
                        continue;
                    }
                    return Err(error);
                }
            };

            let mut authority = self.inner.write_recover();
            if authority.source_revision != source_revision
                || authority.state_revision != state_revision
                || authority.state.generation != next_state.generation
            {
                continue;
            }
            let next_source_revision = checked_source_revision(authority.source_revision)?;
            let generation = reconciled_generation(authority.state.generation, changed)?;
            let next_state_revision = changed
                .then(|| checked_state_revision(authority.state_revision))
                .transpose()?;
            let retired = authority
                .sources
                .insert(source_key.clone(), Arc::clone(&source));
            authority.source_revision = next_source_revision;
            if let Some(next_state_revision) = next_state_revision {
                authority.state.surface = next_state.surface;
                authority.state.generation = generation;
                authority.state_revision = next_state_revision;
            }
            drop(authority);
            drop(retired);
            return Ok(generation);
        }
    }

    fn reconcile_source(
        &self,
        source: Arc<dyn ToolSourceExecutor>,
    ) -> Result<u64, ReconfigureError> {
        let source_key = source.source_key();
        loop {
            let (source_revision, state_revision, mut sources, snapshot) = {
                let authority = self.inner.read_recover();
                (
                    authority.source_revision,
                    authority.state_revision,
                    authority.sources.clone(),
                    ToolState::new(
                        authority.state.generation,
                        export_tool_state_entries(&authority.state.surface),
                    ),
                )
            };
            if matches!(source_key, ToolSourceKey::Orchestrating(_))
                && sources.contains_key(&source_key)
            {
                if self.reconciliation_inputs_changed(
                    source_revision,
                    state_revision,
                    snapshot.generation,
                ) {
                    continue;
                }
                return Err(ReconfigureError::Validation(format!(
                    "duplicate orchestrating tool source `{source_key}`"
                )));
            }
            sources.insert(source_key.clone(), Arc::clone(&source));
            let reconciled = match reconcile_tool_state_entries(
                snapshot.entries(),
                &sources,
                ReconcileMode::LiveSurface,
                None,
            ) {
                Ok(reconciled) => reconciled,
                Err(error) => {
                    if self.reconciliation_inputs_changed(
                        source_revision,
                        state_revision,
                        snapshot.generation,
                    ) {
                        continue;
                    }
                    return Err(error);
                }
            };

            let mut authority = self.inner.write_recover();
            if authority.source_revision != source_revision
                || authority.state_revision != state_revision
                || authority.state.generation != snapshot.generation
            {
                continue;
            }
            let next_source_revision = checked_source_revision(authority.source_revision)?;
            let generation = reconciled_generation(authority.state.generation, reconciled.changed)?;
            let next_state_revision = reconciled
                .changed
                .then(|| checked_state_revision(authority.state_revision))
                .transpose()?;
            let retired = authority
                .sources
                .insert(source_key.clone(), Arc::clone(&source));
            authority.source_revision = next_source_revision;
            if let Some(next_state_revision) = next_state_revision {
                authority.state.surface = reconciled.surface;
                authority.state.surface.debug_assert_invariant();
                authority.state.generation = generation;
                authority.state_revision = next_state_revision;
            }
            drop(authority);
            drop(retired);
            return Ok(generation);
        }
    }

    pub(crate) fn refresh_sources(&self) -> Result<u64, ReconfigureError> {
        // This is the explicit admission seam for live surface changes. Source
        // advertisements are enumerated and reconciled here; dispatch lookup
        // only reads the admitted surface.
        loop {
            let (source_revision, state_revision, sources, snapshot) = {
                let authority = self.inner.read_recover();
                (
                    authority.source_revision,
                    authority.state_revision,
                    authority.sources.clone(),
                    ToolState::new(
                        authority.state.generation,
                        export_tool_state_entries(&authority.state.surface),
                    ),
                )
            };
            let reconciled = match reconcile_tool_state_entries(
                snapshot.entries(),
                &sources,
                ReconcileMode::LiveSurface,
                None,
            ) {
                Ok(reconciled) => reconciled,
                Err(error) => {
                    if self.reconciliation_inputs_changed(
                        source_revision,
                        state_revision,
                        snapshot.generation,
                    ) {
                        continue;
                    }
                    return Err(error);
                }
            };

            let mut authority = self.inner.write_recover();
            if authority.source_revision != source_revision
                || authority.state_revision != state_revision
                || authority.state.generation != snapshot.generation
            {
                continue;
            }
            let generation = reconciled_generation(authority.state.generation, reconciled.changed)?;
            if reconciled.changed {
                let next_state_revision = checked_state_revision(authority.state_revision)?;
                authority.state.surface = reconciled.surface;
                authority.state.surface.debug_assert_invariant();
                authority.state.generation = generation;
                authority.state_revision = next_state_revision;
            }
            return Ok(generation);
        }
    }

    fn pin_current_surface(&self) -> Self {
        let authority = self.inner.read_recover().clone();
        Self {
            inner: Arc::new(RwLock::new(authority)),
        }
    }

    fn reconciliation_inputs_changed(
        &self,
        source_revision: u64,
        state_revision: u64,
        generation: u64,
    ) -> bool {
        let inner = self.inner.read_recover();
        inner.source_revision != source_revision
            || inner.state_revision != state_revision
            || inner.state.generation != generation
    }

    pub(crate) fn fork_with_state(&self, snapshot: ToolState) -> Result<Self, ReconfigureError> {
        let sources = self.inner.read_recover().sources.clone();
        let rebound = reconcile_tool_state_entries(
            snapshot.entries(),
            &sources,
            ReconcileMode::LiveSurface,
            None,
        )?;
        let generation = reconciled_generation(snapshot.generation.max(1), rebound.changed)?;
        Ok(Self {
            inner: Arc::new(RwLock::new(ToolRegistryInner {
                source_revision: 0,
                state_revision: 0,
                sources,
                state: ToolRegistryState {
                    generation,
                    surface: rebound.surface,
                    next_live_source_id: 0,
                },
            })),
        })
    }
}

fn checked_source_revision(source_revision: u64) -> Result<u64, ReconfigureError> {
    source_revision.checked_add(1).ok_or_else(|| {
        ReconfigureError::Validation("tool registry source revision overflow".to_string())
    })
}

pub(super) fn checked_state_revision(state_revision: u64) -> Result<u64, ReconfigureError> {
    state_revision.checked_add(1).ok_or_else(|| {
        ReconfigureError::Validation("tool registry state revision overflow".to_string())
    })
}

fn reconciled_generation(generation: u64, changed: bool) -> Result<u64, ReconfigureError> {
    if !changed {
        return Ok(generation);
    }
    generation.checked_add(1).ok_or_else(|| {
        ReconfigureError::Validation("tool registry generation overflow".to_string())
    })
}

#[cfg(test)]
#[path = "admission_tests.rs"]
mod admission_tests;
