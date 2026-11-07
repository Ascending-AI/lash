//! A native library call answered in place: which function a node calls,
//! where it answers, and what a native hook's row reads and writes.
use super::*;

/// The library function `node` calls: a call expression's, or a native call
/// act's, which answers at its Enter instead of its Commit.
pub(super) fn callee(node: doc::Node<'_>, source: &Source) -> Option<FunctionId> {
    match node {
        doc::Node::Expr(doc::Expr::Call { function, .. }) => Some(*function),
        _ => crate::compile::room::native_act(node, &source.calls),
    }
}

/// The phase at which `node`'s library call answers.
pub(super) fn phase(node: doc::Node<'_>) -> Phase {
    match node {
        doc::Node::Expr(doc::Expr::Act(_)) => Phase::Enter,
        _ => Phase::Commit,
    }
}

type Facts = (
    FunctionId,
    Option<Reg<LeaseValue>>,
    Result<Vec<Reg<LeaseValue>>, Refused>,
);

/// The callee, output and inputs a Native row at `step` must name, given
/// its event's. A protocol hook's or a return-via policy's native answers
/// into the sink, as a hook's call returns there; `next` and `item` take
/// what the loop holds. A `check` hook has no native row.
pub(super) fn facts(
    draft: &RegionDraft,
    step: &Step,
    output: Option<Reg<LeaseValue>>,
    inputs: Result<Vec<Reg<LeaseValue>>, Refused>,
) -> Result<Facts, Refused> {
    let source = &draft.source;
    let node = resources::node(source, &step.at.site)?;
    let Some(hook) = crate::compile::room::native_hook(node, step.at.phase, &source.calls) else {
        return Ok((callee(node, source).ok_or(Refused::Source)?, output, inputs));
    };
    let sink = Some(Reg::new(draft.roles.len())?);
    Ok(match step.at.phase {
        Phase::Next | Phase::Item => (
            hook,
            sink,
            super::via::hook_arguments(draft, step, step.at.phase),
        ),
        Phase::Check => return Err(Refused::Source),
        _ => (hook, sink, inputs),
    })
}
