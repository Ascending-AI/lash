//! H3 transfer rows on the workbench product host: S12, S23, S31 and S32.
//!
//! Two workbench generations share one SQLite file store. The production
//! generation drain cuts the Run on N; N+1 adopts the fenced follow-on.
//! Transfer facts are read from the authoritative session head, never from
//! a Restate completion slot, and every Run fact from its own V7 journal.
use super::h2::{Segment, Shared, transferred};
use super::tools::{Scenario, assert_body_identity, assert_call, attempts, calls, events};
use anyhow::{Context, Result, anyhow, ensure};
use lash_core::ToolCallId;
use lash_core::tool_run::{AttemptResult, LogicalTerminal, RunEvent, RunJournalEntry, RunTransfer};
use lash_remote_protocol::RemoteTurnStatus;
use lash_upgrade_harness::e2e::case::{
    ArtifactIdentity, CaseSpec, Channel, Leg, Permutation, StoreKind,
};
use lash_upgrade_harness::e2e::control::{
    Barrier, BarrierKind, BarrierProof, Fault, ToolControl, WorkIdentity,
};
use lash_upgrade_harness::e2e::evidence::{DecodedRecord, Evidence};
use lash_upgrade_harness::e2e::host::{HostAdapter as _, HostCommand, HostKind};
use lash_upgrade_harness::e2e::provider::ProviderKind;
use serde_json::json;

h2_case!(
    s12_deferred_survives_removal_of_n,
    RetirePending,
    SqliteFile,
    Live
);
h2_case!(
    s23_cancel_between_capture_and_adoption,
    CancelAtCapture,
    SqliteFile,
    Live
);
h2_case!(
    s23_cancel_after_adoption,
    CancelAfterAdoption,
    SqliteFile,
    Live
);
h2_case!(
    s31_publication_crash_hands_over_once,
    PublicationCrash,
    SqliteFile,
    Live
);
h2_case!(
    s32_missing_retained_material_refuses_without_body_replay,
    RetainedRemoval,
    SqliteFile,
    Live
);

pub fn spec(id: &str, store: StoreKind, artifacts: Vec<ArtifactIdentity>) -> Result<CaseSpec> {
    let rules = match id {
        "S12" => vec!["L07", "L09", "L11"],
        "S23" => vec!["L10"],
        "S31" => vec!["L09", "L13", "L16"],
        "S32" => vec!["L12", "L13"],
        _ => return Err(anyhow!("unknown H3 transfer scenario {id}")),
    };
    ensure!(
        store != StoreKind::SqliteMemory,
        "two generations share one persistent store"
    );
    Ok(CaseSpec {
        id: id.to_owned(),
        rules: rules.into_iter().map(str::to_owned).collect(),
        // B01–B04, D01/D02, O02 and F02 are landed in the candidate baseline.
        requires: Vec::new(),
        host: HostKind::Workbench,
        store,
        channel: Channel::Rlm,
        provider: ProviderKind::Scripted,
        restate_nodes: 1,
        artifacts,
        cuts: Vec::new(),
        expected_terminal: match id {
            "S23" => "Cancelled",
            "S32" => "retained_result_refused",
            _ => "Answered",
        }
        .to_owned(),
    })
}

/// Journal provenance is mandatory: an adapter's reconstructed trace is not X/D/V.
fn records<'a>(evidence: &'a Evidence, work: &WorkIdentity) -> Result<Vec<&'a RunJournalEntry>> {
    let facts: Vec<_> = evidence
        .journals
        .iter()
        .filter(|fact| fact.work.run == work.run)
        .collect();
    ensure!(!facts.is_empty(), "{} has no journal evidence", work.run);
    ensure!(
        facts.iter().all(|fact| !fact.admin_url.is_empty()
            && !fact.invocation.is_empty()
            && fact.protocol == 7),
        "journal lacks real admin/invocation/V7 provenance"
    );
    let records: Vec<_> = facts
        .into_iter()
        .filter_map(|fact| match &fact.decoded {
            Some(DecodedRecord::Run(entry)) => Some(entry),
            _ => None,
        })
        .collect();
    ensure!(
        !records.is_empty(),
        "{} has no independently decoded Run record",
        work.run
    );
    Ok(records)
}

/// The canonical transfer lives in the fenced session head, not in a Restate
/// Run completion. Read the retained follow-on artifact independently.
fn transfer(evidence: &Evidence, work: &WorkIdentity) -> Result<RunTransfer> {
    let matches: Vec<RunTransfer> = evidence
        .transfers
        .iter()
        .filter(|fact| fact.work.run == work.run)
        .map(|fact| {
            let snapshot: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&fact.artifact)?)?;
            let key: serde_json::Value =
                serde_json::from_str(snapshot["turn_id"].as_str().unwrap_or_default())?;
            ensure!(
                key.pointer("/scope/turn_id") == Some(&json!(work.run)),
                "retained artifact belongs to a different Run"
            );
            let value = snapshot
                .pointer("/receipt/pending_follow_on/owes/opener/run")
                .ok_or_else(|| anyhow!("store artifact has no canonical transfer"))?;
            let actual: RunTransfer = serde_json::from_value(value.clone())?;
            ensure!(
                actual == fact.transfer,
                "typed transfer differs from store artifact"
            );
            Ok(actual)
        })
        .collect::<Result<_>>()?;
    ensure!(
        !matches.is_empty(),
        "{} has no retained follow-on store receipt",
        work.run
    );
    let first = &matches[0];
    ensure!(
        matches.iter().all(|value| value == first),
        "retained transfer observations disagree"
    );
    first.ledger()?;
    Ok(first.clone())
}

/// Store publication cuts retain an independently read fenced-store receipt.
/// They cannot substitute a store revision for a Restate journal index.
fn store_cut(proof: &BarrierProof, kind: BarrierKind) -> Result<()> {
    ensure!(
        proof.barrier.kind == kind
            && kind.durable()
            && !kind.journal_backed()
            && proof.journal_index.is_none(),
        "publication cut lacks store provenance"
    );
    let snapshot: serde_json::Value = serde_json::from_slice(&std::fs::read(&proof.artifact)?)?;
    ensure!(
        snapshot.is_object(),
        "store publication receipt is not an object"
    );
    Ok(())
}

fn source_work(scenario: &Scenario<'_>, call: &ToolCallId) -> Result<WorkIdentity> {
    let mut work = scenario.work()?.clone();
    work.call = Some(call.to_string());
    work.ordinal = Some(1);
    Ok(work)
}

/// Release a Deferred body and prove its acknowledged descriptor is durable.
async fn park_source(scenario: &mut Scenario<'_>, source: &ToolCallId) -> Result<()> {
    scenario
        .release(scenario.barrier(source, BarrierKind::BodyEntered)?)
        .await?;
    scenario
        .wait(scenario.barrier(source, BarrierKind::XDurable)?)
        .await?;
    let parked = scenario.read().await?;
    ensure!(
        attempts(&parked)
            .iter()
            .any(|entry| &entry.call_id == source
                && matches!(
                    entry.result,
                    AttemptResult::Pending { .. } | AttemptResult::Deferred { .. }
                )),
        "source has no acknowledged Deferred descriptor"
    );
    Ok(())
}

/// Request the production generation drain. With `gate`, its body is held
/// across the request and then released: the gate's consumption is the Run's
/// next recorded cut check. Without one, the Run is parked on its source wait.
async fn hand_over(
    scenario: &mut Scenario<'_>,
    gate: Option<&ToolCallId>,
) -> Result<(BarrierProof, Evidence)> {
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
    if let Some(gate) = gate {
        scenario
            .release(scenario.barrier(gate, BarrierKind::BodyEntered)?)
            .await?;
    }
    let published = scenario
        .wait_owner(BarrierKind::ContinuationPublished)
        .await?;
    store_cut(&published, BarrierKind::ContinuationPublished)?;
    let transferred = scenario.read().await?;
    let cut = transfer(&transferred, scenario.work()?)?;
    cut.check_capture(lash_core::tool_run::CutPhase::Capturable)?;
    Ok((published, transferred))
}

/// The S12/S23 program: `gate` returns inline, then the VM awaits a Deferred
/// `source`. Returns both call identities once the source is parked.
async fn gate_then_source(
    scenario: &mut Scenario<'_>,
    initial: &Evidence,
) -> Result<(ToolCallId, ToolCallId)> {
    let gate = calls(initial, &["gate"])?["gate"].clone();
    scenario
        .release(scenario.barrier(&gate, BarrierKind::BodyEntered)?)
        .await?;
    scenario
        .host
        .command(HostCommand::Process {
            action: "await-tool-bodies".into(),
            input: json!({"run":scenario.work()?.run,"labels":["source"]}),
        })
        .await?;
    let progressed = scenario.read().await?;
    let source = calls(&progressed, &["gate", "source"])?["source"].clone();
    park_source(scenario, &source).await?;
    Ok((gate, source))
}

/// The S11-shaped program S23/S31/S32 share: an inline `winner` races a
/// Deferred `source`, then the VM awaits `gate`. The source stays pending as
/// the race's loser; the held gate's consumption is where the drain cuts.
async fn race_then_gate(
    scenario: &mut Scenario<'_>,
    source: &str,
) -> Result<(ToolCallId, ToolCallId, ToolCallId)> {
    scenario
        .host
        .command(HostCommand::Process {
            action: "await-tool-bodies".into(),
            input: json!({"run":scenario.work()?.run,"labels":["winner",source]}),
        })
        .await?;
    let initial = scenario.read().await?;
    let ids = calls(&initial, &["winner", source])?;
    let (winner, pending) = (ids["winner"].clone(), ids[source].clone());
    park_source(scenario, &pending).await?;
    scenario
        .release(scenario.barrier(&winner, BarrierKind::BodyEntered)?)
        .await?;
    scenario
        .host
        .command(HostCommand::Process {
            action: "await-tool-bodies".into(),
            input: json!({"run":scenario.work()?.run,"labels":["gate"]}),
        })
        .await?;
    let progressed = scenario.read().await?;
    let gate = calls(&progressed, &["winner", source, "gate"])?["gate"].clone();
    ensure!(
        !events(&progressed)?.iter().any(|event| matches!(event,
            RunEvent::Decided { call_id, .. } if call_id == &pending)),
        "the race decided its Deferred loser"
    );
    Ok((winner, pending, gate))
}

/// S23's program awaits a fresh Deferred `later` after the cut's gate: park
/// it, so the owner that resumed the VM stays live on its own wait.
async fn await_later(scenario: &mut Scenario<'_>) -> Result<ToolCallId> {
    scenario
        .host
        .command(HostCommand::Process {
            action: "await-tool-bodies".into(),
            input: json!({"run":scenario.work()?.run,"labels":["later"]}),
        })
        .await?;
    let progressed = scenario.read().await?;
    let later = calls(&progressed, &["winner", "source", "gate", "later"])?["later"].clone();
    park_source(scenario, &later).await?;
    Ok(later)
}

/// An RLM program answers with `finish(value)`: the follow's typed final value.
pub(super) fn assert_final_value(evidence: &Evidence, expected: &str) -> Result<()> {
    let outcome = evidence
        .outputs
        .last()
        .and_then(|output| output.output.pointer("/outcome/report/outcome"))
        .context("follow has no typed terminal report")?;
    ensure!(
        outcome.pointer("/finish") == Some(&json!({"type":"final_value","value":expected})),
        "the Run did not finish with {expected:?}: {outcome}"
    );
    Ok(())
}

fn generation(receipt: Option<&serde_json::Value>) -> Result<String> {
    receipt
        .and_then(|receipt| receipt["generation"].as_str())
        .map(str::to_owned)
        .context("no accepted generation drain receipt")
}

/// The segments a successor runs the transferred Run under.
async fn successor_segments(shared: &Shared, work: &WorkIdentity) -> Result<Vec<Segment>> {
    Ok(shared
        .segments(&work.run)
        .await?
        .into_iter()
        .filter(|segment| segment.target_service_key.contains(&transferred(&work.run)))
        .collect())
}

/// S12: N drains and is removed non-forcibly, by the production drain report,
/// while N+1's transferred source is still unresolved; the source then seals
/// once and wakes N+1 with the exact value.
pub async fn retire_pending(
    scenario: &mut Scenario<'_>,
    shared: &Shared,
    spec: &CaseSpec,
) -> Result<Evidence> {
    let initial = scenario.start(spec).await?;
    let (gate, source) = gate_then_source(scenario, &initial).await?;
    let (gate, source) = (&gate, &source);
    let (_, transferred) = hand_over(scenario, None).await?;
    let work = scenario.work()?.clone();
    let cut = transfer(&transferred, &work)?;
    let admitted = scenario.wait_owner(BarrierKind::SuccessorAdmitted).await?;
    store_cut(&admitted, BarrierKind::SuccessorAdmitted)?;
    let old = generation(shared.generation_drain.lock().await.as_ref())?;
    let retired = scenario
        .control
        .inject(Fault::DrainAndRetire { generation: old }, &admitted)
        .await?;
    scenario.faults.push(retired);
    let unresolved = scenario.read().await?;
    ensure!(
        !events(&unresolved)?
            .iter()
            .any(|event| matches!(event, RunEvent::Decided { call_id, .. } if call_id == source)),
        "the source was decided before N was retired"
    );
    scenario
        .control
        .tool(ToolControl::Resolve {
            work: source_work(scenario, source)?,
            value: json!("retained"),
        })
        .await?;
    let evidence = scenario.finish(RemoteTurnStatus::Answered, None).await?;
    assert_final_value(&evidence, "gate|retained")?;
    assert_s12(&evidence, &work, &cut, source)?;
    let successors = successor_segments(shared, &work).await?;
    ensure!(
        successors.len() == 1,
        "the transfer needs exactly one successor segment: {successors:?}"
    );
    ensure!(
        evidence
            .journals
            .iter()
            .any(|fact| fact.invocation == successors[0].id
                && matches!(fact.decoded, Some(DecodedRecord::Run(_)))),
        "N+1 recorded no Run fact of its own after it woke"
    );
    for call in [source, gate] {
        assert_call(&evidence, call, LogicalTerminal::Final)?;
        assert_body_identity(&evidence, call, Some(1))?;
    }
    Ok(evidence)
}

fn assert_s12(
    evidence: &Evidence,
    work: &WorkIdentity,
    transfer: &RunTransfer,
    source: &ToolCallId,
) -> Result<()> {
    ensure!(
        !transfer.sources.is_empty() && !transfer.subscriptions.is_empty(),
        "cut lost pending source"
    );
    ensure!(
        transfer
            .sources
            .iter()
            .any(|descriptor| &descriptor.call_id == source),
        "cut did not carry the unresolved source"
    );
    ensure!(
        transfer
            .sources
            .iter()
            .all(|source| source.owner == transfer.owner),
        "pending source authority drifted"
    );
    ensure!(
        evidence
            .faults
            .iter()
            .any(|fault| matches!(fault.fault, Fault::DrainAndRetire { .. })
                && fault.proof.barrier.kind == BarrierKind::SuccessorAdmitted),
        "predecessor was never retired after adoption"
    );
    records(evidence, work)?;
    let decisions = events(evidence)?
        .into_iter()
        .filter(|event| matches!(event, RunEvent::Decided { call_id, .. } if call_id == source))
        .count();
    ensure!(decisions == 1, "source has {decisions} decisions");
    Ok(())
}

#[derive(Clone, Copy, Debug)]
pub enum Cut {
    CaptureToAdoption,
    PostAdoption,
}

/// S23: cancel the transferred Run at the named cut, then submit identical
/// input. The fresh Run executes from scratch and inherits nothing.
pub async fn cancel_then_fresh(
    scenario: &mut Scenario<'_>,
    shared: &Shared,
    spec: &CaseSpec,
    cut: Cut,
) -> Result<Evidence> {
    scenario.start(spec).await?;
    let (_, _, gate) = race_then_gate(scenario, "source").await?;
    let old = scenario.work()?.clone();
    let mut owner = old.clone();
    owner.call = None;
    owner.ordinal = None;
    let starting = Barrier {
        work: owner,
        kind: BarrierKind::SuccessorStarting,
    };
    if matches!(cut, Cut::CaptureToAdoption) {
        scenario
            .control
            .tool(ToolControl::Hold(starting.clone()))
            .await?;
    }
    let (_, transferred) = hand_over(scenario, Some(&gate)).await?;
    let captured = transfer(&transferred, &old)?;
    match cut {
        Cut::CaptureToAdoption => {
            scenario.wait(starting.clone()).await?;
            let held = successor_segments(shared, &old).await?;
            ensure!(
                held.iter()
                    .all(|segment| segment.status != "suspended" && segment.status != "completed"),
                "successor adopted while its Start was held: {held:?}"
            );
        }
        Cut::PostAdoption => {
            // N+1 resumes the VM, which parks on its own new `later` wait.
            await_later(scenario).await?;
            let admitted = scenario.wait_owner(BarrierKind::SuccessorAdmitted).await?;
            store_cut(&admitted, BarrierKind::SuccessorAdmitted)?;
        }
    }
    scenario.cancel().await?;
    if matches!(cut, Cut::CaptureToAdoption) {
        scenario.release(starting).await?;
    }
    let mut cancelled = scenario.finish(RemoteTurnStatus::Cancelled, None).await?;
    cancelled.stores.push(shared.run_snapshot(&old.run).await?);
    let fresh = scenario
        .host
        .command(HostCommand::Submit {
            session: format!("{}-{}", scenario.lease.namespace, spec.id.to_lowercase()),
            idempotency_key: format!("{}:{}:fresh", scenario.lease.gate_id, spec.id),
            input: json!({"text": spec.id}),
        })
        .await?;
    ensure!(
        fresh.work.run != old.run && !fresh.work.run.is_empty(),
        "new input inherited the cancelled Run"
    );
    scenario.work = Some(fresh.work.clone());
    scenario.wait_owner(BarrierKind::AdmissionDurable).await?;
    let (fresh_winner, fresh_source, fresh_gate) = race_then_gate(scenario, "source").await?;
    scenario
        .release(scenario.barrier(&fresh_gate, BarrierKind::BodyEntered)?)
        .await?;
    let fresh_later = await_later(scenario).await?;
    scenario
        .control
        .tool(ToolControl::Resolve {
            work: source_work(scenario, &fresh_later)?,
            value: json!("fresh"),
        })
        .await?;
    let finished = scenario.finish(RemoteTurnStatus::Answered, None).await?;
    assert_final_value(&finished, "winner|gate|fresh")?;
    assert_s23(&cancelled, &finished, &old, &fresh.work, &captured.owner)?;
    for call in [&fresh_winner, &fresh_gate, &fresh_later] {
        assert_call(&finished, call, LogicalTerminal::Final)?;
    }
    for call in [&fresh_winner, &fresh_source, &fresh_gate, &fresh_later] {
        assert_body_identity(&finished, call, Some(1))?;
    }
    let old_invocations: Vec<_> = cancelled.journals.iter().map(|f| &f.invocation).collect();
    ensure!(
        !finished
            .journals
            .iter()
            .any(|fact| old_invocations.contains(&&fact.invocation)),
        "the fresh Run executed in a cancelled Run's invocation"
    );
    let mut evidence = finished;
    evidence.stores.extend(cancelled.stores);
    evidence.transfers.extend(cancelled.transfers);
    evidence.outputs.extend(cancelled.outputs);
    Ok(evidence)
}

/// Actual store terminal and continuation observations are required in
/// addition to D. A cancelled call inside a live owner is not Run cancellation.
fn assert_s23(
    old_evidence: &Evidence,
    fresh_evidence: &Evidence,
    old: &WorkIdentity,
    fresh: &WorkIdentity,
    old_owner: &lash_core::EffectOpener,
) -> Result<()> {
    ensure!(
        old.run != fresh.run,
        "fresh input reused the old logical owner"
    );
    let snapshots: Vec<_> = old_evidence
        .stores
        .iter()
        .filter(|snapshot| snapshot["run"].as_str() == Some(old.run.as_str()))
        .collect();
    ensure!(
        !snapshots.is_empty(),
        "cancelled owner has no authoritative store snapshot"
    );
    for snapshot in snapshots {
        let terminal: lash_core::store::RunTerminal =
            serde_json::from_value(snapshot["terminal"].clone())?;
        ensure!(
            terminal.run.as_str() == old.run
                && terminal.kind() == lash_core::store::RunTerminalKind::Cancelled,
            "old owner did not end cancelled"
        );
        ensure!(
            snapshot["continuation"].is_null() && snapshot["unfinished"] == false,
            "old owner retained a continuation/admission"
        );
    }
    let cut = transfer(old_evidence, old)?;
    ensure!(
        &cut.owner == old_owner,
        "cut lost the captured owner's authority"
    );
    let fresh_records = records(fresh_evidence, fresh)?;
    for event in fresh_records.iter().flat_map(|entry| &entry.record.events) {
        if let RunEvent::Admitted { round } = event {
            for call in &round.members {
                ensure!(
                    !cut.ledger()?.has_call(&call.call_id),
                    "new Run inherited an old admitted call"
                );
                ensure!(
                    call.request.owner
                        != lash_core::tool_run::MaterialOwner::Run {
                            opener: old_owner.clone()
                        },
                    "new Run inherited old prepared material"
                );
            }
        }
    }
    Ok(())
}

/// S31: SIGKILL the predecessor after the successor publication and before it
/// discharged: its last command before Output is held on its own transport.
/// N's process returns under its registered deployment and replays the cut;
/// exactly one successor adopts and finishes the logical Run.
pub async fn publication_crash(
    scenario: &mut Scenario<'_>,
    shared: &Shared,
    spec: &CaseSpec,
) -> Result<Evidence> {
    scenario.start(spec).await?;
    let (winner, loser, gate) = race_then_gate(scenario, "loser").await?;
    let work = scenario.work()?.clone();
    let mut owner = work.clone();
    owner.call = None;
    owner.ordinal = None;
    let discharge = Barrier {
        work: owner,
        kind: BarrierKind::PredecessorDischarged,
    };
    scenario
        .control
        .tool(ToolControl::Hold(discharge.clone()))
        .await?;
    let (published, transferred) = hand_over(scenario, Some(&gate)).await?;
    let cut = transfer(&transferred, &work)?;
    scenario.wait(discharge.clone()).await?;
    let predecessor = shared
        .segments(&work.run)
        .await?
        .into_iter()
        .find(|segment| segment.id == work.segment)
        .context("predecessor segment is absent")?;
    ensure!(
        predecessor.status != "completed",
        "the predecessor discharged before the publication crash: {predecessor:?}"
    );
    ensure!(
        successor_segments(shared, &work).await?.is_empty(),
        "a successor started before the predecessor sent its start"
    );
    let ready = scenario.ready.clone().context("host is not ready")?;
    let killed = scenario
        .control
        .inject(
            Fault::KillHost {
                target: ready.process.role.clone(),
            },
            &published,
        )
        .await?;
    ensure!(
        killed.target_incarnation == ready.process.incarnation,
        "host kill hit another incarnation"
    );
    scenario.faults.push(killed);
    let restarted = shared
        .host
        .lock()
        .await
        .command(HostCommand::Process {
            action: "restart-in-place".into(),
            input: json!({}),
        })
        .await?;
    let reopened: lash_upgrade_harness::e2e::host::HostReady =
        serde_json::from_value(restarted.output)?;
    ensure!(
        reopened.process.pid != ready.process.pid
            && reopened.process.incarnation > ready.process.incarnation,
        "cold restart did not replace the killed predecessor"
    );
    scenario.ready = Some(reopened);
    // Restate pauses an invocation whose deployment stopped answering; the
    // operator resumes it once the build serves again.
    if shared
        .segments(&work.run)
        .await?
        .iter()
        .any(|segment| segment.id == work.segment && segment.status == "paused")
    {
        shared.view.resume(&work.segment).await?;
    }
    scenario.release(discharge).await?;
    let evidence = scenario.finish(RemoteTurnStatus::Answered, None).await?;
    assert_final_value(&evidence, "winner|gate")?;
    assert_s31(&evidence, &work, &cut)?;
    let successors = successor_segments(shared, &work).await?;
    let deployment = shared.successor_deployment().await?;
    ensure!(
        successors.len() == 1
            && successors[0].pinned_deployment_id.as_deref() == Some(deployment.id.as_str()),
        "the publication crash produced other than one successor on N+1: {successors:?}"
    );
    for call in [&winner, &gate] {
        assert_call(&evidence, call, LogicalTerminal::Final)?;
    }
    for call in [&winner, &loser, &gate] {
        assert_body_identity(&evidence, call, Some(1))?;
    }
    Ok(evidence)
}

/// L09/L13/L16: the OS exit receipt never substitutes for a logical close.
fn assert_s31(evidence: &Evidence, work: &WorkIdentity, cut: &RunTransfer) -> Result<()> {
    ensure!(
        !cut.sources.is_empty() && !cut.material.is_empty(),
        "process lost pending source or retained material"
    );
    ensure!(
        cut.environment.is_some() && cut.ledger()?.held_calls() > 0,
        "process lost admitted environment/capacity"
    );
    ensure!(
        cut.entries
            .iter()
            .flat_map(|entry| &entry.record.events)
            .all(|event| !matches!(
                event,
                RunEvent::Lifecycle {
                    state: lash_core::tool_run::RunLifecycle::Closing
                }
            )),
        "physical process cut closed the logical Run"
    );
    ensure!(
        evidence
            .faults
            .iter()
            .any(|fault| matches!(fault.fault, Fault::KillHost { .. })
                && fault.proof.barrier.kind == BarrierKind::ContinuationPublished),
        "publication crash was missed"
    );
    records(evidence, work)?;
    Ok(())
}

/// S32: publish a real retained result, then remove its stored artifact
/// while the successor's Start is held. The successor's adoption and a cold
/// follow refuse typed; no body runs again and no new identity appears.
pub async fn remove_retained(
    scenario: &mut Scenario<'_>,
    shared: &Shared,
    spec: &CaseSpec,
) -> Result<Evidence> {
    scenario.start(spec).await?;
    let (winner, source, gate) = race_then_gate(scenario, "source").await?;
    let work = scenario.work()?.clone();
    let mut owner = work.clone();
    owner.call = None;
    owner.ordinal = None;
    let starting = Barrier {
        work: owner,
        kind: BarrierKind::SuccessorStarting,
    };
    scenario
        .control
        .tool(ToolControl::Hold(starting.clone()))
        .await?;
    let (_, transferred) = hand_over(scenario, Some(&gate)).await?;
    let cut = transfer(&transferred, &work)?;
    ensure!(
        !cut.material.is_empty()
            && cut
                .material
                .iter()
                .all(|bundle| bundle.held_by(cut.holder()).is_retained()),
        "the cut published no retained owner-qualified material: {:?}",
        cut.material
    );
    ensure!(
        !cut.material_aliases.is_empty(),
        "the cut published no owner-qualified reference"
    );
    scenario.wait(starting.clone()).await?;
    let artifacts: Vec<String> = cut
        .material
        .iter()
        .map(|bundle| bundle.artifact.artifact_ref.clone())
        .collect();
    let readable = material_rows(shared, &artifacts).await?;
    ensure!(
        readable
            .iter()
            .all(|row| row["bundle"] == true && row["leases"].as_u64() > Some(0)),
        "required bytes were not retained before the last dependency ended: {readable:?}"
    );
    let removed = remove_material(shared, &artifacts).await?;
    scenario.release(starting).await?;
    let outcome = tokio::time::timeout_at(
        scenario.lease.deadline.into(),
        scenario.host.command(HostCommand::Attach {
            run: work.run.clone(),
        }),
    )
    .await
    .context("cold follow never settled after the retained artifact was removed")??;
    super::write(&scenario.lease.directory.join("cold-follow.json"), &outcome)?;
    let mut evidence = scenario.read().await?;
    // The follow answers with the engine's typed refusal, never a body rerun.
    let refusal = &outcome.output["outcome"];
    ensure!(
        refusal["type"] == "refused" && refusal["input_id"] == json!(work.ingress),
        "missing retained material did not refuse the follow: {refusal}"
    );
    let error: lash_core::RuntimeError = serde_json::from_value(refusal["error"].clone())?;
    ensure!(
        error.code == lash_core::RuntimeErrorCode::RetainedResultRefused,
        "the refusal is not a typed retained-result refusal: {error:?}"
    );
    for call in [&winner, &source, &gate] {
        assert_body_identity(&evidence, call, Some(1))?;
    }
    // Every later segment is the same logical Run: the follow found no other.
    let successors = successor_segments(shared, &work).await?;
    evidence
        .stores
        .push(json!({"kind":"h3_retained_material","before":readable,
        "removal":removed,"successor_segments":successors}));
    evidence.outputs.push(outcome);
    evidence.barriers.extend(scenario.proofs.clone());
    evidence.faults.extend(scenario.faults.clone());
    Ok(evidence)
}

fn store_path(shared: &Shared) -> std::path::PathBuf {
    shared.store_root.join("durable-core.db")
}

/// Each retained bundle's pointer and its lease edges, read from the store.
async fn material_rows(shared: &Shared, artifacts: &[String]) -> Result<Vec<serde_json::Value>> {
    let path = store_path(shared);
    let artifacts = artifacts.to_vec();
    tokio::task::spawn_blocking(move || -> Result<Vec<serde_json::Value>> {
        let db = rusqlite::Connection::open_with_flags(
            path,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        artifacts
            .iter()
            .map(|artifact| {
                let bundle: i64 = db.query_row(
                    "SELECT COUNT(*) FROM artifact_refs WHERE namespace='tool_material' AND artifact_ref=?1",
                    [artifact],
                    |row| row.get(0),
                )?;
                let leases: i64 = db.query_row(
                    "SELECT COUNT(*) FROM artifact_referrer_edges WHERE namespace='tool_material' AND artifact_ref=?1",
                    [artifact],
                    |row| row.get(0),
                )?;
                Ok(json!({"artifact":artifact,"bundle":bundle == 1,"leases":leases}))
            })
            .collect()
    })
    .await?
}

/// Remove each retained bundle's stored pointer; its leases stay recorded.
async fn remove_material(shared: &Shared, artifacts: &[String]) -> Result<serde_json::Value> {
    let path = store_path(shared);
    let artifacts = artifacts.to_vec();
    tokio::task::spawn_blocking(move || -> Result<serde_json::Value> {
        let db = rusqlite::Connection::open(path)?;
        let mut removed = Vec::new();
        for artifact in &artifacts {
            let rows = db.execute(
                "DELETE FROM artifact_refs WHERE namespace='tool_material' AND artifact_ref=?1",
                [artifact],
            )?;
            ensure!(rows == 1, "retained bundle {artifact} was not stored");
            removed.push(artifact.clone());
        }
        Ok(json!({"kind":"h3_retained_material_removed","removed":removed}))
    })
    .await?
}
