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

pub fn transfer<'a>(evidence: &'a Evidence, work: &WorkIdentity) -> Result<&'a RunTransfer> {
    let matches: Vec<_> = evidence
        .journals
        .iter()
        .filter(|fact| fact.work.run == work.run)
        .filter_map(|fact| match &fact.decoded {
            Some(DecodedRecord::Transfer(transfer)) => Some(transfer),
            _ => None,
        })
        .collect();
    ensure!(
        matches.len() == 1,
        "expected one durable transfer, found {}",
        matches.len()
    );
    Ok(matches[0])
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
