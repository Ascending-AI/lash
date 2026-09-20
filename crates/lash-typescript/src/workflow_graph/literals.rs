//! The workflow lens's inline process-body projection (FIG-2997).
//!
//! A process literal in an argument is a process container of the module the
//! same way a `const`-bound process arrow is (ADR 0095): it projects its own
//! declaration, named identically to what the linker will lift it to, and its
//! authored arrow re-parses straight off the call site.

use super::*;

impl GraphProjector<'_> {
    /// Projects one inline process literal (FIG-2997).
    ///
    /// The literal's authored body — inside the wrapper a lowered run carries
    /// — is projected exactly like a declared process's `run` body, at the
    /// wrapper's path. Its identity is the linker's lift name: canonical body
    /// plus the path the literal sits at, so graph-implied and linked
    /// artifacts agree.
    pub(crate) fn project_literal_process(
        &self,
        path: &[u32],
        literal: &lashlang::ProcessLiteralExpr,
    ) -> WorkflowProcess {
        let name = lashlang::lifted_process_identity(&literal.body, path);
        let owner = format!("process:{name}");
        let mut versions = VersionState::default();
        for param in &literal.params {
            versions.seed(param.name.as_str());
        }
        // The body is addressed the way a declared process's body is: by the
        // path *inside the lifted declaration*, not by where the literal sits
        // in `main`. The linker lifts the literal to a module process, so at
        // run time its execution sites are owned by `process:<lifted name>`
        // and pathed from that declaration's own body root. Keying the
        // projection on the literal's `main` path instead left every site
        // inside a process container uncorrelated, so a run emitted no events
        // for it at all (FIG-3118). The literal's `main` path still decides
        // the lifted *name*, which is what keeps one container distinct from
        // another.
        let (wrapper_path, authored) = match crate::lower::process_run_body_path_of(&literal.body) {
            Some((relative, body)) => (relative, body),
            None => (Vec::new(), literal.body.as_ref()),
        };
        // Facts are keyed by the literal's position in `main`, where the
        // linker lowered it: the body is the literal's child 0, and the
        // wrapper path descends from there.
        let mut facts_base = lashlang::AstPath::main(path.to_vec()).child(0);
        facts_base.steps.extend(wrapper_path.iter().copied());
        WorkflowProcess {
            id: self.node_id(&owner, &[], "process"),
            name: name.clone(),
            display_name: name,
            description: None,
            name_source: WorkflowNodeNameSource::Derived,
            params: literal.params.clone(),
            signals: Vec::new(),
            return_ty: None,
            body: self.project_block(authored, &owner, &wrapper_path, &facts_base, &mut versions),
        }
    }
}

/// Every inline process literal in `main`, with its AST path, in walk order.
pub(crate) fn collect_process_literals<'a>(
    expr: &'a Expr,
    path: &mut Vec<u32>,
    literals: &mut Vec<(Vec<u32>, &'a lashlang::ProcessLiteralExpr)>,
) {
    if let Expr::ProcessLiteral(literal) = expr {
        literals.push((path.clone(), literal));
    }
    // The path is `u32`-keyed, so count in `u32` rather than converting a
    // `usize` back down: the walk cannot then fail on a conversion at all.
    for (index, child) in (0u32..).zip(expr.children()) {
        path.push(index);
        collect_process_literals(child, path, literals);
        path.pop();
    }
}
