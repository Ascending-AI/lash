//! Scanner peers must be alive at their exact incarnation. Native scanners
//! dispose themselves when the querying peer's failure-detector watch is dead.
use super::metadata::NodeId;
use anyhow::{Result, ensure};
use serde::Serialize;
use std::path::PathBuf;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
enum DetectorState {
    Alive,
    Unavailable,
}

#[derive(Serialize)]
pub(super) struct ScannerReadiness {
    querying_peer: NodeId,
    observers: Vec<ScannerObserver>,
}

#[derive(Serialize)]
struct ScannerObserver {
    node: u32,
    log: PathBuf,
    state: DetectorState,
    event: String,
}

fn last_transition(log: &str, peer: &NodeId) -> Option<(DetectorState, String)> {
    let prefix = format!("N{}:{} transitioned from ", peer.id, peer.generation?);
    log.lines()
        .filter_map(|line| {
            let transition = line.trim().strip_prefix(&prefix)?;
            let (_, destination) = transition.rsplit_once(" to ")?;
            let state = if destination.starts_with("Alive (") {
                DetectorState::Alive
            } else {
                DetectorState::Unavailable
            };
            Some((state, line.trim().to_owned()))
        })
        .next_back()
}

#[cfg(test)]
fn peer_is_alive(log: &str, peer: &NodeId) -> bool {
    last_transition(log, peer).is_some_and(|(state, _)| state == DetectorState::Alive)
}

/// Wait for the actual failure-detector events used by native ScannerTask's
/// peer watch, not for another successful SQL query or metadata health.
pub(super) async fn await_readiness(
    peer: NodeId,
    logs: Vec<(u32, PathBuf)>,
    deadline: Instant,
) -> Result<ScannerReadiness> {
    ensure!(
        peer.generation.is_some(),
        "scanner peer lacks an incarnation"
    );
    loop {
        let mut observers = Vec::new();
        for (node, path) in &logs {
            let log = std::fs::read_to_string(path)?;
            if let Some((state, event)) =
                last_transition(&log, &peer).filter(|(state, _)| *state == DetectorState::Alive)
            {
                observers.push(ScannerObserver {
                    node: *node,
                    log: path.clone(),
                    state,
                    event,
                });
            }
        }
        if observers.len() == logs.len() {
            return Ok(ScannerReadiness {
                querying_peer: peer,
                observers,
            });
        }
        ensure!(
            Instant::now() < deadline,
            "scanner querying peer N{}:{:?} never became alive at every observer",
            peer.id,
            peer.generation
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// FIG-4964: a scanner opened by N1:3 on N2 was immediately disposed
    /// while N2's detector still knew only the dead N1:2, despite GetIdent
    /// and partition metadata already accepting N1:3.
    #[test]
    fn scanner_survives_only_after_its_querying_incarnation_is_alive() {
        let peer = NodeId {
            id: 1,
            generation: Some(3),
        };
        let prior = "  N1:2 transitioned from Suspect(since 5s ago) to Alive (gossip-age=2)\n  N1:2 transitioned from Alive to Dead (gossip-age=11)\n";
        assert!(
            !peer_is_alive(prior, &peer),
            "metadata readiness cannot authorize a scanner from the new incarnation"
        );
        let ready = format!(
            "{prior}  N1:3 transitioned from Suspect(since 5s ago) to Alive (gossip-age=1)\n"
        );
        assert!(peer_is_alive(&ready, &peer));
        assert!(!peer_is_alive(
            &format!("{ready}  N1:3 transitioned from Alive to Dead (gossip-age=11)\n"),
            &peer
        ));
    }
}
