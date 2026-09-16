//! One law for the release stamp both SQL backends write.
//!
//! The stamp is a durable fact with an update rule, and an update rule is
//! exactly the kind of thing two backends implement slightly differently unless
//! one executable law says otherwise. What is certified here is the rule, not
//! the storage: a store records the release that wrote it, a reopen under the
//! same release leaves the record alone, a newer release takes it over, and an
//! older release never claws it back.
//!
//! The law never asserts a *particular* release string. `main` builds every
//! crate as `0.0.0-dev` and the release workflow stamps the real version into an
//! ephemeral checkout, so pinning a literal would certify the build profile
//! rather than the rule. It asserts the relationship between what the deployment
//! reports and what the backend says it is writing.

use async_trait::async_trait;

use lash_core::{StoreError, StoreReleaseStamp, StoreReleaseState, StoreSchemaStatus};

/// One SQL deployment a backend can open, preflight, and have its stamp forced,
/// for the release-stamp law.
///
/// `force_release` exists because the rule's interesting arms are about a stamp
/// written by *another* release, and a conformance run only has this build. It
/// rewrites the stored release string in place and is test-only by contract: no
/// production path may offer it.
#[async_trait]
pub trait ReleaseStampDeployment: Send + Sync {
    /// The release string this build stamps. Whatever the backend writes, the
    /// law compares against this rather than a literal.
    fn build_release(&self) -> String;

    /// Open the store the way a host would, provisioning it if it is new.
    async fn open(&self) -> Result<(), StoreError>;

    /// Read the deployment's schema status without opening it.
    async fn preflight(&self) -> Result<StoreSchemaStatus, StoreError>;

    /// Overwrite the stored release string, leaving the rest of the stamp
    /// alone. Panics rather than returning if the store carries no stamp yet.
    async fn force_release(&self, release: &str) -> Result<(), StoreError>;
}

/// A release no build will ever reach, standing in for "written by the future".
const NEWER_RELEASE: &str = "999.0.0";
/// A prerelease that sorts below both `0.0.0-dev` and every real release.
const OLDER_RELEASE: &str = "0.0.0-aaa";

fn stamp(status: &StoreSchemaStatus, context: &str) -> StoreReleaseStamp {
    match &status.release {
        StoreReleaseState::Stamped(stamp) => stamp.clone(),
        other => panic!("{context}: expected a release stamp, store reported {other}"),
    }
}

/// Certify the release-stamp update rule against one deployment.
///
/// The deployment must be fresh: the first assertion is that a store nothing has
/// opened reports the absence of a stamp rather than an empty release.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: every result is established by the deployment under test"
)]
pub async fn release_stamp_conformance(deployment: &dyn ReleaseStampDeployment) {
    let build_release = deployment.build_release();

    // An unopened store records no release. Absence, not a release of "" and
    // not a zero — a host that cannot tell those apart cannot decide whether
    // the schema integers beside them are trustworthy.
    let before = deployment
        .preflight()
        .await
        .expect("preflight reads an unopened deployment");
    assert_eq!(
        before.release,
        StoreReleaseState::Unstamped,
        "a store nothing has written names no release"
    );

    // First open stamps.
    deployment.open().await.expect("first open provisions");
    let first = stamp(
        &deployment
            .preflight()
            .await
            .expect("preflight reads the opened deployment"),
        "after the first open",
    );
    assert_eq!(
        first.release, build_release,
        "the first open records the release that wrote the store"
    );
    assert!(
        !first.schema_versions.is_empty(),
        "the stamp carries the schema versions that release required, not just its name"
    );
    assert!(
        first.written_at_epoch_ms > 0,
        "the stamp records when it was written: {first:?}"
    );

    // A reopen under the same release changes nothing, so the recorded instant
    // stays the moment this release took the store over rather than sliding
    // forward on every open.
    deployment.open().await.expect("reopen under this build");
    let reopened = stamp(
        &deployment
            .preflight()
            .await
            .expect("preflight after reopen"),
        "after reopening under the same release",
    );
    assert_eq!(
        reopened, first,
        "reopening under the same release leaves the stamp untouched"
    );

    // A newer release owns the store, and this build must not claw it back.
    deployment
        .force_release(NEWER_RELEASE)
        .await
        .expect("force a newer release");
    deployment
        .open()
        .await
        .expect("reopen under a store a newer release wrote");
    let after_newer = stamp(
        &deployment
            .preflight()
            .await
            .expect("preflight after the newer release"),
        "after a newer release wrote the store",
    );
    assert_eq!(
        after_newer.release, NEWER_RELEASE,
        "an older build never downgrades the stamp"
    );

    // An older stamp is taken over, because this build is the one that just
    // wrote the store.
    deployment
        .force_release(OLDER_RELEASE)
        .await
        .expect("force an older release");
    deployment
        .open()
        .await
        .expect("reopen over an older release's stamp");
    let after_older = stamp(
        &deployment
            .preflight()
            .await
            .expect("preflight after the older release"),
        "after an older release's stamp",
    );
    assert_eq!(
        after_older.release, build_release,
        "a newer release opening the store takes the stamp over"
    );
}
