//! Lowering-only calls retain their execution coordinates on their authored consumer.
use std::collections::{BTreeMap, BTreeSet};

use lash_kernel_doc::{Document, ProjectionRole};
use lash_kernel_value::CallableId;
use lash_kernel_value::sites::Site;

use crate::{CalleeView, FlowTarget, ViewContext, WorkflowView};

pub(crate) struct Folded {
    actions: BTreeMap<Site, Site>,
    statements: BTreeMap<Site, Site>,
}

fn under(site: &Site, parent: &Site) -> bool {
    site.unit == parent.unit && site.path.starts_with(&parent.path)
}

pub(crate) fn prepare(
    document: &Document,
    view: &mut WorkflowView,
    context: &ViewContext,
) -> Folded {
    let roles: BTreeMap<_, _> = view
        .dataflow
        .actions
        .iter()
        .filter_map(|action| {
            let Some(CalleeView::Library { id, link, .. }) = &action.callee else {
                return None;
            };
            let role = context.projection_role(CallableId {
                link: *link,
                function: *id,
            });
            role.is_glue().then_some((action.site.clone(), role))
        })
        .collect();
    let mut folded = Folded {
        actions: BTreeMap::new(),
        statements: BTreeMap::new(),
    };
    for (site, role) in &roles {
        let candidates = view
            .dataflow
            .actions
            .iter()
            .filter(|action| !roles.contains_key(&action.site));
        let consumer = if *role == ProjectionRole::ResultDecode {
            candidates
                .filter(|action| under(&action.site, site))
                .min_by_key(|action| action.site.path.len())
        } else {
            candidates
                .filter(|action| under(site, &action.site))
                .max_by_key(|action| action.site.path.len())
        };
        if let Some(consumer) = consumer {
            folded.actions.insert(site.clone(), consumer.site.clone());
        }
    }
    // The declaration marks this return at the point the lowering emits it.
    // An explicitly authored return, including `return undefined`, stays visible.
    for (id, function) in &document.functions {
        if function.projection != ProjectionRole::ImplicitReturn {
            continue;
        }
        let Some(body) = function.body() else {
            continue;
        };
        let Some(last) = body.block.len().checked_sub(1) else {
            continue;
        };
        if !matches!(
            &body.block[last],
            lash_kernel_doc::Stmt::Return {
                value: lash_kernel_doc::Expr::Literal(lash_kernel_doc::Literal::Absent)
            } | lash_kernel_doc::Stmt::ReturnVia {
                value: lash_kernel_doc::Expr::Literal(lash_kernel_doc::Literal::Absent),
                ..
            }
        ) {
            continue;
        }
        let site = Site::new(lash_kernel_value::sites::Unit::Library(*id), [last as u32]);
        if let Some(previous) = view.statements.iter().rev().find(|statement| {
            statement.site.unit == site.unit
                && statement.site.path.len() == 1
                && statement.site.path[0] < last as u32
        }) {
            folded.statements.insert(site, previous.site.clone());
        } else {
            view.statements.retain(|statement| statement.site != site);
        }
    }
    view.statements
        .retain(|statement| !folded.statements.contains_key(&statement.site));
    for statement in &mut view.statements {
        statement
            .actions
            .retain(|action| !folded.actions.contains_key(action));
    }
    // Display operands are the authored values; execution keeps the complete
    // wrapper, forwarding factory and conversion in the document.
    for action in &mut view.dataflow.actions {
        for argument in &mut action.args {
            while let Some(site) = &argument.site {
                if !folded.actions.contains_key(site) {
                    break;
                }
                let Some(crate::ExpressionView {
                    expression: lash_kernel_doc::Expr::Act(call),
                    ..
                }) = view
                    .dataflow
                    .expressions
                    .iter()
                    .find(|expression| expression.site == *site)
                else {
                    break;
                };
                let Some(value) = call.args.first() else {
                    break;
                };
                argument.expression = value.clone();
                argument.site = Some(site.child(0));
                argument.binding = view
                    .dataflow
                    .expressions
                    .iter()
                    .find(|expression| Some(&expression.site) == argument.site.as_ref())
                    .and_then(|expression| {
                        if let lash_kernel_doc::Expr::Variable(name) = &expression.expression {
                            view.dataflow
                                .bindings
                                .iter()
                                .find(|binding| {
                                    &binding.name == name
                                        && expression.references.contains(&binding.id)
                                })
                                .map(|binding| binding.id)
                        } else {
                            None
                        }
                    });
            }
        }
    }
    folded
}

impl Folded {
    pub(crate) fn attach(self, view: &mut WorkflowView) {
        for (site, consumer) in self.actions.iter().chain(&self.statements) {
            if let Some(node) = view
                .nodes
                .iter_mut()
                .rev()
                .find(|node| node.site == *consumer || node.statement == *consumer)
            {
                node.folded.push(site.clone());
            }
        }
        let aliases: BTreeMap<_, _> = view
            .nodes
            .iter()
            .flat_map(|node| {
                node.folded
                    .iter()
                    .map(|site| (site.clone(), node.site.clone()))
            })
            .collect();
        let parents: BTreeMap<_, _> = view
            .execution_sites
            .iter()
            .map(|site| (site.site.clone(), site.within.clone()))
            .collect();
        for node in &mut view.nodes {
            let mut parent = node.within.take();
            while let Some(site) = &parent {
                let Some(consumer) = aliases.get(site) else {
                    break;
                };
                if *consumer == node.site {
                    parent = parents.get(site).cloned().flatten();
                } else {
                    parent = Some(consumer.clone());
                    break;
                }
            }
            node.within = parent;
        }
        let nodes = &view.nodes;
        let owner = |site: &Site| {
            nodes
                .iter()
                .find(|node| {
                    node.site == *site || node.members.contains(site) || node.folded.contains(site)
                })
                .map(|node| &node.site)
        };
        let mut seen = BTreeSet::new();
        view.flow.retain_mut(|edge| {
            let mut changed = false;
            if let Some(site) = aliases.get(&edge.from) {
                edge.from = site.clone();
                changed = true;
            }
            if let FlowTarget::Site(site) = &mut edge.to
                && let Some(consumer) = aliases.get(site)
            {
                *site = consumer.clone();
                changed = true;
            }
            if changed
                && let FlowTarget::Site(site) = &edge.to
                && owner(site).is_some()
                && owner(site) == owner(&edge.from)
            {
                return false;
            }
            let target = match &edge.to {
                FlowTarget::Site(site) => (0, Some(site.clone())),
                FlowTarget::Leave => (1, None),
                FlowTarget::End => (2, None),
            };
            seen.insert((edge.from.clone(), target, edge.kind as u8))
        });
    }
}
