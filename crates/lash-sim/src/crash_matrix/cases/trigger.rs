//! The trigger seam: two subscriptions match one source, and the host emits
//! one occurrence through the production trigger router. The router commits
//! the occurrence, a process per delivery and the deliveries bound to them
//! in one `trigger.start` mailbox transaction; each started process ends at
//! once. An emission that did not answer is emitted again under the same
//! occurrence key, as a host retries.
//!
//! Laws: one occurrence, one delivery per subscription, one process per
//! delivery under the delivery's start key and no other process, and every
//! started process ended with its success.

use std::collections::BTreeSet;
use std::sync::Arc;

use lash_core_execution::facade_support::{TriggerRouter, trigger_delivery_start_key};
use lash_core_execution::{
    ActorContext, JsonSchema, ProcessEngineRegistration, ProcessEngineRegistry, ProcessIdentity,
    ProcessInput, ProcessListFilter, ProcessOriginator, ProcessStatusFilter, ProcessWorkWiring,
    TriggerCommand, TriggerOccurrenceFilter, TriggerOccurrenceRequest, TriggerOwnerScope,
    TriggerSubscriptionDraft,
};
use lash_durable::{DurableError, StoreFailure, StoreFailureKind};
use lash_durable_test::{Cut, SimNodes};
use serde_json::json;

use super::{LOOK_EVERY, LOOKS, ended, find, outcome, process_actor};
use crate::crash_matrix::deployment::Workload;
use crate::crash_matrix::engine::{KIND, SimProcessEngine, ends_at_once};
use crate::crash_matrix::world::{World, poll, retry};

const SOURCE_TYPE: &str = "lash-sim.event";
const SOURCE_KEY: &str = "lash-sim-source";
const SUBSCRIPTIONS: [&str; 2] = ["first", "second"];

#[derive(Default)]
pub struct TriggerCase {
    tag: String,
}

impl TriggerCase {
    /// The case with its occurrence named apart by `tag`.
    #[must_use]
    pub fn tagged(tag: &str) -> Self {
        Self {
            tag: tag.to_owned(),
        }
    }

    fn source_key(&self) -> String {
        format!("{SOURCE_KEY}{}", self.tag)
    }

    fn request(&self) -> TriggerOccurrenceRequest {
        TriggerOccurrenceRequest::new(
            SOURCE_TYPE,
            self.source_key(),
            json!({ "lash-sim": true }),
            format!("lash-sim-occurrence{}", self.tag),
        )
    }
}

/// A refused emission as a store failure, so the host emits again.
fn emit_failure(error: impl std::fmt::Display) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Unavailable,
        message: error.to_string(),
    })
}

#[async_trait::async_trait]
impl Workload for TriggerCase {
    async fn seed(&self, world: &Arc<World>, _nodes: &Arc<SimNodes>) -> Result<(), String> {
        let backend = world.backend()?;
        let env_ref = lash_core_execution::testing::process_execution_env_fixture(
            &*backend.process_env_store(),
        )
        .await;
        for key in SUBSCRIPTIONS {
            backend
                .trigger_store()
                .execute_command(
                    &format!("lash-sim-register-{key}{}", self.tag),
                    TriggerCommand::Register {
                        owner_scope: TriggerOwnerScope::host("lash-sim")
                            .map_err(|error| error.to_string())?,
                        actor: ProcessOriginator::host_scoped("lash-sim"),
                        draft: TriggerSubscriptionDraft::for_process(
                            format!("lash-sim/{key}{}", self.tag),
                            env_ref.clone(),
                            SOURCE_TYPE,
                            self.source_key(),
                            ProcessInput::Engine {
                                kind: KIND.to_owned(),
                                payload: ends_at_once(key),
                            },
                            ProcessIdentity::labelled(KIND, Some(key)),
                        )
                        .with_payload_schema(JsonSchema::any()),
                    },
                )
                .await
                .map_err(|error| error.to_string())?
                .map_err(|error| error.to_string())?;
        }
        let host = Arc::clone(world);
        let request = self.request();
        world.spawn(async move { emit(&host, request).await });
        Ok(())
    }

    async fn done(&self, world: &World, nodes: &SimNodes) -> bool {
        let processes = delivered(world, &self.source_key()).await;
        processes.len() == SUBSCRIPTIONS.len() && ended(nodes, &processes).await
    }

    async fn laws(&self, world: &World, _nodes: &SimNodes, _cut: Option<&Cut>) -> Vec<String> {
        let Ok(backend) = world.backend() else {
            return vec!["the run's backend is not built".to_owned()];
        };
        let triggers = backend.trigger_store();
        let occurrences: Vec<_> = triggers
            .list_occurrences(TriggerOccurrenceFilter::default())
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|occurrence| occurrence.source_key == self.source_key())
            .collect();
        let [occurrence] = occurrences.as_slice() else {
            return vec![format!("the store holds {} occurrences", occurrences.len())];
        };
        let mut violations = Vec::new();
        let deliveries = triggers
            .list_deliveries_by_occurrence_id(&occurrence.occurrence_id)
            .await
            .unwrap_or_default();
        let subscriptions: BTreeSet<&str> = deliveries
            .iter()
            .map(|delivery| delivery.subscription.subscription_key.as_str())
            .collect();
        if deliveries.len() != SUBSCRIPTIONS.len() || subscriptions.len() != SUBSCRIPTIONS.len() {
            violations.push(format!(
                "the occurrence holds {} deliveries to {subscriptions:?}",
                deliveries.len()
            ));
        }
        let bound: BTreeSet<_> = deliveries
            .iter()
            .map(|delivery| delivery.process_id.clone())
            .collect();
        let keys: BTreeSet<_> = deliveries.iter().map(trigger_delivery_start_key).collect();
        let started: BTreeSet<_> = backend
            .process_registry()
            .list_processes(&ProcessListFilter {
                status: ProcessStatusFilter::Any,
                ..ProcessListFilter::default()
            })
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|record| {
                record
                    .start_key
                    .as_ref()
                    .is_some_and(|key| keys.contains(key))
            })
            .map(|record| record.id)
            .collect();
        if bound.len() != deliveries.len() || started != bound {
            violations.push(format!(
                "the deliveries are bound to {bound:?}, their start keys hold {started:?}"
            ));
        }
        for delivery in &deliveries {
            let key = trigger_delivery_start_key(delivery);
            match backend
                .process_registry()
                .get_process_by_start_key(&key)
                .await
            {
                Ok(Some(record)) if record.id == delivery.process_id => {}
                other => violations.push(format!(
                    "the start key of the delivery to `{}` holds {other:?}",
                    delivery.subscription.subscription_key
                )),
            }
            match outcome(world, &delivery.process_id).await {
                Some(end) if find(&end, "ended") == Some(&json!(true)) => {}
                other => violations.push(format!(
                    "the process started for `{}` did not end with its success: {other:?}",
                    delivery.subscription.subscription_key
                )),
            }
        }
        violations
    }
}

/// The processes the occurrence on `source_key` started, once its
/// deliveries are recorded.
async fn delivered(world: &World, source_key: &str) -> Vec<lash_core_execution::ProcessId> {
    let Ok(backend) = world.backend() else {
        return Vec::new();
    };
    let triggers = backend.trigger_store();
    let Ok(occurrences) = triggers
        .list_occurrences(TriggerOccurrenceFilter::default())
        .await
    else {
        return Vec::new();
    };
    let mut processes = Vec::new();
    for occurrence in occurrences
        .iter()
        .filter(|occurrence| occurrence.source_key == source_key)
    {
        for delivery in triggers
            .list_deliveries_by_occurrence_id(&occurrence.occurrence_id)
            .await
            .unwrap_or_default()
        {
            processes.push(delivery.process_id);
        }
    }
    processes
}

/// The host: emit the occurrence until an emission answers, then track the
/// processes it started.
async fn emit(world: &Arc<World>, request: TriggerOccurrenceRequest) {
    let engines = ProcessEngineRegistry::new().with_registration(
        ProcessEngineRegistration::accepting(Arc::new(SimProcessEngine)),
    );
    let answered = retry(world, |host| {
        let router = TriggerRouter::new(
            host.trigger_store(),
            ProcessWorkWiring::without_process_work(host.process_registry()),
        )
        .with_process_artifacts(host.process_env_store(), engines.clone());
        let request = request.clone();
        async move {
            router
                .emit(request, &ActorContext::detached(host))
                .await
                .map_err(emit_failure)
        }
    })
    .await;
    if answered.is_ok() {
        world.note("trigger.emitted");
    }
    let source_key = request.source_key.clone();
    let started = poll(world, LOOK_EVERY, LOOKS, || async {
        let processes = delivered(world, &source_key).await;
        (processes.len() == SUBSCRIPTIONS.len()).then_some(processes)
    })
    .await;
    for process in started.unwrap_or_default() {
        if let Ok(actor) = process_actor(&process) {
            world.track(actor);
        }
    }
}
