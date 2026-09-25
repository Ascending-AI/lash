//! A code cell's journaled ambient binding set (FIG-3587).
//!
//! A cell links its host tool calls against the ambient Tool Catalog. The
//! results of those calls are journaled as tool attempts, but which tool each
//! call path named — its manifest and contract — is decided by the live
//! registry at link time. A redrive after the registry changed would link the
//! cell against another surface than the pass that wrote its journal: a
//! removed tool fails to link, a changed one may validate or route its call
//! differently, and in either case the cell no longer replays what it
//! recorded.
//!
//! So before a cell's first effect its executor journals the binding set the
//! cell resolved: each referenced ambient call path and the full definition of
//! the tool the catalog bound there. A redrive is served that record and
//! links against it, not against the live registry. A binding whose live
//! tool is missing or changed is *drifted*: calls on it are served only from
//! their recorded results, and one that would reach the tool live refuses
//! with [`lash_core::RuntimeErrorCode::LashlangCellBindingDrift`], which
//! parks the turn.

use std::collections::{BTreeMap, BTreeSet};

use crate::required_tool_typescript_executable;

/// The journal suffix under an exec effect's replay key.
const CELL_TOOL_BINDINGS_SUFFIX: &str = "cell-tool-bindings";

/// The operation label the binding record journals under; it names the
/// referenced call paths, so a redrive of other source cannot read it.
const CELL_TOOL_BINDINGS_OPERATION: &str = "cell_tool_bindings:v1";

/// How a drifted binding's live tool differs from its recorded one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellBindingDriftKind {
    /// The live registry holds no tool with the recorded tool's id.
    Missing,
    /// The live registry's tool with the recorded id has another definition.
    Changed,
}

impl CellBindingDriftKind {
    fn describe(self) -> &'static str {
        match self {
            Self::Missing => "missing from",
            Self::Changed => "changed in",
        }
    }
}

/// One binding of a redriven cell whose live tool drifted from its record.
#[derive(Clone, Debug)]
pub struct CellBindingDrift {
    pub path: String,
    pub kind: CellBindingDriftKind,
    pub recorded: lash_core::ToolDefinition,
}

impl CellBindingDrift {
    /// The refusal a call on this binding meets when the journal cannot serve
    /// it: names the binding, its tool, and whether it is missing or changed.
    pub fn refusal(&self) -> lash_core::RuntimeEffectControllerError {
        lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::LashlangCellBindingDrift,
            format!(
                "code cell binding `{}` (tool `{}`) is {} the live tool registry since the \
                 cell was journaled; its journal serves only recorded results, and this call \
                 would reach the tool live, so nothing was dispatched",
                self.path,
                self.recorded.manifest.id,
                self.kind.describe(),
            ),
        )
    }

    /// The recorded tool, as the grant a replayed call is authorized under.
    pub fn recorded_binding(&self) -> lash_core::ToolExecutionGrant {
        lash_core::ToolExecutionGrant::from_definition(self.recorded.clone())
    }
}

/// A cell's journaled binding set, compared against the live registry.
#[derive(Clone, Debug, Default)]
pub struct CellToolBindings {
    /// Every referenced ambient call path the record decides: the recorded
    /// definition bound there, or `None` for a path the catalog did not bind.
    recorded: BTreeMap<String, Option<lash_core::ToolDefinition>>,
    /// Drifted bindings, by the recorded tool's id.
    drifted: BTreeMap<lash_core::ToolId, CellBindingDrift>,
}

impl CellToolBindings {
    /// The drift of the binding a call on `tool_id` names, if it drifted.
    pub fn drift_for(&self, tool_id: &lash_core::ToolId) -> Option<&CellBindingDrift> {
        self.drifted.get(tool_id)
    }

    #[cfg(test)]
    pub(crate) fn has_drift(&self) -> bool {
        !self.drifted.is_empty()
    }

    /// The catalog the cell links against: the live catalog with every call
    /// path the record decides replaced by what it records — the recorded
    /// definition, or nothing.
    pub fn link_catalog(&self, live: &lash_core::ToolCatalog) -> lash_core::ToolCatalog {
        // A live tool at a recorded path that is not the recorded tool — a
        // path the live pass found unbound, or another tool now claiming a
        // bound one — would link differently than the record.
        let foreign_at_recorded_path = || {
            live.tools.iter().any(|entry| {
                binding_path(&entry.manifest).is_some_and(|path| match self.recorded.get(&path) {
                    Some(None) => true,
                    Some(Some(recorded)) => recorded.manifest.id != entry.manifest.id,
                    None => false,
                })
            })
        };
        // An undrifted record is the live catalog at every path it decides.
        if self.drifted.is_empty() && !foreign_at_recorded_path() {
            return live.clone();
        }
        let recorded_ids = self
            .recorded
            .values()
            .flatten()
            .map(|definition| definition.manifest.id.clone())
            .collect::<BTreeSet<_>>();
        let mut definitions = live
            .tools
            .iter()
            .filter(|entry| {
                !recorded_ids.contains(&entry.manifest.id)
                    && binding_path(&entry.manifest)
                        .is_none_or(|path| !self.recorded.contains_key(&path))
            })
            .map(|entry| lash_core::ToolDefinition {
                manifest: entry.manifest.clone(),
                contract: (*entry.contract).clone(),
            })
            .collect::<Vec<_>>();
        definitions.extend(self.recorded.values().flatten().cloned());
        lash_core::ToolCatalog::from_tool_definitions(definitions)
    }
}

fn binding_path(manifest: &lash_core::ToolManifest) -> Option<String> {
    let binding = required_tool_typescript_executable(manifest).ok()?;
    Some(format!(
        "{}.{}",
        binding.module_path.join("."),
        binding.operation
    ))
}

/// The referenced call paths the live catalog binds, outside `excluded` (the
/// paths a deferred resolution already records), with each bound tool's full
/// definition as JSON; unbound paths map to `null`.
fn resolve_ambient_bindings(
    referenced: &BTreeSet<String>,
    live: &lash_core::ToolCatalog,
    excluded: &BTreeSet<String>,
) -> Result<serde_json::Value, serde_json::Error> {
    let mut bound = BTreeMap::new();
    for entry in &live.tools {
        if entry.manifest.activation == lash_core::ToolActivation::Internal {
            continue;
        }
        let Some(path) = binding_path(&entry.manifest) else {
            continue;
        };
        if referenced.contains(&path) && !excluded.contains(&path) {
            bound.entry(path).or_insert(entry);
        }
    }
    let mut record = serde_json::Map::new();
    for path in referenced.iter().filter(|path| !excluded.contains(*path)) {
        let value = match bound.get(path) {
            Some(entry) => serde_json::to_value(lash_core::ToolDefinition {
                manifest: entry.manifest.clone(),
                contract: (*entry.contract).clone(),
            })?,
            None => serde_json::Value::Null,
        };
        record.insert(path.clone(), value);
    }
    Ok(serde_json::Value::Object(record))
}

/// Compares a served binding record against the live registry.
fn compare(
    record: serde_json::Value,
    live: &lash_core::ToolCatalog,
) -> Result<CellToolBindings, lash_core::RuntimeEffectControllerError> {
    let invalid = |detail: String| {
        lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::RecordEncodingFailed,
            format!("journaled cell tool binding set is unreadable: {detail}"),
        )
    };
    let serde_json::Value::Object(entries) = record else {
        return Err(invalid("not an object".to_string()));
    };
    let live_by_id = live
        .tools
        .iter()
        .map(|entry| (entry.manifest.id.clone(), entry))
        .collect::<BTreeMap<_, _>>();
    let mut bindings = CellToolBindings::default();
    for (path, value) in entries {
        if value.is_null() {
            bindings.recorded.insert(path, None);
            continue;
        }
        let recorded: lash_core::ToolDefinition =
            serde_json::from_value(value.clone()).map_err(|error| invalid(error.to_string()))?;
        let kind = match live_by_id.get(&recorded.manifest.id) {
            None => Some(CellBindingDriftKind::Missing),
            // Drift is judged on what decides linking and dispatch; a reworded
            // descriptor is not drift, since the prompt is served from the
            // journaled environment sync.
            Some(entry) => (lash_core::tool_dispatch_surface(&entry.manifest, &entry.contract)
                != lash_core::tool_dispatch_surface(&recorded.manifest, &recorded.contract))
            .then_some(CellBindingDriftKind::Changed),
        };
        if let Some(kind) = kind {
            bindings.drifted.insert(
                recorded.manifest.id.clone(),
                CellBindingDrift {
                    path: path.clone(),
                    kind,
                    recorded: recorded.clone(),
                },
            );
        }
        bindings.recorded.insert(path, Some(recorded));
    }
    Ok(bindings)
}

/// Journals the binding set a cell resolved from `catalog`, the turn's
/// recorded tool surface, before its first effect, under the exec effect at
/// `exec_replay_key`; a redrive is served the recorded set, and each binding
/// is judged against the catalog the live registry resolves to now.
#[expect(
    clippy::expect_used,
    reason = "the referenced call paths are strings, so they encode as canonical JSON, as the site's own message states"
)]
pub async fn journal_cell_tool_bindings(
    referenced: &BTreeSet<String>,
    catalog: &lash_core::ToolCatalog,
    excluded: &BTreeSet<String>,
    exec_replay_key: &str,
    ctx: &lash_core::RuntimeExecutionContext<'_>,
) -> Result<CellToolBindings, lash_core::RuntimeEffectControllerError> {
    let effect_id = format!("{exec_replay_key}:{CELL_TOOL_BINDINGS_SUFFIX}");
    let operation = format!(
        "{CELL_TOOL_BINDINGS_OPERATION}:{}",
        serde_json::to_string(referenced).expect("call-path strings encode as canonical JSON")
    );
    let resolved = resolve_ambient_bindings(referenced, catalog, excluded).map_err(|error| {
        lash_core::RuntimeEffectControllerError::new(
            lash_core::RuntimeErrorCode::RecordEncodingFailed,
            format!("failed to encode the cell tool binding set: {error}"),
        )
    })?;
    let record =
        ctx.journaled_deferred_resolution_with(effect_id, operation, move || async move {
            Ok(resolved)
        })
        .await?;
    compare(record, ctx.live_tool_catalog().as_ref())
}

#[cfg(test)]
#[path = "cell_bindings_tests.rs"]
mod tests;
