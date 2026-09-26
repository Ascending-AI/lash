//! This crate's twins on the Restate server double and the storage-only
//! store set (D1 F8, PR-S2).

const SEED: u64 = 0x5_2d30;

/// An open handler on this crate's double lends a turn-scoped controller and
/// closes cleanly.
#[tokio::test(flavor = "multi_thread")]
async fn an_open_handler_lends_its_scope_on_the_double() {
    let double = super::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
    let admitted = lash_core::AdmittedScope::turn(
        lash_core::SessionId::from("root"),
        lash_core::TurnId::from("t"),
    );
    let handler = double
        .open_handler(admitted.clone())
        .await
        .expect("open the handler");
    assert_eq!(handler.scoped().admitted_scope(), &admitted);
    handler.close().await.expect("close the handler");
}

/// The storage-only twins hand out the artifact and trigger ports a law that
/// runs no effect reaches.
#[tokio::test]
async fn the_storage_only_twins_serve_artifacts_and_triggers() {
    let backend = super::memory_store_backend().await;
    let _artifacts = lashlang::LashlangArtifacts::of_backend(&backend);
    let stores = super::memory_store_set().await;
    assert_ne!(
        lash_core::StoreSet::binding_identity(stores.as_ref()),
        &backend.binding_identity(),
        "each twin call opens a fresh store set"
    );
    let _triggers = lash_core::StoreSet::trigger_store(stores.as_ref());
}
