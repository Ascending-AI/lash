//! L07 and L12 on the source seal (K4, FIG-4883), through the real
//! `LashDurableWaitIndex` and `LashDurableWaitWorkflow` handlers served by
//! the in-process server double.
//!
//! A Deferred source ends exactly once, `Resolved(ref)` or `Cancelled`. Its
//! Run arms it, subscribes a segment with a short call and reads the seal
//! from its wake or from the subscribe reply; a cancel is the Run's own seal
//! write and its reply is the source's answer.

#![allow(clippy::disallowed_methods)]

use super::*;
use crate::durable_wait::{
    RestateDurableWaitAddress, RestateSourceArmReply, RestateSourceArmRequest,
    RestateSourceSealReply, RestateSourceSealRequest, RestateSourceSubscribeReply,
    RestateSourceSubscribeRequest,
};
use lash_core::EffectOpener;
use lash_core::tool_run::SourceRefusal;
use lash_core::tool_run::{
    ExternalCancelPolicy, MaterialDigest, MaterialLocation, MaterialOwner, MaterialRef,
    MaterialRole, SealOutcome, SealRefusal, SealWriter, SegmentOrdinal, SourceAuthority,
    SourceDescriptor, SourceSeal, SourceSubscription,
};
use lash_restate_test::protocol::MessageType;
use lash_restate_test::{CrashPoint, CrashRule, RestateTestBackend, ServerConfig};

const INDEX: &str = "LashDurableWaitIndex";

struct World {
    engine: RestateTestBackend,
    session: SessionId,
    run: TurnId,
}

impl World {
    async fn new(seed: u64) -> Self {
        Self {
            engine: lash_restate_test::backend(seed, ServerConfig::default())
                .await
                .expect("double deployment"),
            session: SessionId::fixture(format!("fig4883-{seed:x}")),
            run: TurnId::fixture("run-1"),
        }
    }

    fn owner(&self) -> EffectOpener {
        EffectOpener::turn(self.session.clone(), self.run.clone())
    }

    /// The descriptor of call `label`'s source, resolved by the external
    /// completer of its key.
    fn source(&self, label: &str) -> SourceDescriptor {
        self.source_by(label, SourceAuthority::ExternalCompletion)
    }

    fn source_by(&self, label: &str, authority: SourceAuthority) -> SourceDescriptor {
        let call_id = lash_core::ToolCallId::fixture(label);
        let source = restate_await_event_key(
            &ExecutionScope::turn(self.session.clone(), self.run.clone()),
            AwaitEventWaitIdentity::tool_completion(call_id.clone()),
        )
        .expect("derive the source key");
        SourceDescriptor {
            source,
            call_id,
            owner: self.owner(),
            resolver: lash_core::plugin::PluginRevision::new(
                "tools",
                lash_core::plugin::BehaviorRevision::ONE,
            ),
            authority,
            cancel: ExternalCancelPolicy::Ignore,
        }
    }

    async fn call<T: Serialize, R: DeserializeOwned>(&self, handler: &str, body: T) -> R {
        let reply: crate::Reply<R> = self
            .engine
            .ingress()
            .call_object_json(
                INDEX,
                self.session.as_str(),
                handler,
                &crate::Call::new(body),
            )
            .await
            .unwrap_or_else(|error| panic!("{INDEX}/{handler}: {error}"));
        reply.into_body()
    }

    async fn arm(&self, descriptor: &SourceDescriptor) -> RestateSourceArmReply {
        self.call(
            "arm_source",
            RestateSourceArmRequest {
                descriptor: descriptor.clone(),
            },
        )
        .await
    }

    async fn subscribe(
        &self,
        descriptor: &SourceDescriptor,
        segment: u32,
        awakeable_id: &str,
    ) -> RestateSourceSubscribeReply {
        self.call(
            "subscribe_source",
            subscription(descriptor, &descriptor.owner, segment, awakeable_id),
        )
        .await
    }

    async fn seal(
        &self,
        descriptor: &SourceDescriptor,
        writer: SealWriter,
        seal: SourceSeal,
    ) -> RestateSourceSealReply {
        self.call(
            "seal_source",
            RestateSourceSealRequest {
                source: descriptor.source.clone(),
                writer,
                seal,
            },
        )
        .await
    }

    /// The Run's cancel: its reply is the seal the Run accepts.
    async fn cancel(&self, descriptor: &SourceDescriptor) -> RestateSourceSealReply {
        self.seal(
            descriptor,
            SealWriter::Owner {
                opener: self.owner(),
            },
            SourceSeal::Cancelled,
        )
        .await
    }

    /// Every seal the index woke `awakeable_id` with, in journal order.
    fn wakes(&self, awakeable_id: &str) -> Vec<SourceSeal> {
        let server = self.engine.server();
        server
            .invocations()
            .into_iter()
            .filter(|view| view.target.starts_with(&format!("{INDEX}/")))
            .flat_map(|view| server.journal(&view.id).unwrap_or_default())
            .filter_map(|entry| entry.completed_awakeable_value())
            .filter(|(id, _)| id == awakeable_id)
            .map(|(_, value)| serde_json::from_slice(&value).expect("a wake carries its seal"))
            .collect()
    }

    /// The index's row for `descriptor`'s source, if it holds one.
    fn row(&self, descriptor: &SourceDescriptor) -> Option<serde_json::Value> {
        let state_key = format!(
            "wait-index/v2/source/{}",
            RestateDurableWaitAddress::for_key(&descriptor.source).workflow_key
        );
        self.engine
            .server()
            .object_state(INDEX, self.session.as_str())
            .get(&state_key)
            .map(|row| {
                let row: serde_json::Value =
                    serde_json::from_slice(row).expect("decode the stamped row");
                row["body"].clone()
            })
    }
}

fn subscription(
    descriptor: &SourceDescriptor,
    owner: &EffectOpener,
    segment: u32,
    awakeable_id: &str,
) -> RestateSourceSubscribeRequest {
    RestateSourceSubscribeRequest {
        subscription: SourceSubscription {
            source: descriptor.source.clone(),
            owner: owner.clone(),
            segment: SegmentOrdinal(segment),
        },
        awakeable_id: awakeable_id.to_owned(),
    }
}

/// The source's result, already retained under its own lease.
fn resolved(descriptor: &SourceDescriptor, bundle: &str) -> SourceSeal {
    SourceSeal::Resolved {
        result: Box::new(source_output(descriptor, bundle)),
    }
}

fn source_output(descriptor: &SourceDescriptor, bundle: &str) -> MaterialRef {
    MaterialRef {
        owner: MaterialOwner::Source {
            source: descriptor.source.clone(),
        },
        role: MaterialRole::AttemptOutput,
        location: MaterialLocation::RetainedArtifact {
            artifact: lash_core::ArtifactName {
                store: lash_core::ArtifactStoreId::ToolMaterial,
                artifact_ref: bundle.to_owned(),
            },
        },
        digest: MaterialDigest::parse(&"a".repeat(64)).expect("digest"),
    }
}

fn sealed(seal: SourceSeal) -> RestateSourceSealReply {
    RestateSourceSealReply::Outcome {
        outcome: SealOutcome::Sealed { seal },
    }
}

fn already(seal: SourceSeal) -> RestateSourceSealReply {
    RestateSourceSealReply::Outcome {
        outcome: SealOutcome::AlreadySealed { seal },
    }
}

fn refused(refusal: SourceRefusal) -> RestateSourceSealReply {
    RestateSourceSealReply::Refused { refusal }
}

/// Resolve before subscribe: an early authenticated resolution stays, and
/// the segment that subscribes later reads it from the subscribe reply.
/// Duplicate delivery, of the same result or another, reads the first seal.
#[tokio::test]
async fn an_early_resolution_answers_the_later_subscription_and_duplicates_read_it() {
    let world = World::new(0x4883_0001).await;
    let source = world.source("resolve-before-subscribe");
    let result = resolved(&source, "bundle-1");
    assert_eq!(
        world.arm(&source).await,
        RestateSourceArmReply::Armed { seal: None }
    );
    assert_eq!(
        world
            .seal(&source, SealWriter::External, result.clone())
            .await,
        sealed(result.clone())
    );
    assert_eq!(
        world
            .seal(&source, SealWriter::External, result.clone())
            .await,
        already(result.clone()),
        "a duplicate delivery reads the seal"
    );
    assert_eq!(
        world
            .seal(&source, SealWriter::External, resolved(&source, "bundle-2"))
            .await,
        already(result.clone()),
        "a second result cannot replace the first"
    );
    assert_eq!(
        world.subscribe(&source, 0, "segment-0").await,
        RestateSourceSubscribeReply::Sealed {
            seal: result.clone()
        }
    );
    assert!(
        world.wakes("segment-0").is_empty(),
        "a sealed source answers its subscriber directly"
    );
    assert_eq!(
        world.arm(&source).await,
        RestateSourceArmReply::Armed { seal: Some(result) },
        "a re-arm after a crash reads the seal"
    );
}

/// Every terminal race ends in one seal. A resolution that beat the cancel
/// is protected: the cancel's reply is that result, so the Run accepts it.
/// A cancellation that won stays: the late resolution revives nothing.
/// Each subscribed segment wakes once, with the seal that won.
#[tokio::test]
async fn resolve_and_cancel_race_to_one_seal_and_wake_each_subscriber_once() {
    let world = World::new(0x4883_0002).await;

    let protected = world.source("resolve-then-cancel");
    world.arm(&protected).await;
    assert_eq!(
        world.subscribe(&protected, 0, "protected-0").await,
        RestateSourceSubscribeReply::Subscribed
    );
    let result = resolved(&protected, "bundle-protected");
    assert_eq!(
        world
            .seal(&protected, SealWriter::External, result.clone())
            .await,
        sealed(result.clone())
    );
    assert_eq!(
        world.cancel(&protected).await,
        already(result.clone()),
        "the cancel records the resolved answer"
    );
    assert_eq!(world.wakes("protected-0"), vec![result.clone()]);

    let cancelled = world.source("cancel-then-resolve");
    world.arm(&cancelled).await;
    assert_eq!(
        world.subscribe(&cancelled, 0, "cancelled-0").await,
        RestateSourceSubscribeReply::Subscribed
    );
    assert_eq!(
        world.subscribe(&cancelled, 1, "cancelled-1").await,
        RestateSourceSubscribeReply::Subscribed,
        "a successor segment subscribes beside its predecessor"
    );
    world
        .call::<_, ()>(
            "unsubscribe_source",
            subscription(&cancelled, &cancelled.owner, 0, "cancelled-0"),
        )
        .await;
    assert_eq!(
        world.cancel(&cancelled).await,
        sealed(SourceSeal::Cancelled)
    );
    assert_eq!(
        world
            .seal(
                &cancelled,
                SealWriter::External,
                resolved(&cancelled, "late")
            )
            .await,
        already(SourceSeal::Cancelled),
        "a late resolution cannot revive cancelled work"
    );
    assert_eq!(
        world.cancel(&cancelled).await,
        already(SourceSeal::Cancelled)
    );
    assert!(
        world.wakes("cancelled-0").is_empty(),
        "an unsubscribed predecessor is not woken"
    );
    assert_eq!(world.wakes("cancelled-1"), vec![SourceSeal::Cancelled]);
    assert_eq!(
        world.row(&cancelled).expect("the sealed row")["subscribers"],
        serde_json::Value::Null,
        "a seal leaves no subscription behind"
    );
}

/// A crash after the workflow holds the seal and before any wake is stored
/// replays the recorded seal: the subscriber wakes once, and a segment that
/// lost its wake reads the seal when it subscribes again.
#[tokio::test]
async fn a_crash_between_seal_and_wake_still_wakes_once_with_the_seal() {
    let world = World::new(0x4883_0003).await;
    let source = world.source("seal-before-wake-crash");
    world.arm(&source).await;
    world.subscribe(&source, 0, "crashed-0").await;
    world.engine.server().crash_on(
        CrashRule::new(CrashPoint::BeforeFrame {
            ty: MessageType::CompleteAwakeableCommand,
        })
        .service(INDEX)
        .handler("seal_source"),
    );
    let result = resolved(&source, "bundle-crash");
    assert_eq!(
        world
            .seal(&source, SealWriter::External, result.clone())
            .await,
        sealed(result.clone())
    );
    let attempts: Vec<u32> = world
        .engine
        .server()
        .invocations()
        .into_iter()
        .filter(|view| view.target == format!("{INDEX}/{}/seal_source", world.session))
        .map(|view| view.attempts)
        .collect();
    assert_eq!(attempts, vec![2], "the seal's handler crashed once");
    assert_eq!(world.wakes("crashed-0"), vec![result.clone()]);
    assert_eq!(
        world.subscribe(&source, 1, "successor-1").await,
        RestateSourceSubscribeReply::Sealed { seal: result }
    );
}

/// Retirement: an unsealed source keeps a quiescence-proved retirement
/// open; once sealed the scope retires, and every later arm, subscribe or
/// seal of it is refused, typed. A closed run's unsealed source is sealed
/// `Cancelled` for it, its subscribers wake, and its row goes.
#[tokio::test]
async fn retirement_seals_or_waits_for_open_sources_and_later_writes_refuse() {
    let world = World::new(0x4883_0004).await;
    let source = world.source("retirement");
    world.arm(&source).await;
    let revoked: bool = world.call("revoke_all_if_quiescent", ()).await;
    assert!(!revoked, "an unsealed source keeps its scope");
    world.cancel(&source).await;
    let revoked: bool = world.call("revoke_all_if_quiescent", ()).await;
    assert!(revoked, "a sealed source does not");
    assert_eq!(
        world.arm(&world.source("after-retirement")).await,
        RestateSourceArmReply::Refused {
            refusal: SourceRefusal::Retired
        }
    );
    assert_eq!(
        world.subscribe(&source, 1, "late").await,
        RestateSourceSubscribeReply::Refused {
            refusal: SourceRefusal::Retired
        }
    );
    assert_eq!(
        world
            .seal(&source, SealWriter::External, resolved(&source, "late"))
            .await,
        refused(SourceRefusal::Retired)
    );

    let world = World::new(0x4883_0005).await;
    let open = world.source("closed-run");
    world.arm(&open).await;
    world.subscribe(&open, 0, "closed-0").await;
    world
        .call::<_, ()>(
            "retire_run",
            crate::durable_wait::RestateDurableWaitRunRequest {
                session_id: world.session.clone(),
                run: world.run.clone(),
                committed_turn: None,
            },
        )
        .await;
    assert_eq!(world.wakes("closed-0"), vec![SourceSeal::Cancelled]);
    assert!(world.row(&open).is_none(), "the closed run's row is gone");
    assert_eq!(
        world
            .seal(&open, SealWriter::External, resolved(&open, "late"))
            .await,
        refused(SourceRefusal::Retired)
    );

    assert_eq!(
        world.arm(&open).await,
        RestateSourceArmReply::Refused {
            refusal: SourceRefusal::Retired
        },
        "the retired identity cannot be re-armed"
    );

    // Recover the index at the crash cut after the workflow held the
    // winning result but before the index mirrored it. Retirement must use
    // that winning seal, even though the recovered row is still unsealed.
    let world = World::new(0x4883_0007).await;
    let source = world.source("result-before-mirror");
    world.arm(&source).await;
    let unsealed = world
        .engine
        .server()
        .object_state(INDEX, world.session.as_str());
    let result = resolved(&source, "result-before-mirror");
    world.seal(&source, SealWriter::External, result).await;
    world
        .engine
        .server()
        .set_object_state(INDEX, world.session.as_str(), unsealed);
    world
        .call::<_, ()>(
            "retire_run",
            crate::durable_wait::RestateDurableWaitRunRequest {
                session_id: world.session.clone(),
                run: world.run.clone(),
                committed_turn: None,
            },
        )
        .await;
    let state = world
        .engine
        .server()
        .object_state(INDEX, world.session.as_str());
    let fence_key = format!(
        "wait-index/v2/source-retired/{}",
        RestateDurableWaitAddress::for_key(&source.source).workflow_key
    );
    let fence: serde_json::Value = serde_json::from_slice(&state[&fence_key]).unwrap();
    assert_eq!(
        fence["body"]["terminal"], "resolved",
        "retirement retains the winning terminal kind after an unmirrored result"
    );

    let world = World::new(0x4883_0006).await;
    let open = world.source("revoked-session");
    world.arm(&open).await;
    world.subscribe(&open, 0, "revoked-0").await;
    world.call::<_, ()>("revoke_all", ()).await;
    assert_eq!(world.wakes("revoked-0"), vec![SourceSeal::Cancelled]);
}

/// L12: a write is authenticated against the descriptor its Run pinned.
/// Wrong writers, foreign or unretained results, another descriptor or
/// owner, and a source nobody armed are refused, typed, and seal nothing.
#[tokio::test]
async fn only_the_pinned_authority_and_owner_reach_a_source() {
    let world = World::new(0x4883_0007).await;
    let worker = lash_core::ProcessId::fixture("worker");
    let source = world.source_by(
        "process-source",
        SourceAuthority::ProcessTerminal {
            process_id: worker.clone(),
        },
    );
    let result = resolved(&source, "bundle-process");
    assert_eq!(
        world
            .seal(
                &source,
                SealWriter::Process {
                    process_id: worker.clone()
                },
                result.clone()
            )
            .await,
        refused(SourceRefusal::NotArmed),
        "a source nobody armed takes no seal"
    );
    world.arm(&source).await;
    assert_eq!(
        world
            .arm(&SourceDescriptor {
                cancel: ExternalCancelPolicy::CancelExternalWork,
                ..source.clone()
            })
            .await,
        RestateSourceArmReply::Refused {
            refusal: SourceRefusal::DescriptorMismatch
        }
    );
    let other_owner = EffectOpener::turn(world.session.clone(), TurnId::fixture("run-2"));
    assert_eq!(
        world
            .call::<_, RestateSourceSubscribeReply>(
                "subscribe_source",
                subscription(&source, &other_owner, 0, "foreign"),
            )
            .await,
        RestateSourceSubscribeReply::Refused {
            refusal: SourceRefusal::WrongOwner
        }
    );
    for (writer, seal, refusal) in [
        (
            SealWriter::External,
            result.clone(),
            SealRefusal::WrongAuthority,
        ),
        (
            SealWriter::Process {
                process_id: lash_core::ProcessId::fixture("impostor"),
            },
            result.clone(),
            SealRefusal::WrongAuthority,
        ),
        (
            SealWriter::Owner {
                opener: other_owner.clone(),
            },
            SourceSeal::Cancelled,
            SealRefusal::WrongAuthority,
        ),
        (
            SealWriter::Process {
                process_id: worker.clone(),
            },
            resolved(&world.source("another-source"), "bundle-foreign"),
            SealRefusal::ResultNotOwned,
        ),
        (
            SealWriter::Process {
                process_id: worker.clone(),
            },
            SourceSeal::Resolved {
                result: Box::new(MaterialRef {
                    location: MaterialLocation::JournalLocal,
                    ..source_output(&source, "unretained")
                }),
            },
            SealRefusal::UnretainedResult,
        ),
    ] {
        assert_eq!(
            world.seal(&source, writer, seal).await,
            refused(SourceRefusal::Seal { seal: refusal })
        );
    }
    assert_eq!(
        world
            .seal(
                &source,
                SealWriter::Process { process_id: worker },
                result.clone()
            )
            .await,
        sealed(result),
        "refusals sealed nothing"
    );
}

/// L02/L12: the native terminal source retains its canonical capture under
/// its own lease. Pruning the producer cannot remove or change that value.
#[tokio::test]
async fn l02_l12_process_terminal_source_keeps_material_after_producer_pruning() {
    use lash_core::tool_run::MaterialHolder;
    use lash_core::{
        ProcessLifecycle as _, ProcessQuery as _, ProcessRegistrar as _, ProcessRetention as _,
        StoreSet as _,
    };

    let world = World::new(0x1863_0012).await;
    let registry = world.engine.stores().process_registry();
    for (label, output) in [
        (
            "success",
            process_success(serde_json::json!({ "retained": [1, 2, 3] })),
        ),
        (
            "failure",
            process_failure(
                lash_core::ToolFailureClass::Execution,
                "recorded_failure",
                "the recorded failure",
                Some(serde_json::json!({ "detail": 7 })),
            ),
        ),
    ] {
        let process = registry
            .register_process(held_registration())
            .await
            .unwrap();
        registry
            .complete_process(
                &process.id,
                output.clone(),
                lash_core::ProcessCompletionAuthority::workflow_key(&process.id),
            )
            .await
            .unwrap();
        let mut source = world.source_by(
            label,
            SourceAuthority::ProcessTerminal {
                process_id: process.id.clone(),
            },
        );
        source.source =
            test_restate_await_event_key(&source.source.scope, source.source.wait.clone())
                .expect("the native producer requires an authority-bound source key");
        let subscription =
            crate::durable_wait::ProcessTerminalSubscription::for_source(source.clone()).unwrap();
        assert!(
            world
                .call::<_, bool>("attach_process_terminal", subscription.clone())
                .await
        );
        world
            .call::<_, ()>(
                "deliver_process_terminal",
                crate::durable_wait::ProcessTerminalDelivery {
                    subscription,
                    output: output.clone(),
                },
            )
            .await;
        let RestateSourceSubscribeReply::Sealed { seal } =
            world.subscribe(&source, 0, "before-prune").await
        else {
            panic!("the native terminal must seal its source");
        };
        let terminal = registry.get_process(&process.id).await.unwrap().unwrap();
        registry
            .prune_terminal_processes(
                terminal.updated_at_ms.saturating_add(1),
                None,
                lash_core::ProjectionWatermark::NoProjector,
            )
            .await
            .unwrap();
        assert!(
            matches!(
                registry.get_process(&process.id).await,
                Err(PluginError::ProcessNoLongerRetained { .. })
            ),
            "the producer was actually pruned"
        );
        assert_eq!(
            world.subscribe(&source, 1, "after-prune").await,
            RestateSourceSubscribeReply::Sealed { seal: seal.clone() }
        );
        let SourceSeal::Resolved { result } = seal else {
            panic!("the recorded terminal must resolve the source");
        };
        let material = world
            .engine
            .stores()
            .tool_material_store()
            .read_material(
                &MaterialHolder::Source {
                    source: source.source.clone(),
                },
                &result,
                &MaterialOwner::Source {
                    source: source.source.clone(),
                },
                std::slice::from_ref(&source.resolver),
            )
            .await
            .unwrap();
        let capture: lash_core::tool_dispatch::SingletonCapture =
            serde_json::from_str(&material.text).unwrap();
        let lash_core::tool_dispatch::SingletonCapture::Done {
            output: captured, ..
        } = capture
        else {
            panic!("the native source must retain a captured process outcome");
        };
        assert_eq!(
            serde_json::from_str::<ProcessAwaitOutput>(&captured).unwrap(),
            output
        );
    }
}
