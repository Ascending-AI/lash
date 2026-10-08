//! The attachment-store cause that survives a journal or plugin boundary.

use crate::attachments::{
    AttachmentStoreError, AttachmentStoreFailureClass, ContentMismatchDetail,
};
use crate::runtime_error::{RuntimeEffectControllerError, RuntimeErrorCause, RuntimeErrorCode};
use crate::{AttachmentId, MediaType, StoreError};

/// Structured attachment failure evidence. Backend diagnostics remain on the
/// runtime error's message; classification and refusal data remain typed.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AttachmentRetentionFailure {
    NotFound {
        attachment_id: AttachmentId,
    },
    SizeLimitExceeded {
        byte_len: u64,
        max_bytes: u64,
    },
    ReadLimitExceeded {
        byte_len: u64,
        max_bytes: u64,
    },
    RequestBudgetExceeded {
        max_bytes: u64,
    },
    /// No form the request accepted can carry this attachment.
    DeliveryUnsupported {
        attachment_id: AttachmentId,
        media_type: MediaType,
    },
    /// The stored content disagrees with the reference that names it.
    ContentMismatch {
        attachment_id: AttachmentId,
        detail: AttachmentContentMismatch,
    },
    Io {
        path: std::path::PathBuf,
        raw_os_error: Option<i32>,
    },
    ReferrersOperationFailed {
        operation: String,
        attachment_id: AttachmentId,
        source: Box<AttachmentRetentionStoreFailure>,
    },
    WriteRollbackFailed {
        attachment_id: AttachmentId,
        write_error: Box<Self>,
        abort_error: Box<AttachmentRetentionStoreFailure>,
    },
    Backend {
        operation: String,
        class: AttachmentStoreFailureClass,
    },
    Contract,
    RootSetEnumerationFailed {
        source: Box<AttachmentRetentionStoreFailure>,
    },
    RootSetOperationFailed {
        operation: String,
        source: Box<AttachmentRetentionStoreFailure>,
    },
    ReclamationInFlight {
        attachment_id: AttachmentId,
        attempts: u32,
    },
}

/// Which claim of a reference its stored content failed.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "claim", rename_all = "snake_case")]
pub enum AttachmentContentMismatch {
    Length { expected: u64, actual: u64 },
    Digest,
}

/// A nested store cause with the store's authoritative retry class and its
/// runtime code and structured refusal. The diagnostic is carried separately.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum AttachmentRetentionStoreFailure {
    Transient {
        #[schemars(with = "String")]
        code: RuntimeErrorCode,
        cause: Option<Box<RuntimeErrorCause>>,
    },
    Refused {
        #[schemars(with = "String")]
        code: RuntimeErrorCode,
        cause: Option<Box<RuntimeErrorCause>>,
    },
}

impl AttachmentRetentionStoreFailure {
    fn of(error: &StoreError) -> Box<Self> {
        let mapped = RuntimeEffectControllerError::from(error);
        let cause = mapped.cause.map(Box::new);
        Box::new(if error.is_transient() {
            Self::Transient {
                code: mapped.code,
                cause,
            }
        } else {
            Self::Refused {
                code: mapped.code,
                cause,
            }
        })
    }

    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Transient { .. } => true,
            Self::Refused { .. } => false,
        }
    }
}

impl AttachmentRetentionFailure {
    /// The tool-visible class of a failed attachment retention. This does
    /// not authorize replaying the external tool that produced the attachment.
    pub fn tool_failure_class(&self) -> lash_sansio::ToolFailureClass {
        use lash_sansio::ToolFailureClass as C;
        match self {
            Self::SizeLimitExceeded { .. }
            | Self::ReadLimitExceeded { .. }
            | Self::RequestBudgetExceeded { .. } => C::ResourceLimit,
            Self::DeliveryUnsupported { .. } => C::InvalidRequest,
            Self::NotFound { .. } | Self::ContentMismatch { .. } | Self::Contract => C::Internal,
            Self::Io { .. } => C::Io,
            Self::Backend { class, .. } => match class {
                AttachmentStoreFailureClass::Transient => C::Unavailable,
                AttachmentStoreFailureClass::Credentials => C::PermissionDenied,
                AttachmentStoreFailureClass::Terminal => C::External,
            },
            Self::ReferrersOperationFailed { source, .. }
            | Self::RootSetOperationFailed { source, .. }
            | Self::RootSetEnumerationFailed { source } => match source.as_ref() {
                AttachmentRetentionStoreFailure::Transient { .. } => C::Unavailable,
                AttachmentRetentionStoreFailure::Refused { .. } => C::Internal,
            },
            Self::WriteRollbackFailed { write_error, .. } => write_error.tool_failure_class(),
            Self::ReclamationInFlight { .. } => C::Unavailable,
        }
    }

    /// Whether retrying the identical retention can succeed.
    pub fn is_retryable(&self) -> bool {
        match self {
            Self::Backend { class, .. } => class.is_retryable(),
            Self::RootSetOperationFailed { source, .. }
            | Self::ReferrersOperationFailed { source, .. } => source.is_retryable(),
            Self::WriteRollbackFailed {
                write_error,
                abort_error,
                ..
            } => write_error.is_retryable() && abort_error.is_retryable(),
            Self::ReclamationInFlight { .. } => true,
            Self::NotFound { .. }
            | Self::SizeLimitExceeded { .. }
            | Self::ReadLimitExceeded { .. }
            | Self::RequestBudgetExceeded { .. }
            | Self::DeliveryUnsupported { .. }
            | Self::ContentMismatch { .. }
            | Self::Io { .. }
            | Self::Contract
            | Self::RootSetEnumerationFailed { .. } => false,
        }
    }
}

impl AttachmentStoreError {
    /// Capture the typed failure while retaining backend diagnostics on the live source.
    pub fn retention_failure(&self) -> AttachmentRetentionFailure {
        use AttachmentRetentionFailure as F;
        match self {
            Self::NotFound(id) => F::NotFound {
                attachment_id: id.clone(),
            },
            Self::SizeLimitExceeded {
                byte_len,
                max_bytes,
            } => F::SizeLimitExceeded {
                byte_len: *byte_len,
                max_bytes: *max_bytes,
            },
            Self::ReadLimitExceeded {
                byte_len,
                max_bytes,
            } => F::ReadLimitExceeded {
                byte_len: *byte_len,
                max_bytes: *max_bytes,
            },
            Self::RequestBudgetExceeded { max_bytes } => F::RequestBudgetExceeded {
                max_bytes: *max_bytes,
            },
            Self::DeliveryUnsupported { id, media_type } => F::DeliveryUnsupported {
                attachment_id: id.clone(),
                media_type: media_type.clone(),
            },
            Self::ContentMismatch { id, detail } => F::ContentMismatch {
                attachment_id: id.clone(),
                detail: match *detail {
                    ContentMismatchDetail::Length { expected, actual } => {
                        AttachmentContentMismatch::Length { expected, actual }
                    }
                    ContentMismatchDetail::Digest => AttachmentContentMismatch::Digest,
                },
            },
            Self::Io { path, source } => F::Io {
                path: path.clone(),
                raw_os_error: source.raw_os_error(),
            },
            Self::ReferrersOperationFailed {
                operation,
                attachment_id,
                source,
            } => F::ReferrersOperationFailed {
                operation: (*operation).to_string(),
                attachment_id: attachment_id.clone(),
                source: AttachmentRetentionStoreFailure::of(source),
            },
            Self::WriteRollbackFailed {
                attachment_id,
                write_error,
                abort_error,
            } => F::WriteRollbackFailed {
                attachment_id: attachment_id.clone(),
                write_error: Box::new(write_error.retention_failure()),
                abort_error: AttachmentRetentionStoreFailure::of(abort_error),
            },
            Self::Backend {
                operation, class, ..
            } => F::Backend {
                operation: (*operation).to_string(),
                class: *class,
            },
            Self::Contract(_) => F::Contract,
            Self::RootSetEnumerationFailed { source } => F::RootSetEnumerationFailed {
                source: AttachmentRetentionStoreFailure::of(source),
            },
            Self::RootSetOperationFailed { operation, source } => F::RootSetOperationFailed {
                operation: (*operation).to_string(),
                source: AttachmentRetentionStoreFailure::of(source),
            },
            Self::ReclamationInFlight {
                attachment_id,
                attempts,
            } => F::ReclamationInFlight {
                attachment_id: attachment_id.clone(),
                attempts: *attempts,
            },
        }
    }
}

impl RuntimeEffectControllerError {
    /// Refuse a required retention without granting retry authority to permanent causes.
    pub fn output_retention_failed(source: &AttachmentStoreError) -> Self {
        let retryable = source.is_retryable();
        let mut error = Self::new(
            if retryable {
                RuntimeErrorCode::OutputRetentionFailed
            } else {
                RuntimeErrorCode::OutputRetentionRefused
            },
            format!("retaining the full output as an attachment failed: {source}"),
        );
        error.cause = Some(RuntimeErrorCause::AttachmentRetention {
            failure: Box::new(source.retention_failure()),
        });
        if retryable {
            error.retryable_uncommitted_derivation()
        } else {
            error
        }
    }
}
