use crate::ast::{AstPath, Expr, LabelMetadata};
use crate::runtime::{
    STEP_EXECUTION_SITE_KIND, execution_site_descriptor, label_attaches_to_concrete_node,
};
use lash_sansio::{WorkflowExecutionSite, WorkflowSitePath, WorkflowSiteRole};

use super::{WorkflowNodePath, WorkflowOwnership};

/// Every execution site a workflow node contributes. A site's address is its
/// node and the typed slot path from the node's statement to the executable
/// subexpression, read from the same ownership walk the compiler attributes
/// instructions with: two calls in one statement are two sites.
///
/// Execution sites are a compiler/runtime concept, not a syntax one, so this
/// walk stays in `lash_vm` while the projector that calls it lives in
/// `lash-typescript`.
pub fn execution_sites(
    expression: &Expr,
    owner: &str,
    ast_path: &AstPath,
    ownership: &WorkflowOwnership,
    label: Option<&LabelMetadata>,
) -> Vec<WorkflowExecutionSite> {
    let mut sites = Vec::new();
    let Some(node_path) = ownership.path_for_ast(ast_path) else {
        panic!("projected execution-site expression must have workflow ownership");
    };
    collect_execution_sites(
        expression, owner, ast_path, node_path, ownership, label, &mut sites,
    );
    sites.sort();
    sites
}

fn collect_execution_sites(
    expression: &Expr,
    owner: &str,
    ast_path: &AstPath,
    node_path: &WorkflowNodePath,
    ownership: &WorkflowOwnership,
    label: Option<&LabelMetadata>,
    sites: &mut Vec<WorkflowExecutionSite>,
) {
    // An inline process literal's body runs in the process it lifts to,
    // never under the node that holds it, so it contributes no site here.
    if ownership.path_for_ast(ast_path) != Some(node_path)
        || matches!(expression, Expr::ProcessLiteral(_))
    {
        return;
    }
    if let Some(label) = label
        && !label_attaches_to_concrete_node(expression)
    {
        sites.push(
            WorkflowExecutionSite::new(
                owner,
                node_path.indices(),
                STEP_EXECUTION_SITE_KIND,
                label.title.as_str(),
            )
            .at(site_path(ast_path, ownership).role(WorkflowSiteRole::LabeledStep)),
        );
        collect_execution_sites(
            expression, owner, ast_path, node_path, ownership, None, sites,
        );
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
                &ast_path.child(value_index),
                node_path,
                ownership,
                label,
                sites,
            );
        }
        Expr::Await(expr) | Expr::ResultUnwrap(expr) if label.is_some() => {
            collect_execution_sites(
                expr,
                owner,
                &ast_path.child(0),
                node_path,
                ownership,
                label,
                sites,
            );
        }
        _ => {
            if execution_site_descriptor(expression).is_some() {
                push_execution_site_descriptor(
                    expression,
                    owner,
                    node_path.indices(),
                    site_path(ast_path, ownership),
                    sites,
                );
            }
            collect_child_execution_sites(expression, owner, ast_path, node_path, ownership, sites);
        }
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
    site_path: WorkflowSitePath,
    sites: &mut Vec<WorkflowExecutionSite>,
) {
    let (kind, label) = execution_site_descriptor(expression)
        .expect("execution-site expression must have a compiler descriptor");
    sites.push(WorkflowExecutionSite::new(owner, node_path, kind, label).at(site_path));
}

/// The typed path from the owning node's statement to the expression at
/// `ast_path`.
fn site_path(ast_path: &AstPath, ownership: &WorkflowOwnership) -> WorkflowSitePath {
    ownership
        .site_for_ast(ast_path)
        .map(|(_, slots)| WorkflowSitePath::slots(slots.iter().copied()))
        .unwrap_or_default()
}

fn collect_child_execution_sites(
    expression: &Expr,
    owner: &str,
    ast_path: &AstPath,
    node_path: &WorkflowNodePath,
    ownership: &WorkflowOwnership,
    sites: &mut Vec<WorkflowExecutionSite>,
) {
    for (index, child) in expression.children().enumerate() {
        collect_execution_sites(
            child,
            owner,
            &ast_path.child(index as u32),
            node_path,
            ownership,
            None,
            sites,
        );
    }
}
