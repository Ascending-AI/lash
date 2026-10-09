//! A host's workflow on the kernel document (FIG-5715): a document is
//! admitted as a definition, read back as its total view, edited through
//! typed transactions, published again and run, on SQLite stores.
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "test target: these laws fail by panicking"
)]

use std::sync::Arc;

use futures_util::StreamExt as _;

use lash::workflow::document::{Document, Expr, Literal, Name, Place, Rhs, Stmt, Unit};
use lash::workflow::edit::{Draft, Edit, Transaction};
use lash::workflow::{WorkflowDocumentEntry, WorkflowPublication, WorkflowPublish, WorkflowRead};

/// `guarded(name)`: binds `out` inside a `try`, sleeps once in each turn of
/// a nested loop, and returns `out`. Written as a kernel document: no
/// source language is involved anywhere in these laws.
const GUARDED: &str = r#"kernel 1
numbers float
entry guarded(name: Text) -> Any

fn guarded(name) {
  let out = "start"
  try {
    set out = name
  } catch e {
    set out = "caught"
  }
  for outer in [1, 2] {
    for inner in [1] {
      do sleep 5
    }
  }
  return out
}

main {
  finish null
}
"#;

/// `fan(items)`: one task for each of three elements, each performing the
/// one `tools.echo` of the shared closure, joined together.
const FAN: &str = r#"kernel 1
numbers float
effect tools.echo(input: Any) -> Any
entry fan(a: Any, b: Any, c: Any) -> Any

fn fan(a, b, c) {
  let work = fn(item) {
    let input = {value: item}
    let got = perform tools.echo(input) as Any
    return got
  }
  let first = spawn apply work(a)
  let second = spawn apply work(b)
  let third = spawn apply work(c)
  let tasks = [first, second, third]
  let every = join all tasks
  return every
}

main {
  finish null
}
"#;

/// `supervisor(value)` starts the entry `worker` of its own document by
/// function reference, awaits it and returns what it ended with.
const SUPERVISOR: &str = r#"kernel 1
numbers float
effect processes.start(input: Any) -> Any
effect processes.await(input: Any) -> Any
entry supervisor(value: Any) -> Any
entry worker(value: Any) -> Any

fn worker(value) {
  return [value, "worked"]
}

fn supervisor(value) {
  let args = {value: value}
  let request = {definition: &worker, args: args}
  let handle = perform processes.start(request) as Any
  let wait = {handle: handle}
  let done = perform processes.await(wait) as Any
  return done
}

main {
  finish null
}
"#;

struct Echo;

#[lash::async_trait]
impl lash::tools::StaticToolExecute for Echo {
    async fn execute(&self, call: lash::tools::ToolCall<'_>) -> lash::tools::ToolAttemptOutcome {
        lash::tools::ToolOutcome::ok(call.args["value"].clone()).into()
    }
}

fn echo_tool() -> Arc<lash::tools::StaticToolProvider<Echo>> {
    use lash::tools::ToolDefinitionBindingExt as _;
    Arc::new(lash::tools::StaticToolProvider::new(
        vec![
            lash::tools::ToolDefinition::raw(
                "tool:echo",
                "echo",
                "Answers its value.",
                serde_json::json!({"type": "object"}),
                serde_json::json!({}),
            )
            .expect("tool schema")
            .with_execution(std::time::Duration::from_secs(120))
            .with_tool_binding(lash::tools::ToolBinding::new(["tools"], "echo")),
        ],
        Echo,
    ))
}

async fn core() -> lash::LashCore {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    core_over(stores, "boot").await
}

/// A node over `stores`, as the incarnation `boot` of the one deployment.
async fn core_over(stores: Arc<lash_sqlite_store::SqliteStoreSet>, boot: &str) -> lash::LashCore {
    lash_core::testing::process_execution_env_fixture(stores.process_env_store().as_ref()).await;
    let backend = lash_conformance::backend_over(stores);
    let factory = lash::rlm::RlmProtocolPluginFactory::new(
        lash::rlm::RlmProtocolPluginConfig::builder()
            .channel(lash::rlm::RlmChannel::Cell)
            .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
            .build(),
        lash::rlm::CellDialect::typescript(),
    );
    lash::LashCore::rlm_builder(backend, factory)
        .tools(echo_tool())
        .plugin(Arc::new(
            lash::process_controls::SessionProcessAdminPluginFactory::new(
                lash_core::lifetime::session_or_starter,
            ),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .tool_source_policy(lash::tools::ToolSourcePolicy::Tolerate)
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash_core::LeaseOwnerIdentity::opaque(
            lash_core::LeaseOwnerId::new("workflow-laws-worker"),
            lash_core::LeaseIncarnationId::new(format!("workflow-laws-{boot}")),
        ))
        .expect("the core builds")
}

fn environment() -> lash_core::ProcessExecutionEnvSpec {
    lash_core::ProcessExecutionEnvSpec::new(
        lash_core::AdmittedPluginConfig::default(),
        lash_core::SessionPolicy::new(
            lash::TurnBudget::bounded(32),
            lash::MaxToolCalls::new(1024),
            lash::NoProgressBudget::bounded(12),
        ),
        lash_core::SessionToolAccess::ambient(),
    )
}

fn published(publish: WorkflowPublish) -> WorkflowPublication {
    match publish {
        WorkflowPublish::Published(publication) => *publication,
        other => panic!("the workflow publishes: {other:?}"),
    }
}

/// Starts `definition` as `guarded(name)` and answers the process.
async fn start(
    core: &lash::LashCore,
    env_ref: &lash_core::ProcessExecutionEnvRef,
    definition: &lash_core::ProcessDefinition,
    key: &str,
    name: &str,
) -> lash_core::ProcessId {
    let mut args = serde_json::Map::new();
    args.insert("name".to_owned(), serde_json::json!(name));
    start_with(core, env_ref, definition, key, args).await
}

async fn start_with(
    core: &lash::LashCore,
    env_ref: &lash_core::ProcessExecutionEnvRef,
    definition: &lash_core::ProcessDefinition,
    key: &str,
    args: serde_json::Map<String, serde_json::Value>,
) -> lash_core::ProcessId {
    core.processes()
        .start(
            lash_core::ProcessStartRequest::new(
                lash_core::ProcessStartTarget::Definition {
                    definition_id: definition.id.clone(),
                    signature_claim: Some(definition.signature.clone()),
                    args,
                },
                lash_core::ProcessOriginator::host(),
                lash_core::LifetimeDecision::Detached,
            )
            .with_host_start_key(key)
            .with_env_ref(env_ref.clone()),
            core.effect_host(),
        )
        .await
        .expect("the process starts")
        .process_id
}

/// What the process finished with.
async fn finished(core: &lash::LashCore, process_id: &lash_core::ProcessId) -> serde_json::Value {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        core.processes().await_output(process_id),
    )
    .await
    .expect("the process settles")
    .expect("the output reads");
    let lash_core::ProcessAwaitOutput::Settled { output } = output else {
        panic!("the process settles with an output: {output:?}");
    };
    assert!(output.is_success(), "the process finishes: {output:?}");
    output.value_for_projection()
}

/// A document is admitted as a definition, read back with its derived
/// view, edited inside its `try` region through a typed transaction and
/// published as a second definition. Each definition's process runs the
/// document it was admitted under, through every park of the nested loop,
/// and a read of the earlier process still answers the earlier document.
#[tokio::test]
async fn a_document_is_published_read_edited_in_a_try_region_republished_and_run() {
    let core = core().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment();
    let checked = artifacts
        .workflow_environment(&environment)
        .await
        .expect("the environment reads")
        .expect("the kernel engine reads workflow documents");
    let entry = Name::new("guarded");

    let document: Document =
        lash::workflow::document::parse_document(GUARDED).expect("the document parses");
    let draft = Draft::open(document.clone(), None).expect("the document opens");
    let first = published(
        artifacts
            .publish_workflow(&pin, &draft, &entry, &environment)
            .await
            .expect("the publication answers"),
    );
    assert_eq!(first.document.document(), &document);
    assert_eq!(
        first.document.reference().entry,
        WorkflowDocumentEntry::Entry {
            function: entry.clone()
        }
    );

    let WorkflowRead::Inspected(inspection) = artifacts
        .definition_graph(&first.definition.id)
        .await
        .expect("the definition reads")
    else {
        panic!("a published workflow has a document");
    };
    assert_eq!(inspection.definition, first.definition);
    assert_eq!(inspection.document, first.document);

    // The view names the one wait of the body inside both loops.
    let graph = inspection.document.graph();
    let unit = Unit::Function(entry.clone());
    let [sleep] = graph.execution_sites() else {
        panic!(
            "the body has one execution site: {:?}",
            graph.execution_sites()
        );
    };
    assert_eq!(sleep.site.unit, unit);
    assert_eq!(sleep.loops.len(), 2, "the wait sits in the inner loop");

    // Edit inside the `try` region: its first statement now binds a
    // constant.
    let guarded = graph
        .statement_sites()
        .find(|site| site.unit == unit && site.path.len() > 1 && site.path[0] == 1)
        .expect("the first statement of the try body")
        .clone();
    let mut draft =
        Draft::open(inspection.document.document().clone(), None).expect("the document opens");
    let applied = draft
        .apply(
            &Transaction {
                base: draft.identity(),
                edits: vec![Edit::ReplaceStatement {
                    statement: guarded.clone(),
                    with: Stmt::Assign {
                        place: Place::Variable(Name::new("out")),
                        value: Rhs::Expr(Expr::Literal(Literal::Text("edited".to_owned()))),
                    },
                }],
            },
            &checked.checker(),
        )
        .expect("the edit applies");
    assert_ne!(applied.admitted.identity, first.document.identity());
    let second = published(
        artifacts
            .publish_workflow(&pin, &draft, &entry, &environment)
            .await
            .expect("the publication answers"),
    );
    assert_ne!(second.definition.id, first.definition.id);
    assert_eq!(second.document.identity(), applied.admitted.identity);
    let survivor = second
        .correspondence
        .survivor(&sleep.site)
        .expect("the wait survives the edit");
    assert_eq!((&survivor.to, survivor.edited), (&sleep.site, false));

    // The first definition is immutable: a process of it runs the document
    // it was admitted with, beside a process of the edited one.
    let env_ref = artifacts
        .publish_process_env(&pin, &environment)
        .await
        .expect("publish the process environment");
    let kept = start(&core, &env_ref, &first.definition, "first", "operator").await;
    let edited = start(&core, &env_ref, &second.definition, "second", "operator").await;
    assert_eq!(finished(&core, &kept).await, serde_json::json!("operator"));
    assert_eq!(finished(&core, &edited).await, serde_json::json!("edited"));

    let WorkflowRead::Inspected(kept_read) = core
        .processes()
        .graph(&kept)
        .await
        .expect("the process reads")
    else {
        panic!("a kernel process has a document");
    };
    assert_eq!(kept_read.document, first.document);
    assert_eq!(kept_read.definition, first.definition);
}

/// A fan-out's overlay tells its elements apart (`K-EFF-008`). Three tasks
/// perform the one `tools.echo` of a shared closure, so each has occurrence
/// 0 of that site. The process's feed reports each at the machine's own
/// identity, and the overlay folded from it holds one completed row for
/// each task at the one site, every site being the document's.
#[tokio::test]
async fn a_fan_out_overlay_holds_one_row_for_each_elements_effect() {
    use lash::process::{ProcessObservationEventPayload, ProcessObservationStreamItem};
    use lash::workflow::{WorkflowExecutionOverlayAccumulator, WorkflowOverlayOccurrence};

    let core = core().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment();
    let checked = artifacts
        .workflow_environment(&environment)
        .await
        .expect("the environment reads")
        .expect("the kernel engine reads workflow documents");
    let echo = lash::workflow::document::EffectName::new("tools.echo").expect("an effect name");
    let mut document = lash::workflow::document::parse_document(FAN).expect("the document parses");
    // The document states the signature the host offers the effect under.
    document.manifest.effects.insert(
        echo.clone(),
        checked
            .effects()
            .get(&echo)
            .expect("the host offers the tool")
            .clone(),
    );
    let entry = Name::new("fan");
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &Draft::open(document, None).expect("the document opens"),
                &entry,
                &environment,
            )
            .await
            .expect("the publication answers"),
    );
    let performs: Vec<_> = publication
        .document
        .graph()
        .execution_sites()
        .iter()
        .filter(|site| site.kind == lash::workflow::graph::SiteKind::Perform)
        .collect();
    let [perform] = performs.as_slice() else {
        panic!("the document performs at one site: {performs:?}");
    };

    let env_ref = artifacts
        .publish_process_env(&pin, &environment)
        .await
        .expect("publish the process environment");
    let args = serde_json::json!({"a": "x", "b": "y", "c": "z"});
    let serde_json::Value::Object(args) = args else {
        unreachable!()
    };
    let process = start_with(&core, &env_ref, &publication.definition, "fan", args).await;
    let observed = core.processes().observe(&process);
    let snapshot = observed.snapshot().await.expect("the durable snapshot");
    let mut feed = observed.subscribe_and_recover(snapshot.cursor);
    assert_eq!(
        finished(&core, &process).await,
        serde_json::json!(["x", "y", "z"])
    );

    let mut overlay = WorkflowExecutionOverlayAccumulator::default();
    overlay.set_document(publication.document.overlay_document());
    loop {
        let item = tokio::time::timeout(std::time::Duration::from_secs(60), feed.next())
            .await
            .expect("the feed wakes")
            .expect("the feed continues")
            .expect("the observation reads");
        match item {
            ProcessObservationStreamItem::Event(event) => match &event.payload {
                ProcessObservationEventPayload::LanguageExecution(observation) => {
                    overlay.observe(observation).expect("the observation folds");
                }
                ProcessObservationEventPayload::StepBodyStarted(observation) => {
                    overlay
                        .step_body_started(observation)
                        .expect("the step body start folds");
                }
                _ => {}
            },
            ProcessObservationStreamItem::Gap { .. } => overlay.reset_live(),
        }
        let done = overlay.snapshot().is_some_and(|overlay| {
            overlay
                .sites
                .iter()
                .filter(|row| {
                    row.site.site == perform.site
                        && matches!(
                            row.state.occurrence,
                            WorkflowOverlayOccurrence::Completed { .. }
                        )
                })
                .count()
                == 3
        });
        if done {
            break;
        }
    }
    let overlay = overlay.snapshot().expect("the overlay");
    assert!(overlay.mismatches.is_empty(), "{:?}", overlay.mismatches);
    let rows: Vec<_> = overlay
        .sites
        .iter()
        .filter(|row| row.site.site == perform.site)
        .collect();
    assert_eq!(rows.len(), 3, "one row for each element: {rows:#?}");
    let tasks: std::collections::BTreeSet<_> = rows.iter().map(|row| &row.site.task).collect();
    assert_eq!(tasks.len(), 3, "each row is another task's: {rows:#?}");
    for row in rows {
        assert!(
            matches!(
                row.state.occurrence,
                WorkflowOverlayOccurrence::Completed { occurrence: 0, .. }
            ),
            "each element ran the site once, as its occurrence 0: {row:#?}"
        );
        assert!(
            matches!(
                row.site.task,
                lash::workflow::document::TaskIdentity::Spawned(_)
            ),
            "an element's effect belongs to its spawned task: {row:#?}"
        );
    }
}

/// Starting a process is an effect that takes a function reference
/// (kernel spec §1). A document lists two entries; one publication holds a
/// definition for each. The supervisor's `&worker` crosses the effect
/// boundary as the definition of that entry of its own document, the child
/// runs `worker`, and the supervisor's await of the handle answers the
/// child's result.
#[tokio::test]
async fn a_process_starts_another_entry_of_its_document_by_function_reference_and_awaits_it() {
    let core = core().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment();
    let checked = artifacts
        .workflow_environment(&environment)
        .await
        .expect("the environment reads")
        .expect("the kernel engine reads workflow documents");
    let mut document =
        lash::workflow::document::parse_document(SUPERVISOR).expect("the document parses");
    for effect in ["processes.start", "processes.await"] {
        let name = lash::workflow::document::EffectName::new(effect).expect("an effect name");
        let offered = checked
            .effects()
            .get(&name)
            .unwrap_or_else(|| panic!("the host offers `{effect}`"));
        document.manifest.effects.insert(name, offered.clone());
    }
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &Draft::open(document, None).expect("the document opens"),
                &Name::new("supervisor"),
                &environment,
            )
            .await
            .expect("the publication answers"),
    );
    let env_ref = artifacts
        .publish_process_env(&pin, &environment)
        .await
        .expect("publish the process environment");
    let mut args = serde_json::Map::new();
    args.insert("value".to_owned(), serde_json::json!("x"));
    let supervisor = start_with(&core, &env_ref, &publication.definition, "supervisor", args).await;
    let ended = finished(&core, &supervisor).await;
    assert!(
        ended.to_string().contains(r#"["x","worked"]"#),
        "the supervisor returns what its child ended with: {ended}"
    );
}

/// A process parked inside its inner loop outlives its node (kernel spec
/// §9). The node that started it stops while the run stands on a `sleep`
/// in the first turn of the nested loop; a second node over the same
/// SQLite file resumes the saved run from that park, takes it through the
/// remaining turns and finishes it with the value bound before the park.
#[tokio::test]
async fn a_process_parked_in_its_inner_loop_resumes_on_another_node_and_finishes() {
    let files = tempfile::tempdir().expect("a SQLite store directory");
    let open = || async {
        Arc::new(
            lash_sqlite_store::SqliteStoreSet::open(
                files.path().join("lash.db"),
                lash_sqlite_store::SqliteSynchronous::Normal,
            )
            .await
            .expect("open the SQLite file stores"),
        )
    };
    let first = core_over(open().await, "first").await;
    let artifacts = first.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment();
    let document =
        lash::workflow::document::parse_document(&GUARDED.replace("sleep 5", "sleep 1500"))
            .expect("the document parses");
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &Draft::open(document, None).expect("the document opens"),
                &Name::new("guarded"),
                &environment,
            )
            .await
            .expect("the publication answers"),
    );
    let env_ref = artifacts
        .publish_process_env(&pin, &environment)
        .await
        .expect("publish the process environment");
    let process = start(
        &first,
        &env_ref,
        &publication.definition,
        "parked",
        "operator",
    )
    .await;

    // The run parks on its first `sleep`: the process waits on a timer.
    let parked = async {
        loop {
            let observed = first
                .processes()
                .get(&process)
                .await
                .expect("the process reads")
                .expect("the process is retained");
            if matches!(
                observed.lifecycle,
                lash_core::ProcessLifecycleState::Waiting { .. }
            ) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(60), parked)
        .await
        .expect("the process parks");
    first.shutdown().await.expect("the first node stops");
    drop(first);

    let second = core_over(open().await, "second").await;
    assert_eq!(
        finished(&second, &process).await,
        serde_json::json!("operator")
    );
    let WorkflowRead::Inspected(read) = second
        .processes()
        .graph(&process)
        .await
        .expect("the process reads")
    else {
        panic!("a kernel process has a document");
    };
    assert_eq!(read.document.identity(), publication.document.identity());
}
