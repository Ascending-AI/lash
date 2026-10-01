//! FIG-4379: a session records each installed owner's plugin configuration
//! at creation, delivers it unchanged on every open, changes it only through
//! its owner's typed config commands in one revision-checked transaction, and
//! hands every scoped hook the revision its root was admitted under.

use super::*;

const PROBE: &str = "probe-config";

/// The probe namespace.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    lash_core::facade_support::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
struct ProbeConfig {
    turn_cap: u64,
}

/// The probe owner's typed refusal: its cap may be raised, never lowered,
/// and never above what the core turn budget bounds.
#[derive(
    Clone,
    Debug,
    PartialEq,
    Eq,
    serde::Serialize,
    serde::Deserialize,
    lash_core::facade_support::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(tag = "kind", rename_all = "snake_case")]
enum ProbeRefusal {
    CapLowered { recorded: u64, requested: u64 },
    CapAboveBudget { cap: u64, budget: u64 },
}

impl std::fmt::Display for ProbeRefusal {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CapLowered {
                recorded,
                requested,
            } => write!(
                formatter,
                "the probe cap may only be raised: recorded {recorded}, requested {requested}"
            ),
            Self::CapAboveBudget { cap, budget } => write!(
                formatter,
                "the probe cap {cap} exceeds the session's turn budget of {budget}"
            ),
        }
    }
}

struct ProbeOwner;

impl lash_core::ConfigOwner for ProbeOwner {
    type Create = ProbeConfig;
    type Recorded = ProbeConfig;
    type Refusal = ProbeRefusal;

    fn implementation(&self) -> &str {
        "probe-config:1"
    }

    /// The stated cap, or the default of 8.
    fn create(
        &self,
        input: Option<ProbeConfig>,
        _facts: lash_core::CreationFacts<'_, ProbeConfig>,
    ) -> std::result::Result<Option<ProbeConfig>, ProbeRefusal> {
        Ok(Some(input.unwrap_or(ProbeConfig { turn_cap: 8 })))
    }

    /// A cap never exceeds a bounded core turn budget, judged on the final
    /// candidate of a transaction.
    fn validate(
        &self,
        value: &ProbeConfig,
        _base: Option<&ProbeConfig>,
        facts: &lash_core::CandidateFacts<'_>,
    ) -> std::result::Result<(), ProbeRefusal> {
        match facts.core.turn_budget {
            lash_core::TurnBudget::Bounded(budget) if value.turn_cap > budget.get() as u64 => {
                Err(ProbeRefusal::CapAboveBudget {
                    cap: value.turn_cap,
                    budget: budget.get() as u64,
                })
            }
            _ => Ok(()),
        }
    }
}

/// Raise the probe cap.
#[derive(
    Clone, Debug, serde::Serialize, serde::Deserialize, lash_core::facade_support::JsonSchema,
)]
#[schemars(crate = "lash_core::facade_support::schemars")]
#[serde(deny_unknown_fields)]
struct RaiseProbeCap {
    turn_cap: u64,
}

impl lash_core::ConfigCommand for RaiseProbeCap {
    type Owner = ProbeOwner;
    /// The cap it replaced.
    type Output = u64;
    const NAME: &'static str = "raise_probe_cap";
}

fn capped(cap: u64) -> serde_json::Value {
    serde_json::json!({ "turn_cap": cap })
}

fn raise(cap: u64) -> crate::config::ConfigTransaction {
    crate::config::ConfigTransaction::of(RaiseProbeCap { turn_cap: cap })
}

/// What a build or a hook saw: the revision and the probe's namespace.
type Seen = (u64, Option<serde_json::Value>);

/// An owner of the `probe-config` namespace, reporting what every build and
/// every before-turn hook was handed.
#[derive(Default)]
struct ProbeFactory {
    builds: StdMutex<Vec<Seen>>,
    hooks: Arc<StdMutex<Vec<Seen>>>,
}

struct ProbePlugin {
    hooks: Arc<StdMutex<Vec<Seen>>>,
}

impl lash_core::facade_support::SessionPlugin for ProbePlugin {
    fn id(&self) -> &'static str {
        PROBE
    }

    fn register(
        &self,
        reg: &mut lash_core::facade_support::PluginRegistrar,
    ) -> std::result::Result<(), lash_core::PluginError> {
        let hooks = Arc::clone(&self.hooks);
        reg.turn().before(Arc::new(
            move |ctx: lash_core::facade_support::TurnHookContext| {
                hooks.lock_recover().push((
                    ctx.plugin_config.revision,
                    ctx.plugin_config.config.get(PROBE).cloned(),
                ));
                Box::pin(async { Ok(Vec::new()) })
            },
        ));
        Ok(())
    }
}

impl lash_core::facade_support::PluginFactory for ProbeFactory {
    fn id(&self) -> &'static str {
        PROBE
    }

    fn build(
        &self,
        ctx: &lash_core::facade_support::PluginSessionContext,
    ) -> std::result::Result<
        Arc<dyn lash_core::facade_support::SessionPlugin>,
        lash_core::PluginError,
    > {
        self.builds.lock_recover().push((
            ctx.plugin_config.revision,
            ctx.plugin_config.config.get(PROBE).cloned(),
        ));
        Ok(Arc::new(ProbePlugin {
            hooks: Arc::clone(&self.hooks),
        }))
    }

    fn register_config(
        &self,
        registrar: &mut lash_core::ConfigRegistrar,
    ) -> std::result::Result<(), lash_core::ConfigRegistrationError> {
        registrar.owner(ProbeOwner)?;
        registrar.command::<RaiseProbeCap>(|recorded, command| {
            if command.turn_cap < recorded.turn_cap {
                return Err(ProbeRefusal::CapLowered {
                    recorded: recorded.turn_cap,
                    requested: command.turn_cap,
                });
            }
            Ok(lash_core::OwnerChange {
                recorded: ProbeConfig {
                    turn_cap: command.turn_cap,
                },
                output: recorded.turn_cap,
            })
        })
    }
}

async fn probe_core(backend: lash_core::Backend, probe: &Arc<ProbeFactory>) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .plugin(probe.clone())
    .serve_test_model(mock_provider(), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())
}

fn stating(cap: u64) -> Result<lash_core::PluginOptions> {
    Ok(lash_core::PluginOptions::typed(
        PROBE,
        ProbeConfig { turn_cap: cap },
    )?)
}

/// The session's recorded state, read from its store.
async fn recorded_state(core: &LashCore, id: &str) -> Result<lash_core::RuntimeSessionState> {
    let id = SessionId::from(id);
    let store = crate::session::resolve_existing_session(&core.store_factory, &id).await?;
    crate::session::load_state_from_store(&id, &core.policy, &store).await
}

/// The turn budget the session's durable config head records.
async fn recorded_turn_budget(core: &LashCore, id: &str) -> Result<crate::TurnBudget> {
    Ok(lash_core::SessionCommitStore::load_session_head_meta(
        core.store_factory.as_ref(),
        &SessionId::from(id),
    )
    .await?
    .expect("persisted head")
    .config
    .turn_budget)
}

fn creation_refusal(error: &crate::EmbedError) -> &lash_core::SessionConfigRefusal {
    let crate::EmbedError::Session(lash_core::SessionError::SessionConfigRefused(refusal)) = error
    else {
        panic!("expected a typed session config refusal, got: {error:?}");
    };
    refusal
}

/// Apply `transaction` written against `revision` under `id`.
async fn apply(
    session: &crate::LashSession,
    id: &str,
    revision: u64,
    transaction: crate::config::ConfigTransaction,
) -> Result<crate::config::ConfigTransactionOutcome> {
    session
        .admin()
        .config()
        .apply(crate::config::ConfigWrite::new(id, revision), transaction)
        .await
}

/// The configuration stated at creation reaches the owner's build and its
/// hooks on a host open and on the engine driver's reopen.
#[tokio::test]
async fn created_config_reaches_every_open() -> Result<()> {
    let probe = Arc::new(ProbeFactory::default());
    let core = probe_core(double_backend().await, &probe).await?;
    let durable = core
        .session("probe-session")
        .create(crate::SessionCreation {
            spec: lash_core::facade_support::SessionSpec::new().plugin_options(stating(12)?),
            ..Default::default()
        })
        .await?;
    probe.builds.lock_recover().clear();
    let session = core.session("probe-session").open().await?;
    session.send(TurnInput::text("host open")).output().await?;
    drop(session);
    durable
        .send(TurnInput::text("open through the engine"))
        .output()
        .await?;
    let builds = probe.builds.lock_recover().clone();
    let hooks = probe.hooks.lock_recover().clone();
    assert!(builds.len() >= 2, "host and engine opens build: {builds:?}");
    assert!(hooks.len() >= 2, "both turns run the hook: {hooks:?}");
    for (_, value) in builds.iter().chain(hooks.iter()) {
        assert_eq!(value, &Some(capped(12)));
    }
    Ok(())
}

/// The command catalog is generated from the installed registrations: every
/// core command and every plugin command, each with its generated schemas,
/// described at the session's current revision.
#[tokio::test]
async fn the_command_catalog_lists_every_registered_command() -> Result<()> {
    let probe = Arc::new(ProbeFactory::default());
    let core = probe_core(double_backend().await, &probe).await?;
    let session = core.session("probe-catalog").created().await.open().await?;
    let config = session.admin().config();
    let catalog = config.commands().await?;
    assert_eq!(catalog.revision, config.revision().await?);
    let names = |owner: &str| {
        catalog
            .commands
            .iter()
            .filter(|command| command.owner == owner)
            .map(|command| command.command.as_str())
            .collect::<Vec<_>>()
    };
    assert_eq!(
        names(crate::config::CORE_CONFIG_OWNER),
        [
            "set_attachment_acceptance",
            "set_autonomy",
            "set_charge_safety",
            "set_generation",
            "set_max_tool_calls",
            "set_model",
            "set_no_progress_budget",
            "set_reasoning",
            "set_tool_access",
            "set_turn_budget",
        ],
        "the catalog lists each owner's commands by name"
    );
    assert_eq!(names(PROBE), ["raise_probe_cap"]);
    let raise = catalog
        .commands
        .iter()
        .find(|command| command.owner == PROBE)
        .expect("probe command");
    assert!(
        raise.input_schema.to_string().contains("turn_cap"),
        "the input schema is the command's: {}",
        raise.input_schema
    );
    assert!(
        raise.refusal_schema.to_string().contains("cap_lowered"),
        "the refusal schema is the owner's: {}",
        raise.refusal_schema
    );
    Ok(())
}

/// An owner asked at creation with nothing stated records its default.
#[tokio::test]
async fn unstated_namespace_records_owner_default() -> Result<()> {
    let probe = Arc::new(ProbeFactory::default());
    let core = probe_core(double_backend().await, &probe).await?;
    drop(
        core.session("probe-default")
            .create(crate::SessionCreation::default())
            .await?,
    );
    let state = recorded_state(&core, "probe-default").await?;
    assert_eq!(state.authority.plugin_config.get(PROBE), Some(&capped(8)));
    Ok(())
}

/// A namespace no installed plugin owns is refused typed at creation, a
/// command no installed plugin owns is refused typed at submission, and
/// neither writes anything.
#[tokio::test]
async fn unknown_owner_is_refused_at_create_and_submit() -> Result<()> {
    let probe = Arc::new(ProbeFactory::default());
    let core = probe_core(double_backend().await, &probe).await?;
    let Err(error) = core
        .session("probe-unknown-create")
        .create(crate::SessionCreation {
            spec: lash_core::facade_support::SessionSpec::new().plugin_options(
                lash_core::PluginOptions::typed("no-such-plugin", serde_json::json!({ "k": 1 }))?,
            ),
            ..Default::default()
        })
        .await
    else {
        panic!("an unowned namespace is refused at creation");
    };
    assert_eq!(
        creation_refusal(&error).downcast_ref::<lash_core::UnknownPluginConfigOwner>(),
        Some(&lash_core::UnknownPluginConfigOwner {
            plugin_ids: vec!["no-such-plugin".to_string()],
        })
    );
    assert!(
        recorded_state(&core, "probe-unknown-create").await.is_err(),
        "a refused creation records no session"
    );

    drop(
        core.session("probe-unknown-command")
            .create(crate::SessionCreation::default())
            .await?,
    );
    let before = recorded_state(&core, "probe-unknown-command").await?;
    let session = core.session("probe-unknown-command").open().await?;
    let error = apply(
        &session,
        "unknown-owner",
        before.config_revision,
        raise(20).then_entry(crate::config::ConfigCommandEntry {
            owner: "no-such-plugin".to_string(),
            command: "set_k".to_string(),
            args: serde_json::json!({ "k": 1 }),
        }),
    )
    .await
    .expect_err("a command no plugin owns is refused at submission");
    assert!(
        matches!(
            &error,
            crate::EmbedError::ConfigSubmit(crate::config::ConfigSubmitError::UnknownOwner { owner })
                if owner == "no-such-plugin"
        ),
        "{error:?}"
    );
    let after = recorded_state(&core, "probe-unknown-command").await?;
    assert_eq!(after.config_revision, before.config_revision);
    assert_eq!(
        after.authority.plugin_config,
        before.authority.plugin_config
    );
    Ok(())
}

/// A command is reduced by its namespace's owner: an admitted change is one
/// revision-advancing durable write with the command's typed output, which a
/// reopen delivers; a refused change settles with the owner's typed refusal
/// and writes nothing.
#[tokio::test]
async fn a_command_is_owner_reduced_and_revision_checked() -> Result<()> {
    let probe = Arc::new(ProbeFactory::default());
    let core = probe_core(double_backend().await, &probe).await?;
    drop(
        core.session("probe-command")
            .create(crate::SessionCreation {
                spec: lash_core::facade_support::SessionSpec::new().plugin_options(stating(12)?),
                ..Default::default()
            })
            .await?,
    );
    let created = recorded_state(&core, "probe-command").await?;
    let session = core.session("probe-command").open().await?;
    let raised_outcome = apply(&session, "raise-20", created.config_revision, raise(20)).await?;
    assert_eq!(
        raised_outcome,
        crate::config::ConfigTransactionOutcome::Applied {
            base_revision: created.config_revision,
            revision: created.config_revision + 1,
            outputs: vec![serde_json::json!(12)],
        },
        "the command's typed output is the cap it replaced"
    );
    let raised = recorded_state(&core, "probe-command").await?;
    assert_eq!(raised.authority.plugin_config.get(PROBE), Some(&capped(20)));
    assert_eq!(raised.config_revision, created.config_revision + 1);

    let refused = apply(&session, "lower-5", raised.config_revision, raise(5)).await?;
    let crate::config::ConfigTransactionOutcome::Refused { refusal } = refused else {
        panic!("the owner refuses lowering its cap: {refused:?}");
    };
    assert_eq!(
        (
            refusal.index,
            refusal.owner.as_str(),
            refusal.command.as_deref()
        ),
        (Some(0), PROBE, Some("raise_probe_cap"))
    );
    assert_eq!(
        serde_json::from_value::<ProbeRefusal>(refusal.refusal)?,
        ProbeRefusal::CapLowered {
            recorded: 20,
            requested: 5,
        }
    );
    let unchanged = recorded_state(&core, "probe-command").await?;
    assert_eq!(unchanged.config_revision, raised.config_revision);
    assert_eq!(
        unchanged.authority.plugin_config,
        raised.authority.plugin_config
    );

    drop(session);
    probe.builds.lock_recover().clear();
    drop(core.session("probe-command").open().await?);
    assert_eq!(
        *probe.builds.lock_recover(),
        vec![(raised.config_revision, Some(capped(20)))],
        "a reopen delivers the changed configuration at its revision"
    );
    Ok(())
}

/// A transaction across owners applies all or none, and every touched owner
/// judges the final candidate: a core turn budget and a probe cap it bounds
/// are judged together, and a refusal by either publishes neither.
#[tokio::test]
async fn a_transaction_across_owners_is_all_or_none() -> Result<()> {
    let probe = Arc::new(ProbeFactory::default());
    let core = probe_core(double_backend().await, &probe).await?;
    drop(
        core.session("probe-atomic")
            .create(crate::SessionCreation {
                spec: lash_core::facade_support::SessionSpec::new().plugin_options(stating(12)?),
                ..Default::default()
            })
            .await?,
    );
    let created = recorded_state(&core, "probe-atomic").await?;
    let created_budget = recorded_turn_budget(&core, "probe-atomic").await?;
    let session = core.session("probe-atomic").open().await?;
    let budget = |turns: usize| crate::config::SetTurnBudget {
        turn_budget: crate::TurnBudget::bounded(turns),
    };

    // The budget alone admits the recorded cap of 12; with a cap of 40 in
    // the same transaction the final candidate is refused, and neither the
    // budget nor the cap is published.
    let refused = apply(
        &session,
        "budget-and-cap-refused",
        created.config_revision,
        crate::config::ConfigTransaction::of(budget(30)).then(RaiseProbeCap { turn_cap: 40 }),
    )
    .await?;
    let crate::config::ConfigTransactionOutcome::Refused { refusal } = refused else {
        panic!("the final candidate is refused: {refused:?}");
    };
    assert_eq!((refusal.index, refusal.owner.as_str()), (None, PROBE));
    assert_eq!(
        serde_json::from_value::<ProbeRefusal>(refusal.refusal)?,
        ProbeRefusal::CapAboveBudget {
            cap: 40,
            budget: 30,
        }
    );
    let unchanged = recorded_state(&core, "probe-atomic").await?;
    assert_eq!(unchanged.config_revision, created.config_revision);
    assert_eq!(
        recorded_turn_budget(&core, "probe-atomic").await?,
        created_budget
    );
    assert_eq!(
        unchanged.authority.plugin_config.get(PROBE),
        Some(&capped(12))
    );

    // In order: a raised cap then a budget that bounds it applies as one
    // step.
    let applied = apply(
        &session,
        "cap-and-budget",
        created.config_revision,
        crate::config::ConfigTransaction::of(RaiseProbeCap { turn_cap: 40 }).then(budget(50)),
    )
    .await?;
    assert_eq!(
        applied,
        crate::config::ConfigTransactionOutcome::Applied {
            base_revision: created.config_revision,
            revision: created.config_revision + 1,
            outputs: vec![serde_json::json!(12), serde_json::Value::Null],
        }
    );
    let changed = recorded_state(&core, "probe-atomic").await?;
    assert_eq!(
        recorded_turn_budget(&core, "probe-atomic").await?,
        crate::TurnBudget::bounded(50)
    );
    assert_eq!(
        changed.authority.plugin_config.get(PROBE),
        Some(&capped(40))
    );
    Ok(())
}

/// A transaction written against a revision the session has moved past
/// settles stale without running a reducer, and publishes nothing; the
/// caller's base is never refreshed for it.
#[tokio::test]
async fn a_stale_transaction_publishes_nothing() -> Result<()> {
    let probe = Arc::new(ProbeFactory::default());
    let core = probe_core(double_backend().await, &probe).await?;
    drop(
        core.session("probe-stale")
            .create(crate::SessionCreation {
                spec: lash_core::facade_support::SessionSpec::new().plugin_options(stating(12)?),
                ..Default::default()
            })
            .await?,
    );
    let session = core.session("probe-stale").open().await?;
    session.admin().config().configure(raise(20)).await?;
    let current = recorded_state(&core, "probe-stale").await?;
    let stale = apply(&session, "stale", current.config_revision - 1, raise(30)).await?;
    assert_eq!(
        stale,
        crate::config::ConfigTransactionOutcome::Stale {
            expected: current.config_revision - 1,
            actual: current.config_revision,
        }
    );
    let after = recorded_state(&core, "probe-stale").await?;
    assert_eq!(after.config_revision, current.config_revision);
    assert_eq!(after.authority.plugin_config.get(PROBE), Some(&capped(20)));
    Ok(())
}

/// On a Restate double that replays every await, each root's hooks see the
/// configuration and revision that root was admitted under, however often it
/// is redriven; the root after a config transaction sees its revision.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn redriven_roots_see_their_admitted_revision() -> Result<()> {
    let probe = Arc::new(ProbeFactory::default());
    let backend = double_backend_over(
        lash_restate_test::ServerConfig::default().always_replay(true),
        |stores| stores,
    )
    .await;
    let core = probe_core(backend, &probe).await?;
    drop(
        core.session("probe-admitted")
            .create(crate::SessionCreation {
                spec: lash_core::facade_support::SessionSpec::new().plugin_options(stating(12)?),
                ..Default::default()
            })
            .await?,
    );
    let session = core.session("probe-admitted").open().await?;
    let admitted = recorded_state(&core, "probe-admitted")
        .await?
        .config_revision;
    session.send(TurnInput::text("first root")).output().await?;
    let first = std::mem::take(&mut *probe.hooks.lock_recover());
    assert!(!first.is_empty());
    assert!(
        first
            .iter()
            .all(|seen| seen == &(admitted, Some(capped(12)))),
        "every run of the first root sees its admission: {first:?}"
    );

    session.admin().config().configure(raise(20)).await?;
    let committed = recorded_state(&core, "probe-admitted")
        .await?
        .config_revision;
    assert!(committed > admitted);
    let settling = std::mem::take(&mut *probe.hooks.lock_recover());
    assert!(
        settling
            .iter()
            .all(|seen| seen == &(admitted, Some(capped(12)))),
        "no root runs under a revision it was not admitted with while the transaction settles: \
         {settling:?}"
    );
    session
        .send(TurnInput::text("second root"))
        .output()
        .await?;
    let second = probe.hooks.lock_recover().clone();
    assert!(!second.is_empty());
    assert!(
        second
            .iter()
            .all(|seen| seen == &(committed, Some(capped(20)))),
        "the next root is admitted under the transaction's revision: {second:?}"
    );
    Ok(())
}

/// A fork records the configuration its fork point's frame captured.
#[tokio::test]
async fn a_fork_captures_the_config_of_its_fork_point() -> Result<()> {
    let probe = Arc::new(ProbeFactory::default());
    let core = probe_core(double_backend().await, &probe).await?;
    drop(
        core.session("probe-fork-source")
            .create(crate::SessionCreation {
                spec: lash_core::facade_support::SessionSpec::new().plugin_options(stating(12)?),
                ..Default::default()
            })
            .await?,
    );
    let session = core.session("probe-fork-source").open().await?;
    session
        .send(TurnInput::text("before the fork"))
        .output()
        .await?;
    let source = recorded_state(&core, "probe-fork-source").await?;
    let point = source
        .session_graph
        .leaf_node_id
        .clone()
        .expect("the source has a leaf");
    core.pin(&point).await?;
    core.fork_at(crate::ForkRequest {
        session_id: "probe-fork".into(),
        node_id: point.clone(),
        relation: lash_core::SessionRelation::Fork {
            source_session_id: "probe-fork-source".into(),
            source_node_id: point,
        },
        observed_processes: Vec::new(),
    })
    .await?;
    let fork = recorded_state(&core, "probe-fork").await?;
    assert_eq!(fork.authority.plugin_config.get(PROBE), Some(&capped(12)));
    Ok(())
}
