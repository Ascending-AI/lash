//! Exact occurrence attribution. The observer owns evidence; the view owns its
//! placement and the live badge's precedence across concurrent tasks.

use lash_kernel_doc::{DocumentId, EffectId, PositionLabel, Site, Unit};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{WorkflowNodeKind, WorkflowView};

/// What an occurrence of an action did. `Recovered` is a failure whose
/// error a `catch` took, so the run went on (FIG-6817): the process
/// overlay's `recovered` row. `Failed` is a failure nothing recovered from.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActionState {
    Running,
    Waiting,
    Completed,
    Failed,
    Recovered,
    Cancelled,
    Incomplete,
}

/// Outcome data stays on the exact occurrence the observer supplies, never on
/// every action of its statement. `T` can retain the host's typed result/cause.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionObservation<T> {
    pub identity: EffectId,
    /// The authored statement supplied by the effect request. This is display
    /// attribution; `identity` keeps the actual performing unit and occurrence.
    pub origin: Site,
    pub state: ActionState,
    pub outcome: Option<T>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionActivity<T> {
    pub site: Site,
    /// The state its occurrences give it, `None` with none. A run commits
    /// occurrences for its effects and its waits only (FIG-6810). A library
    /// call receives its body's effects through their authored `origin`; the
    /// occurrences keep their library identities. A pure call, and a `sleep`
    /// that never parked, has no occurrence of its own. A
    /// process's live feed may report one as entered, never as completed
    /// or failed. A run that failed names the site that raised in its
    /// terminal failure's `UncaughtError` cause: a wait action, or the
    /// statement a library call raised in, whose actions are the nodes with
    /// that `statement`.
    pub state: Option<ActionState>,
    pub occurrences: Vec<ActionObservation<T>>,
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ActivityRefusal {
    #[error("the observations name another document")]
    Document,
    #[error("the observation names no action at {site}")]
    Action {
        site: lash_kernel_doc::PositionLabel,
    },
}

impl WorkflowView {
    /// The canonical carrier of an authored statement owns effects issued by
    /// its library call. Nested sibling actions retain their exact evidence.
    fn origin_node(&self, origin: &Site) -> Option<&crate::WorkflowNode> {
        self.nodes
            .iter()
            .find(|node| node.kind == WorkflowNodeKind::Action && node.members.contains(origin))
            .or_else(|| {
                self.nodes.iter().find(|node| {
                    node.kind == WorkflowNodeKind::Action
                        && node.statement == *origin
                        && node.within.is_none()
                })
            })
    }

    /// Attribute a snapshot of occurrence evidence for this document. A wait
    /// wins over completed occurrences, and a failure over a recovered
    /// one; no sibling or enclosing action inherits its state. Declared
    /// callees are inspected through their own views.
    pub fn activity<T: Clone>(
        &self,
        document: DocumentId,
        observations: &[ActionObservation<T>],
    ) -> Result<Vec<ActionActivity<T>>, ActivityRefusal> {
        if document != self.document {
            return Err(ActivityRefusal::Document);
        }
        let attributed = observations
            .iter()
            .map(|observation| {
                let position = observation.identity.position();
                if matches!(position.site().unit, Unit::Library(_)) {
                    if !self
                        .execution_sites
                        .iter()
                        .any(|site| site.statement == observation.origin)
                    {
                        return Err(ActivityRefusal::Action {
                            site: observation.origin.position(),
                        });
                    }
                    Ok(self.origin_node(&observation.origin).map_or_else(
                        || observation.origin.position(),
                        |node| node.site.position(),
                    ))
                } else if self
                    .execution_sites
                    .iter()
                    .any(|site| site.site == position)
                {
                    Ok(position)
                } else {
                    Err(ActivityRefusal::Action { site: position })
                }
            })
            .collect::<Result<Vec<PositionLabel>, ActivityRefusal>>()?;
        Ok(self
            .nodes
            .iter()
            .filter(|node| node.kind == WorkflowNodeKind::Action)
            .map(|node| {
                let occurrences: Vec<_> = observations
                    .iter()
                    .zip(&attributed)
                    .filter(|(_, site)| **site == node.site)
                    .map(|(observation, _)| observation.clone())
                    .collect();
                let state = occurrences
                    .iter()
                    .max_by_key(|observation| match observation.state {
                        ActionState::Waiting => 6,
                        ActionState::Running => 5,
                        ActionState::Failed | ActionState::Cancelled => 4,
                        ActionState::Recovered => 3,
                        ActionState::Incomplete => 2,
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
