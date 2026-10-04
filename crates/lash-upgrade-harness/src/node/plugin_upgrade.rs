//! H5's live plugin fixture. The host submits inputs; Restate owns every turn.
//! The append reducer is deliberately noncommutative and changes with revision.
//! The JSONL ledger counts actual code entry, independently of the journal.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use clap::{Args, ValueEnum};
use lash_core::plugin::{PluginDeclaration, PluginFactory, PluginSessionContext, SessionPlugin};
use lash_core::{PluginError, ToolDefinitionBindingExt as _};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::{RestateArgs, StoreArgs};
use crate::identity::BuildLabel;

pub const PLUGIN: &str = "e2e-upgrade-state";
pub const OTHER: &str = "e2e-upgrade-other";

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Action {
    Serve,
    Send,
    Follow,
    Cancel,
    Read,
}

#[derive(Clone, Debug, Args)]
pub struct PluginUpgradeArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    #[command(flatten)]
    pub restate: RestateArgs,
    #[arg(long, value_enum)]
    pub action: Action,
    /// All incarnations append to the same ledger and use the same controls.
    #[arg(long)]
    pub controls: PathBuf,
    #[arg(long, default_value = "plugin-upgrade")]
    pub session: String,
    /// `same`, `disjoint`, `namespace` or `single`.
    #[arg(long, default_value = "single")]
    pub variant: String,
    #[arg(long)]
    pub input: Option<lash_core::InputId>,
    #[arg(long, default_value = "127.0.0.1:0")]
    pub bind: std::net::SocketAddr,
    #[arg(long)]
    pub ready_file: Option<PathBuf>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub build: BuildLabel,
    pub plugin: String,
    pub phase: String,
    pub detail: Value,
    pub converter_calls: usize,
}

#[derive(Clone)]
struct Probe {
    plugin: &'static str,
    controls: PathBuf,
    converters: Arc<AtomicUsize>,
}

impl Probe {
    fn record(&self, phase: &str, detail: Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(&Entry {
            build: BuildLabel::current(),
            plugin: self.plugin.into(),
            phase: phase.into(),
            detail,
            converter_calls: self.converters.load(Ordering::SeqCst),
        })?;
        bytes.push(b'\n');
        let mut log = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.controls.join("entries.jsonl"))?;
        log.write_all(&bytes)?;
        log.sync_data()?;
        Ok(())
    }

    async fn hold(&self, name: &str, detail: &Value) -> Result<()> {
        super::write_atomically(
            &self.controls.join(format!("{name}.entered")),
            &serde_json::to_vec(detail)?,
        )?;
        let release = self.controls.join(format!("{name}.release"));
        tokio::time::timeout(Duration::from_secs(120), async {
            while !release.try_exists()? {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("tool body release was never observed")??;
        Ok(())
    }
}

impl PluginFactory for Probe {
    fn id(&self) -> &'static str {
        self.plugin
    }

    fn declaration(&self) -> PluginDeclaration {
        let mut declaration = PluginDeclaration::initial(self.plugin);
        if cfg!(feature = "synthetic-next") {
            declaration.format_version =
                lash_core::FormatVersion::new(2).unwrap_or(lash_core::FormatVersion::ONE);
            declaration.writable_formats =
                vec![lash_core::FormatVersion::ONE, declaration.format_version];
            declaration.behavior_revision = lash_core::plugin::BehaviorRevision::new(2)
                .unwrap_or(lash_core::plugin::BehaviorRevision::ONE);
        }
        declaration
    }

    fn register_config(
        &self,
        registrar: &mut lash_core::ConfigRegistrar,
    ) -> Result<(), lash_core::ConfigRegistrationError> {
        registrar.owner(ConfigOwner)
    }

    fn migrate_format(
        &self,
        from: lash_core::FormatVersion,
        namespace: lash_core::FormatNamespace,
        value: Value,
    ) -> Result<Value, lash_core::FormatRefusal> {
        let native = self.declaration().format_version;
        if from == native {
            return Ok(value);
        }
        if from == lash_core::FormatVersion::ONE {
            self.converters.fetch_add(1, Ordering::SeqCst);
            return Ok(value);
        }
        Err(lash_core::FormatRefusal {
            plugin: self.plugin.into(),
            namespace,
            stored: from,
            readable: native,
        })
    }

    fn encode_format(
        &self,
        to: lash_core::FormatVersion,
        namespace: lash_core::FormatNamespace,
        value: &Value,
    ) -> Result<Value, lash_core::FormatRefusal> {
        let native = self.declaration().format_version;
        if to == native || to == lash_core::FormatVersion::ONE {
            return Ok(value.clone());
        }
        Err(lash_core::FormatRefusal {
            plugin: self.plugin.into(),
            namespace,
            stored: to,
            readable: native,
        })
    }

    fn build(&self, _: &PluginSessionContext) -> Result<Arc<dyn SessionPlugin>, PluginError> {
        Ok(Arc::new(self.clone()))
    }
}

#[derive(
    Clone, Debug, PartialEq, Eq, Serialize, Deserialize, lash_core::facade_support::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
struct RecordedConfig {
    label: String,
}

struct ConfigOwner;

impl lash_core::ConfigOwner for ConfigOwner {
    type Create = RecordedConfig;
    type Recorded = RecordedConfig;
    type Refusal = String;
    type RunOptions = lash_core::NoRunOptions;
    fn create(
        &self,
        input: Option<RecordedConfig>,
        _: lash_core::CreationFacts<'_, RecordedConfig>,
    ) -> Result<Option<RecordedConfig>, String> {
        Ok(Some(input.unwrap_or(RecordedConfig {
            label: BuildLabel::current().to_string(),
        })))
    }
    fn validate(
        &self,
        _: &RecordedConfig,
        _: Option<&RecordedConfig>,
        _: &lash_core::CandidateFacts<'_>,
    ) -> Result<(), String> {
        Ok(())
    }
    fn apply_run_options(
        &self,
        config: &RecordedConfig,
        _: lash_core::NoRunOptions,
    ) -> Result<RecordedConfig, String> {
        Ok(config.clone())
    }
}

impl SessionPlugin for Probe {
    fn id(&self) -> &'static str {
        self.plugin
    }

    fn register(
        &self,
        registrar: &mut lash_core::plugin::PluginRegistrar,
    ) -> Result<(), PluginError> {
        registrar.tools().provider(Arc::new(self.clone()))?;
        let probe = self.clone();
        registrar.state_reducer(
            "append",
            Arc::new(move |reduction: lash_core::plugin::StateReduction<'_>| {
                probe
                    .record("reducer", reduction.input.clone())
                    .map_err(|error| lash_core::tool_run::HookCause {
                        error_type: "fixture_io".into(),
                        error_version: std::num::NonZeroU32::MIN,
                        payload: json!(error.to_string()),
                    })?;
                let current = reduction.current.and_then(Value::as_str).unwrap_or("");
                let addition = reduction.input.as_str().unwrap_or("");
                let value = if cfg!(feature = "synthetic-next") {
                    format!("{addition}{current}")
                } else {
                    format!("{current}{addition}")
                };
                Ok(Some(json!(value)))
            }),
        )?;
        if self.plugin == OTHER {
            return Ok(());
        }
        let probe = self.clone();
        registrar.turn().after(
            lash_core::hook_key!("completed"),
            Arc::new(move |_| {
                let probe = probe.clone();
                Box::pin(async move {
                    probe
                        .record("hook", Value::Null)
                        .map_err(|error| PluginError::Session(error.to_string()))?;
                    Ok(lash_core::plugin::AfterTurnContributions {
                        state: lash_core::plugin::StateCommands::new().apply(
                            "hooks",
                            "append",
                            json!(if cfg!(feature = "synthetic-next") {
                                "J"
                            } else {
                                "H"
                            }),
                        ),
                        ..Default::default()
                    })
                })
            }),
        )?;
        Ok(())
    }
}

fn definition(plugin: &str) -> Result<lash_core::ToolDefinition> {
    let name = if plugin == PLUGIN {
        "upgrade_append"
    } else {
        "upgrade_other"
    };
    Ok(lash_core::ToolDefinition::raw(
        format!("{plugin}:append"), name, "Append one recorded symbol",
        json!({"type":"object", "properties":{"key":{"type":"string"},"symbol":{"type":"string"},"hold":{"type":"boolean"}}, "required":["key","symbol","hold"], "additionalProperties":false}),
        json!({"type":"string"}),
    )?.with_tool_binding(lash_core::ToolBinding::new([plugin], "append")))
}

#[lash_core::async_trait]
impl lash_core::ToolProvider for Probe {
    fn tool_manifests(&self) -> Vec<lash_core::ToolManifest> {
        definition(self.plugin)
            .map(|tool| vec![tool.manifest()])
            .unwrap_or_default()
    }

    fn resolve_contract(&self, _: &str) -> Option<Arc<lash_core::ToolContract>> {
        definition(self.plugin)
            .ok()
            .map(|tool| Arc::new(tool.contract()))
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        let result = async {
            let symbol = call.args["symbol"].as_str().context("symbol")?;
            let key = call.args["key"].as_str().context("key")?;
            let detail = json!({"symbol":symbol, "key":key, "call_id":call.context.call_id(), "attempt":call.context.attempt_number(), "run":call.context.logical_run().context("tool has no logical Run")?});
            self.record("body", detail.clone())?;
            if call.args["hold"].as_bool() == Some(true) {
                self.hold(symbol, &detail).await?;
            }
            Ok::<_, anyhow::Error>(lash_core::ToolOutcomeDone::ok(json!(symbol)).with_state(
                lash_core::plugin::StateCommands::new().apply(key, "append", json!(symbol)),
            ))
        }.await;
        match result {
            Ok(result) => lash_core::ToolAttemptOutcome::done_without_intents(result),
            Err(error) => lash_core::ToolAttemptOutcome::host_failed(
                lash_core::RuntimeEffectControllerError::new(
                    lash_core::RuntimeErrorCode::Plugin,
                    error.to_string(),
                ),
            ),
        }
    }
}

fn calls(variant: &str) -> Vec<lash_core::LlmOutputPart> {
    let mut calls = vec![(
        if variant == "single" {
            if cfg!(feature = "synthetic-next") {
                "S"
            } else {
                "N"
            }
        } else {
            "B"
        },
        if variant == "disjoint" { "b" } else { "value" },
        false,
        variant == "namespace",
    )];
    if variant != "single" {
        calls.insert(
            0,
            (
                "A",
                if variant == "disjoint" { "a" } else { "value" },
                true,
                false,
            ),
        );
    }
    calls
        .into_iter()
        .map(
            |(symbol, key, hold, other)| lash_core::LlmOutputPart::ToolCall {
                call_id: format!("provider-{symbol}"),
                tool_name: if other {
                    "upgrade_other"
                } else {
                    "upgrade_append"
                }
                .into(),
                input_json: json!({"key":key,"symbol":symbol,"hold":hold}).to_string(),
                replay: None,
            },
        )
        .collect()
}

fn core(
    engine: Arc<lash::restate::RestateEngine>,
    controls: &std::path::Path,
) -> Result<lash::LashCore> {
    let probe = Probe {
        plugin: PLUGIN,
        controls: controls.to_path_buf(),
        converters: Arc::default(),
    };
    let observed = probe.clone();
    let provider = lash_core::testing::TestProvider::builder()
        .kind("upgrade-harness")
        .serialize_config(|| json!({"fixture":"h5-plugin-upgrade"}))
        .complete(move |request| {
            let probe = observed.clone();
            async move {
                probe.record("provider", Value::Null).map_err(|error| {
                    lash_core::facade_support::LlmTransportError::new(error.to_string())
                })?;
                let answered = request
                    .messages
                    .iter()
                    .rev()
                    .take_while(|message| {
                        message.role != lash_core::llm::types::LlmRole::User
                            || !message.starts_user_segment
                    })
                    .any(|message| {
                        message.blocks.iter().any(|block| {
                            matches!(
                                block,
                                lash_core::llm::types::LlmContentBlock::ToolResult { .. }
                            )
                        })
                    });
                let message = super::provider::newest_message(&request);
                let variant = ["single", "same", "disjoint", "namespace"]
                    .into_iter()
                    .find(|variant| message.split_whitespace().any(|word| word == *variant))
                    .unwrap_or("single");
                Ok(lash_core::LlmResponse {
                    parts: if answered {
                        vec![lash_core::LlmOutputPart::Text {
                            text: "recorded plugin state".into(),
                            response_meta: None,
                        }]
                    } else {
                        calls(variant)
                    },
                    terminal_reason: if answered {
                        lash_core::LlmTerminalReason::Stop
                    } else {
                        lash_core::LlmTerminalReason::ToolUse
                    },
                    ..Default::default()
                })
            }
        })
        .build();
    let profiles = lash::LlmProfileRegistry::new().register(
        super::PROFILE_KEY,
        lash::RegisteredLlmProfile::new(super::model()?, provider.into_handle()),
    )?;
    lash::LashCore::standard_builder(lash::Backend::new(engine))
        .llm_profiles(Arc::new(profiles))
        .plugin(Arc::new(probe))
        .plugin(Arc::new(Probe {
            plugin: OTHER,
            controls: controls.to_path_buf(),
            converters: Arc::default(),
        }))
        .recovery_lease(super::recovery_lease())
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "h5",
            format!("{}-{}", BuildLabel::current(), std::process::id()),
        ))
        .map_err(anyhow::Error::from)
}

pub async fn run(args: PluginUpgradeArgs) -> Result<()> {
    std::fs::create_dir_all(&args.controls)?;
    ensure!(
        ["single", "same", "disjoint", "namespace"].contains(&args.variant.as_str()),
        "unknown state variant"
    );
    let stores = super::open_stores(&args.store).await?;
    let session_id = lash::SessionId::parse(&args.session)?;
    if matches!(args.action, Action::Read) {
        let store = stores.session_store_factory();
        let read = store
            .load_session_window(&session_id, lash_core::store::WindowSelector::Current)
            .await?
            .context("stored plugin window")?;
        let state = lash_core::store::window_state(read, store.fleet_format())?.state;
        return super::print(
            &json!({"build":BuildLabel::current(), "head":state.head_revision, "config":state.authority.plugin_config, "state":state.plugin_state()}),
        );
    }
    let engine = super::engine(stores, &args.restate)?;
    let core = core(engine.clone(), &args.controls)?;
    if matches!(args.action, Action::Serve) {
        let worker = lash::durability::DurableProcessWorker::new(
            core.durable_process_worker_config()
                .context("worker config")?,
        )?;
        let endpoint = engine.endpoint_builder(worker)?.build();
        let listener = tokio::net::TcpListener::bind(args.bind).await?;
        let uri = format!("http://{}", listener.local_addr()?);
        let serving = tokio::spawn(async move {
            lash::restate::serve_endpoint(
                listener,
                endpoint,
                lash::restate::RestateEndpointLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024 + 8),
                std::future::pending::<()>(),
            )
            .await;
        });
        engine.register_deployment(&uri).await?;
        super::write_atomically(
            &args.ready_file.context("serve requires --ready-file")?,
            &serde_json::to_vec(&super::ServeReady {
                build: BuildLabel::current(),
                generation: core.build_generation().to_string(),
                uri,
            })?,
        )?;
        tokio::select! {
            result = serving => { result.context("plugin endpoint panicked")?; anyhow::bail!("plugin endpoint stopped unexpectedly"); }
            result = super::shutdown_signal() => { result?; }
        }
        return Ok(());
    }
    if matches!(args.action, Action::Send) {
        match core
            .session(session_id.clone())
            .create(lash::SessionCreation::root(super::session_spec()))
            .await
        {
            Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
            Err(error) => return Err(error.into()),
        }
    }
    let session = core.session(session_id).durable().await?;
    match args.action {
        Action::Send => {
            let handle = session
                .send(lash::TurnInput::text(args.variant))
                .into_future()
                .await?;
            super::print(handle.receipt())
        }
        Action::Follow => {
            let outcome = tokio::time::timeout(
                Duration::from_secs(120),
                session
                    .attach(args.input.context("follow requires --input")?)
                    .outcome(),
            )
            .await??;
            super::print(&outcome)
        }
        Action::Cancel => {
            let outcome = session
                .attach(args.input.context("cancel requires --input")?)
                .cancel()
                .into_future()
                .await?;
            super::print(&json!({"cancel":format!("{outcome:?}")}))
        }
        Action::Read | Action::Serve => unreachable!(),
    }
}
