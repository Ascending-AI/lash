use lash_core::{
    SessionPolicy, SessionRelation, SessionStoreCreateRequest, SessionStoreFactory, TurnBudget,
};
use lash_sansio::SessionId;
use lash_sqlite_store::{SqliteSessionStoreFactory, Store};

lash_conformance::unbound_session_meta_tests!({
    let dir = tempfile::tempdir().expect("unbound-session-meta tempdir");
    let factory = SqliteSessionStoreFactory::new(dir.path());
    for session_id in ["unbound-session-meta-a", "unbound-session-meta-b"] {
        factory
            .create_store(&SessionStoreCreateRequest {
                pending_observer_intents: Vec::new(),
                session_id: SessionId::from(session_id.to_string()),
                relation: SessionRelation::Root,
                policy: SessionPolicy::new(TurnBudget::Unbounded),
            })
            .await
            .unwrap_or_else(|error| panic!("admit `{session_id}`: {error}"));
    }
    let unbound = Store::open(&factory.catalog_path())
        .await
        .expect("open unbound SQLite store");
    (
        dir,
        "SQLite",
        async move { unbound.load_session_meta().await },
    )
});
