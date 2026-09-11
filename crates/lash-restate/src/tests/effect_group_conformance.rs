//! Live Restate registration of the shared durable effect-group laws.

use lash_sansio::ProcessId;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use lash_core::{
    ExecutionScope, GroupExecutors, GroupWakePolicy, LoserPolicy, Resolution, RuntimeEffectCommand,
    RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectKind,
    RuntimeEffectLocalExecutor, RuntimeEffectOutcome, RuntimeErrorCode, RuntimeInvocation,
    RuntimeScope,
};
use restate_sdk::context::WorkflowContext;
use restate_sdk::endpoint::Endpoint;
use restate_sdk::errors::{HandlerResult, TerminalError};
use restate_sdk::http_server::HttpServer;
use restate_sdk::serde::Json;

use crate::durable_wait::arm_wait_registration_witness;
use crate::effect_group::{
    EffectGroupChildRequest, admit_wait_request, arm_admission_witness, decode_wait_resolution,
    payload_key, rank_wait_request, ready_wait_request,
};
use crate::{
    EffectGroupAdoptRequest, EffectGroupCleanupFacts, EffectGroupDispatchRequest,
    EffectGroupOpenRequest, EffectGroupOpenResponse, EffectGroupPayloadPutRequest,
    EffectGroupPayloadPutResponse, EffectGroupProbeAdoptResponse, EffectGroupReadRankRequest,
    EffectGroupReadRankResponse, EffectGroupRecordDispatchRequest,
    EffectGroupRecordDispatchResponse, EffectGroupRecordSettlementRequest,
    EffectGroupRecordSettlementResponse, EffectGroupRetireResponse, EffectGroupSettlementTerminal,
    EffectGroupShape, EffectGroupWaitResolution, LashDurableWaitIndex, LashDurableWaitWorkflow,
    RestateDurableWaitAddress, RestateDurableWaitAwaitRequest, RestateDurableWaitRegistration,
    RestateEffectGroupRetryPolicy, RestateEffectGroupServices, RestateEffectHost,
    RestateIngressClient,
};

#[derive(Default)]
struct ConformanceExecutors {
    current: Mutex<Option<Arc<dyn GroupExecutors>>>,
    staged: Mutex<HashMap<String, Arc<Mutex<Option<RuntimeEffectLocalExecutor<'static>>>>>>,
    mapping_current: AtomicBool,
}

impl ConformanceExecutors {
    fn install(&self, executors: Arc<dyn GroupExecutors>) {
        self.mapping_current.store(false, Ordering::SeqCst);
        self.staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        *self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(executors);
    }

    fn install_mapping_current(&self, executors: Arc<dyn GroupExecutors>) {
        self.install(executors);
        self.mapping_current.store(true, Ordering::SeqCst);
    }
}

impl GroupExecutors for ConformanceExecutors {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        let replay_key = envelope.invocation.replay_key()?.to_owned();
        if self.mapping_current.load(Ordering::SeqCst) {
            return self
                .current
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .as_ref()
                .and_then(|executors| executors.executor_for(envelope));
        }
        if let Some(staged) = self
            .staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(&replay_key)
            .cloned()
        {
            return Some(staged_executor(staged, replay_key));
        }
        let current = self
            .current
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .cloned()?;
        let executor = current.executor_for(envelope)?;
        let staged = Arc::new(Mutex::new(Some(executor)));
        self.staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(replay_key.clone(), Arc::clone(&staged));
        Some(staged_executor(staged, replay_key))
    }
}

fn staged_executor(
    staged: Arc<Mutex<Option<RuntimeEffectLocalExecutor<'static>>>>,
    replay_key: String,
) -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(move |envelope| async move {
        let executor = staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .take()
            .ok_or_else(|| {
                RuntimeEffectControllerError::new(
                    RuntimeErrorCode::RuntimeEffectLocalExecutorUnavailable,
                    format!("conformance executor for {replay_key} was already consumed"),
                )
            })?;
        executor.execute(envelope).await
    })
}

#[derive(Default)]
struct WitnessExecutors {
    staged: Mutex<HashMap<String, WitnessRoute>>,
    resolutions: AtomicUsize,
}

#[derive(Clone)]
struct WitnessRoute {
    executions: Arc<AtomicUsize>,
    label: &'static str,
}

impl WitnessExecutors {
    fn stage(
        &self,
        child: &RuntimeEffectEnvelope,
        executions: Arc<AtomicUsize>,
        label: &'static str,
    ) {
        self.staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(
                child
                    .invocation
                    .replay_key()
                    .expect("witness child has replay key")
                    .to_owned(),
                WitnessRoute { executions, label },
            );
    }
}

impl GroupExecutors for WitnessExecutors {
    fn executor_for(
        &self,
        envelope: &RuntimeEffectEnvelope,
    ) -> Option<RuntimeEffectLocalExecutor<'static>> {
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        let route = self
            .staged
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .get(envelope.invocation.replay_key()?)
            .cloned()?;
        Some(RuntimeEffectLocalExecutor::testing(move |_| async move {
            route.executions.fetch_add(1, Ordering::SeqCst);
            Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                value: serde_json::json!({ "witness": route.label }),
            })
        }))
    }
}

type GroupHostFactory =
    Box<dyn Fn(Option<Arc<dyn GroupExecutors>>) -> Arc<dyn lash_core::EffectHost> + Send + Sync>;

pub(super) struct LiveConformanceHarness {
    ingress_url: String,
    executors: Arc<ConformanceExecutors>,
    shutdown_tx: tokio::sync::oneshot::Sender<()>,
    server: tokio::task::JoinHandle<()>,
}

impl LiveConformanceHarness {
    pub(super) async fn start() -> Self {
        let ingress_url = required("RESTATE_INGRESS_URL");
        let admin_url = required("RESTATE_ADMIN_URL");
        let bind_addr = required("EG_RESTATE_ENDPOINT_BIND")
            .parse::<SocketAddr>()
            .expect("valid EG_RESTATE_ENDPOINT_BIND");
        let endpoint_url = required("EG_RESTATE_ENDPOINT_URL");
        let ingress = RestateIngressClient::new(ingress_url.clone());
        let executors = Arc::new(ConformanceExecutors::default());
        let services = RestateEffectGroupServices::new(
            Arc::clone(&executors) as Arc<dyn GroupExecutors>,
            ingress,
            RestateEffectGroupRetryPolicy::infinite(),
        );
        let listener = tokio::net::TcpListener::bind(bind_addr)
            .await
            .expect("bind Restate effect-group endpoint");
        let endpoint = Endpoint::builder()
            .bind(ScopeLivenessProbeImpl.serve())
            .bind(services.index)
            .bind(services.payload)
            .bind(services.dispatch)
            .bind(services.wait.workflow.serve())
            .bind(services.wait.index.serve())
            .build();
        let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
        let server = tokio::spawn(async move {
            HttpServer::new(endpoint)
                .serve_with_cancel(listener, async {
                    let _ = shutdown_rx.await;
                })
                .await;
        });
        wait_for_endpoint(bind_addr).await;
        register_deployment(&admin_url, &endpoint_url).await;

        Self {
            ingress_url,
            executors,
            shutdown_tx,
            server,
        }
    }

    pub(super) fn effect_host_factory(
        &self,
    ) -> Box<dyn Fn() -> Arc<dyn lash_core::EffectHost> + Send + Sync> {
        let ingress_url = self.ingress_url.clone();
        Box::new(move || {
            Arc::new(RestateEffectHost::new(ingress_url.clone())) as Arc<dyn lash_core::EffectHost>
        })
    }

    pub(super) fn group_host_factory(&self) -> GroupHostFactory {
        let ingress_url = self.ingress_url.clone();
        let executors = Arc::clone(&self.executors);
        Box::new(move |resolver| match resolver {
            Some(resolver) => {
                executors.install(resolver);
                Arc::new(RestateEffectHost::new(ingress_url.clone()))
                    as Arc<dyn lash_core::EffectHost>
            }
            None => Arc::new(lash_core::facade_support::NativeEffectHost::default())
                as Arc<dyn lash_core::EffectHost>,
        })
    }

    pub(super) async fn finish(self) {
        let _ = self.shutdown_tx.send(());
        self.server
            .await
            .expect("Restate effect-group endpoint task");
    }

    pub(super) async fn run_design_witnesses(&self) {
        run_design_witnesses(&self.ingress_url, &self.executors).await;
    }

    /// The handler-side half of the quiescence law (FIG-2499 fix round 3,
    /// ruling 4): an effect executing inside a Restate handler under a
    /// runtime-operation scope holds `WhenQuiescent` off until it completes.
    /// The deployment-level host cannot run a local executor, so the shared
    /// law early-returns on it; this witness runs the effect where Restate
    /// runs it.
    pub(super) async fn run_executing_effect_quiescence_witness(&self) {
        let ingress = RestateIngressClient::new(self.ingress_url.clone());
        let host = (self.effect_host_factory())();
        let scope_id = format!("live-effect-{}", nonce());
        let scope = ExecutionScope::runtime_operation(scope_id.clone());
        let gate = executing_effect_gate();
        let workflow_key = scope_id.clone();
        let workflow = tokio::spawn(async move {
            ingress
                .call_workflow_json::<_, bool>(
                    "ScopeLivenessProbe",
                    &workflow_key,
                    "run",
                    &scope_id,
                )
                .await
        });
        tokio::time::timeout(Duration::from_secs(30), gate.started.notified())
            .await
            .expect("the handler's effect starts executing");

        let refused = host
            .retire_effect_journal(
                lash_core::EffectJournalRetirement::for_scope(&scope)
                    .expect("runtime operations are retirable")
                    .when_quiescent(),
            )
            .await
            .expect_err("an executing handler effect is not quiescent");
        assert_eq!(refused.code.as_str(), "effect_scope_not_quiescent");
        host.await_event_key(
            &scope,
            lash_core::AwaitEventWaitIdentity::tool_completion("still-open"),
        )
        .await
        .expect("the refused retirement left the scope unfenced");

        gate.release.notify_one();
        let completed = tokio::time::timeout(Duration::from_secs(60), workflow)
            .await
            .expect("the released handler completes")
            .expect("the workflow task joins")
            .expect("the workflow returns");
        assert!(completed, "the handler ran its effect to completion");

        host.retire_effect_journal(
            lash_core::EffectJournalRetirement::for_scope(&scope)
                .expect("runtime operations are retirable")
                .when_quiescent(),
        )
        .await
        .expect("the scope is quiescent once the handler's effect completed");
        let fenced = host
            .await_event_key(
                &scope,
                lash_core::AwaitEventWaitIdentity::tool_completion("after-retirement"),
            )
            .await
            .expect_err("the retired scope mints nothing");
        assert_eq!(fenced.code.as_str(), "await_event_unknown_or_revoked");
    }

    /// Prove both serialized orders between an await workflow's durable index
    /// registration and scope retirement.
    ///
    /// The registration-first case uses the host's real await path. Its
    /// test-only marker fires from the index handler after `ctx.set` is issued;
    /// the following retirement is an exclusive call on that same virtual
    /// object, so Restate orders it after registration. Refusal is the state
    /// oracle: omitting the wait-row write would make retirement succeed and
    /// this witness fail.
    pub(super) async fn run_active_wait_registration_witnesses(
        &self,
        host: Arc<dyn lash_core::EffectHost>,
    ) {
        let suffix = nonce();
        let scope =
            ExecutionScope::runtime_operation(format!("restate-await-registration-first-{suffix}"));
        let key = host
            .await_event_key(
                &scope,
                lash_core::AwaitEventWaitIdentity::tool_completion("active-wait"),
            )
            .await
            .expect("mint registration-first wait key");
        let registered = arm_wait_registration_witness(&key);
        let waiter_host = Arc::clone(&host);
        let waiter_key = key.clone();
        let waiter = lash_core::task::spawn(async move {
            waiter_host
                .await_await_event(&waiter_key, CancellationToken::new(), None)
                .await
        });
        let registration = tokio::time::timeout(Duration::from_secs(30), registered)
            .await
            .expect("await workflow reached its index registration")
            .expect("registration witness sender remained live");
        assert_eq!(
            registration,
            RestateDurableWaitRegistration::Registered,
            "the workflow registered an unresolved wait before retirement"
        );

        lash_conformance::effect_host_registered_wait_rejects_quiescent_retirement(
            Arc::clone(&host),
            scope,
            key,
            waiter,
        )
        .await;

        // The opposite legal ordering: retirement fences an empty scope, then
        // the real wait workflow reaches the same index and observes Revoked.
        let retired_scope =
            ExecutionScope::runtime_operation(format!("restate-retirement-first-{suffix}"));
        let retired_key = host
            .await_event_key(
                &retired_scope,
                lash_core::AwaitEventWaitIdentity::tool_completion("late-wait"),
            )
            .await
            .expect("mint retirement-first wait key");
        host.retire_effect_journal(
            lash_core::EffectJournalRetirement::for_scope(&retired_scope)
                .expect("runtime operations are retirable")
                .when_quiescent(),
        )
        .await
        .expect("an empty scope retires before registration");

        let late_registration = arm_wait_registration_witness(&retired_key);
        let ingress = RestateIngressClient::new(self.ingress_url.clone());
        let workflow_key = RestateDurableWaitAddress::for_key(&retired_key).workflow_key;
        let late_workflow = lash_core::task::spawn(async move {
            ingress
                .call_workflow_json::<_, Resolution>(
                    "LashDurableWaitWorkflow",
                    &workflow_key,
                    "await_resolution",
                    &RestateDurableWaitAwaitRequest {
                        key: retired_key,
                        timeout_ms: None,
                    },
                )
                .await
        });
        let late_registration = tokio::time::timeout(Duration::from_secs(30), late_registration)
            .await
            .expect("late workflow reached the retired index")
            .expect("late registration witness sender remained live");
        assert_eq!(
            late_registration,
            RestateDurableWaitRegistration::Revoked,
            "retirement legitimately wins before durable registration"
        );
        let late_resolution = tokio::time::timeout(Duration::from_secs(30), late_workflow)
            .await
            .expect("late workflow completed after revoked registration")
            .expect("late workflow task joins")
            .expect("late workflow returns its terminal");
        assert_eq!(late_resolution, Resolution::Cancelled);

        println!(
            "RESTATE_QUIESCENCE await_registration_orders=registered-first,retired-first PASS"
        );
    }

    /// The crash cut between a registry's commit and its post-commit index
    /// reinstate (FIG-2499 fix round 3, ruling 2): the index is revoked, the
    /// registration is committed with no host bound, everything is dropped,
    /// and a cold registry plus host are opened and bound. The first effect
    /// under the process is admitted with no explicit re-registration: the
    /// host reads through the revoked index to the registry it is bound to.
    /// Runs over a SQLite-backed registry always and over a PostgreSQL-backed
    /// one when `LASH_POSTGRES_DATABASE_URL` names a server. Returns the
    /// number of registries witnessed.
    pub(super) async fn run_cold_reopen_witnesses(&self) -> usize {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry_path = dir.path().join("registry.db");
        let sessions = dir.path().join("sessions");
        let open_sqlite = || {
            let registry_path = registry_path.clone();
            let sessions = sessions.clone();
            async move {
                Arc::new(
                    lash_sqlite_store::SqliteProcessRegistry::open(&registry_path, sessions)
                        .await
                        .expect("open the SQLite process registry"),
                ) as Arc<dyn lash_core::ProcessRegistry>
            }
        };
        cold_reopen_admits_the_registered_process(
            &self.effect_host_factory(),
            "sqlite",
            open_sqlite,
        )
        .await;
        let mut witnessed = 1;

        if let Ok(url) = std::env::var("LASH_POSTGRES_DATABASE_URL") {
            let database = lash_postgres_store::testing::IsolatedDatabase::create(&url).await;
            let storage = lash_postgres_store::PostgresStorage::connect(database.url())
                .await
                .expect("connect the isolated database");
            let open_postgres = || {
                let storage = storage.clone();
                async move {
                    Arc::new(storage.process_registry()) as Arc<dyn lash_core::ProcessRegistry>
                }
            };
            cold_reopen_admits_the_registered_process(
                &self.effect_host_factory(),
                "postgres",
                open_postgres,
            )
            .await;
            witnessed += 1;
        } else {
            assert!(
                std::env::var("LASH_REQUIRE_POSTGRES").is_err(),
                "LASH_REQUIRE_POSTGRES=1 but LASH_POSTGRES_DATABASE_URL is not set"
            );
        }
        witnessed
    }
}

fn nonce() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock after the epoch")
        .as_nanos()
}

struct ExecutingEffectGate {
    started: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

fn executing_effect_gate() -> &'static ExecutingEffectGate {
    static GATE: std::sync::OnceLock<ExecutingEffectGate> = std::sync::OnceLock::new();
    GATE.get_or_init(|| ExecutingEffectGate {
        started: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    })
}

/// A workflow that runs one scoped local effect and holds it until the test
/// releases it: the executing-effect state of a Restate handler, observed
/// from outside through the durable-wait index.
#[restate_sdk::workflow]
pub(super) trait ScopeLivenessProbe {
    async fn run(input: Json<String>) -> HandlerResult<Json<bool>>;
}

pub(super) struct ScopeLivenessProbeImpl;

impl ScopeLivenessProbe for ScopeLivenessProbeImpl {
    async fn run(
        &self,
        ctx: WorkflowContext<'_>,
        Json(scope_id): Json<String>,
    ) -> HandlerResult<Json<bool>> {
        let controller = crate::RestateRuntimeEffectController::new(ctx);
        let scoped = controller
            .scoped_effect_controller(ExecutionScope::runtime_operation(scope_id.clone()))
            .map_err(TerminalError::from_error)?;
        let envelope = RuntimeEffectEnvelope::new(
            RuntimeInvocation::effect(
                RuntimeScope::new("scope-liveness"),
                "work",
                RuntimeEffectKind::LanguageRuntimeValue,
                format!("{scope_id}:work"),
            ),
            RuntimeEffectCommand::LanguageRuntimeValue {
                operation: "scope-liveness".to_string(),
            },
        );
        scoped
            .controller()
            .execute_effect(
                envelope,
                RuntimeEffectLocalExecutor::testing(|_| async {
                    let gate = executing_effect_gate();
                    gate.started.notify_one();
                    gate.release.notified().await;
                    Ok(RuntimeEffectOutcome::LanguageRuntimeValue {
                        value: serde_json::json!("completed"),
                    })
                }),
            )
            .await
            .map_err(TerminalError::from_error)?;
        Ok(Json(true))
    }
}

async fn cold_reopen_admits_the_registered_process<F, Fut>(
    host_factory: &dyn Fn() -> Arc<dyn lash_core::EffectHost>,
    label: &str,
    open_registry: F,
) where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Arc<dyn lash_core::ProcessRegistry>>,
{
    let nonce = nonce();
    let process_id = ProcessId::from(format!("cold-reopen-{label}-{nonce}"));
    let scope = ExecutionScope::process(process_id.clone());
    let registration = || {
        lash_core::ProcessRegistration::new(
            process_id.clone(),
            lash_core::ProcessInput::External {
                metadata: serde_json::Value::Null,
            },
            lash_core::RecoveryContract::ExternallyOwned,
            lash_core::ProcessProvenance::host(),
        )
        .with_identity(lash_core::ProcessIdentity::new("test"))
    };

    // The index is revoked, and the registration commits with no host bound:
    // the post-commit reinstate never reaches the engine.
    let host = host_factory();
    host.retire_effect_journal(lash_core::EffectJournalRetirement::process(
        process_id.clone(),
    ))
    .await
    .expect("retire the process scope");
    let registry = open_registry().await;
    registry
        .register_process(registration())
        .await
        .expect("register the process");
    drop(registry);
    drop(host);

    // Cold reopen: a fresh registry and a fresh host, bound the way
    // `LashCore::build` binds them, and nothing else.
    let registry = open_registry().await;
    let cold = host_factory();
    registry.bind_effect_host(&cold);
    let other = ExecutionScope::runtime_operation(format!("cold-reopen-ready-{label}-{nonce}"));
    let key = cold
        .await_event_key(
            &other,
            lash_core::AwaitEventWaitIdentity::tool_completion("ready"),
        )
        .await
        .expect("mint the effect's promise");
    cold.resolve_await_event(&key, Resolution::Ok(serde_json::json!("ready")))
        .await
        .expect("resolve the effect's promise");
    let envelope = RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::new("cold-reopen"),
            "first",
            RuntimeEffectKind::AwaitEvent,
            format!("cold-reopen-first-{label}-{nonce}"),
        ),
        RuntimeEffectCommand::AwaitEvent { key },
    );
    cold.scoped(scope.clone())
        .expect("the process scope binds")
        .controller()
        .execute_effect(
            envelope,
            RuntimeEffectLocalExecutor::await_event(CancellationToken::new(), None),
        )
        .await
        .unwrap_or_else(|error| {
            panic!("the first effect under the registered process is admitted after a cold reopen over a {label} registry, with no explicit re-registration: {error:?}")
        });
    cold.await_event_key(
        &scope,
        lash_core::AwaitEventWaitIdentity::tool_completion("after-reopen"),
    )
    .await
    .expect("the registered process mints after the cold reopen");
}

async fn run_design_witnesses(ingress_url: &str, executors: &Arc<ConformanceExecutors>) {
    let ingress = RestateIngressClient::new(ingress_url.to_owned());
    let witness_executors = Arc::new(WitnessExecutors::default());
    executors.install_mapping_current(Arc::clone(&witness_executors) as Arc<dyn GroupExecutors>);

    let group_key = witness_key("dispatcher");
    let child = witness_child(&group_key, 0);
    let shape = witness_shape(&group_key, std::slice::from_ref(&child));
    let executions = Arc::new(AtomicUsize::new(0));
    witness_executors.stage(&child, Arc::clone(&executions), "dispatcher-convergence");
    let opened: EffectGroupOpenResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &group_key,
            "open",
            &EffectGroupOpenRequest {
                shape: shape.clone(),
            },
        )
        .await
        .expect("witness group opens");
    assert_eq!(opened, EffectGroupOpenResponse::OpenedFresh);
    let request = EffectGroupDispatchRequest {
        group_key: group_key.clone(),
        shape: shape.clone(),
        children: vec![child.clone()],
    };
    let (first, second) = tokio::join!(
        ingress.send_workflow_json("EffectGroupDispatch", &group_key, "run", &request),
        ingress.send_workflow_json("EffectGroupDispatch", &group_key, "run", &request)
    );
    let first = first.expect("first dispatcher submission is accepted");
    let second = second.expect("concurrent dispatcher submission attaches");
    assert_eq!(first, second, "one workflow key has one invocation id");
    assert_eq!(
        await_group_wait(
            &ingress,
            ready_wait_request(&shape.wait_scope, &group_key).unwrap()
        )
        .await,
        EffectGroupWaitResolution::Ready
    );
    assert_eq!(
        await_group_wait(
            &ingress,
            rank_wait_request(&shape.wait_scope, &group_key, 1).unwrap()
        )
        .await,
        EffectGroupWaitResolution::Rank
    );
    let rank: EffectGroupReadRankResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &group_key,
            "read_rank",
            &EffectGroupReadRankRequest { rank: 1 },
        )
        .await
        .expect("dispatcher witness rank reads");
    assert!(matches!(rank, EffectGroupReadRankResponse::Settled { .. }));
    assert_eq!(executions.load(Ordering::SeqCst), 1, "child runs once");
    let reopened: EffectGroupOpenResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &group_key,
            "open",
            &EffectGroupOpenRequest {
                shape: shape.clone(),
            },
        )
        .await
        .expect("converged group reopens");
    assert_eq!(reopened, EffectGroupOpenResponse::ReopenedReady);
    println!("EFFECT_GROUP_WITNESS h dispatcher-convergence PASS");
    println!("EFFECT_GROUP_WITNESS l workflow-exactly-once-key PASS");

    let resolutions_before_guard = witness_executors.resolutions.load(Ordering::SeqCst);
    ingress
        .call_workflow_json::<_, ()>(
            "EffectGroupDispatch",
            &format!("{group_key}:stale-dispatch-diagnostic"),
            "run",
            &request,
        )
        .await
        .expect("stale dispatcher reaches its index guard");
    assert_eq!(
        witness_executors.resolutions.load(Ordering::SeqCst),
        resolutions_before_guard,
        "Ready probe guard exits before preflight or sends"
    );
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    println!("EFFECT_GROUP_WITNESS k dispatcher-probe-guard PASS");

    ingress
        .call_workflow_json::<_, ()>("EffectGroupDispatch", &group_key, "retire", &group_key)
        .await
        .expect("retirement saga completes");
    let payload_put: EffectGroupPayloadPutResponse = ingress
        .call_object_json(
            "EffectGroupPayload",
            &payload_key(&group_key, 0),
            "put",
            &EffectGroupPayloadPutRequest {
                bytes: b"late-write".to_vec(),
            },
        )
        .await
        .expect("retired payload fence answers");
    assert_eq!(payload_put, EffectGroupPayloadPutResponse::Retired);
    let late_record: EffectGroupRecordSettlementResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &group_key,
            "record_settlement",
            &EffectGroupRecordSettlementRequest {
                position: 0,
                terminal: EffectGroupSettlementTerminal::Cancelled,
            },
        )
        .await
        .expect("retired index fence answers");
    assert_eq!(late_record, EffectGroupRecordSettlementResponse::Retired);
    println!("EFFECT_GROUP_WITNESS i object-local-retired-fence PASS");

    for request in [
        ready_wait_request(&shape.wait_scope, &group_key).unwrap(),
        rank_wait_request(&shape.wait_scope, &group_key, 1).unwrap(),
    ] {
        assert_eq!(
            await_group_wait(&ingress, request).await,
            EffectGroupWaitResolution::Retired,
            "late registration observes the retained retirement fence"
        );
    }
    println!("EFFECT_GROUP_WITNESS j late-registration-reresolve PASS");

    let admission_group = witness_key("admit");
    let admission_child = witness_child(&admission_group, 0);
    let admission_shape = witness_shape(&admission_group, std::slice::from_ref(&admission_child));
    let opened: EffectGroupOpenResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &admission_group,
            "open",
            &EffectGroupOpenRequest {
                shape: admission_shape.clone(),
            },
        )
        .await
        .expect("admission witness opens");
    assert_eq!(opened, EffectGroupOpenResponse::OpenedFresh);
    let adopted: EffectGroupProbeAdoptResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &admission_group,
            "probe_and_adopt",
            &EffectGroupAdoptRequest {
                invocation_id: "inv_admission_dispatcher".to_owned(),
            },
        )
        .await
        .expect("admission witness adopts dispatcher");
    assert_eq!(adopted, EffectGroupProbeAdoptResponse::Adopted);
    let admission_executions = Arc::new(AtomicUsize::new(0));
    witness_executors.stage(
        &admission_child,
        Arc::clone(&admission_executions),
        "fresh-admission",
    );
    let first_admit = arm_admission_witness(&admission_group);
    let child_invocation = ingress
        .send_workflow_json(
            "EffectGroupDispatch",
            &admission_group,
            "child",
            &EffectGroupChildRequest {
                group_key: admission_group.clone(),
                shape: admission_shape.clone(),
                position: 0,
                envelope: admission_child,
            },
        )
        .await
        .expect("send-before-record child is accepted");
    tokio::time::timeout(Duration::from_secs(10), first_admit.notified())
        .await
        .expect("child reaches NotYetRecorded before dispatcher redrive");
    let recorded: EffectGroupRecordDispatchResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &admission_group,
            "record_dispatch",
            &EffectGroupRecordDispatchRequest {
                position: 0,
                invocation_id: child_invocation.as_str().to_owned(),
            },
        )
        .await
        .expect("dispatcher redrive records mapping");
    assert_eq!(recorded, EffectGroupRecordDispatchResponse::Recorded);
    assert_eq!(
        await_group_wait(
            &ingress,
            admit_wait_request(&admission_shape.wait_scope, &admission_group, 0).unwrap()
        )
        .await,
        EffectGroupWaitResolution::Admit,
        "record-before-register retains the ADMIT notification"
    );
    assert_eq!(
        await_group_wait(
            &ingress,
            rank_wait_request(&admission_shape.wait_scope, &admission_group, 1).unwrap()
        )
        .await,
        EffectGroupWaitResolution::Rank,
        "fresh admission executes and records a settlement"
    );
    assert_eq!(
        admission_executions.load(Ordering::SeqCst),
        1,
        "the crash-before-record child executes exactly once"
    );
    let retired: EffectGroupRetireResponse = ingress
        .call_object_empty_json("EffectGroupIndex", &admission_group, "retire")
        .await
        .expect("admission witness tombstones");
    let cleanup = match retired {
        EffectGroupRetireResponse::Retired { cleanup }
        | EffectGroupRetireResponse::AlreadyRetired { cleanup } => cleanup,
        other => panic!("admission witness expected cleanup facts, got {other:?}"),
    };
    assert_admission_enumerated(&cleanup, child_invocation.as_str());

    let gap_group = witness_key("gap");
    let gap_child = witness_child(&gap_group, 0);
    let gap_shape = witness_shape(&gap_group, std::slice::from_ref(&gap_child));
    let gap_executions = Arc::new(AtomicUsize::new(0));
    witness_executors.stage(
        &gap_child,
        Arc::clone(&gap_executions),
        "never-recorded-child",
    );
    let _: EffectGroupOpenResponse = ingress
        .call_object_json(
            "EffectGroupIndex",
            &gap_group,
            "open",
            &EffectGroupOpenRequest {
                shape: gap_shape.clone(),
            },
        )
        .await
        .expect("send-record-gap witness opens");
    let _: EffectGroupRetireResponse = ingress
        .call_object_empty_json("EffectGroupIndex", &gap_group, "retire")
        .await
        .expect("send-record-gap witness tombstones");
    let executions_before_child = gap_executions.load(Ordering::SeqCst);
    ingress
        .call_workflow_json::<_, ()>(
            "EffectGroupDispatch",
            &gap_group,
            "child",
            &EffectGroupChildRequest {
                group_key: gap_group.clone(),
                shape: gap_shape,
                position: 0,
                envelope: gap_child,
            },
        )
        .await
        .expect("post-tombstone never-recorded child is refused");
    assert_eq!(
        gap_executions.load(Ordering::SeqCst),
        executions_before_child,
        "post-tombstone child whose mapping was never recorded must not execute"
    );
    println!("EFFECT_GROUP_WITNESS m admission-enumeration PASS");
}

fn witness_child(group_key: &str, position: usize) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeInvocation::effect(
            RuntimeScope::new(group_key),
            "effect",
            RuntimeEffectKind::LanguageRuntimeValue,
            format!("{group_key}:child:{position}"),
        ),
        RuntimeEffectCommand::LanguageRuntimeValue {
            operation: format!("witness-child-{position}"),
        },
    )
}

fn witness_key(label: &str) -> String {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    format!(
        "effect-group-witness-{label}-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    )
}

fn witness_shape(group_key: &str, children: &[RuntimeEffectEnvelope]) -> EffectGroupShape {
    EffectGroupShape {
        children: children.len(),
        wake: GroupWakePolicy::All,
        loser_disposition: LoserPolicy::RunToCompletion,
        replay_keys: children
            .iter()
            .map(|child| child.invocation.replay_key().unwrap().to_owned())
            .collect(),
        wait_scope: ExecutionScope::runtime_operation(group_key),
    }
}

async fn await_group_wait(
    ingress: &RestateIngressClient,
    request: RestateDurableWaitAwaitRequest,
) -> EffectGroupWaitResolution {
    let address = RestateDurableWaitAddress::for_key(&request.key);
    let resolution = ingress
        .call_workflow_json::<_, Resolution>(
            "LashDurableWaitWorkflow",
            &address.workflow_key,
            "await_resolution",
            &request,
        )
        .await
        .expect("effect-group witness wait resolves");
    decode_wait_resolution(resolution).expect("effect-group witness resolution is tagged")
}

fn assert_admission_enumerated(cleanup: &EffectGroupCleanupFacts, invocation_id: &str) {
    assert_eq!(
        cleanup.dispatched.get(&0).map(String::as_str),
        Some(invocation_id)
    );
}

fn required(name: &str) -> String {
    std::env::var(name)
        .unwrap_or_else(|_| panic!("{name} must be set by `just effect-group-conformance-e2e`"))
}

async fn wait_for_endpoint(addr: SocketAddr) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if tokio::net::TcpStream::connect(addr).await.is_ok() {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "Restate effect-group endpoint did not open at {addr}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn register_deployment(admin_url: &str, endpoint_url: &str) {
    let client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .expect("build Restate admin client");
    let response = client
        .post(format!("{}/deployments", admin_url.trim_end_matches('/')))
        .json(&serde_json::json!({
            "uri": endpoint_url,
            "force": true,
            "breaking": true,
        }))
        .send()
        .await
        .expect("register Restate effect-group deployment");
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    assert!(
        status.is_success(),
        "Restate deployment registration failed: {status} {body}"
    );
}
