//! What a host reads of a workflow document holds without the build that
//! wrote it (FIG-6379): every library function a document's `use` lines
//! name is retained beside it, by identity, as long as the document is.

use std::sync::Arc;

use lash::schema::Name;

use lash::workflow::document::Document;
use lash::workflow::{WorkflowDocumentRead, WorkflowRead};

use super::*;

/// A document whose entry calls `collection.map`, whose body reaches other
/// library functions, then sleeps for an hour: a process of it stands
/// waiting with every helper it reached listed in its manifest.
fn holding() -> Document {
    let release = lash::vm::helper_release().unwrap();
    let root = *release
        .functions
        .iter()
        .find(|(_, function)| function.definition().name.as_str() == "collection.map")
        .unwrap()
        .0;
    let mut pending = vec![root];
    let mut seen = std::collections::BTreeSet::new();
    let mut definitions = String::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        let function = release.functions.get(&id).unwrap().definition();
        if let Some(body) = function.body() {
            pending.extend(body.functions.keys().copied());
        }
        if function.body().is_none() {
            definitions.push_str(&format!("native @{id}\n"));
        }
        definitions.push_str(&format!(
            "definition {}\n",
            serde_json::to_string(&lash_kernel_doc::print_definition(function)).unwrap()
        ));
    }
    assert!(seen.len() > 1, "collection.map reaches other helpers");
    let mut document = lash_kernel_doc::parse_document(&format!(
        "kernel 1\nnumbers float\nimport 0 operation typescript.sleep(milliseconds: Int) -> Null\n{definitions}main {{ let held = fn() {{ let same = fn(x) {{ return x }} let items = [\"first\",\"second\"] let out = invoke collection.map(items,same) do perform 0(3600000) as Null return out }} }}"
    )).unwrap();
    export_closure(
        &mut document,
        "held",
        &[],
        lash::schema::Type::List(Box::new(lash::schema::Type::Text)),
    );
    document
}

/// A node over `stores` whose kernel engine holds no library function: the
/// build after the one that wrote a document retired the helper release it
/// used.
pub(super) fn retired_reader(stores: Arc<lash_sqlite_store::SqliteStoreSet>) -> lash::LashCore {
    let backend = lash_conformance::backend_over(stores);
    let workers = lash::vm::WorkerService::default();
    let bounds = workers.config().run_bounds;
    lash::LashCore::standard_builder(backend)
        .plugin(Arc::new(
            lash::vm::KernelProcessPluginFactory::new(
                workers,
                lash::vm::TYPESCRIPT_DIALECT,
                bounds,
            )
            .with_library(Arc::new(
                lash::vm::WorkerLibrary::new(
                    &lash::workflow::document::FunctionRegistry::default(),
                    [],
                    &Default::default(),
                )
                .unwrap(),
            )),
        ))
        .data_retention(lash::DataRetention::standard())
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .build(lash_core::LeaseOwnerIdentity::opaque(
            lash_core::LeaseOwnerId::new("workflow-laws-reader"),
            lash_core::LeaseIncarnationId::new("workflow-laws-reader-boot"),
        ))
        .expect("the reader core builds")
}

/// DOC-HOLD (FIG-6379): a process's document stays drawable from what the
/// process itself retains. The host publishes under pin P, starts the
/// definition, and releases P; once P's edges are gone, a node of a build
/// that holds none of the document's helpers reads the running process's
/// document and derives its view. The declarations it draws with are the
/// ones the document was admitted against, read back by identity, not the
/// reading build's registry.
#[tokio::test]
async fn a_running_process_draws_its_view_after_its_pin_and_helper_release_are_gone() {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    let core = core_over(Arc::clone(&stores), "doc-hold").await;
    let artifacts = core.host_artifacts();
    let environment = environment(false);
    let document = holding();
    let entry = Name::new("held");
    let pin = lash::process::HostArtifactPin::mint();
    let publication = publish(
        &core,
        &pin,
        &lash::workflow::document::DocumentBytes::from(document.clone()),
        &entry,
        &environment,
        None,
    )
    .await
    .expect("the publication answers");
    let env_ref = publication.env_ref.clone();
    let process = start_with(
        &core,
        &env_ref,
        &publication.definition,
        "held",
        serde_json::Map::new(),
    )
    .await;
    timer_park_after(&core, &process, 0).await;

    // A document only P holds tells when P's cleanup has run: one cleanup
    // severs every edge of its referrer at once.
    let marker = lash::workflow::document::parse_document(&complete_answer("marker", 1))
        .expect("the marker parses");
    let marker = publish(
        &core,
        &pin,
        &lash::workflow::document::DocumentBytes::from(marker.clone()),
        &Name::new("marker"),
        &environment,
        None,
    )
    .await
    .expect("the marker publishes");
    artifacts.release(pin).await.expect("P is released");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while matches!(
        artifacts
            .execution_document(&marker.start)
            .await
            .expect("the marker reads"),
        WorkflowDocumentRead::Read(_)
    ) {
        assert!(
            std::time::Instant::now() < deadline,
            "P's cleanup never ran"
        );
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    let reader = retired_reader(stores);
    let WorkflowRead::Inspected(inspection) = reader
        .processes()
        .graph(&process)
        .await
        .expect("the reader reads the process")
    else {
        panic!("the running process has a document");
    };
    assert_eq!(inspection.definition, publication.definition);
    assert_eq!(inspection.document.document(), &document);
    assert_eq!(
        inspection
            .document
            .declarations()
            .keys()
            .collect::<Vec<_>>(),
        document.functions.keys().collect::<Vec<_>>(),
        "the reader draws with the declarations the document was admitted against"
    );
    let context = reader
        .host_artifacts()
        .workflow_view_context(
            &inspection.document,
            &publication.environment.execution().publications,
        )
        .await
        .unwrap();
    let view =
        lash::workflow::view::derive(&inspection.document, &context).expect("the view derives");
    assert!(
        view.dataflow.actions.iter().any(|action| matches!(
            &action.callee,
            Some(lash::workflow::view::CalleeView::Library { name, .. })
                if name.to_string() == "collection.map"
        )),
        "the view draws the helper call by its retained declaration"
    );
    let WorkflowDocumentRead::Read(read) = reader
        .host_artifacts()
        .execution_document(&publication.start)
        .await
        .expect("the execution document reads")
    else {
        panic!("the execution document is retained by its process");
    };
    assert_eq!(read.document(), &document);

    core.processes()
        .cancel(&process, core.effect_host())
        .await
        .expect("the cancel is requested");
    reader.shutdown().await.expect("the reader shuts down");
    core.shutdown().await.expect("shutdown");
}

/// HOST-DOC-BYTES: retained host documents run without touching JSON.
#[tokio::test]
async fn a_host_runs_opaque_document_bytes_without_json() {
    use lash::workflow::document::{DocumentBytes, document_work};
    let core = super::core().await;

    let pin = lash::process::HostArtifactPin::mint();
    let env = environment(false);
    let typed = lash::workflow::document::parse_document(&complete_answer("answer", 42))
        .expect("typed document");
    let bytes = DocumentBytes::from(typed);
    let received = DocumentBytes::from_parts(
        bytes.grammar_version(),
        bytes.id(),
        bytes.as_bytes().to_vec(),
    );
    let _ = document_work::take();
    let publication = publish(&core, &pin, &received, &Name::new("answer"), &env, None)
        .await
        .expect("publish bytes");
    assert_eq!(publication.document, bytes.id());
    let env_ref = publication.env_ref.clone();
    let process = start_with(
        &core,
        &env_ref,
        &publication.definition,
        "opaque-document",
        Default::default(),
    )
    .await;
    assert_eq!(
        super::finished(&core, &process).await,
        serde_json::json!(42)
    );
    let work = document_work::take();
    assert_eq!((work.json_encodes, work.json_decodes), (0, 0));
    core.shutdown().await.expect("shutdown");
}

/// Rule A (FIG-6563): publication acknowledges admission and its read address;
/// the live publication pin retains the document for an explicit read.
#[tokio::test]
async fn a_publication_acknowledges_admission_and_reads_its_pinned_document() {
    let core = super::core().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let document =
        lash::workflow::document::parse_document(&complete_answer("answer", 42)).expect("document");
    let Publication {
        definition,
        start: reference,
        source_map,
        ..
    } = publish(
        &core,
        &pin,
        &lash::workflow::document::DocumentBytes::from(document.clone()),
        &Name::new("answer"),
        &environment(false),
        None,
    )
    .await
    .expect("publish");
    let WorkflowDocumentRead::Read(read) = artifacts
        .execution_document(&reference)
        .await
        .expect("read publication")
    else {
        panic!("the publication pin retains its document");
    };
    assert_eq!(read.document(), &document);
    assert_eq!(read.reference(), &reference.into());
    assert_eq!(
        artifacts
            .get_definition(&definition.id)
            .await
            .expect("definition"),
        Some(definition)
    );
    assert_eq!(source_map, None);
    artifacts
        .release(pin)
        .await
        .expect("release publication pin");
    core.shutdown().await.expect("shutdown");
}

/// HOST-DOC-REFUSAL: bad versions, malformed tokens and identity tampering
/// are refused before publication, retaining their typed causes.
#[tokio::test]
async fn host_document_bytes_refuse_tampering_and_wrong_grammar_typed() {
    use lash::workflow::document::{DOCUMENT_GRAMMAR_VERSION, DocumentBytes, DocumentBytesRefusal};
    let core = super::core().await;

    let pin = lash::process::HostArtifactPin::mint();
    let env = environment(false);
    let bytes = DocumentBytes::from(Document::new(
        lash::workflow::document::NumberPolicy::BySpelling,
        Vec::new(),
    ));
    let other = DocumentBytes::from(
        lash::workflow::document::parse_document("kernel 1\nnumbers float\nmain { finish 1 }")
            .expect("other document"),
    );
    let cases = [
        (
            DocumentBytes::from_parts(DOCUMENT_GRAMMAR_VERSION + 1, bytes.id(), Vec::new()),
            "version",
        ),
        (
            DocumentBytes::from_parts(
                bytes.grammar_version(),
                other.id(),
                bytes.as_bytes().to_vec(),
            ),
            "identity",
        ),
        (
            DocumentBytes::from_parts(bytes.grammar_version(), bytes.id(), vec![0xff]),
            "tokens",
        ),
    ];
    // HOST-DOC-LIMIT: a host can narrow token recursion before reading.
    let limits = lash_kernel_value::data_limits::KernelDataLimits::new(64, 352, 0, 512, 128)
        .expect("narrow limits");
    assert!(matches!(
        other.decode(limits),
        Err(DocumentBytesRefusal::Tokens(
            lash::workflow::document::tokens::TokenError::Limit { .. }
        ))
    ));
    for (bad, expected) in cases {
        let lash::EmbedError::WorkflowBytes(refusal) =
            publish(&core, &pin, &bad, &Name::new("missing"), &env, None)
                .await
                .expect_err("typed refusal")
        else {
            panic!("bytes must be refused before entry admission")
        };
        match (expected, refusal) {
            ("version", DocumentBytesRefusal::GrammarVersion { found, supported }) => {
                assert_eq!(found, DOCUMENT_GRAMMAR_VERSION + 1);
                assert_eq!(supported, DOCUMENT_GRAMMAR_VERSION);
            }
            ("identity", DocumentBytesRefusal::Identity { .. }) => {}
            ("tokens", DocumentBytesRefusal::Tokens(_)) => {}
            other => panic!("wrong typed refusal: {other:?}"),
        }
    }
    core.shutdown().await.expect("shutdown");
}

fn complete_answer(name: &str, value: i64) -> String {
    let definition = lash_kernel_doc::parse_definition(&format!(
        "function {name}() -> Int\nkernel 1\nmetering statements\nbody {{return {value}}}"
    ))
    .unwrap();
    lash_kernel_doc::print_document(&function_document(definition, name))
}

/// K-CALLABLE-VIEW: a saved cell callable opens its defining code and frozen environment.
#[cfg(feature = "typescript")]
#[tokio::test]
async fn a_saved_cell_callable_has_a_checked_view_of_its_frozen_environment() {
    use lash::workflow::document::{CaptureValue, Datum, FunctionValue, Node};
    let core = super::core().await;
    let pin = lash::process::HostArtifactPin::mint();
    let (_, request) = namespace(&core, &pin, &source_environment(false)).await;
    let lowered = cell_with_implicit_text(
        "const frozen = 7; const helper = (x: number): number => x + 1; const answer = (x: number, y: string, z: boolean): number => { const nested = (n: number): number => helper(n) + frozen; return nested(x); };",
    );
    let start = core
        .host_artifacts()
        .publish_workflow(
            &pin,
            &lowered.document.clone().into(),
            &lash::package::StartSelector::Main,
            &request,
            None,
            None,
        )
        .await
        .unwrap();
    let functions = lowered
        .written
        .iter()
        .filter_map(|(site, written)| {
            let Some(Node::Expr(Expr::Closure(closure))) = lowered.document.node(site) else {
                return None;
            };
            let presentation = closure.presentation.as_ref().unwrap();
            let name = written
                .signature_origin
                .as_ref()
                .and_then(|origin| origin.name.clone())
                .unwrap();
            Some((
                name,
                FunctionValue {
                    written_signature: Some(lash::workflow::document::WrittenSignature {
                        language: written.language,
                        signature: written.signature.clone(),
                    }),
                    signature_origin: written.signature_origin.clone(),
                    implicit_text_conversions: written.implicit_text_conversions.clone(),
                    callable: lash::package::CallableId {
                        link: start.link,
                        function: closure.function,
                    },
                    captures: Default::default(),
                    signature: presentation.signature.clone(),
                    convention: presentation.convention.clone(),
                    captions: lowered.captions.clone(),
                    representation: presentation.representation.clone(),
                    creation: Some(site.clone()),
                    token: Some(vec![
                        Datum::Text("ts.function".into()),
                        Datum::Text(String::new()),
                        Datum::Text("saved".into()),
                        Datum::Float(lash::workflow::document::Float::new(1.0)),
                        Datum::Null,
                        Datum::Record(Vec::new()),
                    ]),
                },
            ))
        })
        .collect::<Vec<_>>();
    let mut function = functions
        .iter()
        .find(|(name, _)| name.contains("answer"))
        .unwrap()
        .1
        .clone();
    let helper = functions
        .iter()
        .find(|(name, _)| name.contains("helper"))
        .unwrap()
        .1
        .clone();
    function.captures.insert(
        Name::new("frozen"),
        CaptureValue::Data(Datum::Float(lash::workflow::document::Float::new(7.0))),
    );
    function.captures.insert(
        Name::new("helper"),
        CaptureValue::Function(Box::new(helper)),
    );
    let origin = lash::package::CallableOrigin {
        publications: request
            .snapshot
            .advertisements
            .iter()
            .map(|ad| ad.publication.key())
            .collect(),
        authored: [start.link].into(),
    };
    let definition = core
        .host_artifacts()
        .publish_callable(&pin, &function, &request, &origin, None)
        .await
        .unwrap();
    let WorkflowRead::Inspected(inspection) = core
        .host_artifacts()
        .definition_graph(&definition.id)
        .await
        .unwrap()
    else {
        panic!("a retained callable has a workflow document");
    };
    assert_eq!(
        inspection.document.entry_unit(),
        Unit::Library(function.callable.function)
    );
    assert_eq!(
        inspection.document.entry_signature(),
        Some(&function.signature)
    );
    assert_eq!(inspection.document.callable(), Some(&function));
    assert_eq!(inspection.document.captures(), Some(&function.captures));
    assert_eq!(inspection.document.selector(), None);
    assert!(inspection.document.graph().session_reads().is_empty());
    let captures = inspection
        .document
        .graph()
        .bindings()
        .iter()
        .filter(|binding| binding.kind == lash_kernel_check::BindingKind::Capture)
        .map(|binding| binding.name.clone())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(captures, function.captures.keys().cloned().collect());
    let context = core
        .host_artifacts()
        .workflow_view_context(&inspection.document, &[])
        .await
        .unwrap();
    let view = lash::workflow::view::derive(&inspection.document, &context).unwrap();
    assert!(!view.canonical_source.is_empty());
    assert_eq!(view.entry.written_signature, function.written_signature);
    assert_eq!(
        view.entry.params.len(),
        3,
        "K-CALLABLE-PRESENTATION: no public parameter is truncated to the private ABI"
    );
    assert_eq!(
        view.entry
            .params
            .iter()
            .map(|param| &param.name)
            .collect::<Vec<_>>(),
        function
            .signature
            .params
            .iter()
            .map(|param| &param.name)
            .collect::<Vec<_>>()
    );
    assert!(matches!(
        function.convention,
        lash::workflow::document::StartConvention::TsArgsList { .. }
    ));
    assert!(
        view.entry
            .params
            .iter()
            .all(|param| param.binding.is_none())
    );
    assert_eq!(view.entry.result, function.signature.result);
    assert!(
        view.callables.len() > 1,
        "the selected callable retains its lexical children"
    );
    core.shutdown().await.unwrap();
}
