//! Guest completion enters a distinct caller Restore with the task's Incoming
//! holding. The physical incoming register is not a fabricated logical Slot.
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
    let dynamic = match node {
        doc::Node::Expr(doc::Expr::Call { function, .. }) => {
            if !matches!(source.calls.get(function), Some(CallSource::Body(_))) {
                return Ok(None);
            }
            false
        }
        doc::Node::Expr(doc::Expr::Act(act)) if matches!(act.kind, doc::ActKind::Call(_)) => true,
        _ => return Ok(None),
    };
    let restore = lower::point(site, Phase::Restore);
    let commit = lower::point(site, if dynamic { Phase::Enter } else { Phase::Commit });
    graph.insert(
        restore.clone(),
        next.map_or(lower::Edge::Stop, lower::Edge::Next),
    );
    graph.insert(commit.clone(), lower::Edge::Next(restore));
    let mut entry = commit;
    for (index, child) in node.children().into_iter().enumerate().rev() {
        entry = lower::connect(
            child,
            &site.child(u32::try_from(index).map_err(|_| Refused::Path)?),
            Some(entry),
            source,
            graph,
        )?;
    }
    if matches!(node, doc::Node::Expr(doc::Expr::Act(act)) if matches!(act.kind, doc::ActKind::Call(doc::Callee::Value(_))))
    {
        let open = lower::point(site, Phase::Open);
        graph.insert(open.clone(), lower::Edge::Next(entry));
        entry = open;
    }
    Ok(Some(entry))
}
