use lash_sansio::{
    ExecutionNodeKind, ExprSlot, ProcessId, WorkflowExecutionSite, WorkflowLoopFrame,
    WorkflowOccurrenceContext, WorkflowSitePath, WorkflowSiteRef, WorkflowSiteRole,
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
            WorkflowSitePath::slots(slots.iter().copied()),
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
            WorkflowSitePath::slots(slots.iter().copied()).role(WorkflowSiteRole::LabeledStep),
            ExecutionNodeKind::Step,
            label,
        )
    }

    pub(crate) fn branch_site(
        &self,
        node_path: &WorkflowNodePath,
        slots: &[ExprSlot],
    ) -> LashVmExecutionSite {
        let mut site = self.node_site(node_path, slots, ExecutionNodeKind::Branch, "if");
        site.branch = Some(LashVmBranchSite {
            then_edge_id: self.branch_edge_id(node_path, ProcessBranchSelection::Then),
            else_edge_id: self.branch_edge_id(node_path, ProcessBranchSelection::Else),
        });
        site
    }

    fn site(
        &self,
        node_path: &WorkflowNodePath,
        site_path: WorkflowSitePath,
        kind: ExecutionNodeKind,
        label: impl Into<String>,
    ) -> LashVmExecutionSite {
        let label = label.into();
        let owner = self.context.entry.workflow_owner();
        LashVmExecutionSite {
            node_id: workflow_node_id(&owner, node_path.indices()).to_string(),
            node_kind: kind,
            label: label.clone(),
            branch: None,
            workflow_site: WorkflowExecutionSite::new(owner, node_path.indices(), kind, label)
                .at(site_path),
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

impl LashVmExecutionSite {
    /// The typed path from the node's statement to this site's expression.
    pub fn site_path(&self) -> &WorkflowSitePath {
        &self.workflow_site.site_path
    }

    /// The site's static address in its workflow document.
    pub fn site_ref(&self) -> WorkflowSiteRef {
        WorkflowSiteRef::new(self.node_id.clone(), self.site_path().clone())
    }

    /// Where an occurrence of this site that began inside `loops` ran.
    pub fn occurrence_context(&self, loops: &[WorkflowLoopFrame]) -> WorkflowOccurrenceContext {
        WorkflowOccurrenceContext {
            site_path: self.site_path().clone(),
            loops: loops.to_vec(),
        }
    }

    /// Whether `site` is this site's address.
    pub fn is_at(&self, site: &WorkflowSiteRef) -> bool {
        self.node_id == site.node_id && *self.site_path() == site.site_path
    }
}

/// One occurrence of a site as the VM hands it to its host: the site, which
/// run of that site this is (from 1), and the loop activations that enclosed
/// it when it began.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LashVmExecutionCallSite {
    pub site: LashVmExecutionSite,
    pub occurrence: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub loops: Vec<WorkflowLoopFrame>,
}

impl LashVmExecutionCallSite {
    /// The occurrence's identity within its execution: its site and number.
    pub fn occurrence_key(&self) -> (WorkflowSiteRef, u64) {
        (self.site.site_ref(), self.occurrence)
    }

    /// Where this occurrence ran inside its node, as a durable effect or
    /// blocker records it.
    pub fn context(&self) -> WorkflowOccurrenceContext {
        self.site.occurrence_context(&self.loops)
    }
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

impl LashVmExecutionObservation {
    /// The occurrence this observation is about: its site, which run of the
    /// site it is, and the loop context it began in.
    pub fn call_site(&self) -> LashVmExecutionCallSite {
        match self {
            Self::NodeStarted {
                site,
                occurrence,
                loops,
            }
            | Self::ChildProcessWaiting {
                site,
                occurrence,
                loops,
                ..
            }
            | Self::NodeResumed {
                site,
                occurrence,
                loops,
            }
            | Self::NodeCompleted {
                site,
                occurrence,
                loops,
            }
            | Self::NodeFailed {
                site,
                occurrence,
                loops,
                ..
            }
            | Self::BranchSelected {
                site,
                occurrence,
                loops,
                ..
            }
            | Self::ChildStarted {
                site,
                occurrence,
                loops,
                ..
            } => LashVmExecutionCallSite {
                site: site.clone(),
                occurrence: *occurrence,
                loops: loops.clone(),
            },
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum LashVmExecutionObservation {
    NodeStarted {
        site: LashVmExecutionSite,
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        loops: Vec<WorkflowLoopFrame>,
    },
    ChildProcessWaiting {
        site: LashVmExecutionSite,
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        loops: Vec<WorkflowLoopFrame>,
        process_ids: Vec<ProcessId>,
    },
    NodeResumed {
        site: LashVmExecutionSite,
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        loops: Vec<WorkflowLoopFrame>,
    },
    NodeCompleted {
        site: LashVmExecutionSite,
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        loops: Vec<WorkflowLoopFrame>,
    },
    NodeFailed {
        site: LashVmExecutionSite,
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        loops: Vec<WorkflowLoopFrame>,
        failure: LashVmExecutionFailure,
    },
    BranchSelected {
        site: LashVmExecutionSite,
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        loops: Vec<WorkflowLoopFrame>,
        edge_id: String,
        selected: ProcessBranchSelection,
    },
    ChildStarted {
        site: LashVmExecutionSite,
        occurrence: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        loops: Vec<WorkflowLoopFrame>,
        child: LashVmExecutionChild,
    },
}
