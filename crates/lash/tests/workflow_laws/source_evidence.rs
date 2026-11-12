//! FIG-7711: a workflow's declared types reach every worker that runs it.
//! A capture's printed source needs the declared type of the binding it
//! closes over (`const us: unknown[]`, printed `us: JsonValue[]`); a run
//! that saves nothing has only its publication's retained evidence to read
//! it from. Without it the capture prints `const us = [1, 2]`, which lowers
//! to another document, and publication refuses the readback.

use super::*;
use lash::dialect::SourceFormat;
use lash::workflow::document::FunctionValue;

/// A node whose capture tool admits a nested function, running every
/// process on one worker, over `stores`.
async fn evidence_core(
    stores: Arc<lash_sqlite_store::SqliteStoreSet>,
    boot: &str,
) -> lash::LashCore {
    let mut config = lash::vm::WorkerService::default().config().clone();
    config.min_workers = 1;
    config.max_workers = 1;
    builder_on_workers(
        stores,
        false,
        EchoComponent::Installed,
        lash::vm::WorkerService::new(config),
    )
    .await
    .process_component(
        Arc::new(super::crossing::capture_component(false)),
        super::crossing::CAPTURE_TOOLS,
    )
    .build(node_identity(boot))
    .expect("the core builds")
}

fn capture_env() -> lash::process::ProcessEnv {
    lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(super::crossing::CAPTURE_TOOLS, &())
        .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
        .build()
        .unwrap()
}

/// The workflow `entry` of `source`, published under `pin`.
async fn published(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    source: &str,
    entry: &str,
) -> super::support::Publication {
    let base = capture_env();
    let (packages, request) = namespace(core, pin, &base).await;
    let imported = core
        .host_artifacts()
        .import_workflow(
            SourceFormat::TypeScriptV1.language(),
            source,
            &request,
            None,
        )
        .await
        .unwrap();
    let lash::workflow::WorkflowImport::Imported { document, .. } = imported else {
        panic!("{imported:?}");
    };
    publish_recorded(
        core,
        pin,
        &document,
        &Name::new(entry),
        &base,
        None,
        (&packages, &request),
    )
    .await
    .unwrap()
}

/// The function the last call to `tools.capture` marked `marker` handed in.
fn captured(marker: &str) -> FunctionValue {
    let shipped = ECHO_CALLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .rev()
        .find(|args| args["value"] == marker)
        .unwrap_or_else(|| panic!("a capture marked {marker}"))["nested"]["fact"]
        .clone();
    FunctionValue::from_transport_json(
        &shipped,
        &lash::workflow::document::FunctionRegistry::default(),
        Default::default(),
        lash_kernel_value::data_limits::KernelDataLimits::standard(),
    )
    .unwrap()
}

/// Runs `publication`'s entry with `marker` to its end.
async fn ship(core: &lash::LashCore, publication: &super::support::Publication, marker: &str) {
    let run = start_with(
        core,
        &publication.env_ref,
        &publication.definition,
        marker,
        [("marker".into(), serde_json::json!(marker))]
            .into_iter()
            .collect(),
    )
    .await;
    assert_eq!(finished(core, &run).await, serde_json::json!(marker));
}

/// The capture marked `marker` publishes, prints with `annotation`, and its
/// printed source imports, publishes and answers `[1, 2]` for `x = 1`.
/// Answers the printed source.
async fn prints(
    core: &lash::LashCore,
    pin: &lash::process::HostArtifactPin,
    publication: &super::support::Publication,
    marker: &str,
    annotation: &str,
) -> String {
    let base = capture_env();
    let (packages, request) = namespace(core, pin, &base).await;
    let origin = lash::package::CallableOrigin {
        publications: publication
            .environment
            .execution()
            .publications
            .iter()
            .map(lash::package::PublicationRecord::key)
            .collect(),
        authored: publication.environment.execution().authored.clone(),
    };
    let definition = core
        .host_artifacts()
        .publish_callable(pin, &captured(marker), &request, &origin, None)
        .await
        .unwrap_or_else(|error| panic!("{marker} publication: {error:?}"));
    let printed = core
        .host_artifacts()
        .print_workflow(
            &definition.id,
            None,
            SourceFormat::TypeScriptV1,
            Default::default(),
        )
        .await
        .unwrap()
        .source;
    assert!(printed.contains(annotation), "{marker}\n{printed}");
    let imported = core
        .host_artifacts()
        .import_workflow(
            SourceFormat::TypeScriptV1.language(),
            &printed,
            &request,
            None,
        )
        .await
        .unwrap();
    let lash::workflow::WorkflowImport::Imported { document, .. } = imported else {
        panic!("{marker} reimport: {imported:?}\n{printed}");
    };
    let roundtrip = publish_recorded(
        core,
        pin,
        &document,
        &Name::new("run"),
        &base,
        None,
        (&packages, &request),
    )
    .await
    .unwrap();
    let run = start_with(
        core,
        &roundtrip.env_ref,
        &roundtrip.definition,
        &format!("{marker}-roundtrip"),
        [("x".into(), serde_json::json!(1))].into_iter().collect(),
    )
    .await;
    assert_eq!(
        finished(core, &run).await,
        serde_json::json!([1, 2]),
        "{marker}\n{printed}"
    );
    printed
}

/// `ships` captures an arrow over a binding declared `unknown[]`, and one
/// a nested arrow returns over its own such binding; `naps` captures the
/// first after a sleep.
const SHIPS: &str = r#"
export async function ships(marker: string): Promise<string> {
    const us: unknown[] = [1, 2];
    const annotated = async (x: number): Promise<unknown> => [x, us[1]];
    const make = async (): Promise<unknown> => {
        const vs: unknown[] = [1, 2];
        return async (x: number): Promise<unknown> => [x, vs[1]];
    };
    await tools.capture({value: marker, nested: {fact: annotated}});
    await tools.capture({value: `${marker}-nested`, nested: {fact: await make()}});
    return marker;
}

export async function naps(marker: string): Promise<string> {
    const us: unknown[] = [1, 2];
    const annotated = async (x: number): Promise<unknown> => [x, us[1]];
    await sleep(60000);
    await tools.capture({value: marker, nested: {fact: annotated}});
    return marker;
}
"#;

/// FIG-7711 (astra Q3): a capture over a declared binding prints that
/// declaration exactly with save absent, on a fresh worker, on a worker
/// that already holds the document (a cached start), after the store
/// reloads on a fresh worker, for a capture a nested arrow makes, and for
/// a run that resumes after the reload once its publication's pin is
/// released, holding the evidence itself.
#[tokio::test(flavor = "multi_thread")]
async fn annotated_captures_print_their_declaration_on_every_worker_path() {
    let files = Files::new();
    let core = evidence_core(files.open().await, "evidence-first").await;
    let pin = lash::process::HostArtifactPin::mint();
    let ships = published(&core, &pin, SHIPS, "ships").await;
    let naps = published(&core, &pin, SHIPS, "naps").await;
    let napping = start_with(
        &core,
        &naps.env_ref,
        &naps.definition,
        "evidence-naps",
        [("marker".into(), serde_json::json!("evidence-naps"))]
            .into_iter()
            .collect(),
    )
    .await;
    let due = timer_park_after(&core, &napping, 0).await;
    for marker in ["evidence-fresh", "evidence-cached"] {
        ship(&core, &ships, marker).await;
        Box::pin(prints(&core, &pin, &ships, marker, "us: JsonValue[]")).await;
        Box::pin(prints(
            &core,
            &pin,
            &ships,
            &format!("{marker}-nested"),
            "vs: JsonValue[]",
        ))
        .await;
    }
    core.host_artifacts().release(pin).await.unwrap();
    lash::testing::quiesce_artifact_cleanup(&core)
        .await
        .settle()
        .await
        .expect("pin cleanup");
    core.shutdown().await.unwrap();
    drop(core);

    files.clock.set(due);
    let core = evidence_core(files.open().await, "evidence-reload").await;
    let pin = lash::process::HostArtifactPin::mint();
    assert_eq!(
        finished(&core, &napping).await,
        serde_json::json!("evidence-naps")
    );
    Box::pin(prints(
        &core,
        &pin,
        &naps,
        "evidence-naps",
        "us: JsonValue[]",
    ))
    .await;
    let ships = published(&core, &pin, SHIPS, "ships").await;
    ship(&core, &ships, "evidence-reload").await;
    Box::pin(prints(
        &core,
        &pin,
        &ships,
        "evidence-reload",
        "us: JsonValue[]",
    ))
    .await;
    core.shutdown().await.unwrap();
}

/// `pairs` with each declaration of `u`: the executable document is the
/// same, the declared type alone differs.
fn pair(declared: &str) -> String {
    format!(
        "export async function pairs(marker: string): Promise<string> {{\n    const u: {declared} = 2;\n    const annotated = async (x: number): Promise<unknown> => [x, u];\n    await tools.capture({{value: marker, nested: {{fact: annotated}}}});\n    return marker;\n}}\n"
    )
}

/// FIG-7711: two publications of one executable document whose authors
/// declared a binding differently each print their own declaration, run
/// one after the other on one worker, in either order. Evidence is keyed
/// by publication, never by the document alone.
#[tokio::test(flavor = "multi_thread")]
async fn publications_of_one_document_print_their_own_declarations() {
    // Each declaration as written, and as the printer spells it.
    let unknown = ("unknown", "JsonValue");
    let number = ("number", "number");
    for (order, first, second) in [("ab", unknown, number), ("ba", number, unknown)] {
        let files = Files::new();
        let core = evidence_core(files.open().await, &format!("evidence-pair-{order}")).await;
        let pin = lash::process::HostArtifactPin::mint();
        let a = published(&core, &pin, &pair(first.0), "pairs").await;
        let b = published(&core, &pin, &pair(second.0), "pairs").await;
        assert_eq!(a.document, b.document, "one executable document");
        for (publication, (written, spelled), other) in [(&a, first, second), (&b, second, first)] {
            let marker = format!("evidence-pair-{order}-{written}");
            ship(&core, publication, &marker).await;
            let printed = Box::pin(prints(
                &core,
                &pin,
                publication,
                &marker,
                &format!("u: {spelled}"),
            ))
            .await;
            assert!(!printed.contains(&format!("u: {}", other.1)), "{printed}");
        }
        core.shutdown().await.unwrap();
    }
}
