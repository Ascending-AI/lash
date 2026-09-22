use crate::ast::{Expr, LabelMetadata, ListComprehensionClause};
use crate::runtime::{
    STEP_EXECUTION_SITE_KIND, execution_site_descriptor, label_attaches_to_concrete_node,
};
use lash_sansio::WorkflowExecutionSite;

use super::child_path;

/// Every typed execution site a workflow node contributes, keyed by the
/// node's owner and AST path plus site kind.
///
/// Execution sites are a compiler/runtime concept, not a syntax one, so this
/// walk stays in `lashlang` while the projector that calls it lives in
/// `lash-typescript`.
pub fn execution_sites(
    expression: &Expr,
    owner: &str,
    path: &[u32],
    label: Option<&LabelMetadata>,
) -> Vec<WorkflowExecutionSite> {
    let mut sites = Vec::new();
    collect_execution_sites(expression, owner, path, path, label, &mut sites);
    sites.sort();
    sites.dedup();
    sites
}

fn collect_execution_sites(
    expression: &Expr,
    owner: &str,
    node_path: &[u32],
    path: &[u32],
    label: Option<&LabelMetadata>,
    sites: &mut Vec<WorkflowExecutionSite>,
) {
    if let Some(label) = label
        && !label_attaches_to_concrete_node(expression)
    {
        sites.push(WorkflowExecutionSite::new(
            owner,
            node_path,
            STEP_EXECUTION_SITE_KIND,
            label.title.as_str(),
        ));
        collect_execution_sites(expression, owner, node_path, path, None, sites);
        return;
    }
    match expression {
        Expr::Assign { target, expr } if label.is_some() => {
            let value_index = target
                .steps
                .iter()
                .filter(|step| matches!(step, crate::AssignPathStep::Index(_)))
                .count() as u32;
            collect_execution_sites(
                expr,
                owner,
                node_path,
                &child_path(path, value_index),
                label,
                sites,
            );
        }
        Expr::Await(expr) | Expr::ResultUnwrap(expr) if label.is_some() => {
            collect_execution_sites(expr, owner, node_path, &child_path(path, 0), label, sites);
        }
        Expr::ReceiverCall { .. }
        | Expr::SleepFor(_)
        | Expr::SleepUntil(_)
        | Expr::WaitSignal { .. }
        | Expr::Finish(_)
        | Expr::Fail(_)
        | Expr::Yield(_)
        | Expr::Call { .. } => {
            push_execution_site_descriptor(expression, owner, node_path, sites);
            collect_child_execution_sites(expression, owner, node_path, path, sites);
        }
        Expr::If { condition, .. } => {
            push_execution_site_descriptor(expression, owner, node_path, sites);
            collect_execution_sites(
                condition,
                owner,
                node_path,
                &child_path(path, 0),
                None,
                sites,
            );
        }
        Expr::For { iterable, .. } => {
            push_execution_site_descriptor(expression, owner, node_path, sites);
            collect_execution_sites(
                iterable,
                owner,
                node_path,
                &child_path(path, 0),
                None,
                sites,
            );
        }
        Expr::While { condition, .. } => {
            push_execution_site_descriptor(expression, owner, node_path, sites);
            collect_execution_sites(
                condition,
                owner,
                node_path,
                &child_path(path, 0),
                None,
                sites,
            );
        }
        Expr::ListComprehension { clauses, .. } => {
            for (index, clause) in clauses.iter().enumerate() {
                let expression = match clause {
                    ListComprehensionClause::For { iterable, .. } => iterable,
                    ListComprehensionClause::If { condition } => condition,
                };
                collect_execution_sites(
                    expression,
                    owner,
                    node_path,
                    &child_path(path, index as u32),
                    None,
                    sites,
                );
            }
        }
        _ => collect_child_execution_sites(expression, owner, node_path, path, sites),
    }
}

#[expect(
    clippy::expect_used,
    reason = "execution-site expressions come from lowered core paths that always have a compiler descriptor, per the message"
)]
fn push_execution_site_descriptor(
    expression: &Expr,
    owner: &str,
    node_path: &[u32],
    sites: &mut Vec<WorkflowExecutionSite>,
) {
    let (kind, label) = execution_site_descriptor(expression)
        .expect("execution-site expression must have a compiler descriptor");
    sites.push(WorkflowExecutionSite::new(owner, node_path, kind, label));
}

fn collect_child_execution_sites(
    expression: &Expr,
    owner: &str,
    node_path: &[u32],
    path: &[u32],
    sites: &mut Vec<WorkflowExecutionSite>,
) {
    for (index, child) in expression.children().enumerate() {
        collect_execution_sites(
            child,
            owner,
            node_path,
            &child_path(path, index as u32),
            None,
            sites,
        );
    }
}
