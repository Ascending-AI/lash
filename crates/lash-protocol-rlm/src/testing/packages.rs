//! The accepted package facts a test's model round records before a cell.
use lash_core_store::tool_surface::RecordedPackages;
use lash_vocabulary::{names::*, package::*};

pub(crate) async fn record(ctx: &lash_core::RuntimeExecutionContext<'_>) -> RecordedPackages {
    let backend = ctx.actor_context().backend();
    let store = backend.package_store();
    let claim = ctx.execution_claim().expect("cell execution claim");
    let consumer = PublicationContext {
        domain: PublicationDomain("cell-laws".into()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("cell-law-consumer".into()),
        root: Segment::new("cell").unwrap(),
    };
    let release = lash_vm_releases::helper_release().unwrap();
    let mut recorded = RecordedPackages {
        consumer: Some(consumer.clone()),
        ..Default::default()
    };
    for package in release.content.packages.values() {
        let context = PublicationContext {
            root: package.path.as_slice()[0].clone(),
            publisher: PublisherInstance("helper-release".into()),
            ..consumer.clone()
        };
        let publication = PublicationRecord {
            context,
            path: package.path.clone(),
            content: PublishedContent::Package(package.identity()),
            code_links: package
                .members
                .values()
                .filter_map(|member| match &member.kind {
                    MemberKind::Export { link, .. } => Some(*link),
                    _ => None,
                })
                .collect(),
            imported_publications: vec![],
        };
        store.publish(&claim, &publication).await.unwrap();
        recorded
            .snapshot
            .advertisements
            .push(AcceptedAdvertisement {
                publication,
                advertisement: PackageAdvertisement::Enumerated(package.clone()),
            });
    }
    use lash_core::plugin::packages::PackageProvider as _;
    for service in super::dialect_services() {
        let root = service.roots().into_iter().next().unwrap();
        let context = PublicationContext {
            root,
            publisher: PublisherInstance("cell-law-services".into()),
            ..consumer.clone()
        };
        for contribution in service.advertise(ctx.owner_config(), &context).unwrap() {
            let PackageAdvertisement::Enumerated(package) = &contribution.advertisement else {
                unreachable!()
            };
            store
                .put_package(
                    &claim,
                    &lash_core_store::packages::PackageRecord {
                        id: package.identity(),
                        package: package.clone(),
                    },
                )
                .await
                .unwrap();
            store
                .publish(&claim, &contribution.publication)
                .await
                .unwrap();
            recorded.routes.extend(contribution.routes);
            recorded
                .snapshot
                .advertisements
                .push(AcceptedAdvertisement {
                    publication: contribution.publication,
                    advertisement: contribution.advertisement,
                });
        }
    }
    let catalog = ctx.tool_catalog();
    let definitions: Vec<_> = catalog
        .entries()
        .iter()
        .map(|entry| lash_vocabulary::ToolDefinition {
            manifest: entry.manifest.clone(),
            contract: (*entry.contract).clone(),
        })
        .collect();
    let roots: std::collections::BTreeSet<_> = definitions
        .iter()
        .map(|d| d.manifest.operation.package.as_slice()[0].clone())
        .collect();
    for root in roots {
        let context = PublicationContext {
            root,
            publisher: PublisherInstance("cell-law-tools".into()),
            ..consumer.clone()
        };
        let contributions = lash_core::plugin::packages::operation_packages(
            &lash_core::testing::test_tools_provider(),
            definitions.clone(),
            |_| None,
            &context,
        )
        .unwrap();
        for contribution in contributions {
            let PackageAdvertisement::Enumerated(package) = &contribution.advertisement else {
                unreachable!()
            };
            store
                .put_package(
                    &claim,
                    &lash_core_store::packages::PackageRecord {
                        id: package.identity(),
                        package: package.clone(),
                    },
                )
                .await
                .unwrap();
            store
                .publish(&claim, &contribution.publication)
                .await
                .unwrap();
            recorded.routes.extend(contribution.routes);
            recorded
                .snapshot
                .advertisements
                .push(AcceptedAdvertisement {
                    publication: contribution.publication,
                    advertisement: contribution.advertisement,
                });
        }
    }
    lash_kernel_package::Namespace {
        advertisements: recorded
            .snapshot
            .advertisements
            .iter()
            .map(|accepted| (&accepted.publication.context, &accepted.advertisement))
            .collect(),
    }
    .validate()
    .expect("the fixture records a coherent namespace");
    recorded
}
