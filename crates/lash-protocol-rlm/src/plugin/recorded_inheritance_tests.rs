//! FIG-4527 laws: no run reads the behaviour of the deployment it happens
//! to run on.
//!
//! A Lashlang process whose captured plugin configuration has no RLM
//! namespace (a host started it under an environment the host published)
//! records the creating deployment's behaviour with its row and runs under
//! that record on a deployment configured otherwise. A child session records
//! its parent's behaviour, not the behaviour of the host that creates it.
//!
//! Both laws run over SQLite file, SQLite memory and PostgreSQL, each on the
//! Restate server double plain and always-replay.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use lash_core::facade_support::{PluginHost, RuntimeHostConfig};
use lash_core::plugin::PluginFactory;
use lash_core::testing::TestTurnDrive as _;
use lash_core::{
    CommitBudget, ProcessExecutionEnvSpec, QueuedWorkBatchingConfig, SessionCreationHead,
    SessionRelation, SessionStoreCreateRequest, TurnInput,
};
use lash_lashlang_runtime::LashlangProcessInput;
use lash_sansio::{SessionId, TurnId};

use super::recorded_behaviour_tests::{
    LOOP_ITERATIONS, LOST_FEATURES_ANSWER, Model, always_replay, creating_config, nonce,
    on_postgres, on_sqlite_file, on_sqlite_memory, open_runtime, plain, policy, redeploying_config,
};
use crate::plugin::{RlmProtocolPluginConfig, RlmRecordedConfig};
use crate::{RLM_PROTOCOL_PLUGIN_ID, RlmProtocolPluginFactory};

type Double = lash_restate_test::RestateTestBackend<dyn lash_core::StoreSet>;

enum ProcessHost {
    Double(Double),
    Live(lash_restate_test::live::LiveRestateBackend),
}

impl ProcessHost {
    fn backend(&self) -> lash_core::Backend {
        match self {
            Self::Double(host) => host.lash_backend(),
            Self::Live(host) => host.lash_backend(),
        }
    }

    fn install_worker(&self, worker: lash_core_worker::DurableProcessWorker) {
        match self {
            Self::Double(host) => host.install_process_worker(worker),
            Self::Live(host) => host.install_process_worker(worker),
        }
    }

    async fn in_handler(
        &self,
        admitted: lash_core::AdmittedScope,
        attempt: lash_restate_test::HandlerAttempt,
    ) {
        match self {
            Self::Double(host) => host.run_in_handler(admitted, attempt).await,
            Self::Live(host) => host.run_in_handler(admitted, attempt).await,
        }
        .expect("the registered Restate handler completes");
    }
}

/// The process body: the same loop the session law's cell runs, far past the
/// redeployed instruction bound and far inside the recorded one.
fn looping_process() -> String {
    format!(
        "const main = async () => {{\n  let i = 0;\n  while (i < {LOOP_ITERATIONS}) {{ i = i + 1; }}\n  return \"ran \" + String(i);\n}};\n"
    )
}

/// One deployment over `backend`: its RLM factory, its plugin set and the
/// runtime host carrying the process engine the factory contributes.
struct Deployment {
    factory: Arc<RlmProtocolPluginFactory>,
    plugin_host: Arc<PluginHost>,
    runtime_host: RuntimeHostConfig,
}

impl Deployment {
    fn new(config: RlmProtocolPluginConfig, backend: &lash_core::Backend) -> Self {
        let factory = Arc::new(RlmProtocolPluginFactory::new(
            config,
            Arc::new(crate::TypescriptDialect),
            backend,
        ));
        let plugin_host = Arc::new(PluginHost::new(vec![
            Arc::clone(&factory) as Arc<dyn PluginFactory>
        ]));
        let runtime_host = plugin_host
            .install_process_engine_contributions(
                RuntimeHostConfig::new(
                    backend.clone(),
                    CommitBudget::bounded(8 * 1024 * 1024, 1024),
                    QueuedWorkBatchingConfig::new(1),
                ),
                false,
            )
            .expect("the deployment installs the RLM process engine");
        Self {
            factory,
            plugin_host,
            runtime_host,
        }
    }
}

fn host_pin_claim() -> lash_core::ReferrerClaim {
    lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
        lash_core::HostArtifactPin::mint(),
    ))
    .expect("a host pin is an unguarded referrer")
}

/// Start through the handler's journaled registration, including its engine
/// configuration, rather than calling the registrar outside a handler.
async fn register_on(
    host: &ProcessHost,
    deployment: &Deployment,
    registration: lash_core::ProcessRegistration,
    starter: &str,
) -> (
    lash_core::ProcessRecord,
    lash_core::ProcessRegistrationOutcome,
) {
    let backend = host.backend();
    let engines = deployment.runtime_host.process_engines.clone();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let attempt: lash_restate_test::HandlerAttempt = Arc::new(move |scoped| {
        let backend = backend.clone();
        let engines = engines.clone();
        let registration = registration.clone();
        let tx = tx.clone();
        Box::pin(async move {
            let command = lash_core::ProcessCommand::Start {
                registration,
                observers: Vec::new(),
                execution_context: Box::new(lash_core::ProcessExecutionContext::default()),
            };
            let effect_id = command.effect_id();
            let envelope = lash_core::RuntimeEffectEnvelope::new(
                lash_core::RuntimeEffectInvocation::new(
                    lash_core::EffectAddress::new(
                        scoped.execution_scope().clone(),
                        effect_id.clone(),
                    )
                    .unwrap(),
                    lash_core::RuntimeAttribution::none(),
                    effect_id,
                ),
                lash_core::RuntimeEffectCommand::process(command),
            );
            let outcome = scoped
                .execute_effect(
                    envelope,
                    lash_core::RuntimeEffectLocalExecutor::processes(
                        backend.process_registry(),
                        Arc::clone(backend.process_work().port()),
                    )
                    .with_process_env_store(backend.process_env_store())
                    .with_process_engines(engines),
                )
                .await
                .expect("the process start runs through its Restate registration");
            let lash_core::RuntimeEffectOutcome::Process {
                result:
                    lash_core::ProcessEffectOutcome::Start {
                        record,
                        disposition,
                    },
            } = outcome
            else {
                panic!("a start returns its recorded process")
            };
            tx.send((*record, disposition)).unwrap();
        })
    });
    host.in_handler(
        lash_core::AdmittedScope::turn("recorded-process-behaviour", starter),
        attempt,
    )
    .await;
    let mut result = rx
        .recv()
        .await
        .expect("the completed handler returned its record");
    while let Ok(replayed) = rx.try_recv() {
        assert_eq!(
            replayed.0.engine_config, result.0.engine_config,
            "replay keeps the recorded configuration"
        );
        result = replayed;
    }
    result
}

/// A host starts a Lashlang process on one deployment under an environment
/// with no RLM namespace. The row records that deployment's behaviour, a
/// second registration under the same key on a deployment configured
/// otherwise is returned the same row, and the process runs on that other
/// deployment's worker under the recorded behaviour: its loop finishes where
/// the running deployment's instruction bound would stop it.
async fn a_host_started_process_runs_under_the_behaviour_its_creation_recorded(
    host: ProcessHost,
    name: &str,
) {
    let backend = host.backend();
    let creating = Deployment::new(creating_config(), &backend);
    let env_spec =
        ProcessExecutionEnvSpec::new(lash_core::AdmittedPluginConfig::default(), policy());
    let compiled = creating
        .factory
        .compile_lashlang_module(
            &creating.plugin_host,
            false,
            crate::LashlangModuleCompileRequest::new(name, looping_process(), env_spec.clone()),
        )
        .expect("the process module compiles");
    creating
        .factory
        .artifact_store()
        .publish_module_artifact(&host_pin_claim(), &compiled.artifact)
        .await
        .expect("the module publishes");
    let target = compiled
        .introspection
        .exported_processes
        .iter()
        .find(|process| process.params.is_empty())
        .expect("the module exports its process");
    let input = LashlangProcessInput {
        module_ref: compiled.module_ref.clone(),
        process_ref: target.definition.process_ref.clone(),
        host_requirements_ref: compiled.host_requirements_ref.clone(),
        process_name: compiled
            .artifact
            .process_name_for_ref(&target.definition.process_ref)
            .expect("the process's name")
            .to_owned(),
        args: serde_json::Map::new(),
    }
    .into_process_input()
    .expect("the process input encodes");
    let env_store = backend.process_env_store();
    let env_ref = lash_core::runtime::publish_process_execution_env(
        env_store.as_ref(),
        &host_pin_claim(),
        &env_spec,
    )
    .await
    .expect("the host publishes the environment");
    let lash_core::ProcessInput::Engine { kind, payload } = &input else {
        panic!("a Lashlang start is an engine start");
    };
    let identity = creating
        .runtime_host
        .process_engines
        .admit(kind, payload, Some(&env_spec))
        .await
        .expect("the creating deployment admits the start");
    let registration = lash_core::ProcessRegistration::new(
        input.clone(),
        lash_core::ProcessProvenance::host(),
        lash_core::Lifetime::Detached,
    )
    .with_start_key(Some(lash_core::StartKey::for_host(name)))
    .with_execution_env_ref(Some(env_ref))
    .with_admitted_identity(identity);

    let redeployed = Deployment::new(redeploying_config(), &backend);
    let worker = lash_core_worker::DurableProcessWorker::new(
        lash_core_worker::DurableProcessWorkerConfig::new(
            Arc::clone(&redeployed.plugin_host),
            redeployed.runtime_host.clone(),
            backend.process_work(),
            Arc::new(lash_core::NoSessionWork::new()),
            lash_core::testing::runtime_lease_owner(),
        ),
    )
    .expect("the redeployed process worker");
    host.install_worker(worker);

    let (created, disposition) = register_on(
        &host,
        &creating,
        registration.clone(),
        &format!("{name}-create"),
    )
    .await;
    assert_eq!(disposition, lash_core::ProcessRegistrationOutcome::Created);
    let recorded = serde_json::to_value(creating_config().recorded_behaviour(false))
        .expect("the creating behaviour encodes");
    assert_eq!(
        created.engine_config.as_ref(),
        Some(&recorded),
        "the row records the creating deployment's behaviour"
    );

    let remote = lash_remote_protocol::RemoteProcessRecord::try_from(created.clone()).unwrap();
    let wire = serde_json::to_vec(&remote).unwrap();
    let received = lash_core::ProcessRecord::try_from(
        serde_json::from_slice::<lash_remote_protocol::RemoteProcessRecord>(&wire).unwrap(),
    )
    .unwrap();
    assert_eq!(
        received.engine_config.as_ref(),
        Some(&recorded),
        "the remote host receives the creating behaviour"
    );

    let (retained, disposition) =
        register_on(&host, &redeployed, registration, &format!("{name}-redrive")).await;
    assert_eq!(disposition, lash_core::ProcessRegistrationOutcome::Existing);
    assert_eq!(retained.id, created.id);
    assert_eq!(
        retained.engine_config.as_ref(),
        Some(&recorded),
        "a start redriven on another deployment keeps what its creation recorded"
    );

    let terminal = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        lash_core::NoProcessWork::for_registry(backend.process_registry())
            .await_terminal(&created.id),
    )
    .await
    .expect("the process reaches its terminal on the redeployed worker")
    .expect("the process's terminal record");
    let spelled = serde_json::to_string(&terminal).expect("terminal JSON");
    assert!(
        matches!(
            terminal,
            lash_core::ProcessAwaitOutput::Settled { ref output } if output.is_success()
        ) && spelled.contains(&format!("ran {LOOP_ITERATIONS}")),
        "the process ran its loop under the recorded bound: {spelled}"
    );
}

/// A child session created on a host whose RLM factory states other bounds
/// and features than its parent recorded records the parent's behaviour, and
/// its root runs under it on that host: its prompt offers `continue_as` and
/// its cell runs a loop the creating host's bound would stop.
async fn a_child_session_runs_under_its_parents_recorded_behaviour(double: Double, name: &str) {
    let backend = double.lash_backend();
    let parent_id = SessionId::from(format!("{name}-parent"));
    let child_id = SessionId::from(format!("{name}-child"));
    let mut parent: lash_core::PersistedSessionConfig = policy().into();
    parent.plugin_config = PluginHost::new(vec![super::recorded_behaviour_tests::factory(
        creating_config(),
        &backend,
    )])
    .resolve_creation_plugin_config(
        Some(RLM_PROTOCOL_PLUGIN_ID),
        &lash_core::PluginOptions::default(),
        None,
        true,
    )
    .expect("the parent's deployment records the RLM namespace");
    let mut child: lash_core::PersistedSessionConfig = policy().into();
    child.plugin_config = PluginHost::new(vec![super::recorded_behaviour_tests::factory(
        redeploying_config(),
        &backend,
    )])
    .resolve_creation_plugin_config(
        Some(RLM_PROTOCOL_PLUGIN_ID),
        &lash_core::PluginOptions::default(),
        Some(&parent.plugin_config),
        false,
    )
    .expect("the child's creating host records the RLM namespace");
    let recorded = child
        .plugin_config
        .decode::<RlmRecordedConfig>(RLM_PROTOCOL_PLUGIN_ID)
        .expect("the child's RLM namespace decodes")
        .expect("the child records an RLM namespace");
    assert_eq!(
        recorded.behaviour,
        creating_config().recorded_behaviour(false),
        "the child records its parent's behaviour, not its creating host's"
    );

    let sessions = backend.session_store_factory();
    lash_core::runtime::admit_session_view(
        &sessions,
        &SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: parent_id.clone(),
            relation: SessionRelation::Root,
            config: parent,
            head: SessionCreationHead::Config,
        },
    )
    .await
    .expect("create the parent session");
    let store = lash_core::runtime::admit_session_view(
        &sessions,
        &SessionStoreCreateRequest {
            owning_process_id: None,
            pending_observer_intents: Vec::new(),
            session_id: child_id.clone(),
            relation: SessionRelation::Child {
                parent_session_id: parent_id,
                caused_by: None,
            },
            config: child,
            head: SessionCreationHead::Config,
        },
    )
    .await
    .expect("create the child session");

    let model = Arc::new(Model::default());
    let (turn_tx, mut turn_rx) = tokio::sync::mpsc::unbounded_channel();
    let attempt: lash_restate_test::HandlerAttempt = {
        let backend = backend.clone();
        let model = Arc::clone(&model);
        Arc::new(move |scoped| {
            let backend = backend.clone();
            let store = store.clone();
            let model = Arc::clone(&model);
            let turn_tx = turn_tx.clone();
            Box::pin(async move {
                let mut runtime = open_runtime(&backend, store, redeploying_config(), &model).await;
                let turn = runtime
                    .drive_turn(
                        TurnInput::text("loop it"),
                        lash_core::facade_support::TurnOptions::new(
                            tokio_util::sync::CancellationToken::new(),
                            scoped,
                        ),
                    )
                    .await;
                let _ = turn_tx.send(turn);
            })
        })
    };
    double
        .run_in_handler(
            lash_core::AdmittedScope::turn(&child_id, TurnId::from(format!("{name}-root"))),
            attempt,
        )
        .await
        .expect("the child's root runs on its creating host");
    let turn = turn_rx
        .recv()
        .await
        .expect("the handler ran the child's root")
        .unwrap_or_else(|error| panic!("the child's root runs: {error:?}"));
    let outcome = serde_json::to_string(&turn.outcome).expect("outcome JSON");
    assert!(
        !outcome.contains(LOST_FEATURES_ANSWER),
        "the child's prompt withheld continue_as: {outcome}"
    );
    assert!(
        matches!(
            turn.outcome,
            lash_core::facade_support::TurnOutcome::Finished(_)
        ) && outcome.contains(&format!("ran {LOOP_ITERATIONS}")),
        "the child's cell ran its loop under its parent's recorded bound: {outcome}"
    );
    assert!(model.calls.load(Ordering::SeqCst) >= 1);
}

macro_rules! on_every_store {
    ($law:ident, $prefix:literal, $file:ident, $file_replay:ident, $memory:ident, $memory_replay:ident, $postgres:ident, $postgres_replay:ident) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $file() {
            let dir = tempfile::tempdir().expect("SQLite directory");
            $law(
                on_sqlite_file(plain(), &dir).await,
                concat!($prefix, "-sqlite-file"),
            )
            .await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $file_replay() {
            let dir = tempfile::tempdir().expect("SQLite directory");
            $law(
                on_sqlite_file(always_replay(), &dir).await,
                concat!($prefix, "-sqlite-file-replay"),
            )
            .await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $memory() {
            $law(
                on_sqlite_memory(plain()).await,
                concat!($prefix, "-sqlite-memory"),
            )
            .await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $memory_replay() {
            $law(
                on_sqlite_memory(always_replay()).await,
                concat!($prefix, "-sqlite-memory-replay"),
            )
            .await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $postgres() {
            let Some((double, _attachments)) = on_postgres(plain()).await else {
                return;
            };
            $law(double, &format!(concat!($prefix, "-pg-{}"), nonce())).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $postgres_replay() {
            let Some((double, _attachments)) = on_postgres(always_replay()).await else {
                return;
            };
            $law(double, &format!(concat!($prefix, "-pg-replay-{}"), nonce())).await;
        }
    };
}

async fn process_law_on_double(double: Double, name: &str) {
    a_host_started_process_runs_under_the_behaviour_its_creation_recorded(
        ProcessHost::Double(double),
        name,
    )
    .await;
}

on_every_store!(
    process_law_on_double,
    "recorded-process-behaviour",
    a_host_started_process_runs_under_the_behaviour_its_creation_recorded_on_sqlite_file,
    a_host_started_process_runs_under_the_behaviour_its_creation_recorded_on_sqlite_file_always_replay,
    a_host_started_process_runs_under_the_behaviour_its_creation_recorded_on_sqlite_memory,
    a_host_started_process_runs_under_the_behaviour_its_creation_recorded_on_sqlite_memory_always_replay,
    a_host_started_process_runs_under_the_behaviour_its_creation_recorded_on_postgres,
    a_host_started_process_runs_under_the_behaviour_its_creation_recorded_on_postgres_always_replay
);

on_every_store!(
    a_child_session_runs_under_its_parents_recorded_behaviour,
    "inherited-behaviour",
    a_child_session_runs_under_its_parents_recorded_behaviour_on_sqlite_file,
    a_child_session_runs_under_its_parents_recorded_behaviour_on_sqlite_file_always_replay,
    a_child_session_runs_under_its_parents_recorded_behaviour_on_sqlite_memory,
    a_child_session_runs_under_its_parents_recorded_behaviour_on_sqlite_memory_always_replay,
    a_child_session_runs_under_its_parents_recorded_behaviour_on_postgres,
    a_child_session_runs_under_its_parents_recorded_behaviour_on_postgres_always_replay
);

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a live Restate server; recorded-process-behaviour suite"]
#[allow(
    clippy::disallowed_methods,
    reason = "the live suite supplies isolated server endpoints"
)]
async fn live_restate_process_runs_under_the_behaviour_its_creation_recorded() {
    let env = |key: &str| std::env::var(key).unwrap_or_else(|_| panic!("{key} is required"));
    let host =
        lash_restate_test::live::LiveRestateBackend::start(lash_restate_test::live::LiveConfig {
            ingress_url: env("RESTATE_INGRESS_URL"),
            admin_url: env("RESTATE_ADMIN_URL"),
            endpoint_bind: env("RPB_BIND").parse().unwrap(),
            endpoint_url: env("RPB_URL"),
            run_tag: format!("recorded-process-{}", nonce()),
            namespace: Default::default(),
        })
        .await
        .expect("serve and register the live Restate handlers");
    a_host_started_process_runs_under_the_behaviour_its_creation_recorded(
        ProcessHost::Live(host),
        &format!("recorded-process-live-{}", nonce()),
    )
    .await;
}
