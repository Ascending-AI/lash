use lash_sansio::{
    ExecutionNodeKind, ExprSlot, ProcessId, WorkflowOccurrence, WorkflowSitePath, WorkflowSiteRef,
    WorkflowSiteRole,
};
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
    /// The site of the expression `slots` reach from the statement of the
    /// node at `node_path`.
    pub(crate) fn node_site(
        &self,
        node_path: &WorkflowNodePath,
        slots: &[ExprSlot],
        kind: ExecutionNodeKind,
        label: impl Into<String>,
    ) -> LashVmExecutionSite {
        self.site(
            node_path,
            WorkflowSitePath::at(slots.iter().copied()),
            kind,
            label,
        )
    }

    /// The step a label declares over the expression `slots` reach.
    pub(crate) fn labeled_step_site(
        &self,
        node_path: &WorkflowNodePath,
        slots: &[ExprSlot],
        label: impl Into<String>,
    ) -> LashVmExecutionSite {
        self.site(
            node_path,
            WorkflowSitePath::at(slots.iter().copied()).with_role(WorkflowSiteRole::LabeledStep),
            ExecutionNodeKind::Step,
            label,
        )
    }

    pub(crate) fn branch_site(
        &self,
        node_path: &WorkflowNodePath,
        slots: &[ExprSlot],
    ) -> LashVmExecutionSite {
        self.node_site(node_path, slots, ExecutionNodeKind::Branch, "if")
    }

    fn site(
        &self,
        node_path: &WorkflowNodePath,
        site_path: WorkflowSitePath,
        kind: ExecutionNodeKind,
        label: impl Into<String>,
    ) -> LashVmExecutionSite {
        let owner = self.context.entry.workflow_owner();
        LashVmExecutionSite {
            site: WorkflowSiteRef::new(workflow_node_id(&owner, node_path.indices()), site_path),
            kind,
            label: label.into(),
        }
    }
}

pub fn process_ref_key(process_ref: &ProcessRef) -> String {
    format!("{}:{}", process_ref.component, process_ref.pos)
}

/// One execution site as the compiler attributes an instruction to it: its
/// static address in the workflow document and a description of what runs
/// there.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashVmExecutionSite {
    pub site: WorkflowSiteRef,
    pub kind: ExecutionNodeKind,
    pub label: String,
}

/// One occurrence of a site as the VM hands it to its host: which run of
/// which site it is and the loop activations that enclosed it when it began
/// (`at`), with the description of what runs at the site.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashVmExecutionCallSite {
    pub at: WorkflowOccurrence,
    pub kind: ExecutionNodeKind,
    pub label: String,
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

/// One fact the VM observed about one occurrence of one execution site.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashVmExecutionObservation {
    pub call_site: LashVmExecutionCallSite,
    pub fact: LashVmExecutionFact,
}

/// What happened to the occurrence a [`LashVmExecutionObservation`] names.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LashVmExecutionFact {
    NodeStarted,
    ChildProcessWaiting { process_ids: Vec<ProcessId> },
    NodeResumed,
    NodeCompleted,
    NodeFailed { failure: LashVmExecutionFailure },
    BranchSelected { selected: ProcessBranchSelection },
    ChildStarted { child: LashVmExecutionChild },
}
