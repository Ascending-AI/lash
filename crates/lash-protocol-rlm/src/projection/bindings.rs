use lash_sansio::sync::RwLockExt;
use std::any::Any;
use std::collections::BTreeMap;
use std::sync::Arc;

use lash_core::{
    PromptContribution, ProtocolSessionExtension, ProtocolTurnExtension,
    ProtocolTurnExtensionHandle, TurnInput,
};

pub(crate) const RLM_TURN_INPUT_PLUGIN_ID: &str = "rlm";
use lashlang::{
    ProjectedBindingError, ProjectedBindings, ProjectedHostDescriptor, ProjectedValue,
    Value as FlowValue,
};

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProjectionRef {
    pub kind: String,
    pub key: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub descriptor_type: Option<String>,
}

impl ProjectionRef {
    pub fn new(kind: impl Into<String>, key: serde_json::Value) -> Self {
        Self {
            kind: kind.into(),
            key,
            descriptor_type: None,
        }
    }

    pub fn with_descriptor_type(mut self, descriptor_type: impl Into<String>) -> Self {
        self.descriptor_type = Some(descriptor_type.into());
        self
    }
}

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

pub type RlmToolResultProjector =
    Arc<dyn Fn(&str, &serde_json::Value) -> Option<FlowValue> + Send + Sync + 'static>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RlmProjectedSeedError {
    Binding(ProjectedBindingError),
    InvalidProjectionRef { name: String, source: String },
}

impl RlmProjectedSeedError {
    pub fn invalid_projection_ref(name: impl Into<String>, source: impl std::fmt::Display) -> Self {
        Self::InvalidProjectionRef {
            name: name.into(),
            source: source.to_string(),
        }
    }
}

impl From<ProjectedBindingError> for RlmProjectedSeedError {
    fn from(value: ProjectedBindingError) -> Self {
        Self::Binding(value)
    }
}

impl std::fmt::Display for RlmProjectedSeedError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Binding(err) => err.fmt(f),
            Self::InvalidProjectionRef { name, source } => {
                write!(
                    f,
                    "invalid projection ref for projected seed `{name}`: {source}"
                )
            }
        }
    }
}

impl std::error::Error for RlmProjectedSeedError {}

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
    ) -> Result<Self, RlmProjectedSeedError> {
        let mut out = Self::new();
        for (name, value) in &snapshot.entries {
            out = if let Some(reference) =
                super::transport::projection_ref_from_seed_value(name, value)?
            {
                out.bind_lazy(name.clone(), reference)?
            } else {
                out.bind_json(name.clone(), value.clone())?
            };
        }
        Ok(out)
    }
}

#[derive(Clone, Default)]
pub(crate) struct RlmProjectionExtension {
    pub(crate) bindings: RlmProjectedBindings,
    pub(crate) tool_result_projectors: Vec<RlmToolResultProjector>,
    /// The words the read-only-variables blurb is written in.
    ///
    /// An extension can be built by a host before any dialect is resolved, so
    /// this starts at the default dialect's vocabulary and the protocol sets
    /// the session's real one as soon as it owns the extension.
    pub(crate) vocabulary: crate::dialect::DialectPromptVocabulary,
}

impl RlmProjectionExtension {
    pub(crate) fn new(bindings: RlmProjectedBindings) -> Self {
        Self {
            bindings,
            tool_result_projectors: Vec::new(),
            vocabulary: crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
        }
    }

    pub(crate) fn with_projector(projector: RlmToolResultProjector) -> Self {
        Self {
            bindings: RlmProjectedBindings::new(),
            tool_result_projectors: vec![projector],
            vocabulary: crate::dialect::lashlang::LASHLANG_PROMPT_VOCABULARY,
        }
    }

    fn merge(mut self, other: Self) -> Result<Self, ProjectedBindingError> {
        self.bindings = self.bindings.merge(other.bindings)?;
        self.tool_result_projectors
            .extend(other.tool_result_projectors);
        Ok(self)
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

impl ProtocolTurnExtension for RlmProjectionExtension {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn prompt_contributions(&self) -> Vec<PromptContribution> {
        Self::prompt_contributions_for(&self.bindings, self.vocabulary)
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

pub trait RlmTurnInputExt {
    fn rlm_project(self, bindings: RlmProjectedBindings) -> Result<Self, ProjectedBindingError>
    where
        Self: Sized;

    fn rlm_project_tool_results(
        self,
        projector: RlmToolResultProjector,
    ) -> Result<Self, ProjectedBindingError>
    where
        Self: Sized;
}

impl RlmTurnInputExt for TurnInput {
    fn rlm_project(
        mut self,
        bindings: RlmProjectedBindings,
    ) -> Result<Self, ProjectedBindingError> {
        let extension = if let Some(existing) = self
            .turn_context
            .plugin_input::<RlmProjectionExtension>(RLM_TURN_INPUT_PLUGIN_ID)
            .cloned()
        {
            existing
                .clone()
                .merge(RlmProjectionExtension::new(bindings))?
        } else {
            RlmProjectionExtension::new(bindings)
        };
        self.turn_context
            .insert_plugin_input(RLM_TURN_INPUT_PLUGIN_ID, extension);
        self.protocol_extension = Some(ProtocolTurnExtensionHandle::new(
            RlmProjectionExtension::new(
                self.turn_context
                    .plugin_input::<RlmProjectionExtension>(RLM_TURN_INPUT_PLUGIN_ID)
                    .expect("RLM projection was just inserted")
                    .bindings
                    .clone(),
            ),
        ));
        Ok(self)
    }

    fn rlm_project_tool_results(
        mut self,
        projector: RlmToolResultProjector,
    ) -> Result<Self, ProjectedBindingError> {
        let extension = if let Some(existing) = self
            .turn_context
            .plugin_input::<RlmProjectionExtension>(RLM_TURN_INPUT_PLUGIN_ID)
            .cloned()
        {
            existing
                .clone()
                .merge(RlmProjectionExtension::with_projector(projector))?
        } else {
            RlmProjectionExtension::with_projector(projector)
        };
        self.turn_context
            .insert_plugin_input(RLM_TURN_INPUT_PLUGIN_ID, extension);
        self.protocol_extension = Some(ProtocolTurnExtensionHandle::new(
            RlmProjectionExtension::new(
                self.turn_context
                    .plugin_input::<RlmProjectionExtension>(RLM_TURN_INPUT_PLUGIN_ID)
                    .expect("RLM projection was just inserted")
                    .bindings
                    .clone(),
            ),
        ));
        Ok(self)
    }
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
        snapshot.push(
            "doc",
            serde_json::json!({
                lash_rlm_types::PROJECTION_REF_JSON_TAG: reference,
            }),
        );

        let bindings = RlmProjectedBindings::from_snapshot(&snapshot).expect("snapshot");
        assert_eq!(
            bindings.names().collect::<Vec<_>>(),
            vec!["doc".to_string()]
        );
    }

    #[test]
    fn projected_seed_snapshot_reports_invalid_projection_refs() {
        let mut snapshot = lash_rlm_types::RlmProjectedSeedSnapshot::new();
        snapshot.push(
            "doc",
            serde_json::json!({
                lash_rlm_types::PROJECTION_REF_JSON_TAG: "not a projection ref",
            }),
        );

        let err = match RlmProjectedBindings::from_snapshot(&snapshot) {
            Ok(_) => panic!("invalid projection ref should fail"),
            Err(err) => err,
        };

        assert!(err.to_string().contains("invalid projection ref"));
        assert!(err.to_string().contains("doc"));
    }

    #[test]
    fn turn_input_extension_attaches_prompt_contribution() {
        let input = TurnInput {
            items: Vec::new(),
            protocol_turn_options: None,
            trace_turn_id: None,
            protocol_extension: None,
            turn_context: lash_core::TurnContext::default(),
        }
        .rlm_project(
            RlmProjectedBindings::new()
                .bind_json("current_file", serde_json::json!("src/lib.rs"))
                .expect("bind"),
        )
        .expect("attach");
        let contribution = input
            .protocol_extension
            .expect("extension")
            .prompt_contributions()
            .pop()
            .expect("prompt contribution");
        assert!(contribution.content.contains("`current_file`"));
        assert!(
            contribution
                .content
                .contains("`current_file`: `str`, read-only")
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

    #[test]
    fn turn_input_extension_is_skipped_by_serde() {
        let input = TurnInput {
            items: Vec::new(),
            protocol_turn_options: None,
            trace_turn_id: Some("stable".to_string()),
            protocol_extension: None,
            turn_context: lash_core::TurnContext::default(),
        }
        .rlm_project(
            RlmProjectedBindings::new()
                .bind_json("current_file", serde_json::json!("src/lib.rs"))
                .expect("bind"),
        )
        .expect("attach");

        let encoded = serde_json::to_string(&input).expect("serialize");
        assert!(!encoded.contains("protocol_extension"));
        assert!(!encoded.contains("current_file"));
        let decoded: TurnInput = serde_json::from_str(&encoded).expect("deserialize");
        assert!(decoded.protocol_extension.is_none());
        assert_eq!(decoded.trace_turn_id.as_deref(), Some("stable"));
    }

    #[test]
    fn matching_trace_turn_ids_do_not_share_projection_extensions() {
        let first = TurnInput {
            items: Vec::new(),
            protocol_turn_options: None,
            trace_turn_id: Some("same-trace".to_string()),
            protocol_extension: None,
            turn_context: lash_core::TurnContext::default(),
        }
        .rlm_project(
            RlmProjectedBindings::new()
                .bind_json("first_name", serde_json::json!("first"))
                .expect("bind"),
        )
        .expect("attach first");
        let second = TurnInput {
            items: Vec::new(),
            protocol_turn_options: None,
            trace_turn_id: Some("same-trace".to_string()),
            protocol_extension: None,
            turn_context: lash_core::TurnContext::default(),
        }
        .rlm_project(
            RlmProjectedBindings::new()
                .bind_json("second_name", serde_json::json!("second"))
                .expect("bind"),
        )
        .expect("attach second");

        let first_extension = first
            .protocol_extension
            .as_ref()
            .and_then(|extension| extension.as_any().downcast_ref::<RlmProjectionExtension>())
            .expect("first extension");
        let second_extension = second
            .protocol_extension
            .as_ref()
            .and_then(|extension| extension.as_any().downcast_ref::<RlmProjectionExtension>())
            .expect("second extension");
        assert_eq!(
            first_extension.bindings.names().collect::<Vec<_>>(),
            vec!["first_name".to_string()]
        );
        assert_eq!(
            second_extension.bindings.names().collect::<Vec<_>>(),
            vec!["second_name".to_string()]
        );
    }
}
