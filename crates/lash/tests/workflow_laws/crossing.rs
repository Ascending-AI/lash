//! A function a process hands to a tool crosses as a self-contained frozen
//! function value (`K-EFF-002`, `K-CLO-004`; FIG-6381), and its document
//! runs on its own.

use super::*;
use lash::workflow::document::{FunctionValue, FunctionValueLimits};

/// A node whose worker pool crosses functions, as C1c's tool contract will
/// select for every pool.
async fn crossing_core() -> lash::LashCore {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite stores"),
    );
    let defaults = lash::vm::WorkerService::default();
    let mut config = defaults.config().clone();
    config.tuning.crosses_functions = true;
    builder_on_workers(
        stores,
        false,
        EchoComponent::Installed,
        lash::vm::WorkerService::new(config),
    )
    .await
    .build(node_identity("crossing"))
    .expect("the core builds")
}

/// A TypeScript workflow's entry hands `tools.echo` a recursive function
/// that reads a list it shares, then changes the list.
const SHIPS: &str = "export async function ships(marker: string) {\n  const shared = [1];\n  const fact = async (n: number): Promise<number> => n <= 1 ? shared[0] : n * (await fact(n - 1));\n  await tools.echo({ value: marker, nested: { fact } });\n  shared[0] = 2;\n  return marker;\n}\n";

/// FN-CROSS in a process, and FN-FREEZE: the process's tool reads the
/// function as a function value. Its captures were frozen when it crossed,
/// so the list's later change is not in it, and its recursion runs from
/// the small document it carries, published and started on its own. Main
/// raised `not_data` at the `perform`.
#[tokio::test(flavor = "multi_thread")]
async fn a_function_a_process_ships_runs_from_its_own_document_as_it_was() {
    let core = crossing_core().await;
    let artifacts = core.host_artifacts();
    let pin = lash::process::HostArtifactPin::mint();
    let environment = environment(false);
    let lash::workflow::WorkflowImport::Imported { document, .. } = artifacts
        .import_workflow(
            lash::workflow::document::SourceLanguage::TypeScript,
            SHIPS,
            &environment,
            None,
        )
        .await
        .expect("import answers")
    else {
        panic!("the workflow imports");
    };
    let publication = published(
        artifacts
            .publish_workflow(&pin, &document, &Name::new("ships"), &environment, None)
            .await
            .expect("publish"),
    );
    let env_ref = artifacts
        .publish_process_env(&pin, &linking(&environment, &["tools.echo"]))
        .await
        .expect("publish the process environment");
    let marker = "fn-cross-process";
    let mut args = serde_json::Map::new();
    args.insert("marker".to_owned(), serde_json::json!(marker));
    let process = start_with(&core, &env_ref, &publication.definition, "ships", args).await;
    assert_eq!(finished(&core, &process).await, serde_json::json!(marker));

    let shipped = ECHO_CALLS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .iter()
        .find(|args| args["value"] == marker)
        .expect("the process called the tool")["nested"]["fact"]
        .clone();
    let functions = lash::vm::standard_functions().expect("the build's library");
    let value = FunctionValue::from_transport_json(
        &shipped,
        functions.as_ref(),
        FunctionValueLimits::standard(),
        lash::persistence::KernelDataLimits::standard(),
    )
    .unwrap_or_else(|refusal| panic!("a function value: {refusal}\n{shipped}"));

    let alone = published(
        artifacts
            .publish_workflow(
                &pin,
                &lash::workflow::document::DocumentBytes::from(value.document.clone()),
                &value.entry,
                &environment,
                None,
            )
            .await
            .expect("the function value's document publishes"),
    );
    let env_ref = artifacts
        .publish_process_env(&pin, &linking(&environment, &[]))
        .await
        .expect("publish the empty environment");
    let mut args = serde_json::Map::new();
    args.insert("n".to_owned(), serde_json::json!(4));
    let run = start_with(&core, &env_ref, &alone.definition, "fact", args).await;
    assert_eq!(
        finished(&core, &run).await,
        serde_json::json!(24),
        "4! over the list as it was when the function crossed"
    );
    core.shutdown().await.expect("the node stops");
}
