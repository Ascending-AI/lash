fn validate_unique_manifests<'a>(
    manifests: impl IntoIterator<Item = &'a ToolManifest>,
) -> Result<(), ReconfigureError> {
    let mut names = BTreeSet::new();
    let mut ids = BTreeSet::new();
    for manifest in manifests {
        if manifest.id.as_str().trim().is_empty() {
            return Err(ReconfigureError::Validation(
                "tool id cannot be empty".to_string(),
            ));
        }
        if !ids.insert(&manifest.id) {
            return Err(ReconfigureError::Validation(format!(
                "duplicate tool id `{}` in source",
                manifest.id
            )));
        }
        if manifest.name.trim().is_empty() {
            return Err(ReconfigureError::Validation(
                "tool name cannot be empty".to_string(),
            ));
        }
        if !names.insert(manifest.name.as_str()) {
            return Err(ReconfigureError::Validation(format!(
                "duplicate tool name `{}` in source",
                manifest.name
            )));
        }
    }
    Ok(())
}

fn manifest_with_compact_contract(
    source: &dyn ToolSourceExecutor,
    mut manifest: ToolManifest,
) -> ToolManifest {
    if manifest.compact_contract.is_none()
        && let Some(contract) = source.resolve_contract_by_id(&manifest.id)
    {
        manifest.compact_contract = Some(contract.compact_contract(&manifest));
    }
    manifest
}

fn export_tool_state_entries(
    surface: &ToolSurface,
) -> BTreeMap<ToolId, ToolStateEntry> {
    surface
        .by_id
        .iter()
        .map(|(id, entry)| (id.clone(), entry.export()))
        .collect()
}

/// Which side defines the set of ids at the registry's reconciliation seam.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReconcileMode {
    /// Automatic rebuilds: live advertisements define the surface and the
    /// snapshot overlays per-id curation. Snapshot-only ids are resolved
    /// lazily or retained as orphans.
    LiveSurface,
    /// Explicit `apply_state`: the host-provided snapshot defines the surface;
    /// deletion is an intentional generation-fenced delta.
    SnapshotSurface,
}

struct ReconciledTools {
    surface: ToolSurface,
    orphaned: Vec<ToolId>,
    changed: bool,
}

/// Reconcile live sources with persisted per-id state at the one registry seam.
///
/// `preferred_source_key` is used only by the context-catalog adapter, whose
/// documented semantics replace base tools that collide by id or model-facing
/// name. All ordinary live-source collisions are rejected.
fn reconcile_tool_state_entries(
    entries: &BTreeMap<ToolId, ToolStateEntry>,
    sources: &BTreeMap<ToolSourceKey, Arc<dyn ToolSourceExecutor>>,
    mode: ReconcileMode,
    preferred_source_key: Option<&ToolSourceKey>,
    hidden_tool_names: &BTreeSet<String>,
) -> Result<ReconciledTools, ReconfigureError> {
    validate_snapshot_entries(entries)?;

    let mut surface = match mode {
        ReconcileMode::LiveSurface => {
            advertised_tool_entries(sources, preferred_source_key, hidden_tool_names)?
        }
        ReconcileMode::SnapshotSurface => ToolSurface::default(),
    };
    let mut orphaned = Vec::new();

    for (id, stored) in entries {
        if let Some(live) = surface.get_mut(id) {
            live.member = stored.member && !hidden_tool_names.contains(&live.manifest.name);
            continue;
        }

        let resolved = resolve_snapshot_id(id, sources, preferred_source_key)?;
        match resolved {
            Some((source_key, manifest, kind)) => {
                let mut entry = bound_tool_entry(manifest, source_key, kind, hidden_tool_names);
                entry.member &= stored.member;
                insert_result_entry(&mut surface, id.clone(), entry)?;
            }
            None if mode == ReconcileMode::SnapshotSurface && !stored.orphaned => {
                return Err(ReconfigureError::Validation(format!(
                    "no registered tool source resolves tool id `{id}`"
                )));
            }
            None => {
                // A live tool now owns this model-facing alias under a new id.
                // The old authority grant is not transferred and its orphan is
                // superseded, while the new id remains a default member.
                if mode == ReconcileMode::LiveSurface
                    && surface.get_by_name(&stored.manifest.name).is_some()
                {
                    continue;
                }
                orphaned.push(id.clone());
                let mut orphan = ToolRegistryEntry::orphaned(
                    stored.manifest.clone(),
                    stored.registration_kind,
                );
                orphan.member =
                    stored.member && !hidden_tool_names.contains(&orphan.manifest.name);
                insert_result_entry(&mut surface, id.clone(), orphan)?;
            }
        }
    }

    let changed = export_tool_state_entries(&surface) != *entries;
    Ok(ReconciledTools {
        surface,
        orphaned,
        changed,
    })
}

fn advertised_tool_entries(
    sources: &BTreeMap<ToolSourceKey, Arc<dyn ToolSourceExecutor>>,
    preferred_source_key: Option<&ToolSourceKey>,
    hidden_tool_names: &BTreeSet<String>,
) -> Result<ToolSurface, ReconfigureError> {
    let mut advertised = ToolSurface::default();
    for (source_key, source) in sources {
        let manifests = source
            .advertised_tools()
            .into_iter()
            .map(|manifest| manifest_with_compact_contract(source.as_ref(), manifest))
            .collect::<Vec<_>>();
        validate_unique_manifests(&manifests)?;
        for manifest in manifests {
            insert_advertised_entry(
                &mut advertised,
                source_key,
                source.registration_kind(),
                manifest,
                preferred_source_key,
                hidden_tool_names,
            )?;
        }
    }
    Ok(advertised)
}

fn insert_advertised_entry(
    advertised: &mut ToolSurface,
    source_key: &ToolSourceKey,
    kind: ToolRegistrationKind,
    manifest: ToolManifest,
    preferred_source_key: Option<&ToolSourceKey>,
    hidden_tool_names: &BTreeSet<String>,
) -> Result<(), ReconfigureError> {
    let manifest_id = manifest.id.clone();
    let id_conflict = advertised.get(&manifest.id).map(|entry| {
        (
            manifest.id.clone(),
            entry
                .binding
                .source_key()
                .expect("advertised entries are bound")
                .clone(),
            entry.registration_kind(),
        )
    });
    let name_conflict = advertised.get_by_name(&manifest.name).map(|(id, entry)| {
        (
            id.clone(),
            entry
                .binding
                .source_key()
                .expect("advertised entries are bound")
                .clone(),
        )
    });

    if let Some((tool_id, owner, existing_kind)) = id_conflict.as_ref()
        && *existing_kind != kind
    {
        let leaf_source_id = if kind == ToolRegistrationKind::Leaf {
            source_key.to_string()
        } else {
            owner.to_string()
        };
        return Err(ReconfigureError::CrossLaneToolIdCollision {
            tool_id: tool_id.clone(),
            leaf_source_id,
        });
    }

    let conflicts = [
        id_conflict.as_ref().map(|(id, _owner, _)| id.clone()),
        name_conflict.as_ref().map(|(id, _owner)| id.clone()),
    ]
        .into_iter()
        .flatten()
        .collect::<BTreeSet<_>>();
    if !conflicts.is_empty() {
        if preferred_source_key == Some(source_key) {
            for id in conflicts {
                advertised.remove(&id);
            }
        } else if id_conflict
            .as_ref()
            .is_some_and(|(_, owner, _)| preferred_source_key == Some(owner))
            || name_conflict
                .as_ref()
                .is_some_and(|(_, owner)| preferred_source_key == Some(owner))
        {
            return Ok(());
        }
    }

    let entry = bound_tool_entry(manifest, source_key.clone(), kind, hidden_tool_names);
    match advertised.insert(entry) {
        Ok(()) => Ok(()),
        Err(ToolSurfaceInsertError::DuplicateId) => {
            let (_, owner, _) = id_conflict.expect("surface id conflict was indexed");
            Err(ReconfigureError::Validation(format!(
                "duplicate tool id `{}` from source `{source_key}` conflicts with source `{owner}`",
                manifest_id
            )))
        }
        Err(ToolSurfaceInsertError::DuplicateName { name }) => {
            let (id, owner) = name_conflict.expect("surface name conflict was indexed");
            Err(ReconfigureError::Validation(format!(
                "duplicate tool name `{name}` from source `{source_key}` conflicts with tool id `{id}` from source `{owner}`"
            )))
        }
    }
}

fn bound_tool_entry(
    manifest: ToolManifest,
    source_key: ToolSourceKey,
    kind: ToolRegistrationKind,
    hidden_tool_names: &BTreeSet<String>,
) -> ToolRegistryEntry {
    let mut entry = ToolRegistryEntry::new(manifest, source_key, kind);
    entry.member = !hidden_tool_names.contains(&entry.manifest.name);
    entry
}

fn resolve_snapshot_id(
    id: &ToolId,
    sources: &BTreeMap<ToolSourceKey, Arc<dyn ToolSourceExecutor>>,
    preferred_source_key: Option<&ToolSourceKey>,
) -> Result<Option<(ToolSourceKey, ToolManifest, ToolRegistrationKind)>, ReconfigureError> {
    let mut matches = Vec::new();
    for (source_key, source) in sources {
        let Some(manifest) = source.resolve_manifest_by_id(id) else {
            continue;
        };
        if manifest.id != *id {
            return Err(ReconfigureError::Validation(format!(
                "source `{source_key}` resolved tool id `{id}` with mismatched manifest id `{}`",
                manifest.id
            )));
        }
        matches.push((
            source_key.clone(),
            manifest_with_compact_contract(source.as_ref(), manifest),
            source.registration_kind(),
        ));
    }

    if matches
        .iter()
        .any(|(_, _, kind)| *kind == ToolRegistrationKind::Leaf)
        && matches
            .iter()
            .any(|(_, _, kind)| *kind == ToolRegistrationKind::Orchestrating)
    {
        let leaf_source_id = matches
            .iter()
            .find(|(_, _, kind)| *kind == ToolRegistrationKind::Leaf)
            .map(|(source_key, _, _)| source_key.to_string())
            .expect("mixed registration kinds include a leaf source");
        return Err(ReconfigureError::CrossLaneToolIdCollision {
            tool_id: id.clone(),
            leaf_source_id,
        });
    }
    if matches.len() <= 1 {
        return Ok(matches.pop());
    }
    if let Some(preferred_source_key) = preferred_source_key
        && let Some(preferred) = matches
            .into_iter()
            .find(|(source_key, _, _)| source_key == preferred_source_key)
    {
        return Ok(Some(preferred));
    }
    Err(ReconfigureError::Validation(format!(
        "tool id `{id}` is resolved by multiple registered sources"
    )))
}

fn insert_result_entry(
    surface: &mut ToolSurface,
    id: ToolId,
    entry: ToolRegistryEntry,
) -> Result<(), ReconfigureError> {
    if id != entry.manifest.id {
        return Err(ReconfigureError::Validation(format!(
            "tool state key `{id}` does not match manifest id `{}`",
            entry.manifest.id
        )));
    }
    let name_conflict = surface
        .get_by_name(&entry.manifest.name)
        .map(|(existing_id, _)| existing_id.clone());
    if let Some(existing_id) = name_conflict {
        return Err(ReconfigureError::Validation(format!(
            "duplicate tool name `{}` for tool ids `{existing_id}` and `{id}`",
            entry.manifest.name
        )));
    }
    match surface.insert(entry) {
        Ok(()) => Ok(()),
        Err(ToolSurfaceInsertError::DuplicateId) => Err(ReconfigureError::Validation(
            format!("duplicate tool id `{id}` in reconciled surface"),
        )),
        Err(ToolSurfaceInsertError::DuplicateName { .. }) => {
            unreachable!("surface name conflicts were checked before insertion")
        }
    }
}

fn validate_snapshot_entries(
    entries: &BTreeMap<ToolId, ToolStateEntry>,
) -> Result<(), ReconfigureError> {
    for (id, entry) in entries {
        if id != &entry.manifest.id {
            return Err(ReconfigureError::Validation(format!(
                "tool state key `{id}` does not match manifest id `{}`",
                entry.manifest.id
            )));
        }
    }
    validate_unique_manifest_entries(entries.values())
}

fn validate_unique_manifest_entries<'a>(
    entries: impl IntoIterator<Item = &'a ToolStateEntry>,
) -> Result<(), ReconfigureError> {
    validate_unique_manifests(entries.into_iter().map(ToolStateEntry::stored_manifest))
}
