//! The drain seam: a rolling deploy by release (ADR 0106 §2). A root
//! process runs its `Once` and `Repeatable` steps; while their bodies run,
//! the host drains the serving node, which marks itself draining
//! (`node.drain`) and claims nothing more, lets the bodies settle, releases
//! the root `ready` at its committed phase (`drain.release`) and stops; the
//! host starts a new boot of that node, as the next build's. A node claims
//! the root, which pins a key and parks awaiting it; the host resolves the
//! key, and the root awaits the process it named until its deadline and
//! ends.
//!
//! Laws: the root ended with its await timed out, and with its key resolved
//! whenever the host's resolve won; the drained node stopped `Drained`
//! unless the cut fell on it, and uncut the drain ran to that stop; the
//! process the root awaited lives on. The invariants hold the rest: no
//! `Once` body ran twice across the release.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::sync::MutexExt as _;
use lash_core_execution::ProcessId;
use lash_durable::ActorState;
use lash_durable::runner::Stopped;
use lash_durable_test::{Cut, SimNodes};
use serde_json::Value;

use super::process::{pinned_key, resolve};
use super::{LOOK_EVERY, LOOKS, ended, find, outcome, process_actor, register, state};
use crate::crash_matrix::deployment::{NODES, Workload};
use crate::crash_matrix::engine::{ONCE, hold, root};
use crate::crash_matrix::world::{World, poll};

/// What the host notes, with the node, when it drains one.
const STARTED: &str = "drain: started on";
/// What the host notes once a drained node stopped `Drained`.
const DRAINED: &str = "drain: stopped drained";
/// What the host notes once a drained node stopped otherwise.
const STOPPED_OTHERWISE: &str = "drain: stopped otherwise:";
/// How often the host looks for the root's running bodies: well inside
/// the time a body takes.
const LOOK_FOR_BODIES: Duration = Duration::from_millis(5);

#[derive(Clone, Default)]
struct Seeded {
    held: Option<ProcessId>,
    root: Option<ProcessId>,
}

#[derive(Default)]
pub struct DrainCase {
    seeded: Mutex<Seeded>,
}

impl DrainCase {
    fn seeded(&self) -> Seeded {
        self.seeded.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl Workload for DrainCase {
    async fn seed(&self, world: &Arc<World>, _nodes: &Arc<SimNodes>) -> Result<(), String> {
        let held = register(world, hold("dh"), None).await?;
        let root_process = register(world, root("dr", &held), None).await?;
        *self.seeded.lock_recover() = Seeded {
            held: Some(held),
            root: Some(root_process.clone()),
        };
        let host = Arc::clone(world);
        world.spawn(async move { deploy(&host, &root_process).await });
        Ok(())
    }

    async fn done(&self, _world: &World, nodes: &SimNodes) -> bool {
        let seeded = self.seeded();
        let root: Vec<ProcessId> = seeded.root.into_iter().collect();
        !root.is_empty() && ended(nodes, &root).await
    }

    async fn laws(&self, world: &World, nodes: &SimNodes, cut: Option<&Cut>) -> Vec<String> {
        let seeded = self.seeded();
        let mut violations = Vec::new();
        let Some(root_process) = &seeded.root else {
            return vec!["the root was never seeded".to_owned()];
        };
        match outcome(world, root_process).await {
            Some(ended) => {
                if find(&ended, "timed_out") != Some(&Value::Bool(true)) {
                    violations.push(format!("the root's await did not time out: {ended}"));
                }
                if world.noted("key.resolved") && find(&ended, "key") != Some(&Value::Bool(true)) {
                    violations.push(format!(
                        "the host's resolve won, but the root saw no resolution: {ended}"
                    ));
                }
            }
            None => violations.push("the root has no outcome".to_owned()),
        }
        let notes = world.notes();
        let drained = notes
            .iter()
            .find_map(|note| note.strip_prefix(STARTED))
            .map(str::trim);
        if cut.is_none_or(|cut| Some(&*cut.node) != drained) {
            violations.extend(
                notes
                    .into_iter()
                    .filter(|note| note.starts_with(STOPPED_OTHERWISE)),
            );
        }
        if cut.is_none() && !world.noted(DRAINED) {
            violations.push("uncut, the drained node did not stop drained".to_owned());
        }
        if let Some(held) = &seeded.held
            && let Ok(actor) = process_actor(held)
            && state(nodes, &actor).await == Some(ActorState::Terminal)
        {
            violations.push(format!("the awaited process {held} ended"));
        }
        violations
    }
}

/// The host: once the root's `Once` body runs, drain the serving node and,
/// once it stopped, start its next boot; then resolve the root's key.
async fn deploy(world: &Arc<World>, root_process: &ProcessId) {
    let running = poll(world, LOOK_FOR_BODIES, LOOKS, || async {
        (!world.ledger().of_tool(ONCE).is_empty()).then_some(())
    })
    .await;
    if running.is_some()
        && let Some(nodes) = world.nodes()
        && let Some(serving) = NODES.into_iter().find(|node| nodes.serving(node))
    {
        nodes.drain(serving);
        world.note(format!("{STARTED} {serving}"));
        let stopped = poll(world, LOOK_EVERY, LOOKS, || async {
            (!nodes.serving(serving)).then_some(())
        })
        .await;
        if stopped.is_some() {
            match nodes.stopped(serving).await {
                Some(Ok(Stopped::Drained)) => world.note(DRAINED),
                Some(other) => world.note(format!("{STOPPED_OTHERWISE} {other:?}")),
                // Killed by a fault: nothing to say of how it stopped.
                None => {}
            }
            nodes.start(serving);
        }
    }
    if let Some(key) = pinned_key(world, root_process).await {
        resolve(world, key).await;
    }
}
