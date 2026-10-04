mod executor;

mod tests {

    use crate::SessionId;
    use crate::runtime::effect::*;
    use crate::support::prelude::*;

    const SEED: u64 = 0x5_f700;

    #[tokio::test]
    async fn resolver_defaults_refuse_turn_control_without_an_explicit_host() {
        struct UnsupportedResolver;
        impl AwaitEventResolver for UnsupportedResolver {
            /// A test double that mints keys under no durable authority.
            fn await_event_authority_binding_id(&self) -> Option<String> {
                None
            }
        }

        let resolver = UnsupportedResolver;
        let scope = ExecutionScope::turn("unsupported-session", "unsupported-turn");
        for wait in [
            AwaitEventWaitIdentity::TurnCancelGate,
            AwaitEventWaitIdentity::TurnTerminal,
            AwaitEventWaitIdentity::tool_completion(lash_core_execution::ToolCallId::fixture(
                "unsupported-call",
            )),
        ] {
            let error = resolver
                .await_event_key(&scope, wait)
                .await
                .expect_err("default resolver must refuse every identity");
            assert_eq!(error.code.as_str(), "await_event_unsupported");
        }

        let double =
            crate::support::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let key = double
            .lash_backend()
            .effect_host()
            .await_event_key(&scope, AwaitEventWaitIdentity::TurnCancelGate)
            .await
            .expect("explicit backend host key");
        assert_eq!(
            resolver
                .resolve_await_event(&key, Resolution::Cancelled)
                .await
                .expect("default resolution has one opaque shape"),
            ResolveOutcome::UnknownOrRevoked
        );
        for error in [
            resolver
                .peek_await_event(&key)
                .await
                .expect_err("default resolver must refuse reads"),
            resolver
                .await_await_event(&key, tokio_util::sync::CancellationToken::new())
                .await
                .expect_err("default resolver must refuse waits"),
            resolver
                .revoke_await_events_for_session(&SessionId::from("unsupported-session"))
                .await
                .expect_err("default resolver must refuse revocation"),
        ] {
            assert_eq!(error.code.as_str(), "await_event_unsupported");
        }
    }
}
