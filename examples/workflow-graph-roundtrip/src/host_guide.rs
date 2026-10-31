//! Compiled host flow quoted by `docs/workflow-hosts.md`.

// ANCHOR: joined-packages
use std::{collections::BTreeSet, sync::Arc};

use lash::package::{JoinedToolPackageProvider, Segment, ToolPackageProvider};
use lash::plugins::{
    FormatVersion, PluginDeclaration, PluginError, PluginSpec, StaticPluginFactory,
};
use lash::process::ProcessConfigKey;
use lash::tools::ToolProvider;

pub fn with_cenote_packages(
    mut builder: lash::LashCoreBuilder,
    knowledge: Arc<dyn ToolProvider>,
    decision: Arc<dyn ToolProvider>,
) -> Result<lash::LashCoreBuilder, PluginError> {
    let key = lash::tool_provider_key!("tools");
    let root =
        Segment::new("cenote").map_err(|error| PluginError::Registration(error.to_string()))?;
    let roots: BTreeSet<_> = [root].into();
    let packages = JoinedToolPackageProvider::new(vec![
        ToolPackageProvider::new("knowledge", key, roots.clone(), knowledge.clone()),
        ToolPackageProvider::new("decision", key, roots, decision.clone()),
    ]);
    let registrations = [
        (
            "knowledge",
            PluginSpec::new().with_tool_provider(key, knowledge),
        ),
        (
            "decision",
            PluginSpec::new().with_tool_provider(key, decision),
        ),
        (
            "cenote_packages",
            PluginSpec::new()
                .with_package_provider(lash::tool_provider_key!("code"), Arc::new(packages)),
        ),
    ];
    for (owner, spec) in registrations {
        let component = StaticPluginFactory::new(PluginDeclaration::initial(owner), spec)
            .as_component(FormatVersion::ONE)?;
        builder = builder.process_component(
            Arc::new(component),
            ProcessConfigKey::<()>::new(owner, FormatVersion::ONE),
        );
    }
    Ok(builder)
}
// ANCHOR_END: joined-packages

// ANCHOR: namespace
use std::collections::BTreeMap;

use lash::LashCore;
use lash::dialect::workflow::WorkflowSourceLimits;
use lash::package::{
    Attachment, CallableOrigin, LinkedStart, PublicationContext, RecordedPackages, StartSelector,
};
use lash::process::{
    EngineLaunch, HostArtifactPin, Lifetime, ProcessDefinition, ProcessEnv, ProcessOriginator,
    ProcessStartRequest,
};
use lash::schema::Name;
use lash::vm::WorkflowEnvironmentRequest;
use lash::workflow::document::{Captions, FunctionValue, SourceLanguage, SourceMapId};
use lash::workflow::{SourceMapRead, WorkflowImport, WorkflowRead};

// Public vocabulary has one home: BuildHelpers, LibraryDependencies and
// WrittenSignature are in lash::workflow::document, including for viewers
// without code mode. WorkflowReference is in
// lash::workflow.

pub async fn namespace(
    core: &LashCore,
    pin: &HostArtifactPin,
    base: &ProcessEnv,
    context: PublicationContext,
    attachment: Attachment,
) -> lash::Result<(RecordedPackages, WorkflowEnvironmentRequest)> {
    let packages = core
        .host_artifacts()
        .process_namespace(pin, base, context.clone())
        .await?;
    let request = WorkflowEnvironmentRequest {
        snapshot: packages.snapshot.clone(),
        context,
        attachment,
    };
    Ok((packages, request))
}
// ANCHOR_END: namespace

// ANCHOR: publish
pub struct SavedWorkflow {
    pub start: LinkedStart,
    pub definition: ProcessDefinition,
    pub environment: ProcessEnv,
    pub source_map: SourceMapId,
    pub captions: Captions,
}

pub async fn publish(
    core: &LashCore,
    pin: &HostArtifactPin,
    base: &ProcessEnv,
    namespace: &(RecordedPackages, WorkflowEnvironmentRequest),
    source: &str,
    entry: Name,
    limits: Option<WorkflowSourceLimits>,
) -> anyhow::Result<SavedWorkflow> {
    let artifacts = core.host_artifacts();
    let WorkflowImport::Imported {
        environment,
        document,
        source_map,
        source,
        captions,
    } = artifacts
        .import_workflow(SourceLanguage::TypeScript, source, &namespace.1, limits)
        .await?
    else {
        anyhow::bail!("source was refused or its dialect is unavailable");
    };
    let start = artifacts
        .publish_workflow(
            pin,
            &document,
            &StartSelector::Export(entry),
            &environment,
            Some((&source_map, &source)),
            limits,
        )
        .await?;
    // Supply selections by OperationUse here if providers are ambiguous.
    let environment = artifacts
        .link_process_env(
            base,
            &lash::process::LinkTarget::Linked(start.clone()),
            &namespace.0,
            &BTreeMap::new(),
        )
        .await?;
    let draft = lash::vm::definition_draft(&lash::process::KernelProcessDefinition {
        code: lash::vm::KernelInlineRequest::Linked {
            start: start.clone(),
        },
    })?;
    let definition = artifacts.publish_definition(pin, &draft).await?;
    Ok(SavedWorkflow {
        start,
        definition,
        environment,
        source_map: source_map.identity(),
        captions,
    })
}

pub async fn start(
    core: &LashCore,
    pin: &HostArtifactPin,
    saved: &SavedWorkflow,
    key: &str,
    args: serde_json::Map<String, serde_json::Value>,
) -> lash::Result<lash::persistence::ProcessStartReceipt> {
    let env = core
        .host_artifacts()
        .publish_process_env(pin, &saved.environment)
        .await?;
    let settings = core
        .processes()
        .kernel_engine_defaults()?
        .into_engine_settings();
    let request = ProcessStartRequest::definition(
        saved.definition.id.clone(),
        Some(saved.definition.signature.clone()),
        args,
        EngineLaunch::new(env, Some(settings)),
        ProcessOriginator::host(),
        Lifetime::Detached,
    );
    core.processes()
        .start(lash::OperationId::new(key), request)
        .await
}
// ANCHOR_END: publish

// ANCHOR: read
pub async fn read(
    core: &LashCore,
    saved: &SavedWorkflow,
) -> anyhow::Result<(lash::workflow::view::WorkflowView, SourceMapRead)> {
    let artifacts = core.host_artifacts();
    let WorkflowRead::Inspected(inspection) =
        artifacts.definition_graph(&saved.definition.id).await?
    else {
        anyhow::bail!("definition is unavailable or its engine has no document");
    };
    let context = artifacts
        .workflow_view_context(
            &inspection.document,
            &saved.environment.execution().publications,
        )
        .await?;
    let view = lash::workflow::view::derive(&inspection.document, &context)?;
    let _caption_view = view.captions(&inspection.document, &saved.captions)?;
    let source = artifacts.read_source_map(&saved.source_map).await?;
    Ok((view, source))
}
// ANCHOR_END: read

// ANCHOR: callable
pub async fn save_callable(
    core: &LashCore,
    pin: &HostArtifactPin,
    function: &FunctionValue,
    environment: &WorkflowEnvironmentRequest,
    attempt: &lash::tools::AttemptContext<'_>,
    limits: Option<WorkflowSourceLimits>,
    review: impl FnOnce(&lash::package::LinkInspection) -> anyhow::Result<()>,
) -> anyhow::Result<ProcessDefinition> {
    let origin: CallableOrigin = attempt.callable_origin()?;
    let artifacts = core.host_artifacts();
    let (_definition, inspection) = artifacts
        .inspect_callable(function, environment, &origin)
        .await?;
    // Bind accounts or refuse operations according to the host's policy before saving.
    review(&inspection)?;
    Ok(artifacts
        .publish_callable(pin, function, environment, &origin, limits)
        .await?)
}
// ANCHOR_END: callable

// ANCHOR: authoring
pub async fn edit_saved(
    core: &LashCore,
    pin: &HostArtifactPin,
    definition: &lash::process::ProcessDefinitionId,
    environment: &WorkflowEnvironmentRequest,
    edits: Vec<lash::workflow::authoring::Operation>,
) -> anyhow::Result<ProcessDefinition> {
    let artifacts = core.host_artifacts();
    let mut opened = artifacts.open_workflow(definition, None).await?;
    let checked = artifacts
        .workflow_environment(environment)
        .await?
        .ok_or_else(|| anyhow::anyhow!("workflow engine unavailable"))?;
    opened.draft.apply(
        &lash::workflow::edit::Transaction {
            base: opened.draft.identity(),
            edits,
        },
        &checked.checker(),
        lash::workflow::edit::CONVENTIONS,
    )?;
    let start = artifacts
        .publish_workflow(
            pin,
            &opened.draft.into_document().into(),
            &opened.selector,
            environment,
            None,
            None,
        )
        .await?;
    let draft = lash::vm::definition_draft(&lash::process::KernelProcessDefinition {
        code: lash::vm::KernelInlineRequest::Linked { start },
    })?;
    Ok(artifacts.publish_definition(pin, &draft).await?)
}

pub async fn print_saved(
    core: &LashCore,
    definition: &lash::process::ProcessDefinitionId,
    captions: Option<&Captions>,
    format: lash::dialect::SourceFormat,
) -> lash::Result<lash::dialect::PrintedSource> {
    core.host_artifacts()
        .print_workflow(definition, captions, format, Default::default())
        .await
}
// The host edits PrintedSource::source, chooses an entry and calls publish
// above (or import_workflow for Python) under its chosen namespace. No original
// definition, saved captures or edit token is an input to that compilation.
// ANCHOR_END: authoring

#[cfg(test)]
mod tests {
    /// HOST-GUIDE-COMPILED: every Rust fence is an exact, ordered quotation
    /// of an anchored region in this compiled module, with no ignored fences.
    #[test]
    fn every_host_guide_snippet_is_compiled() {
        let source = include_str!("host_guide.rs");
        let guide = include_str!("../../../docs/workflow-hosts.md");
        let mut expected = Vec::new();
        for region in source.split("\n// ANCHOR: ").skip(1) {
            let (name, rest) = region.split_once('\n').unwrap();
            let (body, end) = rest.split_once("\n// ANCHOR_END: ").unwrap();
            assert_eq!(end.lines().next(), Some(name));
            expected.push((name, body.trim()));
        }
        assert!(!expected.is_empty());
        let mut quoted = Vec::new();
        let mut lines = guide.lines();
        let mut anchor = None;
        while let Some(line) = lines.next() {
            if let Some(name) = line
                .strip_prefix("<!-- host-guide: ")
                .and_then(|name| name.strip_suffix(" -->"))
            {
                assert!(anchor.replace(name).is_none(), "unused snippet anchor");
            } else if line.starts_with("```rust") {
                assert_eq!(line, "```rust", "Rust fences must be compiled quotations");
                let name = anchor.take().expect("Rust fence needs a source anchor");
                let mut body = Vec::new();
                loop {
                    let line = lines.next().expect("unclosed Rust fence");
                    if line == "```" {
                        break;
                    }
                    body.push(line);
                }
                quoted.push((name, body.join("\n")));
            }
        }
        assert!(anchor.is_none(), "unused snippet anchor");
        let quoted = quoted
            .iter()
            .map(|(name, body)| (*name, body.trim()))
            .collect::<Vec<_>>();
        assert_eq!(quoted, expected, "refresh the guide from host_guide.rs");
    }
}
