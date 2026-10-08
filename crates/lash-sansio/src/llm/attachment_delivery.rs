//! Provider acceptance and transient attachment deliveries.
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    Hash,
    PartialOrd,
    Ord,
    Serialize,
    Deserialize,
    schemars::JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum AttachmentPosition {
    Message,
    ToolResult,
}

#[derive(
    Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DeliveryForms {
    pub bytes: bool,
    pub url: bool,
    pub provider_file: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderFileScope {
    pub provider: String,
    pub endpoint: String,
    pub credential_scope: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ProviderAccepts {
    pub bytes: bool,
    pub url: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_file: Option<ProviderFileScope>,
}
impl ProviderAccepts {
    pub const NONE: Self = Self {
        bytes: false,
        url: false,
        provider_file: None,
    };
    pub fn is_empty(&self) -> bool {
        !self.bytes && !self.url && self.provider_file.is_none()
    }
    pub fn narrowed(&self, host: DeliveryForms) -> Self {
        Self {
            bytes: self.bytes && host.bytes,
            url: self.url && host.url,
            provider_file: self.provider_file.clone().filter(|_| host.provider_file),
        }
    }
    pub fn narrowed_to_live_scope(&self, live: Option<&ProviderFileScope>) -> Self {
        Self {
            bytes: self.bytes,
            url: self.url,
            provider_file: self
                .provider_file
                .clone()
                .filter(|scope| Some(scope) == live),
        }
    }
    pub fn allows(&self, delivery: &Delivery) -> bool {
        match delivery {
            Delivery::Bytes(_) => self.bytes,
            Delivery::Url { .. } => self.url,
            Delivery::ProviderFile { scope, .. } => self.provider_file.as_ref() == Some(scope),
        }
    }
}

/// Live wire material. Its debug representation cannot disclose the value.
pub struct DeliverySecret(String);
impl DeliverySecret {
    pub fn new(value: String) -> Self {
        Self(value)
    }
    pub fn expose(&self) -> &str {
        &self.0
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}
impl fmt::Debug for DeliverySecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("DeliverySecret(<redacted>)")
    }
}

pub enum Delivery {
    Bytes(Vec<u8>),
    Url {
        url: DeliverySecret,
        valid_until_ms: Option<u64>,
    },
    ProviderFile {
        scope: ProviderFileScope,
        id: DeliverySecret,
        valid_until_ms: Option<u64>,
        /// Whether this delivery read the attachment's bytes to upload them
        /// (a cache miss) rather than reusing a file already uploaded. The
        /// request budget charges the upload's scratch only then.
        uploaded: bool,
    },
}
impl Delivery {
    pub fn forms(&self) -> DeliveryForms {
        DeliveryForms {
            bytes: matches!(self, Self::Bytes(_)),
            url: matches!(self, Self::Url { .. }),
            provider_file: matches!(self, Self::ProviderFile { .. }),
        }
    }
    pub fn secret(&self) -> Option<&DeliverySecret> {
        match self {
            Self::Bytes(_) => None,
            Self::Url { url, .. } => Some(url),
            Self::ProviderFile { id, .. } => Some(id),
        }
    }
}
impl fmt::Debug for Delivery {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Bytes(bytes) => f
                .debug_struct("Bytes")
                .field("len", &bytes.len())
                .field("capacity", &bytes.capacity())
                .finish(),
            Self::Url { valid_until_ms, .. } => f
                .debug_struct("Url")
                .field("valid_until_ms", valid_until_ms)
                .finish_non_exhaustive(),
            Self::ProviderFile {
                scope,
                valid_until_ms,
                ..
            } => f
                .debug_struct("ProviderFile")
                .field("scope", scope)
                .field("valid_until_ms", valid_until_ms)
                .finish_non_exhaustive(),
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeliveryLimits {
    /// The most bytes a `Bytes` delivery may hold: what the request budget
    /// still affords once every occurrence is encoded inline.
    pub max_bytes: u64,
    /// The most bytes an upload behind a `ProviderFile` delivery may read:
    /// the scratch the request budget still affords. Never below
    /// `max_bytes`, since a file id is not encoded inline.
    pub max_upload_bytes: u64,
    pub valid_through_ms: u64,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeliveryContext {
    pub valid_through_ms: u64,
    pub live_file_scope: Option<ProviderFileScope>,
}

/// Provider fetch slack after a call's deadline, retained in its admitted template.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DeliveryFetchHorizon {
    /// Zero requests validity only through the call's deadline.
    pub millis: u64,
}
impl DeliveryFetchHorizon {
    /// Standard preset: 60,000 milliseconds. No workload measurements justify this slack.
    pub const fn standard() -> Self {
        Self { millis: 60_000 }
    }
}
impl Default for DeliveryFetchHorizon {
    fn default() -> Self {
        Self::standard()
    }
}
