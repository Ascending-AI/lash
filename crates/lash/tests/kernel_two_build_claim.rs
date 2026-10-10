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
//! The refusal law beside it: a process parked inside a library function's
//! kernel body, which the synthetic migration declares it cannot carry, is
//! listed with that typed reason by the survey `lashctl kernel-migration
//! list` prints, and build N+1 parks it with the same reason at the claim.

#![cfg(all(feature = "synthetic-next", feature = "rlm", feature = "sqlite"))]
#![allow(clippy::disallowed_methods, clippy::expect_used, clippy::unwrap_used)]

use std::sync::Arc;
use std::time::Duration;

use lash::workflow::document::KernelVersion;
use lash_core::StoreSet;
use lash_core::durable_port::{ActorKey, ActorState};

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

/// A core over `stores`: of build N when `previous`, else of build N+1.
fn core(stores: &Arc<dyn StoreSet>, previous: bool, boot: &str) -> (lash::Backend, lash::LashCore) {
    let build = if previous { Build::N } else { Build::NPlus1 };
    serving(stores, build, boot, &[])
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
    serving_under(stores, build, boot, cells, settings)
}

/// [`serving`] under the durable `settings`.
fn serving_under(
    stores: &Arc<dyn StoreSet>,
    build: Build,
    boot: &str,
    cells: &[&str],
    settings: lash::durable::DurableSettings,
) -> (lash::Backend, lash::LashCore) {
    let previous = build == Build::N;
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
    let factory = if previous {
        factory.writing_kernel(KernelVersion::One)
    } else {
        factory
    };
    let core = lash::LashCore::rlm_builder(backend.clone(), factory)
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
        .tool_source_policy(lash_core::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new(boot),
            lash::persistence::LeaseIncarnationId::new(format!("{boot}-boot")),
        ))
        .expect("the core builds");
    (backend, core)
}

fn environment() -> lash_core::ProcessExecutionEnvSpec {
    let mut environment = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(16),
            lash_core::NoProgressBudget::bounded(12),
        ),
        lash_core::SessionToolAccess::ambient(),
    );
    environment.render = Some(lash_core::RecordedRender {
        renderer_id: lash::render::ToolOutputRendererSlot::default()
            .0
            .id()
            .to_owned(),
        params: serde_json::to_value(lash::render::ResolvedStandardRenderConfig {
            defaults: lash::render::ToolRenderParams::default(),
            per_tool: std::collections::BTreeMap::new(),
        })
        .expect("the render config encodes"),
    });
    environment
}

/// Publishes the worker document under a host pin, and answers the start
/// payload of its `worker` entry.
async fn payload(backend: &lash::Backend, text: &str) -> serde_json::Value {
    let document = lash::workflow::document::parse_document(text).expect("the document parses");
    let identity = lash_vm_runtime::KernelDocuments::new(backend.module_artifacts())
        .publish(
            &lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                lash_core::HostArtifactPin::mint(),
            ))
            .expect("a host pin is unguarded"),
            &document,
        )
        .await
        .expect("the document publishes");
    serde_json::to_value(lash_vm_runtime::KernelProcessInput {
        document: identity,
        entry: lash::workflow::document::Name::new("worker"),
        args: serde_json::Map::new(),
    })
    .expect("the input encodes")
}

async fn start(core: &lash::LashCore, payload: serde_json::Value) -> lash_core::ProcessId {
    let env_ref = core
        .host_artifacts()
        .publish_process_env(&lash_core::HostArtifactPin::mint(), &environment())
        .await
        .expect("the environment is published");
    let request = lash_core::ProcessStartRequest::new(
        lash_core::ProcessInput::Engine {
            kind: KIND.to_owned(),
            payload,
        },
        lash_core::ProcessOriginator::host(),
        lash_core::LifetimeDecision::Detached,
    )
    .with_env_ref(env_ref);
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

/// What `lashctl kernel-migration list` prints for `backend`.
async fn survey(backend: &lash::Backend) -> serde_json::Value {
    let functions = lash::vm::standard_functions().expect("the shipped library assembles");
    let survey = lash::vm::survey_kernel_migration(backend, &functions)
        .await
        .expect("the survey reads the store");
    serde_json::to_value(&survey).expect("the survey encodes")
}

/// `worker()` sleeps inside `collection.map`'s kernel body, through the
/// closure it hands it. It sleeps twice: a sleep's deadline is counted from
/// its run's admission, which a cold worker's start can outlast, so only
/// the second is sure to park the process.
fn library_body_worker() -> String {
    let functions = lash::vm::standard_functions().expect("the shipped library assembles");
    let map = functions
        .iter()
        .find(|(_, function)| {
            function.definition.name.to_string() == "collection.map"
                && function.definition.kernel == KernelVersion::One.number()
        })
        .map(|(identity, _)| *identity)
        .expect("the shipped library has collection.map");
    format!(
        r#"kernel 1
numbers float
use collection.map = @{map}
entry worker() -> Any

fn worker() {{
  let nap = fn(x) {{ do sleep 1500 return x }}
  let items = ["first", "second"]
  let out = invoke collection.map(items, nap)
  return out
}}

main {{
  finish null
}}
"#
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_the_migration_cannot_carry_is_listed_and_parked_with_its_typed_reason() {
    let stores: Arc<dyn StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("an in-memory store set opens"),
    );
    let (old_backend, old) = core(&stores, true, "build-n");
    let payload = payload(&old_backend, &library_body_worker()).await;
    let process = start(&old, payload).await;
    let actor = ActorKey::process(process.as_str()).expect("a process actor key");
    let processes = old.processes();
    let row = tokio::select! {
        row = waiting(&old_backend, &actor) => row,
        output = processes.await_output(&process) => {
            panic!("the process ended instead of sleeping: {output:?}")
        }
    };
    assert_eq!(row.state, ActorState::Waiting);
    old.drain().await.expect("build N's node drains");

    let survey = survey(&old_backend).await;
    assert_eq!(survey["checked"], 1, "{survey}");
    let listed = &survey["refused"][0];
    assert_eq!(listed["process"], serde_json::json!(process), "{survey}");
    assert_eq!(listed["refusal"]["refused"], "parked", "{survey}");
    assert_eq!(
        listed["refusal"]["refusal"]["reason"], "site_not_carried",
        "{survey}"
    );

    let (new_backend, _new) = core(&stores, false, "build-n-plus-1");
    let parked = in_state(&new_backend, &actor, ActorState::Parked).await;
    let reason: serde_json::Value =
        serde_json::from_str(parked.park.as_deref().expect("a parked actor says why"))
            .expect("the park reason is JSON");
    assert_eq!(reason["reason"], "migration_refused", "{reason}");
    assert_eq!(reason["refusal"], listed["refusal"], "{reason}");
    assert!(
        parked
            .formats
            .as_str()
            .contains(&engine_format(KernelVersion::One)),
        "the refused process is as build N left it: {:?}",
        parked.formats
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_process_parked_by_build_n_is_claimed_migrated_and_resumed_by_build_n_plus_1() {
    let stores: Arc<dyn StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("an in-memory store set opens"),
    );
    let (old_backend, old) = core(&stores, true, "build-n");
    let payload = payload(&old_backend, WORKER).await;
    let process = start(&old, payload).await;
    let actor = ActorKey::process(process.as_str()).expect("a process actor key");

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

    let (new_backend, new) = core(&stores, false, "build-n-plus-1");
    let output = tokio::time::timeout(
        Duration::from_secs(60),
        new.processes().await_output(&process),
    )
    .await
    .expect("the process ends within a minute")
    .expect("the process's end is read");
    let lash_core::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process ended without an answer: {output:?}");
    };
    assert!(output.is_success(), "the process failed: {output:?}");
    assert_eq!(output.value_for_projection(), serde_json::json!("second"));

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
            .contains(&engine_format(KernelVersion::SyntheticNext)),
        "the process ended in build N+1's format set, having been migrated at the claim: {:?}",
        ended.formats
    );
    assert_ne!(ended.formats, written);
}

/// The session `id` of `core`, created if it is not there.
async fn session(core: &lash::LashCore, id: &str) -> lash::LashSession {
    let id = lash::SessionId::parse(id).expect("a session id");
    match core
        .session(id.clone())
        .create(lash::SessionCreation::root(
            lash::plugins::SessionToolAccess::ambient(),
            lash::SessionSpec::new(
                MODEL,
                lash::TurnBudget::Unbounded,
                lash::MaxToolCalls::new(1024),
            )
            .no_progress_budget(lash::NoProgressBudget::bounded(12)),
        ))
        .await
    {
        Ok(_) | Err(lash::EmbedError::SessionAlreadyExists { .. }) => {}
        Err(error) => panic!("the session is created: {error:?}"),
    }
    core.session(id).open().await.expect("the session opens")
}

/// What each cell of the session `output` ends in finished with, oldest
/// first.
fn cell_finishes(output: &lash::TurnOutput) -> Vec<serde_json::Value> {
    output
        .result
        .state
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
                .filter(|result| result["kind"] == "finished")?
                .get("value")?
                .get("inline")
                .cloned(),
            _ => None,
        })
        .collect()
}

/// The two-build claim law of a session (kernel spec §6): a session of
/// build N saves a function, and its next turn's cell parks in a `sleep`
/// with a call of that function still ahead of it. Build N dies. Build
/// N+1 claims the session, which is in build N's format set: restoring the
/// turn, it carries the cell's document, parked run and ledger to its own
/// kernel version and moves the session to its own format set in one
/// commit, and the cell resumes, calling the function its document
/// declares, to the answer it would have reached.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_session_cell_parked_by_build_n_is_claimed_migrated_and_resumed_by_build_n_plus_1() {
    const SESSION: &str = "kernel-two-build-session";
    let stores: Arc<dyn StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("an in-memory store set opens"),
    );
    let actor = ActorKey::session(SESSION).expect("a session actor key");

    // Build N runs on a runtime of its own, which the law shuts down under
    // it: a node lets go of a turn whose cell sleeps only by dying.
    let build_n = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build N's runtime");
    let written = build_n
        .spawn({
            let stores = Arc::clone(&stores);
            let actor = actor.clone();
            async move {
                let (old_backend, old) = serving(
                    &stores,
                    Build::N,
                    "build-n",
                    &[
                        "function double(n: number) { return n * 2; }\nfinish('bound');",
                        "await sleep(3000);\nfinish(double(21));",
                    ],
                );
                let opened = session(&old, SESSION).await;
                let bound = opened
                    .send(lash::TurnInput::text("bind the function"))
                    .output()
                    .await
                    .expect("the binding turn");
                assert!(bound.is_success(), "the binding turn: {bound:?}");
                tokio::spawn(async move {
                    let _ = opened
                        .send(lash::TurnInput::text("sleep, then call it"))
                        .output()
                        .await;
                });
                // The cell parks in its sleep: its snapshot is the
                // session's last commit until the sleep is over.
                let row = || async {
                    old_backend
                        .durable()
                        .actor(&actor)
                        .await
                        .expect("the actor row is read")
                        .expect("the session has its actor")
                };
                let mut parked = row().await;
                let mut still = 0;
                while still < 100 {
                    tokio::time::sleep(Duration::from_millis(10)).await;
                    let now = row().await;
                    still = if now.revision == parked.revision && now.state == ActorState::Owned {
                        still + 1
                    } else {
                        0
                    };
                    parked = now;
                }
                // The core is kept from its drop, which would let the
                // session go in order.
                std::mem::forget(old);
                parked.formats
            }
        })
        .await
        .expect("build N parks the cell");
    assert!(
        written.as_str().contains("kernel-parked-state@1"),
        "the parked session is in build N's format set: {written:?}"
    );
    build_n.shutdown_background();

    // Build N+1 reaps the dead node, claims the session and carries the
    // cell before the cell runs again: the session is in its format set
    // while no later turn has been admitted.
    let (new_backend, new) = serving(
        &stores,
        Build::NPlus1,
        "build-n-plus-1",
        &["finish('after');"],
    );
    let mut carried = None;
    for _ in 0..6000 {
        let row = new_backend
            .durable()
            .actor(&actor)
            .await
            .expect("the actor row is read")
            .expect("the session has its actor");
        if row.formats != written {
            carried = Some(row);
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let carried = carried.expect("build N+1 carries the session within a minute");
    assert!(
        carried.formats.as_str().contains("kernel-parked-state@2"),
        "the session is in build N+1's format set: {:?}",
        carried.formats
    );

    let later = tokio::time::timeout(
        Duration::from_secs(60),
        session(&new, SESSION)
            .await
            .send(lash::TurnInput::text("call it again"))
            .output(),
    )
    .await
    .expect("the later turn ends within a minute")
    .expect("the later turn");
    assert!(later.is_success(), "the later turn: {later:?}");
    let finishes = cell_finishes(&later);
    assert_eq!(
        finishes.get(1),
        Some(&serde_json::json!(42)),
        "the parked cell resumed under build N+1 to its answer: {finishes:?}"
    );
    assert_eq!(finishes.last(), Some(&serde_json::json!("after")));
}

/// `worker()` of [`WORKER`], its sleep an hour long: a process that no node
/// claims within a law's time unless something wakes it.
fn long_sleeper() -> String {
    WORKER.replace("do sleep 1500", "do sleep 3600000")
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
const BIND: &str = "function double(n: number) { return n * 2; }\nfinish('bound');";

/// FIG-5787: a process that waits on a long timer and a session that runs
/// no turn are not claimed by build N+1 within its window, so they would
/// be stranded under the build that closes it. The operator's sweep wakes
/// both: a node of build N+1 claims each and carries it to its own formats,
/// the process at its claim and the idle session outside a turn. Once
/// build N+1 drains, the closing build, which no longer reads build N's
/// formats, starts, decodes the process and runs a turn of the session
/// that calls the function it saved under build N.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_idle_process_and_an_idle_session_are_swept_by_build_n_plus_1_and_read_by_the_closing_build()
 {
    const SESSION: &str = "kernel-sweep-session";
    let stores: Arc<dyn StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("an in-memory store set opens"),
    );
    let (old_backend, old) = idling(&stores, Build::N, "build-n", &[BIND]);
    let payload = payload(&old_backend, &long_sleeper()).await;
    let process = start(&old, payload).await;
    let process_actor = ActorKey::process(process.as_str()).expect("a process actor key");
    let session_actor = ActorKey::session(SESSION).expect("a session actor key");
    let process_written = waiting(&old_backend, &process_actor).await.formats;
    let session_written = idle_session(&old_backend, &old, SESSION).await;
    old.drain().await.expect("build N's node drains");
    let listed = survey(&old_backend).await;
    assert_eq!(listed["unmigrated"], 2, "{listed}");
    assert_eq!(
        listed["sessions"],
        serde_json::json!([{"session": SESSION, "refused": []}]),
        "{listed}"
    );

    let (new_backend, new) = serving(&stores, Build::NPlus1, "build-n-plus-1", &[]);
    let swept = lash::vm::sweep_kernel_migration(&new_backend)
        .await
        .expect("the sweep wakes what is left");
    assert_eq!(
        serde_json::to_value(&swept).expect("the sweep encodes"),
        serde_json::json!({"processes": 1, "sessions": 1})
    );
    let process_row = carried_from(&new_backend, &process_actor, &process_written).await;
    let session_row = carried_from(&new_backend, &session_actor, &session_written).await;
    assert!(
        process_row
            .formats
            .as_str()
            .contains(&engine_format(KernelVersion::SyntheticNext)),
        "the process is in build N+1's format set: {:?}",
        process_row.formats
    );
    assert_eq!(
        &session_row.formats,
        new_backend.formats().session(),
        "the idle session is in build N+1's format set"
    );
    let listed = survey(&new_backend).await;
    assert_eq!(listed["unmigrated"], 0, "{listed}");
    new.drain().await.expect("build N+1's node drains");

    let (closing_backend, closing) = serving(
        &stores,
        Build::Closing,
        "build-closing",
        &["finish(double(4));"],
    );
    // The closing build reads the swept process: woken, its node claims
    // it, decodes its state and lets it wait for its timer again, where a
    // state it did not read would park it.
    let before = new_backend
        .durable()
        .actor(&process_actor)
        .await
        .expect("the actor row is read")
        .expect("the process has its actor");
    let mut wake = lash_core::durable_port::MailTx::new();
    wake.wake(process_actor.clone());
    closing_backend
        .commit_mail(wake, lash_core::durable_port::CommitLabel::MAIL_PROCESS)
        .await
        .expect("the process is woken");
    let mut last = None;
    for _ in 0..6000 {
        let row = closing_backend
            .durable()
            .actor(&process_actor)
            .await
            .expect("the actor row is read")
            .expect("the process has its actor");
        assert_ne!(
            row.state,
            ActorState::Parked,
            "the closing build did not read the swept process: {row:?}"
        );
        if row.revision > before.revision && row.state == ActorState::Waiting {
            last = Some(row);
            break;
        }
        last = Some(row);
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        last.as_ref()
            .is_some_and(|row| row.revision > before.revision && row.state == ActorState::Waiting),
        "the closing build's node did not take the woken process up: {last:?}"
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
        cell_finishes(&read).last(),
        Some(&serde_json::json!(8)),
        "the closing build called the function the session saved under build N"
    );
}

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
        &["finish(double(21));"],
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
    assert_eq!(cell_finishes(&read).last(), Some(&serde_json::json!(42)));
}

/// FIG-5787: `lashctl kernel-migration list` names a session whose open
/// turn stopped in a cell the migration would refuse, with the typed
/// reason. The cell awaits a promise it made of a sleep, so its run is
/// parked inside the dialect's `ts.await`, a library function's kernel
/// body, which the synthetic migration declares it cannot carry.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_listing_names_a_refused_session_cell_with_its_reason() {
    const SESSION: &str = "kernel-refused-cell-session";
    let stores: Arc<dyn StoreSet> = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("an in-memory store set opens"),
    );
    let actor = ActorKey::session(SESSION).expect("a session actor key");
    // Build N runs on a runtime of its own, which the law shuts down under
    // it once the cell is parked: a node lets go of a sleeping cell only by
    // dying.
    let build_n = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("build N's runtime");
    let backend = build_n
        .spawn({
            let stores = Arc::clone(&stores);
            let actor = actor.clone();
            async move {
                let (old_backend, old) = serving(
                    &stores,
                    Build::N,
                    "build-n",
                    &["const nap = sleep(60000);\nawait nap;\nfinish('woke');"],
                );
                let opened = session(&old, SESSION).await;
                tokio::spawn(async move {
                    let _ = opened
                        .send(lash::TurnInput::text("await the nap"))
                        .output()
                        .await;
                });
                // The cell parks in its await: its snapshot is committed
                // under the open turn until the sleep is over.
                let id = lash::SessionId::parse(SESSION).expect("a session id");
                let reads = old_backend.durable();
                let mut last = None;
                for _ in 0..3000 {
                    last = reads.turn(&id).await.expect("the turn row is read");
                    if let Some(turn) = &last
                        && !reads
                            .cell_snapshots(&id, &turn.run)
                            .await
                            .expect("the cell snapshots are read")
                            .is_empty()
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                let parked = match &last {
                    Some(turn) => reads
                        .cell_snapshots(&id, &turn.run)
                        .await
                        .expect("the cell snapshots are read"),
                    None => Vec::new(),
                };
                assert!(
                    !parked.is_empty(),
                    "the cell did not park within half a minute: {last:?}, {:?}",
                    reads.actor(&actor).await
                );
                std::mem::forget(old);
                old_backend
            }
        })
        .await
        .expect("build N parks the cell");
    build_n.shutdown_background();

    let survey = survey(&backend).await;
    assert_eq!(survey["unmigrated"], 1, "{survey}");
    let listed = &survey["sessions"][0];
    assert_eq!(listed["session"], SESSION, "{survey}");
    let refused = &listed["refused"][0];
    assert_eq!(refused["refusal"]["refused"], "parked", "{survey}");
    assert_eq!(
        refused["refusal"]["refusal"]["reason"], "site_not_carried",
        "{survey}"
    );
}
