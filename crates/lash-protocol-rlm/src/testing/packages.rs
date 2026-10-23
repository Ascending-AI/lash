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
    let mut recorded = RecordedPackages {
        consumer: Some(consumer.clone()),
        ..Default::default()
    };
    use lash_core::plugin::packages::PackageProvider as _;
    for service in super::dialect_services() {
        let root = service
            .roots(ctx.owner_config())
            .into_iter()
            .next()
            .unwrap();
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
    recorded = ctx
        .package_composition()
        .advertise_packages(
            ctx.owner_config(),
            &recorded,
            &consumer.domain,
            consumer.scope.clone(),
        )
        .expect("the fixture's registered packages advertise");
    for accepted in &recorded.snapshot.advertisements {
        store.publish(&claim, &accepted.publication).await.unwrap();
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

/// Builtin dialect operation declarations are deployment facts too.
pub(crate) async fn deploy_services(
    backend: &lash_execution_contracts::backend::Backend,
    claim: &lash_core::ReferrerClaim,
) {
    use lash_core::plugin::packages::PackageProvider as _;
    let consumer = PublicationContext {
        domain: PublicationDomain("cell-laws".into()),
        scope: PublicationScope::Host,
        publisher: PublisherInstance("cell-law-services".into()),
        root: Segment::new("cell").unwrap(),
    };
    let config = lash_core::OwnerConfigView::default();
    let store = backend.package_store();
    for service in super::dialect_services() {
        let context = PublicationContext {
            root: service.roots(&config).into_iter().next().unwrap(),
            ..consumer.clone()
        };
        for contribution in service.advertise(&config, &context).unwrap() {
            let PackageAdvertisement::Enumerated(package) = &contribution.advertisement else {
                unreachable!()
            };
            store
                .put_package(
                    claim,
                    &lash_core_store::packages::PackageRecord {
                        id: package.identity(),
                        package: package.clone(),
                    },
                )
                .await
                .unwrap();
            store
                .publish(claim, &contribution.publication)
                .await
                .unwrap();
        }
    }
}
