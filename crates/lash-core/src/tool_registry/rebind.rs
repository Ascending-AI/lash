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
    entries: &BTreeMap<ToolId, ToolRegistryEntry>,
) -> BTreeMap<ToolId, ToolStateEntry> {
    entries
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
    tools: BTreeMap<ToolId, ToolRegistryEntry>,
    orphaned: Vec<ToolId>,
    changed: bool,
}

/// Reconcile live sources with persisted per-id state at the one registry seam.
///
/// `preferred_source_id` is used only by the context-catalog adapter, whose
/// documented semantics replace base tools that collide by id or model-facing
/// name. All ordinary live-source collisions are rejected.
fn reconcile_tool_state_entries(
    entries: &BTreeMap<ToolId, ToolStateEntry>,
    sources: &BTreeMap<String, Arc<dyn ToolSourceExecutor>>,
    mode: ReconcileMode,
    preferred_source_id: Option<&str>,
    hidden_tool_names: &BTreeSet<String>,
) -> Result<ReconciledTools, ReconfigureError> {
    validate_snapshot_entries(entries)?;

    let mut reconciled = match mode {
        ReconcileMode::LiveSurface => {
            advertised_tool_entries(sources, preferred_source_id, hidden_tool_names)?
        }
        ReconcileMode::SnapshotSurface => BTreeMap::new(),
    };
    let mut orphaned = Vec::new();

    for (id, stored) in entries {
        if let Some(live) = reconciled.get_mut(id) {
            live.member = stored.member && !hidden_tool_names.contains(&live.manifest.name);
            continue;
        }

        let resolved = resolve_snapshot_id(id, sources, preferred_source_id)?;
        match resolved {
            Some((source_id, manifest)) => {
                let mut entry = bound_tool_entry(manifest, source_id, hidden_tool_names);
                entry.member &= stored.member;
                insert_result_entry(&mut reconciled, id.clone(), entry)?;
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
                    && reconciled
                        .values()
                        .any(|entry| entry.manifest.name == stored.manifest.name)
                {
                    continue;
                }
                orphaned.push(id.clone());
                let mut orphan = ToolRegistryEntry::orphaned(stored.manifest.clone());
                orphan.member =
                    stored.member && !hidden_tool_names.contains(&orphan.manifest.name);
                insert_result_entry(&mut reconciled, id.clone(), orphan)?;
            }
        }
    }

    let changed = export_tool_state_entries(&reconciled) != *entries;
    Ok(ReconciledTools {
        tools: reconciled,
        orphaned,
        changed,
    })
}

fn advertised_tool_entries(
    sources: &BTreeMap<String, Arc<dyn ToolSourceExecutor>>,
    preferred_source_id: Option<&str>,
    hidden_tool_names: &BTreeSet<String>,
) -> Result<BTreeMap<ToolId, ToolRegistryEntry>, ReconfigureError> {
    let mut advertised = BTreeMap::new();
    for (source_id, source) in sources {
        let manifests = source
            .advertised_tools()
            .into_iter()
            .map(|manifest| manifest_with_compact_contract(source.as_ref(), manifest))
            .collect::<Vec<_>>();
        validate_unique_manifests(&manifests)?;
        validate_reserved_orchestration_manifests(source_id, &manifests)?;
        for manifest in manifests {
            insert_advertised_entry(
                &mut advertised,
                source_id,
                manifest,
                preferred_source_id,
                hidden_tool_names,
            )?;
        }
    }
    Ok(advertised)
}

fn validate_reserved_orchestration_manifests(
    source_id: &str,
    manifests: &[ToolManifest],
) -> Result<(), ReconfigureError> {
    for manifest in manifests {
        let Some(required_source_id) =
            crate::tool_provider::first_party_orchestration_source_id(&manifest.id)
        else {
            continue;
        };
        if source_id != required_source_id {
            return Err(ReconfigureError::ReservedOrchestrationToolId {
                tool_id: manifest.id.clone(),
                source_id: source_id.to_string(),
                required_source_id,
            });
        }
    }
    Ok(())
}

fn insert_advertised_entry(
    advertised: &mut BTreeMap<ToolId, ToolRegistryEntry>,
    source_id: &str,
    manifest: ToolManifest,
    preferred_source_id: Option<&str>,
    hidden_tool_names: &BTreeSet<String>,
) -> Result<(), ReconfigureError> {
    let id_conflict = advertised.get(&manifest.id).map(|entry| {
        (
            manifest.id.clone(),
            entry
                .binding
                .source_id()
                .expect("advertised entries are bound")
                .to_string(),
        )
    });
    let name_conflict = advertised.iter().find_map(|(id, entry)| {
        (entry.manifest.name == manifest.name).then(|| {
            (
                id.clone(),
                entry
                    .binding
                    .source_id()
                    .expect("advertised entries are bound")
                    .to_string(),
            )
        })
    });

    let conflicts = [id_conflict.as_ref(), name_conflict.as_ref()]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    if !conflicts.is_empty() {
        if preferred_source_id == Some(source_id) {
            for (id, _) in conflicts {
                advertised.remove(id);
            }
        } else if conflicts
            .iter()
            .any(|(_, owner)| preferred_source_id == Some(owner.as_str()))
        {
            return Ok(());
        } else if let Some((_, owner)) = id_conflict {
            return Err(ReconfigureError::Validation(format!(
                "duplicate tool id `{}` from source `{source_id}` conflicts with source `{owner}`",
                manifest.id
            )));
        } else if let Some((id, owner)) = name_conflict {
            return Err(ReconfigureError::Validation(format!(
                "duplicate tool name `{}` from source `{source_id}` conflicts with tool id `{id}` from source `{owner}`",
                manifest.name
            )));
        }
    }

    let entry = bound_tool_entry(manifest, source_id, hidden_tool_names);
    advertised.insert(entry.manifest.id.clone(), entry);
    Ok(())
}

fn bound_tool_entry(
    manifest: ToolManifest,
    source_id: impl Into<String>,
    hidden_tool_names: &BTreeSet<String>,
) -> ToolRegistryEntry {
    let mut entry = ToolRegistryEntry::new(manifest, source_id);
    entry.member = !hidden_tool_names.contains(&entry.manifest.name);
    entry
}

fn resolve_snapshot_id(
    id: &ToolId,
    sources: &BTreeMap<String, Arc<dyn ToolSourceExecutor>>,
    preferred_source_id: Option<&str>,
) -> Result<Option<(String, ToolManifest)>, ReconfigureError> {
    let mut matches = Vec::new();
    for (source_id, source) in sources {
        let Some(manifest) = source.resolve_manifest_by_id(id) else {
            continue;
        };
        validate_reserved_orchestration_manifests(source_id, std::slice::from_ref(&manifest))?;
        if manifest.id != *id {
            return Err(ReconfigureError::Validation(format!(
                "source `{source_id}` resolved tool id `{id}` with mismatched manifest id `{}`",
                manifest.id
            )));
        }
        matches.push((
            source_id.clone(),
            manifest_with_compact_contract(source.as_ref(), manifest),
        ));
    }

    if matches.len() <= 1 {
        return Ok(matches.pop());
    }
    if let Some(preferred_source_id) = preferred_source_id
        && let Some(preferred) = matches
            .into_iter()
            .find(|(source_id, _)| source_id == preferred_source_id)
    {
        return Ok(Some(preferred));
    }
    Err(ReconfigureError::Validation(format!(
        "tool id `{id}` is resolved by multiple registered sources"
    )))
}

fn insert_result_entry(
    reconciled: &mut BTreeMap<ToolId, ToolRegistryEntry>,
    id: ToolId,
    entry: ToolRegistryEntry,
) -> Result<(), ReconfigureError> {
    if id != entry.manifest.id {
        return Err(ReconfigureError::Validation(format!(
            "tool state key `{id}` does not match manifest id `{}`",
            entry.manifest.id
        )));
    }
    if let Some((existing_id, existing)) = reconciled
        .iter()
        .find(|(_, existing)| existing.manifest.name == entry.manifest.name)
    {
        return Err(ReconfigureError::Validation(format!(
            "duplicate tool name `{}` for tool ids `{existing_id}` and `{id}`",
            existing.manifest.name
        )));
    }
    if reconciled.insert(id.clone(), entry).is_some() {
        return Err(ReconfigureError::Validation(format!(
            "duplicate tool id `{id}` in reconciled surface"
        )));
    }
    Ok(())
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
