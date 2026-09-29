//! The RLM registrations of the frame-open laws (FIG-4110) on the Restate
//! server double this crate opens.
//!
//! A context-pressure hook is protocol-neutral: an RLM session opens a
//! pressure frame the way a standard one does, and a root whose turn then
//! ends in `control.continue_as` commits both frames, in order, each exactly
//! once, however its execution dies. The live interpreter restarts from the
//! pressure frame's seed exactly as the durable execution state does.
//!
//! The laws live in lash-conformance; this file supplies the RLM protocol
//! plugin, how its model answers, switches frames and touches a session
//! global, and the tier.

#![expect(
    clippy::expect_used,
    reason = "test target: clippy's allow-unwrap-in-tests only exempts #[test] functions, and the registration helpers around them in this target are test code too"
)]

use std::sync::Arc;

use lash_core::EffectHost;

/// The RLM protocol on the cell channel, and how its model answers.
struct RlmFrameLawProtocol {
    factory: Arc<dyn lash_core::facade_support::PluginFactory>,
}

impl RlmFrameLawProtocol {
    fn shared(backend: &lash_core::Backend) -> Arc<dyn lash_conformance::FrameLawProtocol> {
        Arc::new(Self {
            factory: Arc::new(
                lash_protocol_rlm::RlmProtocolPluginFactory::new(
                lash_protocol_rlm::RlmProtocolPluginConfig::builder()
                    .channel(lash_protocol_rlm::RlmChannel::Cell)
                    .instruction_limit(lash_protocol_rlm::InstructionBound::instructions(
                        1_000_000,
                    ))
                    .memory_limit(lash_protocol_rlm::MemoryBound::mebibytes(64))
                    .build(),
                backend,
            )
            // The laws' sessions start no process from a cell.
            .with_process_lifecycle(false),
            ),
        })
    }
}

fn cell(code: &str) -> lash_core::LlmOutputPart {
    lash_core::LlmOutputPart::Text {
        text: format!("<typescript>\n{code}\n</typescript>"),
        response_meta: None,
    }
}

impl lash_conformance::FrameLawProtocol for RlmFrameLawProtocol {
    fn plugins(&self) -> Vec<Arc<dyn lash_core::facade_support::PluginFactory>> {
        vec![Arc::clone(&self.factory)]
    }

    fn answer(&self, text: &str) -> lash_core::LlmOutputPart {
        cell(&format!("finish({text:?});"))
    }

    fn continue_as(&self, task: &str) -> lash_core::LlmOutputPart {
        cell(&format!("await control.continue_as({{ task: {task:?} }});"))
    }

    fn execution_state(&self) -> Option<lash_conformance::ExecutionStateScript> {
        Some(lash_conformance::ExecutionStateScript {
            set_global: cell("globalThis.frameLawGlobal = \"kept\";\nfinish(\"set\");"),
            answer_global_type: cell("finish(typeof globalThis.frameLawGlobal);"),
        })
    }
}

mod restate_double {
    use super::*;

    /// The tier's [`lash_conformance::ConformanceTurnRunner`]: each attempt
    /// runs inside a handler of the double's deployment, on the scoped
    /// controller the invocation's journal owns, and a crashed attempt is
    /// replayed into the redrive.
    struct DoubleTurnRunner {
        backend: lash_restate_test::RestateTestBackend,
    }

    fn into_handler_attempt(
        attempt: lash_conformance::ConformanceTurnAttempt,
    ) -> lash_restate_test::HandlerAttempt {
        Arc::new(move |controller| {
            let attempt = Arc::clone(&attempt);
            Box::pin(async move {
                attempt(controller).await;
            })
        })
    }

    #[async_trait::async_trait]
    impl lash_conformance::ConformanceTurnRunner for DoubleTurnRunner {
        async fn run_turn(
            &self,
            admitted: lash_core::AdmittedScope,
            attempt: lash_conformance::ConformanceTurnAttempt,
        ) {
            self.backend
                .run_in_handler(admitted, into_handler_attempt(attempt))
                .await
                .expect("the double's handler runs the law's turn");
        }

        async fn run_crashed_then_redriven_turn(
            &self,
            admitted: lash_core::AdmittedScope,
            crashing: lash_conformance::ConformanceTurnAttempt,
            redrive: lash_conformance::ConformanceTurnAttempt,
        ) {
            self.backend
                .run_crashed_then_redriven(
                    admitted,
                    into_handler_attempt(crashing),
                    into_handler_attempt(redrive),
                )
                .await
                .expect("the double crashes and redrives the law's turn");
        }
    }

    async fn fixture() -> (
        lash_restate_test::RestateTestBackend,
        &'static str,
        Arc<dyn EffectHost>,
        Arc<dyn lash_core::StoreSet>,
        Arc<dyn lash_conformance::ConformanceTurnRunner>,
        Arc<dyn lash_conformance::FrameLawProtocol>,
    ) {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let double = lash_restate_test::backend(
            0x4110_0000 + NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
            lash_restate_test::ServerConfig::default(),
        )
        .await
        .expect("start the Restate server double");
        let backend = double.lash_backend();
        let host = backend.effect_host() as Arc<dyn EffectHost>;
        let protocol = RlmFrameLawProtocol::shared(&backend);
        (
            double.clone(),
            "rlm-frame-open",
            host,
            Arc::clone(double.engine_stores()),
            Arc::new(DoubleTurnRunner { backend: double })
                as Arc<dyn lash_conformance::ConformanceTurnRunner>,
            protocol,
        )
    }

    lash_conformance::frame_open_protocol_redrive_tests!({ fixture().await });

    lash_conformance::frame_open_execution_state_tests!({ fixture().await });
}
