//! One law for the fleet-format row both SQL backends carry.
//!
//! The row is the durable fact of ADR 0106 §1 `F`: the durable-format
//! generation every writer in the fleet emits. What is certified here is the
//! storage contract FIG-3796 establishes — the row exists after open, it reads
//! back through the store API, and the store a writer holds reports the same
//! value — not the finalize operation (FIG-3800) that will one day move it.
//!
//! The law never asserts a *particular* format integer beyond
//! [`lash_core::FLEET_FORMAT_VERSION`]: the constant the whole workspace
//! shares, so the value the deployment records is exactly the value this build
//! claims to write.

use async_trait::async_trait;

use lash_core::{FleetFormat, FleetFormatState, StoreError, StoreSchemaStatus};

/// One SQL deployment a backend can open and preflight, for the fleet-format
/// law.
///
/// `open` returns the fleet format the *opened store* reports, so the law sees
/// both faces of the contract: the durable row as preflight reads it, and the
/// value a durable writer consults through the handle it holds.
#[async_trait]
pub trait FleetFormatDeployment: Send + Sync {
    async fn open(&self) -> Result<FleetFormat, StoreError>;

    /// Open the deployment admitting `writable` as the opening build's
    /// fleet-format writable range — the seam the rollout arms use to stand
    /// in for a build whose `[min_F, max_F]` differs from this binary's.
    async fn open_admitting(
        &self,
        writable: std::ops::RangeInclusive<u32>,
    ) -> Result<FleetFormat, StoreError>;

    /// Record `version` in the fleet-format row as an operator or a newer
    /// build's `finalize-upgrade` would (FIG-3800). The law uses it to stand
    /// up the rollout states the upgrade arc must survive.
    async fn record_fleet_format(&self, version: u32) -> Result<(), StoreError>;

    async fn preflight(&self) -> Result<StoreSchemaStatus, StoreError>;
}

/// Certify the fleet-format row against one deployment.
///
/// The deployment must be fresh: the first assertion is that a store nothing
/// has opened records no fleet format — absence, not a zero and not a read
/// failure.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: every result is established by the deployment under test"
)]
pub async fn fleet_format_conformance(deployment: &dyn FleetFormatDeployment) {
    // An unopened store records no fleet format. Absence must read as absence:
    // a host that cannot tell "no row" from "a row" cannot stage a rollout.
    let before = deployment
        .preflight()
        .await
        .expect("preflight reads an unopened deployment");
    assert_eq!(
        before.fleet_format,
        FleetFormatState::Unrecorded,
        "a store nothing has opened records no fleet format"
    );

    // The first open writes the row — provisioned on PostgreSQL, finalized on
    // open on SQLite — and the handle reports the value it recorded.
    let opened = deployment.open().await.expect("first open provisions");
    assert_eq!(
        opened,
        FleetFormat::current(),
        "the opened store reports this build's fleet format"
    );
    let recorded = deployment
        .preflight()
        .await
        .expect("preflight reads the opened deployment")
        .fleet_format;
    assert_eq!(
        recorded,
        FleetFormatState::Recorded(FleetFormat::current()),
        "the fleet-format row exists after open and reads back"
    );

    // A reopen leaves the row alone and reads back the same generation: the
    // store is the record, not the opener.
    let reopened = deployment.open().await.expect("reopen under this build");
    assert_eq!(
        reopened, opened,
        "a reopen reports the recorded fleet format, not a new one"
    );
    let after_reopen = deployment
        .preflight()
        .await
        .expect("preflight after reopen")
        .fleet_format;
    assert_eq!(
        after_reopen, recorded,
        "reopening under the same build leaves the fleet-format row untouched"
    );

    // A build whose writable range does not contain the recorded generation
    // refuses the open with the typed routing error (ADR 0106 §7): a worker
    // that opened anyway would emit a format the fleet retired.
    let next_generation = lash_core::FLEET_FORMAT_VERSION + 1;
    deployment
        .record_fleet_format(next_generation)
        .await
        .expect("record a next-generation fleet format");
    let refused = deployment
        .open_admitting(lash_core::FLEET_FORMAT_VERSION..=lash_core::FLEET_FORMAT_VERSION)
        .await;
    let error = refused.expect_err("a build whose range excludes the row refuses the open");
    assert!(
        matches!(
            error,
            StoreError::FleetFormatOutsideWritableRange { recorded, current }
                if recorded == next_generation && current == lash_core::FLEET_FORMAT_VERSION
        ),
        "an out-of-range fleet format must surface the typed refusal: {error}"
    );

    // The build that can still write it preserves the row: the reopen reads
    // the recorded generation rather than winding `F` back to its own.
    let admitted = deployment
        .open_admitting(lash_core::FLEET_FORMAT_VERSION..=next_generation)
        .await
        .expect("a build whose range admits the recorded generation opens");
    assert_eq!(
        admitted,
        FleetFormat::from_version(next_generation),
        "the opened store reports the recorded fleet format"
    );
    let still_recorded = deployment
        .preflight()
        .await
        .expect("preflight reads the preserved row")
        .fleet_format;
    assert_eq!(
        still_recorded,
        FleetFormatState::Recorded(FleetFormat::from_version(next_generation)),
        "reopening under a different build leaves the fleet-format row untouched"
    );
}
