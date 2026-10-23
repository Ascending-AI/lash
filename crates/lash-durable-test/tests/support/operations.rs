//! Explicit linked operation fixtures for non-kernel process-engine laws.
#![allow(dead_code)]
use lash_core_store::tool_surface::{
    ExecutionEnvironment, OperationRoute, RoutedOperation, UseBinding,
};
use lash_vocabulary::{claims::RegistryKind, *};
use std::collections::BTreeMap;

/// One imported operation and its committed pin. Its bytes, rather than a
/// tool's display name or the position in a fixture list, determine its use.
fn content(definition: &ToolDefinition) -> (lash_kernel_doc::Document, Link) {
    let signature = json_schema::tool_signature(
        &definition.contract.input_schema.canonical,
        &definition.contract.output_schema.canonical,
    );
    let mut document =
        lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::BySpelling, Vec::new());
    document.main = None;
    document.imports.insert(
        ImportSlot(0),
        Import {
            name: QualifiedName {
                package: definition.manifest.operation.package.clone(),
                member: definition.manifest.operation.name.clone(),
                type_member: None,
            },
            kind: ImportKind::Operation {
                signature: signature.clone(),
            },
        },
    );
    let link = Link {
        document: document.identity(),
        fills: BTreeMap::from([(
            ImportSlot(0),
            Fill::Operation(OperationRequirement::new(
                signature,
                definition.contract_id(),
            )),
        )]),
    };
    (document, link)
}

pub fn use_for(definition: &ToolDefinition) -> OperationUse {
    OperationUse {
        link: content(definition).1.identity(),
        slot: ImportSlot(0),
    }
}

pub fn environment(
    provider: providers::ToolProviderHandle,
    definitions: &[ToolDefinition],
) -> ExecutionEnvironment {
    let mut execution = ExecutionEnvironment::empty(RegistryKind::Process);
    for definition in definitions {
        let (_, link) = content(definition);
        let Fill::Operation(requirement) = &link.fills[&ImportSlot(0)] else {
            unreachable!()
        };
        let route = OperationRoute::advertised(provider.clone(), definition, None);
        route.require(&mut execution.requirements);
        let mode = if route.policy == ExecutionPolicy::Read {
            OperationMode::Read
        } else {
            OperationMode::Waited
        };
        let offer = OperationOffer {
            requirement: requirement.clone(),
            mode,
            intents: definition
                .manifest
                .declaration()
                .intents
                .iter()
                .map(|intent| Intent(intent.as_str().into()))
                .collect(),
            controls: definition.manifest.declaration().controls.clone(),
        };
        execution.roots.insert(link.identity());
        assert!(
            execution
                .uses
                .insert(
                    use_for(definition),
                    UseBinding::Fixed(RoutedOperation {
                        requirement: requirement.clone(),
                        offer,
                        route,
                    })
                )
                .is_none(),
            "fixture declares an operation twice"
        );
    }
    execution
}

/// Retain the source of every use under the same explicit host claim as its environment.
pub async fn retain(
    backend: &lash::Backend,
    pin: &lash_core::HostArtifactPin,
    definitions: &[ToolDefinition],
) {
    let claim =
        lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(pin.clone()))
            .unwrap();
    let documents = lash_vm_runtime::KernelDocuments::new(
        backend.module_artifacts(),
        backend.package_store(),
        std::sync::Arc::clone(
            lash_vm_client::packages::WorkerLibrary::built()
                .unwrap()
                .helpers(),
        ),
    );
    for definition in definitions {
        let (document, link) = content(definition);
        let document = documents
            .admit_document(&claim, &lash_kernel_doc::DocumentBytes::from(document))
            .await
            .unwrap();
        backend
            .package_store()
            .put_link(
                &claim,
                &lash_core_store::packages::LinkRecord {
                    id: link.identity(),
                    link,
                },
                &[lash_core::ArtifactName {
                    store: lash_core::ArtifactStoreId::KernelDocument,
                    artifact_ref: document.to_string(),
                }],
            )
            .await
            .unwrap();
    }
}

/// A fixture's tools enter code only through its explicit package provider.
pub struct Packages {
    pub provider: providers::ToolProviderHandle,
    pub definitions: Vec<ToolDefinition>,
}
#[async_trait::async_trait]
impl lash_core_execution::plugin::packages::PackageProvider for Packages {
    fn roots(
        &self,
        _: &lash_core_execution::OwnerConfigView,
    ) -> std::collections::BTreeSet<Segment> {
        self.definitions
            .iter()
            .map(|definition| definition.manifest.operation.package.as_slice()[0].clone())
            .collect()
    }
    fn advertise(
        &self,
        _: &lash_core_execution::plugin::OwnerConfigView,
        context: &PublicationContext,
    ) -> Result<
        Vec<lash_core_execution::plugin::packages::PackageContribution>,
        lash_execution_contracts::PluginError,
    > {
        lash_core_execution::plugin::packages::operation_packages(
            &self.provider,
            self.definitions.clone(),
            |_| None,
            context,
        )
    }
    async fn hold(
        &self,
        _: &lash_core_store::artifact_referrer::ReferrerClaim,
        _: &[&PackageAdvertisement],
    ) -> Result<(), lash_execution_contracts::process::linking::EnginePortError> {
        Ok(())
    }
}
