//! FIG-7694 gate tooling, never committed: the base build writes a SQLite
//! store holding a parked actor, a session root with bindings, a saved
//! function and a started `Once` tool row with no outcome. The branch build
//! opens the store in its own gate.
//!
//! `LASH_BASE_SEED=<dir>` names the store's directory. The `Once` body ends
//! the OS process, so the row stays started with no outcome.
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

#[path = "support/operations.rs"]
#[allow(dead_code)]
mod operations;
#[path = "support/packages.rs"]
#[allow(dead_code)]
mod packages;
#[path = "support/served.rs"]
#[allow(dead_code)]
mod served;
#[path = "support/sim.rs"]
#[allow(dead_code)]
mod sim;

use std::sync::Arc;

use served::{Tier, World};

const SESSION: &str = "base-seed";
const ONCE: &str = "seed_once";
const ONCE_OPERATION: &str = "seed.seed_once";

fn once_definition() -> lash_vocabulary::tool_contract::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    lash_vocabulary::tool_contract::ToolDefinition::raw(
        format!("tool:{ONCE}"),
        ONCE_OPERATION,
        "Writes its entry marker, then ends the OS process.",
        object.clone(),
        object,
    )
    .unwrap()
    .with_execution(std::time::Duration::from_secs(120))
    .with_execution_policy(lash_kernel_value::tool_contract::ExecutionPolicy::Once)
}

struct Once {
    dir: std::path::PathBuf,
}

#[async_trait::async_trait]
impl lash_core::ToolProvider for Once {
    fn tool_manifests(
        &self,
        _: &lash_core::OwnerConfigView,
    ) -> Result<Vec<lash_vocabulary::tool_contract::ToolManifest>, lash_execution_contracts::PluginError>
    {
        Ok(vec![once_definition().manifest()])
    }
    fn resolve_contract(
        &self,
        name: &str,
        _: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        (name == ONCE_OPERATION).then(|| Arc::new(once_definition().contract()))
    }
    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        std::fs::write(
            self.dir.join("once-entered.json"),
            serde_json::to_vec(&serde_json::json!({
                "call": call.context.call_id().to_string(),
                "pid": std::process::id(),
            }))
            .unwrap(),
        )
        .unwrap();
        // The body is running and its outcome never commits.
        std::process::exit(0);
    }
}

fn builder(dir: std::path::PathBuf) -> impl FnOnce(&lash::Backend) -> lash::LashCoreBuilder {
    move |backend| {
        lash::LashCore::rlm_builder(
            backend.clone(),
            served::rlm(backend, sim::untimed_workers()),
        )
        .plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                lash_execution_contracts::process::lifetime::session_or_starter,
                packages::run_policy_naming(&[]),
            ),
        ))
        .plugin(Arc::new(lash::plugins::StaticPluginFactory::new(
            lash::plugins::PluginDeclaration::initial("seed"),
            lash::plugins::PluginSpec::new()
                .with_tool_provider(lash::tool_provider_key!("seed"), Arc::new(Once { dir }))
                .with_package_provider(
                    lash::tool_provider_key!("operations"),
                    Arc::new(operations::Packages {
                        provider: lash_vocabulary::providers::ToolProviderHandle::new(
                            lash_vocabulary::claims::RegistryKind::Session,
                            "seed",
                            lash::tool_provider_key!("seed"),
                        ),
                        definitions: vec![once_definition()],
                    }),
                ),
        )))
    }
}

async fn session(world: &World) -> lash::DurableSession {
    world
        .core
        .session(lash_kernel_value::identity::SessionId::try_from(SESSION.to_owned()).unwrap())
        .durable()
        .await
        .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn base_build_writes_the_cutover_gate_store() {
    let Ok(dir) = std::env::var("LASH_BASE_SEED") else {
        eprintln!("skipping: LASH_BASE_SEED is not set");
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).unwrap();
    let stores = lash_sqlite_store::SqliteStoreSet::open(
        dir.join("lash.db"),
        lash_sqlite_store::SqliteSynchronous::Full,
    )
    .await
    .unwrap();
    let world = World::with_stores(
        Tier::SqliteFile,
        Arc::new(stores),
        Vec::new(),
        builder(dir.clone()),
    )
    .await;
    world.session(SESSION, served::spec(64)).await;

    // A session root with bindings, a saved function, and a kernel process
    // parked on a timer an hour out.
    world.script(
        "seed-cell-1",
        vec![served::cell(
            r#"let answer = 41;
const box = { n: 1 };
const reader = () => answer + box.n;
const worker = async (): Promise<null> => { await sleep(3600000); return null; };
const handle = await processes.start({ definition: worker });"#,
        )],
    );
    let first = world.send(&session(&world).await, "seed-cell-1").await;
    served::assert_answered("seed cell 1", &first);

    // The parked process's wait is committed before the Once starts.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let parked = loop {
        let records = lash_core::testing::process_roster_records_for_fixture(
            world.backend.process_registry().as_ref(),
            &lash_execution_contracts::process::ProcessListFilter {
                status: lash::process::ProcessStatusFilter::Any,
                ..Default::default()
            },
        )
        .await
        .unwrap();
        if let Some(record) = records.iter().find(|record| !record.waits().is_empty()) {
            break record.id.clone();
        }
        assert!(std::time::Instant::now() < deadline, "no process parked");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    };
    std::fs::write(
        dir.join("seed.json"),
        serde_json::to_vec(&serde_json::json!({
            "session": SESSION,
            "parked_process": parked.as_str(),
            "pid": std::process::id(),
        }))
        .unwrap(),
    )
    .unwrap();

    // A started Once with no outcome: its body ends this process.
    world.script(
        "seed-cell-2",
        vec![served::cell(r#"const once = await seed.seedOnce({ label: "base" });"#)],
    );
    let second = world.send(&session(&world).await, "seed-cell-2").await;
    panic!("the Once body ends the process before its turn answers: {second:?}");
}
