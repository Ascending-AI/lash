//! K6 under process admission, publication and fixed segment journal pins.

use super::*;
use crate::durable_wait::LashDurableWaitRegistry as _;
use crate::durable_wait::LashDurableWaitWorkflow as _;
use crate::process::LashProcessWorkflow as _;
use crate::process::{
    RestateProcessRunner, RestateProcessWorkflowInput, RestateProcessWorkflowOutput,
    RestateProcessWorkflowPayload, SegmentStarted,
};
use lash_core::facade_support::SystemClock;
use lash_core::tool_run::{
    AggregateConsumer, AggregateLeaf, AggregatePlan, MaterialBundle, MaterialHolder, MaterialOwner,
    MaterialPayload, MaterialRole, RunTransfer, SealWriter, SourceSeal,
};
use lash_core::{
    ProcessExecutionContext, ProcessId, ProcessQuery as _, ProcessRegistrar as _,
    ProcessRegistration,
};
use restate_sdk::prelude::Endpoint;

struct Runner {
    probe: Arc<Probe>,
    calls: Vec<SingletonToolCall>,
    ingress: crate::RestateIngressClient,
    reason: lash_core::BoundaryReason,
    held: bool,
    cutting: tokio::sync::Semaphore,
    adopted: tokio::sync::Semaphore,
    finish: tokio::sync::Semaphore,
    transfers: Mutex<Vec<RunTransfer>>,
    records: Mutex<Vec<RunRecord>>,
}

fn plan(key: &str, calls: &[SingletonToolCall]) -> AggregatePlan {
    AggregatePlan {
        key: key.to_owned(),
        leaves: calls
            .iter()
            .map(|call| AggregateLeaf::Call {
                call_id: call.call_id.clone(),
            })
            .collect(),
        operands: (0..calls.len() as u32).collect(),
    }
}

impl Runner {
    async fn seal(
        &self,
        process_id: &ProcessId,
        source: &lash_core::AwaitEventKey,
        call_id: &ToolCallId,
    ) {
        let capture = SingletonCapture::Done {
            output: output_of(call_id),
            commands: Vec::new(),
            intents: Vec::new(),
            stream: Default::default(),
            start: None,
        };
        let bundle = MaterialBundle::of([MaterialPayload::new(
            MaterialOwner::Source {
                source: source.clone(),
            },
            MaterialRole::AttemptOutput,
            Some(revision()),
            serde_json::to_string(&capture).unwrap(),
        )])
        .unwrap()
        .unwrap();
        let retained = self
            .probe
            .materials
            .as_ref()
            .unwrap()
            .retain_material(
                &MaterialHolder::Source {
                    source: source.clone(),
                },
                &bundle,
            )
            .await
            .unwrap();
        let reply: crate::Reply<crate::durable_wait::RestateSourceSealReply> = self
            .ingress
            .call_object_json(
                "LashDurableWaitIndex",
                &crate::durable_wait::durable_wait_index_key_for_scope(
                    &lash_core::ExecutionScope::process(process_id),
                ),
                "seal_source",
                &crate::Call::new(crate::durable_wait::RestateSourceSealRequest {
                    source: source.clone(),
                    writer: SealWriter::External,
                    seal: SourceSeal::Resolved {
                        result: Box::new(retained.references[0].clone()),
                    },
                }),
            )
            .await
            .unwrap();
        assert!(matches!(
            reply.into_body(),
            crate::durable_wait::RestateSourceSealReply::Outcome { .. }
        ));
    }
}

#[async_trait::async_trait]
impl RestateProcessRunner for Runner {
    fn executable_generation(
        &self,
        _: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        started: &SegmentStarted,
        process_id: ProcessId,
        _: ProcessRegistration,
        _: ProcessExecutionContext,
        scoped: ScopedEffectController<'_>,
        handover: Option<lash_core::SegmentHandover>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, lash_core::PluginError> {
        let owner = EffectOpener::process(process_id.clone());
        let segment = started.write_authority().segment().unwrap();
        let handlers = Arc::clone(&self.probe) as Arc<dyn SingletonToolHandlers>;
        let store = self.probe.materials.as_ref().unwrap();
        if segment == SegmentOrdinal(0) {
            let calls: Vec<_> = self
                .calls
                .iter()
                .cloned()
                .map(|mut call| {
                    call.owner = owner.clone();
                    call
                })
                .collect();
            let mut run = RunCoordinator::open(&scoped, owner, segment, vec![revision()]);
            let prior = plan("prior-cell", &calls[..2]);
            run.start_aggregate(
                &prior,
                &calls[..2],
                lash_core::tool_run::CapacityScope::Held,
                Arc::clone(&handlers),
                Default::default(),
                &SystemClock,
            )
            .await
            .unwrap();
            run.consume_aggregate(&prior.key, AggregateConsumer::Race)
                .await
                .unwrap();
            if self.held {
                run.request_cut(self.reason);
                assert!(matches!(
                    run.capture_cut(),
                    Err(lash_core::tool_dispatch::RunCutRefusal::NotQuiescent)
                ));
                self.cutting.add_permits(1);
            } else {
                while run.progress().await.unwrap().is_some() {}
                let current = plan("current-cell", &calls[2..]);
                run.start_aggregate(
                    &current,
                    &calls[2..],
                    lash_core::tool_run::CapacityScope::Held,
                    Arc::clone(&handlers),
                    Default::default(),
                    &SystemClock,
                )
                .await
                .unwrap();
                while run.progress().await.unwrap().is_some() {}
                // A source terminal is durable before it gets a rank in the Run.
                let source = self.probe.sources.lock().unwrap()[&calls[1].call_id].clone();
                self.seal(&process_id, &source, &calls[1].call_id).await;
                run.request_cut(self.reason);
            }
            let mut transfer = run.quiesce().await.unwrap();
            run.retain_cut(&mut transfer, store.as_ref()).await.unwrap();
            assert_eq!(transfer.ledger().unwrap().lifecycle(), RunLifecycle::Live);
            assert!(self.probe.cancelled_calls.lock().unwrap().is_empty());
            if !self.held {
                // K1: the race holds its whole round while its Deferred loser
                // is unpresented, and the current round holds its two calls.
                assert_eq!(transfer.held_calls, 4);
                assert_eq!(transfer.subscriptions.len(), 2);
                assert_eq!(
                    self.probe.presentations.lock().unwrap().as_slice(),
                    std::slice::from_ref(&calls[0].call_id)
                );
                assert!(transfer.entries.iter().flat_map(|entry| &entry.record.events).any(|event| matches!(event, RunEvent::Decided { call_id, .. } if call_id == &calls[2].call_id)));
                assert!(!transfer.entries.iter().flat_map(|entry| &entry.record.events).any(|event| matches!(event, RunEvent::Decided { call_id, .. } if call_id == &calls[1].call_id)));
            }
            self.transfers.lock().unwrap().push(transfer.clone());
            return Ok(lash_core::ProcessRunOutcome::SegmentBoundary(
                lash_core::SegmentHandover {
                    reason: self.reason,
                    program_hash: "process-run-transfer".to_owned(),
                    engine_state: serde_json::to_vec(&transfer).unwrap(),
                },
            ));
        }
        let transfer: RunTransfer =
            serde_json::from_slice(&handover.unwrap().engine_state).unwrap();
        let successor = MaterialHolder::Segment {
            opener: owner.clone(),
            segment,
        };
        // Publication already committed. Move leases and fence the predecessor
        // before adoption; no read may require its now-ended lease.
        for bundle in &transfer.material {
            store.acquire_material(&successor, bundle).await.unwrap();
        }
        store.release_material(&transfer.holder()).await.unwrap();
        let old_ref = &transfer.material[0].references[0];
        assert!(
            store
                .read_material(&transfer.holder(), old_ref, &old_ref.owner, &[revision()])
                .await
                .is_err()
        );
        let mut run = RunCoordinator::adopt(
            &scoped,
            owner,
            segment,
            vec![revision()],
            transfer.clone(),
            handlers,
            &SystemClock,
        )
        .await
        .unwrap();
        assert_eq!(
            run.records(),
            transfer
                .entries
                .iter()
                .map(|entry| entry.record.clone())
                .collect::<Vec<_>>()
        );
        if !self.held {
            // K6 carries the very count the successor's admission enforces.
            let limit = lash_core::MaxToolCalls::new(transfer.held_calls as usize);
            let held = lash_core::tool_run::CapacityScope::Held;
            assert_eq!(run.admit_capacity(&held, 0, limit), Ok(()));
            assert_eq!(
                run.admit_capacity(&held, 1, limit),
                Err(lash_core::ToolCallLimitExceeded {
                    scope: lash_core::ToolCallLimitScope::Process,
                    limit,
                    counted: 4,
                    requested: 1,
                })
            );
        }
        assert!(matches!(
            transfer
                .clone()
                .adopt(&transfer.owner, RunLifecycle::Live, SegmentOrdinal(2)),
            Err(lash_core::tool_run::ContinuationRefusal::NotSuccessor { .. })
        ));
        self.adopted.add_permits(1);
        self.finish.acquire().await.unwrap().forget();
        self.probe.cancel.store(true, Ordering::SeqCst);
        run.close().await.unwrap();
        assert_eq!(run.lifecycle(), RunLifecycle::Settled);
        *self.records.lock().unwrap() = run.into_records();
        store.release_material(&successor).await.unwrap();
        Ok(super::super::process_success(serde_json::json!("transferred")).into())
    }
}

struct World {
    server: lash_restate_test::RestateTestServer,
    ingress: crate::RestateIngressClient,
    stores: lash_sqlite_store::SqliteStoreSet,
    runner: Arc<Runner>,
    process_id: ProcessId,
    registration: ProcessRegistration,
}

impl World {
    async fn new(reason: lash_core::BoundaryReason, held: bool, crash: Option<CrashPoint>) -> Self {
        let server = lash_restate_test::RestateTestServer::new(ServerConfig::default()).unwrap();
        let connection =
            crate::RestateConnection::with_transport(server.ingress_url(), server.transport());
        let ingress = crate::RestateIngressClient::new(connection.clone());
        let stores = lash_sqlite_store::SqliteStoreSet::memory().await.unwrap();
        let kinds = [
            Kind::IntentFree,
            if held {
                Kind::IntentFree
            } else {
                Kind::Deferred
            },
            Kind::Declares(vec![ToolIntentKind::SignalProcess]),
            Kind::Deferred,
        ];
        let calls: Vec<_> = kinds
            .into_iter()
            .enumerate()
            .map(|(index, kind)| {
                (
                    {
                        let mut call = call(&format!("process-{index}"), &kind);
                        call.cancel = ExternalCancelPolicy::CancelExternalWork;
                        call
                    },
                    kind,
                )
            })
            .collect();
        let mut probe = Probe::new(&calls);
        probe.materials = Some(stores.process_env_store());
        if held {
            probe.gate = Some((calls[1].0.call_id.clone(), calls[0].0.call_id.clone()));
        }
        let runner = Arc::new(Runner {
            probe: Arc::new(probe),
            calls: calls.into_iter().map(|(call, _)| call).collect(),
            ingress: ingress.clone(),
            reason,
            held,
            cutting: tokio::sync::Semaphore::new(0),
            adopted: tokio::sync::Semaphore::new(0),
            finish: tokio::sync::Semaphore::new(0),
            transfers: Mutex::default(),
            records: Mutex::default(),
        });
        let registry = stores.process_registry();
        server
            .register(
                Endpoint::builder()
                    .bind(
                        crate::LashProcessWorkflowImpl::new(
                            Arc::clone(&runner),
                            registry.clone(),
                            registry.clone(),
                            ingress.clone(),
                            Arc::new(lash_core::attachments::NoopAttachmentReferrers),
                            super::super::test_restate_authority_id(),
                            super::super::test_build_generation(),
                            &crate::services::DEFAULT_NAMESPACE,
                        )
                        .serve(),
                    )
                    .bind(crate::durable_wait::LashDurableWaitWorkflowImpl::default().serve())
                    .bind(
                        crate::durable_wait::LashDurableWaitRegistryImpl::new(
                            Default::default(),
                            Default::default(),
                            crate::RestateAdminClient::new(connection),
                        )
                        .serve(),
                    )
                    .build(),
            )
            .await
            .unwrap();
        let registration = super::super::executed_registration();
        let process_id = registry
            .register_process(registration.clone())
            .await
            .unwrap()
            .id;
        if let Some(point) = crash {
            server.crash_on(
                CrashRule::new(point)
                    .service("LashProcessWorkflow")
                    .handler("run")
                    .key(process_id.to_string()),
            );
        }
        Self {
            server,
            ingress,
            stores,
            runner,
            process_id,
            registration,
        }
    }

    fn start(&self) -> tokio::task::JoinHandle<RestateProcessWorkflowOutput> {
        let ingress = self.ingress.clone();
        let process_id = self.process_id.clone();
        let input = RestateProcessWorkflowPayload::from(RestateProcessWorkflowInput {
            process_id: process_id.clone(),
            registration: self.registration.clone(),
            execution_context: Default::default(),
            segment_ordinal: 0,
            sender_generation: super::super::test_build_generation(),
        });
        tokio::spawn(async move {
            ingress
                .call_lash_workflow("LashProcessWorkflow", process_id.as_str(), "run", &input)
                .await
                .unwrap()
        })
    }

    async fn permit(&self, semaphore: &tokio::sync::Semaphore) {
        tokio::time::timeout(Duration::from_secs(30), semaphore.acquire())
            .await
            .unwrap_or_else(|_| {
                panic!(
                    "process did not reach its barrier: {:#?}; transfers={}; bodies={:?}",
                    self.server.invocations(),
                    self.runner.transfers.lock().unwrap().len(),
                    self.runner.probe.executions.lock().unwrap()
                )
            })
            .unwrap()
            .forget();
    }

    async fn finish(&self, expected_executions: usize) {
        self.permit(&self.runner.adopted).await;
        // Wait for this predecessor only. The successor deliberately holds
        // local runner work, so settling the whole server would never finish.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            let finished = self.server.invocations().iter().any(|view| {
                view.target == format!("LashProcessWorkflow/{}/run", self.process_id)
                    && view.status == "completed"
            });
            if finished {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "predecessor did not finish: {:#?}",
                self.server.invocations()
            );
            tokio::task::yield_now().await;
        }
        self.server.advance(Duration::from_secs(3601));
        assert!(!self.runner.probe.cancel.load(Ordering::SeqCst));
        assert_eq!(
            self.stores
                .process_registry()
                .get_process(&self.process_id)
                .await
                .unwrap()
                .unwrap()
                .status(),
            lash_core::ProcessStatus::Running
        );
        let root = self
            .server
            .invocations()
            .into_iter()
            .find(|view| view.target == format!("LashProcessWorkflow/{}/run", self.process_id))
            .unwrap();
        let journal = self.server.journal(&root.id).unwrap();
        let publish = journal
            .iter()
            .position(|entry| entry.name.as_deref() == Some("lash.segment.handover"))
            .unwrap();
        let release = journal
            .iter()
            .position(|entry| {
                entry
                    .name
                    .as_deref()
                    .is_some_and(|name| name.contains("release_process_journal"))
            })
            .or_else(|| {
                journal.iter().position(|entry| {
                    entry.ty == MessageType::CallCommand
                        && entry
                            .call_command()
                            .is_some_and(|call| call.handler_name == "release_process_journal")
                })
            });
        assert!(publish < release.expect("the predecessor released its journal pin"));
        let key = crate::durable_wait::durable_wait_index_key_for_scope(
            &lash_core::ExecutionScope::process(&self.process_id),
        );
        let pins = self
            .server
            .object_state("LashDurableWaitIndex", &key)
            .keys()
            .filter(|key| key.starts_with("wait-index/v2/process-journal/"))
            .count();
        assert_eq!(pins, 1, "only the successor journal pins the scope");
        self.runner.finish.add_permits(1);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        while self.runner.records.lock().unwrap().is_empty() {
            assert!(tokio::time::Instant::now() < deadline);
            tokio::task::yield_now().await;
        }
        loop {
            let finished = self.server.invocations().iter().any(|view| {
                view.target == format!("LashProcessWorkflow/{}#1/run", self.process_id)
                    && view.status == "completed"
            });
            if finished {
                break;
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "successor did not finish: {:#?}",
                self.server.invocations()
            );
            tokio::task::yield_now().await;
        }
        assert_eq!(
            self.stores
                .process_registry()
                .get_process(&self.process_id)
                .await
                .unwrap()
                .unwrap()
                .status(),
            lash_core::ProcessStatus::Completed
        );
        assert_eq!(
            self.server
                .object_state("LashDurableWaitIndex", &key)
                .keys()
                .filter(|key| key.starts_with("wait-index/v2/process-journal/"))
                .count(),
            0,
            "terminal publication releases the successor journal pin"
        );
        assert_eq!(
            self.runner.probe.executions.lock().unwrap().len(),
            expected_executions
        );
        assert!(
            self.runner
                .probe
                .presentations
                .lock()
                .unwrap()
                .iter()
                .filter(|id| **id == self.runner.calls[0].call_id)
                .count()
                == 1
        );
    }
}

#[tokio::test]
async fn l09_process_transfers_prior_current_finals_and_unranked_sources_after_predecessor_fence() {
    for reason in [
        lash_core::BoundaryReason::HandOver,
        lash_core::BoundaryReason::JournalBudget,
    ] {
        let world = World::new(
            reason,
            false,
            Some(CrashPoint::BeforeRun {
                name: "lash.segment.handover".to_owned(),
            }),
        )
        .await;
        let root = world.start();
        world.finish(4).await;
        assert!(matches!(
            root.await.unwrap(),
            RestateProcessWorkflowOutput::SegmentChained {
                next_segment_ordinal: 1
            }
        ));
        let transfer = world
            .runner
            .transfers
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .clone();
        assert_eq!(transfer.from, SegmentOrdinal(0));
        assert_eq!(transfer.sources.len(), 2);
        let records = world.runner.records.lock().unwrap();
        assert!(records.iter().flat_map(|record| &record.events).any(|event| matches!(event, RunEvent::Decided { call_id, decision: CallDecision::Final { source: lash_core::tool_run::ResultSource::DeferredCompletion { .. }, .. }, .. } if call_id == &world.runner.calls[1].call_id)));
    }
}

#[tokio::test]
async fn l16_process_inline_loser_blocks_publication_until_ack_for_both_cut_reasons() {
    for reason in [
        lash_core::BoundaryReason::HandOver,
        lash_core::BoundaryReason::JournalBudget,
    ] {
        let world = World::new(reason, true, None).await;
        let root = world.start();
        world.permit(&world.runner.cutting).await;
        assert!(world.runner.transfers.lock().unwrap().is_empty());
        assert!(
            world
                .runner
                .probe
                .cancelled_calls
                .lock()
                .unwrap()
                .is_empty()
        );
        assert!(!root.is_finished());
        let record = world
            .stores
            .process_registry()
            .get_process(&world.process_id)
            .await
            .unwrap()
            .unwrap();
        assert!(
            record.external_ref.is_none(),
            "a blocked cut publishes no successor"
        );
        world.runner.probe.gate_open.store(true, Ordering::SeqCst);
        world.runner.probe.gate_wake.notify_waiters();
        world.finish(2).await;
        root.await.unwrap();
    }
}

#[tokio::test]
async fn l16_process_unacknowledged_proposal_recovers_on_the_predecessor() {
    let held_id = ToolCallId::fixture("process-1");
    let world = World::new(
        lash_core::BoundaryReason::HandOver,
        true,
        Some(CrashPoint::BeforeRunResultEnding {
            suffix: format!("{held_id}:attempt:1"),
        }),
    )
    .await;
    let root = world.start();
    world.permit(&world.runner.cutting).await;
    assert!(world.runner.transfers.lock().unwrap().is_empty());
    world.runner.probe.gate_open.store(true, Ordering::SeqCst);
    world.runner.probe.gate_wake.notify_waiters();
    world.finish(3).await;
    root.await.unwrap();
    let executions = world.runner.probe.executions.lock().unwrap();
    assert_eq!(
        executions.iter().filter(|(id, _)| *id == held_id).count(),
        2
    );
    assert_eq!(
        executions
            .iter()
            .filter(|(id, _)| *id == world.runner.calls[0].call_id)
            .count(),
        1,
        "the acknowledged winner never reruns"
    );
}
