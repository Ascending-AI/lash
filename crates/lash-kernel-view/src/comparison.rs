//! A condition read as the comparison its package declares.
//! The supplied package view owns each declaration; no dialect name appears here.

use crate::ViewContext;
use lash_helper_labels::Comparison;
use lash_kernel_check::{
    ActNode, ActionNode, AtomNode, BindingKind, CalleeNode, ExprNode, Graph, MemberNode, NodeId,
    NodeKind, Reference, StmtNode,
};
use lash_kernel_doc::Literal;
use lash_vocabulary::CallableId;
use lash_vocabulary::sites::Site;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::BindingView;

/// A condition that is one comparison of values a host can name. A binary
/// comparison reads `left` then `right`; `contains` holds when `left`
/// holds `right`, `starts_with` and `ends_with` when `left` begins or ends
/// with `right`. Each holds under its dialect's own equality, order and
/// truth: TypeScript's `===` and Python's `==` both read as `eq`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum ComparisonView {
    Eq {
        left: ValueRef,
        right: ValueRef,
    },
    Ne {
        left: ValueRef,
        right: ValueRef,
    },
    Lt {
        left: ValueRef,
        right: ValueRef,
    },
    Le {
        left: ValueRef,
        right: ValueRef,
    },
    Gt {
        left: ValueRef,
        right: ValueRef,
    },
    Ge {
        left: ValueRef,
        right: ValueRef,
    },
    Contains {
        left: ValueRef,
        right: ValueRef,
    },
    StartsWith {
        left: ValueRef,
        right: ValueRef,
    },
    EndsWith {
        left: ValueRef,
        right: ValueRef,
    },
    /// The condition holds when its operand is false (falsy, in a dialect
    /// with truthiness).
    Not {
        operand: ValueRef,
    },
}

/// A value a comparison reads: where it comes from, then the record fields
/// read from it, outermost first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ValueRef {
    pub source: ValueSource,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub fields: Vec<String>,
}

/// Where a compared value comes from.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ValueSource {
    /// A parameter nothing assigns, by its binding in `dataflow`.
    Parameter {
        binding: usize,
    },
    Literal {
        value: Literal,
    },
    /// What the action at `site` yielded, held by a variable nothing
    /// reassigns.
    Result {
        site: Site,
    },
}

/// What the condition of a control reads as, when it is a comparison.
pub(crate) struct Comparisons<'a> {
    graph: &'a Graph,
    bindings: &'a [BindingView],
    context: &'a ViewContext,
}

impl<'a> Comparisons<'a> {
    pub(crate) fn new(
        graph: &'a Graph,
        bindings: &'a [BindingView],
        context: &'a ViewContext,
    ) -> Self {
        Self {
            graph,
            bindings,
            context,
        }
    }

    /// The comparison the condition at `condition` of the control `control`
    /// decides: a comparison call itself, or a variable a lowering bound to
    /// one in the statement right before the control, that nothing else
    /// reads or reassigns. Anything else has none.
    pub(crate) fn condition(&self, control: NodeId, condition: NodeId) -> Option<ComparisonView> {
        match &self.graph.node(condition).kind {
            NodeKind::Expr(ExprNode::Variable(Reference::Binding(id))) => {
                let binding = self.graph.binding(*id);
                let view = self.bindings.iter().find(|view| view.id == id.index())?;
                if binding.kind != BindingKind::Let || binding.assigned || view.uses.len() != 1 {
                    return None;
                }
                let declaration = self.graph.node(binding.declared_at);
                let NodeKind::Stmt(StmtNode::Let { value, .. }) = declaration.kind else {
                    return None;
                };
                let control = &self.graph.node(control).site;
                let (Some((last, block)), Some((before, held))) = (
                    control.path.split_last(),
                    declaration.site.path.split_last(),
                ) else {
                    return None;
                };
                (declaration.site.unit == control.unit && block == held && *before + 1 == *last)
                    .then(|| self.comparison(value))
                    .flatten()
            }
            _ => self.comparison(condition),
        }
    }

    fn comparison(&self, id: NodeId) -> Option<ComparisonView> {
        let (function, args) = self.call(id)?;
        let definition = self.context.function(&function)?;
        let comparison = self.context.display(&function)?.comparison?;
        if args.len() != comparison.arity() || definition.signature.params.len() != args.len() {
            return None;
        }
        let pair = || Some((self.value(args[0])?, self.value(args[1])?));
        Some(match comparison {
            Comparison::Not => match args[0] {
                Operand::Node(id) => match self.comparison(id) {
                    Some(ComparisonView::Eq { left, right }) => ComparisonView::Ne { left, right },
                    Some(_) => return None,
                    None => ComparisonView::Not {
                        operand: self.value(args[0])?,
                    },
                },
                Operand::Atom(_) => ComparisonView::Not {
                    operand: self.value(args[0])?,
                },
            },
            Comparison::Eq => pair().map(|(left, right)| ComparisonView::Eq { left, right })?,
            Comparison::Ne => pair().map(|(left, right)| ComparisonView::Ne { left, right })?,
            Comparison::Lt => pair().map(|(left, right)| ComparisonView::Lt { left, right })?,
            Comparison::Le => pair().map(|(left, right)| ComparisonView::Le { left, right })?,
            Comparison::Gt => pair().map(|(left, right)| ComparisonView::Gt { left, right })?,
            Comparison::Ge => pair().map(|(left, right)| ComparisonView::Ge { left, right })?,
            Comparison::Contains => {
                pair().map(|(left, right)| ComparisonView::Contains { left, right })?
            }
            Comparison::StartsWith => {
                pair().map(|(left, right)| ComparisonView::StartsWith { left, right })?
            }
            Comparison::EndsWith => {
                pair().map(|(left, right)| ComparisonView::EndsWith { left, right })?
            }
        })
    }

    /// The library function a node calls and its arguments: a native call,
    /// or a call of a library function's kernel code, nested in an
    /// expression or a statement's own action.
    fn call(&self, id: NodeId) -> Option<(CallableId, Vec<Operand<'a>>)> {
        let graph: &'a Graph = self.graph;
        let (callee, args) = match &graph.node(id).kind {
            NodeKind::Expr(ExprNode::Call { function, args }) => {
                return Some((
                    CallableId {
                        link: self.context.reference.link,
                        function: *function,
                    },
                    args.iter().copied().map(Operand::Node).collect(),
                ));
            }
            NodeKind::Expr(ExprNode::Act {
                kind: ActNode::Call(callee),
                args,
            }) => (callee, args.iter().copied().map(Operand::Node).collect()),
            NodeKind::Action(ActionNode::Call { callee, args }) => {
                (callee, args.iter().map(Operand::Atom).collect())
            }
            _ => return None,
        };
        let function = match callee {
            CalleeNode::Local(function) => CallableId {
                link: self.context.reference.link,
                function: *function,
            },
            CalleeNode::Import(slot) => self.context.resolve(self.context.reference.link, *slot)?,
            CalleeNode::Value(_) => return None,
        };
        Some((function, args))
    }

    fn value(&self, operand: Operand<'_>) -> Option<ValueRef> {
        let source = match operand {
            Operand::Atom(AtomNode::Literal(value)) => ValueSource::Literal {
                value: value.clone(),
            },
            Operand::Atom(AtomNode::Variable(reference)) => self.variable(reference)?,
            Operand::Node(id) => match &self.graph.node(id).kind {
                NodeKind::Expr(ExprNode::Literal(value)) => ValueSource::Literal {
                    value: value.clone(),
                },
                NodeKind::Expr(ExprNode::Variable(reference)) => self.variable(reference)?,
                NodeKind::Expr(ExprNode::Member(MemberNode::Field { target, field })) => {
                    let mut value = self.value(Operand::Node(*target))?;
                    value.fields.push(field.clone());
                    return Some(value);
                }
                _ => return None,
            },
        };
        Some(ValueRef {
            source,
            fields: Vec::new(),
        })
    }

    /// A parameter, or the result of the action a `let` holds, when nothing
    /// reassigns the variable.
    fn variable(&self, reference: &Reference) -> Option<ValueSource> {
        let Reference::Binding(id) = reference else {
            return None;
        };
        let binding = self.graph.binding(*id);
        if binding.assigned {
            return None;
        }
        match binding.kind {
            BindingKind::Param => Some(ValueSource::Parameter {
                binding: id.index(),
            }),
            BindingKind::Let => {
                let value = match self.graph.node(binding.declared_at).kind {
                    NodeKind::Stmt(StmtNode::Let { value, .. })
                    | NodeKind::Expr(ExprNode::Let { value, .. }) => self.graph.node(value),
                    _ => return None,
                };
                matches!(
                    value.kind,
                    NodeKind::Action(_) | NodeKind::Expr(ExprNode::Act { .. })
                )
                .then(|| ValueSource::Result {
                    site: value.site.clone(),
                })
            }
            BindingKind::For | BindingKind::Catch => None,
        }
    }
}

/// An argument of a comparison call: an expression's node, or a statement
/// action's atom.
#[derive(Clone, Copy)]
enum Operand<'a> {
    Node(NodeId),
    Atom(&'a AtomNode),
}
