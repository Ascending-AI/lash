use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountedTools {
    calls: Arc<AtomicUsize>,
    value: Arc<AtomicUsize>,
    refused: Arc<std::sync::atomic::AtomicBool>,
    refreshes: Arc<AtomicUsize>,
}
impl CountedTools {
    fn definition(&self) -> lash_vocabulary::tool_contract::ToolDefinition {
        lash_vocabulary::tool_contract::ToolDefinition::raw(
            "environment-tool",
            "environment.work",
            format!("revision {}", self.value.load(Ordering::SeqCst)),
            serde_json::json!({"type":"object"}),
            serde_json::json!({"type":"null"}),
        )
        .unwrap()
        .with_execution(std::time::Duration::from_secs(1))
    }
}
#[async_trait::async_trait]
impl crate::ToolProvider for CountedTools {
    fn refresh_environment(&self) {
        self.refreshes.fetch_add(1, Ordering::SeqCst);
    }
    fn tool_manifests(
        &self,
        _: &crate::OwnerConfigView,
    ) -> Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::plugin_error::PluginError,
    > {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.refused.load(Ordering::SeqCst) {
            return Err(lash_execution_contracts::PluginError::Session(
                "refused observation".into(),
            ));
        }
        Ok(vec![self.definition().manifest])
    }
    fn resolve_contract(
        &self,
        _: &str,
        _: &crate::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        Some(Arc::new(self.definition().contract))
    }
    async fn execute(&self, _: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolOutcome::err("no tool").into()
    }
}
struct UnaffectedTools(Arc<AtomicUsize>);
#[async_trait::async_trait]
impl crate::ToolProvider for UnaffectedTools {
    fn tool_manifests(
        &self,
        _: &crate::OwnerConfigView,
    ) -> Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        lash_execution_contracts::PluginError,
    > {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Vec::new())
    }
    fn resolve_contract(
        &self,
        _: &str,
        _: &crate::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        None
    }
    async fn execute(&self, _: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        crate::ToolOutcome::err("no tool").into()
    }
}
struct Fixture {
    driver: Box<RuntimeTurnDriver<'static>>,
    calls: Arc<AtomicUsize>,
    value: Arc<AtomicUsize>,
    refused: Arc<std::sync::atomic::AtomicBool>,
    _claim: Box<dyn Send>,
    machine: TurnMachine,
    services: crate::RuntimeServices,
    refreshes: Arc<AtomicUsize>,
}
async fn fixture() -> Fixture {
    fixture_with(Vec::new()).await
}

async fn fixture_with(plugins: Vec<Arc<dyn crate::PluginFactory>>) -> Fixture {
    let backend = crate::testing::sqlite_recording_backend().await;
    let session_id = lash_kernel_value::identity::SessionId::fixture("environment-laws");
    let store =
        crate::testing::runtime_helpers::recording_session_store(&backend, session_id.clone())
            .await;
    let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let value = Arc::new(AtomicUsize::new(0));
    let refreshes = Arc::new(AtomicUsize::new(0));
    let refused = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut runtime = crate::testing::runtime_helpers::TestRuntime::new(
        &backend,
        crate::testing::TestProvider::builder().build(),
    )
    .plugins(plugins)
    .tools(Arc::new(CountedTools {
        calls: calls.clone(),
        value: value.clone(),
        refused: refused.clone(),
        refreshes: refreshes.clone(),
    }))
    .store(store)
    .with_session_id(session_id)
    .build()
    .await;
    let run = lash_vocabulary::identity::TurnId::from("environment-laws");
    let actor = lash_durable::ActorKey::session(runtime.state.session_id.as_str()).unwrap();
    let mut mail = lash_durable::MailTx::new();
    mail.append(
        actor.clone(),
        lash_durable::MailKind::new("law.environment-laws"),
        String::new(),
    );
    backend
        .durable()
        .commit_mail(mail, lash_durable::CommitLabel::MAIL_SESSION)
        .await
        .unwrap();
    let node = backend
        .durable()
        .register_node(&lash_durable::NodeSpec {
            node: lash_durable::NodeId::new("environment-laws"),
            decodes: backend.formats().decodes(),
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let claimed = backend.durable().claim(&node, 1).await.unwrap();
    let controller = crate::ActorContext::new(
        backend,
        actor,
        claimed[0].epoch,
        crate::AdmittedScope::turn(runtime.state.session_id.clone(), run.clone()),
        tokio_util::sync::CancellationToken::new(),
        std::sync::Arc::new(lash_durable::NoProbe),
    );
    let _claim = controller.session_claim_guard();
    crate::runtime::durable::head::SessionOwner::default()
        .head(&controller, &runtime.state.session_id)
        .await
        .expect("hydrate the claimed session projection");
    let mut tx = controller.begin().await.unwrap();
    tx.write(lash_durable::DomainWrite::Turn(
        lash_durable::domain::TurnWrite::Admit {
            session: runtime.state.session_id.clone(),
            run: run.clone(),
            admission: crate::store::RunAdmissionRecord::Turn {
                took: crate::store::AdmittedTurnRows::Inputs {
                    ids: crate::store::AdmittedInputIds::one(
                        lash_vocabulary::identity::InputId::from("environment-input"),
                    ),
                },
                trace: None,
            },
            turn_deadline: None,
        },
    ));
    controller
        .commit(tx, lash_durable::CommitLabel::TURN_ADMIT)
        .await
        .unwrap();
    let (observer, _observations) = TurnObserver::unread();
    let mut turn = Box::pin(runtime.prepare_durable_turn(
        &controller,
        &run,
        crate::runtime::logical_turn::LogicalTurnAdmissions::new(Vec::new()),
        crate::runtime::turn_loop::TurnStart::Fresh(None),
        &observer,
    ))
    .await
    .expect("prepare the fixture's turn");
    let services = runtime.services.clone();
    let machine = turn.driver.prepare_machine(turn.window, turn.messages, 0);
    Fixture {
        driver: turn.driver,
        calls,
        value,
        refused,
        _claim: Box::new(_claim),
        machine,
        services,
        refreshes,
    }
}

async fn boundary(f: &mut Fixture, round: usize) {
    f.driver.refresh_execution_environment().await.unwrap();
    let adoption = f.driver.adopted_environment.as_ref().unwrap().clone();
    f.driver.run_config.install_environment(round, &adoption);
}
async fn publish(f: &mut Fixture, cell: &str) -> lash_core_store::checkpoint_graph::NodeId {
    publish_with_ack(f, cell, true).await
}
async fn stage(
    f: &mut Fixture,
    cell: &str,
) -> (
    lash_durable::ActorTx,
    lash_core_store::checkpoint_graph::NodeId,
) {
    use lash_durable::{
        DomainWrite,
        domain::{CellId, ExecKey, ExecRootWrite},
    };
    let mut tx = f.driver.scoped_effect_controller.begin().await.unwrap();
    let delta = f.machine.checkpoint_delta();
    let phase = crate::runtime::durable::session::PhaseCheckpoint {
        saved: delta.saved,
        delivered: Vec::new(),
        before_turn: Vec::new(),
        trace: None,
        config: Some(f.driver.run_config.clone()),
        cell: f.driver.cell_start.clone(),
    };
    let (root, _stored) =
        crate::runtime::durable::phase_root::StagedPhase::of(&phase, &delta.held, &delta.links)
            .unwrap()
            .staged_in(&mut tx);
    tx.write(DomainWrite::Turn(
        lash_durable::domain::TurnWrite::Advance {
            session: f.driver.session_id.clone(),
            run: f.driver.turn_id.clone(),
            phase: lash_durable::domain::PhaseAdvance::Tools {
                run: lash_durable::domain::RunSeq(1),
                checkpoint: root,
            },
            iteration: 0,
        },
    ));
    tx.write(DomainWrite::ExecRoot(ExecRootWrite::Put {
        exec: ExecKey::Cell(
            f.driver.session_id.clone(),
            f.driver.turn_id.clone(),
            CellId::new(cell),
        ),
        expected: None,
        expected_root: None,
        root,
        commit_id: lash_core_store::checkpoint_graph::CommitId::mint(),
    }));
    (tx, root)
}

async fn publish_with_ack(
    f: &mut Fixture,
    cell: &str,
    acknowledged: bool,
) -> lash_core_store::checkpoint_graph::NodeId {
    let adoption = f.driver.adopted_environment.as_ref().unwrap().clone();
    let (tx, root) = stage(f, cell).await;
    f.driver
        .scoped_effect_controller
        .commit(
            tx,
            lash_durable::CommitLabel::new("law.environment.publish"),
        )
        .await
        .unwrap();
    if acknowledged && let Some(publication) = &adoption.publication {
        publication.committed();
    }
    root
}

/// ENV-PROGRESS (FIG-7310): preparing without publishing a head or phase
/// cannot renew the pass budget, whatever later work refuses the model call.
#[tokio::test]
async fn env_preparation_is_not_pass_progress() {
    let mut f = fixture().await;
    let cx = f.driver.scoped_effect_controller.clone();
    let commits = cx.acknowledged_commits();
    let progress = cx.acknowledged_progress();
    // A failed call leaves the head unpublished. Its next attempt can prepare
    // the same environment again; neither preparation advanced the run.
    for attempt in 0..2 {
        boundary(&mut f, 0).await;
        assert_eq!(cx.acknowledged_commits(), commits + attempt + 1);
        assert_eq!(
            cx.acknowledged_progress(),
            progress,
            "an unpublished environment preparation is not pass progress"
        );
    }
    publish(&mut f, "progress").await;
    assert_eq!(cx.acknowledged_commits(), commits + 3);
    assert_eq!(
        cx.acknowledged_progress(),
        progress + 1,
        "publishing the environment with a phase renews the pass budget"
    );
}

/// ENV-RESTART-UNCHANGED: replacing the native sources of an unchanged
/// session reuses its committed environment and writes no head.
#[tokio::test]
async fn env_restart_unchanged_writes_no_head() {
    let mut f = fixture().await;
    boundary(&mut f, 0).await;
    publish(&mut f, "before-restart").await;
    let committed = f.driver.run_config.current_environment().unwrap().clone();
    f.driver.environment = super::super::session_environment::SessionEnvironment::materialize(
        &f.driver.session,
        f.driver
            .host
            .core
            .backend()
            .durable()
            .environments()
            .as_ref(),
    )
    .await
    .unwrap();
    f.driver.adopted_environment = None;
    boundary(&mut f, 1).await;
    assert_eq!(f.driver.run_config.current_environment(), Some(&committed));
    let (tx, _) = stage(&mut f, "after-restart").await;
    assert!(
        !tx.domain().iter().any(|write| matches!(
            write,
            lash_durable::DomainWrite::Environment(
                lash_durable::environment::EnvironmentWrite::Head { .. }
            )
        )),
        "unchanged providers must not rewrite the durable environment head after restart"
    );
}

/// ENV-UNCHANGED: clean boundaries and reads reuse versioned catalog JSON and preambles.
#[tokio::test]
async fn env_unchanged() {
    let mut f = fixture().await;
    boundary(&mut f, 0).await;
    publish(&mut f, "unchanged").await;
    f.calls.store(0, Ordering::SeqCst);
    let assemblies = f.driver.environment.assemblies.load(Ordering::SeqCst);
    let version = f.driver.adopted_environment.as_ref().unwrap().body.clone();
    let catalog = f.driver.session.shared_tool_catalog().unwrap();
    let preamble = f.driver.session.installed_environment().unwrap().preamble();
    let (result, sql) = lash_core_ids::operational_metrics::sql::collect(
        "ENV-UNCHANGED",
        "sqlite",
        f.driver.refresh_execution_environment(),
    )
    .await;
    result.unwrap();
    assert_eq!(f.driver.session.plugins().tool_catalog().unwrap(), *catalog);
    assert_eq!(
        f.calls.load(Ordering::SeqCst),
        0,
        "ENV-UNCHANGED: a clean boundary re-observed its sources"
    );
    assert_eq!(
        f.driver.environment.assemblies.load(Ordering::SeqCst),
        assemblies
    );
    assert_eq!(
        sql.statements, 0,
        "ENV-UNCHANGED: clean adoption touched SQL: {sql:?}"
    );
    assert!(Arc::ptr_eq(
        &version,
        &f.driver.adopted_environment.as_ref().unwrap().body
    ));
    assert!(Arc::ptr_eq(
        &catalog,
        &f.driver.session.shared_tool_catalog().unwrap()
    ));
    assert!(
        Arc::ptr_eq(&preamble, &f.driver.session.protocol_driver_preamble()),
        "ENV-UNCHANGED: preparing another turn rebuilt its preamble"
    );
    // Replay/read projections of this version share the same derived work.
    let projected = f.driver.session.project_environment(&version).unwrap();
    assert!(
        Arc::ptr_eq(&preamble, &projected.preamble()),
        "ENV-UNCHANGED: the same environment version rebuilt its preamble"
    );
    // An idle recorded read has no installed round to keep its projection alive.
    let plugins = f
        .driver
        .session
        .plugins()
        .host()
        .defer_session(crate::plugin::PluginSessionRequest::creation(
            &f.driver.session_id,
            crate::plugin::SessionAuthorityContext::fixture(),
        ))
        .unwrap();
    plugins.prepare_capabilities().unwrap();
    let mut services = f.services.clone();
    services.plugins = plugins;
    let reader = Session::new(services, &f.driver.session_id).await.unwrap();
    let first = reader.project_environment(&version).unwrap();
    let preamble = first.preamble();
    drop(first);
    let second = reader.project_environment(&version).unwrap();
    assert!(
        Arc::ptr_eq(&preamble, &second.preamble()),
        "ENV-UNCHANGED: an idle read discarded the version projection"
    );
}

/// ENV-ROUND: the next model sees the change, while a cell keeps its round.
/// FIG-7299: the terminal retains an ordinary turn's staged round, even
/// when that phase shares configuration with a previously committed phase.
#[tokio::test]
async fn env_round() {
    let mut f = fixture().await;
    boundary(&mut f, 0).await;
    publish(&mut f, "round-zero").await;
    let graph = f
        .driver
        .scoped_effect_controller
        .durable_reads()
        .unwrap()
        .hydrate(
            lash_durable::domain::RootOwner::TurnPhase {
                session: f.driver.session_id.clone(),
                run: f.driver.turn_id.clone(),
            },
            None,
            lash_durable::domain::graph::OWNER_READ_LIMITS,
        )
        .await
        .unwrap();
    let (phase, _) = crate::runtime::durable::phase_root::load(
        &graph,
        graph.root,
        lash_durable::domain::graph::OWNER_READ_LIMITS,
    )
    .unwrap();
    f.driver.run_config = phase.config.unwrap();
    f.driver.begin_cell(lash_sansio::EffectId(7), "typescript");
    let first = f
        .driver
        .cell_start
        .as_ref()
        .unwrap()
        .environment
        .clone()
        .unwrap();
    f.value.store(1, Ordering::SeqCst);
    f.driver.environment.refresh();
    boundary(&mut f, 1).await;
    let (held, _) = stage(&mut f, "round-one").await;
    let second = f.driver.run_config.current_environment().unwrap().clone();
    assert_ne!(&first.version, &second.version);
    assert_eq!(
        f.driver.cell_start.as_ref().unwrap().environment.as_ref(),
        Some(&first)
    );
    assert!(
        f.driver
            .session
            .installed_environment()
            .unwrap()
            .definitions()[0]
            .manifest
            .description
            .contains("revision 1")
    );
    let cx = &f.driver.scoped_effect_controller;
    let mut tx = cx.begin().await.unwrap();
    tx.append(held).unwrap();
    crate::runtime::durable::turn_terminal::write_terminal(
        cx,
        &mut tx,
        &f.driver.session_id,
        &f.driver.turn_id,
        crate::store::RunTerminalCause::Refused {
            refusal: crate::RuntimeError::new(crate::RuntimeErrorCode::Plugin, "law end").into(),
        },
        None,
    )
    .await
    .unwrap();
    cx.commit(tx, lash_durable::CommitLabel::TURN_COMMIT)
        .await
        .unwrap();
    let graph = cx
        .backend()
        .durable()
        .hydrate(
            lash_durable::domain::RootOwner::RunTranscript {
                session: f.driver.session_id.clone(),
                run: f.driver.turn_id.clone(),
            },
            None,
            lash_durable::domain::graph::OWNER_READ_LIMITS,
        )
        .await
        .unwrap();
    let mut decoder = lash_core_store::checkpoint_graph::GraphDecoder::new(
        &graph,
        lash_durable::domain::graph::OWNER_READ_LIMITS,
    );
    let transcript =
        lash_core_execution::runtime::actor::round::TurnTranscript::load(&mut decoder, graph.root)
            .unwrap();
    let versions: Vec<_> = transcript
        .entries
        .iter()
        .filter_map(|entry| match entry {
            lash_core_execution::runtime::actor::round::TranscriptEntry::Environment(node) => {
                Some(lash_durable::environment::decode_round(&mut decoder, *node).unwrap())
            }
            _ => None,
        })
        .collect();
    assert_eq!(versions, vec![first, second]);
}

/// ENV-ROUND (FIG-7316): the commit barrier and adoption consume one frontier.
/// A notice after a clean barrier decision belongs to the next model boundary.
#[tokio::test]
async fn env_round_barrier_and_adoption_share_the_captured_frontier() {
    let mut f = fixture().await;
    boundary(&mut f, 0).await;
    publish(&mut f, "frontier-zero").await;
    let first = f.driver.run_config.current_environment().unwrap().clone();
    let acknowledged = f.driver.scoped_effect_controller.acknowledged_commits();

    assert!(!f.driver.prepare_environment_sync());
    f.value.store(1, Ordering::SeqCst);
    f.driver.environment.refresh();
    boundary(&mut f, 1).await;
    assert_eq!(f.driver.run_config.current_environment(), Some(&first));
    assert_eq!(
        f.driver.scoped_effect_controller.acknowledged_commits(),
        acknowledged,
        "a sync captured clean must not commit a preparation after its barrier decision"
    );
    publish(&mut f, "frontier-one").await;

    assert!(f.driver.prepare_environment_sync());
    boundary(&mut f, 2).await;
    assert_ne!(
        f.driver.run_config.current_environment().unwrap().version,
        first.version,
        "the next boundary adopts the later notice"
    );
    publish(&mut f, "frontier-two").await;
}

/// ENV-REPLAY: hydrate the recorded round and restore its body after the head moved.
#[tokio::test]
async fn env_replay() {
    use lash_durable::domain::{CellId, ExecKey, RootOwner};
    let mut f = fixture().await;
    boundary(&mut f, 0).await;
    let root = publish(&mut f, "parked").await;
    let saved = f.driver.run_config.clone();
    let first = saved.current_environment().unwrap().clone();
    f.value.store(1, Ordering::SeqCst);
    f.driver.environment.refresh();
    boundary(&mut f, 1).await;
    publish_with_ack(&mut f, "new-head", false).await;
    let graph = f
        .driver
        .host
        .core
        .backend()
        .durable()
        .hydrate(
            RootOwner::Exec(ExecKey::Cell(
                f.driver.session_id.clone(),
                f.driver.turn_id.clone(),
                CellId::new("parked"),
            )),
            Some(root),
            lash_durable::domain::graph::OWNER_READ_LIMITS,
        )
        .await
        .unwrap();
    let (phase, _) = crate::runtime::durable::phase_root::load(
        &graph,
        root,
        lash_durable::domain::graph::OWNER_READ_LIMITS,
    )
    .unwrap();
    let decoded = phase.config.unwrap();
    assert_eq!(decoded.current_environment(), Some(&first));
    f.driver.run_config = decoded;
    f.calls.store(0, Ordering::SeqCst);
    f.driver.reinstall_tool_surface().await.unwrap();
    assert_eq!(f.driver.adopted_environment.as_ref().unwrap().round, first);
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    assert!(
        f.driver
            .session
            .installed_environment()
            .unwrap()
            .definitions()[0]
            .manifest
            .description
            .contains("revision 0")
    );
    let head = f
        .driver
        .host
        .core
        .backend()
        .durable()
        .environments()
        .environment_head(&f.driver.session_id)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(head.version, first.version);
    boundary(&mut f, 2).await;
    publish(&mut f, "after-lost-ack").await;
    assert_eq!(
        f.driver.run_config.current_environment().unwrap().version,
        head.version
    );
}

/// ENV-TAKEOVER: a new epoch reconciles once, then uses the durable head cleanly.
#[tokio::test]
async fn env_takeover() {
    let mut f = fixture().await;
    boundary(&mut f, 0).await;
    publish(&mut f, "before-takeover").await;
    let before = f.driver.run_config.current_environment().unwrap().clone();
    let cx = f.driver.scoped_effect_controller.clone();
    let mut tx = cx.begin().await.unwrap();
    tx.give_up(lash_durable::Release::Ready);
    cx.commit(
        tx,
        lash_durable::CommitLabel::new("law.environment.release"),
    )
    .await
    .unwrap();
    let backend = cx.backend().clone();
    let node = backend
        .durable()
        .register_node(&lash_durable::NodeSpec {
            node: lash_durable::NodeId::new("environment-successor"),
            decodes: backend.formats().decodes(),
            serves: Vec::new(),
            capabilities: Default::default(),
            ttl_millis: 15_000,
        })
        .await
        .unwrap();
    let claimed = backend.durable().claim(&node, 1).await.unwrap();
    let next = crate::ActorContext::new(
        backend,
        cx.actor().clone(),
        claimed[0].epoch,
        crate::AdmittedScope::turn(f.driver.session_id.clone(), f.driver.turn_id.clone()),
        CancellationToken::new(),
        Arc::new(lash_durable::NoProbe),
    );
    assert_ne!(next.epoch(), cx.epoch());
    let guard = next.session_claim_guard();
    f.driver.scoped_effect_controller = next;
    f._claim = Box::new(guard);
    f.driver.environment = super::super::session_environment::SessionEnvironment::materialize(
        &f.driver.session,
        f.driver
            .host
            .core
            .backend()
            .durable()
            .environments()
            .as_ref(),
    )
    .await
    .unwrap();
    f.calls.store(0, Ordering::SeqCst);
    boundary(&mut f, 1).await;
    publish(&mut f, "after-takeover").await;
    assert_eq!(f.calls.swap(0, Ordering::SeqCst), 1);
    assert_eq!(
        f.driver.run_config.current_environment().unwrap().version,
        before.version
    );
    boundary(&mut f, 2).await;
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}

struct CountedNamespace;
#[async_trait::async_trait]
impl crate::plugin::packages::PackageProvider for CountedNamespace {
    fn roots(
        &self,
        _: &crate::OwnerConfigView,
    ) -> std::collections::BTreeSet<lash_kernel_value::names::Segment> {
        [lash_kernel_value::names::Segment::new("environment").unwrap()].into()
    }
    fn advertise(
        &self,
        _: &crate::OwnerConfigView,
        context: &lash_kernel_value::package::PublicationContext,
    ) -> Result<
        Vec<crate::plugin::packages::PackageContribution>,
        lash_execution_contracts::PluginError,
    > {
        let advertisement = lash_kernel_value::package::PackageAdvertisement::Enumerated(
            lash_kernel_value::package::Package {
                path: lash_kernel_value::names::Vec1::new(context.root.clone()),
                members: Default::default(),
            },
        );
        Ok(vec![crate::plugin::packages::PackageContribution {
            publication: crate::plugin::packages::operation_publication(context, &advertisement)?,
            advertisement,
            routes: Vec::new(),
        }])
    }
    async fn hold(
        &self,
        _: &crate::ReferrerClaim,
        _: &[&lash_kernel_value::package::PackageAdvertisement],
    ) -> Result<(), lash_execution_contracts::process::linking::EnginePortError> {
        Ok(())
    }
}

async fn replace_source_composition(f: &mut Fixture) -> Arc<AtomicUsize> {
    let unaffected = Arc::new(AtomicUsize::new(0));
    let mut factories = crate::testing::test_standard_protocol_factories();
    factories.push(Arc::new(crate::plugin::StaticPluginFactory::new(
        crate::plugin::PluginDeclaration::initial("replacement_tools"),
        crate::PluginSpec::new()
            .with_tool_provider(
                crate::tool_provider_key!("tools"),
                Arc::new(CountedTools {
                    calls: f.calls.clone(),
                    value: f.value.clone(),
                    refused: f.refused.clone(),
                    refreshes: f.refreshes.clone(),
                }),
            )
            .with_package_provider(
                crate::tool_provider_key!("packages"),
                Arc::new(CountedNamespace),
            ),
    )));
    factories.push(Arc::new(crate::plugin::StaticPluginFactory::new(
        crate::plugin::PluginDeclaration::initial("unaffected_tools"),
        crate::PluginSpec::new().with_tool_provider(
            crate::tool_provider_key!("tools"),
            Arc::new(UnaffectedTools(unaffected.clone())),
        ),
    )));
    let host = crate::PluginHost::new(
        factories,
        crate::ExecutionBudgets::recommended(),
        crate::trace::TraceRuntime::new(Arc::new(crate::SystemClock)),
    );
    let plugins = host
        .build_session(crate::plugin::PluginSessionRequest::creation(
            &f.driver.session_id,
            crate::plugin::SessionAuthorityContext::fixture(),
        ))
        .unwrap();
    plugins.materialize().unwrap();
    let mut services = f.services.clone();
    services.plugins = plugins;
    f.driver.session = Session::new(services, &f.driver.session_id).await.unwrap();
    f.driver.environment = super::super::session_environment::SessionEnvironment::materialize(
        &f.driver.session,
        f.driver
            .host
            .core
            .backend()
            .durable()
            .environments()
            .as_ref(),
    )
    .await
    .unwrap();
    unaffected
}

/// ENV-ROUND's run-option owner is independent of the session's base head.
#[tokio::test]
async fn env_override_keeps_base_head() {
    let mut f = fixture().await;
    boundary(&mut f, 0).await;
    publish(&mut f, "base").await;
    let store = f.driver.host.core.backend().durable().environments();
    let base = store.environment_head(&f.driver.session_id).await.unwrap();
    let unaffected = replace_source_composition(&mut f).await;
    let run = f.driver.run_config.run().clone();
    let mut config = run.config().clone();
    config.turn_budget = crate::TurnBudget::bounded(3);
    f.driver.run_config =
        crate::runtime::durable::run_config::RecordedRunConfig::resolved(run.with_config(config));
    f.driver.adopted_environment = None;
    boundary(&mut f, 0).await;
    publish(&mut f, "override").await;
    assert_eq!(
        store.environment_head(&f.driver.session_id).await.unwrap(),
        base
    );
    assert_ne!(
        f.driver.run_config.current_environment().unwrap().owner,
        base.as_ref().unwrap().owner
    );
    f.calls.store(0, Ordering::SeqCst);
    boundary(&mut f, 1).await;
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    assert!(
        f.driver
            .adopted_environment
            .as_ref()
            .unwrap()
            .body
            .fragments()
            .keys()
            .all(|source| !source.0.contains("test_tools"))
    );
    // FIG-7407: another adoption may reuse collected metadata, but must
    // prepare its own lifetime owner rather than revive the preceding run's.
    let first_owner = f
        .driver
        .run_config
        .current_environment()
        .unwrap()
        .owner
        .clone();
    f.driver.adopted_environment = None;
    let (_, sql) = lash_core_ids::operational_metrics::sql::collect(
        "override-repeat",
        "sqlite",
        boundary(&mut f, 1),
    )
    .await;
    assert!(
        !sql.shapes
            .keys()
            .any(|shape| shape.contains("artifact_refs")
                && shape.starts_with("select")
                && shape.contains("blobs.encoding")),
        "FIG-7453: unchanged override hydrated immutable package payloads again: {sql:?}"
    );
    publish(&mut f, "equal-override-owner").await;
    assert_ne!(
        f.driver.run_config.current_environment().unwrap().owner,
        first_owner
    );
    assert_eq!(
        store.environment_head(&f.driver.session_id).await.unwrap(),
        base
    );
    unaffected.store(0, Ordering::SeqCst);
    f.calls.store(0, Ordering::SeqCst);
    f.value.store(1, Ordering::SeqCst);
    f.driver
        .session
        .plugins()
        .environment_config_committed(&["replacement_tools".into()]);
    boundary(&mut f, 2).await;
    publish(&mut f, "override-delta").await;
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        unaffected.load(Ordering::SeqCst),
        0,
        "ENV-DELTA: a run override re-observed an unchanged source"
    );
    f.refreshes.store(0, Ordering::SeqCst);
    f.driver.environment.refresh();
    boundary(&mut f, 3).await;
    publish(&mut f, "override-refresh").await;
    assert_eq!(
        f.refreshes.load(Ordering::SeqCst),
        1,
        "explicit refresh must reach override sources"
    );
    assert_eq!(
        store.environment_head(&f.driver.session_id).await.unwrap(),
        base
    );
}

/// A refusal keeps both the adopted head and the unacknowledged dirty prefix.
#[tokio::test]
async fn env_refusal_keeps_dirty_frontier() {
    let source = lash_core_store::environment::SourceId("racing-source".into());
    let error = super::super::session_environment::EnvironmentAdmissionError::Source(
        crate::plugin::environment_sources::SourceCollectionError::ChangedDuringSnapshot(
            source.clone(),
        ),
    )
    .into_runtime_error();
    let Some(crate::RuntimeErrorCause::PluginOperation { failure }) = error.cause else {
        panic!("collection refusal lost its typed cause")
    };
    let decoded: crate::plugin::environment_sources::SourceCollectionError =
        serde_json::from_value(failure.payload).unwrap();
    assert!(
        matches!(decoded, crate::plugin::environment_sources::SourceCollectionError::ChangedDuringSnapshot(id) if id == source)
    );
    let error = super::super::session_environment::EnvironmentAdmissionError::from(
        lash_durable::environment::EnvironmentRefusal::HeadConflict,
    )
    .into_runtime_error();
    let Some(crate::RuntimeErrorCause::PluginOperation { failure }) = error.cause else {
        panic!("owner refusal lost its typed cause")
    };
    let decoded: lash_durable::environment::EnvironmentRefusal =
        serde_json::from_value(failure.payload).unwrap();
    assert_eq!(
        decoded,
        lash_durable::environment::EnvironmentRefusal::HeadConflict
    );
    let mut f = fixture().await;
    boundary(&mut f, 0).await;
    publish(&mut f, "before-refusal").await;
    let store = f.driver.host.core.backend().durable().environments();
    let head = store.environment_head(&f.driver.session_id).await.unwrap();
    f.driver.environment.refresh();
    f.refused.store(true, Ordering::SeqCst);
    assert!(f.driver.refresh_execution_environment().await.is_err());
    assert_eq!(
        store.environment_head(&f.driver.session_id).await.unwrap(),
        head
    );
    f.refused.store(false, Ordering::SeqCst);
    f.value.store(2, Ordering::SeqCst);
    boundary(&mut f, 1).await;
    publish(&mut f, "after-refusal").await;
    assert_ne!(
        store
            .environment_head(&f.driver.session_id)
            .await
            .unwrap()
            .unwrap()
            .version,
        head.unwrap().version
    );
}

/// Contributor-only invalidations change the next round; clean rounds and replay call nothing.
#[tokio::test]
async fn env_catalog_only_owner() {
    let calls = Arc::new(AtomicUsize::new(0));
    let remove = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let callback_calls = calls.clone();
    let removal = remove.clone();
    let factory = crate::plugin::StaticPluginFactory::new(
        crate::plugin::PluginDeclaration::initial("catalog-only"),
        crate::PluginSpec::new().with_tool_catalog_contributor(
            crate::hook_key!("catalog"),
            Arc::new(move |_| {
                callback_calls.fetch_add(1, Ordering::SeqCst);
                Ok(crate::plugin::ToolCatalogContribution {
                    remove: if removal.load(Ordering::SeqCst) {
                        vec![lash_kernel_value::names::QualifiedOperation {
                            package: lash_kernel_value::names::PackagePath::parse("environment")
                                .unwrap(),
                            name: lash_kernel_value::names::Segment::new("work").unwrap(),
                        }]
                    } else {
                        Vec::new()
                    },
                })
            }),
        ),
    );
    let mut f = fixture_with(vec![Arc::new(factory)]).await;
    boundary(&mut f, 0).await;
    publish(&mut f, "catalog-first").await;
    let saved = f.driver.run_config.clone();
    calls.store(0, Ordering::SeqCst);
    boundary(&mut f, 1).await;
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    remove.store(true, Ordering::SeqCst);
    f.driver
        .session
        .plugins()
        .environment_config_committed(&["catalog-only".into()]);
    boundary(&mut f, 2).await;
    publish(&mut f, "catalog-next").await;
    assert_eq!(calls.swap(0, Ordering::SeqCst), 1);
    assert!(
        f.driver
            .session
            .installed_environment()
            .unwrap()
            .definitions()
            .is_empty()
    );
    f.driver.run_config = saved;
    f.driver.reinstall_tool_surface().await.unwrap();
    assert_eq!(
        f.driver
            .session
            .installed_environment()
            .unwrap()
            .definitions()
            .len(),
        1
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
}
