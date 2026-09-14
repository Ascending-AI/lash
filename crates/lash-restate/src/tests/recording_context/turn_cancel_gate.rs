//! The test turn-cancellation gate the recording contexts share.
//!
//! Extracted from its parent purely to keep that file inside the repository's
//! test-file line budget; it is the same gate, with the same visibility.

use super::*;

pub(crate) type TestTurnCancelRaceFuture<'run, T> = Pin<
    Box<dyn Future<Output = Result<RestateTurnCancelRaceOutcome<T>, TerminalError>> + Send + 'run>,
>;

#[derive(Default)]
pub(crate) struct TestTurnCancelGate {
    state: Mutex<TestTurnCancelGateState>,
}

#[derive(Default)]
pub(crate) struct TestTurnCancelGateState {
    next_registration_id: usize,
    revoked_sessions: HashSet<SessionId>,
    registrations: HashMap<usize, TestTurnCancelGateEntry>,
}

pub(crate) struct TestTurnCancelGateEntry {
    session_id: SessionId,
    key: AwaitEventKey,
    sender: tokio::sync::oneshot::Sender<RestateTurnCancelWake>,
}

pub(crate) struct TestTurnCancelRegistration {
    id: usize,
    receiver: tokio::sync::oneshot::Receiver<RestateTurnCancelWake>,
}

pub(crate) enum TestTurnCancelRegistrationVerdict {
    Registered(TestTurnCancelRegistration),
    Revoked,
}

impl TestTurnCancelGate {
    pub(crate) fn register(
        &self,
        key: AwaitEventKey,
    ) -> Result<TestTurnCancelRegistrationVerdict, TerminalError> {
        let Some(session_id) = key.scope.session_id().map(SessionId::from) else {
            return Err(TerminalError::new(
                "turn cancellation gate is missing its session id",
            ));
        };
        let mut state = self.state.lock_recover();
        if state.revoked_sessions.contains(&session_id) {
            return Ok(TestTurnCancelRegistrationVerdict::Revoked);
        }
        let id = state.next_registration_id;
        state.next_registration_id += 1;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        state.registrations.insert(
            id,
            TestTurnCancelGateEntry {
                session_id,
                key,
                sender,
            },
        );
        Ok(TestTurnCancelRegistrationVerdict::Registered(
            TestTurnCancelRegistration { id, receiver },
        ))
    }

    pub(crate) fn unregister(&self, registration_id: usize) {
        self.state
            .lock_recover()
            .registrations
            .remove(&registration_id);
    }

    pub(crate) fn resolve(&self, key: &AwaitEventKey, wake: RestateTurnCancelWake) -> bool {
        self.wake_matching(|entry| entry.key == *key, wake)
    }

    pub(crate) fn revoke_session(&self, session_id: &SessionId) {
        self.state
            .lock_recover()
            .revoked_sessions
            .insert(SessionId::from(session_id.to_string()));
        self.wake_matching(
            |entry| entry.session_id == session_id,
            RestateTurnCancelWake::SessionRevoked,
        );
    }

    pub(crate) fn is_revoked(&self, session_id: &SessionId) -> bool {
        self.state
            .lock_recover()
            .revoked_sessions
            .contains(session_id)
    }

    pub(crate) fn registration_count(&self) -> usize {
        self.state.lock_recover().registrations.len()
    }

    pub(crate) fn registered_keys(&self) -> Vec<AwaitEventKey> {
        let mut keys = self
            .state
            .lock_recover()
            .registrations
            .values()
            .map(|entry| entry.key.clone())
            .collect::<Vec<_>>();
        keys.sort_by_key(AwaitEventKey::promise_key);
        keys
    }

    pub(crate) fn wake_matching(
        &self,
        predicate: impl Fn(&TestTurnCancelGateEntry) -> bool,
        wake: RestateTurnCancelWake,
    ) -> bool {
        let mut state = self.state.lock_recover();
        let registration_ids = state
            .registrations
            .iter()
            .filter_map(|(id, entry)| predicate(entry).then_some(*id))
            .collect::<Vec<_>>();
        for registration_id in &registration_ids {
            if let Some(entry) = state.registrations.remove(registration_id) {
                let _ = entry.sender.send(wake);
            }
        }
        !registration_ids.is_empty()
    }
}

pub(crate) fn test_turn_cancel_wake_outcome<T>(
    wake: RestateTurnCancelWake,
    session_id: SessionId,
) -> RestateTurnCancelRaceOutcome<T> {
    match wake {
        RestateTurnCancelWake::TurnCancelled | RestateTurnCancelWake::TurnCancelDeferred => {
            RestateTurnCancelRaceOutcome::TurnCancelled
        }
        RestateTurnCancelWake::SessionRevoked => {
            RestateTurnCancelRaceOutcome::SessionRevoked { session_id }
        }
    }
}

pub(crate) fn test_sleep_or_turn_cancel<'run, 'ctx, C>(
    context: &'run C,
    gate: &'run TestTurnCancelGate,
    duration: Duration,
    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
    cancellation: tokio_util::sync::CancellationToken,
) -> TestTurnCancelRaceFuture<'run, ()>
where
    C: RestateControllerContext<'ctx> + ?Sized,
    'ctx: 'run,
{
    Box::pin(async move {
        let Some(turn_cancel) = turn_cancel else {
            return tokio::select! {
                result = context.sleep_send(duration) => {
                    result.map(RestateTurnCancelRaceOutcome::Completed)
                }
                _ = cancellation.cancelled() => {
                    Ok(RestateTurnCancelRaceOutcome::TurnCancelled)
                }
            };
        };
        let session_id = turn_cancel
            .key
            .scope
            .session_id()
            .map(SessionId::from)
            .ok_or_else(|| {
                TerminalError::new("turn cancellation gate is missing its session id")
            })?;
        let turn_cancel_key = turn_cancel.key;
        let mut registration = match gate.register(turn_cancel_key.clone())? {
            TestTurnCancelRegistrationVerdict::Registered(registration) => registration,
            TestTurnCancelRegistrationVerdict::Revoked => {
                return Ok(RestateTurnCancelRaceOutcome::SessionRevoked { session_id });
            }
        };
        let mut escalated = false;
        let guarded = context.sleep_send(duration);
        tokio::pin!(guarded);
        loop {
            tokio::select! {
                biased;
                result = &mut guarded => {
                    gate.unregister(registration.id);
                    return result.map(RestateTurnCancelRaceOutcome::Completed);
                }
                wake = &mut registration.receiver => {
                    let wake = wake.map_err(|_| TerminalError::new("test turn cancellation gate was dropped"))?;
                    match test_turn_cancel_wake_step(gate, &turn_cancel_key, escalated, wake)? {
                        TestTurnCancelWakeStep::Continue(next) => {
                            registration = next;
                            escalated = true;
                        }
                        TestTurnCancelWakeStep::Unwind(wake) => {
                            return Ok(test_turn_cancel_wake_outcome(wake, session_id));
                        }
                    }
                }
                _ = cancellation.cancelled() => {
                    gate.unregister(registration.id);
                    return Ok(RestateTurnCancelRaceOutcome::TurnCancelled);
                }
            }
        }
    })
}

pub(crate) fn test_await_event_or_turn_cancel<'run, 'ctx, C>(
    context: &'run C,
    gate: &'run TestTurnCancelGate,
    request: RestateDurableWaitAwaitRequest,
    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
    cancellation: tokio_util::sync::CancellationToken,
) -> TestTurnCancelRaceFuture<'run, Resolution>
where
    C: RestateControllerContext<'ctx> + ?Sized,
    'ctx: 'run,
{
    Box::pin(async move {
        let Some(turn_cancel) = turn_cancel else {
            return context
                .await_event(request, cancellation)
                .await
                .map(RestateTurnCancelRaceOutcome::Completed);
        };
        let session_id = turn_cancel
            .key
            .scope
            .session_id()
            .map(SessionId::from)
            .ok_or_else(|| {
                TerminalError::new("turn cancellation gate is missing its session id")
            })?;
        let turn_cancel_key = turn_cancel.key;
        let mut registration = match gate.register(turn_cancel_key.clone())? {
            TestTurnCancelRegistrationVerdict::Registered(registration) => registration,
            TestTurnCancelRegistrationVerdict::Revoked => {
                return Ok(RestateTurnCancelRaceOutcome::SessionRevoked { session_id });
            }
        };
        let event_key = request.key.clone();
        let mut escalated = false;
        let guarded = context.await_event(request, cancellation);
        tokio::pin!(guarded);
        loop {
            tokio::select! {
                biased;
                wake = &mut registration.receiver => {
                    let wake = wake.map_err(|_| TerminalError::new("test turn cancellation gate was dropped"))?;
                    match test_turn_cancel_wake_step(gate, &turn_cancel_key, escalated, wake)? {
                        TestTurnCancelWakeStep::Continue(next) => {
                            registration = next;
                            escalated = true;
                        }
                        TestTurnCancelWakeStep::Unwind(wake) => {
                            if wake != RestateTurnCancelWake::SessionRevoked {
                                context.resolve_event(RestateDurableWaitResolveRequest {
                                    key: event_key,
                                    resolution: Resolution::Cancelled,
                                }).await?;
                            }
                            return Ok(test_turn_cancel_wake_outcome(wake, session_id));
                        }
                    }
                }
                result = &mut guarded => {
                    gate.unregister(registration.id);
                    return result.map(RestateTurnCancelRaceOutcome::Completed);
                }
            }
        }
    })
}

pub(crate) fn test_await_process_terminal_or_turn_cancel<'run, 'ctx, C>(
    context: &'run C,
    gate: &'run TestTurnCancelGate,
    process_id: ProcessId,
    turn_cancel: Option<RestateDurableWaitAwaitRequest>,
) -> TestTurnCancelRaceFuture<'run, Box<ProcessAwaitOutput>>
where
    C: RestateControllerContext<'ctx> + ?Sized,
    'ctx: 'run,
{
    Box::pin(async move {
        let Some(turn_cancel) = turn_cancel else {
            return context
                .await_process_terminal(process_id)
                .await
                .map(Box::new)
                .map(RestateTurnCancelRaceOutcome::Completed);
        };
        let session_id = turn_cancel
            .key
            .scope
            .session_id()
            .map(SessionId::from)
            .ok_or_else(|| {
                TerminalError::new("turn cancellation gate is missing its session id")
            })?;
        let turn_cancel_key = turn_cancel.key;
        let mut registration = match gate.register(turn_cancel_key.clone())? {
            TestTurnCancelRegistrationVerdict::Registered(registration) => registration,
            TestTurnCancelRegistrationVerdict::Revoked => {
                return Ok(RestateTurnCancelRaceOutcome::SessionRevoked { session_id });
            }
        };
        let mut escalated = false;
        let guarded = context.await_process_terminal(process_id);
        tokio::pin!(guarded);
        loop {
            tokio::select! {
                biased;
                wake = &mut registration.receiver => {
                    let wake = wake.map_err(|_| TerminalError::new("test turn cancellation gate was dropped"))?;
                    match test_turn_cancel_wake_step(gate, &turn_cancel_key, escalated, wake)? {
                        TestTurnCancelWakeStep::Continue(next) => {
                            registration = next;
                            escalated = true;
                        }
                        TestTurnCancelWakeStep::Unwind(wake) => {
                            return Ok(test_turn_cancel_wake_outcome(wake, session_id));
                        }
                    }
                }
                result = &mut guarded => {
                    gate.unregister(registration.id);
                    return result
                        .map(Box::new)
                        .map(RestateTurnCancelRaceOutcome::Completed);
                }
            }
        }
    })
}

pub(crate) async fn wait_for_test_turn_cancel_registration(gate: &TestTurnCancelGate) {
    for _ in 0..100 {
        if gate.registration_count() > 0 {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("test turn cancellation gate was never registered");
}
