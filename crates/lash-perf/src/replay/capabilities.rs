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
    document: String,
    document_revision: u64,
    subscriptions: Vec<(SourceId, WeakEnvironmentSink)>,
    authorized_calls: u64,
}

/// One catalogue shared by the resident runner and its counted snapshot path.
pub struct HostTools {
    definitions: Vec<ToolDefinition>,
    state: Mutex<State>,
}
impl HostTools {
    pub fn new(geometry: Option<&super::script::HostGeometry>) -> Result<Arc<Self>> {
        let roots = geometry.map_or(1, |g| g.package_roots);
        ensure!(
            roots > 0 && roots <= 11,
            "host package_roots must be between 1 and 11"
        );
        let definitions = (0..geometry.map_or(31, |g| g.tools))
            .map(|i| {
                let root = [
                    "tools",
                    "information",
                    "people",
                    "artifacts",
                    "workflows",
                    "integrations",
                    "inbox",
                    "jev",
                    "documents",
                    "onboarding",
                    "confirmation",
                ][i % roots];
                match geometry {
                    Some(geometry) => {
                        host_definition(root, &format!("host{i}"), geometry.schema_fields)
                    }
                    None => definition(&format!("host{i}")),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        Ok(Arc::new(Self {
            definitions,
            state: Mutex::new(State {
                document: "ORIGINAL-DOCUMENT".into(),
                ..State::default()
            }),
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
    /// The host edits its own document between settled turns. Only the
    /// document operation's binding changes; the catalogue geometry is stable.
    pub fn edit_document(&self) {
        let subscriptions = {
            let mut state = self.state.lock_recover();
            state.document = "EDITED-DOCUMENT".into();
            state.document_revision += 1;
            state.subscriptions.clone()
        };
        for (source, sink) in subscriptions {
            if let Some(sink) = sink.upgrade() {
                sink(EnvironmentChange::ConfigSelection {
                    sources: [source].into(),
                });
            }
        }
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
#[derive(
    Clone,
    Debug,
    PartialEq,
    serde::Serialize,
    serde::Deserialize,
    lash::plugins::schemars::JsonSchema,
)]
#[schemars(crate = "lash::plugins::schemars")]
pub struct HostOptions {
    catalogue: Vec<ToolDefinition>,
    for_run: bool,
}
struct HostOwner(HostOptions);
impl lash::plugins::ConfigOwner for HostOwner {
    type Create = HostOptions;
    type Recorded = HostOptions;
    type Refusal = String;
    type RunOptions = HostOptions;
    fn create(&self, input: Option<HostOptions>) -> Result<Option<HostOptions>, String> {
        Ok(Some(input.unwrap_or_else(|| self.0.clone())))
    }
    fn validate(
        &self,
        _: &HostOptions,
        _: Option<&HostOptions>,
        _: &lash::plugins::CandidateFacts<'_>,
    ) -> Result<(), String> {
        Ok(())
    }
    fn apply_run_options(
        &self,
        _: &HostOptions,
        options: HostOptions,
    ) -> Result<HostOptions, String> {
        Ok(options)
    }
}
struct HostFactory {
    inner: Arc<lash::plugins::StaticPluginFactory>,
    initial: HostOptions,
}
impl lash::plugins::PluginMetadata for HostFactory {
    fn plugin_declaration(&self) -> lash::plugins::PluginDeclaration {
        lash::plugins::PluginDeclaration::initial("tools")
    }
}
impl lash::plugins::PluginFactory for HostFactory {
    fn id(&self) -> &'static str {
        "tools"
    }
    fn recipe(self: Arc<Self>) -> Result<lash::plugins::PluginRecipe, lash::plugins::PluginError> {
        let mut config = lash::plugins::ConfigRegistrar::new("tools");
        config.owner_with_run_options(
            HostOwner(self.initial.clone()),
            lash::RunOptionsKey::new("tools"),
        )?;
        Ok(self.inner.clone().recipe()?.with_configuration(config))
    }
}
impl HostTools {
    pub fn factory(
        self: &Arc<Self>,
        tools: Arc<dyn ToolProvider>,
    ) -> Result<Arc<dyn lash::plugins::PluginFactory>> {
        Ok(Arc::new(HostFactory {
            inner: Arc::new(lash::plugins::StaticPluginFactory::new(
                lash::plugins::PluginDeclaration::initial("tools"),
                lash::plugins::PluginSpec::new()
                    .with_tool_provider(lash::tool_provider_key!("tools"), tools.clone())
                    .with_package_provider(
                        lash::tool_provider_key!("tools"),
                        Arc::new(HostPackages {
                            roots: self
                                .definitions
                                .iter()
                                .map(|d| d.manifest.operation.package.as_slice()[0].clone())
                                .collect(),
                            tools,
                        }),
                    ),
            )),
            initial: HostOptions {
                catalogue: self.definitions.clone(),
                for_run: false,
            },
        }))
    }
    pub fn run_spec(
        &self,
        geometry: Option<&super::script::HostGeometry>,
        turn: usize,
    ) -> Result<lash::RunSpec> {
        let mut spec = run_spec_for(geometry);
        if geometry.is_some_and(|g| g.run_override) {
            spec.overrides.owners.set(
                lash::RunOptionsKey::new("tools"),
                HostOptions {
                    catalogue: self.definitions.clone(),
                    for_run: true,
                },
            )?;
        }
        if geometry.is_some() {
            let bindings: Vec<_> = self
                .definitions
                .iter()
                .map(|definition| {
                    serde_json::json!({
                        "operation": definition.manifest.tool(), "turn_id": turn.to_string(),
                    })
                })
                .collect();
            spec.capabilities.insert(
                lash::SlotId::new("documents"),
                lash::CapabilityRef {
                    contract: lash::ContractRef::new("replay.documents", 1),
                    binding: lash::BindingId::new(format!("replay:turn-{turn}")),
                    args: serde_json::json!({"read":true,"bindings":bindings}),
                },
            );
        }
        Ok(spec)
    }
}

/// Figments advertises each root independently, without sharing a tool-source
/// snapshot. Keep that provider geometry in this host replay.
struct HostPackages {
    roots: std::collections::BTreeSet<lash::package::Segment>,
    tools: Arc<dyn ToolProvider>,
}
#[async_trait::async_trait]
impl lash::package::PackageProvider for HostPackages {
    fn roots(&self, _: &OwnerConfigView) -> std::collections::BTreeSet<lash::package::Segment> {
        self.roots.clone()
    }
    fn advertise(
        &self,
        config: &OwnerConfigView,
        context: &lash::package::PublicationContext,
    ) -> Result<Vec<lash::package::PackageContribution>, lash::plugins::PluginError> {
        lash::package::ToolPackageProvider::new(
            "tools",
            lash::tool_provider_key!("tools"),
            self.roots.clone(),
            self.tools.clone(),
        )
        .advertise(config, context)
    }
    async fn hold(
        &self,
        _: &lash::persistence::ReferrerClaim,
        _: &[&lash::package::PackageAdvertisement],
    ) -> Result<(), lash::package::EnginePortError> {
        Ok(())
    }
}

fn host_definition(root: &str, name: &str, fields: usize) -> Result<ToolDefinition> {
    let properties: serde_json::Map<String, serde_json::Value> = (0..fields)
        .map(|i| (format!("field{i}"), serde_json::json!({
            "type":"string", "description":"Optional host record field used to select the document, source, person, or workflow to inspect."
        }))).collect();
    Ok(ToolDefinition::raw(
        format!("replay-{name}"), format!("{root}.{name}"), "Host capability guarded document operation",
        serde_json::json!({"title":name,"type":"object","properties":properties,"additionalProperties":false}),
        output_schema(name),
    )?.with_execution(std::time::Duration::from_secs(1)))
}

fn definition(name: &str) -> Result<ToolDefinition> {
    Ok(ToolDefinition::raw(
        format!("replay-{name}"),
        format!("tools.{name}"),
        "Host capability guarded document operation",
        serde_json::json!({"type":"object","properties":{},"additionalProperties":false}),
        output_schema(name),
    )?
    .with_execution(std::time::Duration::from_secs(1)))
}
fn output_schema(name: &str) -> serde_json::Value {
    let mut schema = serde_json::json!({"type":"object","properties":{"authorized":{"type":"boolean"}},"required":["authorized"],"additionalProperties":false});
    if name == "host0" {
        schema["properties"]["document"] = serde_json::json!({"type":"string"});
    }
    schema
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
        let mut payload = serde_json::json!({"operation":manifest.tool()});
        if manifest.tool() == "tools.host0" {
            payload["document_revision"] = self.state.lock_recover().document_revision.into();
        }
        Some((FormatVersion::ONE, payload))
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let capabilities = call.context.run_capabilities();
        let host_authorized = capabilities
            .get(&lash::SlotId::new("documents"))
            .is_some_and(|capability| {
                capability.contract == lash::ContractRef::new("replay.documents", 1)
                    && capability.args["read"] == true
                    && capability.args["bindings"]
                        .as_array()
                        .is_some_and(|bindings| {
                            bindings.iter().any(|binding| {
                                binding["operation"].as_str() == Some(call.name().as_str())
                                    && binding["turn_id"].as_str().is_some_and(|turn| {
                                        capability.binding.as_str() == format!("replay:turn-{turn}")
                                    })
                            })
                        })
            });
        if capabilities != &run_spec().capabilities && !host_authorized {
            return lash_core::ToolOutcome::err("missing host document capability").into();
        }
        let output = {
            let mut state = self.state.lock_recover();
            state.authorized_calls += 1;
            let mut output = serde_json::json!({"authorized":true});
            if call.name().as_str() == "tools.host0" {
                output["document"] = state.document.clone().into();
            }
            output
        };
        lash_core::ToolOutcome::ok(output).into()
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
    run_spec_for(None)
}
pub fn run_spec_for(geometry: Option<&super::script::HostGeometry>) -> lash::RunSpec {
    let mut spec = if geometry.is_some_and(|g| g.run_override) {
        lash::RunSpec::overrides(lash::RunOverrides {
            generation: Some(lash::direct::GenerationOptions {
                output_token_cap: std::num::NonZeroUsize::new(4096),
                ..Default::default()
            }),
            ..Default::default()
        })
    } else {
        lash::RunSpec::default()
    };
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
            CapabilityTurn::FreshDocument | CapabilityTurn::EditedDocument => {
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
