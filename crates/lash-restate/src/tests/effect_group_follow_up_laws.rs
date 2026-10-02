//! FIG-4521: the last-intent/seat crash boundary and host-built controllers.

use super::effect_group_committed_recovery::HarnessStoreTier;
use super::effect_group_conformance::{HarnessServer, LiveConformanceHarness};
use super::effect_group_generation_routing::{
    RunLog, build_endpoint, build_endpoint_with_host_controller, recording,
};
use super::*;
use lash_restate_test::{AttemptDispatch, CrashPoint, CrashRule, RestateTestServer, ServerConfig};

macro_rules! store_laws {
    ($module:ident, $tier:ident $(, #[$attr:meta])?) => {
        mod $module {
            use super::*;

            $(#[$attr])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_child_crashed_after_its_last_intent_recovers_its_reserved_seat() {
                seat_recovery(HarnessStoreTier::$tier).await;
            }

            $(#[$attr])?
            #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
            async fn a_host_built_controllers_children_stay_on_its_builds_lane() {
                host_controller_lane(HarnessStoreTier::$tier).await;
            }
        }
    };
}

store_laws!(sqlite_memory, SqliteMemory);
store_laws!(sqlite_file, SqliteFile);
store_laws!(postgres, Postgres, #[ignore = "requires isolated PostgreSQL through kiln gate"]);

async fn seat_recovery(tier: HarnessStoreTier) {
    let harness =
        LiveConformanceHarness::start_for_tool_children_over(HarnessServer::in_process(), tier)
            .await;
    let fixture = harness.tool_child_law_fixture();
    let change = harness.deployment_change();
    let server = harness.server_double().expect("the server double");
    let prefix = format!("restate-seat-recovery-{}", harness.run_nonce());
    let group_key = format!("{prefix}-seat-recovery-group");
    let lane = crate::services::DEFAULT_NAMESPACE
        .generation(
            crate::LashService::EffectGroupDispatch,
            test_build_generation(),
        )
        .name()
        .into_owned();
    // This is the first command after the driver's drain returned. Repeated
    // crashes keep the child before payload publication and the index seat.
    server.crash_on(
        CrashRule::new(CrashPoint::BeforeRun {
            name: format!("lash:effect-group:settled:{group_key}:0"),
        })
        .service(lane.clone())
        .handler("child")
        .key(&group_key)
        .times(u32::MAX),
    );
    let expire = harness.child_invocation_expiry();
    let ingress = harness.ingress();
    let stopped = server.clone();
    let expiry: lash_conformance::ChildInvocationExpiry = Arc::new(move |key, position| {
        let expire = Arc::clone(&expire);
        let server = stopped.clone();
        let ingress = ingress.clone();
        Box::pin(async move {
            tokio::time::timeout(Duration::from_secs(30), async {
                while server.stats().crashes == 0 {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("the child actually crashed after draining its last intent");
            assert!(
                matches!(
                    read_rank(&ingress, &key).await,
                    crate::EffectGroupReadRankResponse::NotSettled
                ),
                "the crash stopped the child before its seat"
            );
            expire(key, position).await;
            // No attempt of the original invocation can now finish. The
            // successor must run normally and drain the retained final.
            server.clear_crashes();
        })
    });
    lash_conformance::a_child_crashed_after_its_last_intent_recovers_its_reserved_seat(
        &fixture, &prefix, &expiry, &change,
    )
    .await;
    server.settle().await;
    let seated = read_rank(&harness.ingress(), &group_key).await;
    assert!(
        matches!(seated, crate::EffectGroupReadRankResponse::Settled { ref settlement, .. }
            if settlement.position == 0),
        "the recovered final seats at the original rank: {seated:?}"
    );
    let children: Vec<_> = server
        .invocations()
        .into_iter()
        .filter(|view| view.target.ends_with(&format!("/{group_key}/child")))
        .collect();
    assert_eq!(children.len(), 1, "only the recovered invocation remains");
    assert_eq!(children[0].target, format!("{lane}/{group_key}/child"));
    assert_eq!(children[0].status, "completed");
}

async fn read_rank(
    ingress: &RestateIngressClient,
    key: &str,
) -> crate::EffectGroupReadRankResponse {
    ingress
        .call_lash_object(
            "EffectGroupIndex",
            key,
            "read_rank",
            &crate::EffectGroupReadRankRequest {
                rank: 1,
                for_caller: false,
                run: false,
            },
        )
        .await
        .expect("read the reserved rank")
}

async fn host_controller_lane(tier: HarnessStoreTier) {
    let (stores, _sqlite, _resources) = tier.open().await;
    let server = RestateTestServer::new(ServerConfig::default().with_seed(0x4521_c001))
        .expect("start the server double");
    let connection = RestateConnection::with_transport(server.ingress_url(), server.transport());
    let log = RunLog::default();
    let served: Arc<Mutex<Vec<(&'static str, AttemptDispatch)>>> = Arc::default();
    let (_host_n, endpoint_n) =
        build_endpoint_with_host_controller(&connection, stores.as_ref(), &log).await;
    let (_host_next, endpoint_next) =
        build_endpoint(&connection, stores.as_ref(), "N+1", &log).await;
    let deployment_n = server
        .register_with(endpoint_n, "build-N", recording("N", &served))
        .await
        .expect("register build N");
    server
        .register_with(endpoint_next, "build-N+1", recording("N+1", &served))
        .await
        .expect("register the newer build");
    let lane = crate::services::DEFAULT_NAMESPACE
        .generation(
            crate::LashService::EffectGroupDispatch,
            lash_core::engine::BuildGeneration::for_test("N"),
        )
        .name()
        .into_owned();
    let key = "fig-4521-host-controller";
    server.crash_on(
        CrashRule::new(CrashPoint::BeforeFrame {
            ty: lash_restate_test::protocol::MessageType::CallCommand,
        })
        .service(&lane)
        .handler("run")
        .key(key)
        .within_attempts(1),
    );
    let mut positions: Vec<usize> = tokio::time::timeout(
        Duration::from_secs(60),
        RestateIngressClient::new(connection).call_workflow_json(
            "HostBuiltControllerProbe",
            key,
            "run",
            &key,
        ),
    )
    .await
    .expect("the host-built controller finishes")
    .expect("the host-built controller consumes its group");
    server.settle().await;
    positions.sort_unstable();
    assert_eq!(positions, vec![0, 1, 2]);
    assert_eq!(
        server.stats().crashes,
        1,
        "the dispatch replay actually ran"
    );
    let dispatches: Vec<_> = served
        .lock_recover()
        .iter()
        .filter(|(_, dispatch)| dispatch.service.starts_with("EffectGroupDispatch"))
        .cloned()
        .collect();
    assert!(!dispatches.is_empty());
    assert!(
        dispatches
            .iter()
            .all(|(build, dispatch)| *build == "N" && dispatch.service == lane),
        "preflight, dispatch and every child stay on N: {dispatches:#?}"
    );
    let children: Vec<_> = server
        .invocations()
        .into_iter()
        .filter(|view| view.target.ends_with("/child"))
        .collect();
    assert_eq!(children.len(), 3, "replay creates no second child");
    assert!(
        children
            .iter()
            .all(|view| view.target == format!("{lane}/{key}/child")
                && view.pinned_deployment_id == deployment_n.as_str())
    );
    let mut runs = log.lock_recover().clone();
    runs.sort();
    assert_eq!(
        runs,
        (0..3)
            .map(|position| ("N", format!("{key}:child:{position}")))
            .collect::<Vec<_>>(),
        "every child body runs exactly once on N"
    );
}
