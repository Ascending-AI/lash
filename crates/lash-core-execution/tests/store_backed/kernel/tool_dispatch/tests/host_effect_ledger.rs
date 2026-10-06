// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

//! Witnesses for `runbooks/host-effect-ledger`.
//!
//! A host that owns an external system builds a durable undo ledger on the
//! tool boundary with no new core primitive: the attempt body claims a row
//! keyed by [`crate::AttemptContext::tool_call_id`] *before* touching the
//! world, and the after-check notes the call's durable outcome under the
//! same call id ([`crate::plugin::PreparedCallReadView::call_id`]). The check
//! runs at least once per final result, so the ledger deduplicates on it. Deduplication,
//! crash reconciliation, and reverse compensation are host passes over that
//! ledger — the runtime's only obligation is that both seams carry the same
//! call id.

use super::*;
use crate::plugin::PluginSessionRequest;

const SEED: u64 = 0x5_2d23;

/// The external system the instrumented tools write to: a stand-in for a
/// ticket tracker, a payments API, anything whose writes the runtime cannot
/// journal.
#[derive(Default)]
struct ExternalWorld {
    applied: std::sync::Mutex<Vec<String>>,
}

impl ExternalWorld {
    fn apply(&self, effect: &str) {
        self.applied.lock_recover().push(effect.to_string());
    }

    fn applied(&self) -> Vec<String> {
        self.applied.lock_recover().clone()
    }
}

/// The stage a ledger row is in, in the order a healthy row walks it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stage {
    /// Claimed inside the attempt, the effect's landing unconfirmed — the
    /// crash window. A row found here is reconciled against the world,
    /// never assumed.
    Pending,
    /// The world write landed and the attempt marked it.
    Applied,
}

struct EffectRow {
    seq: u64,
    stage: Stage,
    /// The call's durable outcome as the after-tool hook observed it. `false`
    /// on a `Pending` row means the call is over and the row still cannot
    /// say whether the effect landed — that is what reconciliation is for.
    outcome: Option<bool>,
}

/// What `claim` tells the attempt body about the row it just touched.
#[derive(Debug, PartialEq, Eq)]
enum Claim {
    /// No row existed; this attempt owns applying the effect.
    Fresh,
    /// A dead attempt left the row pending; this attempt finishes the apply.
    ResumePending,
    /// The effect is already applied; deduplicate and touch nothing.
    AlreadyApplied,
    /// The call already reached a terminal ledger state.
    Terminal,
}

/// The host's durable ledger: one row per instrumented call, keyed by the
/// call id the runtime mints at preparation and reports on the executed-call
/// record. `next_seq` orders rows so reverse compensation can name a
/// retained history point.
#[derive(Default)]
struct HostEffectLedger {
    state: std::sync::Mutex<LedgerState>,
}

#[derive(Default)]
struct LedgerState {
    next_seq: u64,
    rows: BTreeMap<String, EffectRow>,
}

impl HostEffectLedger {
    /// The write-ahead seam inside the attempt: claim the call's row before
    /// the world is touched, so whichever side of the window a crash lands
    /// on is distinguishable afterward.
    fn claim(&self, call_id: &str, _effect: &str) -> Claim {
        let mut state = self.state.lock_recover();
        match state.rows.get_mut(call_id) {
            None => {
                state.next_seq += 1;
                let seq = state.next_seq;
                state.rows.insert(
                    call_id.to_string(),
                    EffectRow {
                        seq,
                        stage: Stage::Pending,
                        outcome: None,
                    },
                );
                Claim::Fresh
            }
            Some(row) if row.stage == Stage::Pending => Claim::ResumePending,
            Some(row) if row.stage == Stage::Applied => Claim::AlreadyApplied,
            Some(_) => Claim::Terminal,
        }
    }

    fn mark_applied(&self, call_id: &str) {
        if let Some(row) = self.state.lock_recover().rows.get_mut(call_id) {
            row.stage = Stage::Applied;
        }
    }

    /// The after-tool hook's write: the call's durable outcome, noted on the
    /// row — a correlator, never the verdict on whether the effect landed.
    fn note_outcome(&self, call_id: &str, ok: bool) {
        if let Some(row) = self.state.lock_recover().rows.get_mut(call_id) {
            row.outcome = Some(ok);
        }
    }

    fn row(&self, call_id: &str) -> Option<(u64, Stage, Option<bool>)> {
        self.state
            .lock_recover()
            .rows
            .get(call_id)
            .map(|row| (row.seq, row.stage, row.outcome))
    }
}

/// An instrumented tool: claims its ledger row before writing to the world.
/// `args.mode` decides where the attempt stops, modelling the failures a
/// real host's tool meets:
///
/// - `ok` — claim, apply, mark, report success.
/// - `fail_before_apply` — claim, then fail; the world is untouched.
/// - `die_after_apply` — claim, apply, then die: the crash window, where
///   neither the pending row nor the failed call can say the effect landed.
/// - `ok` under a nonzero `transient_failures` budget — apply and mark, then
///   report a retryable failure; the retry's claim deduplicates on the row.
struct LedgeredEffectTools {
    definition: crate::ToolDefinition,
    ledger: Arc<HostEffectLedger>,
    world: Arc<ExternalWorld>,
    executions: Arc<AtomicUsize>,
    transient_failures: usize,
}

#[async_trait::async_trait]
impl ToolProvider for LedgeredEffectTools {
    fn tool_manifests(&self) -> Vec<crate::ToolManifest> {
        manifests(vec![self.definition.clone()])
    }

    fn resolve_contract(&self, name: &str) -> Option<Arc<crate::ToolContract>> {
        (name == self.definition.name()).then(|| Arc::new(self.definition.contract()))
    }

    async fn execute(&self, call: ToolCall<'_>) -> crate::ToolAttemptOutcome {
        self.executions.fetch_add(1, Ordering::SeqCst);
        let call_id = call.context.call_id().to_string();
        let effect = call.args["effect"].as_str().unwrap_or("effect").to_string();
        let mode = call.args["mode"].as_str().unwrap_or("ok");

        match self.ledger.claim(&call_id, &effect) {
            // The effect is durably on record — repeating the write would
            // double it. A replayed or retried attempt answers as if it had
            // just run.
            Claim::AlreadyApplied | Claim::Terminal => {
                return ToolOutcome::ok(json!({ "deduplicated": call_id })).into();
            }
            Claim::Fresh | Claim::ResumePending => {}
        }

        if mode == "fail_before_apply" {
            return ToolOutcome::err_fmt("the host refused before the effect was written").into();
        }

        self.world.apply(&effect);

        if mode == "die_after_apply" {
            return ToolOutcome::err_fmt("the host died inside the crash window").into();
        }

        self.ledger.mark_applied(&call_id);

        if self.transient_failures > 0
            && self.executions.load(Ordering::SeqCst) <= self.transient_failures
        {
            return ToolOutcome::failure_with_delay(
                crate::ToolFailureClass::External,
                "transient",
                "the acknowledgement was lost after the write",
                Some(0),
            )
            .into();
        }

        ToolOutcome::ok(json!({ "applied": effect })).into()
    }
}

fn ledger_tool(name: &str, execution_policy: ExecutionPolicy) -> crate::ToolDefinition {
    crate::ToolDefinition::raw(
        format!("tool:{name}"),
        name,
        "",
        json!({
            "type": "object",
            "properties": {
                "effect": { "type": "string" },
                "mode": { "type": "string" }
            },
            "additionalProperties": false
        }),
        json!({ "type": "string" }),
    )
    .expect("valid declared tool schemas")
    .with_execution_policy(execution_policy)
}

/// The hook a ledger-owning host installs: records the observation — call id
/// plus the call's durable outcome — and notes it on the row. It never marks
/// an effect landed or unlanded; that verdict belongs to reconciliation.
fn ledger_hook(
    ledger: Arc<HostEffectLedger>,
    observations: Arc<std::sync::Mutex<Vec<(String, bool)>>>,
) -> crate::plugin::ToolResultCheckHook {
    Arc::new(move |input| {
        let ledger = Arc::clone(&ledger);
        let observations = Arc::clone(&observations);
        Box::pin(async move {
            let ok = matches!(
                input.final_result.outcome,
                crate::ToolCallOutcome::Success(_)
            );
            let call_id = input.prepared.call_id();
            observations.lock_recover().push((call_id.to_string(), ok));
            ledger.note_outcome(call_id.as_str(), ok);
            Ok(crate::plugin::AfterToolContributions::default())
        })
    })
}

async fn ledger_dispatch_context<'h>(
    ports: crate::support::DispatchPorts<'h>,
    provider: Arc<dyn ToolProvider>,
    hook: crate::plugin::ToolResultCheckHook,
) -> ToolDispatchContext<'h> {
    let spec = crate::PluginSpec::new()
        .with_tool_provider(provider)
        .with_tool_result_check(lash_core_execution::hook_key!("ledger"), hook);
    let plugins = crate::support::plugin_host(vec![Arc::new(StaticPluginFactory::new(
        lash_core_execution::plugin::PluginDeclaration::initial("ledger_tools"),
        spec,
    ))])
    .build_session(PluginSessionRequest::creation("root", Default::default()))
    .expect("plugin session");
    exact_dispatch_context_with_plugins(ports, plugins).await
}

async fn dispatch_ledger_call(
    context: &ToolDispatchContext<'_>,
    call_id: &str,
    args: serde_json::Value,
) -> ToolDispatchOutcome {
    let tool_context = crate::testing::ToolCallFixture::from_dispatch(Arc::new(context.clone()))
        .call_id(lash_core_execution::ToolCallId::fixture(call_id));
    Box::pin(dispatch_tool_call_with_execution_context(
        context,
        "effect".to_string(),
        args,
        tool_context,
    ))
    .await
}

#[tokio::test]
async fn host_effect_ledger_hook_call_id_matches_the_executed_call_record() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let ledger = Arc::new(HostEffectLedger::default());
    let world = Arc::new(ExternalWorld::default());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider: Arc<dyn ToolProvider> = Arc::new(LedgeredEffectTools {
        definition: ledger_tool("effect", ExecutionPolicy::Once),
        ledger: Arc::clone(&ledger),
        world: Arc::clone(&world),
        executions: Arc::new(AtomicUsize::new(0)),
        transient_failures: 0,
    });
    let context = ledger_dispatch_context(
        crate::support::double_dispatch_ports(&double, &handler),
        provider,
        ledger_hook(Arc::clone(&ledger), Arc::clone(&observations)),
    )
    .await;

    let outcome = dispatch_ledger_call(&context, "call-1", json!({ "effect": "issue.open" })).await;

    assert!(outcome.record.output.is_success());
    assert_eq!(
        outcome.record.call_id,
        lash_core_execution::ToolCallId::fixture("call-1")
    );
    // The hook observation, the durable record, and the ledger row the
    // attempt wrote under `AttemptContext::tool_call_id` all name one id.
    assert_eq!(
        observations.lock_recover().as_slice(),
        &[(ledger_key("call-1"), true)]
    );
    assert_eq!(
        ledger.row(&ledger_key("call-1")),
        Some((1, Stage::Applied, Some(true))),
        "the attempt claimed and marked its row under the same call id"
    );
    assert_eq!(world.applied(), vec!["issue.open".to_string()]);
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn host_effect_ledger_replay_reexecutes_neither_effect_nor_hook() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let ledger = Arc::new(HostEffectLedger::default());
    let world = Arc::new(ExternalWorld::default());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let executions = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(LedgeredEffectTools {
        definition: ledger_tool("effect", ExecutionPolicy::Once),
        ledger: Arc::clone(&ledger),
        world: Arc::clone(&world),
        executions: Arc::clone(&executions),
        transient_failures: 0,
    });
    let mut context = ledger_dispatch_context(
        crate::support::double_dispatch_ports(&double, &handler),
        provider,
        ledger_hook(Arc::clone(&ledger), Arc::clone(&observations)),
    )
    .await;
    // A journaling controller replays the recorded attempt outcome for the
    // same replay key — which is derived from the call id — so the second
    // dispatch is a redrive, not a re-execution.
    context.effect_controller = ScopedEffectController::shared(
        Arc::new(IntentReplayController::new(None).await),
        crate::AdmittedScope::runtime_operation("test-runtime-effect-controller"),
    )
    .expect("valid test runtime scope");

    let args = json!({ "effect": "deploy" });
    let live = dispatch_ledger_call(&context, "call-replay", args.clone()).await;
    let replayed = dispatch_ledger_call(&context, "call-replay", args).await;

    assert!(live.record.output.is_success());
    assert!(replayed.record.output.is_success());
    assert_eq!(
        replayed.record.call_id,
        lash_core_execution::ToolCallId::fixture("call-replay")
    );
    assert_eq!(
        executions.load(Ordering::SeqCst),
        1,
        "replay serves the journaled outcome; the attempt body does not re-run"
    );
    assert_eq!(
        observations.lock_recover().len(),
        1,
        "replay skips the hook — the ledger sees one observation per call"
    );
    assert_eq!(world.applied(), vec!["deploy".to_string()]);
    drop(context);
    handler.close().await.expect("close the dispatch handler");
}

#[tokio::test]
async fn host_effect_ledger_observes_a_settled_pending_call_under_its_call_id() {
    let (double, handler) = crate::support::open_dispatch_handler(SEED).await;
    let ledger = Arc::new(HostEffectLedger::default());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider: Arc<dyn ToolProvider> = Arc::new(PendingProbeTools {
        definition: pending_probe_tool(ExecutionPolicy::Once),
        attempts: Arc::new(AtomicUsize::new(0)),
        mode: PendingProbeMode::PendingWithKey,
    });
    let context = ledger_dispatch_context(
        crate::support::double_dispatch_ports(&double, &handler),
        provider,
        ledger_hook(Arc::clone(&ledger), Arc::clone(&observations)),
    )
    .await;
    let prepared = pending_prepared_call();
    let tool_context = tool_context_for_prepared(&context, &prepared);

    let launch = coordinate_prepared_tool_call_launch_with_execution_context(
        &context,
        prepared,
        None,
        tool_context,
    )
    .await;

    let ToolCallLaunch::Pending(pending) = launch else {
        panic!("the tool should park pending");
    };
    assert!(
        observations.lock_recover().is_empty(),
        "a parked call has no outcome to correlate yet"
    );

    let attachment_store = Arc::clone(&context.attachment_store);
    let execution = crate::RuntimeExecutionContext::new(
        Arc::new(context),
        crate::support::sqlite_memory_store_set()
            .await
            .process_env_store(),
        attachment_store,
        Arc::new(crate::ChronologicalProjection::default()),
        crate::TurnContext::default(),
        crate::ProcessExecutionEnvSpec::new(
            crate::AdmittedPluginConfig::default(),
            crate::SessionPolicy::new(crate::TurnBudget::Unbounded, crate::MaxToolCalls::new(1024)),
        ),
    );
    let completed = execution
        .pending_completion_dispatch_outcome(
            &crate::tool_dispatch::ToolCallIds {
                call_id: crate::ToolCallId::fixture("pending-call"),
                provider_call_id: None,
            },
            "test:pending-call",
            pending.tool_name,
            pending.args,
            crate::Resolution::Ok(serde_json::json!({ "done": true })),
            None,
            pending.attempts,
            pending.captures,
            pending.triggers,
        )
        .await;

    assert!(completed.record.output.is_success());
    assert_eq!(
        observations.lock_recover().as_slice(),
        &[(ledger_key("pending-call"), true)],
        "the settlement observation names the parked call's durable id"
    );
    drop(execution);
    handler.close().await.expect("close the dispatch handler");
}

/// The ledger key a tool keys its effects on: the call's own id.
fn ledger_key(label: &str) -> String {
    lash_core_execution::ToolCallId::fixture(label).to_string()
}
