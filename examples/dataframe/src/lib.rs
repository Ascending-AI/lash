//! A TypeScript library published as a Document and immutable Link. Only its
//! ordinary IO and clock operations cross the VM boundary.
pub mod host;

use lash::package::*;
use std::sync::Arc;

pub const LIBRARY: &str = include_str!("../df.ts");
pub const CELL: &str = include_str!("../cell.ts");
pub const NIGHTLY: &str = include_str!("../nightly.ts");

/// Publish the source's own Export inventory; no surface file or whole-set
/// image supplies a second inventory. The caller already owns `pin` cleanup.
pub async fn publish_library(
    backend: &lash::Backend,
    pin: &lash::process::HostArtifactPin,
    workers: &lash::vm::WorkerService,
    publisher: &ScopedPackagePublisher,
    namespace: &RecordedPackages,
    context: PublicationContext,
) -> Result<(Package, PublicationRecord), String> {
    let documents = Arc::new(lash::vm::KernelDocuments::new(
        backend.module_artifacts(),
        backend.package_store(),
    ));
    let environment = host::environment_request(namespace, context.clone());
    let names = documents
        .namespace(&environment)
        .await
        .map_err(|error| error.to_string())?;
    let (document, _, _) = workers
        .import_workflow(
            lash::workflow::document::SourceLanguage::TypeScript,
            LIBRARY,
            &names,
            None,
        )
        .await
        .map_err(|error| error.to_string())?;
    let exports: Vec<_> = document.exports.keys().cloned().collect();
    let claim = lash::persistence::ReferrerClaim::unguarded(
        lash::persistence::ArtifactReferrer::HostPin(pin.clone()),
    )
    .map_err(|error| error.to_string())?;
    let id = documents
        .admit_document(
            &claim,
            &lash::workflow::document::DocumentBytes::from(document),
        )
        .await
        .map_err(|error| error.to_string())?;
    let path = PackagePath::parse("df").map_err(|error| error.to_string())?;
    let linked = documents
        .fulfil(&FulfilRequest {
            document: id,
            selector: ReachSelector::AllExports,
            snapshot: namespace.snapshot.clone(),
            context: context.clone(),
            purpose: LinkPurpose::PackageCode { path: path.clone() },
        })
        .await
        .map_err(|error| error.to_string())?;
    // This document owner prepared the Link; retain it under the host pin
    // before the independently constructed publisher exposes its identity.
    documents
        .hold(&claim, &ContentRef::Link(linked.content.identity()))
        .await
        .map_err(|error| error.to_string())?;
    let members = exports
        .into_iter()
        .map(|name| {
            Ok((
                Segment::new(name.as_str()).map_err(|error| error.to_string())?,
                Member {
                    visibility: Visibility::Public,
                    kind: MemberKind::Export {
                        link: linked.content.identity(),
                        export: ExportName { name, member: None },
                    },
                },
            ))
        })
        .collect::<Result<_, String>>()?;
    let package = Package { path, members };
    let publication = publisher
        .publish(
            &claim,
            &context,
            PublishPackage {
                package: package.clone(),
                code: vec![linked],
            },
        )
        .await
        .map_err(|error| error.to_string())?;
    Ok((package, publication))
}

#[cfg(test)]
mod tests;
