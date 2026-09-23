//! A file database written by main's per-component constructors opens through
//! `SqliteDeployment::open(root)` with every persisted binding unchanged
//! (ADR 0102, FIG-2971): the deployment answers to the same identity its
//! effect host always bound turn control to, so a session's recorded
//! turn-cancellation binding still matches.

use std::sync::Arc;

use lash_core_execution::{
    EffectHost as _, ExecutionScope, LeaseOwnerIdentity, SessionId, SessionStoreFactory as _,
};
use lash_sqlite_store::{
    SqliteDatabase, SqliteDeployment, SqliteEffectHost, SqliteSessionStoreFactory,
};

const SESSION: &str = "deployment-binding-compat";

async fn validate_binding(
    store: &Arc<dyn lash_core_execution::RuntimePersistence>,
    binding_id: &str,
) -> Result<(), lash_core_execution::StoreError> {
    let owner = LeaseOwnerIdentity::opaque("binding-compat", "binding-compat:incarnation");
    let lease = store
        .try_claim_session_execution_lease(&SessionId::from(SESSION), &owner, "executor", 60_000)
        .await
        .expect("claim the session lease")
        .acquired()
        .expect("the session lease is free");
    store
        .validate_turn_cancellation_binding(
            &SessionId::from(SESSION),
            &lease.authority(),
            binding_id,
            &ExecutionScope::turn(SessionId::from(SESSION), "compat-turn"),
        )
        .await
}

#[tokio::test]
async fn a_file_database_from_the_component_constructors_opens_as_a_deployment() {
    let dir = tempfile::tempdir().expect("deployment root");
    let request = lash_core_execution::testing::store_fixtures::session_store_request(
        &SessionId::from(SESSION),
        SESSION,
        lash_core_execution::SessionRelation::Root,
    );

    // Main's wiring: a factory over the root and a host over its journal.
    let recorded_binding = {
        let factory = SqliteSessionStoreFactory::new(dir.path());
        let host =
            SqliteEffectHost::open(&dir.path().join(SqliteDatabase::EffectReplay.file_name()))
                .await
                .expect("open the component host");
        let store = factory
            .create_store(&request)
            .await
            .expect("create the session");
        let binding = host.turn_control_binding_id();
        validate_binding(&store, &binding)
            .await
            .expect("the session records the host's binding");
        binding
    };

    let deployment = SqliteDeployment::open(dir.path())
        .await
        .expect("open the existing root as a deployment");
    let store = deployment
        .session_store_factory()
        .open_existing_store(&request)
        .await
        .expect("reopen the session")
        .expect("the session exists");
    validate_binding(&store, &deployment.effect_host().turn_control_binding_id())
        .await
        .expect("the existing session activates with no TurnCancelBindingMismatch");
    assert_eq!(
        lash_core_execution::Deployment::binding_identity(&deployment),
        recorded_binding,
        "the deployment answers to the identity the component host persisted"
    );
}
