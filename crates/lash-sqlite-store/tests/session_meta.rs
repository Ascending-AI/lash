use lash_core::{
    SessionPolicy, SessionRelation, SessionStoreCreateRequest, SessionStoreFactory, TurnBudget,
};
use lash_sqlite_store::{SqliteSessionStoreFactory, Store};

#[tokio::test]
async fn sqlite_unbound_session_meta_refuses_ambiguous_resolution() {
    let dir = tempfile::tempdir().expect("unbound-session-meta tempdir");
    let factory = SqliteSessionStoreFactory::new(dir.path());
    for session_id in ["unbound-session-meta-a", "unbound-session-meta-b"] {
        factory
            .create_store(&SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: session_id.to_string(),
                relation: SessionRelation::Root,
                policy: SessionPolicy::new(TurnBudget::Unbounded),
            })
            .await
            .unwrap_or_else(|error| panic!("admit `{session_id}`: {error}"));
    }
    let unbound = Store::open(&factory.catalog_path())
        .await
        .expect("open unbound SQLite store");

    lash_core::testing::conformance::unbound_session_meta_refuses_ambiguous_resolution(
        "SQLite",
        unbound.load_session_meta(),
    )
    .await;
}
