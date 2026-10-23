//! LO-HISTORY measures the actual acknowledged projection, with an allocator
//! confined to this executable. Both histories finish in the same ten-turn window.
#![allow(clippy::expect_used, clippy::unwrap_used)]
#[path = "support/served.rs"]
mod served;
#[path = "support/sim.rs"]
mod sim;

use lash_core::runtime::durable::session::SessionActivation;
use lash_durable::{ActorKey, OwnerRead};
use lash_durable_test::{Matrix, Script, SimClock, SimNodes, SimNodesConfig, Tripwire};
use lash_protocol_types::sync::MutexExt as _;
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct DropCounter;
thread_local! {
    static RELEASED: Cell<Option<usize>> = const { Cell::new(None) };
}
#[global_allocator]
static ALLOCATOR: DropCounter = DropCounter;

#[expect(
    unsafe_code,
    reason = "test-only counting adapter delegates the allocation contract unchanged to System"
)]
unsafe impl GlobalAlloc for DropCounter {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: exactly the caller's allocation request.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let _ = RELEASED.try_with(|released| {
            if let Some(bytes) = released.get() {
                released.set(Some(bytes + layout.size()));
            }
        });
        // SAFETY: pointer and layout are passed unchanged to their allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
}
const WINDOW_TURNS: usize = 10;
const INPUT_BYTES: usize = 4096;

async fn retained_bytes(turns: usize) -> usize {
    let clock = SimClock::new();
    let stores = Arc::new(sim::memory(clock.clone()).await);
    let mut settings = sim::settings();
    settings.owner_retention = lash_durable::OwnerRetentionSettings {
        idle: Duration::from_secs(3600),
        max_resident_owners: std::num::NonZeroUsize::new(1).unwrap(),
    };
    let backend = served::configured_backend(stores, settings, Vec::new());
    let model = lash_core::testing::TestProvider::builder()
        .kind("bounded-history")
        .requires_streaming(true)
        .complete(|request: lash_core::llm::types::LlmRequest| async move {
            Ok(served::text(&request, "done"))
        })
        .build()
        .into_handle();
    let core = lash::LashCore::standard_builder(backend.clone())
        .serve_sessions(false)
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .execution_budgets(lash::ExecutionBudgets::recommended())
        .delta_coalescing(lash::DeltaCoalescing::recommended())
        .data_retention(lash::DataRetention::standard())
        .serve_test_llm_profile(model, served::metadata())
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            lash::persistence::LeaseOwnerId::new("lo-history"),
            lash::persistence::LeaseIncarnationId::new("boot"),
        ))
        .unwrap();
    let id = lash::SessionId::from("lo-history");
    let session = core
        .session(id.clone())
        .create(lash::SessionCreation::root(served::spec(8)))
        .await
        .unwrap();
    let live = core.session(id.clone()).open().await.unwrap();
    let context = Arc::new(Mutex::new(None));
    let observed = context.clone();
    let probe = Arc::new(Tripwire::default());
    let activation = SessionActivation::new(
        backend.clone(),
        lash::testing::session_turn_services(&core),
        probe.clone(),
    )
    .observe_context_for_testing(move |cx| {
        *observed.lock_recover() = Some(cx.clone());
    });
    let nodes = SimNodes::new(
        backend.durable().clone(),
        clock,
        Script::new(),
        SimNodesConfig {
            lease: Matrix::test_lease(),
            decodes: backend.formats().decodes(),
            serves: lash::testing::node_serves(&core),
            max_active: 1,
        },
        Arc::new(activation),
    )
    .with_retention(backend.owner_retention());
    nodes.start("owner");
    let _wakes = backend
        .stores()
        .wake_sink()
        .attach(&nodes.hints("owner").unwrap());
    nodes.quiesce().await;
    for ordinal in 0..turns {
        if ordinal > 0 && ordinal % WINDOW_TURNS == 0 {
            let material = format!("frame-{ordinal:04}");
            let command = lash_core::facade_support::SessionCommand::OpenAgentFrame {
                request: Box::new(lash::OpenAgentFrameRequest::new(
                    lash::FrameKey::from_caller_material(&material).unwrap(),
                    lash_core::AgentFrameReason::new("bounded history"),
                )),
            };
            let accepted = live
                .admin()
                .commands()
                .submit(command, &material)
                .await
                .unwrap();
            nodes.quiesce().await;
            assert!(
                backend
                    .stores()
                    .session_store_factory()
                    .queued_work_batch_completion(&id, accepted.batch_id.as_str())
                    .await
                    .unwrap()
                    .is_some(),
                "frame cut acknowledged"
            );
        }
        let accepted = session
            .send(lash::TurnInput::text("x".repeat(INPUT_BYTES)))
            .id(lash::TurnId::try_from(format!("turn-{ordinal:04}")).unwrap())
            .await
            .unwrap();
        nodes.quiesce().await;
        let output = tokio::time::timeout(served::WATCHDOG, accepted.output())
            .await
            .unwrap()
            .unwrap();
        served::assert_answered("bounded history", &output);
        nodes.quiesce().await;
    }
    let cx = context.lock_recover().take().unwrap();
    let projection = cx.session_projection().await.unwrap().unwrap();
    let actor = ActorKey::session(id.as_str()).unwrap();
    assert_eq!(
        probe
            .counts()
            .owner_reads
            .iter()
            .filter(|((key, _, source), _)| key == &actor
                && *source == OwnerRead::CompletedRunRowsReplayed)
            .map(|(_, count)| *count)
            .sum::<u64>(),
        0,
        "completed run rows are never replayed"
    );
    // The clone is the actual acknowledged projection. Stop the activation so
    // its head handle cannot share the final references whose drop we measure.
    nodes.kill("owner");
    nodes.quiesce().await;
    drop(cx);
    drop(live);
    drop(session);
    core.shutdown().await.unwrap();
    RELEASED.with(|released| released.set(Some(0)));
    drop(projection);
    let bytes = RELEASED.with(|released| released.take().unwrap());
    assert!(
        bytes >= WINDOW_TURNS * INPUT_BYTES,
        "the actual window payload was measured"
    );
    eprintln!("LO-HISTORY completed_turns={turns} released_projection_bytes={bytes}");
    bytes
}

#[tokio::test]
async fn lo_history_on_sqlite_memory() {
    let short = retained_bytes(10).await;
    let long = retained_bytes(1000).await;
    // Allow four copies of the bounded current window for independent graph,
    // message, rendered, and lowered representations. No term uses history age.
    let bound = 4 * WINDOW_TURNS * INPUT_BYTES;
    assert!(
        long.abs_diff(short) <= bound,
        "whole history retained: short={short}, long={long}, window_bound={bound}"
    );
}
