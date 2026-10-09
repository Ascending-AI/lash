use crate::LashVmHostCatalog;
use crate::runtime::{LASH_HOST_DESCRIPTOR_TYPE_KEY, LASH_HOST_DESCRIPTOR_VALUE_KEY};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct HostDescriptor {
    pub source_type: String,
    pub value: serde_json::Value,
}

impl HostDescriptor {
    pub fn new(source_type: impl Into<String>, value: serde_json::Value) -> Self {
        Self {
            source_type: source_type.into(),
            value,
        }
    }

    pub fn decode(source: &serde_json::Value) -> Result<Self, HostDescriptorError> {
        let source_type = source
            .get(LASH_HOST_DESCRIPTOR_TYPE_KEY)
            .and_then(serde_json::Value::as_str)
            .map(ToOwned::to_owned)
            .ok_or(HostDescriptorError::InvalidHostDescriptor)?;
        let value = source
            .get(LASH_HOST_DESCRIPTOR_VALUE_KEY)
            .cloned()
            .ok_or(HostDescriptorError::InvalidHostDescriptor)?;
        Ok(Self { source_type, value })
    }

    pub fn encode(
        source_type: impl Into<String>,
        value: impl Serialize,
    ) -> Result<serde_json::Value, HostDescriptorError> {
        let source_type = source_type.into();
        let value =
            serde_json::to_value(value).map_err(|err| HostDescriptorError::MalformedPayload {
                source_type: source_type.clone(),
                message: err.to_string(),
            })?;
        Ok(Self::new(source_type, value).to_json())
    }

    pub fn decode_as<T: serde::de::DeserializeOwned>(
        &self,
        resources: &LashVmHostCatalog,
    ) -> Result<T, HostDescriptorError> {
        resources.decode_host_descriptor_as(&self.source_type, self.value.clone())
    }

    pub fn to_json(&self) -> serde_json::Value {
        serde_json::json!({
            LASH_HOST_DESCRIPTOR_TYPE_KEY: self.source_type,
            LASH_HOST_DESCRIPTOR_VALUE_KEY: self.value,
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[non_exhaustive]
pub enum HostDescriptorError {
    #[error("host descriptor must be a host descriptor constructor result")]
    InvalidHostDescriptor,
    #[error("host descriptor `{source_type}` is not declared in the host catalog")]
    UnknownSourceType { source_type: String },
    #[error("host descriptor `{source_type}` payload is invalid: {message}")]
    MalformedPayload {
        source_type: String,
        message: String,
    },
}

#[cfg(test)]
mod tests;
