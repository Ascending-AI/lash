//! FIG-6547: per-attempt terminals, cooperative drain, and bounded callbacks.
use super::*;
use crate::provider::{AttemptEndReason, AttemptTerminal, ProviderAttempt};

#[derive(Clone, Debug)]
struct CountedProvider {
    options: ProviderOptions,
    mode: &'static str,
    sent: u32,
    returned: Arc<Mutex<Vec<(u32, bool)>>>,
    abandoned: Arc<Mutex<Vec<(ProviderAttempt, AttemptTerminal)>>>,
    cancellation_seen: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Provider for CountedProvider {
    fn kind(&self) -> &'static str {
        "counted-terminal"
    }
    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }
    fn options(&self) -> ProviderOptions {
        self.options.clone()
    }
    fn set_options(&mut self, options: ProviderOptions) {
        self.options = options;
    }
    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    async fn send(
        &mut self,
        _body: &LiveRequestBody,
        context: ResponseContext,
    ) -> Result<LlmResponse, LlmTransportError> {
        self.sent += 1;
        let ordinal = context.scope.attempt.expect("Lash's ordinal");
        assert_eq!(ordinal, self.sent);
        if matches!(self.mode, "retry" | "retry-deadline") && self.sent == 1 {
            if self.mode == "retry-deadline" {
                tokio::time::sleep(Duration::from_millis(700)).await;
            }
            self.returned.lock_recover().push((ordinal, false));
            return Err(LlmTransportError::new("rejected before generation")
                .with_kind(ProviderFailureKind::Transport)
                .with_retry_verdict(TransportRetryVerdict::RetryableTransient));
        }
        if self.mode == "complete" || self.mode == "retry" {
            self.returned.lock_recover().push((ordinal, true));
            return Ok(LlmResponse {
                usage: LlmUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    ..Default::default()
                },
                ..bare_ok_response()
            });
        }
        if self.mode == "cooperative" {
            context.attempt_control.cancelled().await;
            self.cancellation_seen.fetch_add(1, Ordering::SeqCst);
            // The provider can finish cleanup and return during the grace.
            tokio::time::sleep(Duration::from_millis(10)).await;
            self.returned.lock_recover().push((ordinal, false));
            return Err(LlmTransportError::new("cooperatively cancelled")
                .with_terminal_reason(LlmTerminalReason::Cancelled));
        }
        context
            .attempt_control
            .capture_metadata([("header:x-cost".into(), serde_json::json!("0.01"))].into());
        if self.mode == "explicit-zero" {
            context
                .attempt_control
                .observe(&crate::llm::types::LlmStreamEvent::Evidence(
                    crate::llm::types::LlmStreamEvidence {
                        response_started: true,
                        provider_usage: Some(
                            serde_json::json!({"input_tokens":0,"output_tokens":0}),
                        ),
                        ..Default::default()
                    },
                ));
        }
        if self.mode == "panic" {
            panic!("provider panicked after receiving its receipt");
        }
        // Simulate a provider that ignores cancellation, as a host can.
        std::future::pending().await
    }
    async fn attempt_ended(&mut self, attempt: ProviderAttempt, terminal: AttemptTerminal) {
        assert_eq!(self.sent, attempt.ordinal, "same mutated instance as send");
        self.abandoned.lock_recover().push((attempt, terminal));
        if matches!(
            self.mode,
            "slow-ledger" | "custom-policy" | "clipped-policy"
        ) {
            std::future::pending::<()>().await;
        }
    }
    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[tokio::test(start_paused = true)]
async fn a_provider_gets_one_terminal_per_attempt_and_both_cancellation_waits_are_bounded() {
    for mode in [
        "complete",
        "retry",
        "cooperative",
        "uncooperative",
        "slow-ledger",
        "panic",
        "immediate",
        "tail-drop",
        "superseded",
        "retry-deadline",
        "enclosing",
        "explicit-zero",
        "custom-policy",
        "clipped-policy",
    ] {
        let returned = Arc::new(Mutex::new(Vec::new()));
        let abandoned = Arc::new(Mutex::new(Vec::new()));
        let cancellation_seen = Arc::new(AtomicUsize::new(0));
        let mut reliability = ProviderReliability::default()
            .max_attempts(Some(2))
            .base_delay_ms(0)
            .max_delay_ms(0);
        // The second retry-deadline attempt must start inside its remaining
        // 300 ms. Random retry jitter would test another schedule entirely.
        reliability.retry.jitter_ms = 0;
        let provider = CountedProvider {
            options: ProviderOptions {
                reliability,
                ..Default::default()
            },
            mode,
            sent: 0,
            returned: returned.clone(),
            abandoned: abandoned.clone(),
            cancellation_seen: cancellation_seen.clone(),
        };
        let clock = super::model_total_tests::PausedClock::new();
        let policy = match mode {
            "custom-policy" => crate::provider::ProviderTerminalPolicy {
                drain_timeout: Duration::from_millis(7),
                delivery_timeout: Duration::from_millis(11),
            },
            "clipped-policy" => crate::provider::ProviderTerminalPolicy {
                drain_timeout: Duration::from_secs(5),
                delivery_timeout: Duration::from_secs(5),
            },
            _ => crate::provider::ProviderTerminalPolicy::default(),
        };
        let mut handle = ProviderHandle::new(
            ProviderComponents::new(Box::new(provider)).with_terminal_policy(policy),
        )
        .with_clock(clock.clone());
        let mut request = empty_request();
        let observed = Arc::new(Mutex::new(Vec::new()));
        let sideband = handle
            .prepare_completion(&mut request)
            .with_attempt_observer(
                Arc::new({
                    let observed = observed.clone();
                    move |attempt, _, _| observed.lock_recover().push(attempt)
                }),
                clock,
                Arc::new(lash_trace::LashIdentity),
            );
        let template = Arc::new(handle.lower(&request).await.expect("lowers"));
        let budgets = lash_protocol_types::ExecutionBudgets::new(
            lash_protocol_types::ExecutionBudgetsConfig {
                model_total: Duration::from_secs(1),
                stop_grace: Duration::from_millis(500),
                provider: lash_protocol_types::ProviderAttemptLimits::new(
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    Duration::from_secs(1),
                    2,
                )
                .expect("provider limits"),
                ..lash_protocol_types::ExecutionBudgetsConfig::recommended()
            },
        )
        .expect("budgets");
        let start = tokio::time::Instant::now();
        let reason = match mode {
            "immediate" => Some(AttemptEndReason::ImmediateCancel),
            "tail-drop" => Some(AttemptEndReason::TailDrainDrop),
            "superseded" => Some(AttemptEndReason::SupersededRetry),
            _ => None,
        };
        let stopper = sideband.clone();
        let metrics = lash_trace::telemetry::metrics::TelemetryMetrics::default();
        let completion = handle.complete_prepared(
            ResponseContext::of_request(&request),
            &template,
            &NoSlotDeliveries,
            sideband,
            crate::ChargeSafetyPolicy::default(),
            &metrics,
            None,
            ModelCallBounds {
                budgets,
                enclosing: (mode == "enclosing").then(|| {
                    lash_protocol_types::ExecutionLimit::starting_at(
                        1_000_000,
                        Duration::from_millis(500),
                        Duration::from_millis(500),
                    )
                }),
            },
        );
        let cancellation = async {
            if let Some(reason) = reason {
                tokio::time::sleep(Duration::from_millis(10)).await;
                stopper.cancel(reason);
            } else {
                std::future::pending::<()>().await;
            }
        };
        futures_util::pin_mut!(completion, cancellation);
        let result = match futures_util::future::select(completion.as_mut(), cancellation).await {
            futures_util::future::Either::Left((result, _)) => result,
            futures_util::future::Either::Right(((), _)) => completion.await,
        };
        let observations = observed.lock_recover();
        let delivery_timed_out = matches!(mode, "slow-ledger" | "custom-policy" | "clipped-policy");
        assert_eq!(
            observations
                .last()
                .expect("sealed attempt")
                .span
                .as_ref()
                .expect("attempt span")
                .terminal_delivery_timed_out,
            delivery_timed_out,
            "{mode}"
        );
        if mode == "custom-policy" {
            assert!(
                start.elapsed() < Duration::from_millis(1100),
                "both custom intervals govern cleanup"
            );
        }
        let returned = returned.lock_recover();
        let abandoned = abandoned.lock_recover();
        match mode {
            "complete" | "retry" => {
                let completion = result.expect("completed");
                assert_eq!(completion.response.usage.input_tokens, 7);
                assert_eq!(completion.response.usage.output_tokens, 3);
                assert_eq!(
                    completion.call_record.attempts.len(),
                    if mode == "retry" { 2 } else { 1 }
                );
                assert_eq!(
                    *returned,
                    if mode == "retry" {
                        vec![(1, false), (2, true)]
                    } else {
                        vec![(1, true)]
                    }
                );
                assert!(abandoned.is_empty());
            }
            "cooperative" => {
                assert!(result.is_err());
                assert_eq!(cancellation_seen.load(Ordering::SeqCst), 1);
                assert_eq!(*returned, vec![(1, false)]);
                assert!(abandoned.is_empty(), "a returned Err has no callback");
            }
            _ => {
                assert!(result.is_err());
                assert_eq!(
                    *returned,
                    if mode == "retry-deadline" {
                        vec![(1, false)]
                    } else {
                        vec![]
                    }
                );
                assert_eq!(abandoned.len(), 1);
                let (
                    attempt,
                    AttemptTerminal::Abandoned {
                        reason: actual_reason,
                        drain_timed_out,
                        partial_response,
                    },
                ) = &abandoned[0];
                assert_eq!(
                    attempt.ordinal,
                    if mode == "retry-deadline" { 2 } else { 1 }
                );
                assert_eq!(
                    *actual_reason,
                    if mode == "panic" {
                        AttemptEndReason::ProviderPanicked
                    } else {
                        reason.unwrap_or(AttemptEndReason::ModelDeadline)
                    }
                );
                assert_eq!(*drain_timed_out, mode != "panic", "{mode}");
                let partial = partial_response.as_ref().expect("captured header");
                assert_eq!(partial.response_metadata["header:x-cost"], "0.01");
                assert_eq!(
                    partial.usage,
                    if mode == "explicit-zero" {
                        LlmUsage::default()
                    } else {
                        LlmUsage::unknown_call()
                    },
                    "only reported usage, including explicit zero, is known"
                );
            }
        }
        assert!(
            start.elapsed() <= Duration::from_millis(if mode == "enclosing" { 1000 } else { 1500 }),
            "{mode}: cancellation drain plus callback stay within 500 ms"
        );
    }
}

// FIG-6698: canonical forwarding retains the caller's live receipt channel.
#[derive(Clone, Debug)]
struct CanonicalLeaf {
    hops: u8,
}

#[async_trait::async_trait]
impl Provider for CanonicalLeaf {
    fn kind(&self) -> &'static str {
        "canonical-leaf"
    }
    fn route_identity(&self, model: &str) -> ProviderRouteIdentity {
        ProviderRouteIdentity::new(self.kind(), self.kind(), model)
    }
    fn options(&self) -> ProviderOptions {
        ProviderOptions::default()
    }
    fn set_options(&mut self, _: ProviderOptions) {}
    fn serialize_config(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    async fn send(
        &mut self,
        body: &LiveRequestBody,
        context: ResponseContext,
    ) -> Result<LlmResponse, LlmTransportError> {
        if self.hops == 0 {
            context
                .attempt_control
                .capture_metadata([("header:x-cost".into(), serde_json::json!("0.01"))].into());
            return Ok(bare_ok_response());
        }
        let mut inner = Self {
            hops: self.hops - 1,
        };
        if self.hops == 2 {
            let (request, deliveries) = crate::provider::canonical_call(
                body,
                context,
                &lash_protocol_types::llm::attachment_delivery::DeliveryContext {
                    valid_through_ms: 0,
                    live_file_scope: None,
                },
                crate::provider::AttachmentReadPolicy::STANDARD,
            )?;
            inner
                .complete(
                    request,
                    &deliveries,
                    &crate::provider::LiveCallHorizon::fixture(),
                )
                .await
        } else {
            let request = crate::provider::canonical_request(body, context)?;
            inner
                .complete(
                    request,
                    &NoSlotDeliveries,
                    &crate::provider::LiveCallHorizon::fixture(),
                )
                .await
        }
    }
    fn clone_boxed(&self) -> Box<dyn Provider> {
        Box::new(self.clone())
    }
}

#[tokio::test]
async fn canonical_forwarding_retains_the_callers_header_capture_control() {
    let request = empty_request();
    let context = ResponseContext::of_request(&request);
    let caller_control = context.attempt_control.clone();
    let mut provider = CanonicalLeaf { hops: 2 };
    let template = Arc::new(
        provider
            .lower(&request, &SegmentCache::disabled())
            .await
            .unwrap(),
    );
    let body = LiveRequestBody::fill(template, Default::default()).unwrap();
    provider
        .send(&body, context)
        .await
        .expect("canonical leaf returns");
    let partial = caller_control
        .partial_response()
        .expect("leaf headers reach the caller's control");
    assert_eq!(partial.response_metadata["header:x-cost"], "0.01");
}
