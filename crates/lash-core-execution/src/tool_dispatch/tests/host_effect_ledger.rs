// FIG-2971: this file is test/tooling/host code; ambient fs/env/process
// access is sanctioned here (the workspace clippy ban targets production
// library code).
#![allow(clippy::disallowed_methods)]

//! Witnesses for `runbooks/host-effect-ledger`.
//!
//! A host that owns an external system builds a durable undo ledger on the
//! tool boundary with no new core primitive: the attempt body claims a row
//! keyed by [`crate::AttemptContext::tool_call_id`] *before* touching the
//! world, and the after-tool hook notes the call's durable outcome under the
//! same [`crate::plugin::ToolResultHookContext::call_id`]. Deduplication,
//! crash reconciliation, and reverse compensation are host passes over that
//! ledger — the runtime's only obligation is that both seams carry the same
//! call id.

use super::*;

/// The external system the instrumented tools write to: a stand-in for a
/// ticket tracker, a payments API, anything whose writes the runtime cannot
/// journal.
#[derive(Default)]
struct ExternalWorld {
    applied: std::sync::Mutex<Vec<String>>,
    reverted: std::sync::Mutex<Vec<String>>,
}

impl ExternalWorld {
    fn apply(&self, effect: &str) {
        self.applied.lock_recover().push(effect.to_string());
    }

    fn revert(&self, effect: &str) {
        self.reverted.lock_recover().push(effect.to_string());
        self.applied
            .lock_recover()
            .retain(|landed| landed != effect);
    }

    fn contains(&self, effect: &str) -> bool {
        self.applied
            .lock_recover()
            .iter()
            .any(|landed| landed == effect)
    }

    fn applied(&self) -> Vec<String> {
        self.applied.lock_recover().clone()
    }

    fn reverted(&self) -> Vec<String> {
        self.reverted.lock_recover().clone()
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
    /// Reconciliation proved the pending effect never landed. Nothing to
    /// undo.
    NoEffect,
    /// Reverse-compensated at a retained history point.
    Compensated,
}

struct EffectRow {
    seq: u64,
    /// The effect the attempt intended to write, recorded so a dead attempt
    /// can be reconciled and a replayed one deduplicated.
    effect: String,
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
    fn claim(&self, call_id: &str, effect: &str) -> Claim {
        let mut state = self.state.lock_recover();
        match state.rows.get_mut(call_id) {
            None => {
                state.next_seq += 1;
                let seq = state.next_seq;
                state.rows.insert(
                    call_id.to_string(),
                    EffectRow {
                        seq,
                        effect: effect.to_string(),
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

    /// The host's restart pass: pending rows whose calls are over are
    /// reconciled against the world — the recorded effect either landed or
    /// it did not. Returns how many rows converged.
    fn reconcile(&self, world: &ExternalWorld) -> usize {
        let mut state = self.state.lock_recover();
        let mut resolved = 0;
        for row in state.rows.values_mut() {
            if row.stage == Stage::Pending && row.outcome != Some(true) {
                row.stage = if world.contains(&row.effect) {
                    Stage::Applied
                } else {
                    Stage::NoEffect
                };
                resolved += 1;
            }
        }
        resolved
    }

    /// Reverse compensation at a retained history point: undo every applied
    /// row at or after `seq_floor`, newest first. `budget` models a pass
    /// that dies partway — compensated rows are skipped on re-entry, so a
    /// restarted pass resumes exactly where the dead one stopped.
    fn compensate_from(&self, seq_floor: u64, world: &ExternalWorld, budget: usize) -> usize {
        let mut state = self.state.lock_recover();
        let mut targets: Vec<&mut EffectRow> = state
            .rows
            .values_mut()
            .filter(|row| row.seq >= seq_floor && row.stage == Stage::Applied)
            .collect();
        targets.sort_by(|a, b| b.seq.cmp(&a.seq));
        let mut undone = 0;
        for row in targets {
            if undone == budget {
                break;
            }
            world.revert(&row.effect);
            row.stage = Stage::Compensated;
            undone += 1;
        }
        undone
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
        let call_id = call
            .context
            .tool_call_id()
            .expect("an instrumented tool requires the durable call id")
            .to_string();
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
            return ToolOutcome::retryable_failure(
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

fn ledger_tool(name: &str, retry_policy: ToolRetryPolicy) -> crate::ToolDefinition {
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
    .with_retry_policy(retry_policy)
}

/// The hook a ledger-owning host installs: records the observation — call id
/// plus the call's durable outcome — and notes it on the row. It never marks
/// an effect landed or unlanded; that verdict belongs to reconciliation.
fn ledger_hook(
    ledger: Arc<HostEffectLedger>,
    observations: Arc<std::sync::Mutex<Vec<(String, bool)>>>,
) -> crate::plugin::AfterToolCallHook {
    Arc::new(move |ctx| {
        let ledger = Arc::clone(&ledger);
        let observations = Arc::clone(&observations);
        Box::pin(async move {
            let ok = matches!(
                ctx.result.as_done_output().map(|output| &output.outcome),
                Some(crate::ToolCallOutcome::Success(_))
            );
            observations.lock_recover().push((ctx.call_id.clone(), ok));
            ledger.note_outcome(&ctx.call_id, ok);
            Ok(Vec::new())
        })
    })
}

fn ledger_dispatch_context(
    provider: Arc<dyn ToolProvider>,
    hook: crate::plugin::AfterToolCallHook,
) -> ToolDispatchContext<'static> {
    let spec = crate::PluginSpec::new()
        .with_tool_provider(provider)
        .with_after_tool_call(hook);
    let plugins = PluginHost::new(vec![Arc::new(StaticPluginFactory::new(
        "ledger_tools",
        spec,
    ))])
    .build_session("root")
    .expect("plugin session");
    exact_dispatch_context_with_plugins(plugins)
}

async fn dispatch_ledger_call(
    context: &ToolDispatchContext<'_>,
    call_id: &str,
    args: serde_json::Value,
) -> ToolDispatchOutcome {
    let tool_context = ToolContext::from_dispatch(Arc::new(context.clone()))
        .tool_call_id(Some(call_id.to_string()))
        .build();
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
    let ledger = Arc::new(HostEffectLedger::default());
    let world = Arc::new(ExternalWorld::default());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider: Arc<dyn ToolProvider> = Arc::new(LedgeredEffectTools {
        definition: ledger_tool("effect", ToolRetryPolicy::Never),
        ledger: Arc::clone(&ledger),
        world: Arc::clone(&world),
        executions: Arc::new(AtomicUsize::new(0)),
        transient_failures: 0,
    });
    let context = ledger_dispatch_context(
        provider,
        ledger_hook(Arc::clone(&ledger), Arc::clone(&observations)),
    );

    let outcome = dispatch_ledger_call(&context, "call-1", json!({ "effect": "issue.open" })).await;

    assert!(outcome.record.output.is_success());
    assert_eq!(outcome.record.call_id.as_deref(), Some("call-1"));
    // The hook observation, the durable record, and the ledger row the
    // attempt wrote under `AttemptContext::tool_call_id` all name one id.
    assert_eq!(
        observations.lock_recover().as_slice(),
        &[("call-1".to_string(), true)]
    );
    assert_eq!(
        ledger.row("call-1"),
        Some((1, Stage::Applied, Some(true))),
        "the attempt claimed and marked its row under the same call id"
    );
    assert_eq!(world.applied(), vec!["issue.open".to_string()]);
}

#[tokio::test]
async fn host_effect_ledger_deduplicates_retried_attempts_on_the_call_id() {
    let ledger = Arc::new(HostEffectLedger::default());
    let world = Arc::new(ExternalWorld::default());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let executions = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(LedgeredEffectTools {
        definition: ledger_tool("effect", ToolRetryPolicy::safe(3, 0, 0)),
        ledger: Arc::clone(&ledger),
        world: Arc::clone(&world),
        executions: Arc::clone(&executions),
        transient_failures: 1,
    });
    let context = ledger_dispatch_context(
        provider,
        ledger_hook(Arc::clone(&ledger), Arc::clone(&observations)),
    );

    // Attempt 1 applies the effect, marks the row, then reports a retryable
    // failure — the acknowledgement was lost after the write. Attempt 2's
    // claim finds the applied row and touches nothing.
    let outcome = dispatch_ledger_call(&context, "call-r", json!({ "effect": "charge" })).await;

    assert!(outcome.record.output.is_success());
    assert_eq!(executions.load(Ordering::SeqCst), 2);
    assert_eq!(
        world.applied(),
        vec!["charge".to_string()],
        "the retried attempt must not double-apply the effect"
    );
    assert_eq!(ledger.row("call-r"), Some((1, Stage::Applied, Some(true))));
    // The hook observed both attempts under one call id — re-entry is the
    // documented shape, so observations deduplicate on it.
    assert_eq!(
        observations.lock_recover().as_slice(),
        &[("call-r".to_string(), false), ("call-r".to_string(), true),]
    );
}

#[tokio::test]
async fn host_effect_ledger_replay_reexecutes_neither_effect_nor_hook() {
    let ledger = Arc::new(HostEffectLedger::default());
    let world = Arc::new(ExternalWorld::default());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let executions = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn ToolProvider> = Arc::new(LedgeredEffectTools {
        definition: ledger_tool("effect", ToolRetryPolicy::Never),
        ledger: Arc::clone(&ledger),
        world: Arc::clone(&world),
        executions: Arc::clone(&executions),
        transient_failures: 0,
    });
    let mut context = ledger_dispatch_context(
        provider,
        ledger_hook(Arc::clone(&ledger), Arc::clone(&observations)),
    );
    // A journaling controller replays the recorded attempt outcome for the
    // same replay key — which is derived from the call id — so the second
    // dispatch is a redrive, not a re-execution.
    context.effect_controller =
        RuntimeEffectControllerHandle::shared(Arc::new(IntentReplayController::new(None)));

    let args = json!({ "effect": "deploy" });
    let live = dispatch_ledger_call(&context, "call-replay", args.clone()).await;
    let replayed = dispatch_ledger_call(&context, "call-replay", args).await;

    assert!(live.record.output.is_success());
    assert!(replayed.record.output.is_success());
    assert_eq!(replayed.record.call_id.as_deref(), Some("call-replay"));
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
}

#[tokio::test]
async fn host_effect_ledger_reconciles_a_pending_row_against_the_world() {
    let ledger = Arc::new(HostEffectLedger::default());
    let world = Arc::new(ExternalWorld::default());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider: Arc<dyn ToolProvider> = Arc::new(LedgeredEffectTools {
        definition: ledger_tool("effect", ToolRetryPolicy::Never),
        ledger: Arc::clone(&ledger),
        world: Arc::clone(&world),
        executions: Arc::new(AtomicUsize::new(0)),
        transient_failures: 0,
    });
    let context = ledger_dispatch_context(
        provider,
        ledger_hook(Arc::clone(&ledger), Arc::clone(&observations)),
    );

    // The attempt claimed its row, wrote to the world, and died inside the
    // crash window before marking the row — surfacing to the runtime as a
    // failed call, which the hook notes without concluding anything.
    let outcome = dispatch_ledger_call(
        &context,
        "call-crash",
        json!({ "effect": "refund", "mode": "die_after_apply" }),
    )
    .await;

    assert!(!outcome.record.output.is_success());
    assert_eq!(
        ledger.row("call-crash"),
        Some((1, Stage::Pending, Some(false))),
        "a failed call leaves the pending row undecided"
    );
    assert!(world.contains("refund"));

    // The host's restart pass reconciles the pending row against the world.
    assert_eq!(ledger.reconcile(&world), 1);
    assert_eq!(
        ledger.row("call-crash"),
        Some((1, Stage::Applied, Some(false))),
        "the effect is on record as landed once the world confirms it"
    );
}

#[tokio::test]
async fn host_effect_ledger_compensates_only_what_reconciliation_proves() {
    let ledger = Arc::new(HostEffectLedger::default());
    let world = Arc::new(ExternalWorld::default());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider: Arc<dyn ToolProvider> = Arc::new(LedgeredEffectTools {
        definition: ledger_tool("effect", ToolRetryPolicy::Never),
        ledger: Arc::clone(&ledger),
        world: Arc::clone(&world),
        executions: Arc::new(AtomicUsize::new(0)),
        transient_failures: 0,
    });
    let context = ledger_dispatch_context(
        provider,
        ledger_hook(Arc::clone(&ledger), Arc::clone(&observations)),
    );

    // A partial failure: the first call's effect landed, the second died
    // before writing. Both rows pend the reconciliation the host runs at
    // restart.
    let ok = dispatch_ledger_call(&context, "call-ok", json!({ "effect": "invite" })).await;
    let failed = dispatch_ledger_call(
        &context,
        "call-bad",
        json!({ "effect": "charge", "mode": "fail_before_apply" }),
    )
    .await;
    assert!(ok.record.output.is_success());
    assert!(!failed.record.output.is_success());

    assert_eq!(ledger.reconcile(&world), 1);
    assert_eq!(
        ledger.row("call-bad").map(|(_, stage, _)| stage),
        Some(Stage::NoEffect),
        "the failed call's effect never landed — nothing to undo"
    );

    // Reverse compensation reaches only the row the world confirms.
    assert_eq!(ledger.compensate_from(0, &world, usize::MAX), 1);
    assert_eq!(world.reverted(), vec!["invite".to_string()]);
    assert!(world.applied().is_empty());
    assert_eq!(
        ledger.row("call-ok").map(|(_, stage, _)| stage),
        Some(Stage::Compensated)
    );
}

#[tokio::test]
async fn host_effect_ledger_reverse_compensation_resumes_at_a_retained_point() {
    let ledger = Arc::new(HostEffectLedger::default());
    let world = Arc::new(ExternalWorld::default());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider: Arc<dyn ToolProvider> = Arc::new(LedgeredEffectTools {
        definition: ledger_tool("effect", ToolRetryPolicy::Never),
        ledger: Arc::clone(&ledger),
        world: Arc::clone(&world),
        executions: Arc::new(AtomicUsize::new(0)),
        transient_failures: 0,
    });
    let context = ledger_dispatch_context(
        provider,
        ledger_hook(Arc::clone(&ledger), Arc::clone(&observations)),
    );

    for (call_id, effect) in [
        ("call-a", "step.a"),
        ("call-b", "step.b"),
        ("call-c", "step.c"),
    ] {
        let outcome = dispatch_ledger_call(&context, call_id, json!({ "effect": effect })).await;
        assert!(outcome.record.output.is_success());
    }

    // Compensate everything at or after call-b's history point. A budget of
    // one models a pass that dies mid-compensation: call-c is undone, then
    // the pass stops.
    let retained = ledger.row("call-b").map(|(seq, _, _)| seq).unwrap();
    assert_eq!(ledger.compensate_from(retained, &world, 1), 1);
    assert_eq!(world.reverted(), vec!["step.c".to_string()]);
    assert_eq!(
        world.applied(),
        vec!["step.a".to_string(), "step.b".to_string()]
    );

    // The restarted pass resumes where the dead one stopped — compensated
    // rows are skipped, and rows before the point are out of scope.
    assert_eq!(ledger.compensate_from(retained, &world, usize::MAX), 1);
    assert_eq!(
        world.reverted(),
        vec!["step.c".to_string(), "step.b".to_string()],
        "newest first, each exactly once"
    );
    assert_eq!(world.applied(), vec!["step.a".to_string()]);
    assert_eq!(
        ledger.row("call-a").map(|(_, stage, _)| stage),
        Some(Stage::Applied),
        "the retained history point is not compensated"
    );

    // A further pass is a no-op: reverse compensation is idempotent.
    assert_eq!(ledger.compensate_from(retained, &world, usize::MAX), 0);
}

#[tokio::test]
async fn host_effect_ledger_observes_a_settled_pending_call_under_its_call_id() {
    let ledger = Arc::new(HostEffectLedger::default());
    let observations = Arc::new(std::sync::Mutex::new(Vec::new()));
    let provider: Arc<dyn ToolProvider> = Arc::new(PendingProbeTools {
        definition: pending_probe_tool(ToolRetryPolicy::Never),
        attempts: Arc::new(AtomicUsize::new(0)),
        mode: PendingProbeMode::PendingWithKey,
    });
    let context = ledger_dispatch_context(
        provider,
        ledger_hook(Arc::clone(&ledger), Arc::clone(&observations)),
    );
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
        SessionId::from("session"),
        Arc::new(context),
        Arc::new(crate::InMemoryProcessExecutionEnvStore::new()),
        attachment_store,
        Arc::new(crate::ChronologicalProjection::default()),
        None,
        crate::TurnContext::default(),
    );
    let completed = execution
        .pending_completion_dispatch_outcome(
            "pending-call",
            pending.tool_name,
            pending.args,
            crate::Resolution::Ok(serde_json::json!({ "done": true })),
            None,
            pending.duration_ms,
            pending.attempts,
            pending.captures,
            pending.triggers,
        )
        .await;

    assert!(completed.record.output.is_success());
    assert_eq!(
        observations.lock_recover().as_slice(),
        &[("pending-call".to_string(), true)],
        "the settlement observation names the parked call's durable id"
    );
}
