//! S12/S13/S23/S31/S32: independently read transfer and drain receipts.
use anyhow::{Result, ensure};
use lash_core::tool_run::{RunEvent, RunJournalEntry, RunTransfer};
use lash_upgrade_harness::e2e::{
    case::{ArtifactIdentity, CaseSpec, Channel, StoreKind},
    control::{Barrier, BarrierKind, Control, Fault, ToolControl, WorkIdentity},
    evidence::{DecodedRecord, Evidence},
    host::{HostAdapter, HostCommand, HostKind},
    provider::ProviderKind,
};

/// The catalogue is shared with H8. Final-routing claims are explicitly held
/// by the deletion units, rather than inferred from a successful business reply.
pub fn specs(store: StoreKind, artifacts: Vec<ArtifactIdentity>) -> Vec<CaseSpec> {
    [
        (
            "S12",
            &["L07", "L09", "L11"][..],
            &["FIG-4891", "FIG-1863"][..],
        ),
        ("S13", &["L11", "L21"][..], &["FIG-4900"][..]),
        ("S23", &["L10"][..], &["FIG-4893", "FIG-1863"][..]),
        (
            "S31",
            &["L09", "L13", "L16", "L22"][..],
            &["FIG-4890", "FIG-4889", "FIG-1863"][..],
        ),
        ("S32", &["L12", "L13"][..], &["FIG-4889"][..]),
    ]
    .into_iter()
    .map(|(id, rules, requires)| CaseSpec {
        id: id.into(),
        rules: rules.iter().map(|rule| (*rule).into()).collect(),
        host: HostKind::UpgradeNode,
        store: store.clone(),
        channel: Channel::Rlm,
        provider: ProviderKind::Scripted,
        restate_nodes: 1,
        artifacts: artifacts.clone(),
        cuts: Vec::new(),
        expected_terminal: if id == "S32" {
            "retained_result_refused"
        } else {
            "settled"
        }
        .into(),
        requires: requires.iter().map(|unit| (*unit).into()).collect(),
    })
    .collect()
}

/// Journal provenance is mandatory: an adapter's reconstructed trace is not X/D/V.
pub fn records<'a>(
    evidence: &'a Evidence,
    work: &WorkIdentity,
) -> Result<Vec<&'a RunJournalEntry>> {
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

/// The canonical transfer lives in the fenced session head, not necessarily
/// in a Restate Run completion. Read the retained follow-on independently.
pub fn transfer(evidence: &Evidence, work: &WorkIdentity) -> Result<RunTransfer> {
    let matches: Vec<RunTransfer> = evidence
        .stores
        .iter()
        .filter(|snapshot| snapshot["run"].as_str() == Some(work.run.as_str()))
        .filter(|snapshot| {
            snapshot["store_source"].as_str()
                == Some("deployment_store.session_head.pending_follow_on_json")
        })
        .filter_map(|snapshot| snapshot.pointer("/continuation/continuation/opener/run"))
        .filter(|value| !value.is_null())
        .map(|value| serde_json::from_value(value.clone()))
        .collect::<Result<_, _>>()?;
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

/// Kill only after the independently proved successor-publication cut. The
/// source is still unresolved; its resolve command belongs after retirement.
pub async fn s12_retire_pending_source(
    host: &mut dyn HostAdapter,
    control: &mut dyn Control,
    evidence: &mut Evidence,
    work: WorkIdentity,
    old_generation: String,
    value: serde_json::Value,
) -> Result<()> {
    host.command(HostCommand::Transfer {
        run: work.run.clone(),
    })
    .await?;
    let barrier = Barrier {
        work: work.clone(),
        kind: BarrierKind::SuccessorAdmitted,
    };
    let proof = control.await_barrier(&barrier).await?;
    ensure!(
        proof.barrier == barrier && proof.journal_index.is_some(),
        "missing durable successor barrier"
    );
    let retired = control
        .inject(
            Fault::DrainAndRetire {
                generation: old_generation,
            },
            &proof,
        )
        .await?;
    evidence.barriers.push(proof);
    evidence.faults.push(retired);
    control
        .tool(ToolControl::Resolve {
            work: work.clone(),
            value,
        })
        .await?;
    let terminal = host
        .command(HostCommand::Attach {
            run: work.run.clone(),
        })
        .await?;
    ensure!(terminal.work.run == work.run, "source woke a different Run");
    evidence.outputs.push(terminal);
    Ok(())
}

pub fn assert_s12(
    evidence: &Evidence,
    work: &WorkIdentity,
    expected: &serde_json::Value,
) -> Result<()> {
    let transfer = transfer(evidence, work)?;
    ensure!(
        !transfer.sources.is_empty() && !transfer.subscriptions.is_empty(),
        "cut lost pending source"
    );
    ensure!(
        transfer
            .sources
            .iter()
            .all(|source| source.owner == transfer.owner)
            && transfer
                .subscriptions
                .iter()
                .all(|subscription| subscription.owner == transfer.owner),
        "pending source authority drifted"
    );
    ensure!(
        evidence
            .faults
            .iter()
            .any(|fault| matches!(fault.fault, Fault::DrainAndRetire { .. })),
        "predecessor was never retired"
    );
    let records = records(evidence, work)?;
    let call = work
        .call
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("source has no call identity"))?;
    let decisions = records
        .iter()
        .flat_map(|entry| &entry.record.events)
        .filter(
            |event| matches!(event, RunEvent::Decided { call_id, .. } if call_id.as_str() == call),
        )
        .count();
    ensure!(decisions == 1, "source has {decisions} decisions");
    ensure!(
        evidence
            .outputs
            .iter()
            .any(|output| output.work.run == work.run && &output.output == expected),
        "successor did not return the exact retained value"
    );
    Ok(())
}

/// L09/L13/L16: the OS exit receipt never substitutes for a logical close.
pub fn assert_s31(evidence: &Evidence, work: &WorkIdentity) -> Result<()> {
    let cut = transfer(evidence, work)?;
    ensure!(
        cut.vm_continuation && !cut.sources.is_empty() && !cut.material.is_empty(),
        "process lost VM, pending source or retained material"
    );
    ensure!(
        cut.environment.is_some() && cut.reserved_calls > 0,
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
    Ok(())
}

/// S23 runs at both capture→adoption and post-adoption cuts. A cancellation
/// must finish the old owner before identical input can admit a fresh owner.
pub async fn s23_cancel_then_fresh_input(
    host: &mut dyn HostAdapter,
    control: &mut dyn Control,
    evidence: &mut Evidence,
    work: WorkIdentity,
    cut: BarrierKind,
    session: String,
    input: serde_json::Value,
) -> Result<WorkIdentity> {
    ensure!(
        matches!(
            cut,
            BarrierKind::ContinuationPublished | BarrierKind::SuccessorAdmitted
        ),
        "S23 requires a capture or adoption cut"
    );
    let barrier = Barrier {
        work: work.clone(),
        kind: cut,
    };
    control.tool(ToolControl::Hold(barrier.clone())).await?;
    host.command(HostCommand::Transfer {
        run: work.run.clone(),
    })
    .await?;
    let proof = control.await_barrier(&barrier).await?;
    ensure!(proof.journal_index.is_some(), "S23 cut is not durable");
    evidence.barriers.push(proof);
    let cancel = host
        .command(HostCommand::Cancel {
            run: work.run.clone(),
        })
        .await?;
    ensure!(cancel.work.run == work.run, "cancellation changed owner");
    evidence.outputs.push(cancel);
    control.tool(ToolControl::Release(barrier)).await?;
    let old = host
        .command(HostCommand::Attach {
            run: work.run.clone(),
        })
        .await?;
    ensure!(
        old.work.run == work.run,
        "old continuation attached a different Run"
    );
    evidence.outputs.push(old);
    let fresh = host
        .command(HostCommand::Submit {
            session,
            idempotency_key: "s23-fresh-input".into(),
            input,
        })
        .await?;
    ensure!(
        fresh.work.run != work.run && !fresh.work.run.is_empty(),
        "new input inherited the cancelled Run"
    );
    let fresh_work = fresh.work.clone();
    evidence.outputs.push(fresh);
    Ok(fresh_work)
}

/// Actual store terminal and continuation observations are required in
/// addition to D. A cancelled call inside a live owner is not Run cancellation.
pub fn assert_s23(
    evidence: &Evidence,
    old: &WorkIdentity,
    fresh: &WorkIdentity,
    old_owner: &lash_core::EffectOpener,
) -> Result<()> {
    ensure!(
        old.run != fresh.run,
        "fresh input reused the old logical owner"
    );
    let snapshots: Vec<_> = evidence
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
    let cut = transfer(evidence, old)?;
    ensure!(
        &cut.owner == old_owner,
        "cut lost the captured owner's authority"
    );
    let fresh_records = records(evidence, fresh)?;
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

/// S31 kills the actual owned host only after the successor publication is
/// independently decoded. Resolution belongs to the restarted successor.
pub async fn s31_publication_crash(
    host: &mut dyn HostAdapter,
    control: &mut dyn Control,
    evidence: &mut Evidence,
    work: WorkIdentity,
    predecessor: String,
) -> Result<()> {
    let barrier = Barrier {
        work: work.clone(),
        kind: BarrierKind::ContinuationPublished,
    };
    control.tool(ToolControl::Hold(barrier.clone())).await?;
    host.command(HostCommand::Transfer {
        run: work.run.clone(),
    })
    .await?;
    let proof = control.await_barrier(&barrier).await?;
    ensure!(
        proof.journal_index.is_some(),
        "process cut has no publication journal"
    );
    let receipt = control
        .inject(
            Fault::KillHost {
                target: predecessor,
            },
            &proof,
        )
        .await?;
    ensure!(
        receipt.proof.barrier == barrier,
        "process died at a different cut"
    );
    evidence.barriers.push(proof);
    evidence.faults.push(receipt);
    Ok(())
}

/// S32 corrupts the real artifact only after a successor acquired it. A cold
/// follow must refuse typed; the independent body ledger must remain unchanged.
pub async fn s32_remove_retained_result(
    host: &mut dyn HostAdapter,
    control: &mut dyn Control,
    evidence: &mut Evidence,
    work: WorkIdentity,
    reference: lash_core::tool_run::MaterialRef,
) -> Result<()> {
    ensure!(
        matches!(
            reference.location,
            lash_core::tool_run::MaterialLocation::RetainedArtifact { .. }
        ),
        "S32 did not publish a retained owner-qualified reference"
    );
    let barrier = Barrier {
        work: work.clone(),
        kind: BarrierKind::SuccessorAdmitted,
    };
    let proof = control.await_barrier(&barrier).await?;
    ensure!(
        proof.journal_index.is_some(),
        "successor ownership is not durable"
    );
    evidence.barriers.push(proof);
    let removed = host
        .command(HostCommand::Process {
            action: "remove-retained-material".into(),
            input: serde_json::json!({"work": work, "reference": reference}),
        })
        .await?;
    evidence.outputs.push(removed);
    let cold = host
        .command(HostCommand::Process {
            action: "cold-follow".into(),
            input: serde_json::json!({"work": work}),
        })
        .await?;
    evidence.outputs.push(cold);
    Ok(())
}
