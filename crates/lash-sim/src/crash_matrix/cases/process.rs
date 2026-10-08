//! The process seam: a root process runs a `Once` and a `Repeatable` step,
//! pins a custom key and awaits it; the host lists the root's pending keys and resolves it (`wait.resolve`); the root then
//! awaits a detached process that idles, until that wait's one-second
//! deadline (`wait.timeout`), and ends; its terminal cascades over its three
//! `Until` children in batches of two (`cascade.batch`).
//!
//! Laws: the root ended with its await timed out, and with its key resolved
//! whenever the host's resolve won; every child ended cancelled by its
//! parent's end; the idling process the root awaited lives on.

use std::sync::{Arc, Mutex};

use lash_core::sync::MutexExt as _;
use lash_core_execution::runtime::actor::waits::{self, Resolution};
use lash_core_execution::{ProcessId, ScopeId};
use lash_durable::ActorState;
use lash_durable::domain::ResolveAnswer;
use lash_durable_test::{Cut, SimNodes};
use serde_json::{Value, json};

use super::{LOOK_EVERY, LOOKS, ended, find, outcome, process_actor, register, state};
use crate::crash_matrix::deployment::{CASCADE_BATCH, Workload};
use crate::crash_matrix::engine::{hold, root};
use crate::crash_matrix::world::{World, poll, retry};

/// How many `Until` children the root has: more than one cascade batch.
const CHILDREN: usize = CASCADE_BATCH + 1;

#[derive(Clone, Default)]
struct Seeded {
    held: Option<ProcessId>,
    root: Option<ProcessId>,
    children: Vec<ProcessId>,
}

#[derive(Default)]
pub struct ProcessCase {
    seeded: Mutex<Seeded>,
}

impl ProcessCase {
    fn seeded(&self) -> Seeded {
        self.seeded.lock_recover().clone()
    }
}

#[async_trait::async_trait]
impl Workload for ProcessCase {
    async fn seed(&self, world: &Arc<World>, _nodes: &Arc<SimNodes>) -> Result<(), String> {
        let held = register(world, hold("h"), None).await?;
        let root_process = register(world, root("r", &held), None).await?;
        let mut children = Vec::new();
        for index in 0..CHILDREN {
            children.push(
                register(
                    world,
                    hold(&format!("c{index}")),
                    Some(ScopeId::process(root_process.clone())),
                )
                .await?,
            );
        }
        *self.seeded.lock_recover() = Seeded {
            held: Some(held),
            root: Some(root_process.clone()),
            children,
        };
        let host = Arc::clone(world);
        world.spawn(async move { resolve_key(&host, &root_process).await });
        Ok(())
    }

    async fn done(&self, _world: &World, nodes: &SimNodes) -> bool {
        let seeded = self.seeded();
        let mut tree: Vec<ProcessId> = seeded.root.into_iter().collect();
        tree.extend(seeded.children);
        !tree.is_empty() && ended(nodes, &tree).await
    }

    async fn laws(&self, world: &World, nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
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
        for child in &seeded.children {
            match outcome(world, child).await {
                Some(ended) if find(&ended, "origin") == Some(&json!("parent_ended")) => {}
                other => violations.push(format!(
                    "child {child} did not end by its parent's end: {other:?}"
                )),
            }
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

/// The host: list the root's pinned key, then resolve it.
async fn resolve_key(world: &Arc<World>, root_process: &ProcessId) {
    if let Some(key) = pinned_key(world, root_process).await {
        resolve(world, key).await;
    }
}

/// The key `root_process` pinned, once its durable wait exists: the root is then
/// parked awaiting it.
pub(super) async fn pinned_key(world: &Arc<World>, root_process: &ProcessId) -> Option<String> {
    poll(world, LOOK_EVERY, LOOKS, || async {
        let backend = world.backend().ok()?;
        let actor = process_actor(root_process).ok()?;
        waits::outstanding_keys(&backend, &actor)
            .await
            .ok()?
            .into_iter()
            .next()
            .map(|key| key.as_str().to_owned())
    })
    .await
}

/// The host: resolve `key`, noting `key.resolved` when the resolve won.
pub(super) async fn resolve(world: &Arc<World>, key: String) {
    let answer =
        retry(world, |host| {
            let key = key.clone();
            async move {
                waits::resolve_host(&host, &key, Resolution::Ok(json!({ "answer": "sim" }))).await
            }
        })
        .await;
    if matches!(
        answer,
        Ok(ResolveAnswer::Resolved | ResolveAnswer::AlreadyResolved)
    ) {
        world.note("key.resolved");
    }
}
