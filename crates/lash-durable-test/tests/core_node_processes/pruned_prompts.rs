//! A pruned process takes its prompt snapshots and their texts with it
//! (FIG-7701).
//!
//! A real workflow process (two tool calls and one model query) runs to its
//! end. Its model calls' prompt snapshots read until the process is pruned;
//! the prune deletes them, with their text edges, in the same transaction as
//! the rest of its history.

use super::*;

use lash_durable::domain::{ModelCallId, PromptCallKey};
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::ProcessId;

const PRINCIPAL: lash::process::ProcessConfigKey<serde_json::Value> =
    lash::process::ProcessConfigKey::new("pruned-principal", lash::plugins::FormatVersion::ONE);
const TOOLS: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("pruned-tools", lash::plugins::FormatVersion::ONE);

struct Principal;

impl lash::process::ProcessComponentFactory for Principal {
    fn declaration(&self) -> lash::process::ProcessComponentDeclaration {
        lash::process::ProcessComponentDeclaration::initial(
            PRINCIPAL.component(),
            PRINCIPAL.format(),
        )
        .unwrap()
    }
    fn config_type(&self) -> std::any::TypeId {
        std::any::TypeId::of::<serde_json::Value>()
    }
    fn validate(
        &self,
        _: &lash::process::ProcessComponentEntry,
        _: &lash::process::ProcessEnv,
    ) -> Result<(), lash_core::ProcessEnvRefusal> {
        Ok(())
    }
    fn recipe(
        self: Arc<Self>,
    ) -> Result<lash_core::plugin::ProcessRecipe, lash_execution_contracts::PluginError> {
        lash::process::ProcessComponentSpec::new().recipe(PRINCIPAL.component())
    }
}

/// A source read answering a short text, distinct per call.
struct Source(AtomicUsize);

#[lash::async_trait]
impl lash::tools::StaticToolExecute for Source {
    async fn execute(&self, _: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        let n = self.0.fetch_add(1, Ordering::SeqCst);
        lash_core::ToolAttemptOutcome::done_without_intents(
            lash_core::ToolOutcomeDone::from_output(lash_core::ToolCallOutput::success(
                serde_json::json!({ "source": n, "text": format!("record {n:04}") }),
            )),
        )
    }
}

fn descriptions() -> Vec<lash_vocabulary::ToolDefinition> {
    (0..2)
        .map(|i| {
            lash_vocabulary::ToolDefinition::raw(
                format!("pruned:{i}"),
                format!("pruned{i:02}.read"),
                "Reads a source.",
                serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
                serde_json::json!({ "type": "object" }),
            )
            .unwrap()
            .with_execution(Duration::from_secs(30))
        })
        .collect()
}

/// The model session a process's model calls are recorded under.
fn model_session(process: &ProcessId) -> SessionId {
    SessionId::prefixed("process:", process.as_str())
}

/// The database beside the store, read directly.
enum Raw {
    Sqlite(std::path::PathBuf),
    Postgres(String),
}

impl Raw {
    async fn pg(url: &str) -> sqlx::PgConnection {
        use sqlx::Connection as _;
        sqlx::PgConnection::connect(url).await.unwrap()
    }

    fn sqlite(path: &std::path::Path) -> rusqlite::Connection {
        rusqlite::Connection::open(path).unwrap()
    }

    /// The process's prompt snapshot rows and the text edges they hold.
    async fn prompt_rows(&self, process: &ProcessId) -> i64 {
        let session = model_session(process).as_str().to_owned();
        let sql = |prefix: &str, session_key: &str| {
            format!(
                "SELECT (SELECT count(*) FROM {prefix}prompt_snapshots WHERE session_id = {session_key}) \
                 + (SELECT count(*) FROM {prefix}prompt_snapshot_texts WHERE session_id = {session_key})"
            )
        };
        match self {
            Self::Sqlite(path) => Self::sqlite(path)
                .query_row(&sql("", "?1"), [&session], |row| row.get(0))
                .unwrap(),
            Self::Postgres(url) => sqlx::query_scalar(&sql("lash_", "$1"))
                .bind(&session)
                .fetch_one(&mut Self::pg(url).await)
                .await
                .unwrap(),
        }
    }

    /// Whether the process's end has settled into what a prune takes: no
    /// consumer hold and its terminal cascade complete (the prune's own
    /// predicate on those columns).
    async fn prunable(&self, process: &ProcessId) -> bool {
        let sql = |prefix: &str, id: &str| {
            format!(
                "SELECT consumer_hold_key IS NULL AND cascade_cursor IS NULL \
                 FROM {prefix}processes WHERE process_id = {id}"
            )
        };
        match self {
            Self::Sqlite(path) => Self::sqlite(path)
                .query_row(&sql("", "?1"), [process.as_str()], |row| row.get(0))
                .unwrap(),
            Self::Postgres(url) => sqlx::query_scalar(&sql("lash_", "$1"))
                .bind(process.as_str())
                .fetch_one(&mut Self::pg(url).await)
                .await
                .unwrap(),
        }
    }

    /// The prompt keys of the process's model calls.
    async fn prompt_keys(&self, process: &ProcessId) -> Vec<PromptCallKey> {
        let session = model_session(process);
        let keys: Vec<(String, String)> = match self {
            Self::Sqlite(path) => Self::sqlite(path)
                .prepare(
                    "SELECT run, call FROM model_call_records WHERE session_id = ?1 ORDER BY run, call",
                )
                .unwrap()
                .query_map([session.as_str()], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap(),
            Self::Postgres(url) => sqlx::query_as(
                "SELECT run, call FROM lash_model_call_records WHERE session_id = $1 ORDER BY run, call",
            )
            .bind(session.as_str())
            .fetch_all(&mut Self::pg(url).await)
            .await
            .unwrap(),
        };
        keys.into_iter()
            .map(|(run, call)| PromptCallKey {
                session: session.clone(),
                call: ModelCallId::Owned {
                    owner: run
                        .strip_prefix("owned:")
                        .expect("a process's model call is owned")
                        .to_owned(),
                    key: call,
                },
            })
            .collect()
    }
}

/// A store set of `tier` on a database the law can also open directly, and
/// what must outlive it.
async fn raw_stores(tier: Tier) -> (Arc<dyn StoreSet>, Raw, Vec<Box<dyn std::any::Any + Send>>) {
    match tier {
        Tier::SqliteMemory => {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("pruned.db");
            let stores = lash_sqlite_store::SqliteStoreSet::open(
                &path,
                lash_sqlite_store::SqliteSynchronous::Normal,
            )
            .await
            .unwrap();
            (Arc::new(stores), Raw::Sqlite(path), vec![Box::new(dir)])
        }
        Tier::Postgres => {
            let url = postgres_url().expect("a PostgreSQL URL");
            let isolated = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::testing::connect(isolated.url())
                .await
                .expect("the isolated database opens");
            let stores = lash_postgres_store::PostgresStoreSet::new(
                &storage,
                Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
            );
            let raw = Raw::Postgres(isolated.url().to_owned());
            (Arc::new(stores), raw, vec![Box::new(isolated)])
        }
    }
}

/// A core whose workflow processes call two source reads and one scripted
/// model.
fn core_over(stores: Arc<dyn StoreSet>) -> (lash::Backend, lash::LashCore) {
    node_over(
        stores,
        lash_core_execution::DurableSettings::default(),
        Vec::new(),
        move |backend| {
            let provider: Arc<dyn lash_core::ToolProvider> = Arc::new(
                lash::tools::StaticToolProvider::new(descriptions(), Source(AtomicUsize::new(0))),
            );
            let tools = lash::plugins::StaticPluginFactory::new(
                lash::plugins::PluginDeclaration::initial(TOOLS.component()),
                lash::plugins::PluginSpec::new()
                    .with_tool_provider(lash::tool_provider_key!("tools"), Arc::clone(&provider))
                    .with_package_provider(
                        lash::tool_provider_key!("tools"),
                        packages::operation_provider(
                            TOOLS.component(),
                            lash::tool_provider_key!("tools"),
                            provider,
                        ),
                    ),
            )
            .as_component(TOOLS.format())
            .unwrap();
            let workers = lash::vm::WorkerService::default();
            let bounds = workers.config().run_bounds;
            lash::LashCore::standard_builder(backend.clone())
                .serve_test_llm_profile(
                    scripted(|request, _| {
                        text(
                            request,
                            r#"{"kind":"value","value":{"name":"filed","tags":["source"]},"error":null}"#,
                        )
                    }),
                    metadata(),
                )
                .plugin(Arc::new(lash_llm_tools::LlmToolsPluginFactory::default()))
                .process_component(
                    Arc::new(lash_llm_tools::LlmToolsComponent::default()),
                    lash_llm_tools::LLM_TOOLS_CONFIG,
                )
                .plugin(Arc::new(lash::vm::KernelProcessPluginFactory::new(
                    workers,
                    "typescript",
                    bounds,
                )))
                .process_component(Arc::new(tools), TOOLS)
                .process_component(Arc::new(Principal), PRINCIPAL)
        },
    )
}

/// Run one workflow process (a source read, a model query, another source
/// read) to its successful end.
async fn run_one(core: &lash::LashCore) -> ProcessId {
    let pin = lash_core::HostArtifactPin::mint();
    let base = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(16))
        .model(lash_core::ProcessModel::new(
            lash_core::LlmProfileConfig::new(lash_core::RecordedLlmProfile::mint(
                lash_core::LlmProfileKey::new(MODEL.to_owned()),
                metadata(),
            )),
            Default::default(),
            Default::default(),
        ))
        .component(lash_llm_tools::LLM_TOOLS_CONFIG, &())
        .component(TOOLS, &())
        .component(PRINCIPAL, &serde_json::json!({}))
        .build()
        .unwrap();
    let (packages, namespace) = kernel_process::namespace(core, &pin, &base).await;
    let release = lash_kernel_releases::helper_release().unwrap();
    let roots = release.resolver_roots();
    let library =
        lash::dialect::NamedLibrary::resolving(&release.functions, |id| roots.contains(id))
            .unwrap();
    let checked = core
        .host_artifacts()
        .workflow_environment(&namespace)
        .await
        .unwrap()
        .unwrap();
    let bound = lash::dialect::Environment::new(&library, checked.namespaces());
    let source = "export async function run(): Promise<unknown> {\n  const r0 = await pruned00.read({});\n  const label = await llm.query({ task: \"Name the source\", output: { name: \"str\", tags: \"list[str]\" } });\n  const r1 = await pruned01.read({});\n  return { label: label, first: r0, last: r1 };\n}";
    let lowered = lash::typescript::lower(source, &bound, lash::dialect::Mode::Module).unwrap();
    let start = core
        .host_artifacts()
        .publish_workflow(
            &pin,
            &lowered.document.clone().into(),
            &lash::package::StartSelector::Export(lash_kernel_value::names::Name::new("run")),
            &namespace,
            None,
            None,
        )
        .await
        .unwrap();
    let draft = lash::vm::definition_draft(&lash::process::KernelProcessDefinition {
        code: lash::vm::KernelInlineRequest::Linked { start },
    })
    .unwrap();
    let definition = core
        .host_artifacts()
        .publish_definition(&pin, &draft)
        .await
        .unwrap();
    let env = core
        .host_artifacts()
        .link_process_env(
            &base,
            &lash::process::LinkTarget::Definition(definition.id.clone()),
            &packages,
            &Default::default(),
        )
        .await
        .unwrap();
    let settings = core
        .processes()
        .engine_defaults(lash_vm_runtime::LASH_VM_ENGINE_KIND, &serde_json::json!({}))
        .unwrap();
    let request = lash::process::ProcessStartRequest::definition(
        definition.id.clone(),
        Some(definition.signature.clone()),
        Default::default(),
        lash::process::EngineLaunch::new(env.stable_ref().unwrap(), settings),
        lash::process::ProcessOriginator::host(),
        lash::process::LifetimeDecision::Detached,
    );
    let process = core
        .processes()
        .start_with_env(lash::OperationId::mint(), request, env)
        .await
        .unwrap()
        .process_id;
    success(&ended(core, &process).await);
    process
}

/// A finished process's prompt snapshots read until it is pruned; the prune
/// takes them and their text edges, and a read of one then answers `None`.
async fn a_pruned_process_takes_its_prompt_snapshots(tier: Tier) {
    let (stores, raw, _keep) = raw_stores(tier).await;
    let (backend, core) = core_over(stores);
    let process = Box::pin(run_one(&core)).await;
    let durable = Arc::clone(backend.durable());

    let prompts = raw.prompt_keys(&process).await;
    assert!(!prompts.is_empty(), "the process made a model call");
    for key in &prompts {
        assert!(
            durable.prompt_snapshot(key).await.unwrap().is_some(),
            "the finished process's prompt snapshot reads before the prune"
        );
    }
    assert!(raw.prompt_rows(&process).await > 0);

    // The process's output is read before its terminal cascade completes,
    // and a prune skips a process whose cascade is still running: wait for
    // the end to settle, as a host's later prune would find it.
    tokio::time::timeout(Duration::from_secs(60), async {
        while !raw.prunable(&process).await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the process's terminal cascade completes");
    let pruned = core
        .processes()
        .prune(
            u64::MAX,
            None,
            lash::process::ProjectionWatermark::NoProjector,
        )
        .await
        .unwrap();
    assert_eq!(pruned.pruned_processes, 1);
    for key in &prompts {
        assert_eq!(
            durable.prompt_snapshot(key).await.unwrap(),
            None,
            "prune takes the process's prompt snapshot"
        );
    }
    assert_eq!(
        raw.prompt_rows(&process).await,
        0,
        "prune takes the process's prompt snapshot rows and their text edges"
    );
    core.shutdown().await.unwrap();
}

on_every_tier!(a_pruned_process_takes_its_prompt_snapshots: sqlite_memory, postgres);
