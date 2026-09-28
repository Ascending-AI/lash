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
            format: crate::durable_wait::DURABLE_WAIT_REGISTRY_FORMAT_VERSION,
            body,
        })
        .expect("encode a stamped registry row")
    };
    let metadata = serde_json::to_value(RestateDurableWaitIndexMetadata::default())
        .expect("encode registry metadata");
    let mut state = BTreeMap::from([(
        crate::durable_wait::DURABLE_WAIT_INDEX_METADATA_KEY.to_string(),
        stamped(metadata),
    )]);
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
        state.len(),
        1,
        "only the index metadata survives root close"
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
