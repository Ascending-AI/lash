//! FIG-4236: model usage is engine-owned accounting delivered per call
//! (ADR 0125), stated as laws a tier's engine must keep.
//!
//! Every law drives real turns through the tier's
//! [`ConformanceTurnRunner`](crate::ConformanceTurnRunner): a fresh runtime
//! over the tier's host and stores per execution, and a scripted model that
//! counts every provider invocation and reports fixed usage per attempt. The
//! laws then read the session owner's accounting straight from storage, with
//! no runtime open: what a second host would read.
//!
//! The model's script is replay-safe: a request is answered by how many
//! assistant messages its turn already holds, so a redrive that asks again is
//! answered as the first execution was. A kill fires from inside the model
//! or the probe tool at a point the law names, and the runner kills the
//! execution where it stands.
//!
//! What every law asserts, besides its own contract:
//!
//! - each provider attempt that returned is exactly one fact, however the
//!   turn's executions were cut and replayed;
//! - a dispatch the engine journaled no record of is an explicit `unknown`
//!   run, never an invented fact;
//! - the provider invocation count after journaling equals the count before:
//!   a replay asks the provider nothing it already answered.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use lash_core::testing::TestTurnDrive as _;
use lash_sansio::sync::MutexExt as _;
use lash_sansio::{SessionId, TurnId};

/// What a registering tier hands every usage-accounting engine law.
#[derive(Clone)]
pub struct UsageAccountingTier {
    /// Distinguishes this tier's sessions from every other tier's, and every
    /// run's from the last on a tier whose state outlives a run.
    pub prefix: String,
    /// The engine's host: the one whose controller journals each spending
    /// effect's usage and delivers its settlement.
    pub effect_host: Arc<dyn crate::EffectHost>,
    /// The engine's stores: the accounting the laws read back.
    pub stores: Arc<dyn crate::StoreSet>,
    /// Runs each turn, kills it and redrives it.
    pub runner: Arc<dyn crate::ConformanceTurnRunner>,
    /// The tier's continuation faults, where its engine can inject them.
    pub continuation: Arc<dyn UsageContinuationFaults>,
}

/// Faults a tier injects into its accounting continuation.
pub trait UsageContinuationFaults: Send + Sync {
    /// Kill the continuation's next settlement once, after its projection
    /// committed and before the handler completed, so the engine retries it.
    /// Answers how many such kills fired so far, or `None` when this tier's
    /// engine cannot inject one.
    fn kill_next_settlement_after_projection(&self) -> Option<Arc<dyn Fn() -> u64 + Send + Sync>>;
}

/// A tier whose continuation takes no injected faults.
pub struct NoContinuationFaults;

impl UsageContinuationFaults for NoContinuationFaults {
    fn kill_next_settlement_after_projection(&self) -> Option<Arc<dyn Fn() -> u64 + Send + Sync>> {
        None
    }
}

/// How long a turn gets before the law fails rather than hangs.
const TURN_BUDGET: Duration = Duration::from_secs(90);

/// How long a law waits for the engine to deliver a settlement.
const DELIVERY: Duration = Duration::from_secs(30);

/// The probe tool every non-final model call asks for.
const PROBE: &str = "usage_probe";

/// The usage the provider reports for a successful attempt of call `call`.
fn call_usage(call: usize) -> lash_core::llm::types::LlmUsage {
    let call = i64::try_from(call).unwrap_or(0);
    lash_core::llm::types::LlmUsage {
        input_tokens: 100 + call,
        output_tokens: 10 + call,
        cache_read_input_tokens: call,
        cache_write_input_tokens: 0,
        reasoning_output_tokens: 1,
    }
}

/// The partial usage a billed failed attempt reports before it fails.
fn failed_usage() -> lash_core::llm::types::LlmUsage {
    lash_core::llm::types::LlmUsage {
        input_tokens: 7,
        output_tokens: 3,
        ..lash_core::llm::types::LlmUsage::default()
    }
}

fn token_usage(usage: &lash_core::llm::types::LlmUsage) -> crate::TokenUsage {
    crate::TokenUsage {
        input_tokens: usage.input_tokens,
        output_tokens: usage.output_tokens,
        cache_read_input_tokens: usage.cache_read_input_tokens,
        cache_write_input_tokens: usage.cache_write_input_tokens,
        reasoning_output_tokens: usage.reasoning_output_tokens,
    }
}

/// Where a law kills a turn's first execution.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kill {
    /// No kill.
    None,
    /// At call `n`'s provider entry, before the provider did anything: the
    /// run was admitted, nothing was sent (crash table `p1`).
    AtDispatch(usize),
    /// Inside call `n`'s provider, after it counted the invocation and
    /// before it answered: billed, never journaled (crash table `p2`).
    InFlight(usize),
    /// Inside the probe tool call `n` asked for, after call `n`'s entry was
    /// journaled.
    InTool(usize),
}

/// How the turn's last model call ends.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Ending {
    /// The model answers; the turn completes.
    Completed,
    /// The last call fails terminally after reporting partial usage; the
    /// turn is committed failed.
    Failed,
    /// The probe tool of call `n` cancels the turn; it is committed
    /// cancelled.
    CancelledAfter(usize),
}

/// One law's scripted turn.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Script {
    /// Model calls in the turn: `calls - 1` ask for the probe, the last
    /// answers.
    pub(crate) calls: usize,
    /// The call whose first attempt is billed and fails retryably before
    /// its retry succeeds.
    pub(crate) billed_failure: Option<usize>,
    pub(crate) kill: Kill,
    pub(crate) ending: Ending,
}

impl Script {
    pub(crate) fn completed(calls: usize) -> Self {
        Self {
            calls,
            billed_failure: None,
            kill: Kill::None,
            ending: Ending::Completed,
        }
    }
}

/// Everything the model and the probe did, across every execution.
#[derive(Default)]
struct Witness {
    /// Provider invocations that reached the provider's send.
    invocations: AtomicUsize,
    /// Calls whose billed first attempt already failed.
    failed_once: std::sync::Mutex<BTreeSet<usize>>,
    /// The usage of every provider attempt that returned, in order.
    returned: std::sync::Mutex<Vec<crate::TokenUsage>>,
}

/// One law's world: a session on the tier, its script and the witness.
#[derive(Clone)]
pub(crate) struct World {
    tier: UsageAccountingTier,
    pub(crate) session_id: SessionId,
    script: Script,
    witness: Arc<Witness>,
    kill: crate::ConformanceCrash,
    cancel: tokio_util::sync::CancellationToken,
    /// Set when an execution after the kill starts: the recovery.
    recovering: tokio_util::sync::CancellationToken,
    deletion_child: Option<Arc<deletion::Child>>,
    children: Option<Arc<children::Children>>,
    supersede: bool,
}

impl World {
    pub(crate) fn new(tier: &UsageAccountingTier, law: &str, script: Script) -> Self {
        Self {
            tier: tier.clone(),
            session_id: SessionId::from(format!("{}-{law}", tier.prefix)),
            script,
            witness: Arc::new(Witness::default()),
            kill: crate::ConformanceCrash::new(),
            cancel: tokio_util::sync::CancellationToken::new(),
            recovering: tokio_util::sync::CancellationToken::new(),
            deletion_child: None,
            children: None,
            supersede: false,
        }
    }

    fn owner(&self) -> crate::RuntimeOwner {
        crate::RuntimeOwner::Session(self.session_id.clone())
    }

    fn turn_id(&self) -> TurnId {
        TurnId::from(format!("{}-turn", self.session_id))
    }

    fn admitted(&self) -> crate::AdmittedScope {
        crate::admit(crate::ExecutionScope::turn(
            &self.session_id,
            self.turn_id(),
        ))
    }

    pub(crate) fn invocations(&self) -> usize {
        self.witness.invocations.load(Ordering::SeqCst)
    }

    /// The total the provider reported over every attempt that returned.
    pub(crate) fn returned_total(&self) -> crate::TokenUsage {
        self.witness
            .returned
            .lock_recover()
            .iter()
            .fold(crate::TokenUsage::default(), |total, usage| {
                total.saturating_add(usage).0
            })
    }

    pub(crate) fn returned_attempts(&self) -> usize {
        self.witness.returned.lock_recover().len()
    }

    /// The one model: call `n` is the turn's `n`-th request, found by how
    /// many assistant messages the turn already holds.
    fn model(&self) -> crate::testing::TestProvider {
        let world = self.clone();
        crate::testing::TestProvider::builder()
            .kind("stub")
            .requires_streaming(true)
            .generation_retry_guarantee(lash_core::provider::GenerationRetryGuarantee::Idempotent)
            .options(crate::facade_support::ProviderOptions {
                reliability: lash_core::provider::ProviderReliability::default()
                    .max_attempts(2)
                    .base_delay_ms(0)
                    .max_delay_ms(0),
                ..crate::facade_support::ProviderOptions::default()
            })
            .complete(move |request| {
                let world = world.clone();
                async move { world.answer(request).await }
            })
            .build()
    }

    #[expect(clippy::expect_used, reason = "scripted provider fixture markers")]
    async fn answer(
        &self,
        request: crate::LlmRequest,
    ) -> Result<crate::LlmResponse, crate::facade_support::LlmTransportError> {
        if let Some(child) = request.extra_body.get("usage_child") {
            let index = usize::try_from(child["index"].as_u64().expect("child index"))
                .expect("child index fits");
            self.witness.invocations.fetch_add(1, Ordering::SeqCst);
            if child["abort"].as_bool().expect("abort marker") {
                return Ok(crate::LlmResponse {
                    parts: vec![crate::LlmOutputPart::Text {
                        text: "partial output before abort".into(),
                        response_meta: None,
                    }],
                    terminal_reason: crate::LlmTerminalReason::Cancelled,
                    ..Default::default()
                });
            }
            let failing = self.witness.failed_once.lock_recover().insert(100 + index);
            let usage = if failing {
                failed_usage()
            } else {
                call_usage(index)
            };
            self.witness
                .returned
                .lock_recover()
                .push(token_usage(&usage));
            if failing {
                return Err(
                    crate::facade_support::LlmTransportError::new("billed child failure")
                        .with_kind(crate::ProviderFailureKind::Stream)
                        .with_retry_verdict(
                            lash_core::llm::transport::TransportRetryVerdict::RetryableTransient,
                        )
                        .with_partial_response(crate::LlmResponse {
                            usage,
                            ..Default::default()
                        }),
                );
            }
            return Ok(crate::LlmResponse {
                parts: vec![crate::LlmOutputPart::Text {
                    text: "paid child success".into(),
                    response_meta: None,
                }],
                terminal_reason: crate::LlmTerminalReason::Stop,
                provider_usage: Some(serde_json::json!({"child": index})),
                usage,
                ..Default::default()
            });
        }
        if let Some(child) = &self.deletion_child
            && request.extra_body.contains_key("usage_delete_child")
        {
            self.witness.invocations.fetch_add(1, Ordering::SeqCst);
            if child.dispatched.fetch_add(1, Ordering::SeqCst) != 0 {
                return Err(crate::facade_support::LlmTransportError::new(
                    "a drained child reached the provider",
                )
                .with_retry_verdict(lash_core::llm::transport::TransportRetryVerdict::NotRetryable)
                .with_partial_response(crate::LlmResponse {
                    usage: failed_usage(),
                    ..Default::default()
                }));
            }
            self.kill.fire();
            child.release.cancelled().await;
            let usage = call_usage(0);
            self.witness
                .returned
                .lock_recover()
                .push(token_usage(&usage));
            return Ok(crate::LlmResponse {
                parts: vec![crate::LlmOutputPart::Text {
                    text: "child paid".into(),
                    response_meta: None,
                }],
                terminal_reason: crate::LlmTerminalReason::Stop,
                provider_usage: Some(serde_json::json!({"paid": true})),
                usage,
                ..Default::default()
            });
        }
        let call = call_of(&request);
        let stream = request.stream_events.clone();
        if self.script.kill == Kill::AtDispatch(call) && !self.kill.has_fired() {
            self.kill.fire();
            return std::future::pending().await;
        }
        self.witness.invocations.fetch_add(1, Ordering::SeqCst);
        if self.script.kill == Kill::InFlight(call) && !self.kill.has_fired() {
            self.kill.fire();
            return std::future::pending().await;
        }
        let failing = self.script.billed_failure == Some(call)
            && self.witness.failed_once.lock_recover().insert(call);
        let last = call + 1 == self.script.calls;
        if last && self.supersede {
            let store =
                crate::conformance::law_session_store(self.tier.stores.as_ref(), &self.session_id)
                    .await;
            let recording = crate::testing::runtime_helpers::RecordingStore::over_session(
                store,
                self.session_id.clone(),
            );
            crate::testing::runtime_helpers::advance_session_head(&recording, |state| {
                state.policy = self.policy();
            })
            .await;
        }
        if failing || (last && self.script.ending == Ending::Failed) {
            let usage = failed_usage();
            if let Some(stream) = &stream {
                stream.send(lash_core::llm::types::LlmStreamEvent::Usage(usage.clone()));
            }
            self.witness
                .returned
                .lock_recover()
                .push(token_usage(&usage));
            let verdict = if failing {
                lash_core::llm::transport::TransportRetryVerdict::RetryableTransient
            } else {
                lash_core::llm::transport::TransportRetryVerdict::NotRetryable
            };
            return Err(crate::facade_support::LlmTransportError::new(
                "usage law provider failure",
            )
            .with_kind(lash_core::ProviderFailureKind::Stream)
            .with_retry_verdict(verdict)
            .with_partial_response(crate::LlmResponse {
                usage,
                ..crate::LlmResponse::default()
            }));
        }
        let usage = call_usage(call);
        if let Some(stream) = &stream {
            stream.send(lash_core::llm::types::LlmStreamEvent::Usage(usage.clone()));
        }
        self.witness
            .returned
            .lock_recover()
            .push(token_usage(&usage));
        let parts = if last {
            vec![crate::LlmOutputPart::Text {
                text: "the usage law turn is done".to_string(),
                response_meta: None,
            }]
        } else {
            vec![crate::LlmOutputPart::ToolCall {
                call_id: format!("usage-probe-{call}"),
                tool_name: PROBE.to_string(),
                input_json: serde_json::json!({ "call": call }).to_string(),
                replay: None,
            }]
        };
        Ok(crate::LlmResponse {
            parts,
            terminal_reason: lash_core::LlmTerminalReason::Stop,
            // A completed attempt's usage is reported only with the
            // provider's own accounting record beside it.
            provider_usage: Some(serde_json::json!({ "call": call })),
            usage,
            ..crate::LlmResponse::default()
        })
    }

    fn policy(&self) -> crate::SessionPolicy {
        let mut policy = crate::testing::mock_session_policy();
        policy.session_id = Some(self.session_id.clone());
        policy
    }

    /// A fresh runtime over the tier's host and stores, loading the session
    /// the earlier executions committed.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: each result is established by the setup above"
    )]
    async fn runtime(&self) -> crate::LashRuntime {
        let mut config = crate::LawBackend::over_stores(
            Arc::clone(&self.tier.stores),
            Arc::clone(&self.tier.effect_host),
        )
        .host_config(
            crate::CommitBudget::bounded(1024 * 1024, 512),
            crate::QueuedWorkBatchingConfig::new(1),
        );
        config.providers.models = crate::testing::standard_test_models(self.model().into_handle());
        let probe: Arc<dyn crate::ToolProvider> = Arc::new(Probe {
            world: self.clone(),
        });
        let factories = crate::testing::test_standard_protocol_factories()
            .into_iter()
            .chain([Arc::new(crate::plugin::StaticPluginFactory::new(
                lash_core::plugin::PluginDeclaration::initial("conformance-usage-accounting"),
                crate::facade_support::PluginSpec::new().with_tool_provider(probe),
            ))
                as Arc<dyn crate::facade_support::PluginFactory>])
            .collect::<Vec<_>>();
        let policy = self.policy();
        let store =
            crate::conformance::law_session_store(self.tier.stores.as_ref(), &self.session_id)
                .await;
        Box::pin(
            crate::LashRuntime::builder(config, crate::testing::runtime_lease_owner())
                .with_session_id(&self.session_id)
                .with_policy(policy)
                .with_plugin_host(crate::facade_support::PluginHost::new(factories))
                .with_store(crate::conformance::helpers::session_view(
                    &store,
                    self.session_id.clone(),
                ))
                .with_queued_work(Arc::new(crate::NoSessionWork::new()))
                .build(),
        )
        .await
        .expect("build the usage-accounting runtime")
    }

    async fn drive(
        &self,
        scope: crate::ScopedEffectController<'_>,
    ) -> Result<crate::AssembledTurn, crate::RuntimeError> {
        let mut runtime = self.runtime().await;
        let mut input = crate::TurnInput::text(format!("usage law {}", self.session_id));
        input.trace_turn_id = Some(self.turn_id());
        tokio::time::timeout(
            TURN_BUDGET,
            runtime.drive_turn(input, crate::TurnOptions::new(self.cancel.clone(), scope)),
        )
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the usage-accounting turn of {} settles within {TURN_BUDGET:?}",
                self.session_id
            )
        })
    }

    /// An attempt that drives the turn and hands its result to `report`.
    fn attempt(
        &self,
        report: tokio::sync::mpsc::UnboundedSender<
            Result<crate::AssembledTurn, crate::RuntimeError>,
        >,
    ) -> crate::ConformanceTurnAttempt {
        let world = self.clone();
        Arc::new(move |scope| {
            let world = world.clone();
            let report = report.clone();
            Box::pin(async move {
                if world.kill.has_fired() {
                    world.recovering.cancel();
                }
                let driven = world.drive(scope).await;
                let end = crate::ConformanceTurnEnd::of(&driven);
                let _ = report.send(driven);
                end
            })
        })
    }

    /// The attempt the kill cuts down: it dies where the kill fires.
    fn killed_attempt(&self) -> crate::ConformanceTurnAttempt {
        let world = self.clone();
        Arc::new(move |scope| {
            let world = world.clone();
            Box::pin(async move {
                tokio::select! {
                    biased;
                    () = world.kill.fired() => {
                        panic!("the usage law's kill cut the turn's execution")
                    }
                    driven = world.drive(scope) => {
                        panic!("the killed execution ended ({:?}) before its kill fired", driven.map(|_| ()))
                    }
                }
            })
        })
    }

    /// Runs the script's turn to its end: once when nothing kills it, or
    /// killed where the script says and redriven by the tier's recovery.
    /// Returns the last execution's result.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the redrive reports its turn"
    )]
    pub(crate) async fn run(&self) -> Result<crate::AssembledTurn, crate::RuntimeError> {
        let (report, mut reports) = tokio::sync::mpsc::unbounded_channel();
        if self.script.kill == Kill::None {
            self.tier
                .runner
                .run_turn(self.admitted(), self.attempt(report))
                .await;
        } else {
            self.tier
                .runner
                .run_crashed_then_redriven_turn(
                    self.admitted(),
                    self.killed_attempt(),
                    self.attempt(report),
                )
                .await;
            assert!(self.kill.has_fired(), "the usage law's kill fired");
        }
        let mut last = None;
        while let Ok(driven) = reports.try_recv() {
            last = Some(driven);
        }
        last.expect("an execution of the usage law's turn reported")
    }

    /// Runs the script's turn until its kill fires and leaves it there: no
    /// execution of it ever runs again.
    pub(crate) async fn run_killed_forever(&self) {
        assert_ne!(self.script.kill, Kill::None, "a killed turn names its kill");
        let (report, _reports) = tokio::sync::mpsc::unbounded_channel();
        self.tier
            .runner
            .run_turn_until_crash(self.admitted(), self.attempt(report), self.kill.clone())
            .await;
    }

    /// The owner's accounting once every run it admitted is resolved.
    pub(crate) async fn settled(&self) -> crate::OwnerUsage {
        settled_usage(&self.tier, &self.owner()).await
    }

    /// Every fact of the owner, in order.
    #[expect(
        clippy::expect_used,
        reason = "conformance-law fixture: the tier's accounting answers reads"
    )]
    pub(crate) async fn facts(&self) -> Vec<crate::UsageFactRecord> {
        let accounting = self.tier.stores.usage_accounting();
        let mut facts = Vec::new();
        let mut after = None;
        loop {
            let page = accounting
                .load_usage_fact_page(
                    &self.owner(),
                    after.as_ref(),
                    std::num::NonZeroU32::new(64).expect("nonzero page"),
                )
                .await
                .expect("read the owner's usage facts");
            facts.extend(page.facts);
            match page.next {
                Some(next) => after = Some(next),
                None => return facts,
            }
        }
    }
}

/// The owner's accounting once every run it admitted is resolved, or the
/// law fails: the engine delivers each settlement after the effect it rides
/// is journaled, asynchronously to the turn.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the tier's accounting answers reads"
)]
pub(crate) async fn settled_usage(
    tier: &UsageAccountingTier,
    owner: &crate::RuntimeOwner,
) -> crate::OwnerUsage {
    let accounting = tier.stores.usage_accounting();
    let deadline = tokio::time::Instant::now() + DELIVERY;
    loop {
        let usage = accounting
            .load_owner_usage(owner)
            .await
            .expect("read the owner's usage");
        if usage.completeness.is_settled() {
            return usage;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the owner's usage settles within {DELIVERY:?}: {:?}",
            usage.completeness
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The index of the model call `request` is: how many assistant answers its
/// turn already holds.
fn call_of(request: &crate::LlmRequest) -> usize {
    let opened = request
        .messages
        .iter()
        .rposition(|message| message.starts_user_segment)
        .unwrap_or(0);
    request.messages[opened..]
        .iter()
        .filter(|message| matches!(message.role, lash_sansio::llm::types::LlmRole::Assistant))
        .count()
}

/// The tool every non-final call asks for. It kills or cancels the turn
/// where the script says, and otherwise answers at once.
struct Probe {
    world: World,
}

fn probe_definition() -> crate::ToolDefinition {
    let object = serde_json::json!({ "type": "object", "additionalProperties": true });
    crate::ToolDefinition::raw(
        "tool:conformance_usage_probe",
        PROBE,
        "The usage-accounting law's probe: answers at once.",
        object.clone(),
        object,
    )
}

#[async_trait::async_trait]
impl crate::ToolProvider for Probe {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        vec![probe_definition().manifest()]
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == PROBE).then(|| Arc::new(probe_definition().contract()))
    }

    async fn execute(&self, call: crate::ToolCall<'_>) -> crate::ToolAttemptOutcome {
        if let Some(children) = &self.world.children {
            return children.execute(call).await;
        }
        if let Some(child) = &self.world.deletion_child {
            return child.execute(call).await;
        }
        let index = call
            .args
            .get("call")
            .and_then(serde_json::Value::as_u64)
            .and_then(|index| usize::try_from(index).ok())
            .unwrap_or(usize::MAX);
        let world = &self.world;
        if world.script.kill == Kill::InTool(index) && !world.kill.has_fired() {
            world.kill.fire();
            // Where the tool runs in its own invocation (a Restate tool
            // child), the kill of the turn's execution does not end it: it
            // runs on, and finishes once the recovery is under way, as a
            // surviving child does. In process the kill drops it here.
            world.recovering.cancelled().await;
        }
        if world.script.ending == Ending::CancelledAfter(index) {
            world.cancel.cancel();
        }
        crate::ToolAttemptOutcome::done_without_intents(crate::ToolOutcomeDone::from_output(
            crate::ToolCallOutput::success(serde_json::json!({ "call": index })),
        ))
    }
}

/// Asserts the accounting of a world whose kills were all at `killed_runs`
/// dispatches the engine journaled no record of: one reported fact per
/// provider attempt that returned, each once, summing to what the provider
/// reported; one `unknown` run per such dispatch; nothing open.
pub(crate) async fn assert_each_returned_attempt_once(world: &World, killed_runs: u64, what: &str) {
    let usage = world.settled().await;
    let facts = world.facts().await;
    let identities = facts
        .iter()
        .map(|fact| fact.identity.clone())
        .collect::<BTreeSet<_>>();
    assert_eq!(
        identities.len(),
        facts.len(),
        "{what}: each fact identity is stored once: {facts:#?}"
    );
    let reported = facts
        .iter()
        .filter(|fact| fact.disposition == crate::UsageReporting::Reported)
        .count();
    assert_eq!(
        reported,
        world.returned_attempts(),
        "{what}: one reported fact per provider attempt that returned: {facts:#?}"
    );
    let total = usage
        .rows
        .iter()
        .fold(crate::TokenUsage::default(), |total, row| {
            total.saturating_add(&row.usage).0
        });
    assert_eq!(
        total,
        world.returned_total(),
        "{what}: the owner's totals are the provider's reported sum"
    );
    assert_eq!(usage.completeness.open_runs, 0, "{what}: nothing is open");
    assert_eq!(
        usage.completeness.unknown_runs, killed_runs,
        "{what}: each dispatch the engine journaled no record of is one explicit unknown run: {:?}",
        usage.completeness
    );
    assert_eq!(usage.completeness.conflicted_runs, 0, "{what}: no conflict");
}

// ---------------------------------------------------------------------------
// The laws.
// ---------------------------------------------------------------------------

/// E2: each paid attempt counts once however the turn's executions are cut
/// and replayed: one call or three, a billed failed attempt and its retry,
/// and a kill after each effect entry the turn journals before its last.
pub async fn each_paid_attempt_counts_once_under_any_boundary_grouping_and_replay(
    tier: &UsageAccountingTier,
) {
    for calls in [1, 3] {
        let kills = std::iter::once(Kill::None).chain((0..calls - 1).map(Kill::InTool));
        for kill in kills {
            let law = format!("grouping-{calls}-{kill:?}").to_lowercase();
            let world = World::new(
                tier,
                &law.replace(['(', ')'], ""),
                Script {
                    billed_failure: Some(0),
                    kill,
                    ..Script::completed(calls)
                },
            );
            let turn = world.run().await;
            assert!(turn.is_ok(), "{law}: the turn completes: {:?}", turn.err());
            assert_each_returned_attempt_once(&world, 0, &law).await;
            assert_eq!(
                world.invocations(),
                calls + 1,
                "{law}: the provider was asked once per attempt, never again on replay"
            );
            tier.runner.scenario_finished().await;
        }
    }
}

/// How a crash-table cell's turn ends.
#[derive(Clone, Copy, Debug)]
pub enum UsageCrashEnding {
    CommittedCompleted,
    CommittedCancelled,
    CommittedFailed,
    Forked,
    SessionDeleted,
    ParkedForever,
}

/// The crash table's points a tier's runner can cut on this tier.
#[derive(Clone, Copy, Debug)]
pub enum UsageCrashPoint {
    /// Killed after the run's admission, before the provider was sent
    /// anything.
    AdmittedNotDispatched,
    /// Killed after the provider was invoked, before its answer was
    /// journaled.
    ChargedNotJournaled,
}

/// E3: one cell of the crash table. A three-call turn is killed at `point`
/// in its second call and recovered by the tier, then ends as `ending`.
///
/// - `AdmittedNotDispatched`: no fact for the killed run and no provider
///   invocation for it; the rerun's settlement resolves it
///   `unknown(superseded_run)`. No token is invented.
/// - `ChargedNotJournaled`: the facts are the journaled run's only; the
///   killed run is one `unknown` run; the provider was asked twice for that
///   call, and the rebuy is visible, not hidden.
///
/// `ParkedForever` never recovers the kill: the killed run stays `open`
/// until the owner is retired, and only then becomes `unknown`.
pub async fn usage_crash_cell(
    tier: &UsageAccountingTier,
    point: UsageCrashPoint,
    ending: UsageCrashEnding,
) {
    let kill = match point {
        UsageCrashPoint::AdmittedNotDispatched => Kill::AtDispatch(1),
        UsageCrashPoint::ChargedNotJournaled => Kill::InFlight(1),
    };
    let script_ending = match ending {
        UsageCrashEnding::CommittedCancelled => Ending::CancelledAfter(1),
        UsageCrashEnding::CommittedFailed => Ending::Failed,
        _ => Ending::Completed,
    };
    let law = format!("crash-{point:?}-{ending:?}").to_lowercase();
    let world = World::new(
        tier,
        &law,
        Script {
            calls: 3,
            billed_failure: None,
            kill,
            ending: script_ending,
        },
    );
    if matches!(ending, UsageCrashEnding::ParkedForever) {
        world.run_killed_forever().await;
        assert_parked_forever(tier, &world, point).await;
        return;
    }
    let turn = world.run().await;
    match ending {
        UsageCrashEnding::CommittedFailed => assert!(
            matches!(
                crate::ConformanceTurnEnd::of(&turn),
                crate::ConformanceTurnEnd::Settled
            ),
            "{law}: the failed turn is committed failed"
        ),
        _ => assert!(turn.is_ok(), "{law}: the turn completes: {:?}", turn.err()),
    }
    let billed_calls = match script_ending {
        Ending::CancelledAfter(after) => after + 1,
        _ => 3,
    };
    let rebuy = usize::from(matches!(point, UsageCrashPoint::ChargedNotJournaled));
    assert_eq!(
        world.invocations(),
        billed_calls + rebuy,
        "{law}: the provider invocation count after journaling equals the count before"
    );
    assert_each_returned_attempt_once(&world, 1, &law).await;
    match ending {
        UsageCrashEnding::Forked => assert_fork_carries_no_usage(tier, &world, &law).await,
        UsageCrashEnding::SessionDeleted => {
            let before = world.settled().await;
            delete_session(tier, &world).await;
            assert_drained(tier, &world, &before, &law).await;
        }
        _ => {}
    }
}

/// A killed run no execution recovers is `open` until the owner is retired;
/// the deletion drain then resolves it `unknown(owner_retired)` and refuses
/// every later admission.
async fn assert_parked_forever(tier: &UsageAccountingTier, world: &World, point: UsageCrashPoint) {
    let accounting = tier.stores.usage_accounting();
    let deadline = tokio::time::Instant::now() + DELIVERY;
    let usage = loop {
        let usage = accounting
            .load_owner_usage(&world.owner())
            .await
            .unwrap_or_else(|error| panic!("read the owner's usage: {error}"));
        if usage
            .rows
            .iter()
            .map(|row| row.reported_attempts)
            .sum::<u64>()
            == 1
        {
            break usage;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the first call's fact is delivered: {usage:?}"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    assert_eq!(
        usage.completeness.open_runs, 1,
        "the killed run stays open until retirement: {:?}",
        usage.completeness
    );
    let expected_invocations = match point {
        UsageCrashPoint::AdmittedNotDispatched => 1,
        UsageCrashPoint::ChargedNotJournaled => 2,
    };
    assert_eq!(world.invocations(), expected_invocations);
    delete_session(tier, world).await;
    let drained = settled_usage(tier, &world.owner()).await;
    assert!(drained.completeness.retired, "the delete retired the owner");
    assert_eq!(drained.completeness.open_runs, 0);
    assert_eq!(
        drained.completeness.unknown_runs, 1,
        "the killed run is unknown(owner_retired), never silent"
    );
    assert_eq!(world.facts().await.len(), 1, "no fact was invented for it");
    assert_admission_refused(tier, world).await;
}

/// A fork of the session at its leaf is a new owner: it has no facts, and
/// the parent keeps its own.
async fn assert_fork_carries_no_usage(tier: &UsageAccountingTier, world: &World, law: &str) {
    let factory = tier.stores.session_store_factory();
    let head = factory
        .load_session_head_meta(&world.session_id)
        .await
        .unwrap_or_else(|error| panic!("{law}: read the parent's head: {error}"))
        .unwrap_or_else(|| panic!("{law}: the parent has a head"));
    let leaf = head
        .leaf_node_id
        .unwrap_or_else(|| panic!("{law}: the parent's head has a leaf"));
    let child = SessionId::from(format!("{}-fork", world.session_id));
    factory
        .fork_session(&crate::ForkSessionRequest {
            session_id: child.clone(),
            node_id: leaf,
            relation: crate::SessionRelation::Root,
            pending_observer_intents: Vec::new(),
            policy: crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
            ),
            plugin_config: Default::default(),
        })
        .await
        .unwrap_or_else(|error| panic!("{law}: fork the parent at its leaf: {error}"));
    let parent = world.settled().await;
    let forked = settled_usage(tier, &crate::RuntimeOwner::Session(child)).await;
    assert!(forked.rows.is_empty(), "{law}: the fork child has no facts");
    assert_eq!(
        parent
            .rows
            .iter()
            .map(|row| row.reported_attempts)
            .sum::<u64>(),
        u64::try_from(world.returned_attempts()).unwrap_or(u64::MAX),
        "{law}: the parent keeps its facts"
    );
}

/// Physically delete the world's session the way its delete obligation
/// does: the accounting drain is the first step.
async fn delete_session(tier: &UsageAccountingTier, world: &World) {
    let administration = super::session_close::administration(
        Arc::clone(&tier.effect_host),
        &tier.stores,
        super::session_close::CloseSink::new(tier.stores.session_store_factory(), 0),
    );
    lash_core::runtime::session_delete::physically_delete(&administration, &world.session_id)
        .await
        .unwrap_or_else(|failure| panic!("physically delete the law's session: {failure:?}"));
}

/// After the deletion drain: every fact is still readable, the owner is
/// retired, nothing is open, and a later admission is refused.
async fn assert_drained(
    tier: &UsageAccountingTier,
    world: &World,
    before: &crate::OwnerUsage,
    law: &str,
) {
    let after = settled_usage(tier, &world.owner()).await;
    assert!(after.completeness.retired, "{law}: the owner is retired");
    assert_eq!(after.completeness.open_runs, 0, "{law}: nothing is open");
    assert_eq!(
        after.rows, before.rows,
        "{law}: the facts outlive the physical delete"
    );
    assert_admission_refused(tier, world).await;
}

async fn assert_admission_refused(tier: &UsageAccountingTier, world: &World) {
    let refused = tier
        .stores
        .usage_accounting()
        .admit_usage_run(&crate::UsageRunAdmission {
            owner: world.owner(),
            effect: crate::UsageEffectKey::for_effect(
                &lash_sansio::EffectAddress::new(
                    crate::ExecutionScope::turn(&world.session_id, world.turn_id()),
                    "usage-law-after-retirement",
                )
                .unwrap_or_else(|error| panic!("an effect address: {error}")),
            ),
            execution_scope_key: "usage-law-after-retirement".to_string(),
            run: crate::UsageRunId::mint(),
            source: "turn".to_string(),
            model_key: crate::ModelKey::new("usage-law-key"),
            requested_model: "usage-law".to_string(),
            admitted_at_ms: 1,
        })
        .await;
    assert!(
        matches!(
            refused,
            Err(crate::UsageAdmissionError::OwnerRetired { .. })
        ),
        "a retired owner admits no run: {refused:?}"
    );
}

/// E3 `p5` on a tier that can inject it: the continuation is killed after
/// its projection committed and before it acknowledged. The retried
/// settlement is a no-op; the facts count once.
pub async fn a_settlement_retried_after_its_projection_counts_once(tier: &UsageAccountingTier) {
    let Some(kills) = tier.continuation.kill_next_settlement_after_projection() else {
        eprintln!("the tier's continuation takes no injected kill; the law has nothing to force");
        return;
    };
    let world = World::new(tier, "settle-retried", Script::completed(2));
    let turn = world.run().await;
    assert!(turn.is_ok(), "the turn completes: {:?}", turn.err());
    assert_each_returned_attempt_once(&world, 0, "settlement retried").await;
    assert!(kills() >= 1, "the continuation's kill fired");
    assert_eq!(world.invocations(), 2);
}

/// Preservation: a completed, a cancelled and a failed stop keep the
/// per-attribution totals the provider reported for the same script — every
/// reported attempt under the turn's source, the session's recorded model
/// key and that model's wire name.
pub async fn committed_turn_totals_are_preserved(tier: &UsageAccountingTier) {
    for (label, ending) in [
        ("completed", Ending::Completed),
        ("cancelled", Ending::CancelledAfter(0)),
        ("failed", Ending::Failed),
    ] {
        let world = World::new(
            tier,
            &format!("preserved-{label}"),
            Script {
                ending,
                billed_failure: Some(0),
                ..Script::completed(2)
            },
        );
        let _turn = world.run().await;
        let usage = world.settled().await;
        let policy = crate::testing::mock_session_policy();
        let attribution = crate::UsageAttributionKey {
            source: "turn".to_string(),
            model_key: policy
                .model_key()
                .unwrap_or_else(|| panic!("{label}: the law's policy records a model"))
                .clone(),
            requested_model: policy.wire_model().unwrap_or_default().to_string(),
        };
        let report = usage.report();
        let row = report
            .by_attribution
            .get(&attribution)
            .unwrap_or_else(|| panic!("{label}: the turn row for {attribution:?}: {report:?}"));
        assert_eq!(report.by_attribution.len(), 1, "{label}: one row");
        assert_eq!(
            row.usage,
            world.returned_total(),
            "{label}: the row keeps every reported attempt's usage"
        );
        assert_eq!(row.unreported_attempts, 0, "{label}: nothing unreported");
        tier.runner.scenario_finished().await;
    }
}

macro_rules! usage_crash_cells {
    ($(($name:ident, $point:ident, $ending:ident)),* $(,)?) => {
        $(
            /// One cell of the E3 crash table (see [`usage_crash_cell`]).
            pub async fn $name(tier: &UsageAccountingTier) {
                usage_crash_cell(tier, UsageCrashPoint::$point, UsageCrashEnding::$ending).await;
            }
        )*
    };
}

usage_crash_cells![
    (
        usage_crash_p1_committed_completed,
        AdmittedNotDispatched,
        CommittedCompleted
    ),
    (
        usage_crash_p1_committed_cancelled,
        AdmittedNotDispatched,
        CommittedCancelled
    ),
    (
        usage_crash_p1_committed_failed,
        AdmittedNotDispatched,
        CommittedFailed
    ),
    (usage_crash_p1_forked, AdmittedNotDispatched, Forked),
    (
        usage_crash_p1_session_deleted,
        AdmittedNotDispatched,
        SessionDeleted
    ),
    (
        usage_crash_p1_parked_forever,
        AdmittedNotDispatched,
        ParkedForever
    ),
    (
        usage_crash_p2_committed_completed,
        ChargedNotJournaled,
        CommittedCompleted
    ),
    (
        usage_crash_p2_committed_cancelled,
        ChargedNotJournaled,
        CommittedCancelled
    ),
    (
        usage_crash_p2_committed_failed,
        ChargedNotJournaled,
        CommittedFailed
    ),
    (usage_crash_p2_forked, ChargedNotJournaled, Forked),
    (
        usage_crash_p2_session_deleted,
        ChargedNotJournaled,
        SessionDeleted
    ),
    (
        usage_crash_p2_parked_forever,
        ChargedNotJournaled,
        ParkedForever
    ),
];

mod deletion;
pub use deletion::session_delete_drains_accounting_first;
mod park;
pub use park::usage_of_a_root_parked_forever_before_finalization_is_read_without_driving;
mod children;
pub use children::tool_child_spend_counts_once_without_settlement_charging;
mod endings;
pub use endings::{
    operator_cancelled_parked_keeps_each_paid_call_once,
    refused_superseded_keeps_each_paid_call_once, substrate_lost_keeps_each_paid_call_once,
};
