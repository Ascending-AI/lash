//! L-LIBRARY: source library code is retained under its defining Link and
//! only the declared IO/clock operations enter host routing.
use std::sync::Arc;

#[tokio::test]
async fn df_library_and_workflow_keep_their_defining_links() {
    let stores = lash::sqlite::SqliteStoreSet::memory().await.unwrap();
    let backend = lash::durable::DurableBackendBuilder::new(Arc::new(stores))
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
    let request = crate::host::environment_request(&creator.namespace, creator.context.clone());
    let names = creator
        .artifacts
        .workflow_environment(&request)
        .await
        .unwrap()
        .unwrap();
    let release = lash::vm::helper_release().unwrap();
    let roots = release.resolver_roots();
    let library =
        lash::dialect::NamedLibrary::resolving(names.functions(), |id| roots.contains(id)).unwrap();
    let source_environment = lash::dialect::Environment::new(&library, names.namespaces());
    let mut admission_failures = Vec::new();
    for (name, source, mode) in [
        ("df.ts", crate::LIBRARY, lash::dialect::Mode::Module),
        ("nightly.ts", crate::NIGHTLY, lash::dialect::Mode::Module),
        (
            "cell.ts",
            crate::CELL,
            lash::dialect::Mode::Cell(lash::dialect::CellEnvironment::first()),
        ),
    ] {
        let imported = lash::typescript::lower(source, &source_environment, mode).unwrap();
        let mut draft =
            lash::workflow::edit::Draft::open(imported.document.into_document()).unwrap();
        if let Err(refusal) = draft.apply(
            &lash::workflow::edit::Transaction {
                base: draft.identity(),
                edits: vec![],
            },
            &names.checker(),
            lash::workflow::edit::CONVENTIONS,
        ) {
            admission_failures.push(format!("{name}: zero-edit admission: {refusal:?}"));
        } else {
            let printed = creator
                .workers
                .print(
                    draft.document(),
                    None,
                    lash::dialect::SourceFormat::TypeScriptV1,
                    Default::default(),
                )
                .await
                .unwrap();
            let relowered =
                lash::typescript::lower(&printed.source, &source_environment, mode).unwrap();
            assert_eq!(
                draft.document().canonical_bytes(),
                relowered.document.canonical_bytes(),
                "{name}: ordinary import under the actual package namespace"
            );
            eprintln!("dataframe zero-edit admission: {name}");
        }
    }
    assert!(
        admission_failures.is_empty(),
        "{}",
        admission_failures.join("\n")
    );
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
