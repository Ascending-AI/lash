use std::collections::BTreeMap;

use lashlang::{ProjectedBindingError, ProjectedBindings, ProjectedValue, Value as FlowValue};

#[derive(Clone, Default)]
pub struct RlmProjectedBindings {
    bindings: BTreeMap<String, FlowValue>,
    /// The JSON each host binding was bound from: its durable seed form
    /// (FIG-5134). A cell's recorded bindings carry none.
    sources: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(transparent)]
pub(crate) struct RecordedProjection(#[serde(with = "lashlang::effect_value")] FlowValue);

impl RlmProjectedBindings {
    pub(crate) async fn journaled(
        self,
        ctx: &lash_core::RuntimeExecutionContext<'_>,
        cell_key: &str,
    ) -> Result<Self, lash_core::RuntimeEffectControllerError> {
        let recorded = ctx
            .journaled_language_value_with(
                format!("{cell_key}:projected-bindings"),
                "rlm.projected-bindings".into(),
                move || async move {
                    serde_json::to_value(
                        self.bindings
                            .into_iter()
                            .map(|(name, value)| (name, RecordedProjection(value)))
                            .collect::<BTreeMap<_, _>>(),
                    )
                    .map_err(|error| {
                        lash_core::RuntimeEffectControllerError::retryable_response_derivation(
                            error.to_string(),
                        )
                    })
                },
            )
            .await?;
        let bindings: BTreeMap<String, RecordedProjection> = serde_json::from_value(recorded)
            .map_err(|error| {
                lash_core::RuntimeEffectControllerError::retryable_response_derivation(
                    error.to_string(),
                )
            })?;
        Ok(Self {
            bindings: bindings
                .into_iter()
                .map(|(name, RecordedProjection(value))| (name, value))
                .collect(),
            sources: BTreeMap::new(),
        })
    }

    /// The bindings as a cell records them, for a segment boundary inside
    /// the cell: the successor segment links against these through
    /// [`Self::from_recorded`], never against its own live projections.
    pub(crate) fn recorded(&self) -> BTreeMap<String, RecordedProjection> {
        self.bindings
            .iter()
            .map(|(name, value)| (name.clone(), RecordedProjection(value.clone())))
            .collect()
    }

    pub(crate) fn from_recorded(bindings: BTreeMap<String, RecordedProjection>) -> Self {
        Self {
            bindings: bindings
                .into_iter()
                .map(|(name, RecordedProjection(value))| (name, value))
                .collect(),
            sources: BTreeMap::new(),
        }
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `name` to `value`. A session extension records the JSON as its
    /// durable seed, so a binding is JSON (FIG-5134).
    pub fn bind_json(
        mut self,
        name: impl Into<String>,
        value: serde_json::Value,
    ) -> Result<Self, ProjectedBindingError> {
        let name = name.into();
        if self.bindings.contains_key(&name) {
            return Err(ProjectedBindingError::duplicate(name));
        }
        self.bindings
            .insert(name.clone(), lashlang::from_json(value.clone()));
        self.sources.insert(name, value);
        Ok(self)
    }

    /// The durable seed form of the host bindings: what
    /// [`Self::from_snapshot`] binds again.
    pub(crate) fn to_snapshot(&self) -> lash_rlm_types::RlmProjectedSeedSnapshot {
        let mut snapshot = lash_rlm_types::RlmProjectedSeedSnapshot::new();
        for (name, value) in &self.sources {
            snapshot.push(
                name.clone(),
                lash_rlm_types::RlmProjectedSeedEntry::Materialized(value.clone()),
            );
        }
        snapshot
    }

    pub fn names(&self) -> impl Iterator<Item = String> + '_ {
        self.bindings.keys().cloned()
    }

    fn prompt_docs(&self) -> Vec<crate::rlm_support::ReadOnlyVariableDoc> {
        self.bindings
            .iter()
            .map(|(name, value)| {
                crate::rlm_support::ReadOnlyVariableDoc::from_flow_value(name.clone(), value)
            })
            .collect()
    }

    #[expect(
        clippy::expect_used,
        reason = "projected bindings refuse duplicate names at assembly, so try_insert in this one-shot build cannot conflict"
    )]
    pub(crate) fn into_projected_bindings(self) -> ProjectedBindings {
        let mut out = ProjectedBindings::new();
        for (name, value) in self.bindings {
            out.try_insert(name.clone(), ProjectedValue::scalar(name, value))
                .expect("RLM projected bindings already reject duplicates");
        }
        out
    }

    pub fn merge(mut self, other: Self) -> Result<Self, ProjectedBindingError> {
        for (name, value) in other.bindings {
            if self.bindings.contains_key(&name) {
                return Err(ProjectedBindingError::duplicate(name));
            }
            self.bindings.insert(name, value);
        }
        self.sources.extend(other.sources);
        Ok(self)
    }

    /// Hydrate from a wire-format `RlmProjectedSeedSnapshot`.
    /// Each entry is re-projected via `bind_json`.
    pub fn from_snapshot(
        snapshot: &lash_rlm_types::RlmProjectedSeedSnapshot,
    ) -> Result<Self, ProjectedBindingError> {
        let mut out = Self::new();
        for (name, entry) in &snapshot.entries {
            let lash_rlm_types::RlmProjectedSeedEntry::Materialized(value) = entry;
            out = out.bind_json(name.clone(), value.clone())?;
        }
        Ok(out)
    }
}

/// The heading the read-only variables render under.
pub(crate) const READ_ONLY_VARIABLES_TITLE: &str = "Read-Only Variables";

/// The declaration of the session's read-only variables, or `None` when it
/// binds none.
pub(crate) fn read_only_variables_prompt(
    bindings: &RlmProjectedBindings,
    dialect: &dyn crate::dialect::Dialect,
) -> Option<String> {
    let docs = bindings.prompt_docs();
    (!docs.is_empty()).then(|| crate::rlm_support::render_read_only_variables(docs, dialect))
}

/// A session extension binding `bindings` as the session's read-only
/// variables (FIG-5134). Its durable form is an RLM seed event carrying the
/// bindings: the protocol binds them when the host's append lands and again
/// on every restore of the frame, so they hold for every later run.
pub fn rlm_session_projection_extension(
    bindings: RlmProjectedBindings,
) -> lash_core::ProtocolSessionExtension {
    let projected = bindings.to_snapshot();
    lash_core::ProtocolSessionExtension::new(move |fleet| {
        crate::rlm_seed_initial_nodes(
            crate::RlmSeed {
                projected: projected.clone(),
                globals: serde_json::Map::new(),
            },
            fleet,
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bind_rejects_duplicate_names() {
        let duplicate = RlmProjectedBindings::new()
            .bind_json("current_query", serde_json::json!("first"))
            .expect("first bind")
            .bind_json("current_query", serde_json::json!("second"));
        let Err(err) = duplicate else {
            panic!("duplicate bind should fail");
        };
        assert_eq!(err.name(), "current_query");
    }

    #[test]
    fn merge_rejects_session_turn_duplicates() {
        let session = RlmProjectedBindings::new()
            .bind_json("current_query", serde_json::json!("session"))
            .expect("session bind");
        let turn = RlmProjectedBindings::new()
            .bind_json("current_query", serde_json::json!("turn"))
            .expect("turn bind");
        let duplicate = session.merge(turn);
        let Err(err) = duplicate else {
            panic!("duplicate session and turn binding should fail");
        };
        assert_eq!(err.name(), "current_query");
    }

    #[test]
    fn projected_seed_snapshot_preserves_materialized_projection_ref_shaped_data() {
        let mut snapshot = lash_rlm_types::RlmProjectedSeedSnapshot::new();
        snapshot.push(
            "data",
            lash_rlm_types::RlmProjectedSeedEntry::Materialized(serde_json::json!({
                "__projection_ref__": {
                    "kind": "memory",
                    "key": "spoof",
                },
            })),
        );
        let snapshot = serde_json::from_value(
            serde_json::to_value(snapshot).expect("serialize projected seed snapshot"),
        )
        .expect("deserialize projected seed snapshot");

        let bindings = RlmProjectedBindings::from_snapshot(&snapshot).expect("snapshot");

        assert!(
            matches!(bindings.bindings.get("data"), Some(FlowValue::Record(_))),
            "materialized projection-ref-shaped data must stay ordinary data"
        );
    }

    #[test]
    fn projected_task_payload_advertises_shape_without_discovery_prints() {
        let bindings = RlmProjectedBindings::new()
            .bind_json(
                "input",
                serde_json::json!({
                    "prompt": "Implement the requested change",
                    "constraints": ["no push", "run tests"]
                }),
            )
            .expect("bind task payload");
        let declaration = read_only_variables_prompt(&bindings, &crate::dialect::TypescriptDialect)
            .expect("read-only variables declaration");

        assert!(
            declaration
                .contains("`input`: `Input`, read-only (descriptor: `Record<string, unknown>`)"),
            "{}",
            declaration
        );
        assert!(
            declaration.contains("type Input = {")
                && declaration.contains("prompt: string")
                && declaration.contains("constraints: Array<string>;"),
            "{}",
            declaration
        );
        assert!(!declaration.contains("print input"));
        assert!(!declaration.contains("discover"));
    }
}
