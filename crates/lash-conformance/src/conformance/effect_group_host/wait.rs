//! ADR 0099 §12 on every host: a durable wait as an effect-group child.
//!
//! Selection never cancels a losing wait — it stays admitted while the opener
//! holds the handle — and the opener's `Cancel` close releases the wait
//! itself, never the process it watches. The release is owed whether the wait
//! had parked or the close reached the child first.
//!
//! A child of [`effect_group_host`](super), whose helpers these laws share,
//! so each file keeps its line budget.

use super::*;
use pretty_assertions::assert_eq;

/// §12: a losing `AwaitEvent` child stays admitted for as long as the opener
/// holds the handle, and the close that finally decides it releases *the wait*
/// — it does not reach the process the wait was watching.
///
/// The group is `[AwaitEvent(process signal), Sleep(short)]` under
/// `RunToCompletion`: the sleep wins rank 1 while the process await is still
/// parked. Three observations pin the loser's state down. First, after the
/// winner is consumed the loser's rank is still unwritten — the claim the
/// parked wait holds is `in_progress`, neither cancelled nor settled. Second,
/// `close(Cancel)` journals the cancelled-child terminal, which is the wait's
/// own release: a late completion resolution finds the wait already answered.
/// Third — the half §12 makes a law rather than a courtesy — a second parked
/// wait on the *same* process still resolves `Accepted` afterwards: whatever
/// released the loser never touched the process.
///
/// A tier that cannot run an `AwaitEvent` group child answers the open with
/// the typed refusal and stops; that arm is the report, not a skip.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_losing_wait_stays_admitted_until_the_group_releases_it<F: Fn() -> Host>(
    make: &F,
    prefix: &str,
) {
    let host = make();
    let session_id = crate::SessionId::from(format!("{prefix}-losing-wait-session"));
    let execution_scope = ExecutionScope::turn(
        session_id.clone(),
        crate::TurnId::from(format!("{prefix}-losing-wait-turn")),
    );
    let scoped = host
        .scoped(admit(execution_scope.clone()))
        .expect("a scope binds");
    let key = group_key(prefix, "losing-wait");
    let companion_key_str = group_key(prefix, "losing-wait-companion");
    let process_id = crate::ProcessId::from(format!("{prefix}-watched-process"));

    let envelope = |replay_key: String, command: RuntimeEffectCommand| {
        RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                EffectAddress::new(execution_scope.clone(), replay_key)
                    .expect("valid group-child address"),
                RuntimeAttribution::none(),
                "effect",
            ),
            command,
        )
    };
    let header = |key: &str| {
        RuntimeEffectInvocation::new(
            EffectAddress::new(execution_scope.clone(), format!("{key}:group"))
                .expect("valid group address"),
            RuntimeAttribution::none(),
            "group",
        )
    };

    // The losing child parks on the watched process's exit signal; the winning
    // child is the shortest sleep the substrate will schedule.
    let await_key = scoped
        .controller()
        .await_event_key(
            &execution_scope,
            crate::AwaitEventWaitIdentity::process_signal(process_id.clone(), "exit", 1),
        )
        .await
        .expect("the process-await key mints");
    let losing_group = RuntimeEffectGroup::try_new(
        header(&key),
        key.clone(),
        vec![
            envelope(
                format!("{key}:child:0"),
                RuntimeEffectCommand::AwaitEvent {
                    key: await_key.clone(),
                },
            ),
            envelope(
                format!("{key}:child:1"),
                RuntimeEffectCommand::Sleep {
                    spec: lash_core::SleepSpec::For { duration_ms: 1 },
                },
            ),
        ],
        GroupWakePolicy::All,
        RUN,
    )
    .expect("the two-child group assembles");
    let mut handle = match scoped
        .controller()
        .open_effect_group(staged(
            losing_group,
            vec![
                RuntimeEffectLocalExecutor::await_event(CancellationToken::new(), None),
                RuntimeEffectLocalExecutor::sleep(CancellationToken::new()),
            ],
        ))
        .await
    {
        Ok(handle) => handle,
        // Where `AwaitEvent` cannot be a group child the open is refused by
        // name; the refusal is the arm this tier reports.
        Err(error)
            if matches!(
                error.code,
                crate::RuntimeErrorCode::EffectGroupUnsupported
                    | crate::RuntimeErrorCode::AwaitEventUnsupported
            ) =>
        {
            return;
        }
        Err(error) => panic!("the process-await group opens or is refused by name: {error}"),
    };

    let first = next(&scoped, &mut handle)
        .await
        .expect("the first settlement arrives");
    // A tier whose group dispatch routes the `AwaitEvent` child to an
    // executor that cannot park it answers with the child's typed refusal
    // settlement rather than a parked wait — that arm is its report.
    if first.position == 0
        && let Err(error) = &first.outcome
        && matches!(
            error.code,
            crate::RuntimeErrorCode::RuntimeEffectLocalExecutorMismatch
                | crate::RuntimeErrorCode::EffectGroupUnsupported
                | crate::RuntimeErrorCode::AwaitEventUnsupported
        )
    {
        return;
    }
    assert_eq!(first.position, 1, "the sleep settles first: {first:?}");

    // Still admitted: the loser's rank is unwritten while the opener holds the
    // handle — not cancelled, not settled.
    assert!(
        scoped
            .controller()
            .read_group_settlement(&key, 2)
            .await
            .expect("the rank-2 read is answered")
            .is_none(),
        "the losing wait is still admitted while the opener lives"
    );

    // A second parked wait on the same process, in a sibling group the close
    // is not addressed to. Where the host can enumerate its waits, park it and
    // see it registered before the close; where it cannot, the resolve below
    // is still the verdict.
    let companion_key = scoped
        .controller()
        .await_event_key(
            &execution_scope,
            crate::AwaitEventWaitIdentity::process_signal(process_id, "heartbeat", 1),
        )
        .await
        .expect("the companion key mints");
    let companion_group = RuntimeEffectGroup::try_new(
        header(&companion_key_str),
        companion_key_str.clone(),
        vec![envelope(
            format!("{companion_key_str}:child:0"),
            RuntimeEffectCommand::AwaitEvent {
                key: companion_key.clone(),
            },
        )],
        GroupWakePolicy::All,
        RUN,
    )
    .expect("the companion group assembles");
    let companion_handle = scoped
        .controller()
        .open_effect_group(staged(
            companion_group,
            vec![RuntimeEffectLocalExecutor::await_event(
                CancellationToken::new(),
                None,
            )],
        ))
        .await
        .expect("the companion group opens");
    let can_list = host
        .list_outstanding_await_event_keys(&session_id)
        .await
        .is_ok();
    if can_list {
        tokio::time::timeout(AWAIT_BUDGET, async {
            loop {
                if matches!(
                    host.list_outstanding_await_event_keys(&session_id).await,
                    Ok(keys) if keys.contains(&companion_key)
                ) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("the companion wait registers before the close");
    } else {
        // Give the companion child's claim a beat to commit so its key is live
        // before the close is issued.
        tokio::time::sleep(Duration::from_millis(250)).await;
    }

    close(&scoped, handle, LoserPolicy::Cancel)
        .await
        .expect("the close journals the loser's cancel decision");

    let decided = scoped
        .controller()
        .read_group_settlement(&key, 2)
        .await
        .expect("the rank-2 read is answered")
        .expect("the close settled the losing rank");
    assert_eq!(
        decided.child_replay_key,
        format!("{key}:child:0"),
        "rank 2 is the losing wait's terminal: {decided:?}"
    );
    assert_eq!(
        decided
            .outcome
            .expect_err("a cancel-decided child holds the typed terminal")
            .code,
        crate::RuntimeErrorCode::RuntimeEffectGroupChildCancelled,
        "the loser is released as a cancelled child terminal"
    );

    // The release is the wait's own terminal. Restate's close writes it in
    // its own journal (FIG-3630); on the other tiers the cancelled child's
    // release arm writes it, asynchronous to the close, so the law waits for
    // it to land rather than asserting the instant. A revoked
    // promise is released the same way: the peek error is also an answer.
    let released = tokio::time::timeout(AWAIT_BUDGET, async {
        loop {
            match scoped.controller().peek_await_event(&await_key).await {
                Ok(Some(terminal)) => break Ok(terminal),
                Ok(None) => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(error) => break Err(error),
            }
        }
    })
    .await
    .expect("the close released the losing wait before the budget ran out");
    match released {
        Ok(terminal) => assert!(
            matches!(terminal, crate::Resolution::Cancelled),
            "the losing wait's release terminal is the cancellation, not a completion: {terminal:?}"
        ),
        Err(error) => assert_eq!(
            error.code,
            crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked,
            "a revoked wait is a release; anything else is not: {error}"
        ),
    }
    let late = scoped
        .controller()
        .resolve_await_event(
            &await_key,
            crate::Resolution::Ok(serde_json::json!("late-exit")),
        )
        .await
        .expect("the late resolution is answered");
    assert!(
        !matches!(late, crate::ResolveOutcome::Accepted),
        "the close released the losing wait itself: {late:?}"
    );

    // And the process was not cancelled: its other parked wait still takes a
    // resolution — a process teardown would have swept it.
    let companion_resolution = scoped
        .controller()
        .resolve_await_event(
            &companion_key,
            crate::Resolution::Ok(serde_json::json!("heartbeat")),
        )
        .await
        .expect("the companion resolution is answered");
    assert_eq!(
        companion_resolution,
        crate::ResolveOutcome::Accepted,
        "the watched process was not cancelled: its other wait still resolves"
    );

    // Tidying up: the companion group's child is now settled by the resolve;
    // closing it leaves the suite no parked work.
    close(&scoped, companion_handle, LoserPolicy::Cancel)
        .await
        .expect("the companion group closes");
}

/// §12, the unparked half: a wait child the close cancels *before* it ever
/// parks is released all the same. Opener close cancels and releases the
/// losing wait; whether the child's body had claimed, admitted or parked by
/// then is a scheduling accident, not a different contract. The group is a
/// lone `AwaitEvent` child closed under `Cancel` straight after the open, so
/// on a host whose children dispatch asynchronously the close usually lands
/// before the child claims: the release must still arrive, and a late
/// resolution must still find the wait answered.
///
/// A tier that cannot run an `AwaitEvent` group child answers the open with
/// the typed refusal and stops; that arm is the report, not a skip.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_wait_cancelled_before_it_parks_is_still_released<F: Fn() -> Host>(
    make: &F,
    prefix: &str,
) {
    let host = make();
    let session_id = crate::SessionId::from(format!("{prefix}-unparked-wait-session"));
    let execution_scope = ExecutionScope::turn(
        session_id,
        crate::TurnId::from(format!("{prefix}-unparked-wait-turn")),
    );
    let scoped = host
        .scoped(admit(execution_scope.clone()))
        .expect("a scope binds");
    let key = group_key(prefix, "unparked-wait");
    let await_key = scoped
        .controller()
        .await_event_key(
            &execution_scope,
            crate::AwaitEventWaitIdentity::process_signal(
                crate::ProcessId::from(format!("{prefix}-unparked-process")),
                "exit",
                1,
            ),
        )
        .await
        .expect("the process-await key mints");
    let group = RuntimeEffectGroup::try_new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(execution_scope.clone(), format!("{key}:group"))
                .expect("valid group address"),
            RuntimeAttribution::none(),
            "group",
        ),
        key.clone(),
        vec![RuntimeEffectEnvelope::new(
            RuntimeEffectInvocation::new(
                EffectAddress::new(execution_scope.clone(), format!("{key}:child:0"))
                    .expect("valid group-child address"),
                RuntimeAttribution::none(),
                "effect",
            ),
            RuntimeEffectCommand::AwaitEvent {
                key: await_key.clone(),
            },
        )],
        GroupWakePolicy::All,
        RUN,
    )
    .expect("the one-child group assembles");
    let handle = match scoped
        .controller()
        .open_effect_group(staged(
            group,
            vec![RuntimeEffectLocalExecutor::await_event(
                CancellationToken::new(),
                None,
            )],
        ))
        .await
    {
        Ok(handle) => handle,
        Err(error)
            if matches!(
                error.code,
                crate::RuntimeErrorCode::EffectGroupUnsupported
                    | crate::RuntimeErrorCode::AwaitEventUnsupported
            ) =>
        {
            return;
        }
        Err(error) => panic!("the process-await group opens or is refused by name: {error}"),
    };

    close(&scoped, handle, LoserPolicy::Cancel)
        .await
        .expect("the close journals the wait child's cancel decision");

    // The release is asynchronous to the close on every tier, so the law
    // waits for it to land; a revoked promise is released the same way.
    let released = tokio::time::timeout(AWAIT_BUDGET, async {
        loop {
            match scoped.controller().peek_await_event(&await_key).await {
                Ok(Some(terminal)) => break Ok(terminal),
                Ok(None) => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(error) => break Err(error),
            }
        }
    })
    .await
    .expect("the close released the unparked wait before the budget ran out");
    match released {
        Ok(terminal) => assert!(
            matches!(terminal, crate::Resolution::Cancelled),
            "the unparked wait's release terminal is the cancellation: {terminal:?}"
        ),
        Err(error) => assert_eq!(
            error.code,
            crate::RuntimeErrorCode::AwaitEventUnknownOrRevoked,
            "a revoked wait is a release; anything else is not: {error}"
        ),
    }
    let late = scoped
        .controller()
        .resolve_await_event(
            &await_key,
            crate::Resolution::Ok(serde_json::json!("late-exit")),
        )
        .await
        .expect("the late resolution is answered");
    assert!(
        !matches!(late, crate::ResolveOutcome::Accepted),
        "the close released the unparked wait itself: {late:?}"
    );
}
