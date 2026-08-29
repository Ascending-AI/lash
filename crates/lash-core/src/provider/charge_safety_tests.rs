use super::support::*;
use super::tests::{RecordingClock, empty_request, paid_partial_handle};
use crate::llm::types::LlmUsage;
use std::sync::atomic::{AtomicUsize, Ordering};

#[tokio::test]
async fn authorizes_bounded_duplicate_billing_and_projects_typed_trace() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut handle = paid_partial_handle(Arc::clone(&attempts), 1, 2, None);

    let completion = handle
        .complete_with_charge_safety(
            empty_request(),
            crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: 1,
                max_duplicate_cost_tokens: Some(10),
            },
        )
        .await
        .expect("bounded duplicate billing authorizes one retry");

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(
        completion.call_record.attempts[0]
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.charge_safety.as_ref()),
        Some(&crate::ChargeSafetyDecision::Authorized {
            tokens_at_stake: 10,
            attempt_number: 1,
        })
    );
    let trace =
        crate::trace::trace_llm_attempts(Some(&completion.call_record)).expect("typed retry trace");
    assert_eq!(
        trace[0].charge_safety,
        Some(lash_trace::TraceChargeSafetyDecision::Authorized {
            tokens_at_stake: 10,
            attempt_number: 1,
        })
    );
}

#[tokio::test]
async fn duplicate_cost_bound_denies_and_projects_typed_trace() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut handle = paid_partial_handle(Arc::clone(&attempts), 1, 2, None);

    let failure = handle
        .complete_with_charge_safety(
            empty_request(),
            crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: 1,
                max_duplicate_cost_tokens: Some(9),
            },
        )
        .await
        .expect_err("ten billed tokens exceed a nine-token duplicate bound");

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(
        failure.call_record.attempts[0]
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.charge_safety.as_ref()),
        Some(&crate::ChargeSafetyDecision::Denied {
            tokens_at_stake: 10,
            attempt_number: 1,
            reason: crate::ChargeSafetyDenialReason::DuplicateCostLimitExceeded,
        })
    );
    let trace =
        crate::trace::trace_llm_attempts(Some(&failure.call_record)).expect("typed retry trace");
    assert_eq!(
        trace[0].charge_safety,
        Some(lash_trace::TraceChargeSafetyDecision::Denied {
            tokens_at_stake: 10,
            attempt_number: 1,
            reason: lash_trace::TraceChargeSafetyDenialReason::DuplicateCostLimitExceeded,
        })
    );
}

#[tokio::test]
async fn configured_unsafe_retry_limit_is_hard_clamped_to_five() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let mut handle = paid_partial_handle(Arc::clone(&attempts), 100, 10, None);

    let failure = handle
        .complete_with_charge_safety(
            empty_request(),
            crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: 200,
                max_duplicate_cost_tokens: None,
            },
        )
        .await
        .expect_err("the sixth unsafe retry must be denied by the hard clamp");

    assert_eq!(attempts.load(Ordering::SeqCst), 6);
    assert_eq!(
        failure.call_record.attempts[5]
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.charge_safety.as_ref()),
        Some(&crate::ChargeSafetyDecision::Denied {
            tokens_at_stake: 10,
            attempt_number: 6,
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
        .complete_with_charge_safety(
            empty_request(),
            crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: 1,
                max_duplicate_cost_tokens: None,
            },
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
        .complete_with_charge_safety(
            empty_request(),
            crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
                max_unsafe_retries: 1,
                max_duplicate_cost_tokens: None,
            },
        )
        .await
        .expect_err("unsafe retry delay beyond the cap fails immediately");
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(clock.slept(), Duration::ZERO);
    assert_eq!(
        failure.call_record.attempts[0]
            .retry_decision
            .as_ref()
            .and_then(|decision| decision.charge_safety.as_ref()),
        Some(&crate::ChargeSafetyDecision::Denied {
            tokens_at_stake: 10,
            attempt_number: 1,
            reason: crate::ChargeSafetyDenialReason::RetryAfterExceedsCap,
        })
    );
}

#[test]
fn policy_surface_is_decision_congruent() {
    let usage = LlmUsage {
        input_tokens: 30,
        output_tokens: 20,
        ..LlmUsage::default()
    };
    let decide = |policy: &crate::ChargeSafetyPolicy| {
        charge_safety_decision(
            TransportRetryVerdict::RetryableTransient,
            GenerationRetryGuarantee::None,
            policy,
            Some(&usage),
            2,
        )
    };
    let baseline = decide(&crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
        max_unsafe_retries: 2,
        max_duplicate_cost_tokens: Some(50),
    });
    assert_eq!(
        baseline,
        ChargeSafetyEvaluation::Evaluated(crate::ChargeSafetyDecision::Authorized {
            tokens_at_stake: 50,
            attempt_number: 2,
        })
    );
    assert_eq!(
        decide(&crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries: 1,
            max_duplicate_cost_tokens: Some(50),
        }),
        ChargeSafetyEvaluation::Evaluated(crate::ChargeSafetyDecision::Denied {
            tokens_at_stake: 50,
            attempt_number: 2,
            reason: crate::ChargeSafetyDenialReason::UnsafeRetryLimitExceeded,
        }),
        "changing max_unsafe_retries must change the decision",
    );
    assert_eq!(
        decide(&crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
            max_unsafe_retries: 2,
            max_duplicate_cost_tokens: Some(49),
        }),
        ChargeSafetyEvaluation::Evaluated(crate::ChargeSafetyDecision::Denied {
            tokens_at_stake: 50,
            attempt_number: 2,
            reason: crate::ChargeSafetyDenialReason::DuplicateCostLimitExceeded,
        }),
        "changing max_duplicate_cost_tokens must change the decision",
    );
}

#[test]
fn precedence_is_structural() {
    let appetite = crate::ChargeSafetyPolicy::AcceptDuplicateBilling {
        max_unsafe_retries: 2,
        max_duplicate_cost_tokens: Some(100),
    };
    let usage = LlmUsage {
        input_tokens: 30,
        output_tokens: 20,
        ..LlmUsage::default()
    };

    assert_eq!(
        charge_safety_decision(
            TransportRetryVerdict::Forbidden,
            GenerationRetryGuarantee::None,
            &appetite,
            Some(&usage),
            1,
        ),
        ChargeSafetyEvaluation::NotEvaluated(ChargeSafetyPrecedence::Forbidden),
        "Forbidden must win over the host waiver",
    );
    assert_eq!(
        charge_safety_decision(
            TransportRetryVerdict::NotRetryable,
            GenerationRetryGuarantee::None,
            &appetite,
            Some(&usage),
            1,
        ),
        ChargeSafetyEvaluation::NotEvaluated(ChargeSafetyPrecedence::ServerPushback),
        "server don't-retry pushback must win over the host waiver",
    );
    assert_eq!(
        charge_safety_decision(
            TransportRetryVerdict::RetryableTransient,
            GenerationRetryGuarantee::Resumable,
            &appetite,
            Some(&usage),
            1,
        ),
        ChargeSafetyEvaluation::NotEvaluated(ChargeSafetyPrecedence::ProviderGuarantee),
        "a provider guarantee must stay on the normal safe path",
    );
    assert_eq!(
        charge_safety_decision(
            TransportRetryVerdict::RetryableTransient,
            GenerationRetryGuarantee::None,
            &appetite,
            Some(&usage),
            1,
        ),
        ChargeSafetyEvaluation::Evaluated(crate::ChargeSafetyDecision::Authorized {
            tokens_at_stake: 50,
            attempt_number: 1,
        }),
        "the appetite may authorize only after higher-precedence facts permit evaluation",
    );
}
