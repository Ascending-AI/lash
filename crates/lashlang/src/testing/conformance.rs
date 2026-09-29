//! Module artifact store laws shared by durable store implementations.

use std::sync::Arc;

use lash_core_execution::{
    ArtifactCarry, ArtifactCleanupPlan, ArtifactName, ArtifactReferrer, ArtifactStoreError,
    ArtifactStoreId, DurabilityTier, HostArtifactPin, ModuleArtifactStore, ReferrerClaim,
    ResolvedArtifactCleanup,
};

use super::ast_builders as b;
use crate::{ModuleArtifact, TypeExpr};

/// A writer and a fresh handle over the same durable store.
pub struct ReopenableLashlangArtifactStore {
    pub open: Arc<dyn ModuleArtifactStore>,
    pub reopen: Arc<dyn Fn() -> Arc<dyn ModuleArtifactStore> + Send + Sync>,
}

#[expect(clippy::expect_used, reason = "test fixture constructs a valid module")]
fn module(name: &str) -> ModuleArtifact {
    let program = b::module(
        vec![b::process_returning(
            name,
            vec![b::param("root", TypeExpr::Str)],
            TypeExpr::Str,
            b::finish(b::var("root")),
        )],
        Vec::new(),
    );
    ModuleArtifact::from_program(program).expect("build module")
}

#[expect(clippy::expect_used, reason = "test fixture constructs a valid claim")]
fn host_claim() -> (ArtifactReferrer, ReferrerClaim) {
    let referrer = ArtifactReferrer::HostPin(HostArtifactPin::mint());
    let claim = ReferrerClaim::unguarded(referrer.clone()).expect("host claim");
    (referrer, claim)
}

fn end(referrer: ArtifactReferrer) -> ResolvedArtifactCleanup {
    ResolvedArtifactCleanup {
        referrer,
        carries: Vec::new(),
    }
}

pub async fn lashlang_artifact_store_fresh_instances<F>(make: &F)
where
    F: Fn() -> Arc<dyn ModuleArtifactStore>,
{
    let first = make();
    let second = make();
    assert!(
        !Arc::ptr_eq(&first, &second),
        "factory reused one store Arc"
    );
}

pub async fn lashlang_artifact_store_durability_tier(
    store: Arc<dyn ModuleArtifactStore>,
    expected: DurabilityTier,
) {
    assert_eq!(store.durability_tier(), expected);
}

/// The last ended referrer reclaims bytes, and repeated cleanup changes nothing.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn last_referrer_reclaims_module(store: Arc<dyn ModuleArtifactStore>) {
    let artifact = module("worker");
    let bytes = artifact.to_store_bytes().expect("module bytes");
    let key = artifact.module_ref().as_str();
    let (first, first_claim) = host_claim();
    let (second, second_claim) = host_claim();
    store
        .publish_module_artifact(&first_claim, key, &bytes)
        .await
        .expect("publish");
    store
        .acquire_module_artifact(&second_claim, key)
        .await
        .expect("acquire");
    store
        .end_module_referrer(&end(first))
        .await
        .expect("end first");
    assert_eq!(
        store.get_module_artifact(key).await.expect("read"),
        Some(bytes.clone())
    );
    store
        .end_module_referrer(&end(second.clone()))
        .await
        .expect("end second");
    store
        .end_module_referrer(&end(second))
        .await
        .expect("replay end");
    assert_eq!(store.get_module_artifact(key).await.expect("read"), None);
}

/// An ended start with no successor reclaims what it staged.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn abandoned_start_reclaims_module(store: Arc<dyn ModuleArtifactStore>) {
    let artifact = module("abandoned");
    let bytes = artifact.to_store_bytes().expect("module bytes");
    let key = artifact.module_ref().as_str();
    let start = lash_core_execution::StartKey::for_host("abandoned-module");
    let journal = lash_core_execution::ExecutionScope::runtime_operation("abandoned-module")
        .journal_identity()
        .expect("starter journal");
    let referrer = ArtifactReferrer::Start(start);
    let claim = ReferrerClaim::guarded(
        referrer.clone(),
        ArtifactCleanupPlan::AwaitStart { starter: journal },
    )
    .expect("start claim");
    store
        .publish_module_artifact(&claim, key, &bytes)
        .await
        .expect("stage");
    store
        .end_module_referrer(&end(referrer))
        .await
        .expect("end abandoned start");
    assert_eq!(store.get_module_artifact(key).await.expect("read"), None);
}

/// An atomic cleanup carries an edge before it removes the source edge.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn carry_preserves_module(store: Arc<dyn ModuleArtifactStore>) {
    let artifact = module("carried");
    let bytes = artifact.to_store_bytes().expect("module bytes");
    let key = artifact.module_ref().as_str();
    let (source, claim) = host_claim();
    let (destination, _) = host_claim();
    store
        .publish_module_artifact(&claim, key, &bytes)
        .await
        .expect("publish source");
    let cleanup = ResolvedArtifactCleanup {
        referrer: source.clone(),
        carries: vec![ArtifactCarry {
            artifact: ArtifactName {
                store: ArtifactStoreId::LashlangModule,
                artifact_ref: key.to_owned(),
            },
            to: destination.clone(),
        }],
    };
    store.end_module_referrer(&cleanup).await.expect("carry");
    store
        .end_module_referrer(&cleanup)
        .await
        .expect("replay carry");
    assert_eq!(
        store.get_module_artifact(key).await.expect("read"),
        Some(bytes)
    );
    let refusal = store
        .acquire_module_artifact(&claim, key)
        .await
        .expect_err("source fenced");
    assert!(
        matches!(refusal, ArtifactStoreError::ReferrerEnded { referrer } if referrer == source)
    );
    store
        .end_module_referrer(&end(destination))
        .await
        .expect("end destination");
    assert_eq!(store.get_module_artifact(key).await.expect("read"), None);
}

/// A completed cleanup permanently fences its referrer.
#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn ended_referrer_fences_late_publication(store: Arc<dyn ModuleArtifactStore>) {
    let artifact = module("fenced");
    let bytes = artifact.to_store_bytes().expect("module bytes");
    let (referrer, claim) = host_claim();
    store
        .end_module_referrer(&end(referrer.clone()))
        .await
        .expect("end pin");
    let refusal = store
        .publish_module_artifact(&claim, artifact.module_ref().as_str(), &bytes)
        .await
        .expect_err("fenced publication");
    assert!(
        matches!(refusal, ArtifactStoreError::ReferrerEnded { referrer: ended } if ended == referrer)
    );
}

#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn survives_reopen(reopenable: ReopenableLashlangArtifactStore) {
    let ReopenableLashlangArtifactStore { open, reopen } = reopenable;
    let first_handle = Arc::downgrade(&open);
    let artifact = module("reopen");
    let bytes = artifact.to_store_bytes().expect("module bytes");
    let key = artifact.module_ref().as_str();
    let (referrer, claim) = host_claim();
    open.publish_module_artifact(&claim, key, &bytes)
        .await
        .expect("publish");
    drop(open);
    let reopened = reopen();
    assert!(!std::sync::Weak::ptr_eq(
        &first_handle,
        &Arc::downgrade(&reopened)
    ));
    assert_eq!(
        reopened.get_module_artifact(key).await.expect("read"),
        Some(bytes)
    );
    reopened
        .end_module_referrer(&end(referrer))
        .await
        .expect("end after reopen");
    assert_eq!(reopened.get_module_artifact(key).await.expect("read"), None);
}

pub async fn hostile_module_references_are_rejected(store: Arc<dyn ModuleArtifactStore>) {
    for raw in ["", "nul\0reference"] {
        assert!(store.get_module_artifact(raw).await.is_err());
    }
}

#[expect(clippy::expect_used, reason = "test fixture constructs a valid module")]
fn alpha_variant_module_artifact(binding: &str) -> ModuleArtifact {
    ModuleArtifact::from_program(b::module(
        vec![b::process_returning(
            "worker",
            vec![b::param("tick", TypeExpr::Str)],
            TypeExpr::Bool,
            b::block(vec![b::finish(b::bool_lit(true))]),
        )],
        vec![
            b::assign(binding, b::string("seed")),
            b::finish(b::var(binding)),
        ],
    ))
    .expect("alpha-variant module")
}

#[expect(clippy::expect_used, reason = "test fixture constructs a valid module")]
fn number_module_artifact(value: f64) -> ModuleArtifact {
    ModuleArtifact::from_program(b::program(vec![b::finish(b::num(value))])).expect("number module")
}

#[expect(
    clippy::expect_used,
    reason = "conformance law validates each store step"
)]
pub async fn alpha_variants_publish_distinct_refs(store: Arc<dyn ModuleArtifactStore>) {
    let first = alpha_variant_module_artifact("spawnChild");
    let second = alpha_variant_module_artifact("loadChild");
    assert_ne!(first.module_ref(), second.module_ref());
    let (_, claim) = host_claim();
    for artifact in [&first, &second, &first] {
        store
            .publish_module_artifact(
                &claim,
                artifact.module_ref().as_str(),
                &artifact.to_store_bytes().expect("module bytes"),
            )
            .await
            .expect("publish variant");
    }
    for artifact in [&first, &second] {
        assert_eq!(
            store
                .get_module_artifact(artifact.module_ref().as_str())
                .await
                .expect("read"),
            Some(artifact.to_store_bytes().expect("module bytes")),
        );
    }
    let numbers = [
        f64::NAN,
        f64::from_bits(f64::NAN.to_bits() | 1),
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        -0.0,
    ]
    .map(number_module_artifact);
    assert_eq!(numbers[0].module_ref(), numbers[1].module_ref());
    assert_eq!(numbers[0].source_identity(), numbers[1].source_identity());
    let distinct = [
        &numbers[0],
        &numbers[2],
        &numbers[3],
        &numbers[4],
        &numbers[5],
    ];
    assert_eq!(
        distinct
            .iter()
            .map(|artifact| artifact.module_ref())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        distinct.len(),
    );
    assert_eq!(
        distinct
            .iter()
            .map(|artifact| artifact.source_identity())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        distinct.len(),
    );
    for artifact in distinct {
        let bytes = artifact.to_store_bytes().expect("number module bytes");
        store
            .publish_module_artifact(&claim, artifact.module_ref().as_str(), &bytes)
            .await
            .expect("publish number module");
        let stored = store
            .get_module_artifact(artifact.module_ref().as_str())
            .await
            .expect("read number module")
            .expect("stored number module");
        assert_eq!(stored, bytes);
        assert_eq!(
            ModuleArtifact::from_store_bytes(&stored)
                .expect("decode stored module")
                .source_identity(),
            artifact.source_identity(),
        );
    }
    let mut forged: serde_json::Value =
        serde_json::from_slice(&second.to_store_bytes().expect("module bytes"))
            .expect("module JSON");
    forged["artifact"]["module_ref"] = serde_json::json!(first.module_ref());
    assert!(
        ModuleArtifact::from_store_bytes(&serde_json::to_vec(&forged).expect("forged bytes"),)
            .is_err()
    );
}
