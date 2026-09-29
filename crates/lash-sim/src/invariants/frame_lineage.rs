//! Every session's frames form one chain (FIG-4110 F8).
//!
//! A frame opens exactly once, through one `FrameOpen` node, whoever authors
//! it: a context-pressure hook, overflow recovery, `/compact` or a
//! `continue_as`. Walking a session's committed active path from its first
//! node to its head leaf, the path starts with a `FrameOpen`, every node
//! belongs to the frame whose `FrameOpen` is its nearest ancestor on the
//! path, no frame opens twice, and each `FrameOpen` after the first is the
//! child of a node of the frame that was current when it opened. A frame a
//! node names must have its own `FrameOpen` in the session.

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
            for (session, leaf) in &store.heads {
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
                // Every frame a node names has its own open.
                for node in nodes.values() {
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
                }
                // The active path, first node to leaf.
                let mut path = Vec::new();
                let mut seen = BTreeSet::new();
                let mut cursor = Some(leaf.as_str());
                while let Some(id) = cursor {
                    let Some(node) = nodes.get(id) else {
                        break;
                    };
                    if !seen.insert(id) {
                        violations.push(broken(format!("has a parent cycle at {id}"), &[node]));
                        break;
                    }
                    path.push(*node);
                    cursor = node.parent.as_deref();
                }
                path.reverse();
                let Some(first) = path.first() else {
                    continue;
                };
                if !first.frame_open {
                    violations.push(broken(
                        "does not open its first frame before its first node".to_owned(),
                        &[first],
                    ));
                    continue;
                }
                let mut opened = BTreeSet::new();
                let mut current: Option<&GraphNodeRow> = None;
                for node in path {
                    if node.frame_open {
                        if !opened.insert(node.node_id.as_str()) {
                            violations.push(broken(
                                format!("opens frame {} twice", node.node_id),
                                &[node],
                            ));
                        }
                        if let (Some(previous), Some(parent)) = (current, node.parent.as_deref()) {
                            let parent_frame =
                                nodes.get(parent).map(|parent| parent.frame.as_str());
                            if parent_frame != Some(previous.node_id.as_str()) {
                                violations.push(broken(
                                    format!(
                                        "opens frame {} from frame {}, not from frame {}, the one current when it opened",
                                        node.node_id,
                                        parent_frame.unwrap_or("-"),
                                        previous.node_id
                                    ),
                                    &[previous, node],
                                ));
                            }
                        }
                        current = Some(node);
                    }
                    let frame = current.map(|open| open.node_id.as_str());
                    if frame != Some(node.frame.as_str()) {
                        violations.push(broken(
                            format!(
                                "places {} in frame {}, but its nearest FrameOpen is {}",
                                node.node_id,
                                node.frame,
                                frame.unwrap_or("-")
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
