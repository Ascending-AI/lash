//! FIG-3712: the context source a core installs on its backend's tool-child
//! host, which rebuilds the context of a group tool child whose opener is not
//! live where it runs.

use super::*;

const SEED: u64 = 0x5c_f107;

fn builder(backend: lash_core::Backend) -> crate::core::LashCoreBuilder {
    LashCore::standard_builder(backend, crate::TurnBudget::Unbounded)
        .commit_budget(crate::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(crate::QueuedWorkBatchingConfig::new(1))
        .provider(mock_provider())
        .model(mock_model_spec())
        .without_queued_work()
}

/// What the backend's tool-child host holds now, asked by installing this
/// core's own source again, which changes nothing.
fn installed(core: &LashCore) -> lash_core::facade_support::ContextSourceInstall {
    core.env
        .core
        .control
        .tool_children
        .as_ref()
        .expect("the backend routes tool children")
        .install_context_source(&core.tool_child_context_source)
}

/// One backend rebuilds its tool children under one live core's wiring. A
/// second core on the same backend still builds, but while both are live
/// the backend's host is ambiguous: a child with no live opener is refused
/// there rather than rebuilt under either core's plugins and provider (see
/// the store-backed `two_live_sources_leave_the_host_ambiguous` law). A
/// session holds its core's source, so a host that keeps its sessions and
/// drops the core keeps that source live, and the backend has one sole
/// source again only once every holder of the other is gone.
#[tokio::test]
async fn a_backend_rebuilds_tool_children_under_one_live_core() -> Result<()> {
    use lash_core::facade_support::ContextSourceInstall;
    let double = restate_double(SEED).await;
    let backend = double.lash_backend();
    let first = builder(backend.clone()).build(crate::testing::runtime_lease_owner())?;
    assert_eq!(installed(&first), ContextSourceInstall::Sole);
    let session = first.session("holds-the-source").open().await?;
    assert!(session.binding.holds_tool_child_context_source());

    let second = builder(backend.clone()).build(crate::testing::runtime_lease_owner())?;
    assert_eq!(
        installed(&second),
        ContextSourceInstall::Ambiguous { live: 2 },
        "a second live core leaves the backend ambiguous, never a build failure"
    );
    drop(second);

    drop(first);
    let third = builder(backend.clone()).build(crate::testing::runtime_lease_owner())?;
    assert_eq!(
        installed(&third),
        ContextSourceInstall::Ambiguous { live: 2 },
        "the open session keeps its dropped core's source live"
    );
    drop(third);

    drop(session);
    let fourth = builder(backend.clone()).build(crate::testing::runtime_lease_owner())?;
    assert_eq!(installed(&fourth), ContextSourceInstall::Sole);
    Ok(())
}
