//! How a tool is named in guest code: the dialect-neutral binding a tool's
//! manifest carries, validated and resolved to the path a document performs
//! it under.

pub use lash_core::{TOOL_BINDING_KEY, ToolBinding, ToolDefinitionBindingExt};

/// A failure while decoding or resolving a tool binding.
#[non_exhaustive]
#[derive(Debug, thiserror::Error)]
pub enum ToolBindingError {
    /// A tool manifest omits the binding required by a dialect.
    #[error("tool `{tool}` is missing an explicit `{binding_key}` binding")]
    MissingBinding {
        tool: String,
        binding_key: &'static str,
    },
    /// A tool binding payload cannot be decoded.
    #[error("tool `{tool}` has malformed `{binding_key}` binding payload: {source}")]
    MalformedPayload {
        tool: String,
        binding_key: &'static str,
        #[source]
        source: serde_json::Error,
    },
    /// A tool binding omits its module path.
    #[error("tool `{tool}` is missing an explicit tool-binding module path")]
    MissingModulePath { tool: String },
    /// A tool binding omits its operation name.
    #[error("tool `{tool}` is missing an explicit tool-binding operation name")]
    MissingOperation { tool: String },
    /// A tool binding contains an identifier that cannot be projected into a dialect.
    #[error("tool `{tool}` has invalid tool-binding {part} `{value}`")]
    InvalidIdentifier {
        tool: String,
        part: &'static str,
        value: String,
    },
    /// Two bindings offer one effect.
    #[error("conflicting tool bindings: {source}")]
    ConflictingBinding {
        #[from]
        source: crate::BoundaryError,
    },
}

/// Resolution over the dialect-agnostic [`ToolBinding`] relocated to
/// `lash-core`: validate the authored module path and operation and produce
/// the typed, front-end-neutral [`ResolvedToolBinding`]: the effect a document
/// performs, which each dialect spells in its own syntax (ADR 0096).
pub trait ToolBindingResolutionExt {
    fn executable_for(&self, tool_name: &str) -> Result<ResolvedToolBinding, ToolBindingError>;
}

impl ToolBindingResolutionExt for ToolBinding {
    fn executable_for(&self, tool_name: &str) -> Result<ResolvedToolBinding, ToolBindingError> {
        if self.module_path.is_empty() {
            return Err(ToolBindingError::MissingModulePath {
                tool: tool_name.to_string(),
            });
        }
        for segment in &self.module_path {
            validate_identifier(tool_name, "module path segment", segment)?;
        }
        let operation =
            self.operation
                .as_deref()
                .ok_or_else(|| ToolBindingError::MissingOperation {
                    tool: tool_name.to_string(),
                })?;
        validate_identifier(tool_name, "operation name", operation)?;
        let authority_type = self
            .authority_type
            .as_deref()
            .filter(|authority_type| !authority_type.trim().is_empty())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| default_authority_type(&self.module_path));
        Ok(ResolvedToolBinding {
            module_path: self.module_path.clone(),
            operation: operation.to_string(),
            authority_type,
            aliases: self.aliases.clone(),
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedToolBinding {
    pub module_path: Vec<String>,
    pub operation: String,
    pub authority_type: String,
    pub aliases: Vec<String>,
}

impl ResolvedToolBinding {
    pub fn module_path_string(&self) -> String {
        self.module_path.join(".")
    }

    pub fn call_path(&self) -> String {
        format!("{}.{}", self.module_path_string(), self.operation)
    }
}

fn default_authority_type(module_path: &[String]) -> String {
    module_path
        .last()
        .map(|segment| {
            let mut chars = segment.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => "Tool".to_string(),
            }
        })
        .unwrap_or_else(|| "Tool".to_string())
}

fn validate_identifier(
    tool_name: &str,
    label: &'static str,
    value: &str,
) -> Result<(), ToolBindingError> {
    let value = value.trim();
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return Err(ToolBindingError::InvalidIdentifier {
            tool: tool_name.to_string(),
            part: label,
            value: "<empty>".to_string(),
        });
    };
    if !(first == '_' || first.is_ascii_alphabetic()) {
        return Err(ToolBindingError::InvalidIdentifier {
            tool: tool_name.to_string(),
            part: label,
            value: value.to_string(),
        });
    }
    if !chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric()) {
        return Err(ToolBindingError::InvalidIdentifier {
            tool: tool_name.to_string(),
            part: label,
            value: value.to_string(),
        });
    }
    Ok(())
}

pub fn required_tool_binding(
    manifest: &lash_core::ToolManifest,
) -> Result<ToolBinding, ToolBindingError> {
    ToolManifestBindingExt::tool_binding(manifest)?.ok_or_else(|| {
        ToolBindingError::MissingBinding {
            tool: manifest.name.clone(),
            binding_key: TOOL_BINDING_KEY,
        }
    })
}

pub fn required_tool_executable(
    manifest: &lash_core::ToolManifest,
) -> Result<ResolvedToolBinding, ToolBindingError> {
    required_tool_binding(manifest)?.executable_for(&manifest.name)
}

pub trait ToolManifestBindingExt {
    fn tool_binding(&self) -> Result<Option<ToolBinding>, ToolBindingError>;
}

impl ToolManifestBindingExt for lash_core::ToolManifest {
    fn tool_binding(&self) -> Result<Option<ToolBinding>, ToolBindingError> {
        self.bindings
            .get(TOOL_BINDING_KEY)
            .cloned()
            .map(serde_json::from_value)
            .transpose()
            .map_err(|source| ToolBindingError::MalformedPayload {
                tool: self.name.clone(),
                binding_key: TOOL_BINDING_KEY,
                source,
            })
    }
}

impl crate::HostBoundary {
    /// Offers the tool `manifest` describes under its binding's call path,
    /// typed from the tool's JSON schemas.
    ///
    /// # Errors
    ///
    /// [`ToolBindingError`] when the manifest has no usable binding or its
    /// path is taken.
    pub fn offer_bound_tool(
        &mut self,
        manifest: &lash_core::ToolManifest,
        input: &serde_json::Value,
        output: &serde_json::Value,
    ) -> Result<(), ToolBindingError> {
        let binding = required_tool_executable(manifest)?;
        self.offer_tool(
            &binding.call_path(),
            manifest.id.clone(),
            input,
            output,
            manifest.declaration().controls.clone(),
        )?;
        Ok(())
    }

    /// Every tool of `catalog` under its binding's call path: what a
    /// process of that catalog may perform. A tool that declares a turn
    /// control is not offered: a process cannot end a session turn, so a
    /// process document that performs one is refused at admission as an
    /// effect its catalog does not offer.
    ///
    /// # Errors
    ///
    /// [`ToolBindingError`] when a tool has no usable binding or two tools
    /// take one path.
    pub fn of_catalog(catalog: &lash_core::ToolCatalog) -> Result<Self, ToolBindingError> {
        let mut boundary = Self::new();
        for entry in &catalog.tools {
            if !entry.manifest.declaration().controls.is_empty() {
                continue;
            }
            boundary.offer_bound_tool(
                &entry.manifest,
                entry.contract.input_schema.canonical(),
                entry.contract.output_schema.canonical(),
            )?;
        }
        Ok(boundary)
    }
}
