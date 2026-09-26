use lash_sansio::{ExecutionNodeKind, ProcessId, WorkflowExecutionSite};
use serde::{Deserialize, Serialize};

use crate::{ModuleRef, ProcessRef, WorkflowNodePath, workflow_node_id};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LashlangExecutionContext {
    pub(crate) entry: LashlangExecutionEntry,
}

impl LashlangExecutionContext {
    pub(crate) fn main() -> Self {
        Self {
            entry: LashlangExecutionEntry::Main,
        }
    }

    pub(crate) fn process(process_name: impl Into<String>) -> Self {
        Self {
            entry: LashlangExecutionEntry::Process {
                process_name: process_name.into(),
            },
        }
    }

    pub(crate) fn builder(&self) -> LashlangExecutionSiteBuilder<'_> {
        LashlangExecutionSiteBuilder { context: self }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LashlangExecutionEntry {
    Main,
    Process { process_name: String },
}

impl LashlangExecutionEntry {
    fn workflow_owner(&self) -> String {
        match self {
            Self::Main => "main".to_string(),
            Self::Process { process_name, .. } => format!("process:{process_name}"),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LashlangExecutionSiteBuilder<'context> {
    context: &'context LashlangExecutionContext,
}

impl LashlangExecutionSiteBuilder<'_> {
    pub(crate) fn node_site(
        &self,
        node_path: &WorkflowNodePath,
        kind: ExecutionNodeKind,
        label: impl Into<String>,
    ) -> LashlangExecutionSite {
        let label = label.into();
        LashlangExecutionSite {
            node_id: workflow_node_id(&self.context.entry.workflow_owner(), node_path.indices())
                .to_string(),
            node_kind: kind,
            label: label.clone(),
            branch: None,
            workflow_site: WorkflowExecutionSite::new(
                self.context.entry.workflow_owner(),
                node_path.indices(),
                kind,
                label,
            ),
        }
    }

    pub(crate) fn branch_site(&self, node_path: &WorkflowNodePath) -> LashlangExecutionSite {
        LashlangExecutionSite {
            node_id: workflow_node_id(&self.context.entry.workflow_owner(), node_path.indices())
                .to_string(),
            node_kind: ExecutionNodeKind::Branch,
            label: "if".to_string(),
            branch: Some(LashlangBranchSite {
                then_edge_id: self.branch_edge_id(node_path, ProcessBranchSelection::Then),
                else_edge_id: self.branch_edge_id(node_path, ProcessBranchSelection::Else),
            }),
            workflow_site: WorkflowExecutionSite::new(
                self.context.entry.workflow_owner(),
                node_path.indices(),
                ExecutionNodeKind::Branch,
                "if",
            ),
        }
    }

    pub(crate) fn branch_edge_id(
        &self,
        path: &WorkflowNodePath,
        selection: ProcessBranchSelection,
    ) -> String {
        let label = match selection {
            ProcessBranchSelection::Then => "then",
            ProcessBranchSelection::Else => "else",
        };
        format!(
            "{}:{label}",
            workflow_node_id(&self.context.entry.workflow_owner(), path.indices())
        )
    }
}

pub fn process_ref_key(process_ref: &ProcessRef) -> String {
    format!("{}:{}", process_ref.component, process_ref.pos)
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashlangExecutionSite {
    pub node_id: String,
    pub node_kind: ExecutionNodeKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<LashlangBranchSite>,
    pub workflow_site: WorkflowExecutionSite,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LashlangExecutionCallSite {
    pub site: LashlangExecutionSite,
    pub occurrence: u64,
}

/// Typed provenance for a failed external effect.
///
/// This is projected from the VM's host error when emitting a node terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LashlangEffectFailure {
    pub class: lash_sansio::ToolFailureClass,
    pub code: String,
    pub message: String,
    pub replay_key: String,
    pub source: lash_sansio::ToolFailureSource,
    pub retry: lash_sansio::ToolRetryStatus,
}

/// Why one observed Lashlang node failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LashlangExecutionFailure {
    /// A host effect returned a typed tool failure.
    Effect(LashlangEffectFailure),
    /// The VM itself refused or failed the operation.
    Runtime {
        /// Stable [`crate::RuntimeError::code`] value (or `ProcessFailed` for
        /// the explicit process `fail` terminal).
        code: String,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashlangBranchSite {
    pub then_edge_id: String,
    pub else_edge_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessBranchSelection {
    Then,
    Else,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LashlangExecutionChild {
    pub process_id: ProcessId,
    pub attempt: Option<u32>,
    pub module_ref: ModuleRef,
    pub process_ref: ProcessRef,
    pub process_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LashlangExecutionObservation {
    NodeStarted {
        site: LashlangExecutionSite,
        occurrence: u64,
    },
    ChildProcessWaiting {
        site: LashlangExecutionSite,
        occurrence: u64,
        process_ids: Vec<ProcessId>,
    },
    NodeResumed {
        site: LashlangExecutionSite,
        occurrence: u64,
    },
    NodeCompleted {
        site: LashlangExecutionSite,
        occurrence: u64,
    },
    NodeFailed {
        site: LashlangExecutionSite,
        occurrence: u64,
        failure: LashlangExecutionFailure,
    },
    BranchSelected {
        site: LashlangExecutionSite,
        occurrence: u64,
        edge_id: String,
        selected: ProcessBranchSelection,
    },
    ChildStarted {
        site: LashlangExecutionSite,
        occurrence: u64,
        child: LashlangExecutionChild,
    },
}
