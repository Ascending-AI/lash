//! Tool grants: schemas, call-path bindings, activation, and retry policies.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ensure_protocol_version;
use crate::llm::{RemoteSchemaContract, default_remote_input_schema};
use crate::registry_errors::RemoteProtocolError;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteToolGrant {
    pub protocol_version: u32,
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub description: String,
    #[serde(default = "default_remote_input_schema")]
    pub input_schema: RemoteSchemaContract,
    #[serde(default)]
    pub output_schema: RemoteSchemaContract,
    #[serde(default, skip_serializing_if = "RemoteToolOutputContract::is_static")]
    pub output_contract: RemoteToolOutputContract,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub examples: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activation: Option<RemoteToolActivation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub argument_projection: Option<RemoteToolArgumentProjectionPolicy>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_policy: Option<RemoteToolRetryPolicy>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub bindings: BTreeMap<String, serde_json::Value>,
}

impl RemoteToolGrant {
    pub fn binding_call_path(&self, binding_key: &str) -> Result<String, RemoteProtocolError> {
        let binding = self.required_call_path_binding(binding_key)?;
        Ok(format!(
            "{}.{}",
            binding.module_path.join("."),
            binding.operation
        ))
    }

    pub fn validate(&self) -> Result<(), RemoteProtocolError> {
        ensure_protocol_version(self.protocol_version)?;
        if self.id.trim().is_empty() {
            return Err(RemoteProtocolError::InvalidToolGrant {
                tool_name: self.name.clone(),
                message: "tool grant id cannot be empty".to_string(),
            });
        }
        if self.name.trim().is_empty() {
            return Err(RemoteProtocolError::InvalidToolGrant {
                tool_name: self.name.clone(),
                message: "tool grant name cannot be empty".to_string(),
            });
        }
        for key in self.bindings.keys() {
            if key.trim().is_empty() {
                return Err(RemoteProtocolError::InvalidToolGrant {
                    tool_name: self.name.clone(),
                    message: "tool grant binding keys cannot be empty".to_string(),
                });
            }
        }
        Ok(())
    }

    pub fn validate_all(grants: &[Self]) -> Result<(), RemoteProtocolError> {
        let mut seen_ids = HashSet::new();
        let mut seen_names = HashSet::new();
        let mut seen_call_paths = HashSet::new();
        for grant in grants {
            grant.validate()?;
            if !seen_ids.insert(grant.id.clone()) {
                return Err(RemoteProtocolError::InvalidToolGrant {
                    tool_name: grant.name.clone(),
                    message: format!("duplicate tool grant id `{}`", grant.id),
                });
            }
            if !seen_names.insert(grant.name.clone()) {
                return Err(RemoteProtocolError::InvalidToolGrant {
                    tool_name: grant.name.clone(),
                    message: format!("duplicate tool grant name `{}`", grant.name),
                });
            }
            for call_path in grant.call_path_bindings()? {
                if !seen_call_paths.insert(call_path.clone()) {
                    return Err(RemoteProtocolError::DuplicateRemoteCallPath { call_path });
                }
            }
        }
        Ok(())
    }

    pub fn call_path_bindings(&self) -> Result<Vec<String>, RemoteProtocolError> {
        let mut paths = BTreeSet::new();
        for (key, value) in &self.bindings {
            if let Some(binding) = RemoteCallPathBinding::from_value(value) {
                validate_call_path_binding(&self.name, key, &binding)?;
                paths.insert(format!(
                    "{}.{}",
                    binding.module_path.join("."),
                    binding.operation
                ));
            }
        }
        Ok(paths.into_iter().collect())
    }

    fn required_call_path_binding(
        &self,
        binding_key: &str,
    ) -> Result<RemoteCallPathBinding, RemoteProtocolError> {
        let Some(value) = self.bindings.get(binding_key) else {
            return Err(RemoteProtocolError::MissingToolBinding {
                tool_name: self.name.clone(),
                binding: binding_key.to_string(),
            });
        };
        let Some(binding) = RemoteCallPathBinding::from_value(value) else {
            return Err(RemoteProtocolError::InvalidToolGrant {
                tool_name: self.name.clone(),
                message: format!("tool binding `{binding_key}` does not expose a call path"),
            });
        };
        validate_call_path_binding(&self.name, binding_key, &binding)?;
        Ok(binding)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RemoteCallPathBinding {
    pub module_path: Vec<String>,
    pub operation: String,
}

impl RemoteCallPathBinding {
    fn from_value(value: &serde_json::Value) -> Option<Self> {
        let module_path = value
            .get("module_path")?
            .as_array()?
            .iter()
            .map(|part| part.as_str().map(ToOwned::to_owned))
            .collect::<Option<Vec<_>>>()?;
        let operation = value.get("operation")?.as_str()?.to_string();
        Some(Self {
            module_path,
            operation,
        })
    }
}

fn validate_call_path_binding(
    tool_name: &str,
    binding_key: &str,
    binding: &RemoteCallPathBinding,
) -> Result<(), RemoteProtocolError> {
    if binding.module_path.is_empty() {
        return Err(RemoteProtocolError::InvalidToolGrant {
            tool_name: tool_name.to_string(),
            message: format!("tool binding `{binding_key}` requires an explicit module path"),
        });
    }
    if binding
        .module_path
        .iter()
        .any(|part| part.trim().is_empty())
    {
        return Err(RemoteProtocolError::InvalidToolGrant {
            tool_name: tool_name.to_string(),
            message: format!(
                "tool binding `{binding_key}` module path cannot contain empty segments"
            ),
        });
    }
    if binding.operation.trim().is_empty() {
        return Err(RemoteProtocolError::InvalidToolGrant {
            tool_name: tool_name.to_string(),
            message: format!("tool binding `{binding_key}` requires an explicit operation"),
        });
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RemoteToolActivation {
    #[default]
    Always,
    Internal,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteToolOutputContract {
    #[default]
    Static,
    FromInputSchema {
        input_field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default_schema: Option<serde_json::Value>,
    },
}

impl RemoteToolOutputContract {
    pub(crate) fn is_static(&self) -> bool {
        matches!(self, Self::Static)
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RemoteToolArgumentProjectionPolicy {
    #[default]
    MaterializeProjectedValues,
    PreserveProjectedRefsInField {
        field: String,
    },
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RemoteToolRetryPolicy {
    #[default]
    Never,
    Safe {
        max_attempts: u32,
        base_delay_ms: u64,
        max_delay_ms: u64,
    },
    Idempotent {
        max_attempts: u32,
        base_delay_ms: u64,
        max_delay_ms: u64,
    },
}
