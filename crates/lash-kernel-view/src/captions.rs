//! The join of a document's captions with a view of it, keyed by view
//! location. The view itself stays a pure function of the document: a
//! caption-only change keeps its identity, so captions never enter it.

use lash_kernel_doc::{Caption, Captions, DocumentId, Site};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{WorkflowNodeKind, WorkflowView};

/// A view's captions, each keyed by where the view draws it. Wording for a
/// step without a caption is the host's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptionView {
    pub document: DocumentId,
    /// By node, then site. An action or control node has its carrier
    /// statement's caption; a statement with several actions gives it to
    /// the node of its first. A code node has the caption of every member,
    /// and of every clause arm of a member, that has one.
    pub nodes: Vec<NodeCaption>,
    /// By node, then arm: a control node's [`crate::ControlArm`] captions.
    pub arms: Vec<ArmCaption>,
    /// By callable: a declared function's caption, on its
    /// [`crate::CallableView`]. The entry's is the workflow's title.
    pub callables: Vec<CallableCaption>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeCaption {
    /// The node's index in [`WorkflowView::nodes`].
    pub node: usize,
    /// The captioned statement or arm.
    pub site: Site,
    pub caption: Caption,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ArmCaption {
    /// The control node's index in [`WorkflowView::nodes`].
    pub node: usize,
    /// The arm's index in that node's `arms`.
    pub arm: usize,
    pub block: Site,
    pub caption: Caption,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CallableCaption {
    /// The callable's index in [`WorkflowView::callables`].
    pub callable: usize,
    pub block: Site,
    pub caption: Caption,
}

/// Why captions do not join a view.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CaptionViewRefusal {
    #[error("the captions name document {captions}, the view {view}")]
    Document {
        view: DocumentId,
        captions: DocumentId,
    },
}

impl WorkflowView {
    /// `captions`, of this view's document, keyed by view location.
    ///
    /// # Errors
    ///
    /// [`CaptionViewRefusal::Document`] for captions of another document.
    pub fn captions(&self, captions: &Captions) -> Result<CaptionView, CaptionViewRefusal> {
        if captions.document() != self.document {
            return Err(CaptionViewRefusal::Document {
                view: self.document,
                captions: captions.document(),
            });
        }
        let caption = |site: &Site| captions.get(site).cloned();
        let mut nodes = Vec::new();
        let mut arms = Vec::new();
        for (index, node) in self.nodes.iter().enumerate() {
            match node.kind {
                WorkflowNodeKind::Action | WorkflowNodeKind::Control => {
                    // Only a statement's carrier holds it among its members.
                    if node.members.contains(&node.statement)
                        && let Some(caption) = caption(&node.statement)
                    {
                        nodes.push(NodeCaption {
                            node: index,
                            site: node.statement.clone(),
                            caption,
                        });
                    }
                }
                WorkflowNodeKind::Code => {
                    let mut found: Vec<_> = captions
                        .entries()
                        .iter()
                        .filter(|entry| {
                            node.members.iter().any(|member| {
                                entry.site == *member || self.is_arm_of(&entry.site, member)
                            })
                        })
                        .map(|entry| NodeCaption {
                            node: index,
                            site: entry.site.clone(),
                            caption: entry.caption.clone(),
                        })
                        .collect();
                    found.sort_by(|a, b| a.site.cmp(&b.site));
                    nodes.extend(found);
                }
            }
            for (arm, control) in node.arms.iter().enumerate() {
                if let Some(caption) = caption(&control.block) {
                    arms.push(ArmCaption {
                        node: index,
                        arm,
                        block: control.block.clone(),
                        caption,
                    });
                }
            }
        }
        let callables = self
            .callables
            .iter()
            .enumerate()
            .filter_map(|(index, callable)| {
                Some(CallableCaption {
                    callable: index,
                    block: callable.block.clone(),
                    caption: caption(&callable.block)?,
                })
            })
            .collect();
        Ok(CaptionView {
            document: self.document,
            nodes,
            arms,
            callables,
        })
    }

    /// Whether `site` is a block of the statement at `statement`.
    fn is_arm_of(&self, site: &Site, statement: &Site) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope.block == *site && scope.statement.as_ref() == Some(statement))
    }
}
