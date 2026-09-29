//! Referrer laws shared by SQLite and PostgreSQL artifact stores.
//!
//! The fixture opens both artifact ports over one durable catalog. Each law
//! uses freshly minted pins so no test relies on another test's rows.

use crate::fused_artifact_store::ReopenableArtifactStore;
use lash_core::{
    ArtifactCarry, ArtifactName, ArtifactReferrer, ArtifactReferrerKind, ArtifactStoreError,
    ArtifactStoreId, FrameEnvironmentId, HostArtifactPin, ReferrerClaim, ResolvedArtifactCleanup,
};
use lashlang::testing::ast_builders as b;
use lashlang::{ModuleArtifact, TypeExpr};
use pretty_assertions::assert_eq;

#[expect(clippy::expect_used, reason = "test fixture constructs a valid module")]
fn module(name: &str) -> ModuleArtifact {
    ModuleArtifact::from_program(b::module(
        vec![b::process_returning(
            name,
            vec![b::param("root", TypeExpr::Str)],
            TypeExpr::Str,
            b::finish(b::var("root")),
        )],
        Vec::new(),
    ))
    .expect("module fixture")
}

#[expect(clippy::expect_used, reason = "test fixture constructs a valid claim")]
fn pin_claim() -> (ArtifactReferrer, ReferrerClaim) {
    let referrer = ArtifactReferrer::HostPin(HostArtifactPin::mint());
    let claim = ReferrerClaim::unguarded(referrer.clone()).expect("host pin claim");
    (referrer, claim)
}

fn end(referrer: ArtifactReferrer) -> ResolvedArtifactCleanup {
    ResolvedArtifactCleanup {
        referrer,
        carries: Vec::new(),
    }
}

/// ADR 0113 §7.5: a publication paused before its store transaction cannot
/// acquire a frame that was fenced while it waited. Its execution claim can
/// still publish the same immutable bytes.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn publication_racing_frame_end_is_fenced<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let handles = make().open;
    let artifact = module("racing");
    let key = artifact.module_ref().as_str().to_owned();
    let bytes = artifact.to_store_bytes().expect("module bytes");
    let frame = ArtifactReferrer::FrameEnvironment(FrameEnvironmentId::new(
        lash_core::SessionId::from(format!("artifact-race-{}", HostArtifactPin::mint())),
        lash_core::FrameNodeId::new("frame-1").expect("frame node id"),
    ));
    let frame_claim = ReferrerClaim::unguarded(frame.clone()).expect("frame claim");
    let pause = handles
        .artifacts
        .pause_next_publication_for_testing()
        .expect("backend supports the publication pause");
    let writer_store = handles.artifacts.clone();
    let writer_key = key.clone();
    let writer_bytes = bytes.clone();
    let writer = tokio::spawn(async move {
        writer_store
            .publish_module_artifact(&frame_claim, &writer_key, &writer_bytes)
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !pause.is_reached() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("publication reaches its pause");
    handles
        .artifacts
        .end_module_referrer(&end(frame.clone()))
        .await
        .expect("fence frame");
    pause.resume();
    let refusal = writer
        .await
        .expect("writer joins")
        .expect_err("ended frame refuses publication");
    assert!(matches!(refusal, ArtifactStoreError::ReferrerEnded { referrer } if referrer == frame));
    let journal = lash_core::ExecutionScope::runtime_operation("artifact-race")
        .journal_identity()
        .expect("execution journal");
    let execution = ReferrerClaim::guarded(
        ArtifactReferrer::Execution(journal),
        lash_core::ArtifactCleanupPlan::AwaitJournal,
    )
    .expect("execution claim");
    handles
        .artifacts
        .publish_module_artifact(&execution, &key, &bytes)
        .await
        .expect("execution publication survives frame end");
    assert_eq!(
        handles
            .artifacts
            .get_module_artifact(&key)
            .await
            .expect("read"),
        Some(bytes)
    );
}

/// ADR 0113 §7.14: the same pin holds exact edges in both stores, then cannot
/// publish again after release. A newly minted pin remains usable.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn host_pins_reclaim_and_fence<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let handles = make().open;
    let artifact = module("pinned");
    let module_ref = artifact.module_ref().as_str();
    let module_bytes = artifact.to_store_bytes().expect("module bytes");
    let env = lash_core::ProcessExecutionEnvSpec::new(
        lash_core::PluginOptions::default(),
        lash_core::SessionPolicy::new(lash_core::TurnBudget::Unbounded),
    );
    let env_ref = env.stable_ref().expect("env ref");
    let env_bytes = env.to_store_bytes().expect("env bytes");
    let (pin, claim) = pin_claim();
    handles
        .artifacts
        .publish_module_artifact(&claim, module_ref, &module_bytes)
        .await
        .expect("publish module");
    handles
        .process_env
        .publish_process_execution_env(&claim, &env_ref, &env_bytes)
        .await
        .expect("publish environment");
    assert_eq!(
        handles
            .artifacts
            .get_module_artifact(module_ref)
            .await
            .expect("read"),
        Some(module_bytes.clone())
    );
    assert_eq!(
        handles
            .process_env
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        Some(env_bytes.clone())
    );
    handles
        .artifacts
        .end_module_referrer(&end(pin.clone()))
        .await
        .expect("end module pin");
    handles
        .process_env
        .end_process_env_referrer(&end(pin.clone()))
        .await
        .expect("end env pin");
    assert_eq!(
        handles
            .artifacts
            .get_module_artifact(module_ref)
            .await
            .expect("read"),
        None
    );
    assert_eq!(
        handles
            .process_env
            .get_process_execution_env(&env_ref)
            .await
            .expect("read"),
        None
    );
    let module_error = handles
        .artifacts
        .publish_module_artifact(&claim, module_ref, &module_bytes)
        .await
        .expect_err("released pin refuses module");
    assert!(
        matches!(module_error, ArtifactStoreError::ReferrerEnded { referrer } if referrer == pin)
    );
    let env_error = handles
        .process_env
        .publish_process_execution_env(&claim, &env_ref, &env_bytes)
        .await
        .expect_err("released pin refuses environment");
    assert!(matches!(env_error, ArtifactStoreError::ReferrerEnded { referrer } if referrer == pin));
    let (_, fresh) = pin_claim();
    handles
        .artifacts
        .publish_module_artifact(&fresh, module_ref, &module_bytes)
        .await
        .expect("fresh pin publishes");
    assert_eq!(
        handles
            .artifacts
            .get_module_artifact(module_ref)
            .await
            .expect("read"),
        Some(module_bytes)
    );
}

/// ADR 0113 §7.16: replaying a cleanup, including a carry into a pin that
/// has since ended, cannot recreate an edge or bytes.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn retry_idempotency_after_destination_ends<F>(make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let handles = make().open;
    let artifact = module("idempotent");
    let key = artifact.module_ref().as_str();
    let bytes = artifact.to_store_bytes().expect("module bytes");
    let (source, source_claim) = pin_claim();
    let (destination, _) = pin_claim();
    handles
        .artifacts
        .publish_module_artifact(&source_claim, key, &bytes)
        .await
        .expect("publish source");
    let carry = ResolvedArtifactCleanup {
        referrer: source.clone(),
        carries: vec![ArtifactCarry {
            artifact: ArtifactName {
                store: ArtifactStoreId::LashlangModule,
                artifact_ref: key.to_owned(),
            },
            to: destination.clone(),
        }],
    };
    handles
        .artifacts
        .end_module_referrer(&carry)
        .await
        .expect("first delivery");
    assert_eq!(
        handles
            .artifacts
            .get_module_artifact(key)
            .await
            .expect("read"),
        Some(bytes)
    );
    handles
        .artifacts
        .end_module_referrer(&end(destination))
        .await
        .expect("end destination");
    for _ in 0..2 {
        handles
            .artifacts
            .end_module_referrer(&carry)
            .await
            .expect("replay delivery");
        assert_eq!(
            handles
                .artifacts
                .get_module_artifact(key)
                .await
                .expect("read"),
            None
        );
    }
}

/// ADR 0113 §7.15: every kind has a canonical encoding. Stored corruption
/// checks additionally need each backend's raw SQL fixture.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates canonical fixtures"
)]
pub async fn every_referrer_kind_has_one_canonical_id<F>(_make: F)
where
    F: Fn() -> ReopenableArtifactStore,
{
    let journal = lash_core::ExecutionScope::runtime_operation("canonical-referrer")
        .journal_identity()
        .expect("journal identity");
    let values = [
        ArtifactReferrer::FrameEnvironment(FrameEnvironmentId::new(
            lash_core::SessionId::from("canonical-session"),
            lash_core::FrameNodeId::new("canonical-frame").expect("frame id"),
        )),
        ArtifactReferrer::ProcessRecord(lash_core::ProcessId::fixture("canonical-process")),
        ArtifactReferrer::SubscriptionRevision(
            lash_core::SubscriptionRevisionId::new("sub".into(), "incarnation".into(), 1)
                .expect("subscription revision"),
        ),
        ArtifactReferrer::Start(lash_core::StartKey::for_host(
            lash_core::StartKeyOwner::HOST,
            "canonical-start",
        )),
        ArtifactReferrer::Execution(journal),
        ArtifactReferrer::HostPin(HostArtifactPin::mint()),
        ArtifactReferrer::DefinitionRevision(
            lash_core::DefinitionRevisionId::new("definition".into(), 1)
                .expect("definition revision"),
        ),
    ];
    assert_eq!(values.len(), ArtifactReferrerKind::ALL.len());
    for value in values {
        assert_eq!(
            ArtifactReferrer::decode(value.kind().as_str(), &value.canonical_id())
                .expect("decode canonical id"),
            value,
        );
    }
    for kind in ArtifactReferrerKind::ALL {
        assert!(
            ArtifactReferrer::decode(kind.as_str(), "").is_err(),
            "{kind} rejects empty id"
        );
    }
    assert!(ArtifactReferrer::decode("owner", "old").is_err());
    assert!(ArtifactReferrer::decode("host_pin", "host-pin:v1:INVALID").is_err());
}
