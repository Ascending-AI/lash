//! Host authoring operations bind source to an authenticated stored descriptor.
use super::*;
use crate::dialect::{CallableSourceBase, PrintOptions, PrintedSource, SourceFormat};
use crate::workflow::source_edit::Replacement;
use crate::workflow::{WorkflowEdit, WorkflowEditRefusal, WorkflowInspection};
use lash_vm_runtime::WorkflowEnvironmentRequest;
use lash_vm_runtime::process::{InlineRequest, KernelProcessDefinition};
type Limits = Option<crate::dialect::workflow::WorkflowSourceLimits>;
fn source_error(error: lash_vm_client::service::SourceServiceError) -> crate::EmbedError {
    match error {
        lash_vm_client::service::SourceServiceError::Edit(error) => {
            WorkflowEditRefusal::Callable(error).into()
        }
        error => crate::EmbedError::WorkflowSource(error),
    }
}
impl HostArtifacts {
    async fn edit_base(&self, base: &WorkflowInspection) -> Result<InlineRequest> {
        let draft = self
            .definition_ports
            .read_definition_draft(&base.definition.id)
            .await?
            .ok_or(missing_definition(&base.definition.id))?;
        let stored = KernelProcessDefinition::from_payload(draft.value().as_json())
            .map_err(|e| lash_execution_contracts::PluginError::Session(e.to_string()))?;
        let expected = draft.id();
        let candidate = if let Some(function) = base.document.callable() {
            let InlineRequest::Callable { origin, .. } = &stored.code else {
                return Err(WorkflowEditRefusal::BaseMismatch {
                    expected,
                    actual: base.definition.id.clone(),
                }
                .into());
            };
            InlineRequest::Callable {
                link: function.callable.link,
                function: Box::new(function.clone()),
                origin: origin.clone(),
            }
        } else {
            let lash_kernel_doc::WorkflowReference::Linked(start) = base.document.reference()
            else {
                unreachable!("a workflow has one entry representation")
            };
            InlineRequest::Linked {
                start: start.clone(),
            }
        };
        let actual = lash_vm_runtime::process::definition_draft(&KernelProcessDefinition {
            code: candidate,
        })
        .map_err(|e| lash_execution_contracts::PluginError::Session(e.to_string()))?
        .id();
        let reference = base.document.reference().link();
        let content = self
            .documents()?
            .load_linked(reference)
            .await
            .map_err(|e| lash_execution_contracts::PluginError::Package(Box::new(e)))?;
        let retained = content
            .document(
                content
                    .link(reference)
                    .map_err(|e| {
                        lash_execution_contracts::PluginError::Package(Box::new(
                            lash_vm_runtime::map_link_refusal(e),
                        ))
                    })?
                    .document,
            )
            .map_err(|e| {
                lash_execution_contracts::PluginError::Package(Box::new(
                    lash_vm_runtime::map_link_refusal(e),
                ))
            })?;
        if actual != expected || retained != base.document.document() {
            return Err(WorkflowEditRefusal::BaseMismatch { expected, actual }.into());
        }
        Ok(stored.code)
    }
    /// Print an authenticated workflow with the host's caption sidecar, or
    /// the stored callable's captions when no sidecar is supplied. An override
    /// affects this print only and never changes the authenticated base.
    #[tracing::instrument(level = "debug", name = "lash.artifacts.print_workflow", skip_all, fields(definition_id = %base.definition.id, dialect = format.dialect()))]
    pub async fn print_workflow(
        &self,
        base: &WorkflowInspection,
        captions: Option<&crate::workflow::document::Captions>,
        format: SourceFormat,
        options: PrintOptions,
    ) -> Result<PrintedSource> {
        diagnostics::phase(
            "check",
            tracing::debug_span!("lash.artifacts.check"),
            async { self.edit_base(base).await },
        )
        .await?;
        diagnostics::phase(
            "print",
            tracing::debug_span!("lash.artifacts.print"),
            async {
                let service = self
                    .engines
                    .document_provider(lash_vm_runtime::LASH_VM_ENGINE_KIND)
                    .ok_or_else(|| {
                        lash_execution_contracts::PluginError::Registration(
                            "no source provider".into(),
                        )
                    })?
                    .environment(lash_execution_contracts::process::ProcessDocument::new(
                        lash_vm_runtime::process::WorkflowSourceServiceRequest,
                    ))
                    .await?
                    .downcast::<lash_vm_client::service::Service>()
                    .map_err(|_| {
                        lash_execution_contracts::PluginError::Registration(
                            "source provider returned another service".into(),
                        )
                    })?;
                if let Some(function) = base.document.callable() {
                    let mut function = function.clone();
                    if let Some(captions) = captions {
                        crate::dialect::check_captions(
                            base.document.document(),
                            captions,
                            options.limits,
                        )
                        .map_err(lash_vm_client::service::SourceServiceError::Print)
                        .map_err(source_error)?;
                        function.captions = captions.clone();
                    }
                    service
                        .print_callable(
                            &CallableSourceBase {
                                document: base.document.document().clone().into(),
                                function,
                            },
                            format,
                            options,
                        )
                        .await
                        .map_err(source_error)
                } else {
                    service
                        .print(base.document.document(), captions, format, options)
                        .await
                        .map_err(source_error)
                }
            },
        )
        .await
    }
    pub async fn edited_from_read(
        &self,
        base: &WorkflowInspection,
        source: &str,
        format: SourceFormat,
        environment: &WorkflowEnvironmentRequest,
        limits: Limits,
    ) -> Result<WorkflowEdit> {
        let code = self.edit_base(base).await?;
        let checked = self
            .workflow_environment(environment)
            .await?
            .ok_or_else(|| {
                crate::EmbedError::WorkflowSource(
                    lash_vm_client::service::SourceServiceError::UnknownDialect {
                        dialect: format.dialect().to_owned(),
                    },
                )
            })?;
        let replacement = if let Some(function) = base.document.callable() {
            let read = checked
                .worker_service()
                .read_callable(
                    source,
                    &CallableSourceBase {
                        document: base.document.document().clone().into(),
                        function: function.clone(),
                    },
                    format,
                    Default::default(),
                    limits,
                )
                .await
                .map_err(source_error)?;
            crate::dialect::callable::validate_read(
                &CallableSourceBase {
                    document: base.document.document().clone().into(),
                    function: function.clone(),
                },
                &read,
            )
            .map_err(WorkflowEditRefusal::Callable)?;
            Replacement::Callable(Box::new(read))
        } else {
            let (document, captions) = checked
                .worker_service()
                .read_exported(
                    source,
                    base.document.document(),
                    format,
                    Default::default(),
                    limits,
                )
                .await
                .map_err(source_error)?;
            Replacement::Linked {
                document: document.into(),
                captions,
            }
        };
        let dropped = match (&replacement, base.document.callable()) {
            (Replacement::Callable(read), Some(function)) => function
                .captions
                .entries()
                .iter()
                .filter(|entry| {
                    entry.target.site().is_some_and(|site| {
                        read.correspondence
                            .successor(site)
                            .is_none_or(|next| read.captions.at(next).is_none())
                    })
                })
                .map(|entry| entry.target.clone())
                .collect(),
            _ => Vec::new(),
        };
        Ok(WorkflowEdit {
            base: base.definition.id.clone(),
            code,
            replacement,
            source: source.to_owned(),
            format,
            dropped,
        })
    }
    pub async fn publish_workflow_edit(
        &self,
        pin: &HostArtifactPin,
        edit: &WorkflowEdit,
        environment: &WorkflowEnvironmentRequest,
        limits: Limits,
    ) -> Result<ProcessDefinition> {
        let crate::workflow::WorkflowRead::Inspected(base) =
            self.definition_graph(&edit.base).await?
        else {
            return Err(missing_definition(&edit.base).into());
        };
        // Re-read with the publish policy. The opaque edit never carries source
        // authority across a later policy or helper-library change.
        let authenticated = self
            .edited_from_read(&base, &edit.source, edit.format, environment, limits)
            .await?;
        if authenticated.code != edit.code {
            return Err(WorkflowEditRefusal::BaseMismatch {
                expected: edit.base.clone(),
                actual: base.definition.id.clone(),
            }
            .into());
        }
        match &authenticated.replacement {
            Replacement::Callable(read) => {
                let code = self
                    .documents()?
                    .publish_callable_edit(&claim(pin)?, &authenticated.code, read)
                    .await
                    .map_err(|error| super::publication_error(pin, error))?;
                let InlineRequest::Callable {
                    function, origin, ..
                } = code
                else {
                    unreachable!("a callable edit retains its representation")
                };
                self.publish_callable(pin, &function, environment, &origin, limits)
                    .await
            }
            Replacement::Linked { document, captions } => {
                let InlineRequest::Linked { start } = &authenticated.code else {
                    unreachable!("linked edit")
                };
                captions
                    .validate(&document.to_document()?)
                    .map_err(|error| {
                        lash_execution_contracts::PluginError::Session(error.to_string())
                    })?;
                let start = self
                    .publish_workflow(pin, document, &start.selector, environment, None, limits)
                    .await?;
                let draft = lash_vm_runtime::process::definition_draft(&KernelProcessDefinition {
                    code: InlineRequest::Linked { start },
                })
                .map_err(|e| lash_execution_contracts::PluginError::Session(e.to_string()))?;
                self.publish_definition(pin, &draft).await
            }
        }
    }
}

fn missing_definition(id: &ProcessDefinitionId) -> lash_execution_contracts::PluginError {
    lash_execution_contracts::PluginError::Runtime(lash_core::RuntimeError::new(
        lash_core::RuntimeErrorCode::DefinitionMissing,
        format!("definition {id} is not retained"),
    ))
}
