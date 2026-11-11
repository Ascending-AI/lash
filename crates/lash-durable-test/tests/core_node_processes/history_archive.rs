//! A finished process's history archive (FIG-7701).
//!
//! A real workflow process (two tool calls and one model query) runs to its
//! end. Every reader of its history (run facts, tool calls, model calls and
//! root hydration, with their selectors, unknown keys and refusals) answers
//! the same before and after the pack moves the rows into the process's
//! archive. A history over the cap keeps its rows. A corrupt node refuses
//! only the reads that reach it. Prune removes the archive with its process.

use super::*;
use std::collections::BTreeMap;

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
    let calls = listed.map(|calls| calls.rows).unwrap_or_default();
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
    let process = run_one(&core, 2).await;
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
    let process = run_one(&core, 6).await;
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
