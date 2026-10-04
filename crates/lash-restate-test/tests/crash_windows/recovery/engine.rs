use super::*;
use lash_restate::*;

fn live(engine: &Engine) -> &LiveRestateBackend {
    let Engine::Live { backend, .. } = engine else {
        panic!("live matrix")
    };
    backend
}

async fn object<T: serde::Serialize, R: serde::de::DeserializeOwned>(
    engine: &Engine,
    service: &str,
    key: &str,
    handler: &str,
    request: T,
) -> R {
    let reply: Reply<R> = tokio::time::timeout(
        BOUND,
        live(engine)
            .ingress()
            .call_object_json(service, key, handler, &Call::new(request)),
    )
    .await
    .expect("object call finishes")
    .expect("object answers");
    reply.body
}

/// The dispatcher lane of the engine's build: the route its groups open on
/// and its dispatcher runs under.
fn dispatch_lane(engine: &Engine) -> String {
    format!(
        "{}_g{}",
        live(engine).service_name("EffectGroupDispatch"),
        engine
            .lash_backend()
            .build_generation()
            .expect("the engine's generation is bound")
    )
}

struct GroupCheckpoint {
    key: String,
    request: EffectGroupOpenRequest,
    payload: Vec<u8>,
    second_commit: u64,
    addresses: std::collections::BTreeMap<usize, String>,
}

/// A Ready group of three children, none committed: its index record, its
/// retained membership and a registered dispatch.
async fn ready_group(
    engine: &Engine,
) -> (
    String,
    EffectGroupOpenRequest,
    std::collections::BTreeMap<usize, String>,
) {
    let key = run_tag("cold-group");
    let scope = lash_core::ExecutionScope::runtime_operation(&key);
    let children: Vec<_> = (0..3)
        .map(|position| {
            lash_core::RuntimeEffectEnvelope::new(
                lash_core::RuntimeEffectInvocation::new(
                    lash_core::EffectAddress::new(scope.clone(), format!("{key}:{position}"))
                        .unwrap(),
                    lash_core::RuntimeAttribution::none(),
                    "effect",
                ),
                lash_core::RuntimeEffectCommand::LanguageRuntimeValue {
                    operation: format!("recorded-{position}"),
                },
            )
        })
        .collect();
    let request = EffectGroupOpenRequest {
        shape: EffectGroupShape {
            wake: lash_core::GroupWakePolicy::All,
            loser_disposition: lash_core::LoserPolicy::Cancel,
            replay_keys: children
                .iter()
                .map(|c| c.invocation.effect_replay_key().to_owned())
                .collect(),
            opener: lash_core::AdmittedScope::runtime_operation(&key),
        },
        membership: EffectGroupMembership(
            children
                .iter()
                .map(|c| serde_json::to_string(c).unwrap())
                .collect(),
        ),
        dispatch_route: dispatch_lane(engine),
        content_checked: true,
    };
    let fresh: EffectGroupOpenResponse =
        object(engine, "EffectGroupIndex", &key, "open", request.clone()).await;
    assert!(matches!(fresh, EffectGroupOpenResponse::OpenedFresh { .. }));
    // The index law seeds transitions directly. Server-issued handles of
    // completed, unparked host jobs make cancellation legal without giving
    // an oracle closure any role in recovering group state.
    let mut addresses = std::collections::BTreeMap::new();
    for position in 0..3 {
        let job = format!("{key}:unparked:{position}");
        let id = live(engine)
            .ingress()
            .send_workflow_json("LashTestHandlerHost", &job, "run", &job)
            .await
            .expect("server issues a cancellation handle");
        addresses.insert(position, id.to_string());
    }
    let adopted: EffectGroupProbeAdoptResponse = object(
        engine,
        "EffectGroupIndex",
        &key,
        "probe_and_adopt",
        EffectGroupAdoptRequest {
            invocation_id: addresses[&0].clone(),
        },
    )
    .await;
    assert!(matches!(
        adopted,
        EffectGroupProbeAdoptResponse::Adopted { .. }
    ));
    let registered: EffectGroupRegisterDispatchResponse = object(
        engine,
        "EffectGroupIndex",
        &key,
        "register_dispatch",
        EffectGroupRegisterDispatchRequest {
            addresses: addresses.clone(),
        },
    )
    .await;
    assert_eq!(registered, EffectGroupRegisterDispatchResponse::Registered);
    (key, request, addresses)
}

async fn group_checkpoint(engine: &Engine) -> GroupCheckpoint {
    let (key, request, addresses) = ready_group(engine).await;
    let first: serde_json::Value = object(
        engine,
        "EffectGroupIndex",
        &key,
        "commit_child",
        json!({
            "replay_key": request.shape.replay_keys[0].clone(),
            "committed": {"type": "held"},
        }),
    )
    .await;
    let second: serde_json::Value = object(
        engine,
        "EffectGroupIndex",
        &key,
        "commit_child",
        json!({
            "replay_key": request.shape.replay_keys[1].clone(),
            "committed": {"type": "held"},
        }),
    )
    .await;
    assert_eq!(first["type"], "committed");
    assert_eq!(second["type"], "committed");
    let second_commit = second["rank"].as_u64().expect("reserved rank");
    assert!(second_commit > first["rank"].as_u64().unwrap());
    let payload = b"recorded payload before the cold rebuild".to_vec();
    let written: EffectGroupPayloadPutResponse = object(
        engine,
        "EffectGroupPayload",
        &payload_address(&key, 0),
        "put",
        EffectGroupPayloadPutRequest {
            bytes: payload.clone(),
        },
    )
    .await;
    assert_eq!(written, EffectGroupPayloadPutResponse::Written);
    GroupCheckpoint {
        key,
        request,
        payload,
        second_commit,
        addresses,
    }
}

fn payload_address(key: &str, position: usize) -> String {
    format!(
        "{}:{position}",
        lash_core::stable_hash::sha256_hex(key.as_bytes())
    )
}

async fn recover_group(engine: &Engine, checkpoint: GroupCheckpoint) {
    let GroupCheckpoint {
        key,
        request,
        payload,
        second_commit,
        addresses,
    } = checkpoint;
    let stored: EffectGroupPayloadGetResponse = object(
        engine,
        "EffectGroupPayload",
        &payload_address(&key, 0),
        "get",
        (),
    )
    .await;
    assert_eq!(
        stored,
        EffectGroupPayloadGetResponse::Stored {
            bytes: payload.clone()
        }
    );
    let expected_payload = payload.clone();
    let duplicate: EffectGroupPayloadPutResponse = object(
        engine,
        "EffectGroupPayload",
        &payload_address(&key, 0),
        "put",
        EffectGroupPayloadPutRequest { bytes: payload },
    )
    .await;
    assert_eq!(duplicate, EffectGroupPayloadPutResponse::Duplicate);
    // A waiter at the second commit's §5 barrier: the group index answers
    // it only once every lower commit has seated.
    let barrier = object::<_, serde_json::Value>(
        engine,
        "EffectGroupIndex",
        &key,
        "await_notice",
        json!({"type": "drained", "rank": second_commit}),
    );
    tokio::pin!(barrier);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(500), &mut barrier)
            .await
            .is_err(),
        "an unseated lower commit still holds the barrier after reconstruction"
    );
    for position in 0..2 {
        if position == 1 {
            let written: EffectGroupPayloadPutResponse = object(
                engine,
                "EffectGroupPayload",
                &payload_address(&key, 1),
                "put",
                EffectGroupPayloadPutRequest {
                    bytes: b"second".to_vec(),
                },
            )
            .await;
            assert_eq!(written, EffectGroupPayloadPutResponse::Written);
        }
        let seated: EffectGroupRecordSettlementResponse = object(
            engine,
            "EffectGroupIndex",
            &key,
            "record_settlement",
            EffectGroupRecordSettlementRequest {
                position,
                terminal: EffectGroupSettlementTerminal::StoredPayload,
            },
        )
        .await;
        let EffectGroupRecordSettlementResponse::Recorded { rank } = seated else {
            panic!("a committed child seats")
        };
        assert_eq!(rank, position as u64 + 1);
        let again: EffectGroupRecordSettlementResponse = object(
            engine,
            "EffectGroupIndex",
            &key,
            "record_settlement",
            EffectGroupRecordSettlementRequest {
                position,
                terminal: EffectGroupSettlementTerminal::StoredPayload,
            },
        )
        .await;
        assert_eq!(
            again,
            EffectGroupRecordSettlementResponse::Duplicate { rank }
        );
    }
    assert_eq!(
        barrier.await,
        json!({"type": "drained"}),
        "the lower commit's seat lifts the barrier and answers its waiter"
    );
    // The index law seats its commits itself, so it reopens the group only
    // once no seat is owed: a reopen re-sends every committed child whose
    // seat is owed on the group's lane (FIG-4454), and that child's
    // dispatcher, not this oracle, would seat it.
    let reopened: EffectGroupOpenResponse =
        object(engine, "EffectGroupIndex", &key, "open", request.clone()).await;
    assert_eq!(reopened, EffectGroupOpenResponse::ReopenedReady);
    let mut drifted = request.clone();
    drifted.membership.0[0] = drifted.membership.0[0].replace("recorded-0", "unrecorded-0");
    let refused: EffectGroupOpenResponse =
        object(engine, "EffectGroupIndex", &key, "open", drifted).await;
    assert_eq!(
        refused,
        EffectGroupOpenResponse::ContentMismatch { position: 0 },
        "divergence is rejected before dispatch"
    );
    let ranks: EffectGroupReadRankResponse = object(
        engine,
        "EffectGroupIndex",
        &key,
        "read_rank",
        EffectGroupReadRankRequest {
            rank: 1,
            for_caller: true,
            run: true,
        },
    )
    .await;
    let EffectGroupReadRankResponse::SettledRun { ranks } = ranks else {
        panic!("two ranks replay")
    };
    assert_eq!(
        ranks
            .iter()
            .map(|r| r.settlement.position)
            .collect::<Vec<_>>(),
        vec![0, 1]
    );
    assert_eq!(
        ranks[0].payload,
        Some(EffectGroupPayloadGetResponse::Stored {
            bytes: expected_payload
        })
    );
    assert_eq!(
        ranks[1].payload,
        Some(EffectGroupPayloadGetResponse::Stored {
            bytes: b"second".to_vec()
        })
    );
    let _: EffectGroupCloseResponse = object(
        engine,
        "EffectGroupIndex",
        &key,
        "close",
        EffectGroupCloseRequest {
            disposition: lash_core::LoserPolicy::Cancel,
        },
    )
    .await;
    let late: EffectGroupAdmissionResponse = object(
        engine,
        "EffectGroupIndex",
        &key,
        "admit_child",
        EffectGroupAdmissionRequest {
            position: 2,
            invocation_id: addresses[&2].clone(),
        },
    )
    .await;
    assert_eq!(
        late,
        EffectGroupAdmissionResponse::CancelDecided,
        "cancellation fences the remaining fresh admission"
    );
    let late_commit: serde_json::Value = object(
        engine,
        "EffectGroupIndex",
        &key,
        "commit_child",
        json!({
            "replay_key": request.shape.replay_keys[2].clone(),
            "committed": {"type": "held"},
        }),
    )
    .await;
    assert_eq!(late_commit["type"], "cancel_decided");
    assert!(
        engine
            .invocations(&format!("{}/{key}/child", dispatch_lane(engine)))
            .await
            .is_empty(),
        "the rejected offers dispatch nothing"
    );
}

async fn promise_key(engine: &Engine, label: &str) -> lash_core::AwaitEventKey {
    engine
        .lash_backend()
        .effect_host()
        .await_event_key(
            &lash_core::ExecutionScope::runtime_operation(run_tag(label)),
            lash_core::AwaitEventWaitIdentity::Custom {
                key: label.to_owned(),
            },
        )
        .await
        .expect("routable durable key")
}

async fn await_resolution(
    engine: &Engine,
    key: &lash_core::AwaitEventKey,
) -> lash_core::Resolution {
    tokio::time::timeout(
        BOUND,
        engine
            .lash_backend()
            .effect_host()
            .await_await_event(key, Default::default()),
    )
    .await
    .expect("durable wake resolves")
    .expect("resolution")
}

async fn cold_live(engine: Engine) -> Engine {
    let Engine::Live {
        backend, restarts, ..
    } = engine
    else {
        panic!("cold live engine")
    };
    restarts.abort();
    let rebuilt = backend
        .rebuild()
        .await
        .expect("all handlers and caches are reconstructed");
    assert!(!Arc::ptr_eq(
        backend.lash_backend().engine(),
        rebuilt.lash_backend().engine()
    ));
    assert!(!Arc::ptr_eq(
        &backend.lash_backend().effect_host(),
        &rebuilt.lash_backend().effect_host()
    ));
    Engine::on_live_backend(rebuilt)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the crash-windows Restate suite runs it"]
async fn live_restate_engine_obligations_cold_recovery_matrix() {
    let engine = Engine::live("engine-obligations", Some(1)).await;
    let checkpoint = group_checkpoint(&engine).await;
    let cancelled = group_checkpoint(&engine).await;
    let _: EffectGroupCloseResponse = object(
        &engine,
        "EffectGroupIndex",
        &cancelled.key,
        "close",
        EffectGroupCloseRequest {
            disposition: lash_core::LoserPolicy::Cancel,
        },
    )
    .await;
    let key = promise_key(&engine, "recorded-wake").await;
    let first = lash_core::Resolution::Ok(json!({"first": 1}));
    assert!(matches!(
        engine
            .lash_backend()
            .effect_host()
            .resolve_await_event(&key, first.clone())
            .await
            .unwrap(),
        lash_core::ResolveOutcome::Accepted
    ));
    assert_eq!(await_resolution(&engine, &key).await, first);
    let engine = cold_live(engine).await;
    assert_eq!(await_resolution(&engine, &key).await, first);
    assert!(
        matches!(engine.lash_backend().effect_host().resolve_await_event(&key, lash_core::Resolution::Ok(json!({"second": 2}))).await.unwrap(), lash_core::ResolveOutcome::AlreadyResolved { terminal } if terminal == first)
    );
    recover_group(&engine, checkpoint).await;
    let refused: EffectGroupAdmissionResponse = object(
        &engine,
        "EffectGroupIndex",
        &cancelled.key,
        "admit_child",
        EffectGroupAdmissionRequest {
            position: 2,
            invocation_id: cancelled.addresses[&2].clone(),
        },
    )
    .await;
    assert_eq!(
        refused,
        EffectGroupAdmissionResponse::CancelDecided,
        "the pre-crash fence survives reconstruction"
    );
    engine.finish().await;
    // Real VM effects, durable sleep, a pending wake, and process ownership
    // recover through a fresh endpoint; recorded tools never execute again.
    across_wait(
        Engine::live("engine-process-recovery", Some(1)).await,
        32,
        true,
    )
    .await;
    sleep_cold_recovery().await;
}

async fn sleep_cold_recovery() {
    let engine = Engine::live("pending-sleep", None).await;
    let executions = Arc::new(AtomicUsize::new(0));
    let core = process_core(&engine, &executions);
    engine.install_process_worker(worker(&core));
    let request = waiting_request_with_sleep(&engine, 32, "2s").await;
    let began = tokio::time::Instant::now();
    let id = start(&engine, &core, request).await;
    tokio::time::timeout(BOUND, async {
        loop {
            for invocation in engine
                .invocations(&format!("{PROCESS_WORKFLOW}/{id}/run"))
                .await
            {
                if live(&engine)
                    .journal(&invocation.id)
                    .await
                    .unwrap()
                    .iter()
                    .any(|entry| entry.to_ascii_lowercase().contains("sleep"))
                {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the durable timer is in the journal before reconstruction");
    assert_eq!(executions.load(Ordering::SeqCst), 0);
    drop(core);
    let engine = cold_live(engine).await;
    let core = process_core(&engine, &executions);
    engine.install_process_worker(worker(&core));
    record_where(&engine, &id, signal_wait).await;
    assert!(
        began.elapsed() >= Duration::from_secs(2),
        "rebuilding cannot skip the journaled timer"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    signal(&engine, &core, &id).await;
    let terminal = tokio::time::timeout(BOUND, core.processes().await_output(&id))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        terminal,
        lash_core::ProcessAwaitOutput::Settled { .. }
    ));
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    engine.finish().await;
}
