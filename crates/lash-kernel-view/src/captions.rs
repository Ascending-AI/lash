//! The join of a document's captions with a view of it, keyed by view
//! location. The view itself stays a pure function of the document: a
//! caption-only change keeps its identity, so captions never enter it.

use std::collections::BTreeMap;

use lash_kernel_check::WorkflowDocument;
use lash_kernel_doc::{Caption, CaptionTarget, Captions, Document, FieldPath, ParamTarget};

use lash_vocabulary::names::DocumentId;
use lash_vocabulary::names::Name;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{BindingKind, WorkflowNodeKind, WorkflowView};

/// A view's captions, each keyed by where the view draws it. Wording for a
/// step without a caption is the host's.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CaptionView {
    pub document: DocumentId,
    /// By node, then site. An action or control node has its statement's
    /// caption; a statement with several actions gives it to the node of
    /// its first outermost action. A code node has the caption of every
    /// member, and of every clause arm of a member, that has one.
    pub nodes: Vec<NodeCaption>,
    /// By node, then arm: a control node's [`crate::ControlArm`] captions.
    pub arms: Vec<ArmCaption>,
    /// By callable: a declared function's caption, on its
    /// [`crate::CallableView`]. The entry's is the workflow's title.
    pub callables: Vec<CallableCaption>,
    /// By parameter: the entry's parameters that have a caption, in the
    /// order the entry takes them.
    pub params: Vec<ParamCaption>,
    /// By path: the captioned fields of the entry's parameter and result
    /// types, ascending.
    pub fields: Vec<FieldCaption>,
    /// By binding: each `let` of the entry whose declaring statement has a
    /// caption, which names the value it binds. A variable a lowering
    /// declares inside an expression never has one.
    pub bindings: Vec<BindingCaption>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BindingCaption {
    /// The binding's id in [`crate::Dataflow::bindings`].
    pub binding: usize,
    pub name: Name,
    /// The `let` statement that declares it, whose caption this is.
    pub site: Site,
    pub caption: Caption,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ParamCaption {
    /// The parameter's position among the entry's.
    pub index: usize,
    pub name: Name,
    pub caption: Caption,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FieldCaption {
    /// The field, from the entry's parameter or result.
    pub path: FieldPath,
    pub caption: Caption,
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
    /// The view was derived from another document than the one given,
    /// whose signatures name the entry's parameters.
    #[error("the view is of document {view}, the workflow document given is {given}")]
    Workflow { view: DocumentId, given: DocumentId },
}

impl WorkflowView {
    /// `captions`, of this view's document, keyed by view location.
    /// `document` is the one the view was derived from: its entry's
    /// signature orders and names the parameters, which a view's dataflow
    /// may hold under other names.
    ///
    /// # Errors
    ///
    /// [`CaptionViewRefusal::Document`] for captions of another document,
    /// [`CaptionViewRefusal::Workflow`] for another workflow document.
    pub fn captions(
        &self,
        document: &WorkflowDocument,
        captions: &Captions,
    ) -> Result<CaptionView, CaptionViewRefusal> {
        if document.identity() != self.document {
            return Err(CaptionViewRefusal::Workflow {
                view: self.document,
                given: document.identity(),
            });
        }
        if captions.document() != self.document {
            return Err(CaptionViewRefusal::Document {
                view: self.document,
                captions: captions.document(),
            });
        }
        let caption = |site: &Site| captions.at(site).cloned();
        let mut nodes = Vec::new();
        let mut arms = Vec::new();
        // Evaluation order visits argument helpers before the action that
        // consumes them. A control keeps its own statement's caption.
        let mut owners = BTreeMap::new();
        for (index, node) in self.nodes.iter().enumerate() {
            if node.kind == WorkflowNodeKind::Control
                || (node.kind == WorkflowNodeKind::Action && node.within.is_none())
            {
                owners.entry(node.statement.clone()).or_insert(index);
            }
        }
        for (index, node) in self.nodes.iter().enumerate() {
            match node.kind {
                WorkflowNodeKind::Action | WorkflowNodeKind::Control => {
                    if owners.get(&node.statement) == Some(&index)
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
                        .filter_map(|entry| Some((entry.target.site()?, &entry.caption)))
                        .filter(|(site, _)| {
                            node.members
                                .iter()
                                .any(|member| *site == member || self.is_arm_of(site, member))
                        })
                        .map(|(site, caption)| NodeCaption {
                            node: index,
                            site: site.clone(),
                            caption: caption.clone(),
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
        let (params, fields) = self.signature_captions(document.document(), captions);
        let bindings = self
            .dataflow
            .bindings
            .iter()
            .filter(|binding| binding.kind == BindingKind::Let)
            .filter_map(|binding| {
                Some(BindingCaption {
                    binding: binding.id,
                    name: binding.name.clone(),
                    site: binding.site.clone(),
                    caption: caption(&binding.site)?,
                })
            })
            .collect();
        Ok(CaptionView {
            document: self.document,
            nodes,
            arms,
            callables,
            params,
            fields,
            bindings,
        })
    }

    /// The captions of the entry's parameters, by position, and of its
    /// types' fields, by path.
    fn signature_captions(
        &self,
        document: &Document,
        captions: &Captions,
    ) -> (Vec<ParamCaption>, Vec<FieldCaption>) {
        let entry = self
            .scopes
            .iter()
            .find(|scope| scope.kind == crate::ScopeKind::Entry)
            .map(|scope| &scope.block.unit);
        let Some(Unit::Library(id)) = entry else {
            return (Vec::new(), Vec::new());
        };
        let Some((function, _)) = document
            .exports
            .iter()
            .find(|(_, e)| matches!(e, lash_kernel_doc::Export::Function(f) if f.function == *id))
        else {
            return (Vec::new(), Vec::new());
        };
        let names: Vec<&Name> = document
            .functions
            .get(id)
            .map(|f| f.signature.params.iter().map(|p| &p.name).collect())
            .unwrap_or_default();
        let params = names
            .into_iter()
            .enumerate()
            .filter_map(|(index, name)| {
                let target = CaptionTarget::Param(ParamTarget {
                    function: function.clone(),
                    param: name.clone(),
                });
                Some(ParamCaption {
                    index,
                    name: name.clone(),
                    caption: captions.get(&target)?.clone(),
                })
            })
            .collect();
        let fields = captions
            .entries()
            .iter()
            .filter_map(|entry| match &entry.target {
                CaptionTarget::Field(field) if field.function == *function => Some(FieldCaption {
                    path: field.path.clone(),
                    caption: entry.caption.clone(),
                }),
                _ => None,
            })
            .collect();
        (params, fields)
    }

    /// Whether `site` is a block of the statement at `statement`.
    fn is_arm_of(&self, site: &Site, statement: &Site) -> bool {
        self.scopes
            .iter()
            .any(|scope| scope.block == *site && scope.statement.as_ref() == Some(statement))
    }
}
