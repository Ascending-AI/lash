//! One worker slot, shared by a turn's cells and its process engine as a
//! production RLM registration shares it (FIG-4275, FIG-4707; ported by
//! FIG-5308 from the deleted lash-protocol-rlm `one_slot_process_await.rs`
//! cell laws).
//!
//! A cell that starts a process and awaits it parks while the process runs,
//! so the one slot runs both.

#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use std::sync::Arc;

use served::{Tier, World};

/// A worker service of exactly one slot, with a checkout deadline of
/// `checkout`: long enough for any legitimate wait, short enough to report a
/// deadlock or a saturated slot.
fn one_slot_workers(checkout: std::time::Duration) -> lash::vm::WorkerService {
    let mut config = sim::untimed_workers().config().clone();
    config.min_workers = 1;
    config.max_workers = 1;
    config.deadlines.checkout = checkout;
    lash::vm::WorkerService::new(config)
}

/// A core running RLM turns whose cells and process engine share `workers`,
/// with the session process controls and `tools`.
async fn world(
    tier: Tier,
    workers: lash::vm::WorkerService,
    tools: Option<Arc<dyn lash_core::ToolProvider>>,
) -> Option<World> {
    World::new(tier, move |backend| {
        let builder =
            lash::LashCore::rlm_builder(backend.clone(), served::rlm(backend, None, workers))
                .plugin(Arc::new(
                    lash::process_controls::SessionProcessAdminPluginFactory::new(
                        lash_core::lifetime::starter,
                    ),
                ));
        match tools {
            Some(tools) => builder.tools(tools),
            None => builder,
        }
    })
    .await
}

/// A cell that writes a process, starts it and awaits it completes on one
/// slot: the cell parks while the started body runs, so the body takes the
/// slot the cell gave back and the cell resumes with its terminal.
async fn one_slot_cell_that_starts_and_awaits_a_process_completes(tier: Tier) {
    let workers = one_slot_workers(std::time::Duration::from_secs(10));
    let Some(world) = world(tier, workers.clone(), None).await else {
        return;
    };
    let output = world
        .run(
            "one-slot-await",
            served::spec(64),
            vec![served::cell(
                "const worker = async () => { return \"done\"; };\n\
                 const handle = await processes.start({ definition: worker });\n\
                 await control.finish(await processes.await({ handle }));",
            )],
        )
        .await;
    served::assert_answered("the cell that awaits its process on one slot", &output);
    assert_eq!(
        output.finished().map(|(_, value)| value),
        Some(&serde_json::json!("done")),
        "the cell resumes with the awaited body's terminal"
    );
    assert_eq!(
        workers
            .pool()
            .expect("the shared pool")
            .config()
            .max_workers,
        1,
        "one slot ran the cell and the process body"
    );
    world.shutdown().await;
}

tiered_laws!(one_slot_cell_that_starts_and_awaits_a_process_completes,);
