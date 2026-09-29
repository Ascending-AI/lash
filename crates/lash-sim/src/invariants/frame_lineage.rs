//! Every session's frames form one chain (FIG-4110 F8).
//!
//! A frame opens exactly once, through one `FrameOpen` node, whoever authors
//! it: a context-pressure hook, overflow recovery, `/compact` or a
//! `continue_as`. For every node a session holds, on its active path or off
//! it (a fork's branch, an abandoned open): the node belongs to the frame
//! whose `FrameOpen` is its nearest ancestor, itself included, so every
//! `FrameOpen` names only itself and hangs off a node of the frame that was
//! current when it opened; a node with no parent opens the session's first
//! frame; and a frame a node names has its own `FrameOpen` in the session.
//! A frame opened twice would be a `FrameOpen` placed in another frame, and a
//! path that revisits a node is a parent cycle.

use std::collections::{BTreeMap, BTreeSet};

use super::{GraphNodeRow, History, HistoryChecker, Violation};

pub(super) struct FrameLineage;

const INVARIANT: &str = "frames-form-one-chain";

impl HistoryChecker for FrameLineage {
    fn invariant(&self) -> &'static str {
        INVARIANT
    }

    fn observed(&self, history: &History) -> usize {
        history
            .stores
            .iter()
            .flat_map(|store| &store.graph_nodes)
            .filter(|node| node.frame_open)
            .count()
    }

    fn check(&self, history: &History) -> Vec<Violation> {
        let mut violations = Vec::new();
        for store in &history.stores {
            let mut sessions: BTreeMap<&str, BTreeMap<&str, &GraphNodeRow>> = BTreeMap::new();
            for node in &store.graph_nodes {
                sessions
                    .entry(node.session.as_str())
                    .or_default()
                    .insert(node.node_id.as_str(), node);
            }
            for session in store.heads.iter().map(|(session, _)| session) {
                let Some(nodes) = sessions.get(session.as_str()) else {
                    continue;
                };
                let broken = |detail: String, rows: &[&GraphNodeRow]| {
                    rows.iter().fold(
                        Violation::new(INVARIANT, format!("{}: {session} {detail}", store.label))
                            .session(session.clone()),
                        |violation, row| violation.row(row.render()),
                    )
                };
                // Every node's nearest `FrameOpen`, itself included, memoized
                // over the walk from the node to its session's root.
                let mut nearest: BTreeMap<&str, Option<&str>> = BTreeMap::new();
                let mut rooted: BTreeSet<&str> = BTreeSet::new();
                for node in nodes.values() {
                    if let Err(at) = reaches_a_root(nodes, node, &mut rooted) {
                        violations.push(broken(format!("has a parent cycle at {at}"), &[node]));
                        continue;
                    }
                    match nodes.get(node.frame.as_str()) {
                        Some(open) if open.frame_open => {}
                        _ => violations.push(broken(
                            format!(
                                "names frame {} with no FrameOpen in the session",
                                node.frame
                            ),
                            &[node],
                        )),
                    }
                    if node.parent.is_none() && !node.frame_open {
                        violations.push(broken(
                            "does not open its first frame before its first node".to_owned(),
                            &[node],
                        ));
                    }
                    let open = nearest_frame_open(nodes, node, &mut nearest);
                    if open != Some(node.frame.as_str()) {
                        violations.push(broken(
                            format!(
                                "places {} in frame {}, but its nearest FrameOpen is {}",
                                node.node_id,
                                node.frame,
                                open.unwrap_or("-")
                            ),
                            &[node],
                        ));
                    }
                }
            }
        }
        violations
    }
}

/// Whether `node`'s ancestry ends, at a root or at a parent the session does
/// not hold, without revisiting a node. `Err` names the node it revisits.
/// `rooted` memoizes the nodes already known to end.
fn reaches_a_root<'a>(
    nodes: &BTreeMap<&'a str, &'a GraphNodeRow>,
    node: &'a GraphNodeRow,
    rooted: &mut BTreeSet<&'a str>,
) -> Result<(), &'a str> {
    let mut walked = BTreeSet::new();
    let mut cursor = Some(node);
    while let Some(at) = cursor {
        let id = at.node_id.as_str();
        if rooted.contains(id) {
            break;
        }
        if !walked.insert(id) {
            return Err(id);
        }
        cursor = at
            .parent
            .as_deref()
            .and_then(|parent| nodes.get(parent).copied());
    }
    rooted.extend(walked);
    Ok(())
}

/// The nearest `FrameOpen` at or above `node`, or `None` when its ancestry
/// leaves the session before reaching one. The ancestry is acyclic
/// ([`reaches_a_root`]).
fn nearest_frame_open<'a>(
    nodes: &BTreeMap<&'a str, &'a GraphNodeRow>,
    node: &'a GraphNodeRow,
    nearest: &mut BTreeMap<&'a str, Option<&'a str>>,
) -> Option<&'a str> {
    let mut walked = Vec::new();
    let mut cursor = Some(node);
    let found = loop {
        let Some(at) = cursor else {
            break None;
        };
        let id = at.node_id.as_str();
        if let Some(known) = nearest.get(id) {
            break *known;
        }
        if at.frame_open {
            break Some(id);
        }
        walked.push(id);
        cursor = at
            .parent
            .as_deref()
            .and_then(|parent| nodes.get(parent).copied());
    };
    for id in walked {
        nearest.insert(id, found);
    }
    found
}
