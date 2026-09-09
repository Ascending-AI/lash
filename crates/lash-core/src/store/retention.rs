//! Explicit host policy for terminal-session evidence reclamation (FIG-653).

/// Host-selected exclusive horizon for commit evidence.
///
/// Only receipts in a durably deleted session and strictly before this bound
/// are eligible. No clock or live configuration is consulted by reclamation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RetentionBound {
    /// Exclusive commit timestamp horizon in milliseconds since Unix epoch.
    pub committed_before_epoch_ms: u64,
}

/// Committed counts from one atomic, factory-wide evidence sweep.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct RetentionReport {
    /// Terminal-session receipts removed before the host's horizon.
    pub removed_receipt_count: usize,
    /// Terminal-session usage rows whose owning receipt no longer exists.
    pub removed_usage_delta_count: usize,
    /// Deleted-owner manifest rows no surviving graph prefix needs.
    pub removed_attachment_root_count: usize,
    /// Session-free runtime-operation scopes retired by this sweep: their
    /// owning operation had recorded its receipt and nothing was live under
    /// them any more, so their effect journal, groups, and promises were
    /// deleted and the scope fenced (ADR 0049, ADR 0067).
    pub retired_effect_scope_count: usize,
}

impl super::MaintenanceReport for RetentionReport {
    fn reclaimed_count(&self) -> usize {
        self.removed_receipt_count
            + self.removed_usage_delta_count
            + self.removed_attachment_root_count
            + self.retired_effect_scope_count
    }
}

/// The commit key under which a plugin operation records its receipt: the
/// durable proof the reclaim sweep reads before retiring the operation's
/// effect scope.
pub const PLUGIN_OPERATION_STATE_RECEIPT_KEY: &str = "plugin-operation-state";

/// The `runtime_turn_commits` storage key of the receipt a plugin operation
/// under `scope` records when it completes. A scope with effect rows and no
/// such receipt is still owned by a live operation (or one that failed before
/// recording anything) and is not the sweep's to retire.
pub fn plugin_operation_receipt_storage_key(
    scope: &crate::ExecutionScope,
) -> Result<String, crate::StoreError> {
    crate::OperationId::new(scope.clone(), PLUGIN_OPERATION_STATE_RECEIPT_KEY).storage_key()
}

/// The runtime-operation id segment the facade mints for a plugin command.
pub const FACADE_PLUGIN_COMMAND_OPERATION_TAG: &str = "plugin_command";
/// The runtime-operation id segment the facade mints for a plugin task.
pub const FACADE_PLUGIN_TASK_OPERATION_TAG: &str = "plugin_task";

/// Which facade lever minted a runtime-operation id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FacadePluginOperation {
    /// `run_plugin_command`.
    Command,
    /// `run_plugin_task`.
    Task,
}

impl FacadePluginOperation {
    fn tag(self) -> &'static str {
        match self {
            Self::Command => FACADE_PLUGIN_COMMAND_OPERATION_TAG,
            Self::Task => FACADE_PLUGIN_TASK_OPERATION_TAG,
        }
    }
}

/// Mint the runtime-operation id the facade gives one plugin command or task
/// invocation: `{session_id}:{plugin_command|plugin_task}:{name}:{TurnActivityId}`.
///
/// The one owner of the convention: [`is_facade_minted_operation_id`] reads
/// exactly what this writes, and the retention sweep retires only scopes this
/// mint produced (ADR 0067). A caller that names its own scope through
/// `run_plugin_command(.., scope)` never passes through here, so its scope is
/// never swept.
pub fn mint_facade_operation_id(
    session_id: &str,
    operation: FacadePluginOperation,
    name: &str,
) -> String {
    let activity = crate::TurnActivityId::new(uuid::Uuid::new_v4().to_string());
    format!("{session_id}:{}:{name}:{}", operation.tag(), activity.0)
}

/// Whether `operation_id` is a runtime-operation id the facade minted through
/// [`mint_facade_operation_id`]: a `plugin_command` or `plugin_task` segment
/// followed by a non-empty name and a trailing hyphenated-UUID activity id.
///
/// The sweep's eligibility predicate: caller-supplied ids — a stable request
/// id a host retries after a lost response, or any id without the trailing
/// activity segment — answer `false` and are retired only by their owner's
/// explicit retirement.
#[must_use]
pub fn is_facade_minted_operation_id(operation_id: &str) -> bool {
    let Some((head, activity)) = operation_id.rsplit_once(':') else {
        return false;
    };
    if activity.len() != 36 || uuid::Uuid::parse_str(activity).is_err() {
        return false;
    }
    let Some((head, name)) = head.rsplit_once(':') else {
        return false;
    };
    if name.is_empty() {
        return false;
    }
    let Some((session_id, tag)) = head.rsplit_once(':') else {
        return false;
    };
    !session_id.is_empty()
        && (tag == FACADE_PLUGIN_COMMAND_OPERATION_TAG || tag == FACADE_PLUGIN_TASK_OPERATION_TAG)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Drift pin: the mint and the predicate agree, and neither a caller's
    /// stable id nor a lookalike without the activity segment passes.
    #[test]
    fn facade_mint_and_sweep_predicate_agree() {
        let command = mint_facade_operation_id("sess-1", FacadePluginOperation::Command, "deploy");
        let task = mint_facade_operation_id("sess-1", FacadePluginOperation::Task, "index");
        assert!(command.starts_with("sess-1:plugin_command:deploy:"));
        assert!(task.starts_with("sess-1:plugin_task:index:"));
        assert!(is_facade_minted_operation_id(&command));
        assert!(is_facade_minted_operation_id(&task));
        assert!(!is_facade_minted_operation_id(
            "caller-supplied-stable-request"
        ));
        assert!(!is_facade_minted_operation_id(
            "sess-1:plugin_command:deploy"
        ));
        assert!(!is_facade_minted_operation_id(
            "sess-1:plugin_command:deploy:not-an-activity-id"
        ));
        assert!(!is_facade_minted_operation_id(
            "sess-1:plugin_other:deploy:0f4b9a8e-6f3d-4a4e-9d0e-3a1c2b4d5e6f"
        ));
        assert!(!is_facade_minted_operation_id(
            ":plugin_command:deploy:0f4b9a8e-6f3d-4a4e-9d0e-3a1c2b4d5e6f"
        ));
        assert!(!is_facade_minted_operation_id(
            "sess-1:plugin_command::0f4b9a8e-6f3d-4a4e-9d0e-3a1c2b4d5e6f"
        ));
    }
}
