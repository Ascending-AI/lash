//! Module artifact store laws shared by durable store implementations.
//!
//! The store holds bytes under a content reference and knows nothing of
//! what they encode, so the laws publish opaque samples.

use std::sync::Arc;

use lash_core::{
    ArtifactCarry, ArtifactName, ArtifactReferrer, ArtifactStoreError, ArtifactStoreId,
    DurabilityTier, HostArtifactPin, ModuleArtifactStore, ReferrerClaim, ReferrerGuard,
    ResolvedArtifactCleanup,
};

/// A writer and a fresh handle over the same durable store.
pub struct ReopenableModuleArtifactStore {
    pub open: Arc<dyn ModuleArtifactStore>,
    pub reopen: Arc<dyn Fn() -> Arc<dyn ModuleArtifactStore> + Send + Sync>,
}

/// Bytes a store can publish, and the content reference they publish under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SampleArtifact {
    bytes: Vec<u8>,
    module_ref: lash_sansio::ModuleRef,
}

impl SampleArtifact {
    /// A sample that differs for each `name`.
    pub fn named(name: &str) -> Self {
        let bytes = format!(r#"{{"sample_module":"{name}"}}"#).into_bytes();
        let module_ref = lash_sansio::ModuleRef::new(&lash_sansio::ContentHash::new(
            blake3::hash(&bytes).to_hex().to_string(),
        ));
        Self { bytes, module_ref }
    }

    pub fn module_ref(&self) -> &lash_sansio::ModuleRef {
        &self.module_ref
    }

    pub fn to_store_bytes(&self) -> Result<Vec<u8>, std::convert::Infallible> {
        Ok(self.bytes.clone())
    }
}

fn module(name: &str) -> SampleArtifact {
    SampleArtifact::named(name)
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

pub async fn lash_vm_artifact_store_fresh_instances<F>(make: &F)
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

pub async fn lash_vm_artifact_store_durability_tier(
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
    let start = lash_core::StartKey::for_host("abandoned-module");
    let journal = lash_core::ExecutionScope::runtime_operation("abandoned-module")
        .journal_identity()
        .expect("starter journal");
    let referrer = ArtifactReferrer::Start(start.clone());
    let claim = ReferrerClaim::guarded(ReferrerGuard::Start {
        start_key: start.clone(),
        starter: journal,
    });
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
                store: ArtifactStoreId::VmModule,
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
pub async fn survives_reopen(reopenable: ReopenableModuleArtifactStore) {
    let ReopenableModuleArtifactStore { open, reopen } = reopenable;
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
