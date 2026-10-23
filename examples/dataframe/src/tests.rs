//! L-LIBRARY: source library code is retained under its defining Link and
//! only the declared IO/clock operations enter host routing.
use std::sync::Arc;

#[tokio::test]
async fn df_library_and_workflow_keep_their_defining_links() {
    let stores = lash::sqlite::SqliteStoreSet::memory().await.unwrap();
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
        .config(lash::durable::DurableSettings {
            max_ingress_bytes: Some(const { std::num::NonZeroU64::new(64 * 1024).unwrap() }),
            owner_retention: Some(lash::durable::OwnerRetentionSettings {
                idle: std::time::Duration::from_secs(60),
                max_resident_owners: const { std::num::NonZeroUsize::new(256).unwrap() },
            }),
            ..Default::default()
        })
        .build()
        .unwrap();
    let pin = lash::process::HostArtifactPin::mint();
    let (core, creator) = crate::host::build(
        backend.clone(),
        lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("dataframe"),
            lash::persistence::LeaseIncarnationId::new("library-law"),
        ),
        &pin,
        [("s3://sales.csv".into(), "price\n10\n20\n30".into())].into(),
    )
    .await
    .unwrap();
    let package = creator
        .namespace
        .snapshot
        .advertisements
        .iter()
        .find_map(|accepted| match &accepted.advertisement {
            lash::package::PackageAdvertisement::Enumerated(package)
                if package.path.to_string() == "df" =>
            {
                Some(package)
            }
            _ => None,
        })
        .unwrap();
    let links: std::collections::BTreeSet<_> = package
        .members
        .values()
        .filter_map(|member| match &member.kind {
            lash::package::MemberKind::Export { link, .. } => Some(*link),
            _ => None,
        })
        .collect();
    assert_eq!(links.len(), 1, "one library Document has one defining Link");
    let start = creator
        .create(
            crate::NIGHTLY,
            lash::workflow::document::SourceLanguage::TypeScript,
            "nightly",
            &pin,
        )
        .await
        .unwrap();
    let linked = backend
        .package_store()
        .link(&start.link)
        .await
        .unwrap()
        .unwrap();
    assert!(linked.link.fills.values().any(
        |fill| matches!(fill, lash::package::Fill::Export { link, .. } if links.contains(link))
    ));
    let receipt = Box::pin(creator.start(start, "s3://sales.csv", "nightly-law", &pin))
        .await
        .unwrap();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        core.processes().await_output(&receipt.process_id),
    )
    .await
    .unwrap()
    .unwrap();
    let lash::process::ProcessAwaitOutput::Settled { output } = output else {
        panic!("{output:?}")
    };
    assert!(output.is_success(), "{output:?}");
    assert_eq!(output.value_for_projection(), serde_json::json!(3));
    creator.artifacts.release(pin).await.unwrap();
}
