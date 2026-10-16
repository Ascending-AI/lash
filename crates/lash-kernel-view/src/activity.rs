//! Exact occurrence attribution. The observer owns evidence; the view owns its
//! placement and the live badge's precedence across concurrent tasks.

use lash_kernel_doc::{DocumentId, EffectIdentity, Site};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{WorkflowNodeKind, WorkflowView};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
    Incomplete,
}

/// Outcome data stays on the exact occurrence the observer supplies, never on
/// every action of its statement. `T` can retain the host's typed result/cause.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionObservation<T> {
    pub identity: EffectIdentity,
    pub state: ActionState,
    pub outcome: Option<T>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionActivity<T> {
    pub site: Site,
    pub state: Option<ActionState>,
    pub occurrences: Vec<ActionObservation<T>>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ActivityRefusal {
    #[error("the observations name another document")]
    Document,
    #[error("the observation names no action at {site}")]
    Action { site: Site },
}

impl WorkflowView {
    /// Attribute a snapshot of occurrence evidence for this document. A wait
    /// wins over completed occurrences; no sibling or enclosing action inherits
    /// its state. Declared callees are inspected through their own views.
    pub fn activity<T: Clone>(
        &self,
        document: DocumentId,
        observations: &[ActionObservation<T>],
    ) -> Result<Vec<ActionActivity<T>>, ActivityRefusal> {
        if document != self.document {
            return Err(ActivityRefusal::Document);
        }
        for observation in observations {
            if !self
                .execution_sites
                .iter()
                .any(|site| site.site == observation.identity.site)
            {
                return Err(ActivityRefusal::Action {
                    site: observation.identity.site.clone(),
                });
            }
        }
        Ok(self
            .nodes
            .iter()
            .filter(|node| node.kind == WorkflowNodeKind::Action)
            .map(|node| {
                let occurrences: Vec<_> = observations
                    .iter()
                    .filter(|observation| observation.identity.site == node.site)
                    .cloned()
                    .collect();
                let state = occurrences
                    .iter()
                    .max_by_key(|observation| match observation.state {
                        ActionState::Waiting => 6,
                        ActionState::Running => 5,
                        ActionState::Failed | ActionState::Cancelled => 4,
                        ActionState::Incomplete => 3,
                        ActionState::Completed => 1,
                    })
                    .map(|observation| observation.state);
                ActionActivity {
                    site: node.site.clone(),
                    state,
                    occurrences,
                }
            })
            .collect())
    }
}
