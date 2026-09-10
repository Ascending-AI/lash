use lash_sansio::sync::RwLockExt;
use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::{PromptContribution, ProtocolSessionExtension};
pub use lash_rlm_types::ProjectionRef;

use lashlang::{
    ProjectedBindingError, ProjectedBindings, ProjectedHostDescriptor, ProjectedValue,
    Value as FlowValue,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProjectionResolveError {
    message: String,
}

impl ProjectionResolveError {
    pub fn unavailable(reference: &ProjectionRef) -> Self {
        Self {
            message: format!(
                "projection ref unavailable: kind `{}`, key {}",
                reference.kind, reference.key
            ),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProjectionResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ProjectionResolveError {}

#[async_trait::async_trait]
pub trait ProjectionResolver: Send + Sync {
    async fn resolve_projection(
        &self,
        reference: &ProjectionRef,
    ) -> Result<Arc<dyn ProjectedHostDescriptor>, ProjectionResolveError>;
}

#[derive(Clone, Default)]
pub struct ProjectionRegistry {
    memory: Arc<std::sync::RwLock<BTreeMap<String, Arc<dyn ProjectedHostDescriptor>>>>,
}

impl ProjectionRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_memory(&self, value: Arc<dyn ProjectedHostDescriptor>) -> ProjectionRef {
        let descriptor_type = value.type_name().to_string();
        let key = uuid::Uuid::new_v4().to_string();
        self.memory.write_recover().insert(key.clone(), value);
        ProjectionRef::new("memory", serde_json::Value::String(key))
            .with_descriptor_type(descriptor_type)
    }
}

#[async_trait::async_trait]
impl ProjectionResolver for ProjectionRegistry {
    async fn resolve_projection(
        &self,
        reference: &ProjectionRef,
    ) -> Result<Arc<dyn ProjectedHostDescriptor>, ProjectionResolveError> {
        if reference.kind != "memory" {
            return Err(ProjectionResolveError::unavailable(reference));
        }
        let Some(key) = reference.key.as_str() else {
            return Err(ProjectionResolveError::invalid(
                "memory projection ref key must be a string",
            ));
        };
        self.memory
            .read_recover()
            .get(key)
            .cloned()
            .ok_or_else(|| ProjectionResolveError::unavailable(reference))
    }
}

#[derive(Clone)]
enum RlmProjectedBinding {
    Value(FlowValue),
    Lazy(ProjectionRef),
}

#[derive(Clone, Default)]
pub struct RlmProjectedBindings {
    bindings: BTreeMap<String, RlmProjectedBinding>,
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
        self.bindings
            .insert(name, RlmProjectedBinding::Value(value.into()));
        Ok(self)
    }

    pub fn bind_json(
        self,
        name: impl Into<String>,
        value: serde_json::Value,
    ) -> Result<Self, ProjectedBindingError> {
        self.bind_value(name, lashlang::from_json(value))
    }

    pub fn bind_lazy(
        mut self,
        name: impl Into<String>,
        reference: ProjectionRef,
    ) -> Result<Self, ProjectedBindingError> {
        let name = name.into();
        if self.bindings.contains_key(&name) {
            return Err(ProjectedBindingError::duplicate(name));
        }
        self.bindings
            .insert(name, RlmProjectedBinding::Lazy(reference));
        Ok(self)
    }

    pub fn names(&self) -> impl Iterator<Item = String> + '_ {
        self.bindings.keys().cloned()
    }

    fn prompt_docs(&self) -> Vec<crate::rlm_support::ReadOnlyVariableDoc> {
        self.bindings
            .iter()
            .map(|(name, binding)| match binding {
                RlmProjectedBinding::Value(value) => {
                    crate::rlm_support::ReadOnlyVariableDoc::from_flow_value(name.clone(), value)
                }
                RlmProjectedBinding::Lazy(reference) => {
                    crate::rlm_support::ReadOnlyVariableDoc::descriptor_only(
                        name.clone(),
                        reference
                            .descriptor_type
                            .clone()
                            .unwrap_or_else(|| "any".to_string()),
                    )
                }
            })
            .collect()
    }

    pub(crate) async fn into_projected_bindings(
        self,
        resolver: Arc<dyn ProjectionResolver>,
    ) -> Result<ProjectedBindings, ProjectionResolveError> {
        let mut out = ProjectedBindings::new();
        for (name, binding) in self.bindings {
            let value = match binding {
                RlmProjectedBinding::Value(value) => ProjectedValue::scalar(name.clone(), value),
                RlmProjectedBinding::Lazy(reference) => {
                    let resolved = resolver.resolve_projection(&reference).await?;
                    let ref_json = serde_json::to_value(&reference).map_err(|err| {
                        ProjectionResolveError::invalid(format!(
                            "projection ref did not serialize: {err}"
                        ))
                    })?;
                    ProjectedValue::custom_with_projection_ref(name.clone(), resolved, ref_json)
                }
            };
            out.try_insert(name, value)
                .expect("RLM projected bindings already reject duplicates");
        }
        Ok(out)
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

    /// Hydrate from a wire-format `RlmProjectedSeedSnapshot`. Each entry is
    /// re-projected via `bind_json`. Used by the RLM protocol to seed projections on a
    /// child session (spawn_agent / continue_as) from the parent's classified
    /// seed map.
    pub fn from_snapshot(
        snapshot: &lash_rlm_types::RlmProjectedSeedSnapshot,
    ) -> Result<Self, ProjectedBindingError> {
        let mut out = Self::new();
        for (name, entry) in &snapshot.entries {
            out = match entry {
                lash_rlm_types::RlmProjectedSeedEntry::Materialized(value) => {
                    out.bind_json(name.clone(), value.clone())?
                }
                lash_rlm_types::RlmProjectedSeedEntry::Ref(reference) => {
                    out.bind_lazy(name.clone(), reference.clone())?
                }
            };
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
    use lashlang::{ProjectedFuture, ProjectedReadRequest, ProjectedReadResponse};

    struct TestProjectedValue;

    impl ProjectedHostDescriptor for TestProjectedValue {
        fn type_name(&self) -> &str {
            "string"
        }

        fn read_one(
            &self,
            request: ProjectedReadRequest,
        ) -> ProjectedFuture<'_, ProjectedReadResponse> {
            Box::pin(async move {
                match request {
                    ProjectedReadRequest::Materialize => {
                        ProjectedReadResponse::Value(FlowValue::String("lazy".into()))
                    }
                    ProjectedReadRequest::Render => ProjectedReadResponse::Text("lazy".into()),
                    _ => ProjectedReadResponse::Missing,
                }
            })
        }
    }

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

    #[tokio::test]
    async fn bind_lazy_resolves_memory_projection_ref() {
        let registry = Arc::new(ProjectionRegistry::new());
        let reference = registry.register_memory(Arc::new(TestProjectedValue));
        let bindings = RlmProjectedBindings::new()
            .bind_lazy("doc", reference.clone())
            .expect("lazy bind");

        let projected = bindings
            .into_projected_bindings(registry)
            .await
            .expect("resolve projected bindings");
        let value = projected.get("doc").expect("doc binding");
        assert_eq!(value.projection_ref(), Some(&serde_json::json!(reference)));
        assert_eq!(value.render().await, "lazy");
    }

    #[tokio::test]
    async fn bind_lazy_reports_missing_memory_projection_ref() {
        let registry = Arc::new(ProjectionRegistry::new());
        let reference = ProjectionRef::new("memory", serde_json::json!("missing"));
        let bindings = RlmProjectedBindings::new()
            .bind_lazy("doc", reference)
            .expect("lazy bind");

        let err = match bindings.into_projected_bindings(registry).await {
            Ok(_) => panic!("missing ref should fail"),
            Err(err) => err,
        };
        assert!(err.to_string().contains("projection ref unavailable"));
    }

    #[test]
    fn projected_seed_snapshot_preserves_projection_refs() {
        let reference = ProjectionRef::new("memory", serde_json::json!("stable"));
        let mut snapshot = lash_rlm_types::RlmProjectedSeedSnapshot::new();
        snapshot.push("doc", lash_rlm_types::RlmProjectedSeedEntry::Ref(reference));

        let bindings = RlmProjectedBindings::from_snapshot(&snapshot).expect("snapshot");
        assert_eq!(
            bindings.names().collect::<Vec<_>>(),
            vec!["doc".to_string()]
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
            matches!(
                bindings.bindings.get("data"),
                Some(RlmProjectedBinding::Value(_))
            ),
            "materialized projection-ref-shaped data must not become a lazy reference"
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
            crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
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
