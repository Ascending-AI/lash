use crate::{RestateAdminClient, RestateIngressClient, RestateNamespace};
use restate_sdk::endpoint::{Endpoint, ServiceOptions};
use restate_sdk::prelude::*;
use restate_sdk::service::macro_support::ServiceBoxFuture;
use restate_sdk::service::{Discoverable, Service};

fn named<S>(server: S, name: &str) -> restate_sdk::service::ServiceDefinition
where
    S: Service<Future = ServiceBoxFuture> + Discoverable + Send + Sync + 'static,
{
    let mut discovery = S::discover();
    discovery.name = name
        .to_owned()
        .try_into()
        .expect("qualified SDK service name");
    service_definition(server, discovery).options(
        ServiceOptions::new()
            .retry_policy_max_attempts(1)
            .retry_policy_pause_on_max_attempts(),
    )
}
use restate_sdk::service::macro_support::service_definition;
use std::num::NonZeroUsize;

#[restate_sdk::object]
trait NamespaceFilterProbe {
    async fn shift(input: Json<()>) -> HandlerResult<()>;
    async fn run(input: Json<()>) -> HandlerResult<()>;
}
struct NamespaceFilterProbeImpl;
impl NamespaceFilterProbe for NamespaceFilterProbeImpl {
    async fn shift(&self, _ctx: ObjectContext<'_>, _input: Json<()>) -> HandlerResult<()> {
        Err(HandlerError::from(std::io::Error::other(
            "pause the admin filter fixture",
        )))
    }
    async fn run(&self, _ctx: ObjectContext<'_>, _input: Json<()>) -> HandlerResult<()> {
        Err(HandlerError::from(std::io::Error::other(
            "pause the admin filter fixture",
        )))
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "the live suite supplies endpoint and server addresses"
)]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires the pinned live Restate server: the run-conformance suite runs it"]
async fn every_admin_filter_excludes_foreign_and_dotted_namespaces() {
    let admin_url = std::env::var("RESTATE_ADMIN_URL").expect("live admin URL");
    let ingress_url = std::env::var("RESTATE_INGRESS_URL").expect("live ingress URL");
    let bind = std::env::var("EG_RESTATE_ENDPOINT_BIND").expect("live endpoint bind");
    let endpoint_url = std::env::var("EG_RESTATE_ENDPOINT_URL").expect("live endpoint URL");
    let namespaces = [
        RestateNamespace::default(),
        RestateNamespace::new("foreign-admin-pin").expect("named namespace"),
    ];
    let bases = [
        ("LashSession", "shift"),
        ("LashTurn", "run"),
        ("LashProcessWorkflow", "run"),
    ];
    let mut builder = Endpoint::builder();
    let mut services = Vec::new();
    for namespace in &namespaces {
        for (base, handler) in bases {
            for suffix in ["", "_g123456789abc"] {
                let name = namespace.service_name(&format!("{base}{suffix}"));
                builder = builder.bind(named(NamespaceFilterProbeImpl.serve(), &name));
                services.push((name, handler));
            }
        }
    }
    let listener = tokio::net::TcpListener::bind(&bind)
        .await
        .expect("bind live fixture endpoint");
    let (stop, stopped) = tokio::sync::oneshot::channel();
    let serving = tokio::spawn(crate::serve_endpoint(
        listener,
        builder.build(),
        crate::RestateEndpointLimits::new(32 * 1024 * 1024, 32 * 1024 * 1024 + 8),
        async {
            let _ = stopped.await;
        },
    ));
    let registered = reqwest::Client::new()
        .post(format!("{admin_url}/deployments"))
        .json(&serde_json::json!({"uri":endpoint_url,"force":true}))
        .send()
        .await
        .expect("live discovery");
    assert!(
        registered.status().is_success(),
        "{}",
        registered.text().await.expect("registration body")
    );
    let ingress = RestateIngressClient::new(ingress_url);
    let admin = RestateAdminClient::new(admin_url);
    let mut admitted = std::collections::BTreeMap::new();
    for (service, handler) in &services {
        let id = ingress
            .send_object_json(service, "same-key", handler, &())
            .await
            .expect("send live fixture");
        admitted.insert(service.clone(), id.to_string());
    }
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            let mut all_paused = true;
            for id in admitted.values() {
                all_paused &= admin
                    .invocation_status(&crate::RestateInvocationId::new(id.clone()))
                    .await
                    .expect("live status")
                    .is_some_and(|row| row.status == crate::RestateInvocationLifecycle::Paused);
            }
            if all_paused {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("all twelve real invocations pause");
    for namespace in &namespaces {
        let own = |base: &str| {
            ["", "_g123456789abc"]
                .into_iter()
                .map(|suffix| admitted[&namespace.service_name(&format!("{base}{suffix}"))].clone())
                .collect::<std::collections::BTreeSet<_>>()
        };
        for (base, _) in bases {
            let rows = admin
                .paused_invocations(&namespace.service_name(base))
                .await
                .expect("paused service lanes");
            assert_eq!(
                rows.into_iter()
                    .map(|row| row.id)
                    .collect::<std::collections::BTreeSet<_>>(),
                own(base)
            );
        }
        let shifts = admin
            .paused_session_shifts(namespace, "same-key")
            .await
            .expect("paused session shifts");
        assert_eq!(
            shifts
                .into_iter()
                .map(|row| row.id)
                .collect::<std::collections::BTreeSet<_>>(),
            own("LashSession")
        );
        let expected = bases
            .into_iter()
            .flat_map(|(base, _)| own(base))
            .collect::<std::collections::BTreeSet<_>>();
        let mut after = None;
        let mut page_ids = std::collections::BTreeSet::new();
        loop {
            let rows = admin
                .paused_work_page(
                    namespace,
                    after.as_deref(),
                    NonZeroUsize::new(2).expect("page size"),
                )
                .await
                .expect("paged paused work");
            if rows.is_empty() {
                break;
            }
            after = rows.last().map(|row| row.id.clone());
            for row in rows {
                assert!(page_ids.insert(row.id), "page repeats a row");
            }
        }
        assert_eq!(page_ids, expected);
        let keys = ["same-key".to_string()];
        let runs = admin
            .run_executions(namespace, &keys)
            .await
            .expect("run executes in all lanes");
        assert_eq!(
            runs.into_iter()
                .map(|row| row.id)
                .collect::<std::collections::BTreeSet<_>>(),
            own("LashTurn")
        );
        let segments = admin
            .segment_runs(namespace, &keys)
            .await
            .expect("segment runs in all lanes");
        assert_eq!(
            segments
                .into_iter()
                .map(|row| row.id)
                .collect::<std::collections::BTreeSet<_>>(),
            own("LashProcessWorkflow")
        );
        for (service, handler) in services
            .iter()
            .filter(|(service, _)| namespace.is_default() == !service.contains('.'))
        {
            let row = admin
                .workflow_invocation_status(service, "same-key", handler)
                .await
                .expect("qualified lookup")
                .expect("retained own invocation");
            let base = service.strip_suffix("_g123456789abc").unwrap_or(service);
            assert!(
                row.id == admitted[service]
                    || (!service.ends_with("_g123456789abc")
                        && row.id == admitted[&format!("{base}_g123456789abc")]),
                "qualified lookup returned a foreign lane: {row:?}"
            );
        }
        let prefixes = [
            namespace.service_name("LashSession"),
            namespace.service_name("LashTurn"),
            namespace.service_name("LashProcessWorkflow"),
        ];
        let prefix_refs = prefixes.iter().map(String::as_str).collect::<Vec<_>>();
        let rows = admin
            .unfinished_invocations_for_service_prefixes(&prefix_refs)
            .await
            .expect("qualified unfinished prefixes");
        assert_eq!(
            rows.into_iter()
                .map(|row| row.id)
                .collect::<std::collections::BTreeSet<_>>(),
            expected
        );
    }
    for id in admitted.values() {
        admin
            .kill_invocation(&crate::RestateInvocationId::new(id.clone()))
            .await
            .expect("settle owned fixture invocation");
    }
    let _ = stop.send(());
    serving.await.expect("endpoint stops");
}
