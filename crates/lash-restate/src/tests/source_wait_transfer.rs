//! L07/L09/L11: only logical source facts survive a segment's subscription.

use super::*;
use crate::controller::ProcessCancelRace;
use crate::durable_wait::{
    RestateSourceArmReply, RestateSourceArmRequest, RestateSourceSealReply,
    RestateSourceSealRequest,
};
use lash_core::tool_run::{
    ExternalCancelPolicy, MaterialDigest, MaterialLocation, MaterialOwner, MaterialRef,
    MaterialRole, SealOutcome, SealWriter, SegmentOrdinal, SourceAuthority, SourceDescriptor,
    SourceSeal, SourceSubscription,
};
use lash_restate_test::{RestateTestServer, ServerConfig};
use restate_sdk::context::{ContextPromises as _, SharedWorkflowContext};

const PROBE: &str = "SourceTransferProbe";
const INDEX: &str = "LashDurableWaitIndex";

#[derive(Serialize, serde::Deserialize)]
struct Input {
    source: SourceDescriptor,
    segment: u32,
}

#[derive(Debug, PartialEq, Serialize, serde::Deserialize)]
enum WaitEnd {
    HandedOver,
    Sealed(SourceSeal),
}

#[restate_sdk::workflow]
trait SourceTransferProbe {
    async fn run(input: Json<Input>) -> HandlerResult<Json<WaitEnd>>;
    #[shared]
    async fn hand_over(input: Json<lash_core::engine::BuildGeneration>) -> HandlerResult<Json<()>>;
}

struct SourceTransferProbeImpl {
    generation: lash_core::engine::BuildGeneration,
}

impl SourceTransferProbe for SourceTransferProbeImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(input): Json<Input>,
    ) -> HandlerResult<Json<WaitEnd>> {
        let result = ctx
            .await_run_sources(
                &crate::services::DEFAULT_NAMESPACE,
                vec![SourceSubscription {
                    source: input.source.source,
                    owner: input.source.owner,
                    segment: SegmentOrdinal(input.segment),
                }],
                None,
                Some(self.generation.clone()),
                ProcessCancelRace::Raced,
            )
            .await;
        match result {
            Ok(RestateTurnCancelRaceOutcome::Completed((0, seal))) => {
                Ok(Json(WaitEnd::Sealed(seal)))
            }
            Err(error)
                if crate::wire::typed_terminal(error.message()).is_some_and(|error| {
                    error.code == lash_core::RuntimeErrorCode::TurnWaitHandedOver
                }) =>
            {
                Ok(Json(WaitEnd::HandedOver))
            }
            result => Err(TerminalError::new(format!("unexpected source wait: {result:?}")).into()),
        }
    }

    async fn hand_over(
        &self,
        ctx: SharedWorkflowContext<'_>,
        Json(generation): Json<lash_core::engine::BuildGeneration>,
    ) -> HandlerResult<Json<()>> {
        ctx.resolve_promise(
            crate::process::PROCESS_HAND_OVER_PROMISE_KEY,
            serde_json::to_string(&generation).map_err(TerminalError::from_error)?,
        );
        Ok(Json(()))
    }
}

struct NoRun;

#[async_trait::async_trait]
impl RestateProcessRunner for NoRun {
    fn executable_generation(
        &self,
        _: &ProcessRegistration,
    ) -> Option<lash_core::ExecutableGeneration> {
        None
    }

    async fn run_process_segment(
        &self,
        _: &SegmentStarted,
        _: ProcessId,
        _: ProcessRegistration,
        _: ProcessExecutionContext,
        _: ScopedEffectController<'_>,
        _: Option<lash_core::SegmentHandover>,
        _: tokio_util::sync::CancellationToken,
    ) -> Result<lash_core::ProcessRunOutcome, PluginError> {
        unreachable!("these laws use externally owned sources")
    }
}

pub(super) async fn endpoint(
    connection: &RestateConnection,
    stores: &lash_sqlite_store::SqliteStoreSet,
    build: &'static str,
) -> Endpoint {
    let host = Arc::new(RestateEffectHost::new_for_test(connection.clone()));
    crate::services::bind_lash_services(
        Endpoint::builder(),
        crate::services::LashServiceParts {
            effect_host: &host,
            ingress: RestateIngressClient::new(connection.clone()),
            admin: crate::RestateAdminClient::new(connection.clone()),
            materials: stores.tool_material_store(),
            attachments: stores.attachment_referrers(),
            sessions: stores.session_store_factory(),
            process_workflow: LashProcessWorkflowImpl::new_for_test(
                Arc::new(NoRun),
                stores.process_registry(),
                stores.process_continuations(),
            ),
            session_shifts: crate::RestateSessionShiftsSlot::new(),
            build_generation: lash_core::engine::BuildGeneration::for_test(build),
            namespace: crate::RestateNamespace::default(),
            fleet: crate::object_state::FleetView::default(),
        },
    )
    .bind(
        SourceTransferProbeImpl {
            generation: lash_core::engine::BuildGeneration::for_test(build),
        }
        .serve(),
    )
    .build()
}

/// L11: a process segment leaves no physical subscription on N, although its
/// successor still waits on the same unresolved source. Unrelated N work
/// independently blocks non-forced removal until it drains.
#[tokio::test]
async fn a_process_source_wait_releases_n_before_its_successor_resolves() {
    source_transfer(false).await;
}

/// L09/L11: losing an unsubscribe before its state write still acknowledges
/// the exact predecessor subscription before its segment returns.
#[tokio::test]
async fn a_crash_during_unsubscribe_cannot_leave_a_predecessor_subscription() {
    source_transfer(true).await;
}

async fn source_transfer(crash: bool) {
    let server = RestateTestServer::new(ServerConfig::default().with_seed(0x4891_0001))
        .expect("server double");
    let connection = RestateConnection::with_transport(server.ingress_url(), server.transport());
    let ingress = RestateIngressClient::new(connection.clone());
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("SQLite");
    let old = server
        .register(endpoint(&connection, &stores, "N").await)
        .await
        .expect("N");
    let scope = ExecutionScope::process(ProcessId::fixture("source-owner"));
    let call_id = lash_core::ToolCallId::fixture("source-transfer");
    let source = SourceDescriptor {
        source: test_restate_await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion(call_id.clone()),
        )
        .expect("source key"),
        owner: lash_core::EffectOpener::process(ProcessId::fixture("source-owner")),
        call_id,
        resolver: lash_core::plugin::PluginRevision::new(
            "tools",
            lash_core::plugin::BehaviorRevision::ONE,
        ),
        authority: SourceAuthority::ExternalCompletion,
        cancel: ExternalCancelPolicy::Ignore,
    };
    let address = crate::RestateDurableWaitAddress::for_key(&source.source);
    let arm: crate::Reply<RestateSourceArmReply> = ingress
        .call_object_json(
            INDEX,
            &address.index_key(),
            "arm_source",
            &crate::Call::new(RestateSourceArmRequest {
                descriptor: source.clone(),
            }),
        )
        .await
        .expect("arm");
    assert!(matches!(
        arm.into_body(),
        RestateSourceArmReply::Armed { seal: None }
    ));
    let wait = |key: &'static str, segment| {
        let ingress = ingress.clone();
        let source = source.clone();
        tokio::spawn(async move {
            ingress
                .call_workflow_json::<_, WaitEnd>(PROBE, key, "run", &Input { source, segment })
                .await
        })
    };
    let predecessor = wait("old", 0);
    let unrelated = wait("unrelated", 0);
    wait_until(&server, || {
        server
            .invocations()
            .iter()
            .filter(|view| {
                view.target.starts_with(PROBE)
                    && view.target.ends_with("/run")
                    && (view.status == "suspended" || view.blocked_on_server == Some(true))
            })
            .count()
            == 2
    })
    .await;
    server
        .register(endpoint(&connection, &stores, "N+1").await)
        .await
        .expect("N+1");
    if crash {
        server.crash_on(
            lash_restate_test::CrashRule::new(lash_restate_test::CrashPoint::BeforeFrame {
                ty: lash_restate_test::protocol::MessageType::SetStateCommand,
            })
            .service(INDEX)
            .handler("unsubscribe_source"),
        );
    }
    ingress
        .call_workflow_json::<_, ()>(
            PROBE,
            "old",
            "hand_over",
            &lash_core::engine::BuildGeneration::for_test("N"),
        )
        .await
        .expect("wake N");
    let result = tokio::time::timeout(Duration::from_secs(5), predecessor)
        .await
        .expect("the process source wait must take its segment's handover wake")
        .expect("wait task")
        .expect("predecessor");
    assert_eq!(result, WaitEnd::HandedOver);
    if crash {
        assert_eq!(
            server
                .invocations()
                .iter()
                .filter(|view| view.target.ends_with("/unsubscribe_source") && view.attempts == 2)
                .count(),
            1,
            "the unsubscribe crashed once and replayed"
        );
    }
    let successor = wait("next", 1);
    wait_until(&server, || {
        server.invocations().iter().any(|view| {
            view.target == format!("{PROBE}/next/run")
                && (view.status == "suspended" || view.blocked_on_server == Some(true))
        })
    })
    .await;
    assert!(
        server.remove_deployment(&old, false).is_err(),
        "independent old work still blocks removal"
    );
    ingress
        .call_workflow_json::<_, ()>(
            PROBE,
            "unrelated",
            "hand_over",
            &lash_core::engine::BuildGeneration::for_test("N"),
        )
        .await
        .expect("drain independent work");
    assert_eq!(
        unrelated.await.expect("task").expect("unrelated"),
        WaitEnd::HandedOver
    );
    server.settle().await;
    server
        .remove_deployment(&old, false)
        .expect("no wait or read pins N while the source remains unresolved");
    assert!(!successor.is_finished(), "the successor is still pending");
    let seal = SourceSeal::Resolved {
        result: Box::new(MaterialRef {
            owner: MaterialOwner::Source {
                source: source.source.clone(),
            },
            role: MaterialRole::AttemptOutput,
            location: MaterialLocation::RetainedArtifact {
                artifact: lash_core::ArtifactName {
                    store: lash_core::ArtifactStoreId::ToolMaterial,
                    artifact_ref: "retained-result".into(),
                },
            },
            digest: MaterialDigest::parse(&"a".repeat(64)).expect("digest"),
        }),
    };
    let resolved: crate::Reply<RestateSourceSealReply> = ingress
        .call_object_json(
            INDEX,
            &address.index_key(),
            "seal_source",
            &crate::Call::new(RestateSourceSealRequest {
                source: source.source,
                writer: SealWriter::External,
                seal: seal.clone(),
            }),
        )
        .await
        .expect("seal after N was removed");
    assert_eq!(
        resolved.into_body(),
        RestateSourceSealReply::Outcome {
            outcome: SealOutcome::Sealed { seal: seal.clone() }
        }
    );
    assert_eq!(
        successor.await.expect("task").expect("successor"),
        WaitEnd::Sealed(seal)
    );
}

async fn wait_until(server: &RestateTestServer, done: impl Fn() -> bool) {
    tokio::time::timeout(Duration::from_secs(30), async {
        while !done() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("wait never settled: {:?}", server.invocations()));
}

/// L09/L11: the ACK retires one physical subscription, while a successor's
/// subscription to the same immutable source keeps its authority. Both sides
/// of the index write recover, and removal cannot precede that ACK.
#[tokio::test]
async fn native_retirement_preserves_the_successor_on_both_crash_sides() {
    for point in [
        lash_restate_test::CrashPoint::BeforeFrame {
            ty: lash_restate_test::protocol::MessageType::SetStateCommand,
        },
        lash_restate_test::CrashPoint::BeforeFrame {
            ty: lash_restate_test::protocol::MessageType::OutputCommand,
        },
    ] {
        native_retirement(Some(point), false).await;
    }
}

/// L07/L09/L11: resolve and cancel can land while retirement is awaiting its
/// ACK. The successor observes the one seal, and external work stays live.
#[tokio::test]
async fn completion_and_cancellation_survive_a_retirement_in_flight() {
    for cancel in [false, true] {
        native_retirement(None, cancel).await;
    }
}

async fn native_retirement(crash: Option<lash_restate_test::CrashPoint>, cancel: bool) {
    use lash_core::{ProcessQuery as _, ProcessRegistrar as _};
    let server = RestateTestServer::new(ServerConfig::default().with_seed(0x4928)).expect("server");
    let connection = RestateConnection::with_transport(server.ingress_url(), server.transport());
    let ingress = RestateIngressClient::new(connection.clone());
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("SQLite");
    let old = server
        .register(endpoint(&connection, &stores, "N").await)
        .await
        .expect("N");
    let process = stores
        .process_registry()
        .register_process(external_registration())
        .await
        .expect("external process")
        .id;
    let owner = lash_core::EffectOpener::turn("native-source-session", "logical-run");
    let call_id = lash_core::ToolCallId::fixture("retirement-call");
    let source = SourceDescriptor {
        source: test_restate_await_event_key(
            owner.admitted_scope().scope(),
            AwaitEventWaitIdentity::tool_completion(call_id.clone()),
        )
        .expect("source key"),
        owner,
        call_id,
        resolver: lash_core::plugin::PluginRevision::new(
            "tools",
            lash_core::plugin::BehaviorRevision::ONE,
        ),
        authority: SourceAuthority::ProcessTerminal {
            process_id: process.clone(),
        },
        cancel: ExternalCancelPolicy::Ignore,
    };
    let address = crate::RestateDurableWaitAddress::for_key(&source.source);
    let arm: crate::Reply<RestateSourceArmReply> = ingress
        .call_object_json(
            INDEX,
            &address.index_key(),
            "arm_source",
            &crate::Call::new(RestateSourceArmRequest {
                descriptor: source.clone(),
            }),
        )
        .await
        .expect("arm");
    assert!(matches!(
        arm.body,
        RestateSourceArmReply::Armed { seal: None }
    ));
    let wait = |key: &'static str, segment| {
        let ingress = ingress.clone();
        let source = source.clone();
        tokio::spawn(async move {
            ingress
                .call_workflow_json::<_, WaitEnd>(PROBE, key, "run", &Input { source, segment })
                .await
        })
    };
    let predecessor = wait("native-old", 0);
    wait_until(&server, || {
        server.invocations().iter().any(|view| {
            view.target == format!("{PROBE}/native-old/run") && view.blocked_on_server == Some(true)
        })
    })
    .await;
    server
        .register(endpoint(&connection, &stores, "N+1").await)
        .await
        .expect("N+1");
    let successor = wait("native-next", 1);
    wait_until(&server, || {
        server.invocations().iter().any(|view| {
            view.target == format!("{PROBE}/native-next/run")
                && view.blocked_on_server == Some(true)
        })
    })
    .await;
    let read = || {
        let bytes = server
            .object_state(INDEX, &address.index_key())
            .remove(&format!("wait-index/v2/source/{}", address.workflow_key))
            .expect("source row");
        let row: serde_json::Value = serde_json::from_slice(&bytes).expect("stamped row");
        serde_json::from_value::<crate::durable_wait::source_seal::IndexedSource>(
            row["body"].clone(),
        )
        .expect("source")
    };
    assert_eq!(read().subscribers.len(), 2);
    let count = lash_restate_test::CrashCount::new();
    assert!(server.on_crash(count.listener()));
    let held = if crash.is_none() {
        Some(server.hold(INDEX, &address.index_key()).await)
    } else {
        None
    };
    if let Some(point) = &crash {
        server.crash_on(
            lash_restate_test::CrashRule::new(point.clone())
                .service(INDEX)
                .handler("unsubscribe_source"),
        );
    }
    ingress
        .call_workflow_json::<_, ()>(
            PROBE,
            "native-old",
            "hand_over",
            &lash_core::engine::BuildGeneration::for_test("N"),
        )
        .await
        .expect("handover");
    let seal = if cancel {
        SourceSeal::Cancelled
    } else {
        SourceSeal::Resolved {
            result: Box::new(MaterialRef {
                owner: MaterialOwner::Source {
                    source: source.source.clone(),
                },
                role: MaterialRole::AttemptOutput,
                location: MaterialLocation::RetainedArtifact {
                    artifact: lash_core::ArtifactName {
                        store: lash_core::ArtifactStoreId::ToolMaterial,
                        artifact_ref: "retirement-result".into(),
                    },
                },
                digest: MaterialDigest::parse(&"a".repeat(64)).expect("digest"),
            }),
        }
    };
    if let Some(held) = held {
        wait_until(&server, || {
            predecessor.is_finished()
                || server.invocations().iter().any(|view| {
                    view.target.ends_with("/unsubscribe_source") && view.status != "completed"
                })
        })
        .await;
        assert!(
            !predecessor.is_finished(),
            "the predecessor cannot answer before retirement ACK"
        );
        assert!(
            server.remove_deployment(&old, false).is_err(),
            "the unacknowledged retirement still holds N"
        );
        let resolving = {
            let ingress = ingress.clone();
            let source = source.clone();
            let seal = seal.clone();
            let key = address.index_key();
            let process = process.clone();
            tokio::spawn(async move {
                ingress
                    .call_object_json::<_, crate::Reply<RestateSourceSealReply>>(
                        INDEX,
                        &key,
                        "seal_source",
                        &crate::Call::new(RestateSourceSealRequest {
                            source: source.source,
                            writer: if cancel {
                                SealWriter::Owner {
                                    opener: source.owner,
                                }
                            } else {
                                SealWriter::Process {
                                    process_id: process,
                                }
                            },
                            seal,
                        }),
                    )
                    .await
            })
        };
        wait_until(&server, || {
            server
                .invocations()
                .iter()
                .any(|view| view.target.ends_with("/seal_source") && view.status != "completed")
        })
        .await;
        held.release();
        resolving
            .await
            .expect("resolver task")
            .expect("one terminal");
    }
    assert_eq!(
        predecessor.await.expect("task").expect("predecessor"),
        WaitEnd::HandedOver
    );
    if crash.is_some() {
        let subscribers = read().subscribers;
        assert_eq!(subscribers.len(), 1, "only the predecessor read retired");
        assert_eq!(subscribers[0].segment, SegmentOrdinal(1));
        assert_eq!(count.get(), 1, "the chosen retirement crash fired once");
        assert!(!successor.is_finished());
        server.settle().await;
        server
            .remove_deployment(&old, false)
            .expect("N drains before the external source resolves");
        let _: crate::Reply<RestateSourceSealReply> = ingress
            .call_object_json(
                INDEX,
                &address.index_key(),
                "seal_source",
                &crate::Call::new(RestateSourceSealRequest {
                    source: source.source,
                    writer: SealWriter::Process {
                        process_id: process.clone(),
                    },
                    seal: seal.clone(),
                }),
            )
            .await
            .expect("seal");
    }
    assert_eq!(
        successor.await.expect("task").expect("successor"),
        WaitEnd::Sealed(seal)
    );
    let record = stores
        .process_registry()
        .get_process(&process)
        .await
        .expect("process query")
        .expect("process row");
    assert!(
        record.outcome().is_none() && record.cancel_request.is_none(),
        "retirement and Run cancellation cannot cancel detached external work"
    );
}
