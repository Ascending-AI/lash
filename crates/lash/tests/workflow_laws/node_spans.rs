//! A workflow process exports one `lash.workflow.node` span per occurrence
//! of a site, keyed by its whole effect identity, with the tool its
//! occurrence called beneath it (spec §4.8; laws L18 and L21(g)).

use std::collections::BTreeSet;

use super::*;
use lash::tracing::{
    LashTracerProvider, OtelOptions, OtelTelemetry, TraceDetail, TraceDetailPolicy, TraceSpanKind,
};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider, SpanData};

/// `nodes(name)`: performs `tools.echo` at the one site of `work`, from a
/// three-turn loop that parks on a timer after each turn, then from two
/// concurrently spawned tasks.
fn nodes_document() -> Document {
    let imports = format!(
        "{}\nimport 1 operation typescript.sleep(milliseconds: Int) -> Null",
        echo_import_header(0)
    );
    let work = lash_kernel_doc::parse_definition(&format!("function work(item: Json) -> Json\nkernel 1\nmetering statements\n{imports}\nbody {{let input = {{value: item}} let got = perform 0(input) as Any return got}}")).unwrap();
    let nodes = lash_kernel_doc::parse_definition(&format!("function nodes(name: Text) -> Json\nkernel 1\nmetering statements\n{imports}\nuse work = @{}\nbody {{for turn in [1,2,3] {{do call work(turn) do perform 1(5) as Null}} let first = spawn call work(\"x\") let second = spawn call work(\"y\") let tasks = [first,second] let every = join all tasks return every}}",work.identity())).unwrap();
    let mut document = function_document(nodes, "nodes");
    document.functions.insert(work.identity(), work);
    document
}

/// What one node exports to: its OpenTelemetry spans.
struct Exported {
    spans: InMemorySpanExporter,
    _tracer: LashTracerProvider,
}

impl Exported {
    fn spans(&self, name: &str) -> Vec<SpanData> {
        self.spans
            .get_finished_spans()
            .expect("exported spans")
            .into_iter()
            .filter(|span| span.name == name || span.name.starts_with(&format!("{name} ")))
            .collect()
    }

    fn all(&self) -> Vec<SpanData> {
        self.spans.get_finished_spans().expect("exported spans")
    }

    /// Until `count` spans named `name` were exported.
    async fn until(&self, name: &str, count: usize) -> Vec<SpanData> {
        tokio::time::timeout(std::time::Duration::from_secs(60), async {
            loop {
                let spans = self.spans(name);
                if spans.len() >= count {
                    return spans;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{count} `{name}` spans are exported: {:#?}", self.all()))
    }
}

/// A node over `stores` as the incarnation `boot`, exporting every span it
/// builds under `workflow`'s detail for workflows.
async fn traced_core(
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    boot: &str,
    workflow: TraceDetail,
) -> (lash::LashCore, Exported) {
    let spans = InMemorySpanExporter::default();
    let tracer = LashTracerProvider::new(
        SdkTracerProvider::builder().with_simple_exporter(spans.clone()),
        lash_trace::otel::sdk::trace::Sampler::AlwaysOn,
        lash_trace::otel::sdk::trace::RandomIdGenerator::default(),
    );
    let meter = opentelemetry_sdk::metrics::SdkMeterProvider::builder().build();
    let detail = TraceDetailPolicy::builder()
        .set(TraceSpanKind::Workflow, workflow)
        .build()
        .expect("the detail policy builds");
    let core = builder_with_process_tools(stores, false, EchoComponent::Installed)
        .await
        .trace_sink(Arc::new(lash::testing::RecordingTraceSink::new()))
        .telemetry(OtelTelemetry::new(&tracer, &meter, OtelOptions::default()))
        .trace_detail(detail)
        .build(node_identity(boot))
        .expect("the core builds");
    (
        core,
        Exported {
            spans,
            _tracer: tracer,
        },
    )
}

/// The `nodes` workflow published on `core`, with the env it starts under.
async fn publish_nodes(
    core: &lash::LashCore,
) -> (
    lash_core::ProcessEnvRef,
    lash_execution_contracts::process::ProcessDefinition,
) {
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(false);
    let document = nodes_document();
    let publication = publish(
        core,
        &pin,
        &lash::workflow::document::DocumentBytes::from(document.clone()),
        &Name::new("nodes"),
        &environment,
        None,
    )
    .await
    .expect("the publication answers");
    let env_ref = publication.env_ref;
    (env_ref, publication.definition)
}

fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|attribute| attribute.key.as_str() == key)
        .map(|attribute| attribute.value.to_string())
}

/// The five node spans of one `nodes` run, from every node that exported
/// some: each names its own occurrence of the one site, under the run's
/// workflow span, with the one tool call its occurrence made beneath it.
fn assert_five_occurrences(all_nodes: &[SpanData], every: &[SpanData]) {
    assert_eq!(
        all_nodes.len(),
        8,
        "five echo operations and three ordinary timers"
    );
    let function = nodes_document()
        .functions
        .into_iter()
        .find(|(_, definition)| definition.name.as_str() == "work")
        .unwrap()
        .0;
    let nodes = all_nodes
        .iter()
        .filter(|span| {
            attribute(span, "lash.workflow.node.site")
                .is_some_and(|site| site.starts_with(&format!("function {function}/")))
        })
        .collect::<Vec<_>>();
    assert_eq!(nodes.len(), 5, "one node span per occurrence: {nodes:#?}");
    let ids: std::collections::HashSet<_> = nodes
        .iter()
        .map(|span| span.span_context.span_id())
        .collect();
    assert_eq!(ids.len(), 5, "five distinct node ids: {nodes:#?}");
    let occurrences: BTreeSet<_> = nodes
        .iter()
        .map(|span| {
            let occurrence = attribute(span, "lash.workflow.node.occurrence")
                .expect("a node names its occurrence");
            serde_json::from_str::<lash::workflow::document::EffectId>(&occurrence)
                .expect("the occurrence is an effect identity")
        })
        .collect();
    assert_eq!(occurrences.len(), 5, "each node is another occurrence");
    let sites: BTreeSet<_> = occurrences.iter().map(|at| at.position()).collect();
    assert_eq!(sites.len(), 1, "every occurrence is of the one site");
    let spawned = occurrences
        .iter()
        .filter(|at| !lash::workflow::document::ObservationReader::is_root_task(at))
        .count();
    assert_eq!(spawned, 2, "two occurrences belong to spawned tasks");
    let workflow: Vec<_> = every
        .iter()
        .filter(|span| span.name.starts_with("invoke_workflow"))
        .collect();
    let [workflow] = workflow.as_slice() else {
        panic!("one workflow span: {every:#?}");
    };
    for node in nodes {
        assert_eq!(
            node.parent_span_id,
            workflow.span_context.span_id(),
            "a node is a child of its workflow"
        );
        assert_eq!(
            attribute(node, "lash.outcome").as_deref(),
            Some("completed")
        );
        let tools: Vec<_> = every
            .iter()
            .filter(|span| {
                span.name.starts_with("execute_tool")
                    && span.parent_span_id == node.span_context.span_id()
            })
            .collect();
        let [tool] = tools.as_slice() else {
            panic!("the occurrence's one tool is beneath its node: {node:#?} {every:#?}");
        };
        assert!(
            node.start_time <= tool.start_time && tool.end_time <= node.end_time,
            "a node spans its tool: {node:#?} {tool:#?}"
        );
    }
}

/// L18: a site performed in a three-turn loop, with a park and resume
/// after each turn, and in two concurrently spawned tasks exports
/// five node spans, each keyed by its occurrence's whole identity, with its
/// own tool beneath it. Starting the process again under its key serves the
/// retained process and exports none. Fails while nodes are not projected.
#[tokio::test]
async fn workflow_occurrences_are_distinct_nodes() {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    let (core, exported) = traced_core(stores, "nodes", TraceDetail::Extended).await;
    let (env_ref, definition) = publish_nodes(&core).await;
    let args = serde_json::Map::from_iter([("name".into(), serde_json::json!("n"))]);
    let process = start_with(&core, &env_ref, &definition, "nodes", args.clone()).await;
    assert_eq!(
        finished(&core, &process).await,
        serde_json::json!(["x", "y"])
    );
    exported.until("invoke_workflow", 1).await;
    let nodes = exported.spans("lash.workflow.node");
    assert_five_occurrences(&nodes, &exported.all());

    let again = start_with(&core, &env_ref, &definition, "nodes", args).await;
    assert_eq!(again, process, "the key serves the retained process");
    core.flush_trace_sink().expect("flush the trace sink");
    assert_eq!(
        exported.spans("lash.workflow.node").len(),
        8,
        "a served start exports no node"
    );
    core.shutdown().await.expect("shutdown");
}

/// L21(g): whether a process exports node spans is fixed at its
/// registration. Registered on a node whose policy builds workflow nodes,
/// parked on its timer and completed on a node whose policy does not, its
/// occurrences on the second node are still exported, by that node. Fails
/// when the closing owner decides from its own policy.
#[tokio::test]
async fn the_registered_node_decision_wins_over_the_closing_owners_policy() {
    let files = Files::new();
    let (first, first_exported) =
        traced_core(files.open().await, "registering", TraceDetail::Extended).await;
    let (env_ref, definition) = publish_nodes(&first).await;
    let args = serde_json::Map::from_iter([("name".into(), serde_json::json!("n"))]);
    let process = start_with(&first, &env_ref, &definition, "owners", args).await;
    let due = timer_park_after(&first, &process, 0).await;
    first.flush_trace_sink().expect("flush the trace sink");
    first.shutdown().await.expect("the first node stops");
    drop(first);

    files.clock.set(due);
    let (second, second_exported) =
        traced_core(files.open().await, "completing", TraceDetail::Standard).await;
    // The second and third turns park on their timers on this node.
    let mut due = due;
    for _ in 0..2 {
        due = timer_park_after(&second, &process, due).await;
        files.clock.set(due);
    }
    assert_eq!(
        finished(&second, &process).await,
        serde_json::json!(["x", "y"])
    );
    second_exported.until("invoke_workflow", 1).await;
    let completed_here = second_exported.spans("lash.workflow.node");
    assert_eq!(
        completed_here.len(),
        7,
        "the completing owner exports the occurrences it closed: {completed_here:#?}"
    );
    let first_nodes = first_exported.spans("lash.workflow.node");
    assert_eq!(
        first_nodes.len(),
        1,
        "the registering owner exports the occurrence it closed before the park"
    );
    let nodes: Vec<_> = first_nodes.into_iter().chain(completed_here).collect();
    let every: Vec<_> = first_exported
        .all()
        .into_iter()
        .chain(second_exported.all())
        .collect();
    assert_five_occurrences(&nodes, &every);
    second.shutdown().await.expect("shutdown");
}

/// DEFINITION-WAITS: a definition start names each store and engine operation it waits on.
#[tokio::test]
async fn a_definition_start_attributes_loading_resolution_acquisition_and_claim_verification() {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite"),
    );
    let (core, exported) = traced_core(stores, "definition-probe", TraceDetail::Standard).await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let env = source_environment(false);
    let source = "export async function run(name: string): Promise<string> { const values = [name, name.toUpperCase()]; return values.join(\",\"); }";
    let lash::workflow::WorkflowImport::Imported { document, .. } = artifacts
        .import_workflow(
            lash::workflow::document::SourceLanguage::TypeScript,
            source,
            &namespace(&core, &pin, &env).await.1,
            None,
        )
        .await
        .expect("import")
    else {
        panic!("import refused")
    };
    let publication = publish(&core, &pin, &document, &Name::new("run"), &env, None)
        .await
        .expect("publish");
    let env_ref = publication.env_ref.clone();
    let process = start(
        &core,
        &env_ref,
        &publication.definition,
        "definition-timings",
        "operator",
    )
    .await;
    let starts = exported.spans("lash.process.start");
    let phases: Vec<_> = starts
        .last()
        .expect("start span")
        .attributes
        .iter()
        .filter(|a| a.key.as_str().starts_with("lash.process.start."))
        .collect();
    for step in [
        "definition_load",
        "definition_resolve",
        "definition_acquire",
        "definition_verify",
    ] {
        let key = format!("lash.process.start.{step}_us");
        assert!(
            phases.iter().any(|a| a.key.as_str() == key),
            "missing {key}: {phases:?}"
        );
    }
    let spent: i64 = phases
        .iter()
        .map(|attribute| {
            let lash::tracing::otel::Value::I64(value) = attribute.value else {
                panic!("phase duration is an integer: {attribute:?}")
            };
            assert!(value >= 0, "phase duration is nonnegative");
            value
        })
        .sum();
    let span = starts.last().expect("the start span");
    let span_us = i64::try_from(
        span.end_time
            .duration_since(span.start_time)
            .expect("span duration")
            .as_micros(),
    )
    .expect("short span");
    // Exported span clocks have whole-millisecond resolution. These are
    // disjoint phases, so their sum must fit in the containing start.
    assert!(
        spent <= span_us + 1_000,
        "phase sum {spent} exceeds start {span_us}"
    );
    assert_eq!(
        finished(&core, &process).await,
        serde_json::json!("operator,OPERATOR")
    );
    core.shutdown().await.expect("shutdown");
}
