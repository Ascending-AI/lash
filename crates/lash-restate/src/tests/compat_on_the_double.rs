//! ADR 0115 §3 on the server double: every lash handler takes a versioned
//! [`Call`](crate::Call) and answers a [`Reply`](crate::Reply), every object
//! family's `_compat` record admits or refuses a build before any other
//! state is read, a drive pinned to one build hands its root to the newer
//! build without a drain gate, and a build never registers over another
//! build's endpoint.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use lash_sansio::TurnId;

use lash_restate_test::{CrashPoint, CrashRule, RestateTestServer, ServerConfig};

use super::bindings::{backend_and_process_worker, bindings_generation, discovery_document};
use super::session_drive_roll_on_the_double::{
    BUILD_N_URI, SessionRoll, generation, stable_session,
};
use super::test_restate_authority_id;
use crate::compat::{COMPAT_KEY, Call, ObjectCompat, RESTATE_WIRE, Reply, VersionRange};
use crate::durable_wait::{
    LashDurableWaitRegistry as _, RestateDurableWaitEffectRequest, RestateDurableWaitIndexRequest,
    RestateDurableWaitRegistration, RestateTurnGatePeek,
};
use crate::effect_group::{
    EffectGroupAdmissionResponse, EffectGroupPayload as _, EffectGroupPayloadGetResponse,
    EffectGroupPayloadImpl, EffectGroupPayloadPutRequest, EffectGroupPayloadPutResponse,
    EffectGroupProbeResponse, EffectGroupState as _, EffectGroupStateImpl,
};
use crate::object_state::StampedValue;
use crate::services::LaneClass;
use crate::wire::{CALL_SCHEMA_TITLE, REPLY_SCHEMA_TITLE, RestateCompatError};
use crate::{LASH_TURN_OUTCOME_FORMAT_VERSION, RestateRegistrationError};
use lash_core::engine::DriveStop;
use lash_core_store::compat::CompatRefusal;

/// The done-when enumeration: read from the discovery document of an
/// endpoint `endpoint_builder` bound — what Restate registers — every
/// handler of every lash service under every lane, and require that each
/// takes a `Call` and answers a `Reply`.
#[tokio::test]
async fn every_handler_the_binder_binds_takes_a_call_and_answers_a_reply() {
    let (backend, worker) = backend_and_process_worker().await;
    let endpoint = backend.endpoint_builder(worker).build();
    let document = discovery_document(&endpoint).await;
    let namespace = crate::RestateNamespace::default();
    let mut lanes = BTreeSet::new();
    let mut handlers = 0;
    for service in document["services"].as_array().expect("services") {
        let name = service["name"].as_str().expect("a service name");
        let Some(route) = namespace.parse(name) else {
            continue;
        };
        lanes.insert(route.name().into_owned());
        for handler in service["handlers"].as_array().expect("handlers") {
            let handler_name = handler["name"].as_str().expect("a handler name");
            assert_eq!(
                handler["input"]["jsonSchema"]["title"], CALL_SCHEMA_TITLE,
                "{name}/{handler_name} takes a Call: {handler}"
            );
            assert_eq!(
                handler["output"]["jsonSchema"]["title"], REPLY_SCHEMA_TITLE,
                "{name}/{handler_name} answers a Reply: {handler}"
            );
            handlers += 1;
        }
    }
    let expected = crate::services::LASH_SERVICES
        .iter()
        .flat_map(|&service| {
            let mut names = vec![namespace.stable(service).name().into_owned()];
            if service.lane_class() == LaneClass::Pinned {
                names.push(
                    namespace
                        .generation(service, bindings_generation())
                        .name()
                        .into_owned(),
                );
            }
            names
        })
        .collect::<BTreeSet<_>>();
    assert_eq!(lanes, expected, "every lash lane was enumerated");
    assert!(
        handlers > lanes.len(),
        "the enumeration read the handlers, {handlers} of them"
    );
}

/// The three object families, bound alone on a double: they call no other
/// service on the paths these tests take.
async fn object_families() -> (RestateTestServer, crate::RestateIngressClient) {
    let server = RestateTestServer::new(ServerConfig::default().with_seed(0x4048_0001))
        .expect("start the server double");
    let endpoint = restate_sdk::endpoint::Endpoint::builder()
        .bind(EffectGroupStateImpl::default().serve())
        .bind(EffectGroupPayloadImpl::default().serve())
        .bind(crate::LashDurableWaitRegistryImpl::default().serve())
        .build();
    server
        .register(endpoint)
        .await
        .expect("register the object families");
    let ingress = crate::RestateIngressClient::new(crate::RestateConnection::with_transport(
        server.ingress_url(),
        server.transport(),
    ));
    (server, ingress)
}

/// The typed compatibility refusal a failed call's response carries.
fn compat_refusal(error: &crate::RestateHttpError) -> RestateCompatError {
    let crate::RestateHttpError::Status { body, .. } = error else {
        panic!("the call was refused by the handler, not the transport: {error}");
    };
    let message = serde_json::from_str::<serde_json::Value>(body)
        .ok()
        .and_then(|value| value["message"].as_str().map(str::to_owned))
        .unwrap_or_else(|| body.clone());
    crate::wire::restate_compat_error_in(&message)
        .unwrap_or_else(|| panic!("the refusal is typed: {message}"))
}

fn compat_bytes(format: u32, min_reader: u32, min_writer: u32) -> Vec<u8> {
    serde_json::to_vec(&ObjectCompat {
        format,
        min_reader,
        min_writer,
    })
    .expect("encode a _compat record")
}

fn stamped(body: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec(&StampedValue { format: 1, body }).expect("encode a stamped value")
}

/// One object family under test: its service, a value key it keeps, a
/// mutating exclusive call that succeeds on a fresh object, and a shared
/// call when it has one.
struct Family {
    service: &'static str,
    component: &'static str,
    value_key: &'static str,
    exclusive: (&'static str, serde_json::Value),
    shared: Option<(&'static str, serde_json::Value)>,
}

fn families() -> [Family; 3] {
    [
        Family {
            service: "EffectGroupIndex",
            component: "restate-effect-group-state",
            value_key: "effect-group/v1/state",
            exclusive: ("finish_retirement", serde_json::Value::Null),
            shared: Some(("probe", serde_json::Value::Null)),
        },
        Family {
            service: "EffectGroupPayload",
            component: "restate-effect-group-payload",
            value_key: "effect-group/v1/payload",
            exclusive: ("delete_bytes", serde_json::Value::Null),
            shared: Some(("get", serde_json::Value::Null)),
        },
        Family {
            service: "LashDurableWaitIndex",
            component: "restate-durable-wait-registry",
            value_key: "wait-index/v2/effect/replay",
            exclusive: ("reinstate", serde_json::Value::Null),
            shared: None,
        },
    ]
}

async fn call(
    ingress: &crate::RestateIngressClient,
    service: &str,
    key: &str,
    handler: &str,
    body: &serde_json::Value,
) -> Result<serde_json::Value, Box<crate::RestateHttpError>> {
    ingress
        .call_lash_object::<_, serde_json::Value>(service, key, handler, body)
        .await
        .map_err(Box::new)
}

/// Per family: a `_compat` record whose floors are above this build refuses
/// every handler it must, typed, and changes nothing; a populated object
/// without one refuses as `Unstamped`; a fresh object is stamped by its first
/// exclusive handler.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_family_refuses_a_compat_record_above_this_build_with_zero_state_change() {
    let (server, ingress) = object_families().await;
    let reads = VersionRange::exactly(1);
    for family in families() {
        let service = family.service;
        let component = family.component.to_owned();
        let (exclusive, exclusive_body) = &family.exclusive;

        // A fresh object: the first exclusive handler stamps `_compat` at
        // the format the fleet selects.
        let fresh = format!("{service}-fresh");
        call(&ingress, service, &fresh, exclusive, exclusive_body)
            .await
            .unwrap_or_else(|error| panic!("{service}/{exclusive} on a fresh object: {error}"));
        assert_eq!(
            server.object_state(service, &fresh).get(COMPAT_KEY),
            Some(&compat_bytes(1, 1, 1)),
            "{service}: a fresh object is stamped by its first exclusive handler"
        );

        // A newer release raised both floors: this build neither reads nor
        // mutates the object.
        let raised = format!("{service}-raised");
        server.set_object_state(
            service,
            &raised,
            [
                (COMPAT_KEY.to_owned(), compat_bytes(2, 2, 2)),
                (
                    family.value_key.to_owned(),
                    stamped(serde_json::json!(true)),
                ),
            ]
            .into_iter()
            .collect(),
        );
        let before = server.object_state(service, &raised);
        let error = call(&ingress, service, &raised, exclusive, exclusive_body)
            .await
            .expect_err("a raised reader floor refuses the exclusive handler");
        assert_eq!(
            compat_refusal(&error),
            RestateCompatError::Incompatible {
                refusal: CompatRefusal::ReaderFloorAbove {
                    component: component.clone(),
                    found: 2,
                    min_reader: 2,
                    reads,
                },
            },
            "{service}/{exclusive}"
        );
        if let Some((shared, shared_body)) = &family.shared {
            let error = call(&ingress, service, &raised, shared, shared_body)
                .await
                .expect_err("a raised reader floor refuses the shared handler");
            assert!(
                matches!(
                    compat_refusal(&error),
                    RestateCompatError::Incompatible {
                        refusal: CompatRefusal::ReaderFloorAbove { .. }
                    }
                ),
                "{service}/{shared}"
            );
        }
        assert_eq!(
            server.object_state(service, &raised),
            before,
            "{service}: a refused call changed nothing"
        );

        // Only the writer floor moved: a shared handler still reads, an
        // exclusive one is refused.
        let upgraded = format!("{service}-upgraded");
        server.set_object_state(
            service,
            &upgraded,
            [(COMPAT_KEY.to_owned(), compat_bytes(1, 1, 2))]
                .into_iter()
                .collect(),
        );
        let before = server.object_state(service, &upgraded);
        if let Some((shared, shared_body)) = &family.shared {
            call(&ingress, service, &upgraded, shared, shared_body)
                .await
                .unwrap_or_else(|error| {
                    panic!("{service}/{shared} reads under its floor: {error}")
                });
        }
        let error = call(&ingress, service, &upgraded, exclusive, exclusive_body)
            .await
            .expect_err("a raised writer floor refuses the exclusive handler");
        assert_eq!(
            compat_refusal(&error),
            RestateCompatError::Incompatible {
                refusal: CompatRefusal::WriterFloorAbove {
                    component: component.clone(),
                    found: 1,
                    min_writer: 2,
                    writes: reads,
                },
            },
            "{service}/{exclusive}"
        );
        assert_eq!(server.object_state(service, &upgraded), before);

        // A populated object with no record predates the contract.
        let unstamped = format!("{service}-unstamped");
        server.set_object_state(
            service,
            &unstamped,
            [(
                family.value_key.to_owned(),
                stamped(serde_json::json!(true)),
            )]
            .into_iter()
            .collect(),
        );
        let before = server.object_state(service, &unstamped);
        let error = call(&ingress, service, &unstamped, exclusive, exclusive_body)
            .await
            .expect_err("an unstamped populated object is refused");
        assert_eq!(
            compat_refusal(&error),
            RestateCompatError::Incompatible {
                refusal: CompatRefusal::Unstamped { component },
            },
            "{service}/{exclusive}"
        );
        assert_eq!(server.object_state(service, &unstamped), before);
    }
}

/// Clearing an object keeps its `_compat` record: the payload's
/// `delete_bytes` and the wait index's revocation, which clears every other
/// value, leave it, so a stale handler cannot recreate the state.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn clearing_an_object_keeps_its_compat_record() {
    let (server, ingress) = object_families().await;
    let put = ingress
        .call_lash_object::<_, EffectGroupPayloadPutResponse>(
            "EffectGroupPayload",
            "cleared",
            "put",
            &EffectGroupPayloadPutRequest {
                bytes: b"answer".to_vec(),
            },
        )
        .await
        .expect("put the payload");
    assert_eq!(put, EffectGroupPayloadPutResponse::Written);
    ingress
        .call_lash_object::<_, ()>("EffectGroupPayload", "cleared", "delete_bytes", &())
        .await
        .expect("delete the bytes");
    assert_eq!(
        server.object_state("EffectGroupPayload", "cleared"),
        [(COMPAT_KEY.to_owned(), compat_bytes(1, 1, 1))]
            .into_iter()
            .collect(),
        "only the record survives the clear"
    );
    let got = ingress
        .call_lash_object::<_, EffectGroupPayloadGetResponse>(
            "EffectGroupPayload",
            "cleared",
            "get",
            &(),
        )
        .await
        .expect("read the cleared payload");
    assert_eq!(got, EffectGroupPayloadGetResponse::Missing);

    let begun = ingress
        .call_lash_object::<_, bool>(
            "LashDurableWaitIndex",
            "revoked",
            "begin_effect",
            &RestateDurableWaitEffectRequest {
                replay_key: "replay".to_owned(),
            },
        )
        .await
        .expect("record an effect");
    assert!(begun);
    ingress
        .call_lash_object::<_, ()>("LashDurableWaitIndex", "revoked", "revoke_all", &())
        .await
        .expect("revoke the index");
    let state = server.object_state("LashDurableWaitIndex", "revoked");
    assert_eq!(
        state.get(COMPAT_KEY),
        Some(&compat_bytes(1, 1, 1)),
        "the revocation's clear kept the record: {state:?}"
    );
    assert!(
        !state.contains_key("wait-index/v2/effect/replay"),
        "the revocation cleared every other value: {state:?}"
    );
}

/// A call whose range holds no version this build answers is refused typed,
/// carrying both ranges, before any state is read or written.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_disjoint_wire_is_refused_typed_before_any_state() {
    let (server, ingress) = object_families().await;
    let peer = VersionRange::new(RESTATE_WIRE.max() + 1, RESTATE_WIRE.max() + 2).expect("range");
    let error = ingress
        .call_object_json::<_, Reply<EffectGroupAdmissionResponse>>(
            "EffectGroupIndex",
            "disjoint",
            "admit_child",
            &Call {
                wire: peer,
                body: serde_json::json!({ "a": "shape this build has never seen" }),
            },
        )
        .await
        .expect_err("a disjoint wire is refused");
    assert_eq!(
        compat_refusal(&error),
        RestateCompatError::WireUnsupported {
            local: RESTATE_WIRE,
            peer,
        }
    );
    assert!(
        server
            .object_state("EffectGroupIndex", "disjoint")
            .is_empty(),
        "nothing was written, not even a _compat record"
    );
    let probe = ingress
        .call_object_json::<_, Reply<EffectGroupProbeResponse>>(
            "EffectGroupIndex",
            "disjoint",
            "probe",
            &Call::new(()),
        )
        .await
        .expect("an overlapping wire is answered");
    assert_eq!(
        probe.wire,
        RESTATE_WIRE.max(),
        "the reply is at the selected wire"
    );
    assert_eq!(probe.body, EffectGroupProbeResponse::Absent);
}

/// ADR 0115 §3.1: a drive pinned to build N sends its admitted root to the
/// stable `LashTurn`, which build N+1 serves once it registers. The root
/// runs there, once, with no refusal and no `SubstrateLost`: the request
/// carries no drive stamp for N+1 to gate on. The outcome N+1 records is
/// stamped (§3.4), and a request an older build stamped is still served.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn pinned_older_drive_root_runs_on_the_newer_build() {
    let gn = generation("N");
    let roll = SessionRoll::start(0x4048_0002).await;
    let session = lash_sansio::SessionId::from("compat-pinned");
    roll.driver.accept(&session, "p1");
    let gate = roll.driver.gate("r-pinned", 0);
    roll.send(&session, "r-pinned", &gn, &stable_session())
        .await;
    tokio::time::timeout(Duration::from_secs(60), gate.reached.notified())
        .await
        .expect("the drive reached its gated admission on build N");
    roll.register_next().await;
    gate.release.notify_one();
    let outcome = roll
        .attach(&session, "r-pinned", &gn, &stable_session())
        .await;
    assert_eq!(
        outcome.ran.len(),
        1,
        "the pinned drive ran its root: {outcome:?}"
    );
    assert!(
        matches!(
            outcome.ran[0],
            lash_core::engine::RootOutcome::Committed { .. }
        ),
        "the root committed, neither released nor lost: {outcome:?}"
    );
    assert!(
        !matches!(outcome.stop, DriveStop::SubstrateLost { .. }),
        "{outcome:?}"
    );
    let drive = roll.invocations_of(&format!("LashSession/{session}/drive"));
    assert_eq!(drive.len(), 1);
    assert_eq!(
        drive[0].pinned_deployment_id,
        roll.deployment_n.as_str(),
        "the drive stayed pinned to build N"
    );
    let key = crate::session_driver::turn_workflow_key(&session, &TurnId::from("p1"));
    let turn = roll.invocations_of(&format!("LashTurn/{key}/run"));
    assert_eq!(turn.len(), 1, "the root ran once");
    assert_eq!(
        turn[0].pinned_deployment_id,
        roll.deployment_next().as_str(),
        "the root's LashTurn ran on the newer build"
    );
    assert_eq!(roll.driver.runs_of("p1"), 1);
    assert_eq!(roll.driver.ledger(&session).consumed, ["p1"]);

    // The recorded outcome is stamped at its format, and `outcome` reads it
    // back through the stamp.
    let state = roll.server.object_state("LashTurn", &key);
    let recorded: serde_json::Value =
        serde_json::from_slice(state.get("outcome").expect("the outcome is recorded"))
            .expect("the outcome is JSON");
    assert_eq!(recorded["format"], LASH_TURN_OUTCOME_FORMAT_VERSION);
    let read = roll
        .ingress
        .call_lash_workflow::<_, Option<lash_core::engine::RootOutcome>>(
            "LashTurn",
            &key,
            "outcome",
            &(),
        )
        .await
        .expect("read the recorded outcome");
    assert_eq!(read.as_ref(), Some(&outcome.ran[0]));

    // A request an older build stamped with its drive version is served by
    // the newer build: the stamp is never a gate.
    let session_stamped = lash_sansio::SessionId::from("compat-stamped");
    roll.driver.accept(&session_stamped, "s1");
    let mut body =
        serde_json::to_value(SessionRoll::drive_body(&session_stamped, "r-stamped", &gn))
            .expect("encode the drive");
    body["drive_version"] = serde_json::json!(crate::LASH_SESSION_DRIVE_VERSION + 7);
    let stamped = roll
        .ingress
        .call_lash_object::<_, lash_core::engine::DriveOutcome>(
            "LashSession",
            session_stamped.as_str(),
            "drive",
            &body,
        )
        .await
        .expect("a stamped request is served");
    assert_eq!(stamped.ran.len(), 1, "{stamped:?}");
    assert_eq!(roll.driver.runs_of("s1"), 1);
    roll.settle().await;
}

/// ADR 0115 §3.5: a build registers only at an endpoint URI no other
/// build's deployment holds. Over build N's URI a build of N+1 is refused,
/// typed and with nothing registered; at a fresh URI it registers without
/// force; build N coming back to its own URI re-registers with force.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_refuses_an_endpoint_serving_another_generation() {
    let roll = SessionRoll::start(0x4048_0003).await;
    let server = &roll.server;
    let engine = move |build: &'static str| async move {
        let stores = lash_sqlite_store::SqliteStoreSet::memory()
            .await
            .expect("open a memory store set");
        let connection =
            crate::RestateConnection::with_transport(server.ingress_url(), server.transport());
        crate::RestateEngine::new(
            Arc::new(stores) as Arc<dyn lash_core::StoreSet>,
            crate::RestateConfig::new(
                connection.clone(),
                connection,
                test_restate_authority_id(),
                generation(build),
            ),
        )
    };
    let next = engine("N+1").await;

    let refused = next
        .register_deployment(&format!("{BUILD_N_URI}/"))
        .await
        .expect_err("build N's endpoint is not N+1's to take");
    match refused {
        RestateRegistrationError::EndpointServesAnotherGeneration { uri, held, local } => {
            assert_eq!(uri, format!("{BUILD_N_URI}/"));
            assert_eq!(held, Some(generation("N")));
            assert_eq!(local, generation("N+1"));
        }
        other => panic!("the refusal is typed: {other}"),
    }
    assert!(
        roll.server.registration_requests().is_empty(),
        "a refused registration sent nothing"
    );

    let fresh = "http://lash-roll.test/lash/n1/build-n1";
    next.register_deployment(fresh)
        .await
        .expect("a fresh URI registers");
    let current = engine("N").await;
    current
        .register_deployment(BUILD_N_URI)
        .await
        .expect("build N redeploys over its own URI");
    assert_eq!(
        roll.server.registration_requests(),
        [
            serde_json::json!({ "uri": fresh, "force": false }),
            serde_json::json!({ "uri": BUILD_N_URI, "force": true }),
        ],
        "a fresh URI is never forced; a redeploy of the same build is"
    );
}

/// ADR 0115 §3.5 with ADR 0111 §3: a deployment in another namespace is
/// another deployment, so build N in namespace `beta` is refused build N's
/// default-namespace URI even at the same generation. The URI serves no
/// generation lane of `beta`, so the refusal holds none (`held: None`). At
/// a URI of its own `beta` registers beside it, unforced.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn registration_refuses_an_endpoint_serving_another_namespace() {
    let roll = SessionRoll::start(0x4048_0004).await;
    let server = &roll.server;
    let stores = lash_sqlite_store::SqliteStoreSet::memory()
        .await
        .expect("open a memory store set");
    let connection =
        crate::RestateConnection::with_transport(server.ingress_url(), server.transport());
    let beta = crate::RestateEngine::new(
        Arc::new(stores) as Arc<dyn lash_core::StoreSet>,
        crate::RestateConfig::new(
            connection.clone(),
            connection,
            test_restate_authority_id(),
            generation("N"),
        )
        .with_namespace(crate::RestateNamespace::new("beta").expect("a valid namespace")),
    );

    let refused = beta
        .register_deployment(BUILD_N_URI)
        .await
        .expect_err("the default namespace's endpoint is not beta's to take");
    match refused {
        RestateRegistrationError::EndpointServesAnotherGeneration { uri, held, local } => {
            assert_eq!(uri, BUILD_N_URI);
            assert_eq!(held, None, "the URI serves no generation lane of beta");
            assert_eq!(local, generation("N"));
        }
        other => panic!("the refusal is typed: {other}"),
    }
    assert!(
        roll.server.registration_requests().is_empty(),
        "a refused registration sent nothing"
    );

    let own = format!("{BUILD_N_URI}/ns/beta");
    beta.register_deployment(&own)
        .await
        .expect("beta registers at a URI of its own");
    assert_eq!(
        roll.server.registration_requests(),
        [serde_json::json!({ "uri": own, "force": false })],
        "a URI of beta's own is registered unforced"
    );
}

/// ADR 0115 §3.2 under a concurrent writer: a shared handler admits an
/// object `Unstamped` only when one read shows state and no `_compat`
/// record. Two reads are two views: an eager read is journaled with its
/// value, so an attempt that replays the shared `peek_turn_gate` answers its
/// first read from the attempt that recorded it and its later reads from its
/// own, newer snapshot. Here every attempt of the peek crashes just before
/// its second state read is stored while an exclusive `register` stamps the
/// fresh session index and writes its first rows, and only then may an
/// attempt go past it. The peek must read the stamped index, never refuse it.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_shared_read_never_refuses_an_index_its_writer_stamps_concurrently() {
    let (server, ingress) = object_families().await;
    let session = lash_sansio::SessionId::from("compat-concurrent-stamp");
    let scope = lash_core::ExecutionScope::turn(session.clone(), TurnId::from("turn-1"));
    let gate = crate::durable_wait::restate_await_event_key(
        &scope,
        lash_core::AwaitEventWaitIdentity::TurnCancelGate,
    )
    .expect("derive the turn's cancellation gate");
    let object = session.as_str().to_owned();
    assert!(
        server
            .object_state("LashDurableWaitIndex", &object)
            .is_empty()
    );

    // Command 0 is the input and command 1 the peek's first state read: the
    // crash drops each attempt before its second read is stored.
    server.crash_on(
        CrashRule::new(CrashPoint::BeforeCommand { index: 2 })
            .service("LashDurableWaitIndex")
            .handler("peek_turn_gate")
            .key(object.clone())
            .times(u32::MAX),
    );
    let peek = tokio::spawn({
        let ingress = ingress.clone();
        let object = object.clone();
        let request = RestateDurableWaitIndexRequest { key: gate.clone() };
        async move {
            ingress
                .call_lash_object::<_, RestateTurnGatePeek>(
                    "LashDurableWaitIndex",
                    &object,
                    "peek_turn_gate",
                    &request,
                )
                .await
        }
    });
    tokio::time::timeout(Duration::from_secs(30), async {
        while server.stats().crashes == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .expect("the peek recorded its first read over the fresh index");

    let registered = ingress
        .call_lash_object::<_, RestateDurableWaitRegistration>(
            "LashDurableWaitIndex",
            &object,
            "register",
            &RestateDurableWaitIndexRequest { key: gate.clone() },
        )
        .await
        .expect("register the gate on the fresh index");
    assert_eq!(registered, RestateDurableWaitRegistration::Registered);
    let state = server.object_state("LashDurableWaitIndex", &object);
    assert_eq!(
        state.get(COMPAT_KEY),
        Some(&compat_bytes(1, 1, 1)),
        "the register stamped the index it populated: {state:?}"
    );
    assert!(state.keys().any(|key| key != COMPAT_KEY), "{state:?}");

    server.clear_crashes();
    let peeked = tokio::time::timeout(Duration::from_secs(60), peek)
        .await
        .expect("the peek answers")
        .expect("the peek task")
        .unwrap_or_else(|error| {
            panic!(
                "the shared read refused an index its writer stamped: {:?}",
                compat_refusal(&error)
            )
        });
    assert_eq!(peeked, RestateTurnGatePeek::Open(None));
    assert!(
        server.stats().crashes > 0,
        "the peek replayed across the register"
    );
}
