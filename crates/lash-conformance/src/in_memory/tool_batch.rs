//! The native tier's registration of the cross-tier tool-batch parallelism law,
//! and the proof that the law has teeth (FIG-3400).
//!
//! The native host is where the law is cheapest to falsify, so this is also the
//! file that falsifies it: the same law is run once against a host whose
//! controller reports `supports_concurrent_effects() == false`, and that run
//! must fail with the message naming the leaves that never started. Without
//! that second run "the tiers agree" would be a statement the law could make
//! while asserting nothing.

use std::sync::Arc;

use crate::*;

/// A controller that is a native controller in every respect but one: it
/// refuses concurrent effects, which is the shape Restate has today.
#[derive(Default)]
struct SerialOnlyNativeController {
    inner: NativeRuntimeEffectController,
}

impl crate::AwaitEventResolver for SerialOnlyNativeController {
    fn await_event_authority_binding_id(&self) -> Option<String> {
        self.inner.await_event_authority_binding_id()
    }
}

#[async_trait::async_trait]
impl RuntimeEffectController for SerialOnlyNativeController {
    fn supports_concurrent_effects(&self) -> bool {
        false
    }

    async fn execute_effect(
        &self,
        envelope: RuntimeEffectEnvelope,
        local_executor: RuntimeEffectLocalExecutor<'_>,
    ) -> Result<RuntimeEffectOutcome, RuntimeEffectControllerError> {
        self.inner.execute_effect(envelope, local_executor).await
    }

    async fn open_effect_group(
        &self,
        group: lash_core::RuntimeEffectGroup,
    ) -> Result<lash_core::EffectGroupHandle, lash_core::RuntimeEffectControllerError> {
        self.inner.open_effect_group(group).await
    }

    async fn await_next_settlement(
        &self,
        handle: &mut lash_core::EffectGroupHandle,
        cancel: lash_core::CancellationToken,
    ) -> Result<lash_core::GroupSettlement, lash_core::RuntimeEffectControllerError> {
        self.inner.await_next_settlement(handle, cancel).await
    }
    async fn read_group_settlement(
        &self,
        group_key: &str,
        rank: u64,
    ) -> Result<
        Option<lash_core::runtime::effect::RankedGroupSettlement>,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner.read_group_settlement(group_key, rank).await
    }

    async fn close_effect_group(
        &self,
        handle: lash_core::EffectGroupHandle,
        disposition: lash_core::LoserPolicy,
    ) -> Result<(), lash_core::RuntimeEffectControllerError> {
        self.inner.close_effect_group(handle, disposition).await
    }
    async fn commit_group_child_final(
        &self,
        commit: lash_core::facade_support::effect_replay_driver::GroupChildFinalCommit,
    ) -> Result<
        lash_core::facade_support::effect_replay_driver::EffectGroupChildCommitOutcome,
        lash_core::RuntimeEffectControllerError,
    > {
        self.inner.commit_group_child_final(commit).await
    }

    async fn group_child_drain_blocked(
        &self,
        group_key: &str,
        commit_seq: u64,
    ) -> Result<bool, lash_core::RuntimeEffectControllerError> {
        self.inner
            .group_child_drain_blocked(group_key, commit_seq)
            .await
    }
}

crate::tool_batch_parallelism_tests!({
    // The deferred route parks on a completion key, and the native host refuses
    // to issue one until the embedding accepts that such a key dies with the
    // process. A single-process conformance run is exactly that embedding, and
    // saying so here is what lets the native tier answer the same law as the
    // durable tiers instead of a narrower one.
    let host: Arc<dyn crate::EffectHost> = Arc::new(
        crate::NativeEffectHost::new(
            Arc::new(NativeRuntimeEffectController::default()) as Arc<dyn RuntimeEffectController>
        )
        .allow_process_lifetime_completion_keys(),
    );
    (
        (),
        "native",
        host,
        // Every producer the native tier reaches from this crate. The RLM
        // `Promise.all` bridge and the Lashlang process bridge live above
        // lash-conformance in the dependency graph and register the same law
        // from their own crates.
        vec![crate::parallel_model_tool_calls_producer()],
    )
});

/// The law's own negative test: a serial controller fails it, and the failure
/// names the leaves that never started.
///
/// This is not a registration of a serial tier — no tier is registered as
/// expected-to-fail. It is a test *of the law*, and it is the only thing that
/// makes the three green registrations above evidence of anything.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn a_serial_controller_fails_the_law_naming_the_leaves_that_never_started() {
    let host: Arc<dyn crate::EffectHost> = Arc::new(
        crate::NativeEffectHost::new(
            Arc::new(SerialOnlyNativeController::default()) as Arc<dyn RuntimeEffectController>
        )
        .allow_process_lifetime_completion_keys(),
    );
    let failure = std::panic::AssertUnwindSafe(crate::tool_batch_cross_tier_parallelism(
        "serial-native",
        host,
        crate::parallel_model_tool_calls_producer(),
    ));
    let payload = match futures_util::FutureExt::catch_unwind(failure).await {
        Ok(()) => panic!(
            "a controller that refuses concurrent effects must fail the tool-batch \
             parallelism law"
        ),
        Err(payload) => payload,
    };
    let message = payload
        .downcast_ref::<String>()
        .cloned()
        .or_else(|| {
            payload
                .downcast_ref::<&str>()
                .map(|text| (*text).to_string())
        })
        .unwrap_or_else(|| "<non-string panic payload>".to_string());
    assert!(
        message.contains("the batch did not overlap"),
        "the serial failure must be reported as a missing overlap: {message}"
    );
    assert!(
        message.contains("Leaves that never started"),
        "the serial failure must name the leaves that never started: {message}"
    );
    assert!(
        message.contains("rv_width2_1"),
        "the named leaves must be the ones the plan issued: {message}"
    );
}
