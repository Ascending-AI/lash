//! Saved workflows print through the same owned document used by Draft authoring.
use super::*;
use crate::dialect::{PrintOptions, PrintedSource, SourceFormat};

impl HostArtifacts {
    /// Materialize retained code, then print standalone source. The returned
    /// map names the materialized document; importing needs no saved definition.
    #[tracing::instrument(level = "debug", name = "lash.artifacts.print_workflow", skip_all, fields(definition_id = %definition, dialect = format.dialect()))]
    pub async fn print_workflow(
        &self,
        definition: &ProcessDefinitionId,
        captions: Option<&crate::workflow::document::Captions>,
        format: SourceFormat,
        options: PrintOptions,
    ) -> Result<PrintedSource> {
        if let Some(captions) = captions {
            let crate::workflow::WorkflowRead::Inspected(base) =
                self.definition_graph(definition).await?
            else {
                return Err(lash_execution_contracts::PluginError::Runtime(
                    lash_core::RuntimeError::new(
                        lash_core::RuntimeErrorCode::DefinitionMissing,
                        "definition is not retained",
                    ),
                )
                .into());
            };
            crate::dialect::check_captions(base.document.document(), captions, options.limits)
                .map_err(lash_vm_client::service::SourceServiceError::Print)
                .map_err(crate::EmbedError::WorkflowSource)?;
        }
        let opened = diagnostics::phase(
            "check",
            tracing::debug_span!("lash.artifacts.check"),
            self.materialized_workflow(definition, captions),
        )
        .await?;
        if matches!(
            opened.selector,
            lash_kernel_value::names::StartSelector::Main
        ) {
            return Err(crate::EmbedError::WorkflowSource(
                lash_vm_client::service::SourceServiceError::Print(
                    crate::dialect::PrintError::Unprintable {
                        site: lash_kernel_value::sites::Site::new(
                            lash_kernel_value::sites::Unit::Main,
                            [],
                        ),
                    },
                ),
            ));
        }
        let service = self
            .engines
            .document_provider(lash_vm_runtime::LASH_VM_ENGINE_KIND)
            .ok_or_else(|| {
                lash_execution_contracts::PluginError::Registration("no source provider".into())
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
        let mut printed = diagnostics::phase(
            "print",
            tracing::debug_span!("lash.artifacts.print"),
            async {
                service
                    .print(&opened.document, Some(&opened.captions), format, options)
                    .await
                    .map_err(crate::EmbedError::WorkflowSource)
            },
        )
        .await?;
        printed.dropped.extend(opened.dropped);
        printed.dropped.sort();
        printed.dropped.dedup();
        Ok(printed)
    }
}
