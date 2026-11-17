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
        let (opened, content) = diagnostics::phase(
            "check",
            tracing::debug_span!("lash.artifacts.check"),
            self.materialized_workflow(definition, captions),
        )
        .await?;
        let namespaces = linked_namespaces(&content, &opened.document).map_err(|error| {
            crate::EmbedError::WorkflowSource(lash_vm_client::service::SourceServiceError::Print(
                crate::dialect::PrintError::Namespace(error),
            ))
        })?;
        self.print_materialized(opened, &namespaces, format, options)
            .await
    }

    pub(super) async fn print_materialized(
        &self,
        opened: lash_kernel_package::MaterializedWorkflow,
        namespaces: &crate::dialect::Namespaces,
        format: SourceFormat,
        options: PrintOptions,
    ) -> Result<PrintedSource> {
        if matches!(
            opened.selector,
            lash_kernel_value::names::StartSelector::Main
        ) {
            return Err(crate::EmbedError::WorkflowSource(
                lash_vm_client::service::SourceServiceError::Print(
                    crate::dialect::PrintError::Unprintable {
                        reason: crate::dialect::UnprintableReason::MainEntry,
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
                    .print(
                        &opened.document,
                        namespaces,
                        Some(&opened.captions),
                        format,
                        options,
                    )
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

/// Project retained declarations, including the whole owning type for methods.
/// No provider is consulted and imports supply no callable flags or member kind.
pub(super) fn linked_namespaces(
    content: &lash_kernel_package::LinkedContent,
    document: &lash_kernel_doc::Document,
) -> std::result::Result<crate::dialect::Namespaces, crate::dialect::NamespaceRefusal> {
    use lash_kernel_value::{
        ExportName, Fill, ImportKind, Member, MemberKind, Package, Visibility,
    };
    let mut packages = std::collections::BTreeMap::<_, Package>::new();
    let mut declarations = std::collections::BTreeMap::new();
    for import in document.imports.values() {
        if matches!(import.kind, ImportKind::Operation { .. }) {
            continue;
        }
        let name = &import.name;
        let mut found = false;
        for link in content.links.values() {
            let Some(source) = content.documents.get(&link.document) else {
                continue;
            };
            for (slot, candidate) in &source.imports {
                if candidate != import {
                    continue;
                }
                let Some(Fill::Export { link, export }) = link.fills.get(slot) else {
                    continue;
                };
                let export = ExportName {
                    name: export.name.clone(),
                    member: if name.type_member.is_some() {
                        None
                    } else {
                        export.member.clone()
                    },
                };
                let declaration = content.export(*link, &export).map_err(|_| {
                    crate::dialect::NamespaceRefusal::MissingExport {
                        link: *link,
                        name: export.clone(),
                    }
                })?;
                if let Some(previous) = declarations.insert(
                    (name.package.clone(), name.member.clone()),
                    declaration.clone(),
                ) && previous != declaration
                {
                    return Err(crate::dialect::NamespaceRefusal::DeclarationConflict {
                        name: name.clone(),
                    });
                }
                packages
                    .entry(name.package.clone())
                    .or_insert_with(|| Package {
                        path: name.package.clone(),
                        members: Default::default(),
                    })
                    .members
                    .insert(
                        name.member.clone(),
                        Member {
                            visibility: Visibility::Public,
                            kind: MemberKind::Export {
                                link: *link,
                                export,
                            },
                        },
                    );
                found = true;
            }
        }
        if !found {
            return Err(crate::dialect::NamespaceRefusal::MissingDeclaration {
                name: name.clone(),
            });
        }
    }
    crate::dialect::Namespaces::project(
        packages.values(),
        Default::default(),
        &|link, export| content.export(link, export).ok(),
        &|_| false,
    )
}
