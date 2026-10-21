use super::*;
use crate::TurnInput;
use crate::tests::host::controls::*;
use lash_vocabulary::identity::SessionId;

/// The config head `id` records.
async fn recorded_config(core: &LashCore, id: &str) -> Result<lash_core::PersistedSessionConfig> {
    Ok(lash_core::SessionCommitStore::load_session_head_meta(
        core.store_factory.as_ref(),
        &SessionId::fixture(id),
    )
    .await?
    .expect("persisted head")
    .config)
}

/// The config a fresh open of `id` runs under: what the engine's reopen
/// loads from the store.
async fn reopened_config(core: &LashCore, id: &str) -> Result<lash_core::PersistedSessionConfig> {
    let id = SessionId::fixture(id);
    let store = crate::session::resolve_existing_session(&core.store_factory, &id).await?;
    let state = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await?
    .expect("a recorded session has a current window")
    .state;
    Ok(lash_core::PersistedSessionConfig::from_policy(
        &state.policy,
    ))
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
    let core = core_over(
        sqlite_memory_store_backend().await,
        looping_provider(&calls),
    )?;
    for requested in [crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES + 1, u8::MAX] {
        let error = core
            .session(
                lash_vocabulary::identity::SessionId::parse(ID).expect("nonblank host identity"),
            )
            .create(crate::SessionCreation::root(
                mock_session_spec().charge_safety(
                    crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                        max_unsafe_retries: requested,
                        max_duplicate_cost_tokens: Some(0),
                    },
                ),
            ))
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
        let mut policy = lash_core::SessionPolicy::new(
            crate::TurnBudget::Unbounded,
            lash_core::MaxToolCalls::new(1024),
            crate::NoProgressBudget::bounded(12),
        );
        policy.charge_safety = crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries: requested,
            max_duplicate_cost_tokens: None,
        };
        let error = Box::pin(
            lash_core::runtime::EmbeddedRuntimeBuilder::new(
                core.env.core.clone(),
                crate::tests::host::runtime_lease_owner(),
            )
            .with_session_id(ID)
            .with_creation(policy)
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
    core.session(lash_vocabulary::identity::SessionId::parse(ID).expect("nonblank host identity"))
        .create(crate::SessionCreation::root(
            mock_session_spec().charge_safety(crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES,
                max_duplicate_cost_tokens: None,
            }),
        ))
        .await?;
    assert_eq!(
        recorded_config(&core, ID).await?.charge_safety,
        crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries: crate::ChargeSafetyPolicy::MAX_UNSAFE_RETRIES,
            max_duplicate_cost_tokens: None,
        }
    );
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    Ok(())
}

/// A session created unbounded, open on a core over the
/// server double.
async fn created_session(id: &str) -> Result<(LashCore, crate::LashSession)> {
    let calls = Arc::new(AtomicUsize::new(0));
    let core = core_over(
        sqlite_memory_store_backend().await,
        looping_provider(&calls),
    )?;
    create_with_budget(&core, id, crate::TurnBudget::Unbounded).await?;
    let session = core
        .session(SessionId::fixture(id.to_string()))
        .open()
        .await?;
    Ok((core, session))
}

/// Applied: `change` publishes at the next revision with a unit output, and
/// the head and a fresh open carry it. Stale: `other`, written against the
/// revision `change` moved past, settles stale and publishes nothing.
async fn applied_then_stale<C, T>(
    id: &str,
    change: C,
    other: C,
    control: impl Fn(&lash_core::PersistedSessionConfig) -> T + Send + Sync,
    changed: T,
) -> Result<()>
where
    C: crate::plugins::ConfigCommand,
    T: PartialEq + std::fmt::Debug + Send,
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

    let session = core
        .session(SessionId::fixture(id.to_string()))
        .open()
        .await?;
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
    control: impl Fn(&lash_core::PersistedSessionConfig) -> T + Send + Sync,
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

/// How many times a `max_tool_calls` refusal appears in what the model is
/// shown.
fn refusals_shown(request: &LlmRequest) -> usize {
    request
        .messages
        .iter()
        .flat_map(|message| message.blocks.iter())
        .filter(|block| {
            matches!(
                block,
                LlmContentBlock::ToolResult {
                    status: lash_protocol_types::ToolCallStatus::Failure,
                    ..
                }
            )
        })
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
                            tool_name: "tools__app_lookup".into(),
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

/// The commanded-limit law counts body entries independently of refusals.
struct CountedAppTools(Arc<AtomicUsize>);

#[async_trait]
impl ToolProvider for CountedAppTools {
    fn tool_manifests(
        &self,
        config: &lash_core::OwnerConfigView,
    ) -> std::result::Result<
        Vec<lash_vocabulary::tool_contract::ToolManifest>,
        crate::plugins::PluginError,
    > {
        AppTools.tool_manifests(config)
    }

    fn resolve_contract(
        &self,
        name: &str,
        config: &lash_core::OwnerConfigView,
    ) -> Option<Arc<lash_vocabulary::tool_contract::ToolContract>> {
        AppTools.resolve_contract(name, config)
    }

    async fn execute(&self, call: lash_core::ToolCall<'_>) -> lash_core::ToolAttemptOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        AppTools.execute(call).await
    }
}

/// `SetMaxToolCalls` reaches the next run and no earlier one (FIG-4546): a
/// step of two calls runs under the limit the session was created with, and
/// after the command the same step is refused, on the open runtime and on the
/// engine's own reopen, with the refusal in the model's tool results.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_commanded_max_tool_calls_binds_the_next_run() -> Result<()> {
    const ID: &str = "commanded-max-tool-calls";
    let shown = Arc::new(std::sync::Mutex::new(Vec::new()));
    let executed = Arc::new(AtomicUsize::new(0));
    let core = explicit_ephemeral_facets(LashCore::standard_builder(
        sqlite_memory_store_backend().await,
    ))
    .serve_test_llm_profile(fanning_provider(&shown), mock_llm_profile_spec())
    .tools(
        crate::tool_provider_key!("tools"),
        Arc::new(CountedAppTools(Arc::clone(&executed))),
    )
    .build(runtime_lease_owner())?;
    create_with_budget(&core, ID, crate::TurnBudget::Unbounded).await?;
    assert_eq!(
        recorded_config(&core, ID).await?.max_tool_calls,
        crate::MaxToolCalls::new(1024),
        "the session records the limit it was created with"
    );
    let session = core
        .session(lash_vocabulary::identity::SessionId::parse(ID).expect("nonblank host identity"))
        .open()
        .await?;
    let admitted = session
        .send(TurnInput::text("look two things up"))
        .output()
        .await?;
    let admitted_calls =
        crate::testing::tool_calls_for_activity(&session.durable(), &admitted.activities).await;
    assert_eq!(admitted_calls.len(), 2);
    assert!(
        admitted_calls
            .iter()
            .all(|call| matches!(call.output.outcome, lash_core::ToolCallOutcome::Success(_)))
    );

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
    let refused = session
        .send(TurnInput::text("look two things up again"))
        .output()
        .await?;
    drop(session);
    let reopened = engine_executed_turn(&core, ID, "and once more").await?;
    assert_eq!(
        executed.load(Ordering::SeqCst),
        2,
        "only the initially admitted bodies run"
    );
    let durable = core
        .session(lash_vocabulary::identity::SessionId::from(ID))
        .durable()
        .await?;
    for output in [&refused, &reopened] {
        let output_tool_calls =
            crate::testing::tool_calls_for_activity(&durable, &output.activities).await;
        assert_eq!(output_tool_calls.len(), 2);
        for call in &output_tool_calls {
            let lash_core::ToolCallOutcome::Failure(failure) = &call.output.outcome else {
                panic!("the commanded limit refuses every call: {call:?}");
            };
            assert_eq!(failure.code, crate::ToolCallLimitExceeded::CODE);
            assert_eq!(failure.class, lash_core::ToolFailureClass::ResourceLimit);
            assert_eq!(
                failure.source,
                lash_protocol_types::ToolFailureSource::Runtime
            );
        }
    }

    assert_eq!(
        *shown
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
        // Each run's request carries the runs before it: none refused
        // under the created limit, then both calls of each later run.
        vec![0, 2, 4],
        "the limit binds from the run after the command"
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

/// A fork records the forked revision's recorded config in full (FIG-4594). The
/// source is created from one spec and executes a run; the host then changes
/// what it passes (another session is created from a different spec, on a
/// second core over the same stores), and the fork made there records
/// exactly the source's turn budget, generation and charge safety, with the
/// rest of its config head.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_fork_records_its_fork_points_config_whatever_the_host_passes_now() -> Result<()> {
    const SOURCE: &str = "fork-config-source";
    const LATER: &str = "fork-config-created-later";
    const FORK: &str = "fork-config-branch";
    let backend = sqlite_memory_store_backend().await;
    let stated = mock_session_spec()
        .turn_budget(crate::TurnBudget::bounded(7))
        .generation(lash_core::GenerationOptions {
            seed: Some(321),
            ..Default::default()
        })
        .charge_safety(crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries: 1,
            max_duplicate_cost_tokens: None,
        });
    let core = core_over(backend.clone(), mock_provider())?;
    core.session(
        lash_vocabulary::identity::SessionId::parse(SOURCE).expect("nonblank host identity"),
    )
    .create(crate::SessionCreation::root(stated))
    .await?;
    engine_executed_turn(&core, SOURCE, "before the fork").await?;
    let source = recorded_config(&core, SOURCE).await?;
    assert_eq!(source.turn_budget, crate::TurnBudget::bounded(7));
    let point = core
        .store_factory
        .revisions(&SessionId::from(SOURCE))
        .await?
        .pop()
        .expect("the source has published its head")
        .head_revision;

    // What the host passes changes: its next session states other controls.
    let later = core_over(backend, mock_provider())?;
    let changed = mock_session_spec()
        .turn_budget(crate::TurnBudget::bounded(11))
        .generation(lash_core::GenerationOptions {
            seed: Some(999),
            ..Default::default()
        });
    later
        .session(
            lash_vocabulary::identity::SessionId::parse(LATER).expect("nonblank host identity"),
        )
        .create(crate::SessionCreation::root(changed))
        .await?;
    later
        .fork_at(
            &SessionId::from(SOURCE),
            lash_core::Target::Revision(point),
            crate::ForkRequest {
                session_id: FORK.into(),
                relation: lash_core::SessionRelation::Fork {
                    source_session_id: SOURCE.into(),
                    source_node_id: None,
                },
                observed_processes: Vec::new(),
            },
        )
        .await?;

    let fork = recorded_config(&later, FORK).await?;
    assert_eq!(fork.turn_budget, source.turn_budget);
    assert_eq!(fork.generation, source.generation);
    assert_eq!(fork.charge_safety, source.charge_safety);
    assert_eq!(
        lash_core::PersistedSessionConfig {
            config_revision: source.config_revision,
            ..fork.clone()
        },
        source,
        "the fork's config head is its fork point's, at its own first revision"
    );
    assert_eq!(fork.config_revision, 0);
    assert_eq!(
        reopened_config(&later, FORK).await?.turn_budget,
        source.turn_budget,
        "a reopen of the fork runs what it recorded"
    );
    Ok(())
}
