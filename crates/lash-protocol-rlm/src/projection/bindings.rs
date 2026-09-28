use std::any::Any;
use std::collections::BTreeMap;

use lash_core::{PromptContribution, ProtocolSessionExtension};

use lashlang::{ProjectedBindingError, ProjectedBindings, ProjectedValue, Value as FlowValue};

#[derive(Clone, Default)]
pub struct RlmProjectedBindings {
    bindings: BTreeMap<String, FlowValue>,
}

impl RlmProjectedBindings {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn bind_value(
        mut self,
        name: impl Into<String>,
        value: impl Into<FlowValue>,
    ) -> Result<Self, ProjectedBindingError> {
        let name = name.into();
        if self.bindings.contains_key(&name) {
            return Err(ProjectedBindingError::duplicate(name));
        }
        self.bindings.insert(name, value.into());
        Ok(self)
    }

    pub fn bind_json(
        self,
        name: impl Into<String>,
        value: serde_json::Value,
    ) -> Result<Self, ProjectedBindingError> {
        self.bind_value(name, lashlang::from_json(value))
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

#[derive(Clone, Default)]
pub(crate) struct RlmProjectionExtension {
    pub(crate) bindings: RlmProjectedBindings,
}

impl RlmProjectionExtension {
    pub(crate) fn new(bindings: RlmProjectedBindings) -> Self {
        Self { bindings }
    }

    pub(crate) fn prompt_contributions_for(
        bindings: &RlmProjectedBindings,
        vocabulary: crate::dialect::DialectPromptVocabulary,
    ) -> Vec<PromptContribution> {
        let docs = bindings.prompt_docs();
        if docs.is_empty() {
            return Vec::new();
        }
        vec![PromptContribution::environment(
            "Read-Only Variables",
            crate::rlm_support::render_read_only_variables(docs, vocabulary),
        )]
    }
}

impl ProtocolSessionExtension for RlmProjectionExtension {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

pub fn rlm_session_projection_extension(
    bindings: RlmProjectedBindings,
) -> lash_core::ProtocolSessionExtensionHandle {
    lash_core::ProtocolSessionExtensionHandle::new(RlmProjectionExtension::new(bindings))
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
    fn projected_seed_snapshot_rejects_the_ref_entry_kind() {
        let snapshot = serde_json::json!({
            "entries": [["doc", { "kind": "ref", "value": { "kind": "memory", "key": "doc" } }]],
        });
        assert!(
            serde_json::from_value::<lash_rlm_types::RlmProjectedSeedSnapshot>(snapshot).is_err(),
            "a durable ref seed entry must fail to decode"
        );
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
        let contribution = RlmProjectionExtension::prompt_contributions_for(
            &bindings,
            crate::dialect::DialectPromptVocabulary::default(),
        )
        .pop()
        .expect("prompt contribution");

        assert!(
            contribution
                .content
                .contains("`input`: `Input`, read-only (descriptor: `record`)"),
            "{}",
            contribution.content
        );
        assert!(
            contribution.content.contains("type Input = {")
                && contribution.content.contains("prompt: str,")
                && contribution.content.contains("constraints: list[str],"),
            "{}",
            contribution.content
        );
        assert!(!contribution.content.contains("print input"));
        assert!(!contribution.content.contains("discover"));
    }
}
