//! TOOL-LIVE-BOUNDS: owner admission includes work held by the store.
use super::*;

use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

fn owner() -> ExecutionOwner {
    ExecutionOwner::SessionFrame {
        session_id: lash_vocabulary::identity::SessionId::from("live-owner"),
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
    ToolLivePolicy::new(
        16,
        2,
        16384,
        1024,
        512,
        8,
        Duration::from_millis(100),
        Duration::from_secs(5),
        Duration::from_secs(2),
    )
    .unwrap()
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
        records: Vec<ToolLiveRecord>,
    ) -> Result<(), ToolLiveError> {
        self.batches
            .lock_recover()
            .push((tokio::time::Instant::now(), records.len()));
        self.window.publish(owner, records).await
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
        records: Vec<ToolLiveRecord>,
    ) -> Result<(), ToolLiveError> {
        self.attempts.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        self.window.publish(owner, records).await
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
    assert!(
        pending.slots.len() * policy().progress_bytes()
            + pending.log_bytes
            + pending.in_flight_bytes
            <= policy().bytes()
    );
    assert!(pending.log_count <= policy().logs());
    assert!(pending.in_flight_records <= policy().events());
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

/// TOOL-LIVE-FINALS: every admitted call delivers its final gauge at the
/// default policy, even when more calls finish together than one interval
/// emits (figments feedback-14 a2).
#[tokio::test(start_paused = true)]
async fn every_admitted_parallel_call_delivers_its_final_gauge_at_the_default_policy() {
    let window = Arc::new(ToolLiveMemoryWindow::default());
    let lane = ToolLive::new(Some(window), None, ToolLivePolicy::default());
    let mut feed = lane
        .subscribe(ToolLiveFilter::Owner(owner()))
        .await
        .unwrap();
    let calls = (0..40).map(|id| lane.bind(address(id))).collect::<Vec<_>>();
    for call in &calls {
        call.publish(ToolLiveFact::Progress {
            progress: 1.0,
            total: Some(1.0),
            message: Some("completed".into()),
        });
        call.finish();
    }
    let mut finals = std::collections::HashSet::new();
    let mut omitted = Vec::new();
    while let Ok(Some(item)) = tokio::time::timeout(Duration::from_millis(500), feed.next()).await {
        if let ToolLiveItem::Emission { emission, .. } = item.unwrap() {
            match emission.payload {
                ToolLivePayload::Fact {
                    fact: ToolLiveFact::Progress { progress, .. },
                } => {
                    assert_eq!(progress, 1.0);
                    finals.insert(emission.address.call_id);
                }
                ToolLivePayload::Omitted { cause, count } => omitted.push((cause, count)),
                ToolLivePayload::Fact { .. } => {}
            }
        }
    }
    assert_eq!(finals.len(), calls.len(), "omissions: {omitted:?}");
    assert!(omitted.is_empty(), "nothing was lost: {omitted:?}");
}

/// TOOL-LIVE-ACCOUNTING: a call refused a slot is counted at the owner, never
/// under another call's address; publication after a sealed call's slot
/// retired still counts as `AfterCompletion`; non-finite gauges are counted
/// as `NonFinite` and never stored (figments feedback-14 a5, b2).
#[tokio::test(start_paused = true)]
async fn refusals_late_facts_and_non_finite_gauges_are_counted_where_they_belong() {
    let window = Arc::new(ToolLiveMemoryWindow::default());
    let narrow = ToolLivePolicy::new(
        16,
        2,
        16384,
        1024,
        512,
        2,
        Duration::from_millis(100),
        Duration::from_secs(5),
        Duration::from_secs(2),
    );
    let lane = ToolLive::new(Some(window), None, narrow.unwrap());
    let mut feed = lane
        .subscribe(ToolLiveFilter::Owner(owner()))
        .await
        .unwrap();
    let (early, live) = (lane.bind(address(0)), lane.bind(address(1)));
    let refused = lane.bind(address(2));
    refused.publish(progress(1));
    live.publish(ToolLiveFact::Progress {
        progress: f64::NAN,
        total: None,
        message: None,
    });
    live.publish(ToolLiveFact::Progress {
        progress: 1.0,
        total: Some(f64::INFINITY),
        message: None,
    });
    live.publish(progress(2));
    early.publish(progress(3));
    early.finish();
    assert_eq!(lane.diagnostics(&owner()).unwrap().refused_total, 1);
    let mut seen = Vec::new();
    let drain = async |feed: &mut ToolLiveSubscription, seen: &mut Vec<ToolLiveItem>| {
        for _ in 0..40 {
            tokio::time::advance(Duration::from_millis(100)).await;
            while let std::task::Poll::Ready(Some(item)) = futures_util::poll!(feed.next()) {
                seen.push(item.unwrap());
            }
        }
    };
    drain(&mut feed, &mut seen).await;
    // The early call's slot has retired; a retained handle publishes late.
    assert_eq!(lane.diagnostics(&owner()).unwrap().sealed_calls, 0);
    early.publish(progress(4));
    drain(&mut feed, &mut seen).await;
    let omitted = |call: usize, wanted: ToolLiveOmission| {
        seen.iter()
            .filter_map(|item| match item {
                ToolLiveItem::Emission {
                    emission:
                        ToolLiveEmission {
                            address,
                            payload: ToolLivePayload::Omitted { cause, count },
                            ..
                        },
                    ..
                } if *address == self::address(call) && *cause == wanted => Some(*count),
                _ => None,
            })
            .sum::<u64>()
    };
    assert_eq!(omitted(1, ToolLiveOmission::NonFinite), 2);
    assert_eq!(omitted(0, ToolLiveOmission::AfterCompletion), 1);
    for call in 0..3 {
        assert_eq!(omitted(call, ToolLiveOmission::RoutesFull), 0);
    }
    let owner_level = seen
        .iter()
        .filter_map(|item| match item {
            ToolLiveItem::OwnerOmitted { omission, .. } => Some((omission.cause, omission.count)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(owner_level, [(ToolLiveOmission::RoutesFull, 1)]);
    let gauges = seen
        .iter()
        .filter_map(|item| match item {
            ToolLiveItem::Emission {
                emission:
                    ToolLiveEmission {
                        address,
                        payload:
                            ToolLivePayload::Fact {
                                fact:
                                    ToolLiveFact::Progress {
                                        progress, total, ..
                                    },
                            },
                        ..
                    },
                ..
            } => Some((address.call_id.clone(), *progress, *total)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert!(
        gauges
            .iter()
            .all(|(_, progress, total)| progress.is_finite() && total.is_none_or(f64::is_finite))
    );
    assert!(gauges.contains(&(address(1).call_id, 2.0, Some(10000.0))));
    assert!(!gauges.iter().any(|(call, ..)| *call == address(2).call_id));
}

/// TOOL-LIVE-FENCE: publication and the completion fence are classified
/// under one owner lock. A fact measured before its call's fence and admitted
/// after it is counted `AfterCompletion` whether the sealed slot still
/// lingers or has already retired: never written after the final gauge, never
/// lost (FIG-6854).
#[tokio::test(start_paused = true)]
async fn a_publication_racing_the_completion_fence_is_refused_and_counted_never_lost() {
    let window = Arc::new(ToolLiveMemoryWindow::default());
    let lane = ToolLive::new(Some(window), None, policy());
    let mut feed = lane
        .subscribe(ToolLiveFilter::Owner(owner()))
        .await
        .unwrap();
    let log = |data: &str| ToolLiveFact::Log {
        level: ToolLiveLogLevel::Info,
        logger: None,
        data: serde_json::json!(data),
    };
    let mut seen = Vec::new();
    let drain = async |feed: &mut ToolLiveSubscription, seen: &mut Vec<ToolLiveEmission>| {
        for _ in 0..40 {
            tokio::time::advance(Duration::from_millis(100)).await;
            while let std::task::Poll::Ready(Some(item)) = futures_util::poll!(feed.next()) {
                if let ToolLiveItem::Emission { emission, .. } = item.unwrap() {
                    seen.push(emission);
                }
            }
        }
    };
    let (lingering, retired) = (lane.bind(address(0)), lane.bind(address(1)));
    lingering.publish(progress(1));
    retired.publish(progress(1));
    let (sealed_late, retired_late) = (
        lingering.measure(log("after the seal")),
        retired.measure(log("after retirement")),
    );
    lingering.finish();
    retired.finish();
    lingering.admit(sealed_late);
    drain(&mut feed, &mut seen).await;
    assert_eq!(lane.diagnostics(&owner()).unwrap().sealed_calls, 0);
    retired.admit(retired_late);
    drain(&mut feed, &mut seen).await;
    for call in 0..2 {
        let emissions = seen
            .iter()
            .filter(|emission| emission.address == address(call))
            .map(|emission| &emission.payload)
            .collect::<Vec<_>>();
        let late = emissions
            .iter()
            .filter_map(|payload| match payload {
                ToolLivePayload::Omitted {
                    cause: ToolLiveOmission::AfterCompletion,
                    count,
                } => Some(*count),
                _ => None,
            })
            .sum::<u64>();
        assert_eq!(late, 1, "call {call} counts its late fact: {emissions:?}");
        assert!(
            !emissions.iter().any(|payload| matches!(
                payload,
                ToolLivePayload::Fact {
                    fact: ToolLiveFact::Log { .. }
                }
            )),
            "call {call} writes nothing after its fence: {emissions:?}"
        );
        assert!(emissions.contains(&&ToolLivePayload::Fact { fact: progress(1) }));
    }
}

/// TOOL-LIVE-POLICY: a refused setting names its field, the supplied value
/// and the bound or relationship it broke (figments feedback-14 b4).
#[test]
fn a_refused_policy_names_its_field_value_and_bound() {
    let error = ToolLivePolicy::new(
        64,
        64,
        524288,
        65536,
        1024,
        128,
        Duration::from_millis(100),
        Duration::from_secs(5),
        Duration::from_secs(2),
    )
    .unwrap_err();
    assert_eq!(
        error,
        ToolLivePolicyError {
            field: ToolLivePolicyField::Logs,
            value: ToolLivePolicyValue::Count(64),
            bound: ToolLivePolicyBound::Relation {
                relation: ToolLivePolicyRelation::LogsBelowEvents,
                limit: ToolLivePolicyValue::Count(63),
            },
        }
    );
    let error = ToolLivePolicy::new(
        64,
        16,
        524288,
        65536,
        1024,
        128,
        Duration::from_millis(5),
        Duration::from_secs(5),
        Duration::from_secs(2),
    )
    .unwrap_err();
    assert_eq!(error.field, ToolLivePolicyField::Interval);
    assert_eq!(
        error.bound,
        ToolLivePolicyBound::Range {
            min: ToolLivePolicyValue::Duration(Duration::from_millis(10)),
            max: ToolLivePolicyValue::Duration(Duration::from_secs(60)),
        }
    );
    for (write_timeout, completed_linger, field, value, min, max) in [
        (
            Duration::ZERO,
            Duration::from_secs(2),
            ToolLivePolicyField::WriteTimeout,
            Duration::ZERO,
            Duration::from_millis(10),
            Duration::from_secs(3600),
        ),
        (
            Duration::from_secs(3601),
            Duration::from_secs(2),
            ToolLivePolicyField::WriteTimeout,
            Duration::from_secs(3601),
            Duration::from_millis(10),
            Duration::from_secs(3600),
        ),
        (
            Duration::from_secs(5),
            Duration::from_secs(3601),
            ToolLivePolicyField::CompletedLinger,
            Duration::from_secs(3601),
            Duration::ZERO,
            Duration::from_secs(3600),
        ),
    ] {
        assert_eq!(
            ToolLivePolicy::new(
                64,
                16,
                524288,
                65536,
                1024,
                128,
                Duration::from_millis(100),
                write_timeout,
                completed_linger
            )
            .unwrap_err(),
            ToolLivePolicyError {
                field,
                value: ToolLivePolicyValue::Duration(value),
                bound: ToolLivePolicyBound::Range {
                    min: ToolLivePolicyValue::Duration(min),
                    max: ToolLivePolicyValue::Duration(max)
                }
            }
        );
    }
    for (write_timeout, completed_linger) in [
        (Duration::from_millis(10), Duration::ZERO),
        (Duration::from_secs(3600), Duration::from_secs(3600)),
    ] {
        let policy = ToolLivePolicy::new(
            64,
            16,
            524288,
            65536,
            1024,
            128,
            Duration::from_millis(100),
            write_timeout,
            completed_linger,
        )
        .unwrap();
        assert_eq!(policy.snapshot().write_timeout, write_timeout);
        assert_eq!(policy.snapshot().completion_linger, completed_linger);
    }
    let snapshot = ToolLivePolicy::default().snapshot();
    assert_eq!(snapshot.write_timeout, Duration::from_secs(5));
    assert_eq!(snapshot.completion_linger, Duration::from_secs(2));
    assert_eq!(
        snapshot.reserve_bytes + snapshot.log_bytes + snapshot.write_bytes,
        snapshot.bytes
    );
    assert_eq!(
        snapshot.reserve_bytes,
        snapshot.calls * snapshot.progress_bytes
    );
}
