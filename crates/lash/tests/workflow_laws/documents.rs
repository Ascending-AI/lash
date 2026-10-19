//! What a host reads of a workflow document holds without the build that
//! wrote it (FIG-6379): every library function a document's `use` lines
//! name is retained beside it, by identity, as long as the document is.

use std::sync::Arc;

use lash::workflow::document::{Document, Name};
use lash::workflow::{WorkflowDocumentRead, WorkflowRead};

use super::{core_over, environment, linking, published, start_with, timer_park_after};

/// A document whose entry calls `collection.map`, whose body reaches other
/// library functions, then sleeps for an hour: a process of it stands
/// waiting with every helper it reached listed in its manifest.
fn holding() -> Document {
    let functions = lash::package::lash_packages()
        .map(|packages| std::sync::Arc::clone(packages.functions()))
        .expect("the shipped library assembles");
    let map = functions
        .iter()
        .find(|(_, function)| function.definition().name.to_string() == "collection.map")
        .map(|(identity, _)| *identity)
        .expect("the shipped library has collection.map");
    let mut document = lash::workflow::document::parse_document(&format!(
        r#"kernel 1
numbers float
use collection.map = @{map}
entry held() -> Any

fn held() {{
  let same = fn(x) {{ return x }}
  let items = ["first", "second"]
  let out = invoke collection.map(items, same)
  do sleep 3600000
  return out
}}

main {{
  finish null
}}
"#
    ))
    .expect("the document parses");
    document.manifest.functions =
        lash::workflow::graph::requirements(&document, functions.as_ref()).functions;
    assert!(
        document.manifest.functions.len() > 1,
        "collection.map's body reaches other helpers"
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
            .with_functions(Arc::new(lash::workflow::document::FunctionRegistry::new())),
        ))
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
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
    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(document.clone()),
                &entry,
                &environment,
                None,
            )
            .await
            .expect("the publication answers"),
    );
    let env_ref = artifacts
        .publish_process_env(
            &lash::process::HostArtifactPin::mint(),
            &linking(&environment, &[]),
        )
        .await
        .expect("publish the process environment");
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
    let marker = lash::workflow::document::parse_document(
        "kernel 1\nnumbers float\nentry marker() -> Any\nfn marker() { return 1 }\nmain { finish null }\n",
    )
    .expect("the marker parses");
    let marker = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(marker.clone()),
                &Name::new("marker"),
                &environment,
                None,
            )
            .await
            .expect("the marker publishes"),
    );
    artifacts.release(pin).await.expect("P is released");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    while matches!(
        artifacts
            .execution_document(&marker.reference)
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
        document.manifest.functions.keys().collect::<Vec<_>>(),
        "the reader draws with the declarations the document was admitted against"
    );
    let view = lash::workflow::view::derive(&inspection.document).expect("the view derives");
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
        .execution_document(&publication.reference)
        .await
        .expect("the execution document reads")
    else {
        panic!("the execution document is retained by its process");
    };
    assert_eq!(read.document.document(), &document);
    assert_eq!(read.rewritten_from, None, "nothing rewrote the document");

    core.processes()
        .cancel(&process, core.effect_host())
        .await
        .expect("the cancel is requested");
    reader.shutdown().await.expect("the reader shuts down");
    core.shutdown().await.expect("shutdown");
}

/// DOC-IMPORT (FIG-6379): an import is drawable before anything is
/// published, from the library declarations a runtime ships as data; a
/// declaration that is not the function its identity names is refused
/// before anything is derived from it.
#[cfg(feature = "typescript")]
#[tokio::test]
async fn an_unpublished_import_draws_from_shipped_declarations_and_refuses_a_corrupt_one() {
    use lash::workflow::document::{
        DocumentDeclarationRefusal, Formula, FunctionDeclarations, required_declarations,
    };
    use lash::workflow::{WorkflowDocument, WorkflowDocumentError, WorkflowImport};
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    let core = core_over(stores, "doc-import").await;
    let artifacts = core.host_artifacts();
    let environment = environment(false);
    let checked = artifacts
        .workflow_environment(&environment)
        .await
        .expect("the environment reads")
        .expect("the kernel engine reads workflow documents");
    let WorkflowImport::Imported {
        document: imported, ..
    } = artifacts
        .import_workflow(
            lash::workflow::document::SourceLanguage::TypeScript,
            "export async function digest(items: string[]) { const out = items.map((item) => item); return out; }",
            &environment,
            None,
        )
        .await
        .expect("the import answers")
    else {
        panic!("the source imports");
    };
    let document = imported.to_document().expect("explicit typed import read");
    let imported = lash::workflow::WorkflowDocument::derive(
        document.clone(),
        lash::workflow::WorkflowDocumentEntry::Main,
        checked.functions(),
    )
    .expect("import view");
    assert!(
        !document.manifest.functions.is_empty(),
        "the import uses helpers"
    );
    // A viewer receives the runtime's declarations as data, with no engine.
    let shipped: FunctionDeclarations = serde_json::from_str(
        &serde_json::to_string(&checked.functions().export_declarations())
            .expect("the declarations encode"),
    )
    .expect("the declarations decode");
    let used =
        required_declarations(&document, &shipped).expect("the import's helpers are shipped");
    let held = WorkflowDocument::new(document.clone(), used.clone())
        .expect("the import is held with its shipped declarations");
    assert_eq!(held.declarations(), &used);
    assert_eq!(
        lash::workflow::view::derive(&held).expect("the held import draws"),
        lash::workflow::view::derive(&imported).expect("the import draws")
    );

    let mut corrupt = used;
    let (function, definition) = corrupt.iter_mut().next().expect("a used helper");
    definition.charge = Formula::Constant(999_999);
    let function = *function;
    match WorkflowDocument::new(document.clone(), corrupt) {
        Err(WorkflowDocumentError::Declarations(DocumentDeclarationRefusal::Identity {
            function: refused,
            ..
        })) => assert_eq!(refused, function),
        other => panic!("a corrupt declaration is refused before derivation: {other:?}"),
    }
    let mut short: FunctionDeclarations = held.declarations().clone();
    short.remove(&function);
    assert!(
        matches!(
            WorkflowDocument::new(document, short),
            Err(WorkflowDocumentError::Declarations(
                DocumentDeclarationRefusal::Missing { .. }
            ))
        ),
        "a missing declaration is refused before derivation"
    );
    core.shutdown().await.expect("shutdown");
}

/// DOC-MAP (FIG-6379): one publication stores a host's source map beside
/// the source it maps and answers the map's id; reading the id back
/// answers the same pair, checked. A map of another source, another
/// document or a site the document lacks is refused, and nothing of that
/// publication is stored.
#[cfg(feature = "typescript")]
#[tokio::test]
async fn a_publication_holds_its_checked_source_map_and_refuses_one_that_does_not_fit() {
    use lash::workflow::document::{Site, SourceMapRefusal, SourceSite, Unit};
    use lash::workflow::{
        SourceMapRead, WorkflowDocumentEntry, WorkflowDocumentRef, WorkflowImport,
        WorkflowUnavailable,
    };
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a SQLite memory store set"),
    );
    let core = core_over(stores, "doc-map").await;
    let artifacts = core.host_artifacts();
    let environment = environment(false);
    let import = |source: &'static str| {
        let artifacts = artifacts.clone();
        let environment = environment.clone();
        async move {
            let WorkflowImport::Imported { document, .. } = artifacts
                .import_workflow(
                    lash::workflow::document::SourceLanguage::TypeScript,
                    source,
                    &environment,
                    None,
                )
                .await
                .expect("the import answers")
            else {
                panic!("the source imports: {source}");
            };
            document.to_document().expect("explicit typed import read")
        }
    };
    let document =
        import("export async function digest(items: string[]) { const out = items.map((item) => item); return out; }").await;
    let other = import("export async function digest(items: string[]) { return items; }").await;
    let entry = Name::new("digest");
    let printed = lash::typescript::print(
        &document,
        None,
        lash::dialect::PrintOptions {
            source_map: true,
            ..Default::default()
        },
    )
    .expect("the document prints");
    let map = printed
        .source_map
        .clone()
        .expect("the printer maps its source");

    let pin = lash::process::HostArtifactPin::mint();
    let refused = |publish| match publish {
        lash::workflow::WorkflowPublish::SourceMapRefused(refusal) => refusal,
        other => panic!("the map is refused: {other:?}"),
    };
    assert_eq!(
        refused(
            artifacts
                .publish_workflow(
                    &pin,
                    &lash::workflow::document::DocumentBytes::from(document.clone()),
                    &entry,
                    &environment,
                    Some((&map, "other source"))
                )
                .await
                .expect("the publication answers"),
        ),
        SourceMapRefusal::Digest
    );
    assert_eq!(
        refused(
            artifacts
                .publish_workflow(
                    &pin,
                    &lash::workflow::document::DocumentBytes::from(other.clone()),
                    &entry,
                    &environment,
                    Some((&map, &printed.source))
                )
                .await
                .expect("the publication answers"),
        ),
        SourceMapRefusal::Document
    );
    let mut stray = map.clone();
    let missing = Site::new(Unit::Main, [999]);
    stray.sites.push(SourceSite {
        site: missing.clone(),
        ranges: Vec::new(),
        implicit: None,
    });
    assert_eq!(
        refused(
            artifacts
                .publish_workflow(
                    &pin,
                    &lash::workflow::document::DocumentBytes::from(document.clone()),
                    &entry,
                    &environment,
                    Some((&stray, &printed.source))
                )
                .await
                .expect("the publication answers"),
        ),
        SourceMapRefusal::Site { site: missing }
    );
    for refused_document in [&document, &other] {
        let reference = WorkflowDocumentRef {
            document: refused_document.identity(),
            entry: WorkflowDocumentEntry::Main,
        };
        assert!(
            matches!(
                artifacts
                    .execution_document(&reference)
                    .await
                    .expect("the read answers"),
                lash::workflow::WorkflowDocumentRead::Unavailable(
                    WorkflowUnavailable::Artifact { .. }
                )
            ),
            "a refused publication stores nothing"
        );
    }

    let publication = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(document.clone()),
                &entry,
                &environment,
                Some((&map, &printed.source)),
            )
            .await
            .expect("the publication answers"),
    );
    let id = publication
        .source_map
        .expect("the publication names its map");
    assert_eq!(id, map.identity());
    assert_eq!(
        artifacts.read_source_map(&id).await.expect("the map reads"),
        SourceMapRead::Read {
            map: map.clone(),
            source: printed.source.clone(),
        }
    );
    artifacts.release(pin).await.expect("the pin is released");
    core.shutdown().await.expect("shutdown");
}

/// HOST-DOC-BYTES: retained host documents run without touching JSON.
#[tokio::test]
async fn a_host_runs_opaque_document_bytes_without_json() {
    use lash::workflow::document::{DocumentBytes, document_work};
    let core = super::core().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let env = environment(false);
    let typed = lash::workflow::document::parse_document(
        "kernel 1\nnumbers float\nentry answer() -> Any\nfn answer() { return 42 }\nmain { finish null }\n",
    ).expect("typed document");
    let bytes = DocumentBytes::from(typed);
    let received = DocumentBytes::from_parts(
        bytes.grammar_version(),
        bytes.id(),
        bytes.as_bytes().to_vec(),
    );
    let _ = document_work::take();
    let publication = published(
        artifacts
            .publish_workflow(&pin, &received, &Name::new("answer"), &env, None)
            .await
            .expect("publish bytes"),
    );
    assert_eq!(publication.reference.document, bytes.id());
    let env_ref = artifacts
        .publish_process_env(&pin, &linking(&env, &[]))
        .await
        .expect("publish env");
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
    let document = lash::workflow::document::parse_document(
        "kernel 1\nnumbers float\nentry answer() -> Any\nfn answer() { return 42 }\nmain { finish null }\n",
    )
    .expect("document");
    let lash::workflow::WorkflowPublication {
        definition,
        reference,
        source_map,
    } = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(document.clone()),
                &Name::new("answer"),
                &environment(false),
                None,
            )
            .await
            .expect("publish"),
    );
    let WorkflowDocumentRead::Read(read) = artifacts
        .execution_document(&reference)
        .await
        .expect("read publication")
    else {
        panic!("the publication pin retains its document");
    };
    assert_eq!(read.document.document(), &document);
    assert_eq!(read.document.reference(), &reference);
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

/// HOST-DOC-JSON: editing through typed JSON never contributes to identity.
#[test]
fn typed_json_round_trip_preserves_the_binary_document_id() {
    use lash::workflow::document::DocumentBytes;
    let document =
        lash::workflow::document::parse_document("kernel 1\nnumbers float\nmain { finish 42 }")
            .expect("document");
    let bytes = DocumentBytes::from(document);
    let json = bytes
        .to_document()
        .expect("read bytes")
        .to_json()
        .expect("readable JSON");
    let edited = Document::from_json(&json).expect("typed JSON");
    assert_eq!(DocumentBytes::from(edited).id(), bytes.id());
}

/// HOST-DOC-REFUSAL: bad versions, malformed tokens and identity tampering
/// are refused before publication, retaining their typed causes.
#[tokio::test]
async fn host_document_bytes_refuse_tampering_and_wrong_grammar_typed() {
    use lash::workflow::document::{DOCUMENT_GRAMMAR_VERSION, DocumentBytes, DocumentBytesRefusal};
    use lash::workflow::{WorkflowAdmissionRefusal, WorkflowPublish};
    let core = super::core().await;
    let artifacts = core.host_artifacts();
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
    let limits =
        lash::persistence::KernelDataLimits::new(64, 352, 0, 512, 128).expect("narrow limits");
    assert!(matches!(
        other.decode(limits),
        Err(DocumentBytesRefusal::Tokens(
            lash::workflow::document::graph_tokens::TokenError::Limit { .. }
        ))
    ));
    for (bad, expected) in cases {
        let WorkflowPublish::Refused(WorkflowAdmissionRefusal::Bytes(refusal)) = artifacts
            .publish_workflow(&pin, &bad, &Name::new("missing"), &env, None)
            .await
            .expect("typed answer")
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
