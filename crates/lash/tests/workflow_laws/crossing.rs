//! A function a process hands to a tool crosses as a self-contained frozen
//! function value (`K-EFF-002`, `K-CLO-004`; FIG-6381), and its document
//! runs on its own.

use super::*;
use lash::workflow::document::{FunctionValue, FunctionValueLimits};

/// This tool's one callable slot is nested.fact; every other slot is data.
const CAPTURE_TOOLS: lash::process::ProcessConfigKey<()> =
    lash::process::ProcessConfigKey::new("workflow-capture", lash::plugins::FormatVersion::ONE);

fn capture_definition() -> lash::tools::ToolDefinition {
    lash::tools::ToolDefinition::raw(
        "tool:capture",
        "tools.capture",
        "Captures a nested function and answers its value.",
        serde_json::json!({
            "type": "object",
            "properties": {
                "value": {},
                "nested": {
                    "type": "object",
                    "properties": {"fact": {"x-lash": {"kind": "function"}}},
                    "required": ["fact"],
                    "additionalProperties": false
                }
            },
            "required": ["value", "nested"],
            "additionalProperties": false
        }),
        lash::tools::ToolDefinition::any_output(),
    )
    .expect("the function-taking contract")
    .with_execution(std::time::Duration::from_secs(120))
}

fn capture_component() -> lash::process::StaticProcessComponent {
    lash::plugins::StaticPluginFactory::new(
        lash::plugins::PluginDeclaration::initial(CAPTURE_TOOLS.component()),
        lash::plugins::PluginSpec::new()
            .with_package_provider(
                lash::tool_provider_key!("capture"),
                Arc::new(lash::package::ToolPackageProvider::new(
                    CAPTURE_TOOLS.component(),
                    lash::tool_provider_key!("capture"),
                    [lash::package::Segment::new("tools").unwrap()].into(),
                    Arc::new(lash::tools::StaticToolProvider::new(
                        vec![capture_definition()],
                        Echo,
                    )),
                )),
            )
            .with_tool_provider(
                lash::tool_provider_key!("capture"),
                Arc::new(lash::tools::StaticToolProvider::new(
                    vec![capture_definition()],
                    Echo,
                )),
            ),
    )
    .as_component(CAPTURE_TOOLS.format())
    .expect("the capture component")
}

/// A node whose capture tool explicitly admits a nested function.
async fn crossing_core() -> lash::LashCore {
    let stores = Arc::new(
        lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite stores"),
    );
    let defaults = lash::vm::WorkerService::default();
    let config = defaults.config().clone();
    builder_on_workers(
        stores,
        false,
        EchoComponent::Installed,
        lash::vm::WorkerService::new(config),
    )
    .await
    .process_component(Arc::new(capture_component()), CAPTURE_TOOLS)
    .build(node_identity("crossing"))
    .expect("the core builds")
}

/// A TypeScript workflow's entry hands `tools.capture` a recursive function
/// that reads a list it shares, then changes the list.
const SHIPS: &str = "export async function ships(marker: string): Promise<string> {\n  const shared = [1];\n  const fact = async (n: number): Promise<number> => n <= 1 ? shared[0] : n * (await fact(n - 1));\n  await tools.capture({ value: marker, nested: { fact } });\n  shared[0] = 2;\n  return marker;\n}\n";

/// Mutual lexical recursion has the same capture rule as self recursion.
const MUTUAL_SHIPS: &str = "export async function ships(marker: string): Promise<string> {\n  const shared = [1];\n  const fact = async (n: number): Promise<number> => n <= 0 ? shared[0] : await other(n - 1);\n  const other = async (n: number): Promise<number> => n <= 0 ? 0 : await fact(n - 1);\n  await tools.capture({ value: marker, nested: { fact } });\n  shared[0] = 2;\n  return marker;\n}\n";

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
    let environment = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
        .component(CAPTURE_TOOLS, &())
        .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
        .build()
        .expect("the capture environment");
    let (packages, request) = namespace(&core, &pin, &environment).await;
    let source = format!(
        "{SHIPS}\n{}",
        MUTUAL_SHIPS.replace("function ships(", "function ships_mutual(")
    );
    let lash::workflow::WorkflowImport::Imported { document, .. } = artifacts
        .import_workflow(
            lash::workflow::document::SourceLanguage::TypeScript,
            &source,
            &request,
            None,
        )
        .await
        .expect("import answers")
    else {
        panic!("the workflow imports");
    };
    for (entry, marker, expected, nodes) in [
        ("ships_mutual", "mutual-fn-cross-process", 1, 2),
        ("ships", "fn-cross-process", 24, 1),
    ] {
        let publication = publish_recorded(
            &core,
            &pin,
            &document,
            &Name::new(entry),
            &environment,
            None,
            (&packages, &request),
        )
        .await
        .expect("publish");
        let env_ref = publication.env_ref.clone();

        let mut args = serde_json::Map::new();
        args.insert("marker".to_owned(), serde_json::json!(marker));
        let process = start_with(&core, &env_ref, &publication.definition, entry, args).await;
        assert_eq!(finished(&core, &process).await, serde_json::json!(marker));

        let shipped = ECHO_CALLS
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .find(|args| args["value"] == marker)
            .expect("the process called the tool")["nested"]["fact"]
            .clone();
        let functions = lash::workflow::document::FunctionRegistry::default();
        let value = FunctionValue::from_transport_json(
            &shipped,
            &functions,
            FunctionValueLimits::standard(),
            lash_kernel_value::data_limits::KernelDataLimits::standard(),
        )
        .unwrap_or_else(|refusal| panic!("a function value: {refusal}\n{shipped}"));

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
        assert_eq!(
            value.nodes().len(),
            nodes,
            "recursive environments are encoded once"
        );
        let namespace = request.clone();
        let inspection = lash::vm::KernelDocuments::new(
            core.backend().module_artifacts(),
            core.backend().package_store(),
            std::sync::Arc::clone(lash::vm::WorkerLibrary::built().unwrap().helpers()),
        )
        .inspect_link(
            value.callable.link,
            &lash::package::ReachSelector::Callable {
                function: value.callable.function,
            },
        )
        .await
        .unwrap();
        let names: std::collections::BTreeSet<_> = inspection
            .operations
            .values()
            .map(|need| match need {
                lash::package::OperationNeed::Fixed { name, .. } => name.to_string(),
                lash::package::OperationNeed::Deferred { name, .. } => {
                    panic!("unexpected deferred import {name}")
                }
            })
            .collect();
        assert!(
            names.is_empty(),
            "K-DOC-002: arithmetic and list reads are pure; the callback inherits no operation from its enclosing capture call: {names:?}"
        );
        let mut execution = lash::package::ExecutionEnvironment::empty(
            lash_vocabulary::claims::RegistryKind::Process,
        );
        execution.publications = publication.environment.execution().publications.clone();
        execution.authored = origin.authored.clone();
        execution.roots.insert(value.callable.link);
        execution.requirements.machine = inspection.machine;
        execution.uses = publication
            .environment
            .execution()
            .uses
            .iter()
            .filter(|(use_, _)| inspection.operations.contains_key(use_))
            .map(|(use_, binding)| (*use_, binding.clone()))
            .collect();
        let environment = lash::process::ProcessEnv::builder(lash::MaxToolCalls::new(1024))
            .component(lash::vm::KERNEL_SERVICES_CONFIG, &())
            .execution(execution)
            .build()
            .unwrap();
        let env_ref = artifacts
            .publish_process_env(&pin, &environment)
            .await
            .unwrap();
        let request = lash::vm::KernelInlineRequest::Callable {
            link: value.callable.link,
            function: Box::new(value.clone()),
            origin,
        }
        .into_request(namespace.snapshot, namespace.context, namespace.attachment)
        .unwrap();
        let mut args = serde_json::Map::new();
        args.insert("n".to_owned(), serde_json::json!(4));
        let settings = core
            .processes()
            .engine_defaults(
                lash::process::LASH_VM_ENGINE_KIND,
                &value.transport_json().expect("transport"),
            )
            .expect("explicit kernel settings");
        let launch = lash_execution_contracts::process::EngineLaunch::new(env_ref, settings);
        let run = artifacts
            .start_function(
                request.clone(),
                args.clone(),
                &format!("held-factorial-{entry}"),
                launch.clone(),
            )
            .await
            .expect("start held value")
            .process_id;
        let retry = artifacts
            .start_function(request, args, &format!("held-factorial-{entry}"), launch)
            .await
            .expect("exact retry");
        assert_eq!(run, retry.process_id);
        assert_eq!(
            finished(&core, &run).await,
            serde_json::json!(expected),
            "4! over the list as it was when the function crossed"
        );
    }
    core.shutdown().await.expect("the node stops");
}
