use super::process_await_redrive::{fig1943_apply_state_commands, fig1943_invocation_with_state};
use super::*;
use crate::durable_wait::IndexedWait;
use crate::object_state::StampedValue;

#[tokio::test]
pub(super) async fn closed_runs_leave_flat_wait_index_state_through_a_thousand_turns() {
    let endpoint = Endpoint::builder()
        .bind(LashDurableWaitRegistryImpl::default().serve())
        .build();
    let session_id = SessionId::from("fig3977-session");
    let object_key = session_id.as_str();
    let stamped = |body: serde_json::Value| {
        serde_json::to_vec(&StampedValue {
            format: u32::from(crate::durable_wait::DURABLE_WAIT_REGISTRY_FORMAT_VERSION),
            body,
        })
        .expect("encode a stamped registry row")
    };
    let metadata = serde_json::to_value(RestateDurableWaitIndexMetadata::default())
        .expect("encode registry metadata");
    let mut state = BTreeMap::from([
        super::process_await_redrive::fresh_compat_record(
            crate::durable_wait::DURABLE_WAIT_REGISTRY_FORMAT_VERSION,
        ),
        (
            crate::durable_wait::DURABLE_WAIT_INDEX_METADATA_KEY.to_string(),
            stamped(metadata),
        ),
    ]);
    let mut measurements = Vec::new();

    for ordinal in 1..=1000 {
        let run = TurnId::fixture(format!("run-{ordinal:04}"));
        let key = restate_await_event_key(
            &ExecutionScope::turn(session_id.clone(), run.clone()),
            AwaitEventWaitIdentity::TurnCancelGate,
        )
        .expect("derive turn-control key");
        let address = RestateDurableWaitAddress::for_key(&key);
        state.insert(
            durable_wait_index_state_key(&address),
            stamped(
                serde_json::to_value(IndexedWait {
                    key,
                    terminal: Some(Resolution::Cancelled),
                })
                .expect("encode the indexed wait"),
            ),
        );
        let output = invoke_endpoint_body(
            &endpoint,
            "LashDurableWaitIndex",
            "retire_run",
            fig1943_invocation_with_state(
                object_key,
                &RestateDurableWaitRunRequest {
                    session_id: session_id.clone(),
                    run,
                    committed_turn: None,
                },
                &state,
            ),
        )
        .await
        .expect("retire a terminal run's index rows");
        assert!(
            restate_output_failure_message(&output)
                .or_else(|| restate_error_message(&output))
                .is_none(),
            "run retirement succeeds"
        );
        fig1943_apply_state_commands(&mut state, &output);
        if matches!(ordinal, 1 | 500 | 1000) {
            measurements.push((
                state.len(),
                state
                    .iter()
                    .map(|(key, value)| key.len() + value.len())
                    .sum::<usize>(),
            ));
        }
    }
    eprintln!("FIG-3977 wait-index state at turns 1/500/1000: {measurements:?}");
    assert_eq!(measurements, vec![measurements[0]; 3]);
    assert_eq!(
        state.keys().map(String::as_str).collect::<Vec<_>>(),
        [
            crate::compat::COMPAT_KEY,
            crate::durable_wait::DURABLE_WAIT_INDEX_METADATA_KEY
        ],
        "only the compat record and the index metadata survive run close"
    );

    let closed_run = TurnId::from("closed-run");
    let live_run = TurnId::from("live-run");
    let mut keys = Vec::new();
    for run in [&closed_run, &live_run] {
        let key = restate_await_event_key(
            &ExecutionScope::turn(session_id.clone(), run.clone()),
            AwaitEventWaitIdentity::TurnCancelGate,
        )
        .expect("derive a concurrent run's key");
        let address = RestateDurableWaitAddress::for_key(&key);
        let wait_key = durable_wait_index_state_key(&address);
        state.insert(
            wait_key.clone(),
            stamped(
                serde_json::to_value(IndexedWait {
                    key,
                    terminal: Some(Resolution::Cancelled),
                })
                .expect("encode the indexed wait"),
            ),
        );
        keys.push(wait_key);
    }
    let close = |state: &BTreeMap<String, Vec<u8>>| {
        fig1943_invocation_with_state(
            object_key,
            &RestateDurableWaitRunRequest {
                session_id: session_id.clone(),
                run: closed_run.clone(),
                committed_turn: None,
            },
            state,
        )
    };
    let output = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "retire_run",
        close(&state),
    )
    .await
    .expect("retire the closed run");
    fig1943_apply_state_commands(&mut state, &output);
    assert!(
        !state.contains_key(&keys[0]),
        "the closed run's row is retired"
    );
    assert!(state.contains_key(&keys[1]), "the live run's row survives");
    let before_retry = state.clone();
    let retry = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "retire_run",
        close(&state),
    )
    .await
    .expect("retry the same run close");
    fig1943_apply_state_commands(&mut state, &retry);
    assert_eq!(state, before_retry, "run retirement is idempotent");

    for run in ["fenced-run", "other-fenced-run"] {
        let request = RestateDurableWaitCancelDecidedRequest {
            scope: ExecutionScope::turn(session_id.clone(), run),
            wait: AwaitEventWaitIdentity::Custom {
                key: format!("completion-{run}"),
            },
        };
        let fence = invoke_endpoint_body(
            &endpoint,
            "LashDurableWaitIndex",
            "fence_cancel_decided",
            fig1943_invocation_with_state(object_key, &request, &state),
        )
        .await
        .expect("fence a cancel-decided completion");
        fig1943_apply_state_commands(&mut state, &fence);
    }
    let retire_fence = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "retire_run",
        fig1943_invocation_with_state(
            object_key,
            &RestateDurableWaitRunRequest {
                session_id: session_id.clone(),
                run: TurnId::from("fenced-run"),
                committed_turn: None,
            },
            &state,
        ),
    )
    .await
    .expect("retire the first run's cancel fence");
    fig1943_apply_state_commands(&mut state, &retire_fence);
    let metadata: serde_json::Value = serde_json::from_slice(
        state
            .get(crate::durable_wait::DURABLE_WAIT_INDEX_METADATA_KEY)
            .expect("index metadata remains"),
    )
    .expect("decode stamped metadata");
    let fences = metadata["body"]["cancel_decided"]
        .as_array()
        .expect("one cancel fence remains");
    assert_eq!(fences.len(), 1);
    assert!(
        fences[0]
            .as_str()
            .is_some_and(|id| id.contains("other-fenced-run"))
    );
}

/// Arm the index's registration witness for `key`, start `attach` on it,
/// and wait until the index has registered that attach's wait.
async fn registered_attach(
    attach: &crate::RestateTurnAttach,
    key: &AwaitEventKey,
    address: TurnAddress,
) -> tokio::task::JoinHandle<Result<lash_core::facade_support::TurnTerminal, lash_core::RuntimeError>>
{
    let registration = crate::durable_wait::arm_wait_registration_witness(key);
    let attach = attach.clone();
    let waiter = tokio::spawn(async move { attach.await_terminal(&address).await });
    assert_eq!(
        registration.await.expect("the index observed the attach"),
        crate::durable_wait::RestateDurableWaitRegistration::Registered,
        "the attach parks on the open terminal"
    );
    waiter
}

/// FIG-4025: a committed turn's terminal is published one-way after its
/// commit, so CloseRunScope can retire the run while that publish is still
/// in flight. The wait registered on the terminal, which every attach shares
/// (FIG-4345), resolves with the real terminal, never `Cancelled`, for the
/// attaches still listening and after one whose caller stopped listening;
/// an attach after the publish reads it too. A run that ended without a
/// commit owes no terminal, so retiring it still releases its waiter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
pub(super) async fn retiring_a_run_never_cancels_a_terminal_its_commit_still_publishes() {
    use lash_core::engine::ScopeCloseSink as _;
    use lash_core::store::{RunTerminal, RunTerminalCause, TurnCommitId};

    let server = lash_restate_test::RestateTestServer::new(
        lash_restate_test::ServerConfig::default().with_seed(0x4025),
    )
    .expect("start the server double");
    server
        .register(
            Endpoint::builder()
                .bind(LashDurableWaitWorkflowImpl::default().serve())
                .bind(LashDurableWaitRegistryImpl::default().serve())
                .build(),
        )
        .await
        .expect("register the durable-wait services");
    let connection = RestateConnection::with_transport(server.ingress_url(), server.transport());
    let host = Arc::new(RestateEffectHost::new(
        connection.clone(),
        test_restate_authority_id(),
        crate::tests::test_build_generation(),
    ));
    let attach = crate::RestateTurnAttach::new(connection, test_restate_authority_id());
    let stores = memory_process_stores().await;
    let registry: Arc<dyn ProcessRegistry> = stores.registry.clone();
    let scope_close = lash_core::RegistryScopeClose::new(
        registry,
        Arc::new(lash_core::facade_support::SystemClock),
    )
    .with_effect_host(host.clone());

    let session = SessionId::from("fig4025-session");
    let run = TurnId::from("fig4025-run");
    // The run switched frames once: its final physical turn is its second.
    let final_turn = lash_core::store::PhysicalTurn::derive_turn_id(&run, 1);
    let address = TurnAddress::new(session.clone(), final_turn.clone());
    let key = host
        .await_event_key(
            &address.execution_scope(),
            AwaitEventWaitIdentity::TurnTerminal,
        )
        .await
        .expect("derive the final turn's terminal key");
    let terminal = lash_core::facade_support::TurnTerminal::Committed {
        outcome: lash_sansio::TurnOutcome::Finished(lash_sansio::TurnFinish::AssistantMessage {
            text: "committed before its run closed".to_string(),
        }),
        session_revision: Some(2),
    };
    let published = serde_json::to_value(&terminal).expect("encode the terminal");

    // The first attach registers the terminal's one waiter; later attaches
    // join it, and one whose caller stops listening leaves it parked.
    let mut waiters = vec![registered_attach(&attach, &key, address.clone()).await];
    for _ in 0..2 {
        let attach = attach.clone();
        let address = address.clone();
        waiters.push(tokio::spawn(async move {
            attach.await_terminal(&address).await
        }));
    }
    let dropped = {
        let attach = attach.clone();
        let address = address.clone();
        tokio::spawn(async move { attach.await_terminal(&address).await })
    };
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    dropped.abort();

    // A run lost without a commit, in the same session, owes no terminal.
    let lost_run = TurnId::from("fig4025-lost-run");
    let lost_address = TurnAddress::new(session.clone(), lost_run.clone());
    let lost_key = host
        .await_event_key(
            &lost_address.execution_scope(),
            AwaitEventWaitIdentity::TurnTerminal,
        )
        .await
        .expect("derive the lost run's terminal key");
    let lost_waiter = registered_attach(&attach, &lost_key, lost_address).await;

    // The final turn committed, so its run's scope closes while the
    // commit's one-way terminal publish is still in flight: the publish
    // reaches the index only once the retirement has run.
    scope_close
        .close_run_scope(&RunTerminal {
            session_id: session.clone(),
            run: run.clone(),
            cause: RunTerminalCause::Committed {
                commit: TurnCommitId::new(run.clone(), 1),
                turn: final_turn.clone(),
                outcome: lash_core::store::RunCommittedOutcome::Finished(
                    lash_core::facade_support::TurnFinish::AssistantMessage {
                        text: String::new(),
                    },
                ),
            },
            head_revision: Some(2),
            at_ms: 1,
        })
        .await
        .expect("close the committed run's scope");
    scope_close
        .close_run_scope(&RunTerminal {
            session_id: session.clone(),
            run: lost_run.clone(),
            cause: RunTerminalCause::SubstrateLost { cancelled_by: None },
            head_revision: None,
            at_ms: 1,
        })
        .await
        .expect("close the lost run's scope");
    let lost = lost_waiter
        .await
        .expect("the lost run's waiter task")
        .expect_err("a run without a commit has no terminal to wait for");
    assert_eq!(
        lost.code,
        lash_core::RuntimeErrorCode::TurnControlUnknownOrRevoked,
        "retiring the lost run releases its waiter: {lost}"
    );

    let publish = host
        .publish_await_event(&key, Resolution::Ok(published.clone()))
        .await
        .expect("publish the committed terminal");
    assert!(
        !matches!(publish, Some(ResolveOutcome::AlreadyResolved { .. })),
        "the retirement left the terminal's promise open: {publish:?}"
    );
    for waiter in waiters {
        let observed = waiter
            .await
            .expect("the waiter task")
            .expect("every registered waiter reads the published terminal");
        assert_eq!(
            serde_json::to_value(&observed).expect("encode the observed terminal"),
            published
        );
    }
    let reread = attach
        .await_terminal(&address)
        .await
        .expect("an attach after the publish reads the published terminal");
    assert_eq!(
        serde_json::to_value(&reread).expect("encode the re-read terminal"),
        published
    );
    server.settle().await;
    assert_eq!(
        host.list_outstanding_await_event_keys(&session)
            .await
            .expect("list the session's outstanding waits"),
        Vec::<AwaitEventKey>::new(),
        "the publish settled every wait the retirement left registered"
    );
}

#[tokio::test]
pub(super) async fn late_terminal_attach_does_not_restore_a_closed_runs_index_rows() {
    let endpoint = Endpoint::builder()
        .bind(LashDurableWaitRegistryImpl::default().serve())
        .build();
    let session = SessionId::from("late-terminal-session");
    let key = restate_await_event_key(
        &ExecutionScope::turn(session.clone(), "closed-run"),
        AwaitEventWaitIdentity::TurnTerminal,
    )
    .expect("derive terminal key");
    let address = RestateDurableWaitAddress::for_key(&key);
    let mut state = BTreeMap::new();

    let registered = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "register",
        fig1943_invocation_with_state(
            session.as_str(),
            &RestateDurableWaitIndexRequest { key: key.clone() },
            &state,
        ),
    )
    .await
    .expect("register a late terminal attach");
    fig1943_apply_state_commands(&mut state, &registered);
    assert!(state.contains_key(&durable_wait_index_state_key(&address)));

    let settled = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "settle",
        fig1943_invocation_with_state(
            session.as_str(),
            &RestateDurableWaitSettleRequest {
                key,
                resolution: Resolution::Cancelled,
            },
            &state,
        ),
    )
    .await
    .expect("settle the terminal attach from its durable promise");
    fig1943_apply_state_commands(&mut state, &settled);
    assert!(
        !state.contains_key(&durable_wait_index_state_key(&address)),
        "the settled terminal registration must be retired"
    );
    assert!(
        !state
            .keys()
            .any(|key| key.starts_with("wait-index/v2/wait/")),
        "the workflow promise owns the terminal result"
    );
}
