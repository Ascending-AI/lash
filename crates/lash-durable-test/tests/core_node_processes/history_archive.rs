//! A finished process's history archive (FIG-7701).
//!
//! A real workflow process (two tool calls and one model query) runs to its
//! end. Every reader of its history (run facts, tool calls, model calls and
//! root hydration, with their selectors, unknown keys and refusals) answers
//! the same before and after the pack moves the rows into the process's
//! archive. A history over the cap keeps its rows. A corrupt node refuses
//! only the reads that reach it. Prune removes the archive with its process.

use super::*;
use std::collections::{BTreeMap, BTreeSet};

use lash_core_store::checkpoint_graph::{DecodeLimits, GraphReadPolicy, GraphRefusal, NodeId};
use lash_durable::DurableError;
use lash_durable::domain::process_archive::{PROCESS_ARCHIVE_CAP_BYTES, model_session};
use lash_durable::domain::tool_calls::ToolCallRoot;
use lash_durable::domain::{
    CallOwner, ModelCallId, OwnerKey, PromptCallKey, RootOwner, RunSeq, ToolCallCursor,
    ToolCallSelection,
};
use lash_protocol_types::ToolCallId;
use lash_protocol_types::llm::types::LlmCallId;
use lash_vocabulary::identity::ProcessId;

const PRINCIPAL: lash::process::ProcessConfigKey<serde_json::Value> =
    lash::process::ProcessConfigKey::new("archive-principal", lash::plugins::FormatVersion::ONE);
const TOOLS: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("archive-tools", lash::plugins::FormatVersion::ONE);

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

/// A source read answering about `self.0` text bytes, distinct per call.
struct Source(usize, AtomicUsize);

#[lash::async_trait]
impl lash::tools::StaticToolExecute for Source {
    async fn execute(&self, _: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        let n = self.1.fetch_add(1, Ordering::SeqCst);
        let record = format!("record {n:04}: ");
        let text = record.repeat(self.0 / record.len() + 1);
        lash_core::ToolAttemptOutcome::done_without_intents(
            lash_core::ToolOutcomeDone::from_output(lash_core::ToolCallOutput::success(
                serde_json::json!({ "source": n, "text": text }),
            )),
        )
    }
}

fn descriptions() -> Vec<lash_vocabulary::ToolDefinition> {
    (0..2)
        .map(|i| {
            lash_vocabulary::ToolDefinition::raw(
                format!("archive:{i}"),
                format!("archive{i:02}.read"),
                "Reads a source.",
                serde_json::json!({ "type": "object", "properties": {}, "additionalProperties": false }),
                serde_json::json!({ "type": "object" }),
            )
            .unwrap()
            .with_execution(Duration::from_secs(30))
        })
        .collect()
}

/// The database beside the store: what a law reads and corrupts directly.
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

    /// The process's history decision and archive size, or `None` once the
    /// process row is gone.
    async fn history(&self, process: &ProcessId) -> Option<(Option<String>, Option<i64>)> {
        match self {
            Self::Sqlite(path) => {
                use rusqlite::OptionalExtension as _;
                Self::sqlite(path)
                    .query_row(
                        "SELECT history, length(history_archive) FROM processes WHERE process_id = ?1",
                        [process.as_str()],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()
                    .unwrap()
            }
            Self::Postgres(url) => sqlx::query_as(
                "SELECT history, octet_length(history_archive)::BIGINT FROM lash_processes WHERE process_id = $1",
            )
            .bind(process.as_str())
            .fetch_optional(&mut Self::pg(url).await)
            .await
            .unwrap(),
        }
    }

    /// The process's run fact, tool call and model call rows.
    async fn rows(&self, process: &ProcessId) -> i64 {
        let owner = OwnerKey::Process(process.clone()).stored();
        let session = model_session(process).as_str().to_owned();
        let sql = |prefix: &str, owner_key: &str, session_key: &str| {
            format!(
                "SELECT (SELECT count(*) FROM {prefix}run_records WHERE owner_key = {owner_key}) \
                 + (SELECT count(*) FROM {prefix}tool_calls WHERE owner_key = {owner_key}) \
                 + (SELECT count(*) FROM {prefix}model_call_records WHERE session_id = {session_key})"
            )
        };
        match self {
            Self::Sqlite(path) => Self::sqlite(path)
                .query_row(&sql("", "?1", "?2"), [&owner, &session], |row| row.get(0))
                .unwrap(),
            Self::Postgres(url) => sqlx::query_scalar(&sql("lash_", "$1", "$2"))
                .bind(&owner)
                .bind(&session)
                .fetch_one(&mut Self::pg(url).await)
                .await
                .unwrap(),
        }
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

    /// The owner and call columns of the process's model calls.
    async fn model_keys(&self, process: &ProcessId) -> Vec<(String, String)> {
        let session = model_session(process).as_str().to_owned();
        match self {
            Self::Sqlite(path) => Self::sqlite(path)
                .prepare(
                    "SELECT run, call FROM model_call_records WHERE session_id = ?1 ORDER BY run, call",
                )
                .unwrap()
                .query_map([&session], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap(),
            Self::Postgres(url) => sqlx::query_as(
                "SELECT run, call FROM lash_model_call_records WHERE session_id = $1 ORDER BY run, call",
            )
            .bind(&session)
            .fetch_all(&mut Self::pg(url).await)
            .await
            .unwrap(),
        }
    }

    async fn archive(&self, process: &ProcessId) -> Vec<u8> {
        match self {
            Self::Sqlite(path) => Self::sqlite(path)
                .query_row(
                    "SELECT history_archive FROM processes WHERE process_id = ?1",
                    [process.as_str()],
                    |row| row.get(0),
                )
                .unwrap(),
            Self::Postgres(url) => sqlx::query_scalar(
                "SELECT history_archive FROM lash_processes WHERE process_id = $1",
            )
            .bind(process.as_str())
            .fetch_one(&mut Self::pg(url).await)
            .await
            .unwrap(),
        }
    }

    /// Which of `nodes` a live blob holds.
    async fn live(&self, nodes: &BTreeSet<NodeId>) -> BTreeSet<NodeId> {
        let mut live = BTreeSet::new();
        for node in nodes {
            let found: i64 = match self {
                Self::Sqlite(path) => Self::sqlite(path)
                    .query_row(
                        "SELECT count(*) FROM blobs WHERE hash = ?1",
                        [&node.0[..]],
                        |row| row.get(0),
                    )
                    .unwrap(),
                Self::Postgres(url) => {
                    sqlx::query_scalar("SELECT count(*) FROM lash_blobs WHERE hash = $1")
                        .bind(&node.0[..])
                        .fetch_one(&mut Self::pg(url).await)
                        .await
                        .unwrap()
                }
            };
            if found > 0 {
                live.insert(*node);
            }
        }
        live
    }

    /// The children live node `node`'s stored edges name.
    async fn children(&self, node: &NodeId) -> Vec<NodeId> {
        let sql = "SELECT child.hash FROM {e} AS edge
             JOIN {b} AS parent ON parent.id = edge.parent_id
             JOIN {b} AS child ON child.id = edge.child_id
             WHERE parent.hash = ";
        let hashes: Vec<Vec<u8>> = match self {
            Self::Sqlite(path) => {
                let sql = sql.replace("{e}", "blob_edges").replace("{b}", "blobs") + "?1";
                let connection = Self::sqlite(path);
                let mut statement = connection.prepare(&sql).unwrap();
                statement
                    .query_map([&node.0[..]], |row| row.get(0))
                    .unwrap()
                    .collect::<Result<_, _>>()
                    .unwrap()
            }
            Self::Postgres(url) => {
                let sql = sql
                    .replace("{e}", "lash_blob_edges")
                    .replace("{b}", "lash_blobs")
                    + "$1";
                sqlx::query_scalar(&sql)
                    .bind(&node.0[..])
                    .fetch_all(&mut Self::pg(url).await)
                    .await
                    .unwrap()
            }
        };
        hashes
            .into_iter()
            .map(|hash| NodeId(hash.try_into().unwrap()))
            .collect()
    }

    /// The stored encoding and bytes of live node `node`.
    async fn node(&self, node: &NodeId) -> (i64, Vec<u8>) {
        match self {
            Self::Sqlite(path) => Self::sqlite(path)
                .query_row(
                    "SELECT encoding, content FROM blobs WHERE hash = ?1",
                    [&node.0[..]],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap(),
            Self::Postgres(url) => {
                let (encoding, content): (i16, Vec<u8>) =
                    sqlx::query_as("SELECT encoding, content FROM lash_blobs WHERE hash = $1")
                        .bind(&node.0[..])
                        .fetch_one(&mut Self::pg(url).await)
                        .await
                        .unwrap();
                (i64::from(encoding), content)
            }
        }
    }

    /// Store live node `node` as `encoding` and `bytes`, whatever they are:
    /// the encoding check of the blob table does not hold here.
    async fn set_node(&self, node: &NodeId, encoding: i64, bytes: &[u8]) {
        match self {
            Self::Sqlite(path) => {
                let connection = Self::sqlite(path);
                connection
                    .execute_batch("PRAGMA ignore_check_constraints = ON")
                    .unwrap();
                connection
                    .execute(
                        "UPDATE blobs SET encoding = ?2, content = ?3 WHERE hash = ?1",
                        rusqlite::params![&node.0[..], encoding, bytes],
                    )
                    .unwrap();
            }
            Self::Postgres(url) => {
                let mut connection = Self::pg(url).await;
                sqlx::query("ALTER TABLE lash_blobs DROP CONSTRAINT IF EXISTS ck_blobs_encoding")
                    .execute(&mut connection)
                    .await
                    .unwrap();
                sqlx::query("UPDATE lash_blobs SET encoding = $2, content = $3 WHERE hash = $1")
                    .bind(&node.0[..])
                    .bind(i16::try_from(encoding).unwrap())
                    .bind(bytes)
                    .execute(&mut connection)
                    .await
                    .unwrap();
            }
        }
    }

    /// Run `statements` against the process's tables, in order: `{p}` is
    /// the table prefix, `{owner}`, `{session}` and `{actor}` the process's
    /// owner key, model session and actor key, as quoted literals, and
    /// `{big}` a text of [`BIG`] bytes.
    async fn alter(&self, process: &ProcessId, statements: &[&str]) {
        let quote = |text: &str| format!("'{}'", text.replace('\'', "''"));
        let actor = lash_durable::ActorKey::process(process.as_str()).unwrap();
        let render = |prefix: &str, statement: &str| {
            let big = if prefix.is_empty() {
                format!("printf('%.*c', {BIG}, 'x')")
            } else {
                format!("repeat('x', {BIG})")
            };
            statement
                .replace("{big}", &big)
                .replace("{p}", prefix)
                .replace(
                    "{owner}",
                    &quote(&OwnerKey::Process(process.clone()).stored()),
                )
                .replace("{session}", &quote(model_session(process).as_str()))
                .replace("{actor}", &quote(actor.as_str()))
                .replace("{process}", &quote(process.as_str()))
        };
        for statement in statements {
            match self {
                Self::Sqlite(path) => {
                    Self::sqlite(path)
                        .execute_batch(&render("", statement))
                        .unwrap();
                }
                Self::Postgres(url) => {
                    sqlx::raw_sql(&render("lash_", statement))
                        .execute(&mut Self::pg(url).await)
                        .await
                        .unwrap();
                }
            }
        }
    }

    /// `process`'s first call states a reason of `bytes` bytes.
    async fn reason_of(&self, process: &ProcessId, bytes: usize) {
        let fill = match self {
            Self::Sqlite(_) => format!("printf('%.*c', {bytes}, 'x')"),
            Self::Postgres(_) => format!("repeat('x', {bytes})"),
        };
        self.alter(
            process,
            &[&format!(
                "UPDATE {{p}}tool_calls SET reason = {fill} \
                 WHERE owner_key = {{owner}} AND call_id = \
                   (SELECT min(call_id) FROM {{p}}tool_calls WHERE owner_key = {{owner}})"
            )],
        )
        .await;
    }

    /// Run `sqlite` or `postgres`, whichever is this store's.
    async fn run(&self, sqlite: &str, postgres: &str) {
        match self {
            Self::Sqlite(path) => Self::sqlite(path).execute_batch(sqlite).unwrap(),
            Self::Postgres(url) => {
                sqlx::raw_sql(postgres)
                    .execute(&mut Self::pg(url).await)
                    .await
                    .unwrap();
            }
        }
    }

    /// The store set over this database, opened afresh: what a node that
    /// crashed opens when it comes back.
    async fn reopen(&self) -> Arc<dyn StoreSet> {
        match self {
            Self::Sqlite(path) => Arc::new(
                lash_sqlite_store::SqliteStoreSet::open(
                    path,
                    lash_sqlite_store::SqliteSynchronous::Normal,
                )
                .await
                .unwrap(),
            ),
            Self::Postgres(url) => {
                let storage = lash_postgres_store::testing::connect(url)
                    .await
                    .expect("the isolated database opens");
                Arc::new(lash_postgres_store::PostgresStoreSet::new(
                    &storage,
                    Arc::new(lash_core_store::attachments::UnavailableAttachmentStore),
                ))
            }
        }
    }

    async fn set_archive(&self, process: &ProcessId, bytes: &[u8]) {
        match self {
            Self::Sqlite(path) => {
                Self::sqlite(path)
                    .execute(
                        "UPDATE processes SET history_archive = ?2 WHERE process_id = ?1",
                        rusqlite::params![process.as_str(), bytes],
                    )
                    .unwrap();
            }
            Self::Postgres(url) => {
                sqlx::query("UPDATE lash_processes SET history_archive = $2 WHERE process_id = $1")
                    .bind(process.as_str())
                    .bind(bytes)
                    .execute(&mut Self::pg(url).await)
                    .await
                    .unwrap();
            }
        }
    }
}

/// A store set of `tier` on a database the law can also open directly, and
/// what must outlive it.
async fn raw_stores(tier: Tier) -> (Arc<dyn StoreSet>, Raw, Vec<Box<dyn std::any::Any + Send>>) {
    match tier {
        Tier::SqliteMemory => {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("archive.db");
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

/// A core whose workflow processes call two source reads, each answering
/// about `answer_bytes`, and one scripted model. Its cleanup pass does not
/// run within a law, so only the law packs.
fn core_over(stores: Arc<dyn StoreSet>, answer_bytes: usize) -> (lash::Backend, lash::LashCore) {
    node_over(
        stores,
        lash_core_execution::DurableSettings::default(),
        Vec::new(),
        move |backend| {
            let provider: Arc<dyn lash_core::ToolProvider> =
                Arc::new(lash::tools::StaticToolProvider::new(
                    descriptions(),
                    Source(answer_bytes, AtomicUsize::new(0)),
                ));
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
                .recovery_pacing(
                    lash::RecoveryPacing::new(
                        Duration::from_secs(24 * 3600),
                        std::num::NonZeroUsize::MIN,
                    )
                    .unwrap(),
                )
        },
    )
}

/// Run one workflow process making `reads` source reads to its successful
/// end.
async fn run_one(core: &lash::LashCore, reads: usize) -> ProcessId {
    Box::pin(run_observed(core, reads, false)).await
}

/// Run one workflow process making `reads` source reads to its successful
/// end, observed at counts when `counted` so its segments replay.
async fn run_observed(core: &lash::LashCore, reads: usize, counted: bool) -> ProcessId {
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
    // The model query between the first read and the rest; the answer keeps
    // the first and last reads only when they are small.
    let mut source = String::from(
        "export async function run(): Promise<unknown> {\n  const r0 = await archive00.read({});\n  const label = await llm.query({ task: \"Name the source\", output: { name: \"str\", tags: \"list[str]\" } });\n",
    );
    for read in 1..reads {
        source.push_str(&format!(
            "  const r{read} = await archive0{}.read({{}});\n",
            read % 2
        ));
    }
    source.push_str(&if reads == 2 {
        "  return { label: label, first: r0, last: r1 };\n}".to_owned()
    } else {
        "  return { label: label };\n}".to_owned()
    });
    let lowered = lash::typescript::lower(&source, &bound, lash::dialect::Mode::Module).unwrap();
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
    let settings = if counted {
        Some(
            core.processes()
                .kernel_engine_defaults()
                .unwrap()
                .observing(lash_kernel_value::observation::RunObservation::Counts)
                .into_engine_settings(),
        )
    } else {
        core.processes()
            .engine_defaults(lash_vm_runtime::LASH_VM_ENGINE_KIND, &serde_json::json!({}))
            .unwrap()
    };
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

fn fact(owner: &OwnerKey, run: RunSeq, ordinal: lash_durable::domain::Ordinal) -> RootOwner {
    RootOwner::RunFact {
        owner: owner.clone(),
        run,
        ordinal,
    }
}

/// Every history read of `process`, labelled, with its exact answer or
/// refusal: the selectors, unknown keys, a wrong expected root, the row and
/// graph bounds, and each root's hydration.
async fn answers(
    store: &dyn lash_durable::DurableStore,
    process: &ProcessId,
    model_keys: &[(String, String)],
) -> BTreeMap<String, String> {
    answers_naming(store, process, model_keys, None).await
}

/// [`answers`], with the point reads naming `calls` (the process's calls as
/// its rows listed them) instead of the calls its listing answers.
async fn answers_naming(
    store: &dyn lash_durable::DurableStore,
    process: &ProcessId,
    model_keys: &[(String, String)],
    calls: Option<&[lash_durable::domain::tool_calls::ToolCallRow]>,
) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let limits = GraphReadPolicy::standard().owner();
    let owner = OwnerKey::Process(process.clone());
    let calls_owner = CallOwner::Process(process.clone());
    let session = model_session(process);

    let facts = store.run_facts(&owner).await;
    out.insert("facts".to_owned(), format!("{facts:?}"));
    let rows = facts.map(|facts| facts.rows).unwrap_or_default();
    assert!(
        rows.len() >= 2,
        "the process recorded its run facts: {rows:?}"
    );
    let mut runs: Vec<RunSeq> = rows.iter().map(|row| row.run).collect();
    runs.dedup();
    runs.push(RunSeq(9_999));
    for run in runs {
        out.insert(
            format!("facts of {run:?}"),
            format!("{:?}", store.run_facts_of(&owner, run).await),
        );
    }
    for max_rows in [rows.len(), rows.len() - 1] {
        out.insert(
            format!("facts within {max_rows} rows"),
            format!(
                "{:?}",
                store.run_facts_within(&owner, None, max_rows, limits).await
            ),
        );
    }
    out.insert(
        "facts within one node".to_owned(),
        format!(
            "{:?}",
            store
                .run_facts_within(
                    &owner,
                    None,
                    rows.len(),
                    DecodeLimits { nodes: 1, ..limits }
                )
                .await
        ),
    );
    for row in &rows {
        let at = format!("{:?} {:?}", row.run, row.ordinal);
        let root = fact(&owner, row.run, row.ordinal);
        for (label, expected) in [
            ("", None),
            (" expecting it", Some(row.record)),
            (" expecting another", Some(NodeId([7; 32]))),
        ] {
            out.insert(
                format!("hydrate fact {at}{label}"),
                format!("{:?}", store.hydrate(root.clone(), expected, limits).await),
            );
        }
    }
    out.insert(
        "hydrate an unknown fact".to_owned(),
        format!(
            "{:?}",
            store
                .hydrate(fact(&owner, RunSeq(9_999), rows[0].ordinal), None, limits)
                .await
        ),
    );

    let listed = store
        .tool_calls(
            &calls_owner,
            &ToolCallSelection::List {
                after: None,
                limit: 100,
            },
        )
        .await;
    out.insert("calls".to_owned(), format!("{listed:?}"));
    let calls = match calls {
        Some(calls) => calls.to_vec(),
        None => listed.map(|calls| calls.rows).unwrap_or_default(),
    };
    assert!(
        calls.len() >= 2,
        "the process admitted its reads: {calls:?}"
    );
    let cursor = ToolCallCursor {
        sequence: calls[0].sequence,
        call: calls[0].call.clone(),
    };
    let unknown = ToolCallId::unrecorded_read("an unknown caller", 0);
    for (label, selection) in [
        (
            "calls, first page",
            ToolCallSelection::List {
                after: None,
                limit: 1,
            },
        ),
        (
            "calls after the first",
            ToolCallSelection::List {
                after: Some(cursor.clone()),
                limit: 100,
            },
        ),
        (
            "saved reads",
            ToolCallSelection::SavedReads {
                after: None,
                limit: 100,
            },
        ),
        (
            "saved reads after the first",
            ToolCallSelection::SavedReads {
                after: Some(cursor),
                limit: 100,
            },
        ),
        (
            "named calls",
            ToolCallSelection::Calls(vec![
                unknown.clone(),
                calls[1].call.clone(),
                calls[0].call.clone(),
            ]),
        ),
        ("an unknown call", ToolCallSelection::Calls(vec![unknown])),
    ] {
        out.insert(
            label.to_owned(),
            format!("{:?}", store.tool_calls(&calls_owner, &selection).await),
        );
    }
    for call in &calls {
        for part in [ToolCallRoot::Decision, ToolCallRoot::Answer] {
            let root = RootOwner::ToolCall {
                owner: call.execution.clone(),
                call: call.call.clone(),
                part,
            };
            out.insert(
                format!("hydrate call {} {part:?}", call.call.as_str()),
                format!("{:?}", store.hydrate(root, None, limits).await),
            );
        }
    }

    assert!(
        !model_keys.is_empty(),
        "the process recorded its model call"
    );
    let mut owners: Vec<String> = model_keys.iter().map(|(run, _)| run.clone()).collect();
    owners.dedup();
    owners.push("owned:unknown".to_owned());
    for owner in &owners {
        let ids = store.model_call_ids(&session, owner).await;
        out.insert(format!("model ids of {owner}"), format!("{ids:?}"));
        out.insert(
            format!("models of {owner}"),
            format!("{:?}", store.model_calls(&session, owner).await),
        );
        out.insert(
            format!("model pins of {owner}"),
            format!("{:?}", store.model_call_pins(&session, owner).await),
        );
        let mut named = ids.unwrap_or_default();
        named.insert(0, LlmCallId("unknown".to_owned()));
        out.insert(
            format!("named models of {owner}"),
            format!(
                "{:?}",
                store.model_calls_by_id(&session, owner, &named).await
            ),
        );
        out.insert(
            format!("named model pins of {owner}"),
            format!(
                "{:?}",
                store.model_call_pins_by_id(&session, owner, &named).await
            ),
        );
    }
    for (run, call) in model_keys {
        let key = PromptCallKey {
            session: session.clone(),
            call: ModelCallId::Owned {
                owner: run
                    .strip_prefix("owned:")
                    .expect("a process's model call is owned")
                    .to_owned(),
                key: call.clone(),
            },
        };
        out.insert(
            format!("model pin {run} {call}"),
            format!("{:?}", store.model_call_pin(&key).await),
        );
        out.insert(
            format!("hydrate model {run} {call}"),
            format!(
                "{:?}",
                store.hydrate(RootOwner::ModelCall(key), None, limits).await
            ),
        );
    }
    out
}

/// Run `read` in its own SQL window: its answer and how many times it
/// fetched a process's history archive.
async fn archive_fetches<T>(tier: Tier, read: impl std::future::Future<Output = T>) -> (T, u64) {
    let backend = match tier {
        Tier::Postgres => "postgres",
        Tier::SqliteMemory => "sqlite",
    };
    let window = lash_core_ids::operational_metrics::sql::Window::new("archive-fetches", backend);
    let answer = window.within(read).await;
    let fetches = window
        .snapshot()
        .shapes
        .iter()
        .filter(|(shape, _)| shape.starts_with("select history_archive from"))
        .map(|(_, shape)| shape.statements)
        .sum();
    (answer, fetches)
}

/// One read call takes a packed history's archive once, however many
/// nodes it serves: the whole value comes back on every fetch, so a read
/// that fetched it per node or per row would multiply its bytes by them.
async fn each_read_fetches_the_archive_once(
    tier: Tier,
    store: &dyn lash_durable::DurableStore,
    process: &ProcessId,
    model_keys: &[(String, String)],
) {
    let owner = OwnerKey::Process(process.clone());
    let (facts, fetches) = archive_fetches(tier, store.run_facts(&owner)).await;
    let facts = facts.expect("read every run fact");
    assert_eq!(
        fetches,
        1,
        "the run facts and their {} nodes take one archive fetch",
        facts.nodes.len()
    );
    let (calls, fetches) = archive_fetches(
        tier,
        store.tool_calls(
            &CallOwner::Process(process.clone()),
            &ToolCallSelection::List {
                after: None,
                limit: 100,
            },
        ),
    )
    .await;
    let calls = calls.expect("read every tool call");
    assert_eq!(
        fetches,
        1,
        "the tool calls and their {} nodes take one archive fetch",
        calls.graph.nodes.len()
    );
    let session = model_session(process);
    let (run, _) = &model_keys[0];
    let (models, fetches) = archive_fetches(tier, store.model_calls(&session, run)).await;
    let models = models.expect("read the model calls");
    assert_eq!(
        fetches,
        1,
        "the {} model call records take one archive fetch",
        models.len()
    );
    eprintln!(
        "archive fetches: run facts 1 for {} rows / {} nodes, tool calls 1 for {} rows / {} nodes, model calls 1 for {} records",
        facts.rows.len(),
        facts.nodes.len(),
        calls.rows.len(),
        calls.graph.nodes.len(),
        models.len()
    );
}

/// The prompt keys of the process's model calls.
fn prompt_keys(process: &ProcessId, model_keys: &[(String, String)]) -> Vec<PromptCallKey> {
    model_keys
        .iter()
        .map(|(run, call)| PromptCallKey {
            session: model_session(process),
            call: ModelCallId::Owned {
                owner: run
                    .strip_prefix("owned:")
                    .expect("a process's model call is owned")
                    .to_owned(),
                key: call.clone(),
            },
        })
        .collect()
}

fn same(before: &BTreeMap<String, String>, after: &BTreeMap<String, String>, when: &str) {
    assert_eq!(
        before.keys().collect::<Vec<_>>(),
        after.keys().collect::<Vec<_>>(),
        "{when}: the same reads"
    );
    for (label, answer) in before {
        assert_eq!(answer, &after[label], "{when}: `{label}` answers the same");
    }
}

/// A finished process's history packs into its archive and every read
/// answers as it did from the rows. A corrupt node refuses only the reads
/// that reach it, and prune takes the archive with the process.
async fn a_packed_history_reads_as_its_rows(tier: Tier) {
    let (stores, raw, _keep) = raw_stores(tier).await;
    let (backend, core) = core_over(stores, 400);
    let process = Box::pin(run_one(&core, 2)).await;
    let durable = Arc::clone(backend.durable());
    let model_keys = raw.model_keys(&process).await;
    let before = answers(durable.as_ref(), &process, &model_keys).await;
    assert!(
        raw.rows(&process).await > 0,
        "the history is rows before the pack"
    );

    assert_eq!(
        durable.pack_finished_histories(16).await.unwrap(),
        1,
        "one history packs"
    );
    let (history, size) = raw.history(&process).await.unwrap();
    assert_eq!(history.as_deref(), Some("archive"));
    let size = usize::try_from(size.expect("an archive")).unwrap();
    assert!(
        size > 0 && size <= PROCESS_ARCHIVE_CAP_BYTES,
        "the archive is within the cap: {size}"
    );
    assert_eq!(raw.rows(&process).await, 0, "the pack moved every row");
    same(
        &before,
        &answers(durable.as_ref(), &process, &model_keys).await,
        "after the pack",
    );
    each_read_fetches_the_archive_once(tier, durable.as_ref(), &process, &model_keys).await;
    assert_eq!(
        durable.pack_finished_histories(16).await.unwrap(),
        0,
        "a packed history stays packed"
    );

    // A corrupt node refuses the reads that reach it, and no other.
    let limits = GraphReadPolicy::standard().owner();
    let owner = OwnerKey::Process(process.clone());
    let rows = durable.run_facts(&owner).await.unwrap().rows;
    let mut graphs = Vec::new();
    for row in &rows {
        graphs.push(
            durable
                .hydrate(fact(&owner, row.run, row.ordinal), None, limits)
                .await
                .unwrap(),
        );
    }
    let (target, sibling) = (0..graphs.len())
        .find_map(|at| {
            (0..graphs.len())
                .find(|other| *other != at && !graphs[*other].nodes.contains_key(&graphs[at].root))
                .map(|other| (at, other))
        })
        .expect("two facts, one not reaching the other's root");
    let archive = raw.archive(&process).await;
    let body = &graphs[target].nodes[&graphs[target].root].bytes;
    let at = archive
        .windows(body.len())
        .position(|window| window == &body[..])
        .expect("the archive holds the root's bytes");
    let mut corrupt = archive.clone();
    corrupt[at + body.len() - 1] ^= 0x5a;
    raw.set_archive(&process, &corrupt).await;
    let hydrate = |index: usize| {
        durable.hydrate(
            fact(&owner, rows[index].run, rows[index].ordinal),
            None,
            limits,
        )
    };
    let refused = hydrate(target).await;
    assert!(
        matches!(
            &refused,
            Err(DurableError::Graph(GraphRefusal::HashMismatch(node)))
                if *node == graphs[target].root
        ),
        "the corrupt root refuses its read: {refused:?}"
    );
    assert_eq!(
        hydrate(sibling).await.unwrap(),
        graphs[sibling],
        "a fact that does not reach the corrupt node reads as before"
    );
    raw.set_archive(&process, &archive).await;

    let prompts = prompt_keys(&process, &model_keys);
    for key in &prompts {
        assert!(
            durable.prompt_snapshot(key).await.unwrap().is_some(),
            "the finished process's prompt snapshot reads before the prune"
        );
    }
    assert!(raw.prompt_rows(&process).await > 0);

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
    assert_eq!(raw.history(&process).await, None, "prune takes the archive");
    // The prompt snapshots are the process's history too: prune takes
    // them with it, and a read of one answers as for any pruned fact.
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
    assert!(durable.run_facts(&owner).await.unwrap().rows.is_empty());
    assert!(matches!(
        hydrate(sibling).await,
        Err(DurableError::Graph(GraphRefusal::RootRetired))
    ));
    core.shutdown().await.unwrap();
}

on_every_tier!(a_packed_history_reads_as_its_rows: sqlite_memory, postgres);

/// A history over the archive's cap keeps every row, reads as it did, and
/// is not packed again.
async fn a_history_over_the_cap_keeps_its_rows(tier: Tier) {
    let (stores, raw, _keep) = raw_stores(tier).await;
    // Six reads of 50 KiB: each stays inline in history (64 KiB), together
    // over the cap.
    let (backend, core) = core_over(stores, 50 * 1024);
    let process = Box::pin(run_one(&core, 6)).await;
    let durable = Arc::clone(backend.durable());
    let model_keys = raw.model_keys(&process).await;
    let before = answers(durable.as_ref(), &process, &model_keys).await;
    let held = raw.rows(&process).await;
    let kept = Some((Some("rows".to_owned()), None));

    assert_eq!(
        durable.pack_finished_histories(16).await.unwrap(),
        0,
        "nothing packs"
    );
    assert_eq!(
        raw.history(&process).await,
        kept,
        "the history is decided as rows"
    );
    assert_eq!(raw.rows(&process).await, held, "every row stays");
    same(
        &before,
        &answers(durable.as_ref(), &process, &model_keys).await,
        "after the refused pack",
    );
    assert_eq!(durable.pack_finished_histories(16).await.unwrap(), 0);
    assert_eq!(
        raw.history(&process).await,
        kept,
        "a kept history is not retried"
    );
    core.shutdown().await.unwrap();
}

on_every_tier!(a_history_over_the_cap_keeps_its_rows: sqlite_memory, postgres);

/// A finished history packs only once nothing can write it again: its
/// actor ended, every call has its outcome and every model call its record
/// (review finding 5). Short of that, a call's answer, a reservation's
/// attempt or record, or a revived actor could still write a row the
/// archive would hide, so the history keeps its rows, reads as before and
/// is not retried.
async fn a_history_that_could_still_change_keeps_its_rows(tier: Tier) {
    let cases: [(&str, &[&str]); 3] = [
        (
            "a call with no outcome",
            &[
                "UPDATE {p}tool_calls SET answer_outcome = NULL, body_ref = NULL \
               WHERE owner_key = {owner} AND call_id = \
                 (SELECT min(call_id) FROM {p}tool_calls WHERE owner_key = {owner})",
            ],
        ),
        (
            "a model reservation",
            &["UPDATE {p}model_call_records \
               SET record_ref = NULL, ended_at_ms = NULL, written_epoch = NULL, attempt = 1 \
               WHERE session_id = {session} AND call_id = \
                 (SELECT min(call_id) FROM {p}model_call_records WHERE session_id = {session})"],
        ),
        (
            "no actor",
            &[
                "DELETE FROM {p}actor_mail WHERE actor_key = {actor}",
                "DELETE FROM {p}environment_owners WHERE actor_key = {actor}",
                "DELETE FROM {p}actors WHERE actor_key = {actor}",
            ],
        ),
    ];
    for (case, statements) in cases {
        let (stores, raw, _keep) = raw_stores(tier).await;
        let (backend, core) = core_over(stores, 1024);
        let process = Box::pin(run_one(&core, 2)).await;
        let durable = Arc::clone(backend.durable());
        raw.alter(&process, statements).await;
        let model_keys = raw.model_keys(&process).await;
        let before = answers(durable.as_ref(), &process, &model_keys).await;
        let held = raw.rows(&process).await;
        assert!(held > 0, "{case}: the history has rows");
        let kept = Some((Some("rows".to_owned()), None));

        assert_eq!(
            durable.pack_finished_histories(16).await.unwrap(),
            0,
            "{case}: nothing packs"
        );
        assert_eq!(
            raw.history(&process).await,
            kept,
            "{case}: the history is decided as rows"
        );
        assert_eq!(raw.rows(&process).await, held, "{case}: every row stays");
        same(
            &before,
            &answers(durable.as_ref(), &process, &model_keys).await,
            case,
        );
        assert_eq!(durable.pack_finished_histories(16).await.unwrap(), 0);
        core.shutdown().await.unwrap();
    }
}

on_every_tier!(a_history_that_could_still_change_keeps_its_rows: sqlite_memory, postgres);

/// One value longer than the archive cap.
const BIG: usize = PROCESS_ARCHIVE_CAP_BYTES + 1;

/// Run `work` in its own SQL window: its answer and the bytes its
/// statements returned.
async fn result_bytes<T>(tier: Tier, work: impl std::future::Future<Output = T>) -> (T, u64) {
    let backend = match tier {
        Tier::Postgres => "postgres",
        Tier::SqliteMemory => "sqlite",
    };
    let window = lash_core_ids::operational_metrics::sql::Window::new("pack-bytes", backend);
    let answer = window.within(work).await;
    (answer, window.snapshot().result_bytes)
}

/// The text a history's rows would put in the archive is sized before any
/// row is read (review finding 5): a call reason or a model call's
/// enclosing call id longer than the cap keeps the history as rows, and
/// packing reads fewer bytes than that one value. (Both columns are
/// unindexed: PostgreSQL refuses a value this long in an indexed one.)
async fn an_oversized_value_keeps_its_rows_unread(tier: Tier) {
    let cases: [(&str, &str); 2] = [
        (
            "a call reason",
            "UPDATE {p}tool_calls SET reason = {big} \
             WHERE owner_key = {owner} AND call_id = \
               (SELECT min(call_id) FROM {p}tool_calls WHERE owner_key = {owner})",
        ),
        (
            "a model call's enclosing call id",
            "UPDATE {p}model_call_records \
             SET enclosing_owner = 'enclosing', enclosing_run = 0, enclosing_call = {big} \
             WHERE session_id = {session} AND call_id = \
               (SELECT min(call_id) FROM {p}model_call_records WHERE session_id = {session})",
        ),
    ];
    for (case, statement) in cases {
        let (stores, raw, _keep) = raw_stores(tier).await;
        let (backend, core) = core_over(stores, 1024);
        let process = Box::pin(run_one(&core, 2)).await;
        let durable = Arc::clone(backend.durable());
        raw.alter(&process, &[statement]).await;
        let model_keys = raw.model_keys(&process).await;
        let before = answers(durable.as_ref(), &process, &model_keys).await;
        let held = raw.rows(&process).await;

        let (packed, fetched) = result_bytes(tier, durable.pack_finished_histories(16)).await;
        assert_eq!(packed.unwrap(), 0, "{case}: nothing packs");
        assert!(
            fetched < BIG as u64,
            "{case}: packing read {fetched} bytes, the value alone is {BIG}"
        );
        assert_eq!(
            raw.history(&process).await,
            Some((Some("rows".to_owned()), None)),
            "{case}: the history is decided as rows"
        );
        assert_eq!(raw.rows(&process).await, held, "{case}: every row stays");
        same(
            &before,
            &answers(durable.as_ref(), &process, &model_keys).await,
            case,
        );
        core.shutdown().await.unwrap();
    }
}

on_every_tier!(an_oversized_value_keeps_its_rows_unread: sqlite_memory, postgres);

/// The page store's retirement, run until a step does nothing.
async fn drain(pages: &dyn lash_durable::domain::pages::PageStore) {
    let budget = lash_durable::domain::pages::RetirementBudget { nodes: 4096 };
    while pages.retire_step(budget).await.unwrap()
        != lash_durable::domain::pages::RetirementProgress::default()
    {}
}

/// A reader's lease on run fact `root` of `owner`.
fn reader(owner: RootOwner, root: NodeId) -> lash_durable::domain::pages::RootLeaseRequest {
    lash_durable::domain::pages::RootLeaseRequest {
        owner,
        expected: root,
        fence: lash_durable::domain::pages::LeaseFence::Reader,
        ttl_millis: 3_600_000,
    }
}

/// A packed history's roots lease as they did from its rows (FIG-7701,
/// review finding 1). A reader's lease taken before the pack keeps reading
/// through the pack, the peel of the rows' roots, the prune of the process
/// and the retirement after each; a lease taken after the pack restores the
/// live closure it reads, and its pages, renewals and a parked replay
/// answer as from the rows. A shared node's last other live copy retires
/// while a restored lease still reads it. Released, the restored copies
/// retire with nothing left behind.
async fn a_packed_history_leases_as_its_rows(tier: Tier) {
    let (stores, raw, _keep) = raw_stores(tier).await;
    let (backend, core) = core_over(stores, 400);
    let first = Box::pin(run_observed(&core, 2, true)).await;
    let second = Box::pin(run_observed(&core, 2, true)).await;
    let durable = Arc::clone(backend.durable());
    let pages = durable.pages().expect("the store publishes heap pages");
    let limits = GraphReadPolicy::standard().owner();
    let replay = |process: ProcessId| {
        let core = &core;
        async move {
            core.processes()
                .replay(&process, lash::workflow::ReplaySegments::All)
                .await
                .unwrap()
        }
    };

    // Every run fact of both processes: its root, the graph pages a lease
    // on it reads, and what they read from the rows.
    let mut facts = Vec::new();
    for process in [&first, &second] {
        let owner = OwnerKey::Process(process.clone());
        for row in durable.run_facts(&owner).await.unwrap().rows {
            let at = fact(&owner, row.run, row.ordinal);
            let graph = durable.hydrate(at.clone(), None, limits).await.unwrap();
            let ids: Vec<NodeId> = graph
                .nodes
                .iter()
                .filter(|(_, node)| {
                    node.encoding == lash_core_store::checkpoint_graph::BlobEncoding::Graph
                })
                .map(|(id, _)| *id)
                .collect();
            let lease = pages
                .acquire_root_lease(reader(at.clone(), graph.root))
                .await
                .unwrap();
            let read = format!("{:?}", pages.read_pages(&lease, &ids).await);
            pages.release_root_lease(lease.id).await.unwrap();
            facts.push((process.clone(), at, graph.root, ids, read));
        }
    }
    let first_facts: Vec<usize> = (0..facts.len())
        .filter(|at| facts[*at].0 == first)
        .collect();
    let second_facts: Vec<usize> = (0..facts.len())
        .filter(|at| facts[*at].0 == second)
        .collect();
    assert!(first_facts.len() >= 2 && !second_facts.is_empty());
    let replayed = replay(first.clone()).await;
    assert!(
        replayed.iter().any(|segment| segment.segment > 0
            && matches!(
                segment.answer,
                lash::workflow::SegmentAnswer::Verified { .. }
            )),
        "a segment after a park replays from its parked page: {replayed:?}"
    );
    let every: BTreeSet<NodeId> = facts
        .iter()
        .flat_map(|fact| fact.3.iter().copied())
        .collect();

    // A lease taken before the pack.
    let (_, early_at, early_root, early_ids, early_read) = &facts[first_facts[0]];
    let early = pages
        .acquire_root_lease(reader(early_at.clone(), *early_root))
        .await
        .unwrap();
    assert_eq!(durable.pack_finished_histories(16).await.unwrap(), 2);
    assert_eq!(raw.rows(&first).await + raw.rows(&second).await, 0);
    drain(pages.as_ref()).await;
    let held_by_others = raw.live(&every).await;
    assert_eq!(
        &format!("{:?}", pages.read_pages(&early, early_ids).await),
        early_read,
        "a lease taken before the pack reads through it"
    );
    let early = pages.renew_root_lease(&early).await.unwrap();

    // Leases taken after the pack, on every fact.
    for (_, at, root, ids, read) in &facts {
        let lease = pages
            .acquire_root_lease(reader(at.clone(), *root))
            .await
            .unwrap();
        assert_eq!(
            &format!("{:?}", pages.read_pages(&lease, ids).await),
            read,
            "a lease taken after the pack reads as from the rows"
        );
        let lease = pages.renew_root_lease(&lease).await.unwrap();
        pages.release_root_lease(lease.id).await.unwrap();
    }
    drain(pages.as_ref()).await;
    assert_eq!(
        replay(first.clone()).await,
        replayed,
        "a parked replay answers as from the rows"
    );
    drain(pages.as_ref()).await;
    assert_eq!(
        raw.live(&every).await,
        held_by_others,
        "released leases leave no restored copy"
    );

    // A shared node: a restored lease on the first process holds it while
    // the second process's lease, its other live copy, ends and retires.
    let (_, kept_at, kept_root, kept_ids, kept_read) = &facts[*first_facts.last().unwrap()];
    let kept = pages
        .acquire_root_lease(reader(kept_at.clone(), *kept_root))
        .await
        .unwrap();
    let (_, other_at, other_root, other_ids, _) = &facts[second_facts[0]];
    let other = pages
        .acquire_root_lease(reader(other_at.clone(), *other_root))
        .await
        .unwrap();
    let shared: Vec<NodeId> = kept_ids
        .iter()
        .filter(|id| other_ids.contains(id) && !held_by_others.contains(id))
        .copied()
        .collect();
    pages.release_root_lease(other.id).await.unwrap();
    drain(pages.as_ref()).await;
    assert_eq!(
        &format!("{:?}", pages.read_pages(&kept, kept_ids).await),
        kept_read,
        "the restored lease reads the {} shared nodes after their other copy retired",
        shared.len()
    );

    // Prune: both leases read on, and renew.
    let pruned = core
        .processes()
        .prune(
            u64::MAX,
            None,
            lash::process::ProjectionWatermark::NoProjector,
        )
        .await
        .unwrap();
    assert_eq!(pruned.pruned_processes, 2);
    drain(pages.as_ref()).await;
    for (lease, ids, read) in [
        (&early, early_ids, early_read),
        (&kept, kept_ids, kept_read),
    ] {
        assert_eq!(
            &format!("{:?}", pages.read_pages(lease, ids).await),
            read,
            "a lease reads on after its process is pruned"
        );
        pages.renew_root_lease(lease).await.unwrap();
    }
    assert!(matches!(
        pages
            .acquire_root_lease(reader(kept_at.clone(), *kept_root))
            .await,
        Err(DurableError::Graph(GraphRefusal::RootRetired))
    ));
    pages.release_root_lease(early.id).await.unwrap();
    pages.release_root_lease(kept.id).await.unwrap();
    drain(pages.as_ref()).await;
    assert!(
        raw.live(&every).await.is_subset(&held_by_others),
        "the released leases leave nothing behind"
    );
    core.shutdown().await.unwrap();
}

on_every_tier!(a_packed_history_leases_as_its_rows: sqlite_memory, postgres);

/// One corruption of stored nodes: each named node's new encoding and bytes,
/// from its stored ones.
type Corruption = BTreeMap<NodeId, (i64, Vec<u8>)>;

/// The answers of the reads that walk `process`'s graphs: the aggregate
/// read of every fact and the hydration of fact `target`, each under a
/// spent and an unspent budget, and the reads of its tool calls and of
/// `model_owners`' model calls.
async fn walks(
    store: &dyn lash_durable::DurableStore,
    process: &ProcessId,
    target: RootOwner,
    facts: usize,
    model_owners: &[String],
) -> BTreeMap<String, String> {
    let owner = OwnerKey::Process(process.clone());
    let standard = GraphReadPolicy::standard().owner();
    let mut out = BTreeMap::new();
    let calls = ToolCallSelection::List {
        after: None,
        limit: 100,
    };
    out.insert(
        "calls".to_owned(),
        format!(
            "{:?}",
            store
                .tool_calls(&CallOwner::Process(process.clone()), &calls)
                .await
        ),
    );
    for model_owner in model_owners {
        out.insert(
            format!("models of {model_owner}"),
            format!(
                "{:?}",
                store
                    .model_calls(&model_session(process), model_owner)
                    .await
            ),
        );
    }
    for (label, limits) in [
        ("standard", standard),
        (
            "no nodes",
            DecodeLimits {
                nodes: 0,
                ..standard
            },
        ),
        (
            "one node",
            DecodeLimits {
                nodes: 1,
                ..standard
            },
        ),
        (
            "no bytes",
            DecodeLimits {
                total_bytes: 0,
                ..standard
            },
        ),
    ] {
        out.insert(
            format!("facts, {label}"),
            format!(
                "{:?}",
                store.run_facts_within(&owner, None, facts, limits).await
            ),
        );
        out.insert(
            format!("hydrate, {label}"),
            format!("{:?}", store.hydrate(target.clone(), None, limits).await),
        );
    }
    out
}

/// A corrupt history refuses the same from its archive as from its rows,
/// per store: the archive walk is the row walk with another node source,
/// so a corrupt node under a spent budget, an encoding no blob has, and
/// two corrupt nodes one walk meets in turn give the first refusal they
/// gave from the rows. The answer each store gave from its rows is the
/// oracle.
async fn a_corrupt_packed_history_refuses_as_its_rows(tier: Tier) {
    let (stores, raw, _keep) = raw_stores(tier).await;
    let (backend, core) = core_over(stores, 400);
    let process = Box::pin(run_one(&core, 2)).await;
    let durable = Arc::clone(backend.durable());
    let owner = OwnerKey::Process(process.clone());
    let limits = GraphReadPolicy::standard().owner();
    let rows = durable.run_facts(&owner).await.unwrap().rows;
    let mut graphs = Vec::new();
    for row in &rows {
        graphs.push(
            durable
                .hydrate(fact(&owner, row.run, row.ordinal), None, limits)
                .await
                .unwrap(),
        );
    }
    // The fact with the most graph nodes under its root: two corrupt nodes
    // under it are met in the walk's order.
    let graph_nodes = |graph: &lash_durable::domain::HydratedGraph| {
        graph
            .nodes
            .iter()
            .filter(|(id, node)| {
                **id != graph.root
                    && node.encoding == lash_core_store::checkpoint_graph::BlobEncoding::Graph
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>()
    };
    let target = (0..graphs.len())
        .max_by_key(|at| graph_nodes(&graphs[*at]).len())
        .unwrap();
    let below = graph_nodes(&graphs[target]);
    assert!(
        below.len() >= 2,
        "a fact reaches two graph nodes below its root: {below:?}"
    );
    let target_root = graphs[target].root;
    // Two children of one node under the target's root: the walk meets
    // them in its child order.
    let mut siblings = None;
    for id in graphs[target].nodes.keys() {
        let children: Vec<NodeId> = raw
            .children(id)
            .await
            .into_iter()
            .filter(|child| graphs[target].nodes.contains_key(child))
            .collect();
        if children.len() >= 2 {
            siblings = Some([children[0], children[1]]);
            break;
        }
    }
    let siblings = siblings.expect("a node under the target's root has two children");
    let target_owner = fact(&owner, rows[target].run, rows[target].ordinal);
    let flip = |node: &lash_durable::domain::HydratedNode| {
        let mut bytes = node.bytes.to_vec();
        *bytes.last_mut().unwrap() ^= 0x5a;
        bytes
    };
    let graph_encoding = |node: &lash_durable::domain::HydratedNode| match node.encoding {
        lash_core_store::checkpoint_graph::BlobEncoding::Graph => 1,
        lash_core_store::checkpoint_graph::BlobEncoding::Opaque => 0,
    };
    // Every node a read of the history reaches: its facts', its calls' and
    // its model calls' graphs.
    let mut every: BTreeMap<NodeId, lash_durable::domain::HydratedNode> = graphs
        .iter()
        .flat_map(|graph| graph.nodes.clone())
        .collect();
    let model_keys = raw.model_keys(&process).await;
    let mut model_owners: Vec<String> = model_keys.iter().map(|(run, _)| run.clone()).collect();
    model_owners.dedup();
    let mut roots: Vec<RootOwner> = model_keys
        .iter()
        .map(|(run, call)| {
            RootOwner::ModelCall(PromptCallKey {
                session: model_session(&process),
                call: ModelCallId::Owned {
                    owner: run.strip_prefix("owned:").unwrap().to_owned(),
                    key: call.clone(),
                },
            })
        })
        .collect();
    let calls = durable
        .tool_calls(
            &CallOwner::Process(process.clone()),
            &ToolCallSelection::List {
                after: None,
                limit: 100,
            },
        )
        .await
        .unwrap()
        .rows;
    for call in &calls {
        for part in [ToolCallRoot::Decision, ToolCallRoot::Answer] {
            roots.push(RootOwner::ToolCall {
                owner: call.execution.clone(),
                call: call.call.clone(),
                part,
            });
        }
    }
    for root in roots {
        if let Ok(graph) = durable.hydrate(root, None, limits).await {
            every.extend(graph.nodes);
        }
    }
    let corruptions: Vec<(&str, Corruption)> = vec![
        (
            "a corrupt root",
            BTreeMap::from([(target_root, {
                let node = &graphs[target].nodes[&target_root];
                (graph_encoding(node), flip(node))
            })]),
        ),
        (
            "corrupt nodes below the root",
            below
                .iter()
                .map(|id| {
                    let node = &graphs[target].nodes[id];
                    (*id, (graph_encoding(node), flip(node)))
                })
                .collect(),
        ),
        (
            // A sibling's bytes no longer match its address, or an opaque
            // one is read as a graph node.
            "two corrupt siblings",
            siblings
                .iter()
                .map(|id| {
                    let node = &graphs[target].nodes[id];
                    let bytes = match node.encoding {
                        lash_core_store::checkpoint_graph::BlobEncoding::Graph => flip(node),
                        lash_core_store::checkpoint_graph::BlobEncoding::Opaque => {
                            node.bytes.to_vec()
                        }
                    };
                    (*id, (1, bytes))
                })
                .collect(),
        ),
        (
            "an encoding no blob has",
            every
                .iter()
                .map(|(id, node)| (*id, (7, node.bytes.to_vec())))
                .collect(),
        ),
    ];

    let mut from_rows = Vec::new();
    for (label, corruption) in &corruptions {
        let mut stored = BTreeMap::new();
        for (id, (encoding, bytes)) in corruption {
            stored.insert(*id, raw.node(id).await);
            raw.set_node(id, *encoding, bytes).await;
        }
        from_rows.push(
            walks(
                durable.as_ref(),
                &process,
                target_owner.clone(),
                rows.len(),
                &model_owners,
            )
            .await,
        );
        for (id, (encoding, bytes)) in &stored {
            raw.set_node(id, *encoding, bytes).await;
        }
        assert_eq!(
            durable
                .hydrate(target_owner.clone(), None, limits)
                .await
                .unwrap(),
            graphs[target],
            "{label}: the rows are restored"
        );
    }

    assert_eq!(
        durable.pack_finished_histories(16).await.unwrap(),
        1,
        "one history packs"
    );
    assert_eq!(raw.rows(&process).await, 0, "the pack moved every row");
    let archive = raw.archive(&process).await;
    let contents = lash_durable::domain::process_archive::ProcessArchive::open(&archive)
        .unwrap()
        .contents()
        .unwrap();
    for ((label, corruption), rows_answers) in corruptions.iter().zip(&from_rows) {
        let mut corrupt = contents.clone();
        for node in &mut corrupt.nodes {
            if let Some((encoding, bytes)) = corruption.get(&node.id) {
                node.encoding = u8::try_from(*encoding).unwrap();
                node.bytes.clone_from(bytes);
            }
        }
        raw.set_archive(&process, &corrupt.encode().unwrap()).await;
        same(
            rows_answers,
            &walks(
                durable.as_ref(),
                &process,
                target_owner.clone(),
                rows.len(),
                &model_owners,
            )
            .await,
            label,
        );
        raw.set_archive(&process, &archive).await;
    }
    core.shutdown().await.unwrap();
}

on_every_tier!(a_corrupt_packed_history_refuses_as_its_rows: sqlite_memory, postgres);

/// Text the law writes into an archived call row's reason, then makes
/// invalid UTF-8 in the archive's bytes: a value no stored column holds.
const MALFORMED_CALL: &str = "\u{1}a malformed call reason\u{1}";
/// The same for a model-call row's enclosing owner; the law also sets that
/// row's `answered` flag, the byte before the enclosing owner's presence
/// tag and length, to 2 (the review's reproduction).
const MALFORMED_MODEL: &str = "\u{1}a malformed enclosing owner\u{1}";

/// `contents` encoded, with every malformed marker made invalid UTF-8 and
/// each malformed model row's `answered` flag set to 2.
fn malformed(contents: &lash_durable::domain::process_archive::ArchiveContents) -> Vec<u8> {
    let mut bytes = contents.encode().unwrap();
    let mut patched = 0;
    for (marker, answered) in [(MALFORMED_CALL, false), (MALFORMED_MODEL, true)] {
        let marker = marker.as_bytes();
        while let Some(at) = bytes
            .windows(marker.len())
            .position(|window| window == marker)
        {
            bytes[at..at + marker.len()].fill(0xff);
            if answered {
                // answered, then the presence tag and the u32 length.
                assert!(bytes[at - 6] <= 1, "the byte before is the answered flag");
                bytes[at - 6] = 2;
            }
            patched += 1;
        }
    }
    assert!(patched > 0, "the law corrupted the rows it added");
    bytes
}

/// A packed history's reads select rows on their stored keys and decode
/// only what they select. Malformed fields of rows a read does not select
/// (another owner's calls and model calls, saved reads' excluded calls, a
/// reservation a record read skips) leave every answer as the rows gave
/// it: an unknown id, a good selected sibling, a named call, a key read.
async fn a_packed_history_decodes_only_what_it_selects(tier: Tier) {
    use lash_durable::domain::process_archive::{ArchivedCall, ArchivedModelCall, ProcessArchive};
    let (stores, raw, _keep) = raw_stores(tier).await;
    let (backend, core) = core_over(stores, 400);
    let process = Box::pin(run_one(&core, 2)).await;
    let durable = Arc::clone(backend.durable());
    let model_keys = raw.model_keys(&process).await;
    let from_rows = answers(durable.as_ref(), &process, &model_keys).await;
    let calls = durable
        .tool_calls(
            &CallOwner::Process(process.clone()),
            &ToolCallSelection::List {
                after: None,
                limit: 100,
            },
        )
        .await
        .unwrap()
        .rows;
    assert_eq!(
        durable.pack_finished_histories(16).await.unwrap(),
        1,
        "one history packs"
    );
    let archive = raw.archive(&process).await;
    let contents = ProcessArchive::open(&archive).unwrap().contents().unwrap();
    let model = contents
        .models
        .iter()
        .find(|model| model.record_ref.is_some())
        .expect("a sealed model call")
        .clone();
    let call = contents.calls[0].clone();
    // Appended rows keep the stored order: last by admission sequence.
    let last = |call_id: &str, cancellable: bool, outcome: &str| ArchivedCall {
        admission_seq: i64::MAX,
        call_id: call_id.to_owned(),
        cancellable,
        answer_outcome: Some(outcome.to_owned()),
        reason: Some(MALFORMED_CALL.to_owned()),
        ..call.clone()
    };

    // Another owner's rows, under keys this process's reads name: no read
    // selects them.
    let mut others = contents.clone();
    others.calls.push(ArchivedCall {
        owner_key: "another owner".to_owned(),
        execution_owner: "another owner".to_owned(),
        ..last(&call.call_id, false, "\"Read\"")
    });
    others.models.push(ArchivedModelCall {
        run: "owned:another".to_owned(),
        enclosing_owner: Some(MALFORMED_MODEL.to_owned()),
        ..model.clone()
    });
    raw.set_archive(&process, &malformed(&others)).await;
    same(
        &from_rows,
        &answers_naming(durable.as_ref(), &process, &model_keys, Some(&calls)).await,
        "another owner's malformed rows",
    );

    // This process's own rows that some reads skip: a cancellable read and
    // an uncancellable write, which saved reads exclude, and a reservation,
    // which record reads exclude. Every read that does not select them
    // answers as the rows did; one that selects them refuses.
    let mut own = contents.clone();
    own.calls.push(last("zz-cancellable", true, "\"Read\""));
    own.calls.push(last("zz-written", false, "\"Write\""));
    own.models.push(ArchivedModelCall {
        call: "zz-reserved".to_owned(),
        call_id: "zz-reserved".to_owned(),
        attempt: Some(1),
        record_ref: None,
        ended_at_ms: None,
        written_epoch: None,
        enclosing_owner: Some(MALFORMED_MODEL.to_owned()),
        ..model.clone()
    });
    raw.set_archive(&process, &malformed(&own)).await;
    let selecting = [
        "calls".to_owned(),
        "calls after the first".to_owned(),
        format!("model pins of {}", model.run),
    ];
    let answered = answers_naming(durable.as_ref(), &process, &model_keys, Some(&calls)).await;
    for (label, answer) in &answered {
        if selecting.contains(label) {
            assert!(
                answer.starts_with("Err(") && answer.contains("Corrupt"),
                "`{label}` selects a malformed row and refuses: {answer}"
            );
        } else {
            assert_eq!(
                answer, &from_rows[label],
                "this process's skipped malformed rows: `{label}` answers the same"
            );
        }
    }
    raw.set_archive(&process, &archive).await;
    core.shutdown().await.unwrap();
}

on_every_tier!(a_packed_history_decodes_only_what_it_selects: sqlite_memory, postgres);

/// The packing transfer is one commit, cut on either side of it (item 27):
///
/// - a fault at its last write (the peel), after the archive is installed
///   and the rows deleted, leaves the history undecided with every row,
///   reading as before;
/// - a node that crashes and reopens the store then packs it;
/// - a pack whose acknowledgement is lost is retried by a reopened node
///   and skipped, the archive byte for byte as committed and the history
///   reading as before.
async fn a_pack_cut_at_its_commit_reads_as_its_rows(tier: Tier) {
    let (stores, raw, _keep) = raw_stores(tier).await;
    let (backend, core) = core_over(stores, 1024);
    let process = Box::pin(run_one(&core, 2)).await;
    let durable = Arc::clone(backend.durable());
    let model_keys = raw.model_keys(&process).await;
    let before = answers(durable.as_ref(), &process, &model_keys).await;
    let held = raw.rows(&process).await;

    raw.run(
        "CREATE TRIGGER cut BEFORE DELETE ON blobs BEGIN SELECT RAISE(ABORT, 'cut'); END",
        "CREATE FUNCTION cut() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'cut'; END $$; \
         CREATE TRIGGER cut BEFORE DELETE ON lash_blobs FOR EACH ROW EXECUTE FUNCTION cut()",
    )
    .await;
    assert!(
        durable.pack_finished_histories(16).await.is_err(),
        "the cut refuses the pack"
    );
    assert_eq!(
        raw.history(&process).await,
        Some((None, None)),
        "a pack cut before its commit leaves the history undecided"
    );
    assert_eq!(raw.rows(&process).await, held, "every row stays");
    same(
        &before,
        &answers(durable.as_ref(), &process, &model_keys).await,
        "after a pack cut before its commit",
    );
    raw.run(
        "DROP TRIGGER cut",
        "DROP TRIGGER cut ON lash_blobs; DROP FUNCTION cut()",
    )
    .await;
    core.shutdown().await.unwrap();
    drop((core, durable, backend));

    // The node comes back and packs.
    let (backend, core) = core_over(raw.reopen().await, 1024);
    let durable = Arc::clone(backend.durable());
    assert_eq!(durable.pack_finished_histories(16).await.unwrap(), 1);
    let packed = raw.archive(&process).await;
    assert_eq!(raw.rows(&process).await, 0, "the rows moved");
    same(
        &before,
        &answers_naming(durable.as_ref(), &process, &model_keys, None).await,
        "after the reopened node packed",
    );
    core.shutdown().await.unwrap();
    drop((core, durable, backend));

    // That pack's acknowledgement was lost: a reopened node retries it.
    let (backend, core) = core_over(raw.reopen().await, 1024);
    let durable = Arc::clone(backend.durable());
    assert_eq!(
        durable.pack_finished_histories(16).await.unwrap(),
        0,
        "the retry finds the history decided"
    );
    assert_eq!(
        raw.archive(&process).await,
        packed,
        "the archive is as committed"
    );
    same(
        &before,
        &answers_naming(durable.as_ref(), &process, &model_keys, None).await,
        "after the retried pack",
    );
    core.shutdown().await.unwrap();
}

on_every_tier!(a_pack_cut_at_its_commit_reads_as_its_rows: sqlite_memory, postgres);

/// The cap is inclusive (item 27): an archive of exactly
/// [`PROCESS_ARCHIVE_CAP_BYTES`] packs and reads as its rows, and one a
/// byte longer keeps every row. Three processes of one shape, the first
/// packed to measure it, the others padded by one call's reason, which the
/// archive stores with a fixed-width length.
async fn an_archive_at_the_cap_packs_and_one_byte_over_keeps_its_rows(tier: Tier) {
    let (stores, raw, _keep) = raw_stores(tier).await;
    let (backend, core) = core_over(stores, 1024);
    let durable = Arc::clone(backend.durable());
    let mut processes = Vec::new();
    for _ in 0..3 {
        let process = Box::pin(run_one(&core, 2)).await;
        raw.reason_of(&process, 1).await;
        processes.push(process);
    }
    let hold = "UPDATE {p}processes SET history = 'rows' WHERE process_id = {process}";
    let release = "UPDATE {p}processes SET history = NULL WHERE process_id = {process}";
    for process in &processes[1..] {
        raw.alter(process, &[hold]).await;
    }
    assert_eq!(durable.pack_finished_histories(16).await.unwrap(), 1);
    let measured = raw.archive(&processes[0]).await.len();
    let room = PROCESS_ARCHIVE_CAP_BYTES - measured;

    let at_cap = &processes[1];
    raw.reason_of(at_cap, 1 + room).await;
    raw.alter(at_cap, &[release]).await;
    let model_keys = raw.model_keys(at_cap).await;
    let before = answers(durable.as_ref(), at_cap, &model_keys).await;
    assert_eq!(
        durable.pack_finished_histories(16).await.unwrap(),
        1,
        "an archive at the cap packs"
    );
    assert_eq!(raw.archive(at_cap).await.len(), PROCESS_ARCHIVE_CAP_BYTES);
    assert_eq!(raw.rows(at_cap).await, 0, "the rows moved");
    same(
        &before,
        &answers(durable.as_ref(), at_cap, &model_keys).await,
        "an archive at the cap",
    );

    let over = &processes[2];
    raw.reason_of(over, 2 + room).await;
    raw.alter(over, &[release]).await;
    let model_keys = raw.model_keys(over).await;
    let before = answers(durable.as_ref(), over, &model_keys).await;
    let held = raw.rows(over).await;
    assert_eq!(
        durable.pack_finished_histories(16).await.unwrap(),
        0,
        "an archive a byte over the cap does not pack"
    );
    assert_eq!(
        raw.history(over).await,
        Some((Some("rows".to_owned()), None))
    );
    assert_eq!(raw.rows(over).await, held, "every row stays");
    same(
        &before,
        &answers(durable.as_ref(), over, &model_keys).await,
        "a byte over the cap",
    );
    core.shutdown().await.unwrap();
}

on_every_tier!(an_archive_at_the_cap_packs_and_one_byte_over_keeps_its_rows: sqlite_memory, postgres);

/// Every node process `process`'s run facts reach.
async fn reached(store: &dyn lash_durable::DurableStore, process: &ProcessId) -> BTreeSet<NodeId> {
    let limits = GraphReadPolicy::standard().owner();
    let owner = OwnerKey::Process(process.clone());
    let mut nodes = BTreeSet::new();
    for row in store.run_facts(&owner).await.unwrap().rows {
        let graph = store
            .hydrate(fact(&owner, row.run, row.ordinal), None, limits)
            .await
            .unwrap();
        nodes.extend(graph.nodes.into_keys());
    }
    nodes
}

/// Packing races a prune (item 27): both answer, the prune takes every
/// process it chose, packed or not, and a process the prune leaves reads
/// as its rows, though its nodes are shared with the pruned ones.
async fn a_pack_racing_a_prune_loses_nothing(tier: Tier) {
    let (stores, raw, _keep) = raw_stores(tier).await;
    let (backend, core) = core_over(stores, 1024);
    let durable = Arc::clone(backend.durable());
    let mut pruned = Vec::new();
    for _ in 0..4 {
        pruned.push(Box::pin(run_one(&core, 2)).await);
    }
    tokio::time::sleep(Duration::from_millis(5)).await;
    let cutoff = u64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let left = Box::pin(run_one(&core, 2)).await;
    let model_keys = raw.model_keys(&left).await;
    let before = answers(durable.as_ref(), &left, &model_keys).await;
    let shared: BTreeSet<NodeId> = reached(durable.as_ref(), &pruned[0])
        .await
        .intersection(&reached(durable.as_ref(), &left).await)
        .cloned()
        .collect();
    assert!(!shared.is_empty(), "the processes share nodes");

    let registry = core.processes();
    let (packed, report) = tokio::join!(
        durable.pack_finished_histories(16),
        registry.prune(
            cutoff,
            None,
            lash::process::ProjectionWatermark::NoProjector
        ),
    );
    let packed = packed.expect("the pack answers");
    let report = report.expect("the prune answers");
    assert!(
        (1..=5).contains(&packed),
        "the pack moved the kept process at least: {packed}"
    );
    assert_eq!(
        report.pruned_processes, 4,
        "the prune takes every process it chose"
    );
    for process in &pruned {
        assert_eq!(raw.history(process).await, None, "a pruned process is gone");
        assert_eq!(raw.rows(process).await, 0, "with every row");
    }
    assert_eq!(
        raw.history(&left).await.map(|(history, _)| history),
        Some(Some("archive".to_owned()))
    );
    same(
        &before,
        &answers(durable.as_ref(), &left, &model_keys).await,
        "the process the prune left",
    );
    assert_eq!(durable.pack_finished_histories(16).await.unwrap(), 0);
    core.shutdown().await.unwrap();
}

on_every_tier!(a_pack_racing_a_prune_loses_nothing: sqlite_memory, postgres);

/// A node two processes share stays stored while either holds it as rows
/// (item 27): packing one keeps the other's reads, the shared nodes stay
/// live, and packing the last holder retires them, both reading on.
async fn packing_a_shared_copy_keeps_it_for_its_other_holder(tier: Tier) {
    let (stores, raw, _keep) = raw_stores(tier).await;
    let (backend, core) = core_over(stores, 1024);
    let durable = Arc::clone(backend.durable());
    let first = Box::pin(run_one(&core, 2)).await;
    let second = Box::pin(run_one(&core, 2)).await;
    let keys = [raw.model_keys(&first).await, raw.model_keys(&second).await];
    let before = [
        answers(durable.as_ref(), &first, &keys[0]).await,
        answers(durable.as_ref(), &second, &keys[1]).await,
    ];
    let shared: BTreeSet<NodeId> = reached(durable.as_ref(), &first)
        .await
        .intersection(&reached(durable.as_ref(), &second).await)
        .cloned()
        .collect();
    assert!(!shared.is_empty(), "the processes share nodes");
    assert_eq!(raw.live(&shared).await, shared);

    raw.alter(
        &second,
        &["UPDATE {p}processes SET history = 'rows' WHERE process_id = {process}"],
    )
    .await;
    assert_eq!(durable.pack_finished_histories(16).await.unwrap(), 1);
    assert_eq!(
        raw.live(&shared).await,
        shared,
        "the other holder keeps the shared nodes"
    );
    same(
        &before[1],
        &answers(durable.as_ref(), &second, &keys[1]).await,
        "the other holder, its copy shared with a packed history",
    );
    same(
        &before[0],
        &answers(durable.as_ref(), &first, &keys[0]).await,
        "the packed history",
    );

    raw.alter(
        &second,
        &["UPDATE {p}processes SET history = NULL WHERE process_id = {process}"],
    )
    .await;
    assert_eq!(durable.pack_finished_histories(16).await.unwrap(), 1);
    assert!(
        raw.live(&shared).await.len() < shared.len(),
        "packing the last holder retires the copy no other root holds"
    );
    for (at, process) in [&first, &second].into_iter().enumerate() {
        same(
            &before[at],
            &answers(durable.as_ref(), process, &keys[at]).await,
            "both packed",
        );
    }
    core.shutdown().await.unwrap();
}

on_every_tier!(packing_a_shared_copy_keeps_it_for_its_other_holder: sqlite_memory, postgres);

/// Readers reading through a pack (item 27): every read made while the
/// histories move answers as their rows did, whichever side of the commit
/// it lands on.
async fn a_read_across_the_move_reads_as_its_rows(tier: Tier) {
    let (stores, raw, _keep) = raw_stores(tier).await;
    let (backend, core) = core_over(stores, 1024);
    let durable = Arc::clone(backend.durable());
    let mut held = Vec::new();
    for _ in 0..3 {
        let process = Box::pin(run_one(&core, 2)).await;
        let keys = raw.model_keys(&process).await;
        let before = answers(durable.as_ref(), &process, &keys).await;
        held.push((process, keys, before));
    }
    let moved = std::sync::atomic::AtomicBool::new(false);
    let reads = async {
        let mut passes = 0;
        loop {
            let last = moved.load(Ordering::SeqCst);
            for (process, keys, before) in &held {
                same(
                    before,
                    &answers(durable.as_ref(), process, keys).await,
                    "a read across the move",
                );
            }
            passes += 1;
            if last {
                break passes;
            }
        }
    };
    let pack = async {
        let packed = durable.pack_finished_histories(16).await.unwrap();
        moved.store(true, Ordering::SeqCst);
        packed
    };
    let (passes, packed) = tokio::join!(reads, pack);
    assert_eq!(packed, 3);
    assert!(passes >= 2, "reads ran through the move");
    core.shutdown().await.unwrap();
}

on_every_tier!(a_read_across_the_move_reads_as_its_rows: sqlite_memory, postgres);
