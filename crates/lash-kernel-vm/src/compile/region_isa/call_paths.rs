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
    let doc::Node::Expr(doc::Expr::Call { function, .. }) = node else {
        return Ok(None);
    };
    if !matches!(source.calls.get(function), Some(CallSource::Body(_))) {
        return Ok(None);
    }
    let restore = lower::point(site, Phase::Restore);
    let commit = lower::point(site, Phase::Commit);
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
    Ok(Some(entry))
}
