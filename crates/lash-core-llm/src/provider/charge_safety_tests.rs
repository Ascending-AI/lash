use super::support::*;
use super::tests::{RecordingClock, empty_request, paid_partial_handle};
use crate::llm::types::LlmUsage;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn authorizes_bounded_duplicate_billing_and_projects_typed_trace() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut handle = paid_partial_handle(Arc::clone(&attempts), 1, 2, None);

    let completion = handle
        .complete(
            empty_request(),
            crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: 1,
                max_duplicate_cost_tokens: Some(10),
            },
            lash_sansio::ExecutionBudgets::recommended(),
            &crate::provider::NoSlotDeliveries,
        )
        .await
        .expect("bounded duplicate billing authorizes one retry");

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        completion.call_record.attempts[0]
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.charge_safety()),
        Some(crate::ChargeSafetyDecision::Authorized {
            tokens_at_stake: 10,
            attempt_number: 1,
        })
    );
    let trace =
        crate::trace::trace_llm_attempts(Some(&completion.call_record)).expect("typed retry trace");
    assert_eq!(trace, completion.call_record.attempts);
}

#[tokio::test]
async fn duplicate_cost_bound_denies_and_projects_typed_trace() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut handle = paid_partial_handle(Arc::clone(&attempts), 1, 2, None);

    let failure = handle
        .complete(
            empty_request(),
            crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: 1,
                max_duplicate_cost_tokens: Some(9),
            },
            lash_sansio::ExecutionBudgets::recommended(),
            &crate::provider::NoSlotDeliveries,
        )
        .await
        .expect_err("ten billed tokens exceed a nine-token duplicate bound");

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        failure.call_record.attempts[0]
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.charge_safety()),
        Some(crate::ChargeSafetyDecision::Denied {
            tokens_at_stake: 10,
            attempt_number: 1,
            reason: crate::ChargeSafetyDenialReason::DuplicateCostLimitExceeded,
        })
    );
    let trace =
        crate::trace::trace_llm_attempts(Some(&failure.call_record)).expect("typed retry trace");
    assert_eq!(trace, failure.call_record.attempts);
}

#[tokio::test]
async fn provider_handle_enforces_the_supplied_retry_limit_without_a_second_ceiling() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut handle = paid_partial_handle(Arc::clone(&attempts), 100, 10, None);
    // The deployment's attempt limit (FIG-5171) is stated above the policy
    // under test, so the supplied charge-safety limit is the one that binds.
    let budgets = lash_sansio::ExecutionBudgets::new(lash_sansio::ExecutionBudgetsConfig {
        provider: lash_sansio::ProviderAttemptLimits::new(
            Duration::from_secs(300),
            Duration::from_secs(120),
            Duration::from_secs(120),
            lash_sansio::MAX_PROVIDER_ATTEMPTS,
        )
        .expect("valid provider limits"),
        ..lash_sansio::ExecutionBudgetsConfig::recommended()
    })
    .expect("valid budgets");
    let mut request = empty_request();
    let sideband = handle.prepare_completion(&mut request);
    let body = handle.lower(&request).await.expect("the request lowers");

    let failure = handle
        .complete_prepared(
            ResponseContext::of_request(&request),
            &Arc::new(body),
            &crate::provider::NoSlotDeliveries,
            sideband,
            crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: 6,
                max_duplicate_cost_tokens: None,
            },
            &lash_trace::telemetry::metrics::TelemetryMetrics::default(),
            None,
            super::handle::ModelCallBounds {
                budgets,
                enclosing: None,
            },
        )
        .await
        .expect_err("the seventh unsafe retry exceeds the supplied policy");

    assert_eq!(attempts.load(Ordering::SeqCst), 7);
    assert_eq!(
        failure.call_record.attempts[6]
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.charge_safety()),
        Some(crate::ChargeSafetyDecision::Denied {
            tokens_at_stake: 10,
            attempt_number: 7,
            reason: crate::ChargeSafetyDenialReason::UnsafeRetryLimitExceeded,
        })
    );
}

#[tokio::test]
async fn unsafe_retry_honors_retry_after_and_excessive_delay_fails_fast() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(RecordingClock::default());
    let mut handle = paid_partial_handle(Arc::clone(&attempts), 1, 1, Some(Duration::from_secs(2)))
        .with_clock(Arc::clone(&clock) as _);
    handle
        .complete(
            empty_request(),
            crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: 1,
                max_duplicate_cost_tokens: None,
            },
            lash_sansio::ExecutionBudgets::recommended(),
            &crate::provider::NoSlotDeliveries,
        )
        .await
        .expect("unsafe retry honors bounded server delay");
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(clock.slept(), Duration::from_secs(2));

    let attempts = Arc::new(AtomicUsize::new(0));
    let clock = Arc::new(RecordingClock::default());
    let mut handle =
        paid_partial_handle(Arc::clone(&attempts), 1, 2, Some(Duration::from_secs(61)))
            .with_clock(Arc::clone(&clock) as _);
    let failure = handle
        .complete(
            empty_request(),
            crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: 1,
                max_duplicate_cost_tokens: None,
            },
            lash_sansio::ExecutionBudgets::recommended(),
            &crate::provider::NoSlotDeliveries,
        )
        .await
        .expect_err("unsafe retry delay beyond the cap fails immediately");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(clock.slept(), Duration::ZERO);
    assert_eq!(
        failure.call_record.attempts[0]
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.decline_cause()),
        Some(lash_sansio::llm::types::RetryDeclineCause::RetryAfterExceedsCap)
    );
}

/// A failure that already produced billed output, so no protocol position
/// proves a retry free.
fn paid_failure(verdict: TransportRetryVerdict, usage: LlmUsage) -> LlmTransportError {
    LlmTransportError::new("stream disconnected")
        .with_retry_verdict(verdict)
        .with_partial_response(LlmResponse {
            usage,
            ..LlmResponse::default()
        })
}

fn ground(
    verdict: TransportRetryVerdict,
    guarantee: GenerationRetryGuarantee,
    usage: LlmUsage,
) -> Option<RetryGround> {
    let failure = paid_failure(verdict, usage);
    RetryGround::of(&failure, failure_protocol_position(&failure), guarantee)
}

fn unguaranteed(usage: LlmUsage) -> UnguaranteedRetry {
    match ground(
        TransportRetryVerdict::RetryableTransient,
        GenerationRetryGuarantee::None,
        usage,
    ) {
        Some(RetryGround::Unguaranteed(retry)) => retry,
        other => panic!("paid output without a guarantee is unguaranteed, got {other:?}"),
    }
}

#[test]
fn precedence_is_structural() {
    let usage = LlmUsage {
        input_tokens: 30,
        output_tokens: 20,
        ..LlmUsage::default()
    };

    for verdict in [
        TransportRetryVerdict::Forbidden,
        TransportRetryVerdict::NotRetryable,
    ] {
        for guarantee in [
            GenerationRetryGuarantee::None,
            GenerationRetryGuarantee::Idempotent,
            GenerationRetryGuarantee::Resumable,
        ] {
            assert_eq!(
                ground(verdict, guarantee, usage.clone()),
                None,
                "{verdict:?} leaves nothing for a guarantee or the host waiver to authorize",
            );
        }
    }
    assert_eq!(
        ground(
            TransportRetryVerdict::RetryableTransient,
            GenerationRetryGuarantee::Resumable,
            usage.clone(),
        ),
        Some(RetryGround::Automatic(RetryClass::ProviderResume)),
        "a provider guarantee must stay on the normal safe path",
    );

    let retry = unguaranteed(usage);
    let denied = |attempt_number, reason| crate::ChargeSafetyDecision::Denied {
        tokens_at_stake: 50,
        attempt_number,
        reason,
    };
    let appetite = |max_duplicate_cost_tokens| crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
        max_unsafe_retries: 2,
        max_duplicate_cost_tokens,
    };
    assert_eq!(
        retry.decision(&crate::ChargeSafetyPolicy::RequireGuarantee, 1),
        denied(1, crate::ChargeSafetyDenialReason::GuaranteeRequired),
    );
    assert_eq!(
        retry.decision(&appetite(Some(100)), 3),
        denied(3, crate::ChargeSafetyDenialReason::UnsafeRetryLimitExceeded),
    );
    assert_eq!(
        retry.decision(&appetite(Some(49)), 1),
        denied(
            1,
            crate::ChargeSafetyDenialReason::DuplicateCostLimitExceeded
        ),
    );
    assert_eq!(
        retry.decision(&appetite(Some(100)), 1),
        crate::ChargeSafetyDecision::Authorized {
            tokens_at_stake: 50,
            attempt_number: 1,
        },
        "the appetite may authorize only an unguaranteed retry within its bounds",
    );
}

/// FIG-5582: a one-shot call uses the caller's configured billing bound.
#[tokio::test]
async fn one_shot_completion_applies_configured_charge_safety_policy() {
    for bound in [9, 10] {
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut handle = paid_partial_handle(Arc::clone(&attempts), 1, 2, None)
            .with_clock(Arc::new(RecordingClock::default()));
        let configured = crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries: 1,
            max_duplicate_cost_tokens: Some(bound),
        };
        let result = handle
            .complete(
                empty_request(),
                configured,
                lash_sansio::ExecutionBudgets::recommended(),
                &crate::provider::NoSlotDeliveries,
            )
            .await;
        if bound == 9 {
            let failure = result.expect_err("the configured cost bound refuses the retry");
            assert_eq!(attempts.load(Ordering::SeqCst), 1);
            assert_eq!(
                failure.call_record.attempts[0]
                    .retry_decision
                    .as_ref()
                    .and_then(|decision| decision.charge_safety()),
                Some(crate::ChargeSafetyDecision::Denied {
                    tokens_at_stake: 10,
                    attempt_number: 1,
                    reason: crate::ChargeSafetyDenialReason::DuplicateCostLimitExceeded,
                }),
            );
        } else {
            let completion = result.expect("the configured cost bound permits one retry");
            assert_eq!(attempts.load(Ordering::SeqCst), 2);
            assert_eq!(
                completion.call_record.attempts[0]
                    .retry_decision
                    .as_ref()
                    .and_then(|decision| decision.charge_safety()),
                Some(crate::ChargeSafetyDecision::Authorized {
                    tokens_at_stake: 10,
                    attempt_number: 1,
                }),
            );
        }
    }
}
