use lash_sansio::{ExecutionNodeKind, ProcessId, WorkflowExecutionSite};
use serde::{Deserialize, Serialize};

use crate::{ModuleRef, ProcessRef, WorkflowNodePath, workflow_node_id};

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct LashVmExecutionContext {
    pub(crate) entry: LashVmExecutionEntry,
}

impl LashVmExecutionContext {
    pub(crate) fn main() -> Self {
        Self {
            entry: LashVmExecutionEntry::Main,
        }
    }

    pub(crate) fn process(process_name: impl Into<String>) -> Self {
        Self {
            entry: LashVmExecutionEntry::Process {
                process_name: process_name.into(),
            },
        }
    }

    pub(crate) fn builder(&self) -> LashVmExecutionSiteBuilder<'_> {
        LashVmExecutionSiteBuilder { context: self }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum LashVmExecutionEntry {
    Main,
    Process { process_name: String },
}

impl LashVmExecutionEntry {
    fn workflow_owner(&self) -> String {
        match self {
            Self::Main => "main".to_string(),
            Self::Process { process_name, .. } => format!("process:{process_name}"),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct LashVmExecutionSiteBuilder<'context> {
    context: &'context LashVmExecutionContext,
}

impl LashVmExecutionSiteBuilder<'_> {
    pub(crate) fn node_site(
        &self,
        node_path: &WorkflowNodePath,
        kind: ExecutionNodeKind,
        label: impl Into<String>,
    ) -> LashVmExecutionSite {
        let label = label.into();
        LashVmExecutionSite {
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

    pub(crate) fn branch_site(&self, node_path: &WorkflowNodePath) -> LashVmExecutionSite {
        LashVmExecutionSite {
            node_id: workflow_node_id(&self.context.entry.workflow_owner(), node_path.indices())
                .to_string(),
            node_kind: ExecutionNodeKind::Branch,
            label: "if".to_string(),
            branch: Some(LashVmBranchSite {
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
pub struct LashVmExecutionSite {
    pub node_id: String,
    pub node_kind: ExecutionNodeKind,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<LashVmBranchSite>,
    pub workflow_site: WorkflowExecutionSite,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashVmExecutionCallSite {
    pub site: LashVmExecutionSite,
    pub occurrence: u64,
}

/// Typed provenance for a failed external effect.
///
/// This is projected from the VM's host error when emitting a node terminal.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashVmEffectFailure {
    pub class: lash_sansio::ToolFailureClass,
    pub code: String,
    pub message: String,
    pub replay_key: String,
    pub source: lash_sansio::ToolFailureSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_delay_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cause: Option<Box<lash_sansio::ToolFailureCause>>,
}

/// Why one observed Lash VM node failed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LashVmExecutionFailure {
    /// A host effect returned a typed tool failure.
    Effect(LashVmEffectFailure),
    /// The VM itself refused or failed the operation.
    Runtime {
        /// Stable [`crate::RuntimeError::code`] value (or `ProcessFailed` for
        /// the explicit process `fail` terminal).
        code: String,
        message: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashVmBranchSite {
    pub then_edge_id: String,
    pub else_edge_id: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessBranchSelection {
    Then,
    Else,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashVmExecutionChild {
    pub process_id: ProcessId,
    pub attempt: Option<u32>,
    pub module_ref: ModuleRef,
    pub process_ref: ProcessRef,
    pub process_name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LashVmExecutionObservation {
    NodeStarted {
        site: LashVmExecutionSite,
        occurrence: u64,
    },
    ChildProcessWaiting {
        site: LashVmExecutionSite,
        occurrence: u64,
        process_ids: Vec<ProcessId>,
    },
    NodeResumed {
        site: LashVmExecutionSite,
        occurrence: u64,
    },
    NodeCompleted {
        site: LashVmExecutionSite,
        occurrence: u64,
    },
    NodeFailed {
        site: LashVmExecutionSite,
        occurrence: u64,
        failure: LashVmExecutionFailure,
    },
    BranchSelected {
        site: LashVmExecutionSite,
        occurrence: u64,
        edge_id: String,
        selected: ProcessBranchSelection,
    },
    ChildStarted {
        site: LashVmExecutionSite,
        occurrence: u64,
        child: LashVmExecutionChild,
    },
}
