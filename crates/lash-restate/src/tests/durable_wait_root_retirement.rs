use super::process_await_redrive::{fig1943_apply_state_commands, fig1943_invocation_with_state};
use super::*;
use crate::object_state::StampedValue;

#[tokio::test]
pub(super) async fn closed_roots_leave_flat_wait_index_state_through_a_thousand_turns() {
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
        let root = TurnId::from(format!("root-{ordinal:04}"));
        let key = restate_await_event_key(
            &ExecutionScope::turn(session_id.clone(), root.clone()),
            AwaitEventWaitIdentity::TurnCancelGate,
        )
        .expect("derive turn-control key");
        let address = RestateDurableWaitAddress::for_key(&key);
        state.insert(
            durable_wait_index_state_key(&address),
            stamped(serde_json::to_value(&key).expect("encode key")),
        );
        state.insert(
            format!("wait-index/v2/resolution/{}", address.workflow_key),
            stamped(serde_json::to_value(Resolution::Cancelled).expect("encode terminal")),
        );
        let output = invoke_endpoint_body(
            &endpoint,
            "LashDurableWaitIndex",
            "retire_root",
            fig1943_invocation_with_state(
                object_key,
                &RestateDurableWaitRootRequest {
                    session_id: session_id.clone(),
                    root,
                    committed_turn: None,
                },
                &state,
            ),
        )
        .await
        .expect("retire a terminal root's index rows");
        assert!(
            restate_output_failure_message(&output)
                .or_else(|| restate_error_message(&output))
                .is_none(),
            "root retirement succeeds"
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
        "only the compat record and the index metadata survive root close"
    );

    let closed_root = TurnId::from("closed-root");
    let live_root = TurnId::from("live-root");
    let mut keys = Vec::new();
    for root in [&closed_root, &live_root] {
        let key = restate_await_event_key(
            &ExecutionScope::turn(session_id.clone(), root.clone()),
            AwaitEventWaitIdentity::TurnCancelGate,
        )
        .expect("derive a concurrent root's key");
        let address = RestateDurableWaitAddress::for_key(&key);
        let wait_key = durable_wait_index_state_key(&address);
        let resolution_key = format!("wait-index/v2/resolution/{}", address.workflow_key);
        state.insert(
            wait_key.clone(),
            stamped(serde_json::to_value(&key).expect("encode key")),
        );
        state.insert(
            resolution_key.clone(),
            stamped(serde_json::to_value(Resolution::Cancelled).expect("encode terminal")),
        );
        keys.push((wait_key, resolution_key));
    }
    let close = |state: &BTreeMap<String, Vec<u8>>| {
        fig1943_invocation_with_state(
            object_key,
            &RestateDurableWaitRootRequest {
                session_id: session_id.clone(),
                root: closed_root.clone(),
                committed_turn: None,
            },
            state,
        )
    };
    let output = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "retire_root",
        close(&state),
    )
    .await
    .expect("retire the closed root");
    fig1943_apply_state_commands(&mut state, &output);
    for key in [&keys[0].0, &keys[0].1] {
        assert!(!state.contains_key(key), "the closed root's row is retired");
    }
    for key in [&keys[1].0, &keys[1].1] {
        assert!(state.contains_key(key), "the live root's row survives");
    }
    let before_retry = state.clone();
    let retry = invoke_endpoint_body(
        &endpoint,
        "LashDurableWaitIndex",
        "retire_root",
        close(&state),
    )
    .await
    .expect("retry the same root close");
    fig1943_apply_state_commands(&mut state, &retry);
    assert_eq!(state, before_retry, "root retirement is idempotent");

    for root in ["fenced-root", "other-fenced-root"] {
        let request = RestateDurableWaitCancelDecidedRequest {
            scope: ExecutionScope::turn(session_id.clone(), root),
            wait: AwaitEventWaitIdentity::Custom {
                key: format!("completion-{root}"),
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
        "retire_root",
        fig1943_invocation_with_state(
            object_key,
            &RestateDurableWaitRootRequest {
                session_id: session_id.clone(),
                root: TurnId::from("fenced-root"),
                committed_turn: None,
            },
            &state,
        ),
    )
    .await
    .expect("retire the first root's cancel fence");
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
            .is_some_and(|id| id.contains("other-fenced-root"))
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
/// commit, so CloseRootScope can retire the root while that publish is still
/// in flight. Every wait registered on the terminal — live attaches, and a
/// capped read whose caller already gave up (send resolution's
/// `TERMINAL_READ`) — resolves with the real terminal, never `Cancelled`,
/// and a read after the cap reads it too. A root that ended without a commit
/// owes no terminal, so retiring it still releases its waiter.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
pub(super) async fn retiring_a_root_never_cancels_a_terminal_its_commit_still_publishes() {
    use lash_core::engine::ScopeCloseSink as _;
    use lash_core::store::{RootTerminal, RootTerminalCause, RootTerminalKind, TurnCommitId};

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
    let root = TurnId::from("fig4025-root");
    // The root switched frames once: its final physical turn is its second.
    let final_turn = lash_core::store::PhysicalTurn::derive_turn_id(&root, 1);
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
            text: "committed before its root closed".to_string(),
        }),
        session_revision: Some(2),
    };
    let published = serde_json::to_value(&terminal).expect("encode the terminal");

    let mut waiters = Vec::new();
    for _ in 0..3 {
        waiters.push(registered_attach(&attach, &key, address.clone()).await);
    }
    // Send resolution's capped read registers the same wait, and its caller
    // stops listening when the cap elapses; its invocation stays parked.
    registered_attach(&attach, &key, address.clone())
        .await
        .abort();

    // A root lost without a commit, in the same session, owes no terminal.
    let lost_root = TurnId::from("fig4025-lost-root");
    let lost_address = TurnAddress::new(session.clone(), lost_root.clone());
    let lost_key = host
        .await_event_key(
            &lost_address.execution_scope(),
            AwaitEventWaitIdentity::TurnTerminal,
        )
        .await
        .expect("derive the lost root's terminal key");
    let lost_waiter = registered_attach(&attach, &lost_key, lost_address).await;

    // The final turn committed, so its root's scope closes while the
    // commit's one-way terminal publish is still in flight: the publish
    // reaches the index only once the retirement has run.
    scope_close
        .close_root_scope(&RootTerminal {
            session_id: session.clone(),
            root: root.clone(),
            kind: RootTerminalKind::Answered,
            cause: RootTerminalCause::Committed {
                commit: TurnCommitId::new(root.clone(), 1),
                turn: final_turn.clone(),
                stop: None,
            },
            head_revision: Some(2),
            at_ms: 1,
        })
        .await
        .expect("close the committed root's scope");
    scope_close
        .close_root_scope(&RootTerminal {
            session_id: session.clone(),
            root: lost_root.clone(),
            kind: RootTerminalKind::Failed,
            cause: RootTerminalCause::SubstrateLost { cancelled_by: None },
            head_revision: None,
            at_ms: 1,
        })
        .await
        .expect("close the lost root's scope");
    let lost = lost_waiter
        .await
        .expect("the lost root's waiter task")
        .expect_err("a root without a commit has no terminal to wait for");
    assert_eq!(
        lost.code,
        lash_core::RuntimeErrorCode::TurnControlUnknownOrRevoked,
        "retiring the lost root releases its waiter: {lost}"
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
        .expect("a read after the cap reads the published terminal");
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
pub(super) async fn late_terminal_attach_does_not_restore_a_closed_roots_index_rows() {
    let endpoint = Endpoint::builder()
        .bind(LashDurableWaitRegistryImpl::default().serve())
        .build();
    let session = SessionId::from("late-terminal-session");
    let key = restate_await_event_key(
        &ExecutionScope::turn(session.clone(), "closed-root"),
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
        !state.contains_key(&format!(
            "wait-index/v2/resolution/{}",
            address.workflow_key
        )),
        "the workflow promise owns the terminal result"
    );
}
