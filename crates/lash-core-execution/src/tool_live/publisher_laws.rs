//! TOOL-LIVE-BOUNDS: owner admission includes work held by the store.
use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

fn owner() -> ExecutionOwner {
    ExecutionOwner::SessionFrame {
        session_id: crate::SessionId::from("live-owner"),
        agent_frame_id: crate::FrameNodeId::new("live-frame").unwrap(),
    }
}
fn address(id: usize) -> CallAddress {
    CallAddress {
        owner: owner(),
        call_id: ToolCallId::fixture(&format!("call-{id}")),
        attempt: 1,
    }
}
fn progress(value: usize) -> ToolLiveFact {
    ToolLiveFact::Progress {
        progress: value as f64,
        total: Some(10000.0),
        message: Some("working".into()),
    }
}
fn policy() -> ToolLivePolicy {
    ToolLivePolicy::new(16, 2, 16384, 1024, 8, Duration::from_millis(100)).unwrap()
}

struct Witness {
    window: ToolLiveMemoryWindow,
    batches: Mutex<Vec<(tokio::time::Instant, usize)>>,
}
#[async_trait::async_trait]
impl ToolLiveReplay for Witness {
    async fn publish(
        &self,
        owner: &ExecutionOwner,
        emissions: Vec<ToolLiveEmission>,
    ) -> Result<(), ToolLiveError> {
        self.batches
            .lock_recover()
            .push((tokio::time::Instant::now(), emissions.len()));
        self.window.publish(owner, emissions).await
    }
    async fn subscribe(
        &self,
        owner: &ExecutionOwner,
        after: Option<ToolLiveCursor>,
    ) -> Result<ToolLiveSubscription, ToolLiveError> {
        self.window.subscribe(owner, after).await
    }
}

#[tokio::test(start_paused = true)]
async fn ten_thousand_progress_updates_parallel_calls_and_logs_obey_the_owner_budget() {
    let transport = Arc::new(Witness {
        window: ToolLiveMemoryWindow::default(),
        batches: Mutex::default(),
    });
    let lane = ToolLive::new(Some(transport.clone()), None, policy());
    let mut feed = lane
        .subscribe(ToolLiveFilter::Owner(owner()))
        .await
        .unwrap();
    let calls = (0..4).map(|id| lane.bind(address(id))).collect::<Vec<_>>();
    for value in 1..=10000 {
        calls[0].publish(progress(value));
    }
    for call in &calls[1..] {
        call.publish(progress(10000));
    }
    for _ in 0..10000 {
        calls[0].publish(ToolLiveFact::Log {
            level: ToolLiveLogLevel::Info,
            logger: None,
            data: serde_json::json!("storm"),
        });
    }
    for call in &calls {
        call.finish();
    }
    calls[0].publish(progress(10001)); // sealed at the completion fence
    let mut final_calls = Vec::new();
    let mut omissions = 0;
    let mut logs = 0;
    let mut identities = Vec::new();
    for _ in 0..32 {
        tokio::time::advance(Duration::from_millis(100)).await;
        while let std::task::Poll::Ready(Some(item)) = futures_util::poll!(feed.next()) {
            let ToolLiveItem::Emission { emission, .. } = item.unwrap() else {
                continue;
            };
            assert!(!identities.contains(&emission.id));
            identities.push(emission.id);
            match emission.payload {
                ToolLivePayload::Fact {
                    fact: ToolLiveFact::Progress { progress, .. },
                } => {
                    assert_eq!(progress, 10000.0);
                    final_calls.push(emission.address.call_id);
                }
                ToolLivePayload::Fact {
                    fact: ToolLiveFact::Log { .. },
                } => logs += 1,
                ToolLivePayload::Omitted { count, .. } => omissions += count,
            }
        }
        if final_calls.len() == 4 && omissions == 19998 {
            break;
        }
    }
    assert_eq!(final_calls.len(), 4, "every final admitted gauge arrives");
    assert_eq!(logs, 2, "owner log budget is independent of subscribers");
    assert_eq!(
        omissions, 19998,
        "9999 coalesced + 9998 omitted logs + one late notification"
    );
    let batches = transport.batches.lock_recover();
    assert!(batches.iter().all(|(_, count)| *count <= policy().events));
    assert!(
        batches
            .windows(2)
            .all(|pair| pair[1].0.duration_since(pair[0].0) >= policy().interval)
    );
}

struct Stalled {
    entered: Notify,
    release: Notify,
    attempts: AtomicUsize,
    window: ToolLiveMemoryWindow,
}
#[async_trait::async_trait]
impl ToolLiveReplay for Stalled {
    async fn publish(
        &self,
        owner: &ExecutionOwner,
        emissions: Vec<ToolLiveEmission>,
    ) -> Result<(), ToolLiveError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        self.window.publish(owner, emissions).await
    }
    async fn subscribe(
        &self,
        owner: &ExecutionOwner,
        after: Option<ToolLiveCursor>,
    ) -> Result<ToolLiveSubscription, ToolLiveError> {
        self.window.subscribe(owner, after).await
    }
}
#[tokio::test(start_paused = true)]
async fn a_stalled_store_holds_one_pending_gauge_per_call_and_never_blocks_completion() {
    let store = Arc::new(Stalled {
        entered: Notify::new(),
        release: Notify::new(),
        attempts: AtomicUsize::new(0),
        window: ToolLiveMemoryWindow::default(),
    });
    let lane = ToolLive::new(Some(store.clone()), None, policy());
    let call = lane.bind(address(0));
    call.publish(progress(1));
    store.entered.notified().await;
    for value in 2..=10000 {
        call.publish(progress(value));
    }
    for _ in 0..10000 {
        call.publish(ToolLiveFact::Log {
            level: ToolLiveLogLevel::Info,
            logger: None,
            data: serde_json::json!("storm"),
        });
    }
    call.finish();
    // The completion fence returns synchronously while the only write stalls.
    let held: &Arc<Owner> = &call.owner;
    let pending = held.pending.lock_recover();
    assert!(pending.bytes + pending.in_flight_bytes <= policy().bytes);
    assert!(pending.events + pending.in_flight_events <= policy().events);
    assert_eq!(pending.slots.len(), 1);
    assert!(matches!(
        pending.slots[0].progress,
        Some((
            ToolLiveFact::Progress {
                progress: 10000.0,
                ..
            },
            _
        ))
    ));
    assert_eq!(store.attempts.load(Ordering::SeqCst), 1);
    drop(pending);
    store.release.notify_one();
}
