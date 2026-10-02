use super::*;
use crate::durable_wait::restate_await_event_key;

fn service_call_error(status: u16) -> crate::RestateHttpError {
    crate::RestateHttpError::Status {
        operation: "Restate object call",
        url: "https://restate.invalid/EffectGroupIndex/group/probe".to_string(),
        status,
        body: "not found".to_string(),
    }
}

#[test]
fn effect_group_ingress_404_is_restate_service_unregistered() {
    let error = ingress_group_error("EffectGroupIndex/probe", service_call_error(404));

    assert_eq!(error.code, RuntimeErrorCode::EngineServiceUnregistered);
    assert!(error.message.contains("EffectGroupIndex/probe"));
}

#[test]
fn effect_group_ingress_unavailability_is_retryable() {
    let error = ingress_group_error("EffectGroupIndex/probe", service_call_error(503));

    assert_eq!(error.code, RuntimeErrorCode::EngineAwaitEventAwait);
    assert!(error.code.is_retryable());
}

#[test]
fn effect_group_ingress_definitive_refusal_keeps_its_shape_code() {
    let error = ingress_group_error("EffectGroupIndex/probe", service_call_error(400));

    assert_eq!(error.code, RuntimeErrorCode::RuntimeEffectGroupShape);
    assert!(error.code.is_terminal());
}

/// Eight threads registering different resolvers on one host: exactly one
/// wins, and every loser is refused rather than silently dropped
/// (FIG-1578's law, re-pinned on the engine host after the SQL effect
/// engine's deletion — FIG-3928).
///
/// The `OnceLock` race is what [`OnceLock::set`] arbitrates: a
/// `get`-then-`set` pair leaves a window in which two threads both read
/// `None`, both write, and the loser is told `Ok` while its resolver went
/// nowhere, so the endpoint's dispatch would route a journaled child
/// through a resolver its wiring code did not register.
#[test]
fn concurrent_registration_of_different_resolvers_refuses_every_loser() {
    const REGISTRARS: usize = 8;

    /// A resolver that routes nothing: this law is about which
    /// registration wins, not about what a child does.
    struct NoChildRuns;

    impl GroupExecutors for NoChildRuns {
        fn executor_for(
            &self,
            _envelope: &RuntimeEffectEnvelope,
        ) -> Option<RuntimeEffectLocalExecutor<'static>> {
            None
        }
    }

    let host = Arc::new(RestateEffectHost::new_for_test(RestateConnection::new(
        "https://restate.invalid",
    )));

    let barrier = Arc::new(std::sync::Barrier::new(REGISTRARS));
    let outcomes: Vec<_> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..REGISTRARS)
            .map(|_| {
                let host = Arc::clone(&host);
                let barrier = Arc::clone(&barrier);
                scope.spawn(move || {
                    // A distinct allocation per thread, so the
                    // same-resolver no-op cannot be mistaken for a
                    // winner.
                    let executors = Arc::new(NoChildRuns) as Arc<dyn GroupExecutors>;
                    barrier.wait();
                    host.register_group_executors(executors)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("a registrar thread"))
            .collect()
    });

    assert_eq!(
        outcomes.iter().filter(|outcome| outcome.is_ok()).count(),
        1,
        "exactly one of {REGISTRARS} different resolvers may be this host's \
         answer to what runs a journaled grouped child"
    );
    for refusal in outcomes.iter().filter_map(|outcome| outcome.as_ref().err()) {
        assert_eq!(
            refusal.code,
            RuntimeErrorCode::RuntimeEffectGroupShape,
            "a loser learns its resolver is not the host's"
        );
    }

    // The winner's registration stands: re-registering the resolver the
    // host now holds is the same-resolver no-op, and a ninth different
    // resolver still meets the conflict answer.
    let held = Arc::clone(
        host.controller
            .group_executors
            .get()
            .expect("one registrar won"),
    );
    assert!(
        host.register_group_executors(held).is_ok(),
        "re-registering the held resolver is a no-op"
    );
    assert_eq!(
        host.register_group_executors(Arc::new(NoChildRuns))
            .expect_err("a later different resolver still loses")
            .code,
        RuntimeErrorCode::RuntimeEffectGroupShape,
    );
}

/// A host that serves a lane with no resolver registered can never run a
/// group child, and says which capability it lacks; once one is registered
/// the miss is that resolver's to classify (FIG-4550).
#[test]
fn a_host_with_no_registered_resolver_names_the_missing_registration() {
    struct NoChildRuns;

    impl GroupExecutors for NoChildRuns {
        fn executor_for(
            &self,
            _envelope: &RuntimeEffectEnvelope,
        ) -> Option<RuntimeEffectLocalExecutor<'static>> {
            None
        }
    }

    let host = RestateEffectHost::new_for_test(RestateConnection::new("https://restate.invalid"));
    let envelope = RuntimeEffectEnvelope::new(
        lash_core::RuntimeEffectInvocation::new(
            lash_core::EffectAddress::new(
                ExecutionScope::runtime_operation("group"),
                "group-1:child:0",
            )
            .expect("valid child address"),
            lash_core::RuntimeAttribution::none(),
            "effect",
        ),
        lash_core::RuntimeEffectCommand::LanguageRuntimeValue {
            operation: "child".to_owned(),
        },
    );
    let executors = host.group_executors();
    assert!(executors.executor_for(&envelope).is_none());
    assert_eq!(
        executors.missing_capability(&envelope),
        Some(lash_core::GroupChildCapability::GroupExecutors)
    );

    host.register_group_executors(Arc::new(NoChildRuns) as Arc<dyn GroupExecutors>)
        .expect("the first resolver registers");
    assert!(executors.executor_for(&envelope).is_none());
    assert_eq!(
        executors.missing_capability(&envelope),
        None,
        "a registered resolver that cannot tell claims nothing permanent"
    );
}

#[test]
fn session_administrative_read_rejects_non_session_scope_aliases() {
    for scope in [
        ExecutionScope::process(lash_core::ProcessId::fixture("alias-process")),
        ExecutionScope::runtime_operation("alias-operation"),
    ] {
        let alias = SessionId::fixture(durable_wait_index_key_for_scope(&scope));
        let key = restate_await_event_key(
            &scope,
            AwaitEventWaitIdentity::tool_completion(lash_core::ToolCallId::fixture("alias-wait")),
        )
        .expect("derive non-session wait key");

        assert!(
            outstanding_owned_by_session(&alias, vec![key]).is_empty(),
            "a non-session wait indexed at `{alias}` must not be advertised as session-owned"
        );
    }
}
