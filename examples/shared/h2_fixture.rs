//! H2 scripted provider and real tool bodies, composed by the product host.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;

#[path = "h2_tool_bodies.rs"]
mod bodies;
use bodies::{BodyResult, ToolBodies, ToolDelivery};
#[path = "h2_provider.rs"]
pub(crate) mod provider;

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct FixtureConfig {
    scenario: String,
    delivery_ledger: PathBuf,
    provider_ledger: PathBuf,
    body_callback_url: String,
    #[serde(default)]
    deferred_loser: bool,
    #[serde(default)]
    receiver_hold: bool,
    #[serde(default)]
    intent_process: Option<lash::ProcessId>,
    #[serde(default = "event_type")]
    intent_event_type: String,
}

fn event_type() -> String {
    "h2_mutation".to_owned()
}

pub(crate) struct Fixture {
    config: FixtureConfig,
    receiver: Arc<OnceLock<lash::ProcessId>>,
}

impl Fixture {
    pub(crate) fn from_env(variable: &str) -> Result<Option<Self>> {
        let Some(path) = std::env::var_os(variable) else {
            return Ok(None);
        };
        let bytes = std::fs::read(Path::new(&path)).with_context(|| format!("read {variable}"))?;
        let config: FixtureConfig = serde_json::from_slice(&bytes)?;
        ensure!(
            matches!(
                config.scenario.as_str(),
                "S01"
                    | "S02"
                    | "S05"
                    | "S08"
                    | "S09"
                    | "S10"
                    | "S11"
                    | "S12"
                    | "S23"
                    | "S31"
                    | "S32"
            ),
            "unknown H2 fixture scenario"
        );
        let url = reqwest::Url::parse(&config.body_callback_url)?;
        ensure!(
            url.scheme() == "http"
                && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]")),
            "body controls must be a case-owned loopback HTTP endpoint"
        );
        let receiver = Arc::new(OnceLock::new());
        let retained_path = config.delivery_ledger.with_extension("receiver.json");
        let retained = match &config.intent_process {
            Some(id) => Some(id.clone()),
            None if retained_path.exists() => {
                Some(serde_json::from_slice(&std::fs::read(retained_path)?)?)
            }
            None => None,
        };
        if let Some(id) = retained {
            receiver
                .set(id)
                .map_err(|_| anyhow!("receiver already bound"))?;
        }
        Ok(Some(Self { config, receiver }))
    }

    fn labels(&self) -> &'static [&'static str] {
        match self.config.scenario.as_str() {
            "S01" => &["echo"],
            "S02" => &["a", "b"],
            "S05" => &["a", "b", "c"],
            "S08" | "S09" => &["intent"],
            "S10" => &["rank_one", "rank_two", "rank_three"],
            "S11" => &["winner", "loser", "after"],
            "S12" => &["gate", "source"],
            "S23" => &["winner", "source", "gate", "later"],
            "S32" => &["winner", "source", "gate"],
            "S31" => &["winner", "loser", "gate"],
            _ => &[],
        }
    }

    pub(crate) fn tools(&self) -> Result<Arc<dyn lash::tools::ToolProvider>> {
        let mut plan = BTreeMap::new();
        for label in self.labels() {
            let value = serde_json::json!(match *label {
                "a" => "A",
                "b" => "B",
                "c" => "C",
                value => value,
            });
            let result = if (*label == "loser" && self.config.deferred_loser)
                || matches!(*label, "source" | "later")
            {
                BodyResult::Deferred
            } else if matches!(*label, "intent" | "rank_one" | "rank_three") {
                BodyResult::EmitToReceiver {
                    value,
                    receiver: self.receiver.clone(),
                    event_type: self.config.intent_event_type.clone(),
                }
            } else {
                BodyResult::Inline {
                    value,
                    intents: Default::default(),
                }
            };
            plan.insert((*label).to_owned(), result);
        }
        let client = reqwest::Client::new();
        let url = self.config.body_callback_url.clone();
        let barrier = Arc::new(move |delivery: ToolDelivery| -> bodies::BodyStep {
            let client = client.clone();
            let url = url.clone();
            Box::pin(async move {
                client
                    .post(url)
                    .json(&delivery)
                    .send()
                    .await?
                    .error_for_status()?;
                Ok(())
            })
        });
        ToolBodies::open(&self.config.delivery_ledger, plan, barrier)?.provider()
    }

    pub(crate) fn receiver_binding(&self) -> (Arc<OnceLock<lash::ProcessId>>, PathBuf, String) {
        (
            self.receiver.clone(),
            self.config.delivery_ledger.with_extension("receiver.json"),
            self.config.intent_event_type.clone(),
        )
    }

    /// The receiver-side declaration hold this fixture was configured with:
    /// `None` unless the case asked for it.
    pub(crate) fn receiver_hold(&self) -> Result<Option<ReceiverHold>> {
        if !self.config.receiver_hold {
            return Ok(None);
        }
        let mut url = reqwest::Url::parse(&self.config.body_callback_url)?;
        url.set_path("/DeclarationIssued");
        Ok(Some(ReceiverHold {
            event_type: self.config.intent_event_type.clone(),
            delivery_ledger: self.config.delivery_ledger.clone(),
            url,
            client: reqwest::Client::new(),
        }))
    }

    pub(crate) fn provider(
        &self,
        protocol: provider::FixtureProtocol,
    ) -> Result<lash::provider::ProviderHandle> {
        provider::scripted_provider(
            &self.config.scenario,
            self.labels(),
            protocol,
            &self.config.provider_ledger,
        )
    }
}

/// A declaration hold taken at the receiver's event append rather than on the
/// wire. The append posts the emitting body delivery to the case's
/// body-callback endpoint; the callback publishes the `reached` proof and
/// answers only once the controller releases the barrier, so the response
/// arriving *is* the release.
pub(crate) struct ReceiverHold {
    event_type: String,
    delivery_ledger: PathBuf,
    url: reqwest::Url,
    client: reqwest::Client,
}

impl ReceiverHold {
    pub(crate) async fn before_append(
        &self,
        event_type: &str,
        payload: &serde_json::Value,
    ) -> Result<()> {
        if event_type != self.event_type {
            return Ok(());
        }
        let call_id = payload
            .get("call_id")
            .and_then(|value| value.as_str())
            .ok_or_else(|| anyhow!("held receiver append lacks a call_id payload"))?;
        let delivery = bodies::deliveries(&self.delivery_ledger)?
            .into_iter()
            .filter(|delivery| delivery.call_id.as_str() == call_id)
            .max_by_key(|delivery| delivery.ordinal)
            .ok_or_else(|| anyhow!("held receiver append names an undelivered call"))?;
        self.client
            .post(self.url.clone())
            .json(&delivery)
            .send()
            .await?
            .error_for_status()?;
        Ok(())
    }
}
