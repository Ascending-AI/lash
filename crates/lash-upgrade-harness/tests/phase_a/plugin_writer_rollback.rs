//! L10/L11 use the actual N and synthetic N+1 binaries. Each write sends a
//! turn through the real endpoint on the in-process Restate server double.
//! N's callback publishes `{count:7}`, N+1 publishes v1 `{count:8}` inside
//! the rollback window, and N reopens those bytes and commits `{count:9}`.

use std::path::Path;

use anyhow::{Context, Result, ensure};
use lash_core_store::compat::{CompatRefusal, VersionRange};
use lash_upgrade_harness::harness::{NodeBuilds, block_on};
use lash_upgrade_harness::node::plugin_state::{PLUGIN, PluginStateReport};

fn rollback(stores: &Path, database_url: Option<&str>) -> Result<()> {
    let builds = NodeBuilds::from_env()?;
    let session = "plugin-writer-rollback";
    let step = |node: &lash_upgrade_harness::harness::NodeBinary, action: &str| {
        node.plugin_state(stores, session, action, None, database_url, None)
    };
    let window = Some(VersionRange::exactly(1));
    let first = step(&builds.n, "write")?;
    ensure!(
        first.value == Some(7)
            && first.stored_format == Some(1)
            && first.permitted == window
            && first.callbacks == 1,
        "N's admitted callback did not publish v1 count 7: {first:?}"
    );
    ensure!(first.fleet == 1);
    ensure!(
        first
            .config
            .as_ref()
            .is_some_and(|config| config.get(PLUGIN).is_some())
            && first.model_route.is_some(),
        "the first callback recorded no config/route"
    );

    let rolled = step(&builds.next, "write")?;
    ensure!(
        rolled.value == Some(8)
            && rolled.stored_format == Some(1)
            && rolled.permitted == window
            && rolled.callbacks == 1
            && rolled.fleet == 1,
        "N+1's admitted callback did not publish v1 count 8: {rolled:?}"
    );
    ensure!(
        rolled.config == first.config && rolled.model_route == first.model_route,
        "config/route changed across the roll"
    );
    ensure!(
        rolled.generation != first.generation,
        "N+1 did not use its own generation lane"
    );

    let refused = step(&builds.next, "write-native")?;
    ensure!(
        matches!(&refused.refusal, Some(CompatRefusal::PluginWriterOutsideRange { plugin, writer: 2, permitted, .. })
        if plugin == PLUGIN && Some(*permitted) == window),
        "illegal native publication was admitted: {refused:?}"
    );
    let unchanged = step(&builds.next, "read")?;
    ensure!(
        unchanged.value == Some(8)
            && unchanged.head == rolled.head
            && unchanged.namespace_bytes == rolled.namespace_bytes,
        "the refused candidate changed the head or namespace bytes"
    );

    let read = step(&builds.n, "read")?;
    ensure!(
        read.value == Some(8) && read.stored_format == Some(1) && read.unreadable.is_none(),
        "actual N cannot read N+1's published count 8: {read:?}"
    );
    let onward = step(&builds.n, "write")?;
    ensure!(
        onward.value == Some(9) && onward.stored_format == Some(1) && onward.callbacks == 1,
        "actual N did not commit 9: {onward:?}"
    );
    ensure!(
        onward.config == first.config
            && onward.model_route == first.model_route
            && onward.generation == first.generation,
        "rollback changed config/route or substituted the executable lane"
    );

    let finalized = builds.next.plugin_state(
        stores,
        session,
        "finalize",
        Some("0123456789ab"),
        database_url,
        None,
    )?;
    ensure!(
        finalized.fleet == 2 && finalized.permitted == Some(VersionRange::between(1, 2)),
        "finalize did not move the epoch/map together: {finalized:?}"
    );
    let native = step(&builds.next, "write")?;
    ensure!(
        native.value == Some(10) && native.stored_format == Some(2) && native.callbacks == 1,
        "new admission did not publish v2: {native:?}"
    );
    ensure!(native.model_route == first.model_route);
    ensure!(
        native.config.as_ref().and_then(|config| config.get(PLUGIN))
            == first.config.as_ref().and_then(|config| config.get(PLUGIN))
    );

    let historical: PluginStateReport = builds.next.plugin_state(
        stores,
        session,
        "read",
        None,
        database_url,
        first.head.as_ref(),
    )?;
    ensure!(
        historical.value == Some(7)
            && historical.stored_format == Some(1)
            && historical.namespace_bytes == first.namespace_bytes,
        "retained v1 history was rewritten or is unreadable: {historical:?}"
    );
    let fenced = step(&builds.n, "write")
        .err()
        .context("N cannot open the finalized store")?;
    let expected_refusal = if database_url.is_some() {
        // PostgreSQL finalize contracts the synthetic catalog as well;
        // N's shape admission runs before its fleet-epoch admission.
        "added CHECK constraint lash_session_head.ck_lash_session_head_synthetic_next_note"
    } else {
        "store records fleet epoch 2, outside this build's writable range"
    };
    ensure!(
        format!("{fenced:#}").contains(expected_refusal),
        "N failed for another cause: {fenced:#}"
    );
    let kept = step(&builds.next, "read")?;
    ensure!(
        kept.head == native.head && kept.namespace_bytes == native.namespace_bytes,
        "fenced N changed the finalized head"
    );
    Ok(())
}

#[test]
#[ignore = "requires actual N and synthetic N+1 node binaries"]
fn plugin_writer_rollback() -> Result<()> {
    let directory = tempfile::tempdir()?;
    rollback(&directory.path().join("stores"), None)
}

#[test]
#[ignore = "requires the node binaries and PostgreSQL inside a private pg16 gate"]
fn plugin_writer_rollback_postgres_overlap() -> Result<()> {
    let directory = tempfile::tempdir()?;
    block_on(async {
        let database = lash_postgres_store::testing::IsolatedDatabase::create(
            &lash_postgres_store::testing::required_database_url(),
        )
        .await;
        // Expand using the successor before both builds overlap.
        let operator = std::process::Command::new(std::env::var(
            lash_upgrade_harness::harness::LASHCTL_NEXT_ENV,
        )?)
        .arg("migrate")
        .env("LASH_POSTGRES_DATABASE_URL", database.url())
        .output()?;
        ensure!(
            operator.status.success(),
            "successor expand failed: {}",
            String::from_utf8_lossy(&operator.stderr)
        );
        rollback(directory.path(), Some(database.url()))
    })
}
