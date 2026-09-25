//! The late-completion law: once a group child is cancel-decided, a
//! completion delivered to its completion key is refused, typed, and the
//! child's recorded disposition stays `Cancel` (ADR 0099 §4, W17; FIG-3568).
//!
//! §4 names completion delivery as one of the four sinks the cancel fence
//! covers. A deferred tool child parks on its completion key; the opener's
//! `Cancel` close commits the child's cancel decision; an external resolver
//! then resolves the key late — through the same host surface
//! `core.completions().resolve(...)` reaches. The substrate that owns the
//! fence answers with `RuntimeEffectGroupChildCancelDecided` rather than
//! `Accepted`, writes nothing, and answers the same way on every retry.

use pretty_assertions::assert_eq;

use super::*;

/// W17: a completion-key resolve that arrives after the child's cancel
/// decision committed is refused with the typed
/// `RuntimeEffectGroupChildCancelDecided`, never `Accepted`, and the child's
/// recorded disposition stays `Cancel`.
///
/// The child is a deferred leaf under a `Cancel` close. The resolve is tried
/// twice: the refusal is the fence's, not a one-shot race, so a retry meets
/// the same answer. The key never reappears among the session's outstanding
/// waits, and the deferred body never runs again. On a durable tier the
/// reopen serves rank 0 as the cancelled terminal the close committed — the
/// late completion did not become the child's outcome. On the drain-less
/// Restate tier a closed group's ranks are unreadable by contract, so the
/// typed refusal and the untouched executions are the evidence.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_late_completion_after_a_cancel_decision_is_refused(
    fixture: &ToolChildLawFixture,
    prefix: &str,
) {
    let session_id = crate::SessionId::from(format!("{prefix}-late-completion"));
    let turn_id = crate::TurnId::from(format!("{prefix}-late-completion-turn"));
    let scope = crate::ExecutionScope::turn(session_id.clone(), turn_id);
    let opener = crate::EffectOpener::for_scope(&crate::admit(scope.clone()))
        .expect("a turn scope derives an opener");
    let group_key = format!("{prefix}-late-completion-group");
    let call_id = format!("{group_key}-call-0");

    let world = (fixture.make_world)(ToolChildWorldSpec {
        lease_ttl_ms: LIVE_LEASE_MS,
    })
    .await;
    let host = world.host;
    let scenario = scenario(fixture, &session_id, serde_json::Value::Null).await;
    let _guard = register_opener(
        &host,
        &scope,
        Arc::clone(&scenario.provider) as Arc<dyn crate::ToolProvider>,
        Arc::clone(&scenario.registry),
        Arc::clone(&scenario.process_env_store),
        opener,
        tokio_util::sync::CancellationToken::new(),
    );
    let scoped = host
        .scoped(crate::admit(scope.clone()))
        .expect("the group scope binds");
    let group = || async {
        single_leaf_group(
            &scope,
            &session_id,
            &group_key,
            &scenario.env_ref,
            LEAF_DEFERRED,
            ToolChildCompletionRouting::Durable,
            recorded_cancellation_authority(&host, &crate::admit(scope.clone())).await,
        )
    };
    let handle = scoped
        .controller()
        .open_effect_group(group().await)
        .await
        .expect("the group opens under the live opener");

    // The child parks on its completion key, and the key's wait is durable.
    let key = scenario.observation.parked_key(&call_id).await;
    await_key_registered(&host, &session_id, &key).await;

    // The opener's `Cancel` close commits the parked child's cancel decision.
    scoped
        .controller()
        .close_effect_group(handle, crate::LoserPolicy::Cancel)
        .await
        .expect("the caller closes under Cancel");

    // The late completion, twice: the refusal is typed and it is the fence's.
    for attempt in ["first", "retried"] {
        let refused = host
            .resolve_await_event(
                &key,
                crate::Resolution::Ok(serde_json::json!({ "leaf": "deferred", "late": true })),
            )
            .await;
        match refused {
            Err(error) => assert_eq!(
                error.code,
                crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelDecided,
                "the {attempt} late resolve is refused by the cancel fence: {error}"
            ),
            Ok(outcome) => panic!(
                "the {attempt} late resolve after the cancel decision answered {outcome:?}; \
                 ADR 0099 §4 refuses a completion delivered to a cancel-decided child"
            ),
        }
    }
    let outstanding = host
        .list_outstanding_await_event_keys(&session_id)
        .await
        .unwrap_or_default();
    assert!(
        !outstanding.contains(&key),
        "a cancel-decided child's completion key is no longer an outstanding wait"
    );
    assert_eq!(
        scenario.observation.executions_of("law_deferred").len(),
        1,
        "the deferred body ran once; the refused completion resumed nothing"
    );

    if world.drain.is_some() {
        // The recorded disposition stays `Cancel`: a reopen serves rank 0 as
        // the cancelled terminal the close decided, not the late completion.
        // The close may return while the cancelled child's task is still
        // unwinding, so a same-process reopen is retried until it is served.
        let deadline = std::time::Instant::now() + SETTLE_BUDGET;
        let settlement = loop {
            let mut handle = scoped
                .controller()
                .open_effect_group(group().await)
                .await
                .expect("the identical group reopens");
            match scoped
                .controller()
                .await_next_settlement(
                    &mut handle,
                    lash_core::TurnCancelWait::unobserved(
                        tokio_util::sync::CancellationToken::new(),
                    ),
                )
                .await
            {
                Ok(settlement) => break settlement,
                Err(error) => {
                    assert!(
                        std::time::Instant::now() < deadline,
                        "rank 0 was never served: {error}"
                    );
                    assert!(
                        error.to_string().contains("closed to its caller"),
                        "rank 0 failed to be served: {error}"
                    );
                    tokio::time::sleep(POLL).await;
                }
            }
        };
        assert_eq!(settlement.position, 0);
        let error = settlement
            .outcome
            .as_ref()
            .err()
            .unwrap_or_else(|| panic!("rank 0 is the cancelled terminal: {settlement:?}"));
        assert_eq!(
            error.code.as_str(),
            "runtime_effect_group_child_cancelled",
            "the recorded disposition stays the cancel the close decided"
        );
    }
}
