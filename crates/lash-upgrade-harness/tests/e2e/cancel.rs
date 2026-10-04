//! H2 cancellation, protected drain and physical handover witnesses.
use super::tools::{
    Scenario, assert_body_identity, assert_call, assert_durable_prefix, attempts, calls,
    deliveries, events,
};
use anyhow::{Result, anyhow, ensure};
use lash_core::ToolCallId;
use lash_core::tool_run::{AttemptResult, CallDecision, LogicalTerminal, RunEvent, RunLifecycle};
use lash_remote_protocol::RemoteTurnStatus;
use lash_upgrade_harness::e2e::case::{ArtifactIdentity, CaseSpec, Channel, StoreKind};
use lash_upgrade_harness::e2e::control::{BarrierKind, ToolControl};
use lash_upgrade_harness::e2e::evidence::{DecodedRecord, Evidence};
use lash_upgrade_harness::e2e::host::{HostCommand, HostKind};
use lash_upgrade_harness::e2e::provider::ProviderKind;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s08_pre_final() -> Result<()> {
    super::h2::run(super::h2::Row::PreFinal).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s09_before_intent() -> Result<()> {
    super::h2::run(super::h2::Row::BeforeIntent).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s09_after_intent() -> Result<()> {
    super::h2::run(super::h2::Row::AfterIntent).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s10_empty_middle_rank() -> Result<()> {
    super::h2::run(super::h2::Row::Ranks).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s11_inline_loser() -> Result<()> {
    super::h2::run(super::h2::Row::InlineLoser).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s11_deferred_loser() -> Result<()> {
    super::h2::run(super::h2::Row::DeferredLoser).await
}

pub fn spec(id: &str, store: StoreKind, artifacts: Vec<ArtifactIdentity>) -> Result<CaseSpec> {
    let (rules, channel) = match id {
        "S08" => (vec!["R1", "R5", "L03"], Channel::Standard),
        "S09" => (vec!["R1", "L04"], Channel::Standard),
        "S10" => (vec!["R1", "R2", "L18"], Channel::Rlm),
        "S11" => (vec!["R2", "L06", "L16"], Channel::Rlm),
        _ => return Err(anyhow!("unknown H2 cancel scenario {id}")),
    };
    ensure!(
        id == "S08" || store != StoreKind::SqliteMemory,
        "cold recovery requires a persistent store"
    );
    Ok(CaseSpec {
        id: id.to_owned(),
        rules: rules.into_iter().map(str::to_owned).collect(),
        // F01/F02, C03 and B00/B01 are all landed in the candidate baseline.
        requires: Vec::new(),
        host: HostKind::AgentService,
        store,
        channel,
        provider: ProviderKind::Scripted,
        restate_nodes: 1,
        artifacts,
        cuts: Vec::new(),
        expected_terminal: if matches!(id, "S08" | "S09" | "S10") {
            "Cancelled"
        } else {
            "Answered"
        }
        .to_owned(),
    })
}

/// Raw event pages read from the registered external receiver, never body logs.
fn mutations(evidence: &Evidence, call: &ToolCallId) -> Result<Vec<serde_json::Value>> {
    let mut result = Vec::new();
    for page in &evidence.effects {
        if page.get("kind").and_then(|v| v.as_str()) != Some("h2_receiver_events") {
            continue;
        }
        let events = page
            .get("events")
            .and_then(|v| v.as_array())
            .ok_or_else(|| anyhow!("external receiver has no raw event page"))?;
        for event in events {
            if event.pointer("/payload/call_id").and_then(|v| v.as_str()) == Some(call.as_str()) {
                ensure!(
                    event.get("process_id").is_some()
                        && event.get("sequence").and_then(|v| v.as_u64()).is_some()
                        && event.get("invocation").is_some(),
                    "mutation has no actual event identity"
                );
                result.push(event.clone());
            }
        }
    }
    ensure!(
        evidence
            .effects
            .iter()
            .any(|page| page.get("kind").and_then(|v| v.as_str()) == Some("h2_receiver_events")),
        "missing raw external receiver evidence"
    );
    Ok(result)
}

fn phase_position(evidence: &Evidence, call: &ToolCallId, phase: &str) -> Result<usize> {
    let positions: Vec<_> = events(evidence)?
        .into_iter()
        .enumerate()
        .filter_map(|(index, event)| {
            let found = match (phase, event) {
                (
                    "D",
                    RunEvent::Decided {
                        call_id,
                        decision: CallDecision::Final { .. },
                        ..
                    },
                ) => call_id == call,
                ("issued", RunEvent::DeclarationsIssued { call_id }) => call_id == call,
                ("settled", RunEvent::DeclarationsSettled { call_id }) => call_id == call,
                ("V", RunEvent::Presented { call_id, .. }) => call_id == call,
                ("incorporated", RunEvent::Incorporated { call_id }) => call_id == call,
                _ => false,
            };
            found.then_some(index)
        })
        .collect();
    ensure!(positions.len() == 1, "{phase} must occur once for {call}");
    Ok(positions[0])
}

pub async fn pre_final(scenario: &mut Scenario<'_>, spec: &CaseSpec) -> Result<Evidence> {
    let initial = scenario.start(spec).await?;
    let ids = calls(&initial, &["intent"])?;
    let call = &ids["intent"];
    let body = scenario.barrier(call, BarrierKind::BodyEntered)?;
    scenario.wait(body.clone()).await?;
    let before = scenario.read().await?;
    ensure!(
        !events(&before)?
            .iter()
            .any(|event| matches!(event,RunEvent::Decided {call_id,..} if call_id==call)),
        "pre-final cancel cut was already decided"
    );
    scenario.cancel().await?;
    scenario.release(body).await?;
    let evidence = scenario.finish(RemoteTurnStatus::Cancelled, None).await?;
    assert_call(&evidence, call, LogicalTerminal::Cancelled)?;
    ensure!(
        mutations(&evidence, call)?.is_empty(),
        "cancelled call realized a declared mutation"
    );
    ensure!(!events(&evidence)?.iter().any(|event| matches!(event,
        RunEvent::DeclarationsIssued {call_id} | RunEvent::DeclarationsSettled {call_id} if call_id==call)),
        "pre-final cancellation issued declarations");
    assert_body_identity(&evidence, call, Some(1))?;
    calls(&evidence, &["intent"])?;
    Ok(evidence)
}

#[derive(Clone, Copy)]
pub enum ProtectedCut {
    BeforeIntent,
    AfterIntent,
}

/// Run separately for both named cuts: D→intent and intent→V.
pub async fn post_final(
    scenario: &mut Scenario<'_>,
    spec: &CaseSpec,
    cut: ProtectedCut,
) -> Result<Evidence> {
    let initial = scenario.start(spec).await?;
    let ids = calls(&initial, &["intent"])?;
    let call = &ids["intent"];
    let body = scenario.barrier(call, BarrierKind::BodyEntered)?;
    scenario.wait(body.clone()).await?;
    let held = scenario.barrier(
        call,
        match cut {
            ProtectedCut::BeforeIntent => BarrierKind::DeclarationIssued,
            ProtectedCut::AfterIntent => BarrierKind::VProposed,
        },
    )?;
    scenario
        .control
        .tool(ToolControl::Hold(held.clone()))
        .await?;
    scenario.release(body).await?;
    scenario
        .wait(scenario.barrier(call, BarrierKind::DDurable)?)
        .await?;
    let proof = scenario.wait(held.clone()).await?;
    let before = scenario.read().await?;
    let final_decisions: Vec<_> = events(&before)?.into_iter().filter(|event| matches!(event,
        RunEvent::Decided {call_id,decision:CallDecision::Final {declares:true,..},..} if call_id==call)).collect();
    ensure!(
        final_decisions.len() == 1,
        "protected cut needs an actual declared final"
    );
    let durable_x = attempts(&before)
        .into_iter()
        .find(|entry| &entry.call_id == call)
        .ok_or_else(|| anyhow!("protected final has no independent X"))?
        .clone();
    match cut {
        ProtectedCut::BeforeIntent => ensure!(
            mutations(&before, call)?.is_empty(),
            "before-intent cut missed realization"
        ),
        ProtectedCut::AfterIntent => ensure!(
            mutations(&before, call)?.len() == 1,
            "after-intent cut lacks actual mutation"
        ),
    }
    scenario.cancel().await?;
    scenario.kill_and_reopen(&proof).await?;
    scenario.release(held).await?;
    let evidence = scenario.finish(RemoteTurnStatus::Cancelled, None).await?;
    assert_durable_prefix(&before, &evidence)?;
    ensure!(
        attempts(&evidence)
            .into_iter()
            .filter(|entry| &entry.call_id == call)
            .eq([&durable_x]),
        "cancel or recovery changed the durable final result"
    );
    assert_call(&evidence, call, LogicalTerminal::Final)?;
    assert_body_identity(&evidence, call, Some(1))?;
    ensure!(
        mutations(&evidence, call)?.len() == 1,
        "protected final must realize its mutation once"
    );
    let order = ["D", "issued", "settled", "V", "incorporated"]
        .into_iter()
        .map(|phase| phase_position(&evidence, call, phase))
        .collect::<Result<Vec<_>>>()?;
    ensure!(
        order.windows(2).all(|pair| pair[0] < pair[1]),
        "protected drain or presentation order differs"
    );
    Ok(evidence)
}

/// S10's rank2 is intentionally intent-free. Its seat cannot open rank3's
/// declarations while rank1 still owes a mutation; an unrelated VM effect can.
pub async fn empty_middle_rank(scenario: &mut Scenario<'_>, spec: &CaseSpec) -> Result<Evidence> {
    scenario.start(spec).await?;
    scenario
        .host
        .command(HostCommand::Process {
            action: "await-tool-bodies".into(),
            input: serde_json::json!({"labels":["rank_one","rank_two","rank_three"]}),
        })
        .await?;
    let initial = scenario.read().await?;
    let ids = calls(&initial, &["rank_one", "rank_two", "rank_three"])?;
    let one = &ids["rank_one"];
    let two = &ids["rank_two"];
    let three = &ids["rank_three"];
    for call in [one, two, three] {
        scenario
            .wait(scenario.barrier(call, BarrierKind::BodyEntered)?)
            .await?;
    }
    let intent = scenario.barrier(one, BarrierKind::DeclarationIssued)?;
    scenario
        .control
        .tool(ToolControl::Hold(intent.clone()))
        .await?;
    scenario
        .release(scenario.barrier(one, BarrierKind::BodyEntered)?)
        .await?;
    scenario
        .wait(scenario.barrier(one, BarrierKind::DDurable)?)
        .await?;
    scenario.wait(intent.clone()).await?;
    scenario
        .release(scenario.barrier(two, BarrierKind::BodyEntered)?)
        .await?;
    let cut = scenario
        .wait(scenario.barrier(two, BarrierKind::VDurable)?)
        .await?;
    let before = scenario.read().await?;
    ensure!(
        phase_position(&before, one, "D")? < phase_position(&before, two, "D")?,
        "rank2 was not the middle final"
    );
    ensure!(
        !events(&before)?.iter().any(|event| matches!(event,
        RunEvent::DeclarationsSettled {call_id} if call_id==one)),
        "rank1 already seated at cut"
    );
    ensure!(
        before
            .effects
            .iter()
            .any(
                |effect| effect.get("kind").and_then(|v| v.as_str()) == Some("h2_program_effect")
                    && effect.get("value") == Some(&serde_json::json!("unrelated-progress"))
            ),
        "unrelated program effect did not progress"
    );
    scenario.kill_and_reopen(&cut).await?;
    scenario
        .release(scenario.barrier(three, BarrierKind::BodyEntered)?)
        .await?;
    scenario
        .wait(scenario.barrier(three, BarrierKind::DDurable)?)
        .await?;
    scenario.cancel().await?;
    let protected = scenario.read().await?;
    ensure!(
        !events(&protected)?.iter().any(|event| matches!(event,
        RunEvent::DeclarationsIssued {call_id} if call_id==three)),
        "empty middle rank bypassed rank1 drain"
    );
    ensure!(
        mutations(&protected, three)?.is_empty(),
        "rank3 mutation ran before rank1 seat"
    );
    scenario.release(intent).await?;
    let evidence = scenario.finish(RemoteTurnStatus::Cancelled, None).await?;
    assert_durable_prefix(&before, &evidence)?;
    for call in [one, two, three] {
        assert_call(&evidence, call, LogicalTerminal::Final)?;
    }
    for call in [one, three] {
        ensure!(
            mutations(&evidence, call)?.len() == 1,
            "protected rank did not realize once"
        );
    }
    ensure!(
        mutations(&evidence, two)?.is_empty(),
        "intent-free rank mutated receiver"
    );
    ensure!(
        phase_position(&evidence, one, "settled")? < phase_position(&evidence, three, "issued")?,
        "rank3 declaration preceded lower owed drain"
    );
    for call in [one, two] {
        assert_body_identity(&evidence, call, Some(1))?;
    }
    Ok(evidence)
}

/// S11 uses the same program for inline and Deferred losers. The acknowledged
/// Deferred descriptor transfers unresolved; an executing inline body cannot.
pub async fn live_loser(
    scenario: &mut Scenario<'_>,
    spec: &CaseSpec,
    deferred: bool,
) -> Result<Evidence> {
    scenario.start(spec).await?;
    scenario
        .host
        .command(HostCommand::Process {
            action: "await-tool-bodies".into(),
            input: serde_json::json!({"labels":["winner","loser"]}),
        })
        .await?;
    let initial = scenario.read().await?;
    let ids = calls(&initial, &["winner", "loser"])?;
    let winner = &ids["winner"];
    let loser = &ids["loser"];
    let loser_body = scenario.barrier(loser, BarrierKind::BodyEntered)?;
    scenario.wait(loser_body.clone()).await?;
    scenario
        .release(scenario.barrier(winner, BarrierKind::BodyEntered)?)
        .await?;
    scenario
        .host
        .command(HostCommand::Process {
            action: "await-tool-bodies".into(),
            input: serde_json::json!({"labels":["after"]}),
        })
        .await?;
    let progressed = scenario.read().await?;
    let all = calls(&progressed, &["winner", "loser", "after"])?;
    let after = &all["after"];
    ensure!(
        !events(&progressed)?.iter().any(|event| matches!(event,
        RunEvent::Decided {call_id,decision:CallDecision::Cancelled,..} if call_id==loser)),
        "race winner cancelled its live loser"
    );
    scenario
        .release(scenario.barrier(after, BarrierKind::BodyEntered)?)
        .await?;
    if deferred {
        scenario.release(loser_body.clone()).await?;
        scenario
            .wait(scenario.barrier(loser, BarrierKind::XDurable)?)
            .await?;
        let parked = scenario.read().await?;
        ensure!(
            attempts(&parked).iter().any(|entry| &entry.call_id == loser
                && matches!(
                    entry.result,
                    AttemptResult::Pending { .. } | AttemptResult::Deferred { .. }
                )),
            "Deferred loser has no acknowledged descriptor"
        );
    }
    let request = scenario
        .host
        .command(HostCommand::Transfer {
            run: scenario.work()?.run.clone(),
        })
        .await?;
    ensure!(
        request.work.run == scenario.work()?.run,
        "handover targeted another logical Run"
    );
    if !deferred {
        let held = scenario.read().await?;
        ensure!(
            !held
                .journals
                .iter()
                .any(|fact| matches!(fact.decoded, Some(DecodedRecord::Transfer(_)))),
            "executing inline loser permitted a physical cut"
        );
        ensure!(
            !events(&held)?.iter().any(|event| matches!(
                event,
                RunEvent::Lifecycle {
                    state: RunLifecycle::Closing
                }
            )),
            "physical request made logical Closing"
        );
        ensure!(
            !attempts(&held).iter().any(|entry| &entry.call_id == loser),
            "held inline loser already ACKed"
        );
        scenario.release(loser_body).await?;
        scenario
            .wait(scenario.barrier(loser, BarrierKind::XDurable)?)
            .await?;
    }
    scenario
        .wait_owner(BarrierKind::ContinuationPublished)
        .await?;
    let transferred = scenario.read().await?;
    let transfers: Vec<_> = transferred
        .journals
        .iter()
        .filter_map(|fact| match &fact.decoded {
            Some(DecodedRecord::Transfer(transfer)) => Some(transfer),
            _ => None,
        })
        .collect();
    ensure!(
        transfers.len() == 1,
        "handover must publish exactly one physical successor"
    );
    let transfer = transfers[0];
    transfer.check_capture(&lash_core::tool_run::Cut {
        reason: transfer.reason,
        phase: lash_core::tool_run::CutPhase::Capturable,
    })?;
    ensure!(
        transfer.vm_continuation,
        "physical successor lost the VM continuation"
    );
    ensure!(
        transfer
            .attempts
            .iter()
            .filter(|entry| &entry.call_id == loser)
            .count()
            == 1,
        "transfer lost or duplicated loser acknowledged X"
    );
    if deferred {
        let descriptor = deliveries(&transferred)?
            .into_iter()
            .find(|delivery| &delivery.call_id == loser)
            .and_then(|delivery| delivery.completion)
            .ok_or_else(|| anyhow!("Deferred body lacks reserved source"))?;
        ensure!(
            transfer
                .subscriptions
                .iter()
                .any(|subscription| subscription.source == descriptor),
            "transfer lost pending Deferred subscription"
        );
        let mut work = scenario.work()?.clone();
        work.call = Some(loser.to_string());
        work.ordinal = Some(1);
        scenario
            .control
            .tool(ToolControl::Resolve {
                work,
                value: serde_json::json!("loser"),
            })
            .await?;
    }
    let evidence = scenario
        .finish(RemoteTurnStatus::Answered, Some("winner"))
        .await?;
    for call in [winner, loser, after] {
        assert_call(&evidence, call, LogicalTerminal::Final)?;
        assert_body_identity(&evidence, call, Some(1))?;
    }
    Ok(evidence)
}
