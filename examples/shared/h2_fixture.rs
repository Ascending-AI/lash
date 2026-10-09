//! H2 scripted provider and real tool bodies, composed by the product host.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use anyhow::{Context, Result, anyhow, ensure};
use serde::Deserialize;

#[path = "h2_tool_bodies.rs"]
mod bodies;
use bodies::{BodyOutcome, ToolBodies, ToolDelivery};
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
    intent_process: Option<lash::ProcessId>,
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
                "S01" | "S02" | "S05" | "S11" | "S12" | "S18" | "S23" | "S31" | "S32"
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
            "S11" => &["winner", "loser", "after"],
            "S12" => &["gate", "source"],
            "S18" => &["handle"],
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
            let result = if *label == "handle" {
                BodyOutcome::Handle {
                    process: self.receiver.clone(),
                }
            } else if (*label == "loser" && self.config.deferred_loser)
                || matches!(*label, "source" | "later")
            {
                BodyOutcome::Deferred
            } else {
                BodyOutcome::Inline {
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

    pub(crate) fn receiver_binding(&self) -> (Arc<OnceLock<lash::ProcessId>>, PathBuf) {
        (
            self.receiver.clone(),
            self.config.delivery_ledger.with_extension("receiver.json"),
        )
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
