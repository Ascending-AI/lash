//! The two-binary rollback of a plugin's state format (FIG-4746), over one
//! SQLite store directory. The probe plugin reads and writes format 1 in N;
//! the synthetic N+1 reads format 2 natively and can still write format 1.
//!
//! Inside the rollback window the fleet record permits format 1 alone, so
//! N+1 publishes what N reads and its native format is refused with nothing
//! published. The actual N binary then reads N+1's namespace and keeps
//! writing. N+1's finalize moves `F` and the plugin's writer range together,
//! after which N+1 publishes format 2 and N no longer opens the store.

use anyhow::{Result, ensure};
use lash_core_store::compat::{CompatRefusal, VersionRange};
use lash_upgrade_harness::harness::NodeBuilds;
use lash_upgrade_harness::node::plugin_state::PLUGIN;

#[test]
#[ignore = "needs both node builds: `just phase-a plugin_writer_rollback` runs it"]
fn plugin_writer_rollback() -> Result<()> {
    let builds = NodeBuilds::from_env()?;
    let scratch = tempfile::tempdir()?;
    let stores = scratch.path().join("stores");
    let session = "plugin-writer-rollback";
    let window = Some(VersionRange::exactly(1));

    // N provisions the store and publishes the plugin's first format.
    let first = builds.n.plugin_state(&stores, session, "write", None)?;
    ensure!(
        first.value == Some(1) && first.stored_format == Some(1) && first.permitted == window,
        "N did not publish format 1: {first:?}"
    );
    ensure!(first.fleet == 1, "N's store is not at epoch 1: {first:?}");

    // The roll: N+1 reads N's namespace through its migrate and, inside the
    // window, publishes the format the fleet record permits.
    let rolled = builds.next.plugin_state(&stores, session, "write", None)?;
    ensure!(
        rolled.value == Some(2)
            && rolled.stored_format == Some(1)
            && rolled.permitted == window
            && rolled.fleet == 1,
        "N+1 did not publish the window's format: {rolled:?}"
    );

    // N+1's native format is refused, and nothing is published.
    let refused = builds
        .next
        .plugin_state(&stores, session, "write-native", None)?;
    ensure!(
        matches!(
            &refused.refusal,
            Some(CompatRefusal::PluginWriterOutsideRange {
                plugin,
                writer: 2,
                permitted,
                ..
            }) if plugin == PLUGIN && Some(*permitted) == window
        ),
        "N+1's native format was not refused inside the window: {refused:?}"
    );
    let unchanged = builds.next.plugin_state(&stores, session, "read", None)?;
    ensure!(
        unchanged.value == Some(2) && unchanged.stored_format == Some(1),
        "the refused publication changed the stored namespace: {unchanged:?}"
    );

    // The rollback: the actual N binary reads what N+1 wrote and keeps going.
    let read = builds.n.plugin_state(&stores, session, "read", None)?;
    ensure!(
        read.value == Some(2) && read.stored_format == Some(1) && read.unreadable.is_none(),
        "N did not read the namespace N+1 wrote: {read:?}"
    );
    let onward = builds.n.plugin_state(&stores, session, "write", None)?;
    ensure!(
        onward.value == Some(3) && onward.stored_format == Some(1) && onward.refusal.is_none(),
        "N did not keep writing after the rollback: {onward:?}"
    );

    // Roll forward and finalize: `F` and the plugin's range move together.
    let again = builds.next.plugin_state(&stores, session, "write", None)?;
    ensure!(
        again.value == Some(4) && again.stored_format == Some(1),
        "N+1 did not continue from N's write: {again:?}"
    );
    let finalized = builds
        .next
        .plugin_state(&stores, session, "finalize", Some("0123456789ab"))?;
    ensure!(
        finalized.fleet == 2 && finalized.permitted == Some(VersionRange::between(1, 2)),
        "finalize did not move F and the writer range together: {finalized:?}"
    );
    let native = builds.next.plugin_state(&stores, session, "write", None)?;
    ensure!(
        native.value == Some(5) && native.stored_format == Some(2) && native.refusal.is_none(),
        "N+1 did not publish its native format after finalize: {native:?}"
    );

    // N is fenced afterwards: it no longer opens the finalized store.
    let fenced = builds.n.plugin_state(&stores, session, "write", None);
    ensure!(
        fenced
            .as_ref()
            .is_err_and(|error| format!("{error:#}").contains("fleet epoch 2")),
        "N still wrote to the finalized store: {fenced:?}"
    );
    let kept = builds.next.plugin_state(&stores, session, "read", None)?;
    ensure!(
        kept.value == Some(5) && kept.stored_format == Some(2),
        "N changed the finalized store: {kept:?}"
    );
    Ok(())
}
