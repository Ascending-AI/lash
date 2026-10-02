//! The turn budget, autonomy, no-progress budget and charge safety are
//! recorded session config (FIG-4376): stated at creation and snapshotted per
//! root in its recorded `ResolvedRun`. One engine runs sessions created with
//! different budgets, and each keeps its own across the engine's reopen.

use super::*;

/// The budget of the core every law builds: neither session's own, so a
/// session that ran under it instead of what it recorded is visible.
const CORE_DEFAULT_TURNS: usize = 3;
/// The bounded session's recorded budget.
const BOUNDED_TURNS: usize = 2;
/// Tool results a turn gathers before the model answers: past every bound
/// here, so only an unbounded turn gets there.
const TOOL_CALLS_BEFORE_ANSWER: usize = 5;

const CHAT: &str = "recorded-controls-chat";
const PULSAR: &str = "recorded-controls-pulsar";

/// Tool results in the request since the turn's input: the turn's
/// iterations so far.
fn tool_results_this_turn(request: &LlmRequest) -> usize {
    let mut results = 0;
    for message in request.messages.iter().rev() {
        let answered = message
            .blocks
            .iter()
            .filter(|block| matches!(block, LlmContentBlock::ToolResult { .. }))
            .count();
        if answered > 0 {
            results += answered;
            continue;
        }
        if message.role == LlmRole::User {
            break;
        }
    }
    results
}

/// A model that calls `app_lookup` until the turn holds
/// [`TOOL_CALLS_BEFORE_ANSWER`] results, then answers. `calls` counts the
/// calls the provider served: a replay is served from the journal and never
/// reaches it, so the count is the iterations the turns ran.
fn looping_provider(calls: &Arc<AtomicUsize>) -> ProviderHandle {
    let calls = Arc::clone(calls);
    crate::testing::TestProvider::builder()
        .kind("recorded-controls")
        .complete(move |request| {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let done = tool_results_this_turn(&request);
                if done >= TOOL_CALLS_BEFORE_ANSWER {
                    return Ok(text_response("answered"));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: format!("lookup-{done}"),
                        tool_name: "app_lookup".into(),
                        input_json: "{}".into(),
                        replay: None,
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

fn core_over(backend: lash_core::Backend, provider: ProviderHandle) -> Result<LashCore> {
    explicit_ephemeral_facets(LashCore::standard_builder(
        backend,
        crate::TurnBudget::bounded(CORE_DEFAULT_TURNS),
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(provider, mock_model_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())
}

/// Create `id` with `budget` recorded as its turn budget.
async fn create_with_budget(core: &LashCore, id: &str, budget: crate::TurnBudget) -> Result<()> {
    core.session(id)
        .create(crate::SessionCreation {
            spec: crate::SessionSpec::default().turn_budget(budget),
            ..Default::default()
        })
        .await?;
    Ok(())
}

/// How a turn ended and how many model calls it made since the last
/// reading of `calls`.
fn shape(output: &crate::TurnOutput, calls: &AtomicUsize) -> (TurnOutcome, usize) {
    (
        output.result.outcome.clone(),
        calls.swap(0, Ordering::SeqCst),
    )
}

fn finished_after_all_tools() -> (TurnOutcome, usize) {
    (
        TurnOutcome::Finished(crate::TurnFinish::AssistantMessage {
            text: "answered".to_string(),
        }),
        TOOL_CALLS_BEFORE_ANSWER + 1,
    )
}

fn stopped_at(turns: usize) -> (TurnOutcome, usize) {
    (TurnOutcome::Stopped(crate::TurnStop::MaxTurns), turns)
}

/// Send `text` to `id` through its Durable Session: no host runtime is open,
/// so the engine opens the session itself to drive the root.
async fn engine_driven_turn(core: &LashCore, id: &str, text: &str) -> Result<crate::TurnOutput> {
    core.session(id)
        .durable()
        .await?
        .send(TurnInput::text(text))
        .output()
        .await
}

/// One engine runs two sessions created with different budgets; each turn
/// stops at the budget its session recorded, not the core's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn each_session_runs_the_budget_it_was_created_with() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let core = core_over(double_backend().await, looping_provider(&calls))?;
    create_with_budget(&core, CHAT, crate::TurnBudget::Unbounded).await?;
    create_with_budget(&core, PULSAR, crate::TurnBudget::bounded(BOUNDED_TURNS)).await?;

    let chat = core.session(CHAT).open().await?;
    let pulsar = core.session(PULSAR).open().await?;
    let chat_turn = chat
        .send(TurnInput::text("look it all up"))
        .output()
        .await?;
    let chat_turn = shape(&chat_turn, &calls);
    let pulsar_turn = pulsar
        .send(TurnInput::text("look it all up"))
        .output()
        .await?;
    let pulsar_turn = shape(&pulsar_turn, &calls);

    assert_eq!(
        chat_turn,
        finished_after_all_tools(),
        "the unbounded session runs until the model answers"
    );
    assert_eq!(
        pulsar_turn,
        stopped_at(BOUNDED_TURNS),
        "the bounded session stops at its own budget"
    );
    Ok(())
}

/// Both sessions keep their recorded budgets when the engine opens them
/// itself: no host runtime is open, so every root below is driven on a
/// runtime the engine opened from the store, under a core whose own default
/// is neither session's budget.
async fn budgets_survive_engine_reopen_on(config: lash_restate_test::ServerConfig) -> Result<()> {
    let double = lash_restate_test::backend(0x4376_0001, config)
        .await
        .expect("build the Restate double");
    let calls = Arc::new(AtomicUsize::new(0));
    let core = core_over(double.lash_backend(), looping_provider(&calls))?;
    create_with_budget(&core, CHAT, crate::TurnBudget::Unbounded).await?;
    create_with_budget(&core, PULSAR, crate::TurnBudget::bounded(BOUNDED_TURNS)).await?;

    for round in ["first", "second"] {
        assert_eq!(
            shape(
                &engine_driven_turn(&core, CHAT, &format!("{round} chat")).await?,
                &calls
            ),
            finished_after_all_tools(),
            "the engine's {round} open of the unbounded session keeps it unbounded"
        );
        assert_eq!(
            shape(
                &engine_driven_turn(&core, PULSAR, &format!("{round} pulsar")).await?,
                &calls
            ),
            stopped_at(BOUNDED_TURNS),
            "the engine's {round} open of the bounded session keeps its bound"
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn budgets_survive_engine_reopen() -> Result<()> {
    budgets_survive_engine_reopen_on(lash_restate_test::ServerConfig::default()).await
}

/// Every await suspends and every resumption replays the root from its
/// journal: the replayed roots stop where they first did.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn budgets_survive_engine_reopen_under_always_replay() -> Result<()> {
    budgets_survive_engine_reopen_on(lash_restate_test::ServerConfig::default().always_replay(true))
        .await
}

/// The config head `id` records.
async fn recorded_config(core: &LashCore, id: &str) -> Result<lash_core::PersistedSessionConfig> {
    Ok(lash_core::SessionCommitStore::load_session_head_meta(
        core.store_factory.as_ref(),
        &SessionId::from(id),
    )
    .await?
    .expect("persisted head")
    .config)
}

/// The config a fresh open of `id` runs under: what the engine's reopen
/// loads from the store.
async fn reopened_config(core: &LashCore, id: &str) -> Result<lash_core::PersistedSessionConfig> {
    let id = SessionId::from(id);
    let store = crate::session::resolve_existing_session(&core.store_factory, &id).await?;
    let state = crate::session::load_state_from_store(&id, &core.policy, &store).await?;
    Ok(lash_core::PersistedSessionConfig::from(&state.policy))
}

/// Apply `transaction` to `session`, written against `revision` under `id`.
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

/// A charge-safety policy past the ceiling the core owner admits.
fn charge_safety_above_the_ceiling() -> crate::config::SetChargeSafety {
    crate::config::SetChargeSafety {
        charge_safety: crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries: crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES + 1,
            max_duplicate_cost_tokens: None,
        },
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn creation_refuses_charge_safety_above_the_ceiling_without_recording_a_session() -> Result<()>
{
    const ID: &str = "creation-charge-safety-ceiling";
    let calls = Arc::new(AtomicUsize::new(0));
    let core = core_over(double_backend().await, looping_provider(&calls))?;
    for requested in [crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES + 1, u8::MAX] {
        let error = core
            .session(ID)
            .create(crate::SessionCreation {
                spec: crate::SessionSpec::default().charge_safety(
                    crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                        max_unsafe_retries: requested,
                        max_duplicate_cost_tokens: Some(0),
                    },
                ),
                ..Default::default()
            })
            .await
            .err()
            .expect("creation must refuse an over-ceiling charge safety");
        let crate::EmbedError::Session(lash_core::SessionError::SessionConfigRefused(refusal)) =
            error
        else {
            panic!("expected a typed config refusal, got {error:?}");
        };
        assert_eq!(
            refusal.owner_refusal::<crate::config::CoreConfigRefusal>(),
            Some(
                crate::config::CoreConfigRefusal::UnsafeRetriesAboveCeiling {
                    requested,
                    ceiling: crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES,
                }
            )
        );
        assert!(
            lash_core::SessionCommitStore::load_session_head_meta(
                core.store_factory.as_ref(),
                &SessionId::from(ID),
            )
            .await?
            .is_none()
        );
        let mut policy = core.policy.clone();
        policy.charge_safety = crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries: requested,
            max_duplicate_cost_tokens: None,
        };
        let error = Box::pin(
            lash_core::runtime::EmbeddedRuntimeBuilder::new(
                core.env.core.clone(),
                crate::testing::runtime_lease_owner(),
            )
            .with_session_id(ID)
            .with_policy(policy)
            .build(),
        )
        .await
        .err()
        .expect("direct runtime creation must refuse the same policy");
        let lash_core::SessionError::SessionConfigRefused(refusal) = error else {
            panic!("expected a typed core config refusal, got {error:?}");
        };
        assert_eq!(
            refusal.owner_refusal::<crate::config::CoreConfigRefusal>(),
            Some(
                crate::config::CoreConfigRefusal::UnsafeRetriesAboveCeiling {
                    requested,
                    ceiling: crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES,
                }
            )
        );
    }
    core.session(ID)
        .create(crate::SessionCreation {
            spec: crate::SessionSpec::default().charge_safety(
                crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                    max_unsafe_retries: crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES,
                    max_duplicate_cost_tokens: None,
                },
            ),
            ..Default::default()
        })
        .await?;
    assert_eq!(
        recorded_config(&core, ID).await?.charge_safety,
        crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries: crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES,
            max_duplicate_cost_tokens: None,
        }
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    let inherited = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .session_spec(
        crate::SessionSpec::default()
            .turn_budget(crate::TurnBudget::Unbounded)
            .max_tool_calls(crate::MaxToolCalls::new(1024))
            .charge_safety(charge_safety_above_the_ceiling().charge_safety),
    )
    .serve_test_model(looping_provider(&calls), mock_model_spec())
    .build(crate::testing::runtime_lease_owner())?;
    let error = inherited
        .session(ID)
        .create(crate::SessionCreation::default())
        .await
        .err()
        .expect("creation must validate inherited defaults too");
    let crate::EmbedError::Session(lash_core::SessionError::SessionConfigRefused(refusal)) = error
    else {
        panic!("expected a typed inherited config refusal, got {error:?}");
    };
    assert_eq!(
        refusal.owner_refusal::<crate::config::CoreConfigRefusal>(),
        Some(
            crate::config::CoreConfigRefusal::UnsafeRetriesAboveCeiling {
                requested: crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES + 1,
                ceiling: crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES,
            }
        )
    );
    Ok(())
}

/// A session created with the core's defaults, open on a core over the
/// server double.
async fn created_session(id: &str) -> Result<(LashCore, crate::LashSession)> {
    let calls = Arc::new(AtomicUsize::new(0));
    let core = core_over(double_backend().await, looping_provider(&calls))?;
    create_with_budget(&core, id, crate::TurnBudget::Unbounded).await?;
    let session = core.session(id).open().await?;
    Ok((core, session))
}

/// `SetTurnBudget` reaches the next root: a session created unbounded runs
/// to the model's answer, and after the command its next root, on the open
/// runtime and on the engine's own reopen, stops at the commanded budget.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commanded_turn_budget_bounds_the_next_root() -> Result<()> {
    const ID: &str = "commanded-turn-budget";
    let calls = Arc::new(AtomicUsize::new(0));
    let core = core_over(double_backend().await, looping_provider(&calls))?;
    create_with_budget(&core, ID, crate::TurnBudget::Unbounded).await?;
    let session = core.session(ID).open().await?;
    let before = session
        .send(TurnInput::text("look it all up"))
        .output()
        .await?;
    assert_eq!(shape(&before, &calls), finished_after_all_tools());

    let revision = session.admin().config().revision().await?;
    let outcome = apply(
        &session,
        "bound-the-budget",
        revision,
        crate::config::ConfigTransaction::of(crate::config::SetTurnBudget {
            turn_budget: crate::TurnBudget::bounded(BOUNDED_TURNS),
        }),
    )
    .await?;
    assert!(
        matches!(
            outcome,
            crate::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{outcome:?}"
    );
    let after = session
        .send(TurnInput::text("look it all up again"))
        .output()
        .await?;
    assert_eq!(
        shape(&after, &calls),
        stopped_at(BOUNDED_TURNS),
        "the open runtime's next root runs under the commanded budget"
    );
    drop(session);
    assert_eq!(
        shape(
            &engine_driven_turn(&core, ID, "and once more").await?,
            &calls
        ),
        stopped_at(BOUNDED_TURNS),
        "the engine's reopen runs under the commanded budget"
    );
    Ok(())
}

/// Applied: `change` publishes at the next revision with a unit output, and
/// the head and a fresh open carry it. Stale: `other`, written against the
/// revision `change` moved past, settles stale and publishes nothing.
async fn applied_then_stale<C, T>(
    id: &str,
    change: C,
    other: C,
    control: impl Fn(&lash_core::PersistedSessionConfig) -> T,
    changed: T,
) -> Result<()>
where
    C: crate::plugins::ConfigCommand,
    T: PartialEq + std::fmt::Debug,
{
    let (core, session) = created_session(id).await?;
    let base = session.admin().config().revision().await?;
    assert_ne!(
        control(&recorded_config(&core, id).await?),
        changed,
        "the created session does not already record the change"
    );

    let applied = apply(
        &session,
        "change",
        base,
        crate::config::ConfigTransaction::of(change),
    )
    .await?;
    assert_eq!(
        applied,
        crate::config::ConfigTransactionOutcome::Applied {
            base_revision: base,
            revision: base + 1,
            outputs: vec![serde_json::Value::Null],
        }
    );
    let head = recorded_config(&core, id).await?;
    assert_eq!(head.config_revision, base + 1);
    assert_eq!(control(&head), changed, "the head records the change");
    drop(session);
    assert_eq!(
        control(&reopened_config(&core, id).await?),
        changed,
        "a fresh open runs under the change"
    );

    let session = core.session(id).open().await?;
    let stale = apply(
        &session,
        "stale",
        base,
        crate::config::ConfigTransaction::of(other),
    )
    .await?;
    assert_eq!(
        stale,
        crate::config::ConfigTransactionOutcome::Stale {
            expected: base,
            actual: base + 1,
        }
    );
    let head = recorded_config(&core, id).await?;
    assert_eq!(head.config_revision, base + 1);
    assert_eq!(control(&head), changed, "a stale command publishes nothing");
    Ok(())
}

/// `transaction` is refused at `index`, by the core owner, naming
/// `command`, and publishes nothing: the revision and `control` stay as
/// created.
async fn refused_publishes_nothing<T: PartialEq + std::fmt::Debug>(
    id: &str,
    transaction: crate::config::ConfigTransaction,
    index: usize,
    command: &str,
    control: impl Fn(&lash_core::PersistedSessionConfig) -> T,
) -> Result<crate::config::ConfigRefusal> {
    let (core, session) = created_session(id).await?;
    let created = recorded_config(&core, id).await?;
    let outcome = apply(&session, "refused", created.config_revision, transaction).await?;
    let crate::config::ConfigTransactionOutcome::Refused { refusal } = outcome else {
        panic!("the core owner refuses the transaction: {outcome:?}");
    };
    assert_eq!(
        (&refusal.at, refusal.owner.as_str()),
        (
            &crate::config::RefusalSite::Command {
                index,
                command: command.to_string(),
            },
            crate::config::CORE_CONFIG_OWNER
        )
    );
    let head = recorded_config(&core, id).await?;
    assert_eq!(head.config_revision, created.config_revision);
    assert_eq!(
        control(&head),
        control(&created),
        "a refused transaction publishes nothing"
    );
    Ok(refusal)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_autonomy_is_applied_at_the_next_revision_and_a_stale_one_publishes_nothing()
-> Result<()> {
    applied_then_stale(
        "set-autonomy",
        crate::config::SetAutonomy { autonomous: true },
        crate::config::SetAutonomy { autonomous: false },
        |config| config.autonomous,
        true,
    )
    .await
}

/// Every autonomy is admissible, so its refusal is the transaction's: in a
/// transaction the core owner refuses, it publishes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_autonomy_in_a_refused_transaction_publishes_nothing() -> Result<()> {
    refused_publishes_nothing(
        "set-autonomy-refused",
        crate::config::ConfigTransaction::of(crate::config::SetAutonomy { autonomous: true })
            .then(charge_safety_above_the_ceiling()),
        1,
        "set_charge_safety",
        |config| config.autonomous,
    )
    .await?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_no_progress_budget_is_applied_at_the_next_revision_and_a_stale_one_publishes_nothing()
-> Result<()> {
    applied_then_stale(
        "set-no-progress-budget",
        crate::config::SetNoProgressBudget {
            no_progress_budget: crate::NoProgressBudget::bounded(3),
        },
        crate::config::SetNoProgressBudget {
            no_progress_budget: crate::NoProgressBudget::Unbounded,
        },
        |config| config.no_progress_budget,
        crate::NoProgressBudget::bounded(3),
    )
    .await
}

/// Every decodable no-progress budget is admissible: in a transaction the
/// core owner refuses, it publishes nothing, and a zero bound does not
/// decode, so its submit is refused typed before anything is queued.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_no_progress_budget_is_refused_at_submit_or_with_its_transaction() -> Result<()> {
    refused_publishes_nothing(
        "set-no-progress-budget-refused",
        crate::config::ConfigTransaction::of(crate::config::SetNoProgressBudget {
            no_progress_budget: crate::NoProgressBudget::bounded(3),
        })
        .then(charge_safety_above_the_ceiling()),
        1,
        "set_charge_safety",
        |config| config.no_progress_budget,
    )
    .await?;

    const ID: &str = "set-no-progress-budget-zero";
    let (core, session) = created_session(ID).await?;
    let created = recorded_config(&core, ID).await?;
    let zero =
        crate::config::ConfigTransaction::new().then_entry(crate::config::ConfigCommandEntry {
            owner: crate::config::CORE_CONFIG_OWNER.to_string(),
            command: "set_no_progress_budget".to_string(),
            args: serde_json::json!({ "no_progress_budget": { "bounded": 0 } }),
        });
    let error = apply(&session, "zero", created.config_revision, zero)
        .await
        .expect_err("a zero no-progress bound does not decode");
    assert!(
        matches!(
            &error,
            crate::EmbedError::ConfigSubmit(crate::config::ConfigSubmitError::InvalidArgs {
                owner,
                command,
                ..
            }) if owner == crate::config::CORE_CONFIG_OWNER && command == "set_no_progress_budget"
        ),
        "{error:?}"
    );
    let head = recorded_config(&core, ID).await?;
    assert_eq!(head.config_revision, created.config_revision);
    assert_eq!(head.no_progress_budget, created.no_progress_budget);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_charge_safety_is_applied_at_the_next_revision_and_a_stale_one_publishes_nothing()
-> Result<()> {
    let accepting = crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
        max_unsafe_retries: 2,
        max_duplicate_cost_tokens: Some(4_096),
    };
    applied_then_stale(
        "set-charge-safety",
        crate::config::SetChargeSafety {
            charge_safety: accepting.clone(),
        },
        crate::config::SetChargeSafety {
            charge_safety: crate::ChargeSafetyPolicy::RequireGuarantee,
        },
        |config| config.charge_safety.clone(),
        accepting,
    )
    .await
}

/// The core owner refuses a charge-safety policy accepting more unsafe
/// retries than Lash ever buys, typed, and publishes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_charge_safety_above_the_retry_ceiling_is_refused() -> Result<()> {
    let refusal = refused_publishes_nothing(
        "set-charge-safety-refused",
        crate::config::ConfigTransaction::of(charge_safety_above_the_ceiling()),
        0,
        "set_charge_safety",
        |config| config.charge_safety.clone(),
    )
    .await?;
    assert_eq!(
        refusal
            .owner_refusal::<crate::config::CoreConfigRefusal>()
            .expect("the core owner's typed refusal"),
        crate::config::CoreConfigRefusal::UnsafeRetriesAboveCeiling {
            requested: crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES + 1,
            ceiling: crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES,
        }
    );
    Ok(())
}

/// The budget figments records for a session (FIG-4376): its roots stop
/// after exactly this many model calls.
const RECORDED_TURNS: usize = 12;
/// A model call count no bounded root here reaches: the model answers only
/// after it, so a root that ignored its recorded budget ends answered, and
/// the law fails instead of hanging.
const RUNAWAY_CALLS: usize = 100;

/// A model that never stops calling `app_lookup` until the whole run has
/// made [`RUNAWAY_CALLS`] calls. `calls` counts the calls it served.
fn endless_provider(calls: &Arc<AtomicUsize>) -> ProviderHandle {
    let calls = Arc::clone(calls);
    crate::testing::TestProvider::builder()
        .kind("recorded-controls")
        .complete(move |_request| {
            let calls = Arc::clone(&calls);
            async move {
                let call = calls.fetch_add(1, Ordering::SeqCst) + 1;
                if call >= RUNAWAY_CALLS {
                    return Ok(text_response("runaway"));
                }
                Ok(LlmResponse {
                    parts: vec![LlmOutputPart::ToolCall {
                        call_id: format!("lookup-{call}"),
                        tool_name: "app_lookup".into(),
                        input_json: "{}".into(),
                        replay: None,
                    }],
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// The line prefix the Restate suite runner reads as a completed step
/// (`PROGRESS_MARKER` in `scripts/ci/restate_suite.py`): the live law is
/// bounded per root, not by its four roots together, which a starved host's
/// replay leg stretches.
const PROGRESS_MARKER: &str = "[restate-suite progress] ";

/// A session's recorded budget bounds its roots after the engine restarts
/// under a core whose own budget is unbounded (figments' re-pin at
/// 257bef60f3 ran such a root past 500 model calls). One session is created
/// with the budget, the other is given it by `SetTurnBudget`; then the
/// `first` deployment goes away, and `restart` brings up a new one over its
/// stores, which opens both under an unbounded core. Each root, driven on the
/// engine's own reopen and on a host open, stops after exactly
/// [`RECORDED_TURNS`] model calls, typed: the next call is never made.
/// `prefix` names the law's sessions.
async fn a_recorded_budget_bounds_every_root_after_an_engine_restart(
    first: lash_core::Backend,
    restart: impl AsyncFnOnce() -> lash_core::Backend,
    prefix: &str,
) -> Result<LashCore> {
    let created = format!("{prefix}-created-bounded");
    let commanded = format!("{prefix}-commanded-bounded");
    let calls = Arc::new(AtomicUsize::new(0));
    {
        let creator = core_over(first, endless_provider(&calls))?;
        create_with_budget(
            &creator,
            &created,
            crate::TurnBudget::bounded(RECORDED_TURNS),
        )
        .await?;
        create_with_budget(&creator, &commanded, crate::TurnBudget::Unbounded).await?;
        let session = creator.session(commanded.as_str()).open().await?;
        let revision = session.admin().config().revision().await?;
        let outcome = apply(
            &session,
            "bound-before-restart",
            revision,
            crate::config::ConfigTransaction::of(crate::config::SetTurnBudget {
                turn_budget: crate::TurnBudget::bounded(RECORDED_TURNS),
            }),
        )
        .await?;
        assert!(
            matches!(
                outcome,
                crate::config::ConfigTransactionOutcome::Applied { .. }
            ),
            "{outcome:?}"
        );
    }
    let second = restart().await;
    let unbounded = explicit_ephemeral_facets(LashCore::standard_builder(
        second,
        crate::TurnBudget::Unbounded,
        crate::MaxToolCalls::new(1024),
    ))
    .serve_test_model(endless_provider(&calls), mock_model_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner())?;
    for id in [created.as_str(), commanded.as_str()] {
        assert_eq!(
            shape(
                &engine_driven_turn(&unbounded, id, "look it all up").await?,
                &calls
            ),
            stopped_at(RECORDED_TURNS),
            "{id}: the engine's reopen after the restart runs the recorded budget"
        );
        eprintln!("{PROGRESS_MARKER}{id}: the engine-driven root stopped at its budget");
        let opened = unbounded.session(id).open().await?;
        let output = opened
            .send(TurnInput::text("look it all up again"))
            .output()
            .await?;
        assert_eq!(
            shape(&output, &calls),
            stopped_at(RECORDED_TURNS),
            "{id}: a host open under the unbounded core runs the recorded budget"
        );
        eprintln!("{PROGRESS_MARKER}{id}: the host-opened root stopped at its budget");
    }
    Ok(unbounded)
}

/// The law on the server double: the restart is `first`'s deployment
/// restarted over its stores.
async fn after_a_restart_of_the_double(first: lash_restate_test::RestateTestBackend) -> Result<()> {
    let backend = first.lash_backend();
    let mut second = None;
    a_recorded_budget_bounds_every_root_after_an_engine_restart(
        backend,
        async || {
            let double = redeploy(first).await;
            let backend = double.lash_backend();
            second = Some(double);
            backend
        },
        "restart",
    )
    .await
    .map(|_| ())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_recorded_budget_bounds_every_root_after_an_engine_restart_on_sqlite() -> Result<()> {
    let first = lash_restate_test::backend(0x4376_0002, lash_restate_test::ServerConfig::default())
        .await
        .expect("build the first deployment over SQLite");
    after_a_restart_of_the_double(first).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires PostgreSQL; run with --include-ignored inside a pg16 gate"]
async fn a_recorded_budget_bounds_every_root_after_an_engine_restart_on_postgres() -> Result<()> {
    let Some((stores, _held)) = postgres_store_set().await else {
        return Ok(());
    };
    let first = lash_restate_test::backend_with(
        0x4376_0004,
        lash_restate_test::ServerConfig::default(),
        move |_| stores,
    )
    .await
    .expect("build the first deployment over PostgreSQL");
    after_a_restart_of_the_double(first).await
}

/// The law on a live `restate-server` (the `recorded-roots` suite of
/// `scripts/restate-suites.toml`): the restart replaces the deployment with a
/// new engine and endpoint over its stores, registered with the same server.
/// The server's state outlives a run, so each run names its own sessions.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires an isolated Restate server; run by the recorded-roots suite"]
#[allow(
    clippy::disallowed_methods,
    reason = "the live law reads the suite's server and endpoint addresses"
)]
async fn live_a_recorded_budget_bounds_every_root_after_a_deployment_restart() -> Result<()> {
    let env = |name: &str| {
        std::env::var(name).unwrap_or_else(|_| panic!("the live suite's environment sets {name}"))
    };
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("wall clock after the epoch")
        .as_nanos();
    let prefix = format!("recorded-budget-restart-{nonce}");
    let first =
        lash_restate_test::live::LiveRestateBackend::start(lash_restate_test::live::LiveConfig {
            ingress_url: env("RESTATE_INGRESS_URL"),
            admin_url: env("RESTATE_ADMIN_URL"),
            endpoint_bind: env("RR_BIND").parse().expect("a socket address"),
            endpoint_url: env("RR_URL"),
            run_tag: prefix.clone(),
            namespace: lash_restate::RestateNamespace::default(),
        })
        .await
        .expect("serve the first live deployment");
    let mut second = None;
    let result = a_recorded_budget_bounds_every_root_after_an_engine_restart(
        first.lash_backend(),
        async || {
            let rebuilt = first
                .rebuild()
                .await
                .expect("restart the live deployment over its stores");
            let backend = rebuilt.lash_backend();
            second = Some(rebuilt);
            backend
        },
        &prefix,
    )
    .await;
    if let Some(second) = &second {
        second.finish().await;
    }
    first.finish().await;
    result.map(|_| ())
}

/// How many times a `max_tool_calls` refusal appears in what the model is
/// shown.
fn refusals_shown(request: &LlmRequest) -> usize {
    format!("{:?}", request.messages)
        .matches("tool call limit exceeded")
        .count()
}

/// A model that calls `app_lookup` twice in one step, then answers once the
/// turn holds both results. `shown` records, per answer, how many refusals
/// the request it answered carried.
fn fanning_provider(shown: &Arc<std::sync::Mutex<Vec<usize>>>) -> ProviderHandle {
    let shown = Arc::clone(shown);
    crate::testing::TestProvider::builder()
        .kind("recorded-max-tool-calls")
        .complete(move |request| {
            let shown = Arc::clone(&shown);
            async move {
                if tool_results_this_turn(&request) >= 2 {
                    shown
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(refusals_shown(&request));
                    return Ok(text_response("answered"));
                }
                Ok(LlmResponse {
                    parts: (0..2)
                        .map(|call| LlmOutputPart::ToolCall {
                            call_id: format!("fan-{call}"),
                            tool_name: "app_lookup".into(),
                            input_json: "{}".into(),
                            replay: None,
                        })
                        .collect(),
                    ..LlmResponse::default()
                })
            }
        })
        .build()
        .into_handle()
}

/// `SetMaxToolCalls` reaches the next root and no earlier one (FIG-4546): a
/// step of two calls runs under the limit the session was created with, and
/// after the command the same step is refused, on the open runtime and on the
/// engine's own reopen, with the refusal in the model's tool results.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commanded_max_tool_calls_binds_the_next_root() -> Result<()> {
    const ID: &str = "commanded-max-tool-calls";
    let shown = Arc::new(std::sync::Mutex::new(Vec::new()));
    let core = core_over(double_backend().await, fanning_provider(&shown))?;
    create_with_budget(&core, ID, crate::TurnBudget::Unbounded).await?;
    assert_eq!(
        recorded_config(&core, ID).await?.max_tool_calls,
        crate::MaxToolCalls::new(1024),
        "the session records the limit it was created with"
    );
    let session = core.session(ID).open().await?;
    session
        .send(TurnInput::text("look two things up"))
        .output()
        .await?;

    let revision = session.admin().config().revision().await?;
    let outcome = apply(
        &session,
        "one-call-a-step",
        revision,
        crate::config::ConfigTransaction::of(crate::config::SetMaxToolCalls {
            max_tool_calls: crate::MaxToolCalls::new(1),
        }),
    )
    .await?;
    assert!(
        matches!(
            outcome,
            crate::config::ConfigTransactionOutcome::Applied { .. }
        ),
        "{outcome:?}"
    );
    session
        .send(TurnInput::text("look two things up again"))
        .output()
        .await?;
    drop(session);
    engine_driven_turn(&core, ID, "and once more").await?;

    assert_eq!(
        *shown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        // Each root's request carries the roots before it: none refused
        // under the created limit, then both calls of each later root.
        vec![0, 2, 4],
        "the limit binds from the root after the command"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_max_tool_calls_is_applied_at_the_next_revision_and_a_stale_one_publishes_nothing()
-> Result<()> {
    applied_then_stale(
        "set-max-tool-calls",
        crate::config::SetMaxToolCalls {
            max_tool_calls: crate::MaxToolCalls::new(7),
        },
        crate::config::SetMaxToolCalls {
            max_tool_calls: crate::MaxToolCalls::new(9),
        },
        |config| config.max_tool_calls,
        crate::MaxToolCalls::new(7),
    )
    .await
}

/// A zero limit does not decode, so its submit is refused typed before
/// anything is queued: there is no "no limit" spelling.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn set_max_tool_calls_of_zero_is_refused_at_submit() -> Result<()> {
    const ID: &str = "set-max-tool-calls-zero";
    let (core, session) = created_session(ID).await?;
    let created = recorded_config(&core, ID).await?;
    let zero =
        crate::config::ConfigTransaction::new().then_entry(crate::config::ConfigCommandEntry {
            owner: crate::config::CORE_CONFIG_OWNER.to_string(),
            command: "set_max_tool_calls".to_string(),
            args: serde_json::json!({ "max_tool_calls": 0 }),
        });
    let error = apply(&session, "zero", created.config_revision, zero)
        .await
        .expect_err("a zero tool-call limit does not decode");
    assert!(
        matches!(
            &error,
            crate::EmbedError::ConfigSubmit(crate::config::ConfigSubmitError::InvalidArgs {
                owner,
                command,
                ..
            }) if owner == crate::config::CORE_CONFIG_OWNER && command == "set_max_tool_calls"
        ),
        "{error:?}"
    );
    let head = recorded_config(&core, ID).await?;
    assert_eq!(head.config_revision, created.config_revision);
    assert_eq!(head.max_tool_calls, created.max_tool_calls);
    Ok(())
}

/// A core whose session spec states no `max_tool_calls` is refused, typed,
/// before any session can be created under it (FIG-4546): the limit has no
/// default and no built-in ceiling to fall back to.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_core_without_max_tool_calls_is_refused() -> Result<()> {
    let calls = Arc::new(AtomicUsize::new(0));
    let refused = explicit_ephemeral_facets(LashCore::standard_builder(
        double_backend().await,
        crate::TurnBudget::bounded(CORE_DEFAULT_TURNS),
        crate::MaxToolCalls::new(1024),
    ))
    .session_spec(crate::SessionSpec::new().turn_budget(crate::TurnBudget::Unbounded))
    .serve_test_model(looping_provider(&calls), mock_model_spec())
    .tools(Arc::new(AppTools))
    .build(crate::testing::runtime_lease_owner());
    let Err(error) = refused else {
        panic!("a core without max_tool_calls must not build");
    };
    assert!(
        matches!(error, crate::EmbedError::MissingMaxToolCalls),
        "{error:?}"
    );
    assert!(error.is_terminal() && !error.is_retryable(), "{error}");
    Ok(())
}
