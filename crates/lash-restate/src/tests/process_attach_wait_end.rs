//! A process attach lives as long as the wait it serves, not as long as the
//! process (FIG-4757, FIG-4758), on the real `LashProcessAttach`,
//! `LashProcessWorkflow` and `LashDurableWaitIndex` handlers served by the
//! in-process server double.
//!
//! The awaited process is external and nothing ends it, so only the wait's
//! own end can end the attach and the terminal read it holds. Each law ends
//! the wait one way and then finds both invocations completed, the process
//! untouched and the index holding no watch.

#![allow(clippy::disallowed_methods)]

use super::*;
use crate::durable_wait::{
    DURABLE_WAIT_INDEX_METADATA_KEY, RestateDurableWaitAddress,
    RestateDurableWaitCancelDecidedRequest, durable_wait_index_key_for_scope,
    durable_wait_index_object_key,
};
use lash_restate_test::{RestateTestBackend, ServerConfig};

const ATTACH: &str = "LashProcessAttach";
const INDEX: &str = "LashDurableWaitIndex";
const BOUND: Duration = Duration::from_secs(60);

struct World {
    engine: RestateTestBackend,
    /// The external process nothing ends.
    awaited: ProcessId,
    /// The scope the wait belongs to.
    scope: ExecutionScope,
}

impl World {
    async fn new(seed: u64) -> Self {
        let engine = lash_restate_test::backend(seed, ServerConfig::default())
            .await
            .expect("double deployment");
        let registry = engine.lash_backend().process_registry();
        let awaited = registry
            .register_process(external_registration())
            .await
            .expect("awaited process")
            .id;
        let waiter = registry
            .register_process(external_registration())
            .await
            .expect("waiting process")
            .id;
        Self {
            engine,
            awaited,
            scope: ExecutionScope::Process { process_id: waiter },
        }
    }

    async fn key(&self, wait: AwaitEventWaitIdentity) -> AwaitEventKey {
        self.engine
            .lash_backend()
            .effect_host()
            .await_event_key(&self.scope, wait)
            .await
            .expect("wait key")
    }

    async fn send_attach(&self, key: &AwaitEventKey) {
        self.engine
            .ingress()
            .send_workflow_json(
                ATTACH,
                &RestateDurableWaitAddress::for_key(key).workflow_key,
                "run",
                &crate::Call::new(crate::RestateProcessAttachRequest {
                    process_id: self.awaited.clone(),
                    key: key.clone(),
                }),
            )
            .await
            .expect("send the attach");
    }

    fn attach_target(key: &AwaitEventKey) -> String {
        format!(
            "{ATTACH}/{}/run",
            RestateDurableWaitAddress::for_key(key).workflow_key
        )
    }

    fn terminal_read_target(&self) -> String {
        format!("LashProcessWorkflow/{}/await_terminal", self.awaited)
    }

    fn statuses(&self, target: &str) -> Vec<&'static str> {
        self.engine
            .server()
            .invocations()
            .into_iter()
            .filter(|view| view.target == target)
            .map(|view| view.status)
            .collect()
    }

    /// Waits until every invocation of `target` satisfies `settled`, with at
    /// least one present.
    async fn until(&self, target: &str, settled: impl Fn(&[&'static str]) -> bool) {
        tokio::time::timeout(BOUND, async {
            loop {
                let statuses = self.statuses(target);
                if !statuses.is_empty() && settled(&statuses) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "`{target}` did not settle within the bound: {:#?}",
                self.engine.server().invocations()
            )
        });
    }

    /// The attach is parked on the terminal read it issued.
    async fn armed(&self, key: &AwaitEventKey) {
        self.send_attach(key).await;
        self.until(&self.terminal_read_target(), |statuses| {
            statuses.iter().all(|status| *status != "completed")
        })
        .await;
    }

    /// The attach and its terminal read ended, the process did not, and the
    /// index holds no watch.
    async fn assert_observers_ended(&self, key: &AwaitEventKey) {
        let completed = |statuses: &[&'static str]| statuses.iter().all(|s| *s == "completed");
        self.until(&Self::attach_target(key), completed).await;
        if !self.statuses(&self.terminal_read_target()).is_empty() {
            self.until(&self.terminal_read_target(), completed).await;
        }
        let awaited = self
            .engine
            .lash_backend()
            .process_registry()
            .get_process(&self.awaited)
            .await
            .expect("read the awaited process")
            .expect("the awaited process is retained");
        assert!(
            awaited.outcome().is_none() && awaited.cancel_request.is_none(),
            "ending the wait neither ends nor cancels the process: {awaited:#?}"
        );
        assert_eq!(self.watches(), 0, "no watch outlives its wait");
    }

    /// The awakeable entries the scope's wait index holds.
    fn watches(&self) -> usize {
        self.engine
            .server()
            .object_state(INDEX, &durable_wait_index_key_for_scope(&self.scope))
            .get(DURABLE_WAIT_INDEX_METADATA_KEY)
            .map_or(0, |metadata| {
                let metadata: serde_json::Value =
                    serde_json::from_slice(metadata).expect("decode stamped metadata");
                metadata["body"]["awakeables"]
                    .as_array()
                    .map_or(0, Vec::len)
            })
    }

    async fn resolve(&self, key: &AwaitEventKey, resolution: Resolution) {
        self.engine
            .lash_backend()
            .effect_host()
            .resolve_await_event(key, resolution)
            .await
            .expect("resolve the wait");
    }
}

fn custom(name: &str) -> AwaitEventWaitIdentity {
    AwaitEventWaitIdentity::Custom {
        key: name.to_owned(),
    }
}

/// A wait resolved by anyone but the attach, here its cancellation, ends the
/// attach and the terminal read it held.
#[tokio::test]
async fn a_resolved_wait_ends_its_attach_and_terminal_read() {
    let world = World::new(0x4757_0001).await;
    let key = world.key(custom("attach-wait-resolved")).await;
    world.armed(&key).await;
    world.resolve(&key, Resolution::Cancelled).await;
    world.assert_observers_ended(&key).await;
}

/// A completion key its group child's cancel decision closed never resolves:
/// the fence itself ends the attach, parked wait or none.
#[tokio::test]
async fn a_cancel_decided_key_ends_its_attach_and_terminal_read() {
    let world = World::new(0x4757_0002).await;
    let wait = AwaitEventWaitIdentity::tool_completion(lash_core::ToolCallId::fixture(
        "attach-wait-cancel-decided",
    ));
    let key = world.key(wait.clone()).await;
    world.armed(&key).await;
    let _: crate::Reply<()> = world
        .engine
        .ingress()
        .call_object_json(
            INDEX,
            &durable_wait_index_object_key(&RestateDurableWaitAddress::for_key(&key)),
            "fence_cancel_decided",
            &crate::Call::new(RestateDurableWaitCancelDecidedRequest {
                scope: world.scope.clone(),
                wait,
            }),
        )
        .await
        .expect("close the completion key");
    world.assert_observers_ended(&key).await;

    // An attach that arrives after the decision finds the key closed.
    let late = World::new(0x4757_0003).await;
    let wait = AwaitEventWaitIdentity::tool_completion(lash_core::ToolCallId::fixture(
        "attach-wait-cancel-decided-first",
    ));
    let key = late.key(wait.clone()).await;
    let _: crate::Reply<()> = late
        .engine
        .ingress()
        .call_object_json(
            INDEX,
            &durable_wait_index_object_key(&RestateDurableWaitAddress::for_key(&key)),
            "fence_cancel_decided",
            &crate::Call::new(RestateDurableWaitCancelDecidedRequest {
                scope: late.scope.clone(),
                wait,
            }),
        )
        .await
        .expect("close the completion key");
    late.send_attach(&key).await;
    late.assert_observers_ended(&key).await;
}

/// An attach armed on a wait that already ended reads no terminal it would
/// have nobody to hand to.
#[tokio::test]
async fn an_attach_on_an_ended_wait_ends_at_once() {
    let world = World::new(0x4757_0004).await;
    let key = world.key(custom("attach-wait-ended-first")).await;
    world.resolve(&key, Resolution::Timeout).await;
    world.send_attach(&key).await;
    world.assert_observers_ended(&key).await;
}

/// The terminal still wins a live wait: the attach resolves the key with it,
/// and that resolve drops the attach's own watch.
#[tokio::test]
async fn a_terminal_resolves_a_live_wait_and_drops_the_watch() {
    let world = World::new(0x4757_0005).await;
    let key = world.key(custom("attach-wait-terminal")).await;
    world.armed(&key).await;
    let backend = world.engine.lash_backend();
    let output = process_success(serde_json::json!({ "answered": true }));
    backend
        .process_registry()
        .complete_process(
            &world.awaited,
            output.clone(),
            lash_core::ProcessCompletionAuthority::external_owner(),
        )
        .await
        .expect("the process's terminal");
    backend
        .process_work()
        .port()
        .publish_process_terminal(&world.awaited, &output, "attach-wait-terminal")
        .await
        .expect("publish the terminal");
    let completed = |statuses: &[&'static str]| statuses.iter().all(|s| *s == "completed");
    world.until(&World::attach_target(&key), completed).await;
    world.until(&world.terminal_read_target(), completed).await;
    assert_eq!(world.watches(), 0, "the resolve dropped the attach's watch");
    let outcome = backend
        .effect_host()
        .resolve_await_event(&key, Resolution::Cancelled)
        .await
        .expect("read the wait's terminal");
    assert!(
        matches!(
            outcome,
            lash_core::ResolveOutcome::AlreadyResolved {
                terminal: Resolution::Ok(_)
            }
        ),
        "the wait holds the process's terminal: {outcome:?}"
    );
}
