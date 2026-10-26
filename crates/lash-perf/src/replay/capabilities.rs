//! Host-shaped document catalogue: authority is supplied on each Run, while
//! definitions change only when the host attaches a new document.
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use anyhow::{Result, ensure};
use lash::plugins::FormatVersion;
use lash::tools::{OwnerConfigView, ToolContract, ToolDefinition, ToolManifest, ToolProvider};
use lash_core::plugin::environment_sources::EnvironmentChangeSink;
use lash_core_store::environment::{EnvironmentChange, SourceId, SourceMode};
use lash_protocol_types::sync::MutexExt;

use super::script::CapabilityTurn;

type WeakEnvironmentSink = std::sync::Weak<dyn Fn(EnvironmentChange) + Send + Sync>;

#[derive(Default)]
struct State {
    documents: usize,
    subscriptions: Vec<(SourceId, WeakEnvironmentSink)>,
    authorized_calls: u64,
}

/// One catalogue shared by the resident runner and its counted snapshot path.
pub struct HostTools {
    definitions: Vec<ToolDefinition>,
    state: Mutex<State>,
}
impl HostTools {
    pub fn new() -> Result<Arc<Self>> {
        let definitions = (0..31)
            .map(|i| definition(&format!("host{i}")))
            .collect::<Result<Vec<_>>>()?;
        Ok(Arc::new(Self {
            definitions,
            state: Mutex::new(State::default()),
        }))
    }
    /// A real host document publication changes the catalogue and notifies the
    /// actor. The media upload and this notification are inside the turn cut.
    pub fn attach_document(&self) {
        let mut state = self.state.lock_recover();
        state.documents += 1;
        for (source, sink) in &state.subscriptions {
            if let Some(sink) = sink.upgrade() {
                sink(EnvironmentChange::ConfigSelection {
                    sources: [source.clone()].into(),
                });
            }
        }
    }
    pub fn authorized_calls(&self) -> u64 {
        self.state.lock_recover().authorized_calls
    }
    pub fn counted(
        self: &Arc<Self>,
        counts: &lash_core::testing::CallbackCounts,
    ) -> Arc<dyn ToolProvider> {
        use lash::plugins::PluginCallbackIdentity;
        // Count the default snapshot entry independently from the real calls
        // it makes into the host runner, as the existing ENV-UNCHANGED law does.
        let runner = counts.tool_provider(
            PluginCallbackIdentity::tool_provider("tools", "host"),
            self.clone(),
        );
        counts.tool_provider(
            PluginCallbackIdentity::tool_provider("tools", "snapshot"),
            Arc::new(Snapshot(runner)),
        )
    }
}
fn definition(name: &str) -> Result<ToolDefinition> {
    Ok(ToolDefinition::raw(
        format!("replay-{name}"), format!("tools.{name}"), "Host capability guarded document operation",
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        serde_json::json!({"type":"object","properties":{"authorized":{"type":"boolean"}},"required":["authorized"],"additionalProperties":false}),
    )?.with_execution(std::time::Duration::from_secs(1)))
}
#[async_trait::async_trait]
impl ToolProvider for HostTools {
    fn environment_mode(&self) -> SourceMode {
        SourceMode::Notifying
    }
    fn subscribe_environment(&self, source: &SourceId, sink: &EnvironmentChangeSink) {
        self.state
            .lock_recover()
            .subscriptions
            .push((source.clone(), Arc::downgrade(sink)));
    }
    fn tool_manifests(
        &self,
        _: &OwnerConfigView,
    ) -> Result<Vec<ToolManifest>, lash::plugins::PluginError> {
        let mut manifests: Vec<_> = self
            .definitions
            .iter()
            .map(|d| d.manifest.clone())
            .collect();
        for i in 0..self.state.lock_recover().documents {
            let document = definition(&format!("document{i}"))
                .map_err(|error| lash::plugins::PluginError::Session(error.to_string()))?;
            manifests.push(document.manifest);
        }
        Ok(manifests)
    }
    fn resolve_contract(&self, name: &str, _: &OwnerConfigView) -> Option<Arc<ToolContract>> {
        self.definitions
            .iter()
            .find(|d| d.manifest.tool() == name)
            .map(|d| Arc::new(d.contract.clone()))
            .or_else(|| {
                name.strip_prefix("tools.document")
                    .and_then(|i| i.parse::<usize>().ok())
                    .filter(|i| *i < self.state.lock_recover().documents)
                    .and_then(|i| definition(&format!("document{i}")).ok())
                    .map(|d| Arc::new(d.contract))
            })
    }
    fn readable_binding_formats(&self) -> &'static [FormatVersion] {
        &[FormatVersion::ONE]
    }
    fn link_payload(
        &self,
        manifest: &ToolManifest,
        _: &OwnerConfigView,
    ) -> Option<(FormatVersion, serde_json::Value)> {
        Some((
            FormatVersion::ONE,
            serde_json::json!({"operation":manifest.tool()}),
        ))
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        if call.context.run_capabilities() != &run_spec().capabilities {
            return lash_core::ToolOutcome::err("missing host document capability").into();
        }
        self.state.lock_recover().authorized_calls += 1;
        lash_core::ToolOutcome::ok(serde_json::json!({"authorized":true})).into()
    }
}
/// Retain the default snapshot, so its virtual calls reach the counted runner.
struct Snapshot(Arc<dyn ToolProvider>);
#[async_trait::async_trait]
impl ToolProvider for Snapshot {
    fn environment_mode(&self) -> SourceMode {
        self.0.environment_mode()
    }
    fn subscribe_environment(&self, source: &SourceId, sink: &EnvironmentChangeSink) {
        self.0.subscribe_environment(source, sink);
    }
    fn tool_manifests(
        &self,
        config: &OwnerConfigView,
    ) -> Result<Vec<ToolManifest>, lash::plugins::PluginError> {
        self.0.tool_manifests(config)
    }
    fn resolve_contract(&self, name: &str, config: &OwnerConfigView) -> Option<Arc<ToolContract>> {
        self.0.resolve_contract(name, config)
    }
    fn readable_binding_formats(&self) -> &'static [FormatVersion] {
        self.0.readable_binding_formats()
    }
    fn link_payload(
        &self,
        manifest: &ToolManifest,
        config: &OwnerConfigView,
    ) -> Option<(FormatVersion, serde_json::Value)> {
        self.0.link_payload(manifest, config)
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.0.execute(call).await
    }
}
pub fn run_spec() -> lash::RunSpec {
    let mut spec = lash::RunSpec::default();
    spec.capabilities.insert(
        lash::SlotId::new("documents"),
        lash::CapabilityRef {
            contract: lash::ContractRef::new("replay.documents", 1),
            binding: lash::BindingId::new("replay:workspace"),
            args: serde_json::json!({"read":true}),
        },
    );
    spec
}
/// Compare entries at the same settled cuts as the actual SQL/store counters.
pub fn metrics(
    before: &BTreeMap<(lash::plugins::PluginCallbackIdentity, String), u64>,
    counts: &lash_core::testing::CallbackCounts,
) -> BTreeMap<String, f64> {
    let after = counts.provider_snapshot();
    [
        ("environment_snapshot", "tool_snapshot_callbacks"),
        ("tool_manifests", "tool_manifest_callbacks"),
        ("link_payload", "link_payload_callbacks"),
    ]
    .into_iter()
    .map(|(method, metric)| {
        let entries = after
            .iter()
            .filter(|((_, name), _)| name == method)
            .map(|(key, n)| n - before.get(key).copied().unwrap_or(0))
            .sum::<u64>();
        (metric.to_owned(), entries as f64)
    })
    .collect()
}
pub fn check(turn: CapabilityTurn, metrics: &BTreeMap<String, f64>, authorized: u64) -> Result<()> {
    let expected_calls = u64::from(turn != CapabilityTurn::Quiet);
    ensure!(
        authorized == expected_calls,
        "capability turn must execute {expected_calls} authorized host operations, got {authorized}"
    );
    ensure!(
        metrics
            .get("sql_writes")
            .is_some_and(|writes| *writes > 0.0),
        "capability turn recorded no actual SQL writes"
    );
    for name in [
        "tool_snapshot_callbacks",
        "tool_manifest_callbacks",
        "link_payload_callbacks",
    ] {
        match turn {
            CapabilityTurn::Quiet => ensure!(
                metrics[name] == 0.0,
                "quiet warm turn entered {name}: {}",
                metrics[name]
            ),
            CapabilityTurn::FreshDocument => {
                ensure!(metrics[name] > 0.0, "fresh document entered no {name}")
            }
            CapabilityTurn::Warm => {}
        }
    }
    Ok(())
}
/// The synthetic provider accepts raw document bytes, as declared by session policy.
pub fn acceptance() -> Arc<lash_protocol_types::llm::capability::AttachmentCapabilitySnapshot> {
    use lash_protocol_types::llm::attachment_delivery::{AttachmentPosition, DeliveryForms};
    use lash_protocol_types::llm::capability::{
        AttachmentAcceptanceRule, AttachmentAcceptor, AttachmentCapabilitySnapshot,
    };
    Arc::new(AttachmentCapabilitySnapshot {
        revision: "replay-documents".into(),
        acceptors: vec![AttachmentAcceptor {
            provider: "lash-perf-replay".into(),
            rules: vec![AttachmentAcceptanceRule {
                positions: vec![AttachmentPosition::Message],
                media_types: vec!["text/plain".into()],
                media_families: vec![],
                forms: DeliveryForms {
                    bytes: true,
                    url: false,
                    provider_file: false,
                },
            }],
        }],
    })
}
