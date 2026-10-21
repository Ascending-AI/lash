//! Example host policy for workflows.create: code is the model's input;
//! attachment is the handler's job, derived from typed requirements.
use crate::plugins::*;
use crate::tools::*;
use std::sync::Arc;

pub(super) struct WorkflowCreator {
    pub artifacts: crate::process::HostArtifacts,
    pub workers: crate::vm::WorkerService,
    pub packages: crate::package::CellPackages,
    pub environment: crate::process::ProcessEnv,
    pub language: crate::workflow::document::SourceLanguage,
    pub pin: crate::process::HostArtifactPin,
}

pub(super) fn definition() -> Result<ToolDefinition, PluginError> {
    ToolDefinition::raw(
        "host:workflows_create", "workflows.create", "Create a workflow from its code.",
        serde_json::json!({"type":"object", "properties":{"code":{"type":"string"}}, "required":["code"], "additionalProperties":false}),
        serde_json::json!({"type":"object"}),
    ).map(|draft| draft.with_execution(std::time::Duration::from_secs(120)))
        .map_err(|error| PluginError::Registration(error.to_string()))
}

impl WorkflowCreator {
    async fn create(&self, source: &str) -> Result<crate::workflow::WorkflowPublication, String> {
        let environment = self
            .artifacts
            .workflow_environment(&self.environment)
            .await
            .map_err(|error| error.to_string())?
            .ok_or("no workflow engine")?;
        // Inspection is a host choice over the offered surface. This does
        // not admit or store a definition and chooses nothing inside core.
        let (document, map, _) = self
            .workers
            .import_workflow(
                self.language,
                source,
                environment.operations(),
                crate::vm::SurfaceRef::of(self.packages.digest(), self.packages.stored_wire()),
                self.packages.surface().selection(),
                None,
            )
            .await
            .map_err(|error| error.to_string())?;
        let required =
            crate::workflow::graph::requirements(&document, self.packages.catalog().as_ref());
        let operations: std::collections::BTreeSet<_> = required
            .effects
            .iter()
            .chain(required.performs.keys())
            .filter_map(|effect| crate::package::QualifiedOperation::from_effect(effect).ok())
            .collect();
        let declared = self
            .packages
            .surface()
            .packages()
            .filter_map(|package| {
                let names: std::collections::BTreeSet<_> = operations
                    .iter()
                    .filter(|operation| &operation.package == package.path())
                    .map(|operation| operation.name.clone())
                    .collect();
                (package.ids().any(|id| required.functions.contains_key(&id)) || !names.is_empty())
                    .then(|| (package.path().clone(), names))
            })
            .collect();
        let entry = document
            .entries
            .keys()
            .next()
            .ok_or("no workflow entry")?
            .clone();
        let bytes = crate::workflow::document::DocumentBytes::from(document);
        match self
            .artifacts
            .publish_workflow(
                &self.pin,
                &bytes,
                &entry,
                &self.environment,
                self.packages.declare(declared),
                Some((&map, source)),
            )
            .await
            .map_err(|error| error.to_string())?
        {
            crate::workflow::WorkflowPublish::Published(publication) => Ok(*publication),
            refused => Err(format!("{refused:?}")),
        }
    }
}

#[crate::async_trait]
impl ToolProvider for WorkflowCreator {
    fn tool_manifests(&self, _: &OwnerConfigView) -> Result<Vec<ToolManifest>, PluginError> {
        Ok(vec![definition()?.manifest])
    }
    fn resolve_contract(&self, name: &str, _: &OwnerConfigView) -> Option<Arc<ToolContract>> {
        (name == "workflows.create")
            .then(|| {
                definition()
                    .ok()
                    .map(|definition| Arc::new(definition.contract))
            })
            .flatten()
    }
    async fn execute(&self, call: ToolCall<'_>) -> ToolAttemptOutcome {
        let Some(source) = call.args.get("code").and_then(serde_json::Value::as_str) else {
            return ToolOutcome::failure(ToolFailure::new("invalid_arguments", "code is required"))
                .into();
        };
        match self.create(source).await {
            Ok(publication) => {
                ToolOutcome::ok(serde_json::json!({"definition": publication.definition})).into()
            }
            Err(message) => {
                ToolOutcome::failure(ToolFailure::new("workflow_refused", message)).into()
            }
        }
    }
}

/// WF-HOST-MINIMAL: the tool's handler chooses attachment from typed code
/// requirements in both dialects. Its model-facing input contains code only.
#[tokio::test]
async fn the_workflow_create_handler_attaches_only_the_typed_requirements() {
    let built = lash_vm_releases::built().unwrap();
    let kernel = built
        .iter()
        .find(|package| package.path().to_string() == "kernel")
        .unwrap();
    let dataframe = crate::package::testing::dataframe(kernel).unwrap();
    let spare = crate::package::Package::build(
        "spare",
        "1",
        &crate::package::PackageSources {
            kernel: "",
            surface: "operation unused() -> Int contract \"spare/1\"",
            labels: "",
        },
        &[],
    )
    .unwrap();
    let held = Arc::new(
        crate::package::HeldPackages::install(built.into_iter().chain([dataframe, spare])).unwrap(),
    );
    let workers = crate::vm::WorkerService::default().with_packages(held.clone());
    let factory = Arc::new(
        crate::vm::KernelProcessPluginFactory::new(
            workers.clone(),
            crate::vm::TYPESCRIPT_DIALECT,
            workers.config().run_bounds,
        )
        .with_packages(held.clone()),
    );
    let harness = super::super::PluginHarness::new(factory, super::super::HostPolicy::standard())
        .await
        .unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        while harness
            .core()
            .unwrap()
            .node
            .registration
            .snapshot()
            .is_none()
        {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let environment = crate::process::ProcessEnv::builder(crate::MaxToolCalls::new(64))
        .build()
        .unwrap();
    let pin = crate::process::HostArtifactPin::mint();
    for (language, code) in [
        (
            crate::workflow::document::SourceLanguage::TypeScript,
            "export async function flow() { return dataframe.col(\"word\"); }",
        ),
        (
            crate::workflow::document::SourceLanguage::Python,
            "async def flow():\n    return dataframe.col(\"word\")\n",
        ),
    ] {
        let creator = WorkflowCreator {
            artifacts: harness.core().unwrap().host_artifacts(),
            workers: workers.clone(),
            packages: crate::package::CellPackages::held(&held),
            environment: environment.clone(),
            language,
            pin: pin.clone(),
        };
        let outcome = crate::testing::run_tool(
            &creator,
            "workflows.create",
            &serde_json::json!({"code": code}),
        )
        .await;
        let ToolAttemptOutcome::Done { result, intents } = outcome else {
            panic!("the create handler completes inline");
        };
        assert!(intents.is_empty());
        let output = result.into_output();
        assert!(output.is_success(), "{output:?}");
        let definition: crate::process::ProcessDefinition =
            serde_json::from_value(output.value_for_projection()["definition"].clone()).unwrap();
        let bytes = harness
            .core()
            .unwrap()
            .backend()
            .definition_store()
            .get_process_definition(&definition.id)
            .await
            .unwrap()
            .unwrap();
        let descriptor =
            crate::process::ProcessDefinitionDraft::from_store_bytes(&definition.id, &bytes)
                .unwrap();
        let declared: crate::package::PackageSelection =
            serde_json::from_value(descriptor.value().as_json()["declared_packages"].clone())
                .unwrap();
        assert!(!declared.contains_key(&crate::package::PackagePath::parse("spare").unwrap()));
        assert!(
            declared[&crate::package::PackagePath::parse("dataframe").unwrap()].is_empty(),
            "code-only use attaches no dataframe operations"
        );
        let crate::workflow::WorkflowRead::Inspected(read) = creator
            .artifacts
            .definition_graph(&definition.id)
            .await
            .unwrap()
        else {
            panic!("the declared definition links");
        };
        let required = crate::workflow::graph::requirements(
            read.document.document(),
            held.functions().as_ref(),
        );
        for path in declared.keys() {
            let package = held.surface().package(path).unwrap();
            assert!(
                package.ids().any(|id| required.functions.contains_key(&id)),
                "no unused package {path}"
            );
        }
    }
    let tool = definition().unwrap();
    assert_eq!(
        tool.contract.input_schema.canonical()["required"],
        serde_json::json!(["code"])
    );
}
