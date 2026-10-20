//! The two-build claim law of a kernel migration (FIG-5716; kernel spec §6
//! "Upgrades", ADR 0106 §1, ADR 0115 §6).
//!
//! One kernel process runs over one SQLite store set. A core of build N,
//! whose engine writes kernel version 1, starts it and parks it in a
//! `sleep` inside a loop; the core drains. A core of build N+1, the
//! synthetic successor, claims the parked process: no node of build N is
//! live, so it carries the document and the parked run to its own kernel
//! version as its first commit, and the process resumes under that
//! version to the answer it would have reached.
//!
//! The refusal laws beside it: a process and a session cell pin helper
//! release 1.0, whose manifest cannot supply the next kernel encoding.
//! The survey names that release with a typed refusal, and build N+1 parks
//! the run without publishing a rewritten document or cell snapshot.
//!
#![cfg(all(feature = "synthetic-next", feature = "rlm", feature = "sqlite"))]
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use lash::workflow::document::KernelVersion;
use lash_core::durable_port::{ActorKey, ActorState};
use lash_execution_contracts::backend::StoreSet;

/// `worker()`: a loop that prints, sleeps and goes on. The synthetic
/// successor spells `print` otherwise and prices the loop's test
/// otherwise, so both of its differences lie across the park.
const WORKER: &str = r#"kernel 1
numbers float
entry worker() -> Any

fn worker() {
  let items = ["first", "second"]
  let last = "none"
  for item in items {
    print item
    do sleep 1500
    set last = item
  }
  return last
}

main {
  finish null
}
"#;

const KIND: &str = lash_vm_runtime::LASH_VM_ENGINE_KIND;

/// The model profile a session of these laws runs under.
const MODEL: &str = "kernel-two-build";

/// How a format set spells the kernel engine's state under `kernel`.
fn engine_format(kernel: KernelVersion) -> String {
    format!("engine/{KIND}@{}", kernel.number())
}

/// Which build a core of these laws is.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Build {
    /// Build N, whose engine writes kernel version 1.
    N,
    /// Build N+1, the synthetic successor, which carries build N's state
    /// forward.
    NPlus1,
    /// The build after N+1, which closes the window N+1 opened: it retires
    /// build N's formats and carries nothing of them forward.
    Closing,
}

/// A core of `build` over `stores`, serving a model that answers each call
/// with the next of `cells`, a TypeScript cell.
fn serving(
    stores: &Arc<dyn StoreSet>,
    build: Build,
    boot: &str,
    cells: &[&str],
) -> (lash::Backend, lash::LashCore) {
    serving_under(
        stores,
        build,
        boot,
        cells,
        lash::durable::DurableSettings::standard(),
        Vec::new(),
        None,
    )
}

/// [`serving`], its node letting go of a session with nothing to do soon,
/// so a law can leave one idle, which no node claims until something wakes
/// it.
fn idling(
    stores: &Arc<dyn StoreSet>,
    build: Build,
    boot: &str,
    cells: &[&str],
) -> (lash::Backend, lash::LashCore) {
    let mut settings = lash::durable::DurableSettings::standard();
    settings.idle_evict = Duration::from_millis(200);
    serving_under(stores, build, boot, cells, settings, Vec::new(), None)
}

/// A start policy that counts its calls and states nothing: a law that
/// installs it shows how often a start's environment was chosen.
struct CountingPolicy(Arc<std::sync::atomic::AtomicUsize>);

#[async_trait::async_trait]
impl lash::process::ProcessEnvPolicy for CountingPolicy {
    async fn choose(
        &self,
        _cx: &lash::process::ProcessEnvCx<'_>,
    ) -> Result<lash::process::ProcessStartChoice, lash::process::ProcessEnvPolicyError> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        Err(lash::process::ProcessEnvPolicyError::Refused(
            lash::process::ProcessEnvRefusal::Policy {
                reason: "the counting policy states no environment".to_owned(),
            },
        ))
    }
}

/// A process component a core of these laws installs, and its key.
type Component = (
    Arc<dyn lash::process::ProcessComponentFactory>,
    lash::process::ProcessConfigKey<()>,
);

/// [`serving`] under the durable `settings`, installing `components`, and
/// process controls under a policy counting into `policy` when it is given.
fn serving_under(
    stores: &Arc<dyn StoreSet>,
    build: Build,
    boot: &str,
    cells: &[&str],
    settings: lash::durable::DurableSettings,
    components: Vec<Component>,
    policy: Option<Arc<std::sync::atomic::AtomicUsize>>,
) -> (lash::Backend, lash::LashCore) {
    let kernel_one = matches!(build, Build::N);
    let replies = Arc::new(std::sync::Mutex::new(
        cells
            .iter()
            .map(|cell| format!("<typescript>\n{cell}\n</typescript>"))
            .collect::<std::collections::VecDeque<_>>(),
    ));
    let provider = lash::testing::TestProvider::builder()
        .kind(MODEL)
        .complete(move |_request| {
            let replies = Arc::clone(&replies);
            async move {
                let text = replies
                    .lock()
                    .expect("the scripted replies")
                    .pop_front()
                    .expect("the model was called more often than the law scripts");
                Ok(lash::provider::LlmResponse {
                    parts: vec![lash::direct::LlmOutputPart::Text {
                        text,
                        response_meta: None,
                    }],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    let builder = lash::durable::DurableBackendBuilder::new(Arc::clone(stores)).config(settings);
    let builder = match build {
        Build::N => builder.previous_build(),
        Build::NPlus1 => builder,
        Build::Closing => builder.closing_window(),
    };
    let backend = builder.build().expect("the backend assembles");
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        lash::rlm::CellDialect::typescript(),
    )
    .with_worker_service(lash::vm::WorkerService::default());
    let factory = if kernel_one {
        factory.writing_kernel(KernelVersion::One)
    } else {
        factory
    };
    let builder = components.into_iter().fold(
        lash::LashCore::rlm_builder(backend.clone(), factory),
        |builder, (component, key)| builder.process_component(component, key),
    );
    let builder = match policy {
        Some(calls) => builder.plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                lash::process::lifetime::session_or_starter,
                Arc::new(CountingPolicy(calls)),
            ),
        )),
        None => builder,
    };
    let core = builder
        .serve_test_llm_profile(
            provider,
            lash::LlmProfileMetadata::builder(MODEL)
                .cache_retention(lash::provider::CacheRetention::Short)
                .context_window_tokens(64_000)
                .build()
                .expect("the model's profile"),
        )
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new(boot),
            lash::persistence::LeaseIncarnationId::new(format!("{boot}-boot")),
        ))
        .expect("the core builds");
    (backend, core)
}

/// The environment a kernel process of these laws runs under: no tools and
/// no components.
fn environment() -> lash::process::ProcessEnv {
    lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(16))
        .build()
        .expect("the environment builds")
}

/// Publishes the worker document under a host pin, and answers the start
/// payload of its `worker` entry. Its `use` lines name only what its own
/// code calls; its manifest lists every function that code reaches,
/// through helper bodies too (`K-DOC-002`): a parked call or closure in a
/// helper's body resumes only in a body its document lists.
async fn payload(backend: &lash::Backend, text: &str) -> serde_json::Value {
    let mut document = lash::workflow::document::parse_document(text).expect("the document parses");
    let functions = lash::package::lash_packages()
        .map(|packages| std::sync::Arc::clone(packages.functions()))
        .expect("the shipped library assembles");
    document.manifest.functions =
        lash::workflow::graph::requirements(&document, functions.as_ref()).functions;
    lash::workflow::graph::admit(
        &document,
        &lash::workflow::graph::Environment::new(functions.as_ref()),
    )
    .expect("the document is admitted");
    let declarations =
        lash::workflow::document::CheckedDeclarations::required(&document, functions.as_ref())
            .expect("the document's helpers are shipped");
    let identity = lash_vm_runtime::KernelDocuments::new(backend.module_artifacts())
        .publish(
            &lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                lash_core::HostArtifactPin::mint(),
            ))
            .expect("a host pin is unguarded"),
            &lash::workflow::document::IdentifiedDocument::new(document.clone()),
            &declarations,
        )
        .await
        .expect("the document publishes");
    serde_json::to_value(
        lash_vm_runtime::KernelProcessDefinition::of(
            &document,
            identity,
            lash::workflow::document::Name::new("worker"),
            None,
        )
        .start(serde_json::Map::new()),
    )
    .expect("the input encodes")
}

async fn start(core: &lash::LashCore, payload: serde_json::Value) -> lash_core::ProcessId {
    start_under(core, &environment(), payload).await
}

/// [`start`] under `env`, with the kernel engine's default settings.
async fn start_under(
    core: &lash::LashCore,
    env: &lash::process::ProcessEnv,
    payload: serde_json::Value,
) -> lash_core::ProcessId {
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&lash_core::HostArtifactPin::mint(), env)
        .await
        .expect("the environment is published");
    let settings = core
        .processes()
        .engine_defaults(KIND, &payload)
        .expect("the kernel engine's default settings");
    let request = lash_execution_contracts::process::ProcessStartRequest::engine(
        KIND.to_owned(),
        payload,
        lash_execution_contracts::process::EngineLaunch::new(env_ref, settings),
        lash_execution_contracts::process::ProcessOriginator::host(),
        lash_execution_contracts::process::LifetimeDecision::Detached,
    );
    core.processes()
        .start(request, core.effect_host())
        .await
        .expect("the process starts")
        .process_id
}

/// The row of `actor` once its node let go of it to wait.
async fn waiting(
    backend: &lash::Backend,
    actor: &ActorKey,
) -> lash_core::durable_port::ActorSnapshot {
    in_state(backend, actor, ActorState::Waiting).await
}

async fn in_state(
    backend: &lash::Backend,
    actor: &ActorKey,
    state: ActorState,
) -> lash_core::durable_port::ActorSnapshot {
    let mut last = None;
    for _ in 0..3000 {
        last = backend
            .durable()
            .actor(actor)
            .await
            .expect("the actor row is read");
        if let Some(row) = last.clone().filter(|row| row.state == state) {
            return row;
        }

        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the process did not become {state:?} within half a minute: {last:?}")
}

/// The actor row of `session` once the cell of its open turn has parked:
/// the cell's snapshot is committed. The session's revision standing still
/// says nothing of a park: the turn holds the model response that asked for
/// the cell until the cell's first save (FIG-6000), so a cell that is still
/// lowering has committed nothing since the model call started.
async fn parked_cell(
    backend: &lash::Backend,
    session: &str,
) -> lash_core::durable_port::ActorSnapshot {
    let id = lash::SessionId::parse(session).expect("a session id");
    let actor = ActorKey::session(session).expect("a session actor key");
    let reads = backend.durable();
    let mut last = None;
    for _ in 0..3000 {
        last = reads.turn(&id).await.expect("the turn row is read");
        if let Some(turn) = &last
            && !reads
                .cell_roots(&id, &turn.run)
                .await
                .expect("the cell roots are read")
                .is_empty()
        {
            return reads
                .actor(&actor)
                .await
                .expect("the actor row is read")
                .expect("the session has its actor");
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!(
        "the cell did not park within half a minute: {last:?}, {:?}",
        reads.actor(&actor).await
    )
}

/// What `lashctl kernel-migration list` prints for `backend`.
async fn survey(backend: &lash::Backend) -> serde_json::Value {
    let functions = lash::package::lash_packages()
        .map(|packages| std::sync::Arc::clone(packages.functions()))
        .expect("the shipped library assembles");
    let survey = lash::vm::survey_kernel_migration(backend, &functions)
        .await
        .expect("the survey reads the store");
    serde_json::to_value(&survey).expect("the survey encodes")
}

/// A run parked by build N is never carried: no migrator carries its
/// heap pages. Build N+1 interprets the version it parked under and holds
/// every function its document reaches, so it claims the process and
/// resumes the run in that version, to its end (FIG-6193).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_parked_by_build_n_is_resumed_by_build_n_plus_1_in_the_version_it_parked_under() {
    let stores: Arc<dyn StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("an in-memory store set opens"),
    );
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = |build, boot| {
        serving_under(
            &stores,
            build,
            boot,
            &[],
            lash::durable::DurableSettings::standard(),
            Vec::new(),
            Some(Arc::clone(&calls)),
        )
    };
    let (old_backend, old) = counted(Build::N, "build-n");
    let payload = payload(&old_backend, WORKER).await;
    let process = start(&old, payload).await;
    let actor = ActorKey::process(process.as_str()).expect("a process actor key");
    let launched = launch_of(&old_backend, &process).await;

    // Build N parks the process in its first sleep and lets go of it.
    let parked = waiting(&old_backend, &actor).await;
    let written = parked.formats.clone();
    assert!(
        written
            .as_str()
            .contains(&engine_format(KernelVersion::One)),
        "the parked process is in build N's format set: {written:?}"
    );
    old.drain().await.expect("build N's node drains");
    let survey = survey(&old_backend).await;
    assert_eq!(survey["checked"], 1, "{survey}");
    assert_eq!(survey["unmigrated"], 1, "{survey}");
    assert_eq!(survey["refused"], serde_json::json!([]), "{survey}");

    let (new_backend, new) = counted(Build::NPlus1, "build-n-plus-1");
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        new.processes().await_output(&process),
    )
    .await
    .expect("the process ends within a minute")
    .expect("the process's end is read");
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process ended without an answer: {output:?}");
    };
    assert!(output.is_success(), "the process failed: {output:?}");
    assert_eq!(output.value_for_projection(), serde_json::json!("second"));
    // Adoption carries the process's launch as it was declared: its
    // environment, settings and components, and no policy chooses again.
    assert_eq!(launch_of(&new_backend, &process).await, launched);
    assert_eq!(
        calls.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no start policy ran"
    );

    let ended = new_backend
        .durable()
        .actor(&actor)
        .await
        .expect("the actor row is read")
        .expect("the process has its actor");
    assert!(
        ended
            .formats
            .as_str()
            .contains(&engine_format(KernelVersion::One))
            && !ended
                .formats
                .as_str()
                .contains(&engine_format(KernelVersion::SyntheticNext)),
        "the process ended in the version it parked under, never carried: {:?}",
        ended.formats
    );
}

/// What `process` was launched under: its environment's reference, its
/// engine settings, and the components its environment names.
async fn launch_of(
    backend: &lash::Backend,
    process: &lash_core::ProcessId,
) -> (
    Option<lash_core::ProcessEnvRef>,
    Option<lash::process::EngineSettings>,
    Vec<String>,
) {
    let record = backend
        .process_registry()
        .get_process(process)
        .await
        .expect("the process is read")
        .expect("the process is registered");
    let env_ref = record
        .env_ref()
        .cloned()
        .expect("a kernel process has an environment");
    let bytes = backend
        .stores()
        .process_env_store()
        .get_process_env(&env_ref)
        .await
        .expect("the environment store reads")
        .expect("the environment is stored");
    let env = lash::process::ProcessEnv::from_store_bytes(&bytes).expect("the environment decodes");
    (
        Some(env_ref),
        record.engine_settings().cloned(),
        env.components()
            .iter()
            .map(|entry| entry.id().to_string())
            .collect(),
    )
}

/// The session `id` of `core`, created if it is not there.
async fn session(core: &lash::LashCore, id: &str) -> lash::LashSession {
    let id = lash::SessionId::parse(id).expect("a session id");
    match core
        .session(id.clone())
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            MODEL,
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(1024),
            lash::NoProgressBudget::bounded(12),
        )))
        .await
    {
        Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
        Err(error) => panic!("the session is created: {error:?}"),
    }
    core.session(id).open().await.expect("the session opens")
}

/// What each cell of the session `output` ends in finished with, oldest
/// first.
fn cell_finishes(view: &lash::persistence::SessionReadView) -> Vec<serde_json::Value> {
    view.to_snapshot()
        .session_graph
        .nodes
        .iter()
        .filter_map(|node| match &node.payload {
            lash_core::SessionNodePayload::Event {
                event: lash_core::SessionHistoryRecord::Protocol(event),
            } if event.plugin_id == "rlm_protocol" => event
                .payload
                .get("event")?
                .get("RlmTrajectoryEntry")?
                .get("result")
                .filter(|result| result["kind"] == "controlled")?
                .get("value")?
                .get("control")
                .filter(|control| control["type"] == "finish")?
                .get("value")?
                .get("inline")
                .cloned(),
            _ => None,
        })
        .collect()
}

/// The two-build claim law of a session (kernel spec §6), its refusal: a
/// turn's cell of build N parks in a `sleep` and build N dies. Build N+1
/// claims the session, which is in build N's format set: no migrator
/// carries the parked cell's heap pages to its kernel version (FIG-6193),
/// so the session parks typed, in build N's format set, its cell's roots
/// as build N left them.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_cell_parked_by_build_n_is_carried_onto_no_kernel_version_and_parks_typed() {
    const SESSION: &str = "kernel-two-build-session";
    let stores: Arc<dyn StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("a SQLite memory store set opens"),
    );
    let actor = ActorKey::session(SESSION).expect("a session actor key");
    let build_n = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build N's runtime");
    let (backend, written) = build_n
        .spawn({
            let stores = Arc::clone(&stores);
            async move {
                let (old_backend, old) = serving(
                    &stores,
                    Build::N,
                    "build-n",
                    &["await sleep(60000);\nawait control.finish(21 * 2);"],
                );
                let opened = session(&old, SESSION).await;
                tokio::spawn(async move {
                    let _ = opened
                        .send(lash::TurnInput::text("sleep, then multiply"))
                        .output()
                        .await;
                });
                let parked = parked_cell(&old_backend, SESSION).await;
                std::mem::forget(old);
                (old_backend, parked.formats)
            }
        })
        .await
        .expect("build N parks the cell");
    assert!(
        written.as_str().contains("kernel-parked-state@1"),
        "{written:?}"
    );
    build_n.shutdown_background();

    let listed = survey(&backend).await;
    let refusal = &listed["sessions"][0]["refused"][0]["refusal"];
    assert_eq!(refusal["refused"], "parked", "{listed}");
    assert_eq!(
        refusal["refusal"]["reason"], "state_not_carried",
        "{listed}"
    );
    let session = lash::SessionId::fixture(SESSION);
    let turn = backend
        .durable()
        .turn(&session)
        .await
        .expect("turn read")
        .expect("an open turn");
    let roots = backend
        .durable()
        .cell_roots(&session, &turn.run)
        .await
        .expect("the original roots");

    let (new_backend, _new) = serving(&stores, Build::NPlus1, "build-n-plus-1", &[]);
    let parked = in_state(&new_backend, &actor, ActorState::Parked).await;
    let reason: serde_json::Value =
        serde_json::from_str(parked.park.as_deref().expect("the actor names its refusal"))
            .expect("a JSON park reason");
    assert_eq!(reason["reason"], "undecodable_state", "{reason}");
    assert!(
        reason.to_string().contains("carried by no migrator"),
        "{reason}"
    );
    assert_eq!(parked.formats, written);
    assert_eq!(
        new_backend
            .durable()
            .cell_roots(&session, &turn.run)
            .await
            .expect("the parked roots"),
        roots,
        "a refused carry publishes no rewritten cell",
    );
}

/// The row of `actor` once its format set is no longer `written`.
async fn carried_from(
    backend: &lash::Backend,
    actor: &ActorKey,
    written: &lash_core::durable_port::FormatSet,
) -> lash_core::durable_port::ActorSnapshot {
    let mut last = None;
    for _ in 0..6000 {
        last = backend
            .durable()
            .actor(actor)
            .await
            .expect("the actor row is read");
        if let Some(row) = last.clone().filter(|row| row.formats != *written) {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("{actor} was not carried out of {written:?} within a minute: {last:?}")
}

/// Session `id` of `core`, which binds a function in one turn and then has
/// nothing to do: its node lets go of it, and its row is idle in the format
/// set `core`'s build writes, which it answers.
async fn idle_session(
    backend: &lash::Backend,
    core: &lash::LashCore,
    id: &str,
) -> lash_core::durable_port::FormatSet {
    let bound = session(core, id)
        .await
        .send(lash::TurnInput::text("bind the function"))
        .output()
        .await
        .expect("the binding turn");
    assert!(bound.is_success(), "the binding turn: {bound:?}");
    let actor = ActorKey::session(id).expect("a session actor key");
    in_state(backend, &actor, ActorState::Idle).await.formats
}

/// The binding turn of [`idle_session`].
const BIND: &str = "function double(n: number) { return n * 2; }\nawait control.finish('bound');";

/// FIG-5787 (ADR 0115 §3.5, drain by release): the build that closes the
/// window does not start while a session is still in build N's formats,
/// and its refusal names how many and the command that carries them; once
/// the sweep has a node of build N+1 carry the session, it starts and runs
/// the session's turn.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_closing_build_refuses_to_start_while_a_session_is_unmigrated_and_starts_after_the_sweep()
 {
    const SESSION: &str = "kernel-gate-session";
    let stores: Arc<dyn StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("an in-memory store set opens"),
    );
    let session_actor = ActorKey::session(SESSION).expect("a session actor key");
    let (old_backend, old) = idling(&stores, Build::N, "build-n", &[BIND]);
    let written = idle_session(&old_backend, &old, SESSION).await;
    old.drain().await.expect("build N's node drains");

    let (_refusing_backend, refusing) = serving(&stores, Build::Closing, "build-closing", &[]);
    let stopped = tokio::time::timeout(Duration::from_secs(60), refusing.node_stopped())
        .await
        .expect("the closing build's node stops within a minute");
    match stopped {
        Some(Err(lash::durable::DurableError::Unmigrated {
            unmigrated,
            command,
        })) => {
            assert_eq!(unmigrated, 1);
            assert_eq!(command, lash::vm::KERNEL_MIGRATION_SWEEP);
        }
        other => panic!("the closing build started over an unmigrated session: {other:?}"),
    }
    drop(refusing);

    let (new_backend, new) = serving(&stores, Build::NPlus1, "build-n-plus-1", &[]);
    lash::vm::sweep_kernel_migration(&new_backend)
        .await
        .expect("the sweep wakes the session");
    carried_from(&new_backend, &session_actor, &written).await;
    new.drain().await.expect("build N+1's node drains");

    let (_closing_backend, closing) = serving(
        &stores,
        Build::Closing,
        "build-closing",
        &["await control.finish(double(21));"],
    );
    let read = tokio::time::timeout(
        Duration::from_secs(60),
        session(&closing, SESSION)
            .await
            .send(lash::TurnInput::text("call it"))
            .output(),
    )
    .await
    .expect("the closing build's turn ends within a minute")
    .expect("the closing build's turn");
    assert!(read.is_success(), "the closing build's turn: {read:?}");
    assert_eq!(
        cell_finishes(
            &closing
                .session(lash::SessionId::fixture(SESSION))
                .durable()
                .await
                .expect("durable session")
                .read()
                .await
                .expect("committed read")
                .expect("head")
        )
        .last(),
        Some(&serde_json::json!(42))
    );
}

// --- NO-CLAIM-WITHOUT-COMPONENT (FIG-6034, D3) -----------------------------

/// The component a held kernel process names, and the tool it provides.
const HOLD: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("kernel-claim-hold", lash::plugins::FormatVersion::ONE);
const HOLD_TOOL: &str = "kernel_claim_hold";

/// `worker()` performs the hold tool once and answers what it answered.
const HOLDING_WORKER: &str = r#"kernel 1
numbers float
effect hold.run(input: Any) -> Any contract "1"
entry worker() -> Any

fn worker() {
  let request = {n: 1}
  let got = perform hold.run(request) as Any
  return got
}

main {
  finish null
}
"#;

fn hold_definition() -> lash::tools::ToolDefinition {
    // It answers the text `"released"`: its output schema admits that
    // answer at settle (FIG-6330).
    lash::tools::ToolDefinition::raw(
        HOLD_TOOL,
        "hold.run",
        "Holds, then answers.",
        serde_json::json!({ "type": "object", "additionalProperties": true }),
        serde_json::json!({ "type": "string" }),
    )
    .expect("the hold tool's schemas")
    // The contract the laws' kernel texts pin.
    .declared_contract(lash_core::ContractId::new("1"))
    .with_execution(Duration::from_secs(120))
    .with_execution_policy(lash_core::ExecutionPolicy::repeatable(
        std::num::NonZeroU32::new(4).unwrap(),
        1,
        1,
    ))
}

/// The hold tool as a later build serves it: its schemas unchanged, under
/// the contract `contract` it declares.
fn hold_definition_under(contract: &str) -> lash::tools::ToolDefinition {
    let mut definition = hold_definition();
    definition.manifest.contract = lash_core::ContractSource::Declared {
        id: lash_core::ContractId::new(contract),
    };
    definition
}

/// The hold tool's deliveries, per process run. It outlives every build.
#[derive(Default)]
struct Holds {
    deliveries: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::Notify,
    /// The call and tool of each delivery, in order.
    calls: std::sync::Mutex<Vec<(String, String)>>,
}

/// The hold tool's body: it holds forever on a build that `holds`, and
/// answers `"released"` on one that does not.
struct HoldBody {
    holds: bool,
    seen: Arc<Holds>,
}

#[async_trait::async_trait]
impl lash::tools::StaticToolExecute for HoldBody {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        self.seen.calls.lock().expect("the deliveries").push((
            call.context.call_id().to_string(),
            call.tool_id().to_string(),
        ));
        self.seen
            .deliveries
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self.holds {
            self.seen.entered.notify_one();
            std::future::pending::<()>().await;
        }
        lash::tools::ToolOutcome::ok(serde_json::json!("released")).into()
    }
}

/// The hold component of this build.
struct HoldComponent {
    body: Arc<dyn lash::tools::ToolProvider>,
}

impl lash::process::ProcessComponentFactory for HoldComponent {
    fn declaration(&self) -> lash::process::ProcessComponentDeclaration {
        lash::process::ProcessComponentDeclaration {
            ..lash::process::ProcessComponentDeclaration::initial(HOLD.component(), HOLD.format())
                .expect("the component id")
        }
    }

    fn config_type(&self) -> std::any::TypeId {
        std::any::TypeId::of::<()>()
    }

    fn validate(
        &self,
        entry: &lash::process::ProcessComponentEntry,
        _env: &lash::process::ProcessEnv,
    ) -> Result<(), lash::process::ProcessEnvRefusal> {
        entry.config().decode::<()>().map_err(|error| {
            lash::process::ProcessEnvRefusal::ConfigRefused {
                id: entry.id().to_string(),
                reason: error.to_string(),
            }
        })
    }

    fn recipe(self: Arc<Self>) -> Result<lash::plugins::ProcessRecipe, lash::plugins::PluginError> {
        let mut registrations = lash::plugins::PluginRegistrar::<
            Self,
            lash::plugins::registries::Process,
        >::for_process(HOLD.component());
        registrations
            .tools()
            .provider(lash::tool_provider_key!("body"), |owner, _| {
                Ok(Arc::clone(&owner.body))
            })?;
        Ok(lash::plugins::ProcessRecipe::new(
            HOLD.component(),
            registrations,
            move |_| Ok(Arc::clone(&self)),
        ))
    }
}

/// The hold component, its body holding when `holds`.
fn hold_component(holds: bool, seen: &Arc<Holds>) -> Component {
    hold_component_of(hold_definition(), holds, seen)
}

/// The hold component serving `definition` under the hold tool's provider
/// key, its body holding when `holds`.
fn hold_component_of(
    definition: lash::tools::ToolDefinition,
    holds: bool,
    seen: &Arc<Holds>,
) -> Component {
    (
        Arc::new(HoldComponent {
            body: Arc::new(lash::tools::StaticToolProvider::new(
                vec![definition],
                HoldBody {
                    holds,
                    seen: Arc::clone(seen),
                },
            )),
        }),
        HOLD,
    )
}

/// The environment a held process runs under: the hold tool, from its
/// component.
fn hold_environment() -> lash::process::ProcessEnv {
    lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(16))
        .component(HOLD, &())
        .link_tool(
            HOLD.tool_provider(lash::tool_provider_key!("body")),
            &hold_definition(),
        )
        .build()
        .expect("the environment builds")
}

/// A core of build N+1 that installs `components`.
fn with_components(
    stores: &Arc<dyn StoreSet>,
    boot: &str,
    components: Vec<Component>,
) -> (lash::Backend, lash::LashCore) {
    serving_under(
        stores,
        Build::NPlus1,
        boot,
        &[],
        lash::durable::DurableSettings::standard(),
        components,
        None,
    )
}

/// The worker document as `core` admits it under `env`: each effect with
/// the signature `env`'s catalog offers it under.
async fn payload_under(
    core: &lash::LashCore,
    backend: &lash::Backend,
    env: &lash::process::ProcessEnv,
    text: &str,
) -> serde_json::Value {
    let environment = core
        .host_artifacts()
        .workflow_environment(env)
        .await
        .expect("the environment is read")
        .expect("the core reads workflow documents");
    let mut document = lash::workflow::document::parse_document(text).expect("the document parses");
    for (effect, requirement) in &mut document.manifest.effects {
        *requirement = environment
            .checker()
            .effects
            .get(effect)
            .unwrap_or_else(|| panic!("the environment offers `{effect}`"))
            .clone();
    }
    document.manifest.functions =
        lash::workflow::graph::requirements(&document, environment.functions()).functions;
    lash::workflow::graph::admit(&document, &environment.checker())
        .expect("the document is admitted");
    let identity = lash_vm_runtime::KernelDocuments::new(backend.module_artifacts())
        .publish(
            &lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                lash_core::HostArtifactPin::mint(),
            ))
            .expect("a host pin is unguarded"),
            &lash::workflow::document::IdentifiedDocument::new(document.clone()),
            &lash::workflow::document::CheckedDeclarations::required(
                &document,
                environment.functions(),
            )
            .expect("the document's helpers are held"),
        )
        .await
        .expect("the document publishes");
    serde_json::to_value(lash_vm_runtime::KernelProcessInput {
        packages: None,
        document: identity,
        functions: document.manifest.functions.keys().copied().collect(),
        entry: lash::workflow::document::Name::new("worker"),
        args: serde_json::Map::new(),
    })
    .expect("the input encodes")
}

/// The actor row of `process` once no node owns it.
async fn unowned(
    backend: &lash::Backend,
    process: &lash_core::ProcessId,
) -> lash_core::durable_port::ActorSnapshot {
    let actor = ActorKey::process(process.as_str()).expect("a process actor key");
    for _ in 0..3000 {
        let row = backend
            .durable()
            .actor(&actor)
            .await
            .expect("the actor row is read")
            .expect("the process has its actor");
        if row.owner.is_none() {
            return row;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the process stayed owned for half a minute")
}

/// Build N starts two processes whose environment names the hold component
/// with its config format; each holds its step unsettled and build N stops. A
/// later build without that component serves for a
/// bounded window and claims neither to run it. A cancel sent meanwhile
/// ends one engine-free; a build that has the component claims the other,
/// whose step completes.
async fn a_later_build_without_the_component_never_claims_its_process() {
    let stores: Arc<dyn StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("an in-memory store set opens"),
    );
    let seen = Arc::new(Holds::default());
    let env = hold_environment();

    let (backend, first) = with_components(&stores, "claim-n", vec![hold_component(true, &seen)]);
    let payload = payload_under(&first, &backend, &env, HOLDING_WORKER).await;
    let cancelled = start_under(&first, &env, payload.clone()).await;
    let completed = start_under(&first, &env, payload).await;
    for _ in 0..2 {
        tokio::time::timeout(Duration::from_secs(60), seen.entered.notified())
            .await
            .expect("each process's hold step is delivered");
    }
    first.shutdown().await.expect("build N stops");
    drop(first);
    let before = [
        unowned(&backend, &cancelled).await,
        unowned(&backend, &completed).await,
    ];
    let delivered = seen.deliveries.load(std::sync::atomic::Ordering::SeqCst);

    let later = Vec::new();
    let (later_backend, later) = with_components(&stores, "claim-n-plus-1", later);
    assert!(
        tokio::time::timeout(
            Duration::from_secs(3),
            later.processes().await_output(&completed),
        )
        .await
        .is_err(),
        "the process does not end on a build without its component"
    );
    for (process, before) in [&cancelled, &completed].into_iter().zip(&before) {
        let row = unowned(&later_backend, process).await;
        assert_eq!(
            row.epoch, before.epoch,
            "the build never claimed the process"
        );
        assert!(
            later_backend
                .process_registry()
                .get_process(process)
                .await
                .expect("the process is read")
                .is_some_and(|record| !matches!(
                    record.lifecycle,
                    lash_execution_contracts::process::ProcessLifecycleState::Terminal { .. }
                )),
            "the admitted step stays unsettled"
        );
    }
    assert_eq!(
        seen.deliveries.load(std::sync::atomic::Ordering::SeqCst),
        delivered,
        "no body ran on the later build"
    );

    later
        .processes()
        .cancel(&cancelled, later.effect_host())
        .await
        .expect("the cancel is requested");
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        later.processes().await_output(&cancelled),
    )
    .await
    .expect("the cancelled process ends within a minute")
    .expect("its end is read");
    assert!(
        matches!(&output, lash_execution_contracts::process::ProcessAwaitOutput::Settled { output }
            if matches!(output.outcome, lash_core::ToolCallOutcome::Cancelled(_))),
        "the cancel ends the process engine-free: {output:?}"
    );
    later.shutdown().await.expect("the later build stops");
    drop(later);

    let (_, capable) =
        with_components(&stores, "claim-capable", vec![hold_component(false, &seen)]);
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        capable.processes().await_output(&completed),
    )
    .await
    .expect("the capable build ends the process within a minute")
    .expect("its end is read");
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process ended without an answer: {output:?}");
    };
    assert!(output.is_success(), "{output:?}");
    assert_eq!(output.value_for_projection(), serde_json::json!("released"));
    capable.shutdown().await.expect("the capable build stops");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_build_lacking_the_component_never_claims_a_process_that_names_it() {
    a_later_build_without_the_component_never_claims_its_process().await;
}

/// PROCESS-STEP-CROSSES-DEPLOY: an unsettled step has only its
/// component's config requirements, so new code of that component finishes it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn process_step_crosses_deploy() {
    let stores: Arc<dyn StoreSet> =
        Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
    let seen = Arc::new(Holds::default());
    let env = hold_environment();
    let (backend, first) = with_components(
        &stores,
        "step-before-deploy",
        vec![hold_component(true, &seen)],
    );
    let payload = payload_under(&first, &backend, &env, HOLDING_WORKER).await;
    let process = start_under(&first, &env, payload).await;
    tokio::time::timeout(Duration::from_secs(60), seen.entered.notified())
        .await
        .unwrap();
    first.shutdown().await.unwrap();
    drop(first);
    let before = unowned(&backend, &process).await;
    let (_, next) = with_components(
        &stores,
        "step-after-deploy",
        vec![hold_component(false, &seen)],
    );
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        next.processes().await_output(&process),
    )
    .await
    .unwrap()
    .unwrap();
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process must settle");
    };
    assert!(output.is_success(), "{output:?}");
    assert_eq!(output.value_for_projection(), serde_json::json!("released"));
    assert_eq!(seen.deliveries.load(std::sync::atomic::Ordering::SeqCst), 2);
    let after = backend
        .durable()
        .actor(&ActorKey::process(process.as_str()).unwrap())
        .await
        .unwrap()
        .unwrap();
    assert!(
        after.epoch > before.epoch,
        "the later build claimed the held process"
    );
    next.shutdown().await.unwrap();
}

/// `worker()` sleeps, then performs the hold tool once, answering what it
/// answered or the error it caught.
const SLEEPING_HOLD_WORKER: &str = r#"kernel 1
numbers float
effect hold.run(input: Any) -> Any contract "1"
entry worker() -> Any

fn worker() {
  do sleep 4000
  let request = {n: 1}
  let got = "none"
  try {
    set got = perform hold.run(request) as Any
  } catch e {
    set got = {kind: e.kind, message: e.message, data: e.data}
  }
  return got
}

main {
  finish null
}
"#;

/// OP-CONTRACT-PIN (process arm; D3, §5a): build N starts a process whose
/// code pins the hold tool's contract `1`, and stops while the process
/// sleeps. A later build serving the same provider key with the tool under
/// contract `2` delivers the call the process issues once it wakes a typed,
/// catchable `operation_contract_changed` error, and runs no body. The law asserts this build's own outcome; a build serving the
/// retained surface runs the recorded call (`process_step_crosses_deploy`).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_changed_tool_answers_a_new_process_call_with_a_typed_catchable_error() {
    let stores: Arc<dyn StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("an in-memory store set opens"),
    );
    let seen = Arc::new(Holds::default());
    let env = hold_environment();
    let (backend, first) = with_components(
        &stores,
        "surface-before",
        vec![hold_component(false, &seen)],
    );
    let payload = payload_under(&first, &backend, &env, SLEEPING_HOLD_WORKER).await;
    let process = start_under(&first, &env, payload).await;
    first.shutdown().await.expect("build N stops");
    drop(first);
    unowned(&backend, &process).await;

    let changed = hold_definition_under("2");
    let (_, later) = with_components(
        &stores,
        "surface-changed",
        vec![hold_component_of(changed, false, &seen)],
    );
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        later.processes().await_output(&process),
    )
    .await
    .expect("the later build ends the process within a minute")
    .expect("its end is read");
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process ended without an answer: {output:?}");
    };
    assert!(
        output.is_success(),
        "the process caught the error: {output:?}"
    );
    let caught = output.value_for_projection().to_string();
    assert!(
        caught.contains("operation_contract_changed") && caught.contains(HOLD_TOOL),
        "the call answered the typed drift error naming its tool: {caught}"
    );
    assert_eq!(
        seen.deliveries.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "no body ran"
    );
    later.shutdown().await.expect("the later build stops");
}

// --- SUCCESSOR-PAIR-CARRIES-WAITS (FIG-6121 law 17, D2) --------------------

/// Law 9 (OP-LINK-ONE): takeover judges an already admitted call by its pinned
/// contract before its body is resumed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_admitted_call_on_a_changed_surface_settles_typed_without_restarting() {
    let stores: Arc<dyn StoreSet> =
        Arc::new(lash_sqlite_store::SqliteStoreSet::memory().await.unwrap());
    let seen = Arc::new(Holds::default());
    let env = hold_environment();
    let (backend, first) = with_components(
        &stores,
        "admitted-before",
        vec![hold_component(true, &seen)],
    );
    let text = SLEEPING_HOLD_WORKER.replace("  do sleep 4000\n", "");
    let payload = payload_under(&first, &backend, &env, &text).await;
    let process = start_under(&first, &env, payload).await;
    tokio::time::timeout(Duration::from_secs(60), seen.entered.notified())
        .await
        .unwrap();
    first.shutdown().await.unwrap();
    drop(first);
    unowned(&backend, &process).await;
    let changed = hold_definition_under("2");
    let (_, later) = with_components(
        &stores,
        "admitted-changed",
        vec![hold_component_of(changed, false, &seen)],
    );
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        later.processes().await_output(&process),
    )
    .await
    .unwrap()
    .unwrap();
    let lash_execution_contracts::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("{output:?}")
    };
    assert!(output.is_success(), "{output:?}");
    assert!(
        output
            .value_for_projection()
            .to_string()
            .contains("operation_contract_changed"),
        "{output:?}"
    );
    assert_eq!(
        seen.deliveries.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the drifted admitted body never restarts"
    );
    later.shutdown().await.unwrap();
}
