//! FIG-4393: a session command a lowered commit budget strands is refused
//! once per shift, and raising the budget settles it.
//!
//! The commit budget is host policy (ADR 0058), and every commit over a head
//! carries the head's config and checkpoint manifest. A host that lowers the
//! budget below a live head strands the session's leading command: not even
//! its bare settlement fits. The shift that meets it refuses the command
//! run with the typed budget error and stops, as an engine's shift loop
//! does for a command run it released, instead of admitting the same
//! command under run after run. The command stays open and unsettled, and nothing
//! of it is lost: once the host raises the budget again, the next shift
//! applies it and it settles with its outcome.
//!
//! FIG-4471: a command over a budget its head's bare commit fits settles
//! failed with the budget refusal. The settlement's refusal receipt is not
//! charged, so it lands at a budget of exactly the bare commit's size.

use crate::ActorContext;
use std::sync::Arc;

use lash_core::engine::{AdmitVerdict, RunOutcome, ShiftAbort, ShiftLoop, ShiftStop};
use lash_sansio::TurnId;
use pretty_assertions::assert_eq;

use crate::conformance::shift_admission::{ShiftParts, on_tier};

/// A byte budget below any head's bare commit: the session config alone
/// exceeds it.
const LOWERED_BYTES: usize = 64;

/// More runs than one shift may admit on one stranded command: a shift that
/// reaches it admits the same command under run after run.
const LOOP_BOUND: u32 = 8;

/// The text of the host append a lowered budget strands.
const STRANDED_NOTE: &str = "a note the host appended before the budget was lowered";

/// What one shift of the law's session did: each run it ended refused,
/// with its refusal, and how it stopped. `Err` names a shift that admitted
/// [`LOOP_BOUND`] runs.
type ShiftRecord = Result<(Vec<(TurnId, crate::RuntimeError)>, ShiftStop), String>;

/// Each refused run with its refusal's code.
pub(super) fn refused_codes(
    refusals: &[(TurnId, crate::RuntimeError)],
) -> Vec<(String, crate::RuntimeErrorCode)> {
    refusals
        .iter()
        .map(|(run, error)| (run.to_string(), error.code.clone()))
        .collect()
}

/// `parts` with the host's commit budget set to `budget`.
pub(super) fn under_budget(parts: &ShiftParts, budget: crate::CommitBudget) -> ShiftParts {
    let mut parts = parts.clone();
    parts.host.durability.commit_budget = budget;
    parts
}

fn lowered_budget() -> crate::CommitBudget {
    crate::CommitBudget::new(
        crate::CommitBudgetLimit::bounded(LOWERED_BYTES),
        crate::CommitBudgetLimit::Unbounded,
    )
}

/// Queue a host append on the session's command lane: the command a lowered
/// budget strands.
async fn queue_stranded_append(parts: &ShiftParts) -> crate::BatchId {
    queue_append(parts, "stranded", STRANDED_NOTE).await
}

/// Queue a host append of `text` on the session's command lane, keyed by
/// `key`.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: the session store queues the command"
)]
async fn queue_append(parts: &ShiftParts, key: &str, text: &str) -> crate::BatchId {
    parts
        .store
        .enqueue_queued_work(
            crate::QueuedWorkBatchDraft::new(
                parts.session_id.clone(),
                crate::DeliveryPolicy::AfterCurrentTurnCommit,
                crate::SessionCommand::AppendSessionNodes {
                    request: Box::new(crate::AppendSessionNodesRequest {
                        operation_id: format!("host-append:{key}"),
                        nodes: vec![crate::SessionAppendNode::message(
                            crate::PluginMessage::text(crate::MessageRole::User, text),
                        )],
                        requires_ancestor_node_id: None,
                    }),
                },
            )
            .with_source_key(format!("{key}-append")),
        )
        .await
        .expect("queue the host append")
        .batch_id
}

/// Execute the session on the tier as an engine's shift does: admission after
/// admission under the shift loop's rules, where a run refused terminally
/// is released, as the engine records it.
pub(super) async fn engine_shift(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &ShiftParts,
    shift: &str,
) -> ShiftRecord {
    let request = parts.request(shift);
    on_tier(runner, parts, move |mut runtime, scope| {
        let request = request.clone();
        Box::pin(async move {
            let mut rules = ShiftLoop::new();
            let mut refusals = Vec::new();
            for ordinal in 0..LOOP_BOUND {
                let admitted = match lash_core::shift::admit_shift(
                    &mut runtime,
                    &scope,
                    &request,
                    ordinal,
                    None,
                )
                .await
                {
                    Ok(AdmitVerdict::Admit(admitted)) => admitted,
                    Ok(AdmitVerdict::Idle) => return Ok((refusals, ShiftStop::Idle)),
                    other => return Err(format!("admission answered {other:?}")),
                };
                if let Err(stop) = rules.before(&admitted) {
                    return Ok((refusals, stop));
                }
                let work = admitted.work().clone();
                let run = admitted.run().clone();
                let outcome =
                    match lash_core::shift::execute_admitted_run(&mut runtime, &scope, admitted)
                        .await
                    {
                        Ok(outcome) => outcome,
                        Err(ShiftAbort::Refused(error)) if !error.is_retryable() => {
                            refusals.push((run.clone(), error));
                            RunOutcome::Released { run }
                        }
                        Err(other) => return Err(format!("run `{run}` aborted: {other:?}")),
                    };
                if let Some(stop) = rules.after(&work, &outcome) {
                    return Ok((refusals, stop));
                }
            }
            Err(format!(
                "the shift admitted {LOOP_BOUND} runs on the stranded command: {:?}",
                refused_codes(&refusals)
            ))
        })
    })
    .await
}

/// The budgeted bytes a byte-budget refusal names its commit carrying.
pub(super) fn refused_commit_bytes(message: &str) -> usize {
    message
        .strip_prefix("runtime commit carries ")
        .and_then(|rest| rest.split_once(' '))
        .and_then(|(bytes, _)| bytes.parse().ok())
        .unwrap_or_else(|| panic!("a byte-budget refusal names its commit's bytes: {message}"))
}

/// Execute the session under the lowered budget, and check the one refusal
/// the shift surfaces: the command run is refused with the typed budget
/// error, the shift stops there, and the command stays open and unsettled.
/// Answers the refusal, which names the bare settlement's bytes.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
async fn assert_lowered_shift_refuses_once(
    runner: &Arc<dyn crate::ConformanceTurnRunner>,
    parts: &ShiftParts,
    stranded: &crate::BatchId,
    shift: &str,
) -> crate::RuntimeError {
    let (refusals, stop) = engine_shift(runner, &under_budget(parts, lowered_budget()), shift)
        .await
        .unwrap_or_else(|looped| panic!("the lowered shift stops: {looped}"));
    let [(run, refusal)] = refusals.as_slice() else {
        panic!(
            "the lowered budget surfaces one refusal, got {:?}",
            refused_codes(&refusals)
        );
    };
    assert_eq!(
        refusal.code,
        crate::RuntimeErrorCode::StoreCommitByteBudgetExceeded
    );
    assert!(
        refusal.message.contains(&format!(
            "exceeding the {LOWERED_BYTES}-byte transaction budget"
        )),
        "{}",
        refusal.message
    );
    assert_eq!(
        stop,
        ShiftStop::Yielded { run: run.clone() },
        "the shift stops at the refused command run"
    );
    let open = parts
        .store
        .list_open_queued_work(&parts.session_id)
        .await
        .expect("read the session's open work");
    assert_eq!(
        open.iter()
            .map(|batch| batch.batch_id.clone())
            .collect::<Vec<_>>(),
        vec![stranded.clone()],
        "the stranded command stays open"
    );
    assert!(
        parts
            .store
            .queued_work_batch_completion(&parts.session_id, stranded.as_str())
            .await
            .expect("read the command's settlement")
            .is_none(),
        "nothing settled the stranded command"
    );
    refusal.clone()
}

/// A budget lowered below a live head strands its leading command, and each
/// shift that meets it surfaces one typed budget refusal and stops: no
/// further run is admitted on the same command. The command stays open,
/// unsettled and unlost.
pub async fn a_lowered_budget_refuses_a_stranded_command_once_per_shift(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = ShiftParts::new(prefix, "budget-lowered", &effect_host, &stores, 1).await;
    let stranded = queue_stranded_append(&parts).await;
    assert_lowered_shift_refuses_once(&runner, &parts, &stranded, "budget-lowered-shift-1").await;
    assert_lowered_shift_refuses_once(&runner, &parts, &stranded, "budget-lowered-shift-2").await;
}

/// Raising the budget again recovers a stranded command: the next shift
/// applies it, it settles appended, and the lane is empty.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn raising_the_budget_settles_a_stranded_command(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = ShiftParts::new(prefix, "budget-raised", &effect_host, &stores, 1).await;
    let stranded = queue_stranded_append(&parts).await;
    assert_lowered_shift_refuses_once(&runner, &parts, &stranded, "budget-raised-lowered").await;

    // `parts` runs under a budget every commit of the law fits under.
    let (refusals, stop) = engine_shift(&runner, &parts, "budget-raised-shift")
        .await
        .unwrap_or_else(|looped| panic!("the raised shift stops: {looped}"));
    assert!(
        refusals.is_empty(),
        "the raised budget refuses nothing: {:?}",
        refused_codes(&refusals)
    );
    assert_eq!(stop, ShiftStop::Idle, "the lane drains");
    assert!(
        parts
            .store
            .list_open_queued_work(&parts.session_id)
            .await
            .expect("read the session's open work")
            .is_empty(),
        "the recovered command settled"
    );
    let completion = parts
        .store
        .queued_work_batch_completion(&parts.session_id, stranded.as_str())
        .await
        .expect("read the command's settlement")
        .expect("the recovered command wrote its settlement");
    assert!(
        matches!(
            completion.command_outcomes.get(&stranded),
            Some(crate::SessionCommandOutcome::AppendSessionNodes {
                outcome: crate::AppendSessionNodesOutcome::Appended { .. }
            })
        ),
        "the recovered append lands: {:?}",
        completion.command_outcomes.get(&stranded)
    );
    let head = crate::conformance::helpers::load_window_state(&parts.store, &parts.session_id)
        .await
        .expect("load the recovered head")
        .expect("the recovered session has a head");
    assert!(
        head.session_graph
            .nodes
            .iter()
            .any(|node| format!("{:?}", node.payload).contains(STRANDED_NOTE)),
        "the stranded note reached the head"
    );
}

/// A command over a budget its head's bare commit fits settles failed
/// (FIG-4471): over a committed head, at a budget of exactly the bare
/// settlement's size, measured by the lowered shift's refusal, the append's
/// commit is refused, its failed settlement lands with the budget refusal,
/// nothing of the append reaches the head, and the lane drains.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn an_over_budget_command_settles_failed_at_its_bare_commits_size(
    prefix: &str,
    effect_host: ActorContext,
    stores: Arc<dyn crate::StoreSet>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) {
    let parts = ShiftParts::new(prefix, "budget-bare-fit", &effect_host, &stores, 1).await;
    // A created session's head is durable before any command drains: an
    // append under the law's ample budget commits it.
    queue_append(&parts, "head", "the note that commits the session's head").await;
    let (refusals, stop) = engine_shift(&runner, &parts, "budget-bare-head")
        .await
        .unwrap_or_else(|looped| panic!("the head shift stops: {looped}"));
    assert!(refusals.is_empty(), "{:?}", refused_codes(&refusals));
    assert_eq!(stop, ShiftStop::Idle, "the head's append settles");
    // The append must exceed even the bare failure settlement, whose
    // recorded head and admission can be larger than a short successful note.
    let command = queue_append(&parts, "stranded", &STRANDED_NOTE.repeat(256)).await;
    let bare =
        assert_lowered_shift_refuses_once(&runner, &parts, &command, "budget-bare-measure").await;
    let bare_bytes = refused_commit_bytes(&bare.message);

    let fitting = crate::CommitBudget::new(
        crate::CommitBudgetLimit::bounded(bare_bytes),
        crate::CommitBudgetLimit::Unbounded,
    );
    let (refusals, stop) = engine_shift(&runner, &under_budget(&parts, fitting), "budget-bare-fit")
        .await
        .unwrap_or_else(|looped| panic!("the fitting shift stops: {looped}"));
    assert!(
        refusals.is_empty(),
        "a budget the bare settlement fits refuses no run: {:?}",
        refused_codes(&refusals)
    );
    assert_eq!(stop, ShiftStop::Idle, "the lane drains");
    assert!(
        parts
            .store
            .list_open_queued_work(&parts.session_id)
            .await
            .expect("read the session's open work")
            .is_empty(),
        "the over-budget command settled"
    );
    let completion = parts
        .store
        .queued_work_batch_completion(&parts.session_id, command.as_str())
        .await
        .expect("read the command's settlement")
        .expect("the over-budget command wrote its settlement");
    let Some(crate::SessionCommandOutcome::Failed { code, message }) =
        completion.command_outcomes.get(&command)
    else {
        panic!(
            "the over-budget command settles failed: {:?}",
            completion.command_outcomes.get(&command)
        );
    };
    assert_eq!(
        *code,
        crate::RuntimeErrorCode::StoreCommitByteBudgetExceeded
    );
    assert!(
        message.contains(&format!(
            "exceeding the {bare_bytes}-byte transaction budget"
        )),
        "{message}"
    );
    assert!(
        refused_commit_bytes(message) > bare_bytes,
        "the refused append outgrows its bare settlement: {message}"
    );
    let head = crate::conformance::helpers::load_window_state(&parts.store, &parts.session_id)
        .await
        .expect("load the settled head")
        .expect("the settled session has a head");
    assert!(
        !head
            .session_graph
            .nodes
            .iter()
            .any(|node| format!("{:?}", node.payload).contains(STRANDED_NOTE)),
        "nothing of the refused append reached the head"
    );
}
