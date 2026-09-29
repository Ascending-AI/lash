use super::*;
use lash_restate::*;

struct ProcessEngines(lash_protocol_rlm::RlmProtocolPluginFactory);

#[async_trait::async_trait]
impl lash_core::plugin::PluginFactory for ProcessEngines {
    fn id(&self) -> &'static str {
        "recovery-process-engine"
    }
    fn bound_backend(&self) -> Option<&str> {
        self.0.bound_backend()
    }
    fn process_engine_contributions(
        &self,
        ctx: &lash_core::plugin::ProcessEngineContributionContext<'_>,
    ) -> Result<Vec<lash_core::ProcessEngineRegistration>, lash_core::PluginError> {
        self.0.process_engine_contributions(ctx)
    }
    fn build(
        &self,
        ctx: &lash_core::plugin::PluginSessionContext,
    ) -> Result<Arc<dyn lash_core::plugin::SessionPlugin>, lash_core::PluginError> {
        lash_core::plugin::StaticPluginFactory::new(self.id(), lash_core::plugin::PluginSpec::new())
            .build(ctx)
    }
}

fn deployment_core(
    engine: &Engine,
    executions: &Arc<AtomicUsize>,
    models: &Arc<AtomicUsize>,
) -> lash::LashCore {
    let backend = engine.lash_backend();
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::builder()
            .channel(lash_protocol_rlm::RlmChannel::Cell)
            .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(1_000_000))
            .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
            .build(),
        &backend,
    );
    let models = Arc::clone(models);
    let provider = lash_core::testing::TestProvider::builder()
        .kind("cold-recovery")
        .complete(move |_request: LlmRequest| {
            models.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok::<_, LlmTransportError>(LlmResponse {
                    parts: vec![LlmOutputPart::Text {
                        text: "recorded answer".into(),
                        response_meta: None,
                    }],
                    ..Default::default()
                })
            }
        })
        .build()
        .into_handle();
    lash::LashCore::standard_builder(backend, lash::TurnBudget::Unbounded)
        .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
        .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1))
        .provider(provider)
        .model(model_spec())
        .tools(Arc::new(CountingTool {
            executions: Arc::clone(executions),
            output: json!({"result": "counted"}),
        }) as Arc<dyn lash_core::ToolProvider>)
        .plugin(Arc::new(ProcessEngines(factory)))
        .build(lash_core::LeaseOwnerIdentity::opaque(
            "lash-restate-test",
            "cold-recovery",
        ))
        .expect("fresh session driver, process engine and tool context source")
}

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

struct GroupCheckpoint {
    key: String,
    request: EffectGroupOpenRequest,
    payload: Vec<u8>,
    second_commit: u64,
    addresses: std::collections::BTreeMap<usize, String>,
}

async fn group_checkpoint(engine: &Engine) -> GroupCheckpoint {
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
            wait_scope: scope,
            opener: lash_core::AdmittedScope::runtime_operation(&key),
        },
        membership: EffectGroupMembership(
            children
                .iter()
                .map(|c| serde_json::to_string(c).unwrap())
                .collect(),
        ),
        dispatch_route: "EffectGroupDispatch".to_owned(),
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
    let dispatched = addresses.clone();
    let result: EffectGroupRecordDispatchResponse = object(
        engine,
        "EffectGroupIndex",
        &key,
        "record_dispatch",
        EffectGroupRecordDispatchRequest { dispatched },
    )
    .await;
    assert_eq!(result, EffectGroupRecordDispatchResponse::Recorded);
    let registered: EffectGroupRegisterResponse = object(
        engine,
        "EffectGroupIndex",
        &key,
        "register_children",
        EffectGroupRegisterRequest {
            addresses: addresses.clone(),
        },
    )
    .await;
    assert_eq!(registered, EffectGroupRegisterResponse::Registered);
    let first: serde_json::Value = object(
        engine,
        "EffectGroupIndex",
        &key,
        "commit_child",
        json!({"replay_key": request.shape.replay_keys[0].clone()}),
    )
    .await;
    let second: serde_json::Value = object(
        engine,
        "EffectGroupIndex",
        &key,
        "commit_child",
        json!({"replay_key": request.shape.replay_keys[1].clone()}),
    )
    .await;
    assert_eq!(first["type"], "committed");
    assert_eq!(first["blocking_positions"], json!([]));
    assert_eq!(second["type"], "committed");
    let second_commit = second["commit_seq"]
        .as_u64()
        .expect("recorded commit position");
    assert!(second_commit > first["commit_seq"].as_u64().unwrap());
    assert_eq!(second["blocking_positions"], json!([0]));
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
    let blocker: serde_json::Value = object(
        engine,
        "EffectGroupIndex",
        &key,
        "drain_blockers",
        json!({"commit_seq": second_commit}),
    )
    .await;
    assert!(
        blocker["type"] == "blocked" && blocker["positions"] == json!([0]),
        "an unseated lower commit still blocks the drain after reconstruction"
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
    let clear: serde_json::Value = object(
        engine,
        "EffectGroupIndex",
        &key,
        "drain_blockers",
        json!({"commit_seq": second_commit}),
    )
    .await;
    assert_eq!(clear, json!({"type": "admitted"}));
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
        json!({"replay_key": request.shape.replay_keys[2].clone()}),
    )
    .await;
    assert_eq!(late_commit["type"], "cancel_decided");
    assert!(
        engine
            .invocations(&format!("EffectGroupDispatch/{key}/child"))
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
            .await_await_event(key, Default::default(), None),
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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs a live restate-server: the crash-windows Restate suite runs it"]
async fn live_restate_stateless_service_rebuild_recovers_each_service_kind() {
    let engine = Engine::live("all-services", Some(1)).await;
    let prior: std::collections::HashSet<_> = engine
        .invocations("")
        .await
        .into_iter()
        .map(|i| i.id)
        .collect();
    let executions = Arc::new(AtomicUsize::new(0));
    let models = Arc::new(AtomicUsize::new(0));
    let core = deployment_core(&engine, &executions, &models);
    engine.install_process_worker(worker(&core));
    let session_id = lash_core::SessionId::from(run_tag("rebuild-session"));
    let session = crate::created_session(&core, session_id.as_str())
        .await
        .open()
        .await
        .expect("open session");
    let handle = session
        .send(lash::TurnInput::text("record before reconstruction"))
        .id("before")
        .await
        .expect("accept input");
    let drive = lash_core::drive::ingress_drive_request(
        handle.input_id().as_str(),
        lash_core::drive::FIRST_INGRESS_ATTEMPT,
    );
    handle.output().await.expect("first root commits");
    let first_drive = live(&engine)
        .attach_drive(&session_id, drive.clone())
        .await
        .expect("recorded drive");
    let first_root = first_drive.ran[0].root().clone();
    assert_eq!(models.load(Ordering::SeqCst), 1);
    let checkpoint = group_checkpoint(&engine).await;
    let mut env = process_env_spec();
    env.render = Some(lash_core::RecordedRender {
        renderer_id: "lash.tool.v1".to_owned(),
        params: serde_json::to_value(
            lash::render::resolve(
                &lash::render::StandardRenderConfig::builtin(),
                &lash::render::StandardRenderConfig::default(),
                &lash::render::StandardRenderConfig::default(),
            )
            .expect("Standard renderer parameters"),
        )
        .unwrap(),
    });
    let request = waiting_request(&engine, 32).await.with_env_spec(env);
    let id = start(&engine, &core, request).await;
    record_where(&engine, &id, signal_wait).await;
    let key = promise_key(&engine, "attach-process").await;
    let attach_address = RestateDurableWaitAddress::for_key(&key);
    live(&engine)
        .ingress()
        .send_workflow_json(
            "LashProcessAttach",
            &attach_address.workflow_key,
            "run",
            &Call::new(RestateProcessAttachRequest {
                process_id: id.clone(),
                key: key.clone(),
            }),
        )
        .await
        .expect("arm a terminal wait outside the process");
    tokio::time::timeout(BOUND, async {
        loop {
            if engine
                .invocations(&format!(
                    "LashProcessAttach/{}/run",
                    attach_address.workflow_key
                ))
                .await
                .iter()
                .any(|i| i.status != "completed")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("attach is in flight before reconstruction");
    // A Ready dispatcher owes no new child sends. Cut its final frame so
    // the new instance must replay that retained guard, with no old cache.
    let Engine::Live { restarts, .. } = &engine else {
        unreachable!()
    };
    restarts.abort();
    engine.crash_on(
        CrashRule::new(CrashPoint::BeforeFrame {
            ty: MessageType::OutputCommand,
        })
        .service("EffectGroupDispatch")
        .handler("run")
        .key(&checkpoint.key),
    );
    let dispatch = live(&engine)
        .ingress()
        .send_workflow_json(
            "EffectGroupDispatch",
            &checkpoint.key,
            "run",
            &Call::new(EffectGroupDispatchRequest {
                group_key: checkpoint.key.clone(),
            }),
        )
        .await
        .expect("submit retained dispatcher");
    tokio::time::timeout(BOUND, async {
        while engine.crashes() == 0 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the dispatcher reached its recorded Ready guard");
    assert_eq!(engine.crashes(), 1);
    drop(session);
    drop(core);
    let engine = cold_live(engine).await;
    let core = deployment_core(&engine, &executions, &models);
    engine.install_process_worker(worker(&core));
    tokio::time::timeout(BOUND, async {
        loop {
            if let Some(outcome) = live(&engine).outcome(dispatch.as_str()).await.unwrap() {
                outcome.expect("the fresh dispatcher replays its Ready guard");
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the dispatcher recovers after endpoint reconstruction");
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "the retained dispatcher sends no additional tool work"
    );
    let old_drive = live(&engine)
        .attach_drive(&session_id, drive)
        .await
        .expect("old drive reattaches");
    assert_eq!(old_drive, first_drive);
    let reply: Reply<Option<lash_core::engine::RootOutcome>> = live(&engine)
        .ingress()
        .call_workflow_json(
            "LashTurn",
            &turn_workflow_key(&session_id, &first_root),
            "outcome",
            &Call::new(()),
        )
        .await
        .expect("fresh turn handler reads its recorded state");
    assert_eq!(reply.body, Some(first_drive.ran[0].clone()));
    let session = core
        .session(session_id.as_str())
        .open()
        .await
        .expect("reopen session on fresh core");
    session
        .attach_id("before")
        .output()
        .await
        .expect("old output survives");
    assert_eq!(
        models.load(Ordering::SeqCst),
        1,
        "no model call to reconstruct old work"
    );
    session
        .send(lash::TurnInput::text("record after reconstruction"))
        .id("after")
        .output()
        .await
        .expect("fresh session handler drives only new ingress");
    assert_eq!(models.load(Ordering::SeqCst), 2);
    recover_group(&engine, checkpoint).await;
    signal(&engine, &core, &id).await;
    let terminal = core
        .processes()
        .await_output(&id)
        .await
        .expect("fresh process workflow finishes");
    assert_eq!(
        await_resolution(&engine, &key).await,
        lash_core::Resolution::Ok(serde_json::to_value(terminal).unwrap()),
        "fresh attach and durable-wait services recover the armed terminal"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    engine.settle().await;
    let invocations: Vec<_> = engine
        .invocations("")
        .await
        .into_iter()
        .filter(|i| !prior.contains(&i.id))
        .collect();
    for service in [
        "LashDurableWaitWorkflow",
        "LashDurableWaitIndex",
        "LashProcessWorkflow",
        "LashProcessAttach",
        "EffectGroupIndex",
        "EffectGroupPayload",
        "EffectGroupDispatch",
        "LashSession",
        "LashTurn",
    ] {
        assert!(
            invocations
                .iter()
                .any(|i| i.target.split('/').next().is_some_and(
                    |route| route == service || route.starts_with(&format!("{service}_g"))
                )),
            "the matrix exercised {service}"
        );
    }
    engine.finish().await;
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
