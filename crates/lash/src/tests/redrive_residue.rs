//! A resident runtime replays its root exactly after the engine dropped an
//! attempt of it (FIG-3982).
//!
//! Restate stops polling a handler that suspends, so an attempt ends where it
//! awaited and nothing after that await runs. A code cell stopped by the
//! turn's cancel returns, then asks the cancel gate why through a journaled
//! peek; a handler that suspends at that peek never settles the cell. The
//! next attempt replays the root from its start on the same resident
//! runtime, and must start the cell again exactly as its first execution
//! did: a cell refused over the unsettled one skips the timer its first
//! execution journaled, and the journal mismatch pauses the root.
//!
//! The law runs on the Restate double in its always-replay mode, where every
//! await the journal cannot answer suspends the handler, as a server with a
//! zero inactivity timeout does.

use super::*;
use lash_restate_test::protocol::MessageType;

const SEED: u64 = 0x3982_0001;
const SESSION: &str = "redrive-residue";
const ROOT: &str = "redrive-residue-root";

/// Whether the root's `LashTurn` run has journaled its cell's timer.
fn root_is_sleeping(double: &lash_restate_test::RestateTestBackend) -> bool {
    let server = double.server();
    server
        .invocations()
        .into_iter()
        .filter(|view| view.target.starts_with("LashTurn"))
        .any(|view| {
            server
                .journal(&view.id)
                .unwrap_or_default()
                .iter()
                .any(|entry| entry.ty == MessageType::SleepCommand)
        })
}

/// A cell cancelled mid-sleep on a resident runtime, its handler suspending
/// at every await: every redrive replays the cell's timer where the first
/// execution journaled it, and the root commits its cancellation with no
/// attempt failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_cancelled_cell_replays_its_timer_on_a_resident_runtime() -> Result<()> {
    let double = lash_restate_test::backend(
        SEED,
        lash_restate_test::ServerConfig::default().always_replay(true),
    )
    .await
    .expect("build the always-replay Restate double");
    let calls = Arc::new(AtomicUsize::new(0));
    let core = explicit_ephemeral_facets(rlm_core_builder_over(double.lash_backend()))
        .provider({
            let calls = Arc::clone(&calls);
            crate::testing::TestProvider::builder()
                .kind("redrive-residue")
                .complete(move |_| {
                    calls.fetch_add(1, Ordering::SeqCst);
                    async {
                        Ok(text_response(&typescript_block(
                            "await sleep(60000);\nfinish(\"unreachable\");",
                        )))
                    }
                })
                .build()
                .into_handle()
        })
        .model(mock_model_spec())
        .build(crate::testing::runtime_lease_owner())?;
    let session = core.session(SESSION).open().await?;
    let handle = session
        .send(TurnInput::text("sleep until cancelled"))
        .id(ROOT)
        .await?;
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        while !root_is_sleeping(&double) {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the root's cell journals its timer");

    let receipt = handle.cancel().origin("redrive-residue-law").await?;
    assert!(
        matches!(&receipt, crate::CancelReceipt::Requested { root, .. } if root.as_str() == ROOT),
        "the cancel reaches the sleeping root: {receipt:?}"
    );
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(20), handle.outcome())
        .await
        .expect("the redriven root commits its cancellation")?;
    assert_eq!(outcome.status, crate::TurnStatus::Cancelled);
    assert_eq!(
        calls.load(Ordering::SeqCst),
        1,
        "every redrive replays the journaled model call"
    );
    let failed = double
        .server()
        .invocations()
        .into_iter()
        .filter(|view| view.target.starts_with("LashTurn") && view.last_failure.is_some())
        .map(|view| (view.target, view.last_failure))
        .collect::<Vec<_>>();
    assert!(
        failed.is_empty(),
        "no attempt of the root failed: {failed:?}"
    );
    Ok(())
}
