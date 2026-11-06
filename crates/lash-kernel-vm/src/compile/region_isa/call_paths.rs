//! Guest completion enters a distinct caller Restore with the task's Incoming
//! holding. The physical incoming register is not a fabricated logical Slot.
//! A delivering action suspends at its Enter boundary; the machine delivers
//! its result as the task's Incoming, which the action's Commit takes.
use super::*;
use crate::compile::room::CallSource;
use std::collections::BTreeMap;

pub(super) fn connect(
    node: doc::Node<'_>,
    site: &Site,
    next: Option<Point>,
    source: &Source,
    graph: &mut BTreeMap<Point, lower::Edge>,
) -> Result<Option<Point>, Refused> {
    // The point the operands flow into, and the one after it that the
    // completion resumes at.
    let (enter, resume) = match node {
        doc::Node::Expr(doc::Expr::Call { function, .. }) => {
            if !matches!(source.calls.get(function), Some(CallSource::Body(_))) {
                return Ok(None);
            }
            (Phase::Commit, Phase::Restore)
        }
        doc::Node::Expr(doc::Expr::Act(act)) if matches!(act.kind, doc::ActKind::Call(_)) => {
            (Phase::Enter, Phase::Restore)
        }
        doc::Node::Expr(doc::Expr::Act(_)) => (Phase::Enter, Phase::Commit),
        _ => return Ok(None),
    };
    let enter = lower::point(site, enter);
    let after = next.map_or(lower::Edge::Stop, lower::Edge::Next);
    if crate::compile::room::native_act(node, &source.calls).is_some() {
        // A native call act answers at its Enter: there is no resume point.
        graph.insert(enter.clone(), after);
    } else {
        let resume = lower::point(site, resume);
        graph.insert(resume.clone(), after);
        graph.insert(enter.clone(), lower::Edge::Next(resume));
    }
    let mut entry = enter;
    for (index, child) in node.children().into_iter().enumerate().rev() {
        entry = lower::connect(
            child,
            &site.child(u32::try_from(index).map_err(|_| Refused::Path)?),
            Some(entry),
            source,
            graph,
        )?;
    }
    if matches!(node, doc::Node::Expr(doc::Expr::Act(act)) if matches!(act.kind, doc::ActKind::Call(doc::Callee::Value(_)) | doc::ActKind::Spawn(doc::Callee::Value(_))))
    {
        let open = lower::point(site, Phase::Open);
        graph.insert(open.clone(), lower::Edge::Next(entry));
        entry = open;
    }
    Ok(Some(entry))
}
