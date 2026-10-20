//! Activation-scoped observations; no decision or lifetime changes.
use std::cell::Cell;
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use lash_durable::runner::{Activation, Exit, Owned};
use lash_protocol_types::sync::MutexExt;
use serde::Serialize;

tokio::task_local! {
    static ACTIVATION: (String, Cell<usize>);
}

#[derive(Default)]
pub struct Counters {
    calls: Mutex<BTreeMap<(String, &'static str), usize>>,
    discoveries: Mutex<Vec<Discovery>>,
}
#[derive(Clone, Serialize)]
pub struct Discovery {
    actor: String,
    epoch: i64,
    source: String,
    cause: String,
}
#[derive(Serialize)]
pub struct Call {
    actor: String,
    operation: &'static str,
    calls: usize,
}
impl Counters {
    pub fn record(&self, operation: &'static str) {
        let actor = ACTIVATION
            .try_with(|(actor, _)| actor.clone())
            .unwrap_or_else(|_| "outside-activation".into());
        *self
            .calls
            .lock_recover()
            .entry((actor, operation))
            .or_default() += 1;
    }
    pub fn facts(&self) {
        let first = ACTIVATION.try_with(|(_, n)| {
            let old = n.get();
            n.set(old + 1);
            old == 0
        });
        self.record(match first {
            Ok(true) => "run_facts.restore",
            Ok(false) => "run_facts.warm",
            Err(_) => "run_facts.outside",
        });
    }
    pub fn calls(&self) -> Vec<Call> {
        self.calls
            .lock_recover()
            .iter()
            .map(|((actor, operation), calls)| Call {
                actor: actor.clone(),
                operation,
                calls: *calls,
            })
            .collect()
    }
    pub fn discoveries(&self) -> Vec<Discovery> {
        self.discoveries.lock_recover().clone()
    }
}

pub struct ObservedActivation {
    pub inner: Arc<dyn Activation>,
    pub counters: Arc<Counters>,
}
#[async_trait::async_trait]
impl Activation for ObservedActivation {
    async fn activate(&self, owned: Owned) -> Exit {
        let actor = owned.actor().to_string();
        self.counters.discoveries.lock_recover().push(Discovery {
            actor: actor.clone(),
            epoch: owned.epoch().0,
            source: format!("{:?}", owned.claimed_on()),
            cause: format!("{:?}", owned.cause()),
        });
        ACTIVATION
            .scope((actor, Cell::new(0)), self.inner.activate(owned))
            .await
    }
    async fn hold(&self) -> Result<lash_durable::NodeHolding, lash_durable::DurableError> {
        self.inner.hold().await
    }
}
