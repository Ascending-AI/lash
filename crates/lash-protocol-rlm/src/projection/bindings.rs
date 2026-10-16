use std::collections::BTreeMap;

/// Why a host's read-only binding configuration was refused.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ProjectedBindingError {
    #[error("projected binding `{name}` is already bound")]
    Duplicate { name: String },
    /// Includes the reserved `history` name and canonical token headers.
    #[error(
        "projected binding `{name}` makes host metadata {actual} encoded bytes, exceeding the {limit}-byte limit"
    )]
    HostMetadataTooLarge {
        name: String,
        limit: usize,
        actual: usize,
    },
}

impl ProjectedBindingError {
    fn duplicate(name: String) -> Self {
        Self::Duplicate { name }
    }

    /// A seed restore keeps the size refusal available to its host as a
    /// typed source, rather than reducing it to protocol-error prose.
    pub(crate) fn into_session_error(self) -> lash_core::SessionError {
        match self {
            error @ Self::HostMetadataTooLarge { .. } => {
                lash_core::SessionError::ExecutionStateRestore {
                    source: Box::new(error),
                }
            }
            error @ Self::Duplicate { .. } => lash_core::SessionError::Protocol(format!(
                "code mode projected seed snapshot rejected: {error}"
            )),
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Duplicate { name } | Self::HostMetadataTooLarge { name, .. } => name,
        }
    }
}

/// The session's read-only variables: JSON the host bound, which every cell
/// starts with.
#[derive(Clone, Default)]
pub struct CodeModeProjectedBindings {
    bindings: BTreeMap<String, serde_json::Value>,
}

impl CodeModeProjectedBindings {
    /// The bindings as the cell under `cell_key` records them: a redrive of
    /// the cell starts with the values its first run started with.
    pub(crate) async fn journaled(
        self,
        ctx: &lash_core::RuntimeExecutionContext<'_>,
        cell_key: &str,
    ) -> Result<Self, lash_core::RuntimeEffectControllerError> {
        let recorded = ctx
            .journaled_language_value_with(
                format!("{cell_key}:projected-bindings"),
                "codemode.projected-bindings".into(),
                move || async move {
                    serde_json::to_value(self.bindings).map_err(|error| {
                        lash_core::RuntimeEffectControllerError::retryable_response_derivation(
                            error.to_string(),
                        )
                    })
                },
            )
            .await?;
        let bindings = serde_json::from_value(recorded).map_err(|error| {
            lash_core::RuntimeEffectControllerError::retryable_response_derivation(
                error.to_string(),
            )
        })?;
        Ok(Self { bindings })
    }

    pub fn new() -> Self {
        Self::default()
    }

    /// Bind `name` to `value`. A session extension records the JSON as its
    /// durable seed, so a binding is JSON (FIG-5134). The encoded name list,
    /// including the built-in `history`, must fit the checkpoint root's
    /// 3067-byte host-metadata budget (FIG-6499).
    pub fn bind_json(
        mut self,
        name: impl Into<String>,
        value: serde_json::Value,
    ) -> Result<Self, ProjectedBindingError> {
        let name = name.into();
        if self.bindings.contains_key(&name) {
            return Err(ProjectedBindingError::duplicate(name));
        }
        // Reserve the built-in even when this cell does not mention it:
        // an admitted session can bind history in any later cell.
        let actual = lash_kernel_state::paged::CheckpointRoot::host_metadata_bytes(
            self.bindings
                .keys()
                .map(String::as_str)
                .chain([name.as_str(), super::HISTORY_PROJECTION]),
        );
        let limit = lash_kernel_state::paged::CheckpointRoot::HOST_METADATA_LIMIT;
        if actual > limit {
            return Err(ProjectedBindingError::HostMetadataTooLarge {
                name,
                limit,
                actual,
            });
        }
        self.bindings.insert(name, value);
        Ok(self)
    }

    /// The durable seed form of the host bindings: what
    /// [`Self::from_snapshot`] binds again.
    pub(crate) fn to_snapshot(&self) -> lash_rlm_types::CodeModeProjectedSeedSnapshot {
        let mut snapshot = lash_rlm_types::CodeModeProjectedSeedSnapshot::new();
        for (name, value) in &self.bindings {
            snapshot.push(
                name.clone(),
                lash_rlm_types::CodeModeProjectedSeedEntry::Materialized(value.clone()),
            );
        }
        snapshot
    }

    pub fn names(&self) -> impl Iterator<Item = String> + '_ {
        self.bindings.keys().cloned()
    }

    pub(crate) fn prompt_docs(&self) -> Vec<crate::rlm_support::ReadOnlyVariableDoc> {
        self.bindings
            .iter()
            .map(|(name, value)| {
                crate::rlm_support::ReadOnlyVariableDoc::from_json(
                    name.clone(),
                    json_descriptor_type(value).to_owned(),
                    value,
                )
            })
            .collect()
    }

    /// The bindings, by name.
    pub(crate) fn into_values(self) -> BTreeMap<String, serde_json::Value> {
        self.bindings
    }

    pub fn merge(mut self, other: Self) -> Result<Self, ProjectedBindingError> {
        for (name, value) in other.bindings {
            self = self.bind_json(name, value)?;
        }
        Ok(self)
    }

    /// Hydrate from a wire-format `CodeModeProjectedSeedSnapshot`.
    /// Each entry is re-projected via `bind_json`.
    pub fn from_snapshot(
        snapshot: &lash_rlm_types::CodeModeProjectedSeedSnapshot,
    ) -> Result<Self, ProjectedBindingError> {
        let mut out = Self::new();
        for (name, entry) in &snapshot.entries {
            let lash_rlm_types::CodeModeProjectedSeedEntry::Materialized(value) = entry;
            out = out.bind_json(name.clone(), value.clone())?;
        }
        Ok(out)
    }
}

fn json_descriptor_type(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "list",
        serde_json::Value::Object(_) => "record",
    }
}

/// The heading the read-only variables render under.
pub(crate) const READ_ONLY_VARIABLES_TITLE: &str = "Read-Only Variables";

/// The declaration of the session's read-only variables, or `None` when it
/// binds none.
pub(crate) fn read_only_variables_prompt(
    bindings: &CodeModeProjectedBindings,
    dialect: &dyn crate::dialect::DialectPrompts,
) -> Result<Option<String>, lash_tool_presenter_api::PresentationRefusal> {
    let docs = bindings.prompt_docs();
    (!docs.is_empty())
        .then(|| crate::rlm_support::render_read_only_variables(docs, dialect))
        .transpose()
}

/// A session extension binding `bindings` as the session's read-only
/// variables (FIG-5134). Its durable form is an RLM seed event carrying the
/// bindings: the protocol binds them when the host's append lands and again
/// on every restore of the frame, so they hold for every later run.
pub fn rlm_session_projection_extension(
    bindings: CodeModeProjectedBindings,
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

    /// FIG-6499 / S1: a host configuration reserves the built-in history
    /// name and refuses oversized metadata before any session starts.
    #[test]
    fn projected_host_metadata_is_bounded_at_configuration() {
        let oversized =
            CodeModeProjectedBindings::new().bind_json("h".repeat(3054), serde_json::Value::Null);
        assert!(matches!(
            oversized,
            Err(ProjectedBindingError::HostMetadataTooLarge {
                limit: 3067,
                actual: 3068,
                ..
            })
        ));
        CodeModeProjectedBindings::new()
            .bind_json("h".repeat(3053), serde_json::Value::Null)
            .expect("the largest name plus history fits");
        let left = CodeModeProjectedBindings::new()
            .bind_json("h".repeat(3050), serde_json::Value::Null)
            .unwrap();
        let right = CodeModeProjectedBindings::new()
            .bind_json("xy", serde_json::Value::Null)
            .unwrap();
        assert!(matches!(
            left.merge(right),
            Err(ProjectedBindingError::HostMetadataTooLarge {
                limit: 3067,
                actual: 3068,
                ..
            })
        ));
        let mut snapshot = lash_rlm_types::CodeModeProjectedSeedSnapshot::new();
        snapshot.push(
            "h".repeat(3054),
            lash_rlm_types::CodeModeProjectedSeedEntry::Materialized(serde_json::Value::Null),
        );
        assert!(matches!(
            CodeModeProjectedBindings::from_snapshot(&snapshot),
            Err(ProjectedBindingError::HostMetadataTooLarge {
                limit: 3067,
                actual: 3068,
                ..
            })
        ));
        let restored_error = CodeModeProjectedBindings::from_snapshot(&snapshot)
            .err()
            .expect("oversized seed")
            .into_session_error();
        let lash_core::SessionError::ExecutionStateRestore { source } = restored_error else {
            panic!("seed admission must retain the typed refusal for its host");
        };
        assert!(matches!(
            source.downcast_ref::<ProjectedBindingError>(),
            Some(ProjectedBindingError::HostMetadataTooLarge {
                limit: 3067,
                actual: 3068,
                ..
            })
        ));
        let mut bindings = CodeModeProjectedBindings::new();
        for ordinal in 0..1000 {
            match bindings.bind_json(format!("h{ordinal}"), serde_json::Value::Null) {
                Ok(next) => bindings = next,
                Err(ProjectedBindingError::HostMetadataTooLarge { limit, actual, .. }) => {
                    assert_eq!(limit, 3067);
                    assert!(actual > limit);
                    return;
                }
                Err(error) => panic!("distinct names: {error}"),
            }
        }
        panic!("many short names must also be bounded");
    }

    #[test]
    fn bind_rejects_duplicate_names() {
        let duplicate = CodeModeProjectedBindings::new()
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
        let session = CodeModeProjectedBindings::new()
            .bind_json("current_query", serde_json::json!("session"))
            .expect("session bind");
        let turn = CodeModeProjectedBindings::new()
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
        let mut snapshot = lash_rlm_types::CodeModeProjectedSeedSnapshot::new();
        snapshot.push(
            "data",
            lash_rlm_types::CodeModeProjectedSeedEntry::Materialized(serde_json::json!({
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

        let bindings = CodeModeProjectedBindings::from_snapshot(&snapshot).expect("snapshot");

        assert!(
            matches!(
                bindings.bindings.get("data"),
                Some(serde_json::Value::Object(_))
            ),
            "materialized projection-ref-shaped data must stay ordinary data"
        );
    }

    #[test]
    fn projected_task_payload_advertises_shape_without_discovery_prints() {
        let bindings = CodeModeProjectedBindings::new()
            .bind_json(
                "input",
                serde_json::json!({
                    "prompt": "Implement the requested change",
                    "constraints": ["no push", "run tests"]
                }),
            )
            .expect("bind task payload");
        let declaration = read_only_variables_prompt(&bindings, &crate::dialect::TypescriptPrompts)
            .expect("presentable fixture")
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
