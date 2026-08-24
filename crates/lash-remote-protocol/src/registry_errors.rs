//! The protocol error type, plus the remote tool-registry contract and its
//! reopen-stability assertion.

use crate::tools::RemoteToolGrant;

pub trait RemoteToolRegistry {
    fn grants(&self) -> Vec<RemoteToolGrant>;

    fn validate_registry(&self) -> Result<(), RemoteProtocolError> {
        RemoteToolGrant::validate_all(&self.grants())
    }
}

pub fn assert_remote_tool_registry_reopenable(
    before: &dyn RemoteToolRegistry,
    after_reopen: &dyn RemoteToolRegistry,
) -> Result<(), RemoteProtocolError> {
    let before_grants = before.grants();
    let after_grants = after_reopen.grants();
    RemoteToolGrant::validate_all(&before_grants)?;
    RemoteToolGrant::validate_all(&after_grants)?;
    let before_paths = remote_registry_call_paths(&before_grants)?;
    let after_paths = remote_registry_call_paths(&after_grants)?;
    if before_paths != after_paths {
        return Err(RemoteProtocolError::RemoteToolRegistryReopenMismatch {
            before_call_paths: before_paths,
            after_call_paths: after_paths,
        });
    }
    Ok(())
}

fn remote_registry_call_paths(
    grants: &[RemoteToolGrant],
) -> Result<Vec<String>, RemoteProtocolError> {
    let mut call_paths = grants
        .iter()
        .map(RemoteToolGrant::call_path_bindings)
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .collect::<Vec<_>>();
    call_paths.sort();
    Ok(call_paths)
}

pub(crate) fn require_non_empty(
    type_name: &'static str,
    field: &'static str,
    value: &str,
) -> Result<(), RemoteProtocolError> {
    if value.trim().is_empty() {
        Err(RemoteProtocolError::MissingRequiredField { type_name, field })
    } else {
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
pub enum RemoteProtocolError {
    #[error("unsupported remote protocol version {actual}; expected {expected}")]
    UnsupportedProtocolVersion { actual: u32, expected: u32 },
    #[error(
        "mismatched protocol version in {parent}.{child}: got {child_version}, expected {parent_version}"
    )]
    MismatchedNestedProtocolVersion {
        parent: &'static str,
        child: &'static str,
        parent_version: u32,
        child_version: u32,
    },
    #[error("{type_name}.{field} is required")]
    MissingRequiredField {
        type_name: &'static str,
        field: &'static str,
    },
    #[error("invalid {type_name}: {message}")]
    InvalidEnvelope {
        type_name: &'static str,
        message: String,
    },
    #[error("duplicate LLM call summary `{call_id}` in RemoteTurnReport.llm_calls")]
    DuplicateLlmCallSummary { call_id: String },
    #[error("duplicate ModelCallRecorded activity for LLM call `{call_id}`")]
    DuplicateLlmCallActivity { call_id: String },
    #[error("LLM call summary `{call_id}` has no matching ModelCallRecorded activity")]
    MissingLlmCallActivity { call_id: String },
    #[error("ModelCallRecorded activity `{call_id}` has no matching LLM call summary")]
    MissingLlmCallSummary { call_id: String },
    #[error(
        "LLM call `{call_id}` differs between the result summary and ModelCallRecorded activity"
    )]
    ConflictingLlmCallRecord { call_id: String },
    #[error("invalid attachment data `{id}`: {message}")]
    InvalidAttachmentData { id: String, message: String },
    #[error("invalid attachment reference `{id}`: {message}")]
    InvalidAttachmentRef { id: String, message: String },
    #[error("turn input is not remote-safe: {0}")]
    NonRemoteSafeTurnInput(String),
    #[error("remote tool grant `{tool_name}` is missing required binding `{binding}`")]
    MissingToolBinding { tool_name: String, binding: String },
    #[error("invalid remote tool grant `{tool_name}`: {message}")]
    InvalidToolGrant { tool_name: String, message: String },
    #[error("duplicate remote tool call path `{call_path}`")]
    DuplicateRemoteCallPath { call_path: String },
    #[error(
        "remote tool registry changed across reopen: before={before_call_paths:?}, after={after_call_paths:?}"
    )]
    RemoteToolRegistryReopenMismatch {
        before_call_paths: Vec<String>,
        after_call_paths: Vec<String>,
    },
    #[error("failed to serialize remote activity: {0}")]
    ActivitySerialization(#[from] serde_json::Error),
    #[error("failed to decode remote message: {0}")]
    MessageDecode(#[source] serde_json::Error),
    #[error("failed to write remote activity: {0}")]
    ActivityWrite(String),
}
