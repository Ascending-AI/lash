//! L07/L13: K4 owns process results and their leases after physical waits end.

use super::*;
use crate::durable_wait::{
    ProcessTerminalDelivery, ProcessTerminalSubscription, RestateSourceSealReply,
    RestateSourceSealRequest,
};
use lash_core::tool_run::{
    ExternalCancelPolicy, MaterialHolder, MaterialOwner, SealOutcome, SealWriter, SourceAuthority,
    SourceDescriptor, SourceSeal,
};
use lash_core::{
    ArtifactReferrer, AttachmentStore as _, ProcessQuery as _, ProcessRegistrar as _, ReferrerClaim,
};
use lash_restate_test::{RestateTestServer, ServerConfig};

const INDEX: &str = "LashDurableWaitIndex";

struct World {
    server: RestateTestServer,
    ingress: RestateIngressClient,
    stores: lash_sqlite_store::SqliteStoreSet,
    subscription: ProcessTerminalSubscription,
}

impl World {
    async fn new(seed: u64) -> Self {
        let server =
            RestateTestServer::new(ServerConfig::default().with_seed(seed)).expect("server double");
        let connection =
            RestateConnection::with_transport(server.ingress_url(), server.transport());
        let stores = lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("SQLite");
        server
            .register(super::source_wait_transfer::endpoint(&connection, &stores, "N").await)
            .await
            .expect("endpoint");
        let process = stores
            .process_registry()
            .register_process(external_registration())
            .await
            .expect("external process")
            .id;
        let session = SessionId::fixture(format!("process-source-{seed}"));
        let run = TurnId::fixture("run");
        let owner = lash_core::EffectOpener::turn(session.clone(), run.clone());
        let call_id = lash_core::ToolCallId::fixture("process-await");
        let source = test_restate_await_event_key(
            &ExecutionScope::turn(session, run),
            AwaitEventWaitIdentity::tool_completion(call_id.clone()),
        )
        .expect("source");
        let subscription = ProcessTerminalSubscription::for_source(SourceDescriptor {
            source,
            owner,
            call_id,
            resolver: lash_core::plugin::PluginRevision::new(
                "tools",
                lash_core::plugin::BehaviorRevision::ONE,
            ),
            authority: SourceAuthority::ProcessTerminal {
                process_id: process,
            },
            cancel: ExternalCancelPolicy::Ignore,
        })
        .expect("process terminal subscription");
        Self {
            server,
            ingress: RestateIngressClient::new(connection),
            stores,
            subscription,
        }
    }

    async fn call<T: Serialize, R: DeserializeOwned>(
        &self,
        key: &AwaitEventKey,
        handler: &str,
        body: T,
    ) -> R {
        self.ingress
            .call_object_json::<_, crate::Reply<R>>(
                INDEX,
                &crate::RestateDurableWaitAddress::for_key(key).index_key(),
                handler,
                &crate::Call::new(body),
            )
            .await
            .unwrap_or_else(|error| panic!("{handler}: {error}"))
            .into_body()
    }

    async fn attach(&self) -> bool {
        let attached = self
            .call(
                &self.subscription.receiver,
                "attach_process_terminal",
                self.subscription.clone(),
            )
            .await;
        if attached {
            let output: Option<ProcessAwaitOutput> = self
                .call(
                    &self.subscription.terminal,
                    "subscribe_process_terminal",
                    self.subscription.clone(),
                )
                .await;
            if let Some(output) = output {
                self.deliver(output).await;
            }
        }
        attached
    }

    async fn deliver(&self, output: ProcessAwaitOutput) {
        self.call::<_, ()>(
            &self.subscription.receiver,
            "deliver_process_terminal",
            ProcessTerminalDelivery {
                subscription: self.subscription.clone(),
                output,
            },
        )
        .await;
    }

    async fn cancel(&self) -> SourceSeal {
        let response: RestateSourceSealReply = self
            .call(
                &self.subscription.receiver,
                "seal_source",
                RestateSourceSealRequest {
                    source: self.subscription.receiver.clone(),
                    writer: SealWriter::Owner {
                        opener: self.subscription.descriptor.owner.clone(),
                    },
                    seal: SourceSeal::Cancelled,
                },
            )
            .await;
        match response {
            RestateSourceSealReply::Outcome {
                outcome: SealOutcome::Sealed { seal } | SealOutcome::AlreadySealed { seal },
            } => seal,
            response => panic!("cancel: {response:?}"),
        }
    }

    fn source_seal(&self) -> Option<SourceSeal> {
        let address = crate::RestateDurableWaitAddress::for_key(&self.subscription.receiver);
        self.server
            .object_state(INDEX, &address.index_key())
            .get(&format!("wait-index/v2/source/{}", address.workflow_key))
            .and_then(|bytes| {
                let row: serde_json::Value = serde_json::from_slice(bytes).expect("source row");
                row["body"]
                    .get("seal")
                    .cloned()
                    .map(|seal| serde_json::from_value(seal).expect("seal"))
            })
    }

    fn receivers(&self) -> usize {
        let address = crate::RestateDurableWaitAddress::for_key(&self.subscription.terminal);
        self.server
            .object_state(INDEX, &address.index_key())
            .get(crate::durable_wait::DURABLE_WAIT_INDEX_METADATA_KEY)
            .map_or(0, |bytes| {
                let row: serde_json::Value = serde_json::from_slice(bytes).expect("metadata");
                row["body"]["process_receivers"]
                    .as_array()
                    .map_or(0, Vec::len)
            })
    }

    async fn assert_process_untouched(&self) {
        let SourceAuthority::ProcessTerminal { process_id } =
            &self.subscription.descriptor.authority
        else {
            unreachable!()
        };
        let record = self
            .stores
            .process_registry()
            .get_process(process_id)
            .await
            .expect("process read")
            .expect("process retained");
        assert!(record.outcome().is_none() && record.cancel_request.is_none());
        assert!(
            !self
                .server
                .invocations()
                .iter()
                .any(|view| view.target.starts_with("LashProcessAttach/")
                    || view.target.ends_with("/await_terminal")),
            "short subscriptions create no attach or terminal read"
        );
    }
}

/// L07: cancelling the source ends its process subscription, and a late
/// terminal cannot acquire or publish a result for that cancelled source.
#[tokio::test]
async fn a_cancelled_source_detaches_without_ending_the_process_or_accepting_a_late_result() {
    let world = World::new(0x4891_0002).await;
    assert!(world.attach().await);
    assert_eq!(world.receivers(), 1);
    assert_eq!(world.cancel().await, SourceSeal::Cancelled);
    world.server.settle().await;
    assert_eq!(
        world.receivers(),
        0,
        "the cancelled source leaves no process subscription"
    );
    world
        .deliver(process_success(serde_json::json!("late")))
        .await;
    assert_eq!(world.source_seal(), Some(SourceSeal::Cancelled));
    assert!(
        !world.attach().await,
        "late attach cannot revive the cancelled source"
    );
    world.assert_process_untouched().await;
}

/// L13: the source acquires attachment leases before its seal is published.
/// Ending the producer cannot collect a result a successor has yet to read.
#[tokio::test]
async fn a_process_result_is_held_by_its_source_until_the_logical_run_retires() {
    let world = World::new(0x4891_0003).await;
    assert!(world.attach().await);
    let stored = world
        .stores
        .attachment_store()
        .put(
            b"source-owned attachment".to_vec(),
            lash_core::AttachmentCreateMeta::new(
                lash_core::MediaType::parse("text/plain").expect("media type"),
                None,
                None,
            ),
        )
        .await
        .expect("attachment bytes");
    let id = stored.id.clone();
    let SourceAuthority::ProcessTerminal { process_id } = &world.subscription.descriptor.authority
    else {
        unreachable!()
    };
    let producer = ArtifactReferrer::ProcessRecord(process_id.clone());
    let write = lash_core::AttachmentWrite {
        attachment_id: id.clone(),
        claim: ReferrerClaim::unguarded(producer.clone()).expect("producer claim"),
    };
    let refs = world.stores.attachment_referrers();
    let lash_core::AttachmentWriteFence::Granted(permit) = refs
        .begin_attachment_write(&write)
        .await
        .expect("begin upload")
    else {
        panic!("producer is live")
    };
    refs.complete_attachment_write(&write, permit)
        .await
        .expect("upload evidence");
    let output =
        ProcessAwaitOutput::from_tool_output(lash_core::ToolCallOutput::success_tool_value(
            lash_core::ToolValue::Attachment(lash_core::AttachmentSource::stored(stored)),
        ));
    world.deliver(output.clone()).await;
    world.server.settle().await;
    assert_eq!(
        world.receivers(),
        0,
        "a resolved source ends its process subscription"
    );
    world.assert_process_untouched().await;
    let source_holder = MaterialHolder::Source {
        source: world.subscription.receiver.clone(),
    };
    assert!(
        refs.attachment_referrers(&id)
            .await
            .expect("leases")
            .contains(&source_holder.referrer()),
        "the immutable source owns delivered attachments"
    );
    let seal = world
        .source_seal()
        .expect("source sealed before delivery returns");
    assert_eq!(
        world.cancel().await,
        seal,
        "resolve-before-cancel protects the result and its leases"
    );
    assert!(
        !world.attach().await,
        "a sealed source cannot reopen a process subscription"
    );
    refs.end_attachment_referrer(&producer)
        .await
        .expect("producer ends");
    assert_eq!(
        world
            .stores
            .attachment_store()
            .get(&id, 1024)
            .await
            .expect("the successor reads source-held bytes")
            .bytes,
        b"source-owned attachment"
    );
    let SourceSeal::Resolved { result } = seal else {
        panic!("resolved source")
    };
    let material = world.stores.tool_material_store();
    let payload = material
        .read_material(
            &source_holder,
            &result,
            &MaterialOwner::Source {
                source: world.subscription.receiver.clone(),
            },
            std::slice::from_ref(&world.subscription.descriptor.resolver),
        )
        .await
        .expect("the successor reads retained source material");
    let capture: lash_core::tool_dispatch::SingletonCapture =
        serde_json::from_str(&payload.text).expect("capture");
    assert!(
        matches!(capture, lash_core::tool_dispatch::SingletonCapture::Done { output: value, .. } if serde_json::from_str::<ProcessAwaitOutput>(&value).expect("terminal") == output)
    );
    world
        .call::<_, ()>(
            &world.subscription.receiver,
            "retire_run",
            crate::durable_wait::RestateDurableWaitRunRequest {
                session_id: SessionId::from(
                    world
                        .subscription
                        .receiver
                        .scope
                        .session_id()
                        .expect("session"),
                ),
                run: world
                    .subscription
                    .receiver
                    .scope
                    .turn_id()
                    .expect("run")
                    .clone(),
                committed_turn: None,
            },
        )
        .await;
    assert!(
        !refs
            .attachment_referrers(&id)
            .await
            .expect("ended leases")
            .contains(&source_holder.referrer())
    );
    assert!(
        material
            .read_material(
                &source_holder,
                &result,
                &MaterialOwner::Source {
                    source: world.subscription.receiver.clone()
                },
                std::slice::from_ref(&world.subscription.descriptor.resolver)
            )
            .await
            .is_err(),
        "retired source material cannot resurrect"
    );
}
