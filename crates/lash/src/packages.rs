//! Trusted scoped publication. Package content and its provenance are held
//! before config, a round or a process can expose their references.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use lash_core::ReferrerClaim;
use lash_core::durable_port::packages::PackageStore;
use lash_core::plugin::packages::{
    PackagePublisher, PublishPackage, package_store_error as store_error,
};
use lash_core_store::packages::{LinkRecord, PackageRecord};
use lash_execution_contracts::process::ProcessDocumentProvider;
use lash_execution_contracts::process::linking::{AdmissionRefusal, ContentRef, EnginePortError};
use {lash_kernel_value::package::*, lash_vocabulary::package::*};

/// A publisher bound by the host to its owned roots and immutable accepted
/// namespace. Guest input cannot choose a publication domain or publisher.
pub struct ScopedPackagePublisher {
    context: PublicationContext,
    snapshot: NamespaceSnapshot,
    runtime: Arc<dyn ProcessDocumentProvider>,
    store: Arc<dyn PackageStore>,
}

impl ScopedPackagePublisher {
    pub fn new(
        context: PublicationContext,
        snapshot: NamespaceSnapshot,
        runtime: Arc<dyn ProcessDocumentProvider>,
        store: Arc<dyn PackageStore>,
    ) -> Self {
        Self {
            context,
            snapshot,
            runtime,
            store,
        }
    }

    /// Rebuild config from its exact publication. This does not link against
    /// today's namespace or select a newer publication at the same path.
    pub async fn configured(&self, key: &PublicationKey) -> Result<Package, EnginePortError> {
        let record = self
            .store
            .publication(key)
            .await
            .map_err(store_error)?
            .ok_or_else(|| EnginePortError::Unavailable {
                message: format!("configured package publication {key:?} is missing"),
            })?;
        if record.context != self.context {
            return Err(EnginePortError::Refused(AdmissionRefusal::NotOwner {
                path: record.path,
            }));
        }
        let PublishedContent::Package(id) = record.content else {
            return Err(EnginePortError::Refused(
                AdmissionRefusal::InvalidDocument {
                    message: "configured enumerated package is deferred".into(),
                },
            ));
        };
        self.store
            .package(&id)
            .await
            .map_err(store_error)?
            .map(|record| record.package)
            .ok_or_else(|| EnginePortError::Unavailable {
                message: format!("configured package {id} is missing"),
            })
    }
}

#[async_trait::async_trait]
impl PackagePublisher for ScopedPackagePublisher {
    async fn publish(
        &self,
        claim: &ReferrerClaim,
        context: &PublicationContext,
        request: PublishPackage,
    ) -> Result<PublicationRecord, EnginePortError> {
        let package = request.package;
        if context != &self.context || package.path.as_slice()[0] != context.root {
            return Err(EnginePortError::Refused(AdmissionRefusal::NotOwner {
                path: package.path,
            }));
        }
        let mut content = lash_kernel_package::LinkedContent::default();
        for accepted in &self.snapshot.advertisements {
            if let PackageAdvertisement::Enumerated(package) = &accepted.advertisement {
                content.packages.insert(package.identity(), package.clone());
            }
        }
        let publications: Vec<_> = self
            .snapshot
            .advertisements
            .iter()
            .map(|accepted| &accepted.publication)
            .collect();
        let mut prepared = BTreeMap::new();
        for code in request.code {
            if code.content.identity() != code.inspection.link
                || code.content.document != code.inspection.document
            {
                return Err(EnginePortError::Refused(
                    AdmissionRefusal::InvalidDocument {
                        message: "prepared Link inspection does not identify its content".into(),
                    },
                ));
            }
            let id = code.inspection.link;
            if prepared.insert(id, code).is_some() {
                return Err(EnginePortError::Refused(
                    AdmissionRefusal::InvalidDocument {
                        message: format!("duplicate prepared Link {id}"),
                    },
                ));
            }
        }
        let code_links: BTreeSet<_> = package
            .members
            .values()
            .filter_map(|member| match member.kind {
                MemberKind::Export { link, .. } => Some(link),
                MemberKind::Operation(_) => None,
            })
            .collect();
        let mut pending: Vec<_> = code_links.iter().copied().collect();
        let mut imported = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if content.links.contains_key(&id) {
                continue;
            }
            let link = match prepared.get(&id) {
                Some(code) => code.content.clone(),
                None => self
                    .runtime
                    .load(&ContentRef::Link(id))
                    .await?
                    .material
                    .downcast::<Link>()
                    .map_err(|_| {
                        EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
                            message: format!("runtime returned another material for Link {id}"),
                        })
                    })?,
            };
            let document = self
                .runtime
                .load(&ContentRef::Document(link.document))
                .await?
                .material
                .downcast::<lash_kernel_doc::Document>()
                .map_err(|_| {
                    EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
                        message: format!(
                            "runtime returned another material for Document {}",
                            link.document
                        ),
                    })
                })?;
            for (slot, import) in &document.imports {
                if prepared.contains_key(&id)
                    && import.name.package != package.path
                    && matches!(link.fills.get(slot), Some(Fill::Export { .. }))
                {
                    imported.insert(import.name.package.clone());
                }
            }
            for fill in link.fills.values() {
                if let Fill::Export { link, .. } = fill {
                    pending.push(*link);
                }
            }
            content.documents.insert(link.document, document.into());
            content.links.insert(id, link);
        }
        let namespace = lash_kernel_package::Namespace {
            advertisements: self
                .snapshot
                .advertisements
                .iter()
                .map(|accepted| (&accepted.publication.context, &accepted.advertisement))
                .collect(),
        };
        // A newly prepared Link must be exactly the result of fulfilment
        // under this publisher. Reusing a stored Link instead retains its
        // original publication and never relinks its internal Imports.
        let candidate = PackageAdvertisement::Enumerated(package.clone());
        // A publication replaces its own path in the authoring view. Keep
        // the original namespace for ownership and provenance validation.
        let mut advertisements: Vec<_> = namespace
            .advertisements
            .iter()
            .copied()
            .filter(|(_, advertisement)| match advertisement {
                PackageAdvertisement::Enumerated(old) => old.path != package.path,
                PackageAdvertisement::Deferred(old) => old.path != package.path,
            })
            .collect();
        advertisements.push((context, &candidate));
        let authoring = lash_kernel_package::Namespace { advertisements };
        for code in prepared.values() {
            let expected = authoring
                .fulfil(
                    &content,
                    code.content.document,
                    context,
                    Some(&package.path),
                    None,
                )
                .map_err(lash_vm_runtime::map_link_refusal)?;
            if expected != code.content {
                return Err(EnginePortError::Refused(
                    AdmissionRefusal::InvalidDocument {
                        message: "prepared Link does not match its publisher's admitted Imports"
                            .into(),
                    },
                ));
            }
        }
        content
            .validate_publication(context, &package, &namespace)
            .map_err(lash_vm_runtime::map_link_refusal)?;
        // Keep exact source publications, including alias provenance, rather
        // than treating a copied LinkId as permission to use internal code.
        let mut imported_publications = Vec::new();
        for path in imported {
            let accepted = self
                .snapshot
                .advertisements
                .iter()
                .find(|accepted| match &accepted.advertisement {
                    PackageAdvertisement::Enumerated(source) => source.path == path,
                    PackageAdvertisement::Deferred(_) => false,
                })
                .ok_or_else(|| {
                    EnginePortError::Refused(AdmissionRefusal::NotOwner { path: path.clone() })
                })?;
            let PackageAdvertisement::Enumerated(source) = &accepted.advertisement else {
                unreachable!("selected enumerated source")
            };
            let id = source.identity();
            let mut sources = publications.iter().copied().filter(|record| {
                record.context == accepted.publication.context
                    && record.path == path
                    && record.content == PublishedContent::Package(id)
            });
            let source = sources.next().ok_or_else(|| {
                EnginePortError::Refused(AdmissionRefusal::NotOwner { path: path.clone() })
            })?;
            if sources.any(|other| other != source) {
                return Err(EnginePortError::Refused(AdmissionRefusal::NotOwner {
                    path,
                }));
            }
            imported_publications.push(source.key());
        }
        // An alias has no code Import of its own. Retain the exact publication
        // that already admitted its defining Link, never provenance by hash alone.
        for member in package.members.values() {
            let MemberKind::Export { link, .. } = &member.kind else {
                continue;
            };
            if prepared.contains_key(link) {
                continue;
            }
            let mut sources = self.snapshot.advertisements.iter().filter_map(|accepted| {
                let PackageAdvertisement::Enumerated(source) = &accepted.advertisement else {
                    return None;
                };
                source
                    .members
                    .values()
                    .any(|original| {
                        original.visibility == Visibility::Public && original.kind == member.kind
                    })
                    .then_some(&accepted.publication)
            });
            let source = sources.next().ok_or_else(|| {
                EnginePortError::Refused(AdmissionRefusal::NotOwner {
                    path: package.path.clone(),
                })
            })?;
            if sources.any(|other| other != source) {
                return Err(EnginePortError::Refused(AdmissionRefusal::NotOwner {
                    path: package.path.clone(),
                }));
            }
            let key = source.key();
            if !imported_publications.contains(&key) {
                imported_publications.push(key);
            }
        }
        // Fulfilled carries the validated content across runtime instances;
        // publication must not depend on the producer's private preparation
        // cache. Persist fresh Links, after all admission checks, before holds.
        let mut pending: Vec<_> = code_links.iter().map(|id| (*id, false)).collect();
        let mut retained = BTreeSet::new();
        while let Some((id, children_retained)) = pending.pop() {
            if retained.contains(&id) {
                continue;
            }
            if let Some(code) = prepared.get(&id) {
                if !children_retained {
                    pending.push((id, true));
                    pending.extend(code.content.fills.values().filter_map(|fill| match fill {
                        Fill::Export { link, .. } => Some((*link, false)),
                        Fill::Operation(_) | Fill::Deferred { .. } => None,
                    }));
                    continue;
                }
                self.store
                    .put_link(
                        claim,
                        &LinkRecord {
                            id,
                            link: code.content.clone(),
                        },
                        &[lash_core::ArtifactName {
                            store: lash_core::ArtifactStoreId::KernelDocument,
                            artifact_ref: code.content.document.to_string(),
                        }],
                    )
                    .await
                    .map_err(store_error)?;
            }
            self.runtime.hold(claim, &ContentRef::Link(id)).await?;
            retained.insert(id);
        }
        let id = package.identity();
        let path = package.path.clone();
        self.store
            .put_package(claim, &PackageRecord { id, package })
            .await
            .map_err(store_error)?;
        let record = PublicationRecord {
            context: context.clone(),
            path,
            content: PublishedContent::Package(id),
            code_links,
            imported_publications,
        };
        self.store
            .publish(claim, &record)
            .await
            .map_err(store_error)?;
        Ok(record)
    }
}

#[cfg(test)]
mod tests;
