//! Artifact reads on PostgreSQL.

// Test code: the server comes from the environment the target's runner
// hands it.
#![allow(clippy::disallowed_methods)]

use lash_core_store::artifact_referrer::{ArtifactReferrer, HostArtifactPin, ReferrerClaim};
use lash_execution_contracts::module_artifacts::ModuleArtifactStore as _;

use crate::testing::IsolatedDatabase;

/// A read decodes every referrer edge naming the artifact, in stored-pair
/// order, before it answers the bytes: an edge this build cannot read
/// refuses the read even though the bytes are stored, and the refusal is
/// the first such edge's (FIG-7701 R1, review item 1).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_read_refuses_on_the_first_unreadable_edge_before_its_bytes() {
    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping the artifact-read edge law: database URL is not set");
        return;
    };
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store");
    let store = storage.process_env_store();
    let claim = ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint()))
        .expect("pin claim");
    let document = lash_kernel_value::names::DocumentId::from_bytes([7; 32]).to_string();
    store
        .publish_module_artifact(&claim, &document, b"stored document")
        .await
        .expect("publish the document");
    assert_eq!(
        store
            .get_module_artifact(&document)
            .await
            .expect("a read under readable edges"),
        Some(b"stored document".to_vec()),
    );

    // Two edges no referrer decode accepts. In stored-pair order `[1` comes
    // before `x`; their decode errors differ (`[1` names a non-string
    // element, `x` is no JSON value).
    for id in ["x", "[1"] {
        sqlx::query(
            "INSERT INTO lash_artifact_referrer_edges
                 (namespace, artifact_ref, referrer_kind, referrer_id)
             VALUES ('kernel_document', $1, 'frame_environment', $2)",
        )
        .bind(&document)
        .bind(id)
        .execute(storage.pool())
        .await
        .expect("plant an unreadable edge");
    }
    let refusal = store
        .get_module_artifact(&document)
        .await
        .expect_err("an unreadable edge refuses the read");
    let message = refusal.to_string();
    assert!(
        message.contains("malformed frame_environment referrer id")
            && message.contains("invalid type: integer"),
        "the first edge in stored-pair order refuses: {message}"
    );
}

/// Wait until some statement waits on an advisory lock.
async fn until_a_lock_waits(pool: &sqlx::PgPool) {
    for _ in 0..200 {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND NOT granted",
        )
        .fetch_one(pool)
        .await
        .expect("read pg_locks");
        if waiting > 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("no statement waited on the held lock");
}

/// A definition acquisition refuses the first member missing in its
/// manifest's order, and only then its descriptor: not the first in lock
/// order, which sorts the same members (FIG-7701 R3, review item 28).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_acquisition_refuses_the_first_missing_member_in_request_order() {
    use lash_core_store::artifact_referrer::{ArtifactName, ArtifactStoreId};
    use lash_execution_contracts::module_artifacts::ArtifactStoreError;
    use lash_execution_contracts::process::ProcessDefinitionStore as _;

    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping the acquisition refusal-order law: database URL is not set");
        return;
    };
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store");
    let store = storage.process_env_store();
    let claim = ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint()))
        .expect("pin claim");
    let mut members: Vec<String> = [[0x11; 32], [0xee; 32]]
        .into_iter()
        .map(|bytes| lash_kernel_value::names::DocumentId::from_bytes(bytes).to_string())
        .collect();
    // Request the member that locks last first.
    members
        .sort_by_key(|member| super::artifact_lock_key(super::MODULE_ARTIFACT_NAMESPACE, member));
    members.reverse();
    let manifest: Vec<ArtifactName> = members
        .iter()
        .map(|member| ArtifactName {
            store: ArtifactStoreId::KernelDocument,
            artifact_ref: member.clone(),
        })
        .collect();
    let id = lash_protocol_types::ProcessDefinitionId::from_sha256_digest([5; 32]);
    let missing = |error: ArtifactStoreError| match error {
        ArtifactStoreError::ArtifactMissing { artifact_ref } => artifact_ref,
        other => panic!("a missing member refuses as missing: {other:?}"),
    };

    let refused = store
        .acquire_process_definition(&claim, &id, &manifest)
        .await
        .expect_err("nothing is stored");
    assert_eq!(missing(refused), members[0], "the first in request order");
    store
        .publish_module_artifact(&claim, &members[0], b"first member")
        .await
        .expect("publish the first member");
    let refused = store
        .acquire_process_definition(&claim, &id, &manifest)
        .await
        .expect_err("the second member is missing");
    assert_eq!(missing(refused), members[1]);
    store
        .publish_module_artifact(&claim, &members[1], b"second member")
        .await
        .expect("publish the second member");
    let refused = store
        .acquire_process_definition(&claim, &id, &manifest)
        .await
        .expect_err("the descriptor is missing");
    assert_eq!(
        missing(refused),
        id.as_str(),
        "the descriptor after its manifest"
    );
}

/// A hold reads its referrer's fence after waiting on the referrer's lock,
/// and whether its artifact is stored after waiting on the artifact's: an
/// end or a reclamation committed by the transaction it waited behind
/// refuses it (FIG-7701 R3, review item 28).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_hold_reads_what_the_locks_it_waited_behind_committed() {
    use lash_execution_contracts::module_artifacts::ArtifactStoreError;

    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping the hold snapshot law: database URL is not set");
        return;
    };
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store");
    let store = std::sync::Arc::new(storage.process_env_store());
    let publisher = ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint()))
        .expect("pin claim");
    let document = lash_kernel_value::names::DocumentId::from_bytes([3; 32]).to_string();
    store
        .publish_module_artifact(&publisher, &document, b"held document")
        .await
        .expect("publish the document");

    // The referrer ends while its hold waits on its lock.
    let ended = ArtifactReferrer::HostPin(HostArtifactPin::mint());
    let mut ender = storage.pool().begin().await.expect("begin the end");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(super::referrer_lock_key(&ended))
        .execute(&mut *ender)
        .await
        .expect("hold the referrer lock");
    sqlx::query(super::artifact_sql().fences.insert_fence.sql())
        .bind(ended.kind().as_str())
        .bind(ended.canonical_id())
        .bind(1_i64)
        .execute(&mut *ender)
        .await
        .expect("fence the referrer");
    let hold = tokio::spawn({
        let store = std::sync::Arc::clone(&store);
        let claim = ReferrerClaim::unguarded(ended.clone()).expect("pin claim");
        let document = document.clone();
        async move { store.acquire_module_artifact(&claim, &document).await }
    });
    until_a_lock_waits(storage.pool()).await;
    ender.commit().await.expect("commit the end");
    assert!(
        matches!(
            hold.await.expect("the hold ran"),
            Err(ArtifactStoreError::ReferrerEnded { referrer }) if referrer == ended
        ),
        "the fence committed behind the lock refuses the hold"
    );

    // The artifact is reclaimed while a hold waits on its lock.
    let mut reclaimer = storage.pool().begin().await.expect("begin the reclamation");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(super::artifact_lock_key(
            super::MODULE_ARTIFACT_NAMESPACE,
            &document,
        ))
        .execute(&mut *reclaimer)
        .await
        .expect("hold the artifact lock");
    for statement in [
        "DELETE FROM lash_artifact_referrer_edges WHERE namespace = 'kernel_document' AND artifact_ref = $1",
        "DELETE FROM lash_lash_vm_artifacts WHERE namespace = 'kernel_document' AND artifact_ref = $1",
    ] {
        sqlx::query(statement)
            .bind(&document)
            .execute(&mut *reclaimer)
            .await
            .expect("reclaim the document");
    }
    let hold = tokio::spawn({
        let store = std::sync::Arc::clone(&store);
        let claim = ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint()))
            .expect("pin claim");
        let document = document.clone();
        async move { store.acquire_module_artifact(&claim, &document).await }
    });
    until_a_lock_waits(storage.pool()).await;
    reclaimer.commit().await.expect("commit the reclamation");
    assert!(
        matches!(
            hold.await.expect("the hold ran"),
            Err(ArtifactStoreError::ArtifactMissing { artifact_ref }) if artifact_ref == document
        ),
        "the reclamation committed behind the lock refuses the hold"
    );
}

/// An environment over one link and the document it links, published by
/// `claim`: its reference, its bytes and the document.
pub(crate) async fn published_environment(
    store: &crate::PostgresLashVmArtifactStore,
    claim: &ReferrerClaim,
    round: u8,
) -> (
    lash_execution_contracts::process::ProcessEnvRef,
    Vec<u8>,
    String,
) {
    use lash_durable::packages::PackageStore as _;
    use lash_execution_contracts::process::ProcessEnvStore as _;

    let document = lash_kernel_value::names::DocumentId::from_bytes([round; 32]);
    store
        .publish_module_artifact(claim, &document.to_string(), b"linked document")
        .await
        .expect("publish the document");
    let link = lash_kernel_value::package::Link {
        helper_fills: Default::default(),
        document,
        fills: Default::default(),
    };
    let link = lash_core_store::packages::LinkRecord {
        id: link.identity(),
        link,
    };
    store.put_link(claim, &link, &[]).await.expect("link");
    let (env_ref, bytes) = environment_over(round, None);
    store
        .publish_process_env(claim, &env_ref, &bytes)
        .await
        .expect("publish the environment");
    (env_ref, bytes, document.to_string())
}

/// The environment over [`published_environment`]'s link for `round`,
/// with capability `account` when given: another head over the same body.
pub(crate) fn environment_over(
    round: u8,
    account: Option<&str>,
) -> (lash_execution_contracts::process::ProcessEnvRef, Vec<u8>) {
    use lash_core_store::run_spec::{BindingId, CapabilityRef, ContractRef, SlotId};

    let link = lash_kernel_value::package::Link {
        helper_fills: Default::default(),
        document: lash_kernel_value::names::DocumentId::from_bytes([round; 32]),
        fills: Default::default(),
    };
    let mut execution = lash_core_store::tool_surface::ExecutionEnvironment::empty(
        lash_vocabulary::claims::RegistryKind::Process,
    );
    execution.authored.insert(link.identity());
    let mut builder = lash_core_store::process_env::ProcessEnv::builder(
        lash_protocol_types::session_model::MaxToolCalls::new(1024),
    )
    .execution(execution);
    if let Some(account) = account {
        builder = builder.capability(
            SlotId::new("integration"),
            CapabilityRef {
                binding: BindingId::new("binding:recorded"),
                contract: ContractRef::new("integration", 7),
                args: serde_json::json!({ "account": account }),
            },
        );
    }
    let env = builder.build().expect("environment");
    (
        env.stable_ref().expect("environment reference"),
        env.to_store_bytes().expect("environment bytes"),
    )
}

/// The dependency edges stored, in key order.
async fn dependency_edges(pool: &sqlx::PgPool) -> Vec<(String, String, String, String)> {
    sqlx::query_as(
        "SELECT holder_namespace, holder_ref, namespace, artifact_ref
         FROM lash_artifact_dependency_edges ORDER BY 1, 2, 3, 4",
    )
    .fetch_all(pool)
    .await
    .expect("read the dependency edges")
}

/// An environment published over a stored body answers as a first
/// publication of both does: a corrupt stored body, or a corrupt stored
/// head, refuses as Immutable; missing dependency edges are written again;
/// and a closure module reclaimed by the transaction its lock waited behind
/// refuses as missing (FIG-7701 R5, review items 22 and 29). Other bytes
/// under a reference never reach the store: `publish_process_env` refuses
/// them first.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_publication_over_a_stored_body_answers_as_a_first_publication() {
    use lash_core_store::process_env::stored;
    use lash_execution_contracts::module_artifacts::ArtifactStoreError;
    use lash_execution_contracts::process::ProcessEnvStore as _;

    let Some(database_url) = crate::postgres_test_support::database_url() else {
        eprintln!("skipping the stored-body publication law: database URL is not set");
        return;
    };
    let database = IsolatedDatabase::create(&database_url).await;
    let storage = crate::testing::connect(database.url())
        .await
        .expect("open the isolated store");
    let store = std::sync::Arc::new(storage.process_env_store());
    let pool = storage.pool();
    let claim = ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint()))
        .expect("pin claim");
    let (env_ref, bytes, document) = published_environment(&store, &claim, 21).await;
    let split = stored::split(&bytes).expect("the environment stores split");
    let (other_ref, other_bytes) = environment_over(21, Some("other"));
    assert_eq!(
        stored::split(&other_bytes).map(|other| other.body_ref),
        Some(split.body_ref.clone()),
        "another head over the same body"
    );
    let edges = dependency_edges(pool).await;
    assert!(
        edges.iter().any(|(holder_namespace, _, _, artifact_ref)| {
            holder_namespace == stored::BODY_NAMESPACE && *artifact_ref == document
        }),
        "the body holds the document: {edges:?}"
    );
    let immutable = |error: ArtifactStoreError, expected: &str| match error {
        ArtifactStoreError::Immutable { artifact_ref } => assert_eq!(artifact_ref, expected),
        other => panic!("refuses as Immutable: {other:?}"),
    };
    let publisher = || {
        ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint()))
            .expect("pin claim")
    };

    // A corrupt stored body.
    let set_body = |body: Vec<u8>| {
        sqlx::query(
            "UPDATE lash_lash_vm_artifacts SET artifact_bytes = $2
             WHERE namespace = 'process_env_body' AND artifact_ref = $1",
        )
        .bind(split.body_ref.clone())
        .bind(body)
        .execute(pool)
    };
    set_body(b"{}".to_vec()).await.expect("corrupt the body");
    immutable(
        store
            .publish_process_env(&publisher(), &other_ref, &other_bytes)
            .await
            .expect_err("a corrupt stored body"),
        other_ref.as_str(),
    );
    set_body(split.body.clone())
        .await
        .expect("restore the body");

    // A corrupt stored head.
    let set_head = |head: Vec<u8>| {
        sqlx::query(
            "UPDATE lash_lash_vm_artifacts SET artifact_bytes = $2
             WHERE namespace = 'process_execution_env' AND artifact_ref = $1",
        )
        .bind(env_ref.as_str().to_owned())
        .bind(head)
        .execute(pool)
    };
    set_head([split.head.as_slice(), b" "].concat())
        .await
        .expect("corrupt the head");
    immutable(
        store
            .publish_process_env(&publisher(), &env_ref, &bytes)
            .await
            .expect_err("a corrupt stored head"),
        env_ref.as_str(),
    );
    set_head(split.head.clone())
        .await
        .expect("restore the head");

    // The body's missing closure edges are written again, beside the new
    // head's edge to the body.
    sqlx::query(
        "DELETE FROM lash_artifact_dependency_edges WHERE holder_namespace = 'process_env_body'",
    )
    .execute(pool)
    .await
    .expect("drop the body's edges");
    store
        .publish_process_env(&publisher(), &other_ref, &other_bytes)
        .await
        .expect("publish over the stored body");
    let mut expected = edges.clone();
    expected.push((
        super::PROCESS_ENV_NAMESPACE.to_owned(),
        other_ref.as_str().to_owned(),
        stored::BODY_NAMESPACE.to_owned(),
        split.body_ref.clone(),
    ));
    expected.sort();
    assert_eq!(
        dependency_edges(pool).await,
        expected,
        "the edges are repaired"
    );

    // A closure module reclaimed while the publication waits on its lock.
    let mut reclaimer = pool.begin().await.expect("begin the reclamation");
    sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
        .bind(super::artifact_lock_key(
            super::MODULE_ARTIFACT_NAMESPACE,
            &document,
        ))
        .execute(&mut *reclaimer)
        .await
        .expect("hold the document lock");
    for statement in [
        "DELETE FROM lash_artifact_dependency_edges WHERE namespace = 'kernel_document' AND artifact_ref = $1",
        "DELETE FROM lash_artifact_referrer_edges WHERE namespace = 'kernel_document' AND artifact_ref = $1",
        "DELETE FROM lash_lash_vm_artifacts WHERE namespace = 'kernel_document' AND artifact_ref = $1",
    ] {
        sqlx::query(statement)
            .bind(&document)
            .execute(&mut *reclaimer)
            .await
            .expect("reclaim the document");
    }
    let (third_ref, third_bytes) = environment_over(21, Some("third"));
    let publication = tokio::spawn({
        let store = std::sync::Arc::clone(&store);
        let claim = publisher();
        async move {
            store
                .publish_process_env(&claim, &third_ref, &third_bytes)
                .await
        }
    });
    until_a_lock_waits(pool).await;
    reclaimer.commit().await.expect("commit the reclamation");
    assert!(
        matches!(
            publication.await.expect("the publication ran"),
            Err(ArtifactStoreError::ArtifactMissing { artifact_ref }) if artifact_ref == document
        ),
        "the reclamation committed behind the lock refuses the publication"
    );
}
