//! Conformance for referrer-bound process environment storage.

use super::*;
use pretty_assertions::assert_eq;

/// A writer and a new handle over the same durable store.
pub struct ReopenableProcessExecutionEnvStore {
    pub open: Arc<dyn crate::ProcessExecutionEnvStore>,
    pub reopen: Arc<dyn Fn() -> Arc<dyn crate::ProcessExecutionEnvStore> + Send + Sync>,
}

fn sample_env_spec() -> crate::ProcessExecutionEnvSpec {
    crate::ProcessExecutionEnvSpec::new(
        crate::AdmittedPluginConfig::default(),
        SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
    )
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture validates its setup"
)]
fn host_claim() -> (crate::ArtifactReferrer, crate::ReferrerClaim) {
    let referrer = crate::ArtifactReferrer::HostPin(crate::HostArtifactPin::mint());
    let claim = crate::ReferrerClaim::unguarded(referrer.clone()).expect("host pin claim");
    (referrer, claim)
}

fn cleanup(referrer: crate::ArtifactReferrer) -> crate::ResolvedArtifactCleanup {
    crate::ResolvedArtifactCleanup {
        referrer,
        carries: Vec::new(),
    }
}

pub async fn process_execution_env_store_fresh_instances<F>(make: &F)
where
    F: Fn() -> Arc<dyn crate::ProcessExecutionEnvStore>,
{
    let first = make();
    let second = make();
    assert_fresh_instances(&first, &second, "process_execution_env_store");
}

#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store operation"
)]
pub async fn process_env_last_referrer_reclaims_bytes(
    store: Arc<dyn crate::ProcessExecutionEnvStore>,
) {
    let spec = sample_env_spec();
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let (first, first_claim) = host_claim();
    let (second, second_claim) = host_claim();
    store
        .publish_process_execution_env(&first_claim, &env_ref, &bytes)
        .await
        .expect("publish first edge");
    store
        .acquire_process_execution_env(&second_claim, &env_ref)
        .await
        .expect("acquire second edge");
    store
        .end_process_env_referrer(&cleanup(first))
        .await
        .expect("end first referrer");
    assert_eq!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        Some(bytes.clone())
    );
    store
        .end_process_env_referrer(&cleanup(second.clone()))
        .await
        .expect("end last referrer");
    assert_eq!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        None
    );
    store
        .end_process_env_referrer(&cleanup(second))
        .await
        .expect("replayed end");
    assert_eq!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        None
    );
    let refusal = store
        .publish_process_execution_env(&second_claim, &env_ref, &bytes)
        .await
        .expect_err("ended pin is fenced");
    assert!(
        matches!(refusal, crate::ArtifactStoreError::ReferrerEnded { referrer } if referrer == second_claim.referrer().clone())
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store operation"
)]
pub async fn process_env_carry_precedes_reclamation(
    store: Arc<dyn crate::ProcessExecutionEnvStore>,
) {
    let spec = sample_env_spec();
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let (source, source_claim) = host_claim();
    let (destination, destination_claim) = host_claim();
    store
        .publish_process_execution_env(&source_claim, &env_ref, &bytes)
        .await
        .expect("publish source");
    let transfer = crate::ResolvedArtifactCleanup {
        referrer: source.clone(),
        carries: vec![crate::ArtifactCarry {
            artifact: crate::ArtifactName {
                store: crate::ArtifactStoreId::ProcessEnv,
                artifact_ref: env_ref.as_str().to_owned(),
            },
            to: destination.clone(),
        }],
    };
    for _ in 0..2 {
        store
            .end_process_env_referrer(&transfer)
            .await
            .expect("carry is idempotent");
        assert_eq!(
            store
                .get_process_execution_env(&env_ref)
                .await
                .expect("read"),
            Some(bytes.clone())
        );
    }
    let late = store
        .acquire_process_execution_env(&source_claim, &env_ref)
        .await
        .expect_err("source is fenced");
    assert!(
        matches!(late, crate::ArtifactStoreError::ReferrerEnded { referrer } if referrer == source)
    );
    store
        .end_process_env_referrer(&cleanup(destination))
        .await
        .expect("end destination");
    assert_eq!(
        store
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        None
    );
    let late = store
        .acquire_process_execution_env(&destination_claim, &env_ref)
        .await
        .expect_err("destination is fenced");
    assert!(matches!(
        late,
        crate::ArtifactStoreError::ReferrerEnded { .. }
    ));
}

#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store operation"
)]
pub async fn slow_process_env_writer_is_fenced(store: Arc<dyn crate::ProcessExecutionEnvStore>) {
    let spec = sample_env_spec();
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let (referrer, claim) = host_claim();
    let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
    let writer_store = Arc::clone(&store);
    let writer = tokio::spawn(async move {
        resume_rx.await.expect("fence releases writer");
        writer_store
            .publish_process_execution_env(&claim, &env_ref, &bytes)
            .await
    });
    store
        .end_process_env_referrer(&cleanup(referrer.clone()))
        .await
        .expect("end while writer is paused");
    resume_tx.send(()).expect("resume writer");
    let refusal = writer
        .await
        .expect("writer joins")
        .expect_err("fenced writer");
    assert!(
        matches!(refusal, crate::ArtifactStoreError::ReferrerEnded { referrer: ended } if ended == referrer)
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store operation"
)]
pub async fn process_env_survives_reopen(reopenable: ReopenableProcessExecutionEnvStore) {
    let ReopenableProcessExecutionEnvStore { open, reopen } = reopenable;
    let open_identity = Arc::downgrade(&open);
    let spec = sample_env_spec();
    let env_ref = spec.stable_ref().expect("stable env ref");
    let bytes = spec.to_store_bytes().expect("encode env spec");
    let (first, first_claim) = host_claim();
    let (second, second_claim) = host_claim();
    open.publish_process_execution_env(&first_claim, &env_ref, &bytes)
        .await
        .expect("publish first edge");
    open.acquire_process_execution_env(&second_claim, &env_ref)
        .await
        .expect("acquire second edge");
    open.end_process_env_referrer(&cleanup(first.clone()))
        .await
        .expect("end first referrer");
    drop(open);
    let reopened = reopen();
    assert!(
        !std::sync::Weak::ptr_eq(&open_identity, &Arc::downgrade(&reopened)),
        "reopen reused the writer handle"
    );
    assert_eq!(
        reopened
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        Some(bytes)
    );
    reopened
        .end_process_env_referrer(&cleanup(first))
        .await
        .expect("retry end after reopen");
    reopened
        .end_process_env_referrer(&cleanup(second))
        .await
        .expect("end final referrer");
    assert_eq!(
        reopened
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        None
    );
}

/// A recorded prelude of turn `turn`: its history carries `text`.
#[expect(
    clippy::expect_used,
    reason = "conformance fixture validates its setup"
)]
fn sample_turn_prelude(turn: &'static str, text: &str) -> crate::TurnPrelude {
    crate::TurnPrelude {
        configuration: crate::EffectAddress::new(
            crate::ExecutionScope::turn("prelude-session", turn),
            format!("turn-config:{turn}"),
        )
        .expect("turn config address"),
        pressure: Vec::new(),
        history: crate::MessageSequence::from(vec![crate::Message {
            id: format!("{turn}-input"),
            role: crate::MessageRole::User,
            parts: Arc::new(vec![crate::Part::text(
                format!("{turn}-input-part"),
                text.to_owned(),
                None,
            )]),
            origin: None,
            reply_marker: None,
        }]),
        context: Default::default(),
        before_turn: None,
    }
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture validates its setup"
)]
fn turn_journal(turn: &'static str) -> (crate::ExecutionScope, crate::ArtifactReferrer) {
    let scope = crate::ExecutionScope::turn("prelude-session", turn);
    let journal = scope.journal_identity().expect("turn journal identity");
    (scope, crate::ArtifactReferrer::Execution(journal))
}

#[expect(
    clippy::expect_used,
    reason = "conformance fixture validates its setup"
)]
fn prelude_json(prelude: &crate::TurnPrelude) -> serde_json::Value {
    serde_json::to_value(prelude).expect("encode prelude")
}

/// FIG-5133: a turn's environment sync writes its prelude under its digest,
/// held by the turn's journal, before the outcome that names the digest
/// completes. A reader on another handle reads the same prelude back by the
/// digest alone and verifies it; bytes other than the digest's are refused,
/// and a digest nothing wrote reads as missing, typed, never as a prelude.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store operation"
)]
pub async fn turn_prelude_reads_back_by_digest(
    open: Arc<dyn crate::TurnPreludeStore>,
    reopen: Arc<dyn Fn() -> Arc<dyn crate::TurnPreludeStore> + Send + Sync>,
) {
    let prelude = sample_turn_prelude("recorded-turn", "the recorded transcript");
    let (scope, _) = turn_journal("recorded-turn");
    let prelude_ref = prelude
        .record(open.as_ref(), &scope)
        .await
        .expect("record the prelude");
    assert_eq!(
        prelude
            .record(open.as_ref(), &scope)
            .await
            .expect("a redriven body records the same prelude again"),
        prelude_ref
    );
    let reopened = reopen();
    let bytes = reopened
        .get_turn_prelude(&prelude_ref)
        .await
        .expect("read")
        .expect("the recorded prelude is stored");
    assert!(prelude_ref.matches_store_bytes(&bytes));
    let read = prelude_ref
        .read(reopened.as_ref())
        .await
        .expect("the prelude reads back by its digest");
    assert_eq!(prelude_json(&read), prelude_json(&prelude));

    let (_, claim_referrer) = turn_journal("recorded-turn");
    let crate::ArtifactReferrer::Execution(journal) = claim_referrer else {
        unreachable!("a turn journal is an execution referrer");
    };
    let claim = crate::ReferrerClaim::guarded(crate::ReferrerGuard::Journal(journal));
    let refusal = open
        .publish_turn_prelude(&claim, &prelude_ref, b"other bytes")
        .await
        .expect_err("bytes that are not the digest's are refused");
    assert!(
        matches!(refusal, crate::ArtifactStoreError::Immutable { .. }),
        "{refusal:?}"
    );

    let unrecorded = crate::TurnPreludeRef::of_store_bytes(b"never recorded");
    assert_eq!(
        reopened.get_turn_prelude(&unrecorded).await.expect("read"),
        None
    );
    let missing = unrecorded
        .read(reopened.as_ref())
        .await
        .expect_err("a digest nothing wrote is no prelude");
    assert_eq!(missing.code, crate::RuntimeErrorCode::ArtifactMissing);
}

/// FIG-5133: a recorded prelude lives exactly as long as a turn journal that
/// recorded it. Ending one journal keeps the bytes another journal holds;
/// ending the last reclaims them, after which a replay reading the digest
/// meets a typed `ArtifactMissing`, and the ended journal records nothing
/// again.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store operation"
)]
pub async fn turn_prelude_is_released_with_its_journal(store: Arc<dyn crate::TurnPreludeStore>) {
    let prelude = sample_turn_prelude("shared-turn", "a prelude two journals recorded");
    let (first_scope, first) = turn_journal("first-turn");
    let (second_scope, second) = turn_journal("second-turn");
    let prelude_ref = prelude
        .record(store.as_ref(), &first_scope)
        .await
        .expect("the first journal records the prelude");
    assert_eq!(
        prelude
            .record(store.as_ref(), &second_scope)
            .await
            .expect("the second journal records the same bytes"),
        prelude_ref
    );
    store
        .end_turn_prelude_referrer(&cleanup(first))
        .await
        .expect("end the first journal");
    let read = prelude_ref
        .read(store.as_ref())
        .await
        .expect("the second journal still holds the prelude");
    assert_eq!(prelude_json(&read), prelude_json(&prelude));
    for _ in 0..2 {
        store
            .end_turn_prelude_referrer(&cleanup(second.clone()))
            .await
            .expect("ending the last journal is idempotent");
        assert_eq!(
            store.get_turn_prelude(&prelude_ref).await.expect("read"),
            None
        );
    }
    let missing = prelude_ref
        .read(store.as_ref())
        .await
        .expect_err("a released prelude is never re-derived");
    assert_eq!(missing.code, crate::RuntimeErrorCode::ArtifactMissing);
    let fenced = prelude
        .record(store.as_ref(), &second_scope)
        .await
        .expect_err("an ended journal records nothing");
    assert_eq!(fenced.code, crate::RuntimeErrorCode::ArtifactReferrerEnded);
}
