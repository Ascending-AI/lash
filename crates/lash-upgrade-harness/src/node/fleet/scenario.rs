//! Real fleet witnesses for R3. No load phase belongs here.
//!
//! A worker need not hand completion to a particular peer: acceptance,
//! committed output and terminal converge once (FIG-1671, ADR 0101).
//! Durable X is reused; interrupted bodies keep call ID and ordinal (L02).

use std::collections::{BTreeMap, BTreeSet};

use crate::e2e::Step;
use crate::e2e::case::{ArtifactIdentity, CaseLease, CaseSpec, Channel, StoreKind};
use crate::e2e::cluster::{ClusterControl, ClusterReceipt, LeaderReceipt};
use crate::e2e::control::{
    Barrier, BarrierKind, BarrierProof, Control, Fault, ToolControl, WorkIdentity,
};
use crate::e2e::evidence::{DecodedRecord, Evidence};
use crate::e2e::host::{HostAdapter, HostCommand, HostKind};
use crate::e2e::provider::ProviderKind;
use crate::node::fleet::FleetSnapshot;
use anyhow::{Context, Result, ensure};
use lash_core_store::tool_run::{CallDecision, RunEvent, RunJournalEntry, RunLedger};

mod runtime;
pub use runtime::{run, run_named};

#[derive(Clone, Copy, Debug)]
pub enum Scenario {
    LeaderLoss,
    MinorityPartition,
    TerminalPublication,
    TerminalRedrive,
}

impl Scenario {
    pub fn spec(self, artifacts: Vec<ArtifactIdentity>) -> CaseSpec {
        let (id, rules, nodes, requires) = match self {
            Self::LeaderLoss => ("S14", &["R3", "R1", "L02", "L03"][..], 3, &["FIG-4894"][..]),
            Self::MinorityPartition => {
                ("S15", &["R3", "R5", "L03", "L19"][..], 3, &["FIG-4894"][..])
            }
            Self::TerminalPublication | Self::TerminalRedrive => (
                "S16",
                &["R3", "R4", "L09", "L19"][..],
                1,
                &["FIG-4878", "FIG-4739"][..],
            ),
        };
        CaseSpec {
            id: id.into(),
            rules: rules.iter().map(|rule| (*rule).into()).collect(),
            host: HostKind::UpgradeNode,
            store: StoreKind::PostgreSql,
            channel: Channel::Standard,
            provider: ProviderKind::Scripted,
            restate_nodes: nodes,
            artifacts,
            cuts: Vec::new(),
            expected_terminal: match self {
                Self::MinorityPartition => "committed final or cancellation, same on every node",
                _ => "one committed answer",
            }
            .into(),
            requires: requires.iter().map(|unit| (*unit).into()).collect(),
        }
    }
}

/// H4's fixture observations over H0's shared controls and decoder, and H2's
/// actual Standard host. Every implementation must read real receipts; there
/// is no synthetic fleet implementation. `prepare` boots two host processes
/// over the lease's same database, namespace, authority and fixture ledger.
/// Collection preserves journals from each admin URL independently.
pub trait FleetFixture: Send {
    fn prepare<'a>(&'a mut self, scenario: Scenario, lease: &'a mut CaseLease) -> Step<'a, ()>;
    fn primary(&mut self) -> &mut dyn HostAdapter;
    fn follower(&mut self) -> &mut dyn HostAdapter;
    fn observed_work(&mut self) -> Step<'_, Option<WorkIdentity>>;
    fn barrier<'a>(
        &'a mut self,
        work: &'a WorkIdentity,
        label: &'a str,
        kind: BarrierKind,
    ) -> Step<'a, Barrier>;
    fn capture<'a>(
        &'a mut self,
        work: &'a WorkIdentity,
        excluded_node: Option<u32>,
    ) -> Step<'a, Evidence>;
    fn partition_for<'a>(&'a mut self, work: &'a WorkIdentity) -> Step<'a, u32>;
    fn await_leader<'a>(&'a mut self, previous: &'a LeaderReceipt) -> Step<'a, LeaderReceipt>;
    fn snapshot<'a>(&'a mut self, work: &'a WorkIdentity) -> Step<'a, FleetSnapshot>;
    fn publication_epoch(&self) -> Result<u64>;
    fn drain_complete(&mut self) -> Step<'_, ()>;
    fn kill_publication_host<'a>(&'a mut self, proof: &'a BarrierProof) -> Step<'a, ()>;
    fn finish(&mut self) -> Step<'_, Vec<crate::e2e::control::CleanupReceipt>>;
}

#[derive(serde::Serialize)]
pub struct FleetProof {
    pub before: Evidence,
    pub after: Evidence,
    pub store: FleetSnapshot,
}

/// Each selected fleet scenario owns its teardown even when a fault/oracle
/// fails. A failed cleanup or fixture background task fails the scenario;
/// journal artifacts are written before the processes are stopped.
pub async fn execute(
    scenario: Scenario,
    lease: &mut CaseLease,
    cluster: &mut dyn ClusterControl,
    control: &mut dyn Control,
    fixture: &mut dyn FleetFixture,
) -> Result<FleetProof> {
    let mut result = match scenario {
        Scenario::LeaderLoss => {
            s14_leader_loss_retains_accepted_work(lease, cluster, control, fixture).await
        }
        Scenario::MinorityPartition => {
            s15_minority_partition_cannot_create_another_winner(lease, cluster, control, fixture)
                .await
        }
        Scenario::TerminalPublication => {
            s16_in_flight_terminal_preserves_authority(lease, control, fixture).await
        }
        Scenario::TerminalRedrive => s16_same_invocation_redrive(lease, control, fixture).await,
    };
    let retained = match &result {
        Ok(proof) => serde_json::to_vec_pretty(proof),
        Err(error) => {
            serde_json::to_vec_pretty(&serde_json::json!({ "failure": format!("{error:#}") }))
        }
    }
    .map_err(anyhow::Error::from)
    .and_then(|bytes| {
        let path = lease.directory.join("fleet-proof.json");
        std::fs::write(&path, bytes).with_context(|| format!("retain {}", path.display()))
    });
    // Capture available failed-invocation facts before reaping. Failure to
    // collect is retained as a failure too, never converted into an empty proof.
    let failed_capture = if result.is_err() {
        match fixture.observed_work().await {
            Ok(observation) => match observation {
                Some(work) => match fixture.capture(&work, None).await {
                    Ok(evidence) => serde_json::to_vec_pretty(&evidence)
                        .map_err(anyhow::Error::from)
                        .and_then(|bytes| {
                            std::fs::write(
                                lease.directory.join("fleet-failure-evidence.json"),
                                bytes,
                            )
                            .map_err(anyhow::Error::from)
                        }),
                    Err(error) => Err(error),
                },
                None => Ok(()), // Boot can fail before any input is accepted.
            },
            Err(error) => Err(error),
        }
    } else {
        Ok(())
    };
    let fixture_cleanup = fixture.finish().await;
    let cluster_cleanup = cluster.finish().await;
    let mut cleanup_errors = Vec::new();
    for receipts in [fixture_cleanup, cluster_cleanup] {
        match receipts {
            Ok(receipts) => {
                for receipt in receipts {
                    if !receipt.closed {
                        cleanup_errors.push(format!("{}: {}", receipt.resource, receipt.detail));
                    }
                    lease.cleanup.push(receipt);
                }
            }
            Err(error) => cleanup_errors.push(format!("{error:#}")),
        }
    }
    if let Ok(proof) = &mut result {
        proof.after.cleanup.extend(lease.cleanup.clone());
    }
    let cleanup_artifact = serde_json::to_vec_pretty(&serde_json::json!({
        "receipts": &lease.cleanup,
        "errors": &cleanup_errors,
    }))
    .map_err(anyhow::Error::from)
    .and_then(|bytes| {
        std::fs::write(lease.directory.join("fleet-cleanup.json"), bytes)
            .map_err(anyhow::Error::from)
    });
    if let Err(error) = cleanup_artifact {
        cleanup_errors.push(format!("{error:#}"));
    }
    let primary_failure = result
        .as_ref()
        .err()
        .map(|error| format!("{error:#}"))
        .unwrap_or_default();
    for error in [retained.err(), failed_capture.err()].into_iter().flatten() {
        cleanup_errors.push(format!("{error:#}"));
    }
    ensure!(
        cleanup_errors.is_empty(),
        "{primary_failure}; fleet retention/cleanup failed: {}",
        cleanup_errors.join("; ")
    );
    result
}

fn submit(lease: &CaseLease, scenario: &str) -> HostCommand {
    HostCommand::Submit {
        session: format!("{}-fleet", lease.namespace),
        idempotency_key: format!("{}-{scenario}", lease.gate_id),
        input: serde_json::json!({"scenario": scenario}),
    }
}

fn assert_same_owner(expected: &WorkIdentity, actual: &WorkIdentity) -> Result<()> {
    ensure!(
        !expected.ingress.is_empty() && !expected.run.is_empty(),
        "accepted identity is incomplete"
    );
    ensure!(
        expected.ingress == actual.ingress && expected.run == actual.run,
        "observation attached another input or Run"
    );
    Ok(())
}

fn assert_live_cluster(cluster: &ClusterReceipt, expected: usize) -> Result<()> {
    ensure!(
        cluster.nodes.len() == expected,
        "expected {expected} real Restate members"
    );
    ensure!(
        cluster.binary.sha256.len() == 64 && !cluster.binary.candidate_sha.is_empty(),
        "missing pinned binary provenance"
    );
    let ids: BTreeSet<_> = cluster.nodes.iter().map(|node| node.node).collect();
    let paths: BTreeSet<_> = cluster
        .nodes
        .iter()
        .map(|node| &node.data_directory)
        .collect();
    ensure!(
        ids.len() == expected && paths.len() == expected,
        "members share identity or data"
    );
    ensure!(
        cluster
            .nodes
            .iter()
            .all(|node| !node.admin_url.is_empty() && !node.peer_address.is_empty()),
        "cluster member has no real admin or peer address"
    );
    Ok(())
}

/// S14 / L02 / L03: fault the observed leader of the partition holding
/// the actual accepted invocation, then reattach its original Run.
pub async fn s14_leader_loss_retains_accepted_work(
    lease: &mut CaseLease,
    cluster: &mut dyn ClusterControl,
    control: &mut dyn Control,
    fixture: &mut dyn FleetFixture,
) -> Result<FleetProof> {
    fixture.prepare(Scenario::LeaderLoss, lease).await?;
    let initial = cluster.converge().await?;
    assert_live_cluster(&initial, 3)?;
    let accepted = fixture
        .primary()
        .command(submit(lease, "partial-result"))
        .await?;
    let work = &accepted.work;
    let a = fixture.barrier(work, "a", BarrierKind::XDurable).await?;
    let durable = control.await_barrier(&a).await?;
    let b = fixture.barrier(work, "b", BarrierKind::BodyEntered).await?;
    let held = control.await_barrier(&b).await?;
    assert_barrier(&durable, BarrierKind::XDurable, work, true)?;
    assert_barrier(&held, BarrierKind::BodyEntered, work, false)?;
    let before = fixture.capture(work, None).await?;
    let partition = fixture.partition_for(work).await?;
    let leader = cluster
        .leaders()
        .await?
        .into_iter()
        .find(|leader| leader.partition == partition)
        .context("accepted invocation has no observed partition leader")?;
    let original = initial
        .nodes
        .iter()
        .find(|node| node.node == leader.node)
        .context("leader is not a provisioned cluster member")?;
    let killed = cluster.kill(leader.node, &held).await?;
    ensure!(
        killed.target_incarnation == original.incarnation,
        "kill targeted another incarnation"
    );
    ensure!(
        killed.proof.barrier == held.barrier,
        "kill missed the active body cut"
    );
    let successor = fixture.await_leader(&leader).await?;
    assert_new_leader(&leader, &successor)?;
    let restarted = cluster.restart(leader.node).await?;
    ensure!(
        restarted.data_directory == original.data_directory,
        "restart lost durable server data"
    );
    ensure!(
        restarted.incarnation > original.incarnation,
        "server was not restarted"
    );
    control.tool(ToolControl::Release(b)).await?;
    let answer = fixture
        .follower()
        .command(HostCommand::Attach {
            run: work.run.clone(),
        })
        .await?;
    assert_same_owner(work, &answer.work)?;
    let converged = cluster.converge().await?;
    assert_live_cluster(&converged, 3)?;
    let after = fixture.capture(work, None).await?;
    assert_cluster_journals(&converged, &after, work)?;
    assert_partial_recovery(&before, &after, &durable, &held)?;
    ensure!(
        answer.output == serde_json::json!({"status":"Answered", "reply":"A|B"}),
        "recovered output is not exact A/B output"
    );
    let store = fixture.snapshot(work).await?;
    store.assert_one_settlement()?;
    store.assert_frontier()?;
    ensure!(
        store.inputs[0].id == work.ingress && store.run.as_str() == work.run,
        "store settlement belongs to another accepted input or Run"
    );
    Ok(FleetProof {
        before,
        after,
        store,
    })
}

/// S15 / L03 / L19: isolate one member in both directions. The controller
/// must close established streams, not merely refuse new connections.
pub async fn s15_minority_partition_cannot_create_another_winner(
    lease: &mut CaseLease,
    cluster: &mut dyn ClusterControl,
    control: &mut dyn Control,
    fixture: &mut dyn FleetFixture,
) -> Result<FleetProof> {
    fixture.prepare(Scenario::MinorityPartition, lease).await?;
    let initial = cluster.converge().await?;
    assert_live_cluster(&initial, 3)?;
    let accepted = fixture
        .primary()
        .command(submit(lease, "tool-cancel-race"))
        .await?;
    let work = &accepted.work;
    let body = fixture
        .barrier(work, "intent", BarrierKind::BodyEntered)
        .await?;
    let held = control.await_barrier(&body).await?;
    assert_barrier(&held, BarrierKind::BodyEntered, work, false)?;
    let before = fixture.capture(work, None).await?;
    let partition = fixture.partition_for(work).await?;
    let minority = cluster
        .leaders()
        .await?
        .into_iter()
        .find(|leader| leader.partition == partition)
        .context("active partition has no observed leader")?;
    for node in &initial.nodes {
        if node.node != minority.node {
            cluster.partition(minority.node, node.node).await?;
            cluster.partition(node.node, minority.node).await?;
        }
    }
    let majority = fixture.await_leader(&minority).await?;
    assert_new_leader(&minority, &majority)?;
    // Host routes reach the majority. Release and public cancellation race;
    // the actual recorded decision determines the oracle, not host timing.
    let (cancel, release) = tokio::join!(
        fixture.follower().command(HostCommand::Cancel {
            run: work.run.clone()
        }),
        control.tool(ToolControl::Release(body)),
    );
    assert_same_owner(work, &cancel?.work)?;
    release?;
    let answer = fixture
        .follower()
        .command(HostCommand::Attach {
            run: work.run.clone(),
        })
        .await?;
    assert_same_owner(work, &answer.work)?;
    let settled = fixture.capture(work, Some(minority.node)).await?;
    assert_cancel_race(&settled, work, &held.barrier, &answer)?;
    let store = fixture.snapshot(work).await?;
    store.assert_one_settlement()?;
    store.assert_frontier()?;
    ensure!(
        store.inputs[0].id == work.ingress && store.run.as_str() == work.run,
        "store settlement belongs to another accepted input or Run"
    );
    for node in &initial.nodes {
        if node.node != minority.node {
            cluster.heal(minority.node, node.node).await?;
            cluster.heal(node.node, minority.node).await?;
        }
    }
    let healed = cluster.converge().await?;
    assert_live_cluster(&healed, 3)?;
    let after = fixture.capture(work, None).await?;
    assert_cluster_journals(&healed, &after, work)?;
    ensure!(
        decisions(&settled, work)? == decisions(&after, work)?,
        "healed minority added or changed a final/cancel decision"
    );
    assert_admissions_unchanged(&settled, &after, work)?;
    ensure!(
        store == fixture.snapshot(work).await?,
        "minority changed the settled head/state after heal"
    );
    assert_partition_receipts(&after, work, minority.node, &initial)?;
    assert_receiver_unchanged(&settled, &after)?;
    Ok(FleetProof {
        before,
        after,
        store,
    })
}

/// S16 / B01 / L19, ruling 13951: an in-flight terminal is not a quiet
/// point. Drain remains pending; a disconnect cannot manufacture a higher
/// fence or a second execution segment. The original publication commits once.
pub async fn s16_in_flight_terminal_preserves_authority(
    lease: &mut CaseLease,
    control: &mut dyn Control,
    fixture: &mut dyn FleetFixture,
) -> Result<FleetProof> {
    fixture
        .prepare(Scenario::TerminalPublication, lease)
        .await?;
    let accepted = fixture
        .primary()
        .command(submit(lease, "stale-publication"))
        .await?;
    let work = &accepted.work;
    let publication = fixture
        .barrier(work, "publication", BarrierKind::PublicationRequest)
        .await?;
    let held = control.await_barrier(&publication).await?;
    assert_barrier(&held, BarrierKind::PublicationRequest, work, false)?;
    let before = fixture.capture(work, None).await?;
    let epoch = fixture.publication_epoch()?;
    let drain = fixture
        .primary()
        .command(HostCommand::Process {
            action: "drain".into(),
            input: serde_json::Value::Null,
        })
        .await?;
    ensure!(
        drain.output["drained"] == false,
        "in-flight terminal was reported drained"
    );
    let fault = control
        .inject(
            Fault::DropConnection {
                target: "fleet-primary".into(),
            },
            &held,
        )
        .await?;
    ensure!(
        fault.proof.barrier == held.barrier,
        "disconnect missed publication cut"
    );
    let pending = fixture
        .follower()
        .command(HostCommand::Process {
            action: "drain-status".into(),
            input: serde_json::Value::Null,
        })
        .await?;
    ensure!(
        pending.output["drained"] == false,
        "disconnect bypassed the terminal quiet-point restriction"
    );
    control.tool(ToolControl::Release(publication)).await?;
    let answer = fixture
        .follower()
        .command(HostCommand::Attach {
            run: work.run.clone(),
        })
        .await?;
    assert_same_owner(work, &answer.work)?;
    let published = fixture.snapshot(work).await?;
    published.assert_one_settlement()?;
    published.assert_frontier()?;
    ensure!(
        published.inputs[0].id == work.ingress && published.run.as_str() == work.run,
        "publication settled another accepted input or Run"
    );
    ensure!(
        published.plugin_component.is_some(),
        "terminal publication has no plugin frontier witness"
    );
    ensure!(
        published.shift_epoch == epoch,
        "same-invocation replay raised its journaled seal"
    );
    ensure!(
        answer.work.segment == work.segment,
        "disconnect created a second segment"
    );
    ensure!(
        answer.output == serde_json::json!({"status":"Answered","reply":"A|B"}),
        "terminal answer differs"
    );
    fixture.drain_complete().await?;
    let after = fixture.capture(work, None).await?;
    let store = fixture.snapshot(work).await?;
    ensure!(
        store == published,
        "completed drain changed committed head or plugin frontier"
    );
    assert_decisions_unchanged(&before, &after, work)?;
    Ok(FleetProof {
        before,
        after,
        store,
    })
}

/// The original store request dies with this worker. Restate redrives the
/// original invocation; its recorded seal is reused, never called a transfer.
async fn s16_same_invocation_redrive(
    lease: &mut CaseLease,
    control: &mut dyn Control,
    fixture: &mut dyn FleetFixture,
) -> Result<FleetProof> {
    fixture.prepare(Scenario::TerminalRedrive, lease).await?;
    let accepted = fixture
        .primary()
        .command(submit(lease, "stale-publication"))
        .await?;
    let work = &accepted.work;
    let publication = fixture
        .barrier(work, "publication", BarrierKind::PublicationRequest)
        .await?;
    let held = control.await_barrier(&publication).await?;
    assert_barrier(&held, BarrierKind::PublicationRequest, work, false)?;
    let before = fixture.capture(work, None).await?;
    let epoch = fixture.publication_epoch()?;
    fixture.kill_publication_host(&held).await?;
    let answer = fixture
        .follower()
        .command(HostCommand::Attach {
            run: work.run.clone(),
        })
        .await?;
    assert_same_owner(work, &answer.work)?;
    ensure!(
        answer.work.segment == work.segment,
        "SIGKILL replay started another segment"
    );
    ensure!(
        answer.output == serde_json::json!({"status":"Answered","reply":"A|B"}),
        "redrive changed final output"
    );
    let after = fixture.capture(work, None).await?;
    let store = fixture.snapshot(work).await?;
    store.assert_one_settlement()?;
    store.assert_frontier()?;
    ensure!(
        store.shift_epoch == epoch,
        "SIGKILL replay raised a second seal"
    );
    assert_admissions_unchanged(&before, &after, work)?;
    assert_decisions_unchanged(&before, &after, work)?;
    Ok(FleetProof {
        before,
        after,
        store,
    })
}

fn assert_barrier(
    proof: &BarrierProof,
    kind: BarrierKind,
    work: &WorkIdentity,
    durable: bool,
) -> Result<()> {
    ensure!(proof.barrier.kind == kind, "fault cut has another phase");
    assert_same_owner(work, &proof.barrier.work)?;
    ensure!(
        !proof.artifact.is_empty(),
        "barrier has no independently retained proof"
    );
    ensure!(
        !durable || proof.journal_index.is_some(),
        "durable cut has no real journal index"
    );
    Ok(())
}

fn assert_new_leader(old: &LeaderReceipt, new: &LeaderReceipt) -> Result<()> {
    ensure!(
        old.partition == new.partition,
        "failover observed another partition"
    );
    ensure!(
        new.node != old.node && new.epoch > old.epoch,
        "active partition did not fail over"
    );
    Ok(())
}

fn run_entries<'a>(
    evidence: &'a Evidence,
    work: &WorkIdentity,
    admin: &str,
) -> Result<Vec<&'a RunJournalEntry>> {
    let mut facts: Vec<_> = evidence
        .journals
        .iter()
        .filter(|fact| {
            fact.work.run == work.run
                && fact.work.ingress == work.ingress
                && fact.admin_url == admin
        })
        .filter_map(|fact| match &fact.decoded {
            Some(DecodedRecord::Run(entry)) => Some((fact, entry)),
            _ => None,
        })
        .collect();
    ensure!(!facts.is_empty(), "node {admin} has no decoded Run records");
    facts.sort_by_key(|(_, entry)| (entry.record.segment, entry.record.first));
    let mut slots = BTreeSet::new();
    let owner = facts
        .iter()
        .flat_map(|(_, entry)| &entry.record.events)
        .find_map(|event| match event {
            RunEvent::Admitted { round } => Some(round.owner.clone()),
            _ => None,
        })
        .context("Run journal has no original admission owner")?;
    let mut ledger = RunLedger::new(owner);
    for (fact, entry) in &facts {
        ensure!(
            fact.protocol == 7,
            "journal was not decoded from negotiated V7"
        );
        ensure!(
            slots.insert((&fact.invocation, fact.index)),
            "same journal slot was counted twice"
        );
        ledger
            .append(entry.record.segment, &entry.record)
            .map_err(|error| {
                anyhow::anyhow!(
                    "node {admin} has an invalid Run record at {}:{}: {error}",
                    fact.invocation,
                    fact.index
                )
            })?;
    }
    Ok(facts.into_iter().map(|(_, entry)| entry).collect())
}

fn assert_cluster_journals(
    cluster: &ClusterReceipt,
    evidence: &Evidence,
    work: &WorkIdentity,
) -> Result<()> {
    let first = cluster.nodes.first().context("no cluster members")?;
    let expected = run_entries(evidence, work, &first.admin_url)?;
    ensure!(
        expected
            .iter()
            .flat_map(|entry| &entry.record.events)
            .any(|event| matches!(
                event,
                RunEvent::Lifecycle {
                    state: lash_core_store::tool_run::RunLifecycle::Settled
                }
            )),
        "cluster journal has no settled Run"
    );
    for node in &cluster.nodes {
        ensure!(
            run_entries(evidence, work, &node.admin_url)? == expected,
            "member {} has another journal/head terminal",
            node.node
        );
    }
    Ok(())
}

fn decisions(
    evidence: &Evidence,
    work: &WorkIdentity,
) -> Result<BTreeMap<lash_core::ToolCallId, CallDecision>> {
    let admin = evidence
        .journals
        .iter()
        .find(|fact| fact.work.run == work.run)
        .context("missing Run journal")?
        .admin_url
        .as_str();
    let mut decisions = BTreeMap::new();
    for event in run_entries(evidence, work, admin)?
        .iter()
        .flat_map(|entry| &entry.record.events)
    {
        if let RunEvent::Decided {
            call_id, decision, ..
        } = event
        {
            ensure!(
                decisions
                    .insert(call_id.clone(), decision.clone())
                    .is_none(),
                "call decided twice"
            );
        }
    }
    Ok(decisions)
}

fn receiver_events(evidence: &Evidence) -> Result<crate::node::tools::ReceiverEvents> {
    let pages: Vec<_> = evidence
        .effects
        .iter()
        .filter(|value| value["kind"] == "h2_receiver_events")
        .collect();
    ensure!(
        pages.len() == 1,
        "race must retain one complete actual receiver event page"
    );
    Ok(serde_json::from_value(pages[0].clone())?)
}

fn assert_receiver_unchanged(before: &Evidence, after: &Evidence) -> Result<()> {
    ensure!(
        serde_json::to_value(receiver_events(before)?)?
            == serde_json::to_value(receiver_events(after)?)?,
        "healed minority added or changed a mutation identity/receipt"
    );
    Ok(())
}

fn assert_cancel_race(
    evidence: &Evidence,
    work: &WorkIdentity,
    body: &Barrier,
    answer: &crate::e2e::host::HostObservation,
) -> Result<()> {
    let decisions = decisions(evidence, work)?;
    let call = lash_core::ToolCallId::parse(
        body.work
            .call
            .as_ref()
            .context("intent body has no call identity")?,
    )?;
    ensure!(decisions.len() == 1, "intent race has another decided call");
    let receiver = receiver_events(evidence)?;
    let mutations: Vec<_> = receiver
        .events
        .iter()
        .filter(|event| event.event_type == "tool_receipt")
        .collect();
    let admin = evidence
        .journals
        .first()
        .context("race has no journal")?
        .admin_url
        .as_str();
    let ranks: Vec<_> = run_entries(evidence, work, admin)?
        .iter()
        .flat_map(|entry| &entry.record.events)
        .filter_map(|event| match event {
            RunEvent::Decided { call_id, rank, .. } if call_id == &call => Some(*rank),
            _ => None,
        })
        .collect();
    ensure!(
        ranks == vec![1],
        "race decision did not retain its original rank"
    );
    match decisions
        .get(&call)
        .context("original intent call has no decision")?
    {
        CallDecision::Final { declares: true, .. } => {
            ensure!(
                mutations.len() == 1,
                "protected final intent was omitted or duplicated"
            );
            let event = mutations[0];
            ensure!(
                event.process_id == receiver.process_id
                    && event.payload == serde_json::json!({"call_id":call,"value":"intent"}),
                "intent receipt names another receiver/call/value"
            );
            ensure!(
                answer.output["status"] == "Answered" || answer.output["status"] == "Cancelled",
                "final race has another terminal status"
            );
            if answer.output["status"] == "Answered" {
                ensure!(
                    answer.output["reply"] == "intent",
                    "answered race changed the final value"
                );
            }
        }
        CallDecision::Cancelled => {
            ensure!(
                mutations.is_empty(),
                "cancelled call realized a discarded intent"
            );
            ensure!(
                answer.output == serde_json::json!({"status":"Cancelled","reply":null}),
                "cancel decision and terminal disagree"
            );
        }
        other => anyhow::bail!("tool/cancel race ended with unexpected decision {other:?}"),
    }
    Ok(())
}

fn assert_decisions_unchanged(
    before: &Evidence,
    after: &Evidence,
    work: &WorkIdentity,
) -> Result<()> {
    let prior = decisions(before, work)?;
    let current = decisions(after, work)?;
    for (call, decision) in prior {
        ensure!(
            current.get(&call) == Some(&decision),
            "recovery rewrote a recorded decision"
        );
    }
    ensure!(!current.is_empty(), "Run has no final/cancel decision");
    Ok(())
}

fn assert_partial_recovery(
    before: &Evidence,
    after: &Evidence,
    durable: &BarrierProof,
    held: &BarrierProof,
) -> Result<()> {
    let a = durable
        .barrier
        .work
        .call
        .as_ref()
        .context("durable X has no call ID")?;
    let b = held
        .barrier
        .work
        .call
        .as_ref()
        .context("held body has no call ID")?;
    ensure!(a != b, "A and B alias one call");
    ensure!(
        durable.barrier.work.ordinal == Some(1) && held.barrier.work.ordinal == Some(1),
        "partial-result case unexpectedly retried"
    );
    // The original A/X fact, including its payload and actual journal slot,
    // survives; no reconstructed provider log can satisfy this equality.
    let x = before
        .journals
        .iter()
        .find(|fact| {
            Some(fact.index) == durable.journal_index
                && fact.invocation == durable.barrier.work.segment
                && matches!(&fact.decoded, Some(DecodedRecord::Attempt(attempt))
                if attempt.call_id.as_str() == a && attempt.attempt.get() == 1)
        })
        .context("A has no independent pre-fault typed X receipt")?;
    ensure!(
        after
            .journals
            .iter()
            .any(|fact| fact.invocation == x.invocation
                && fact.index == x.index
                && fact.value == x.value
                && fact.work == x.work),
        "durable X disappeared or changed during failover"
    );
    let work = &held.barrier.work;
    assert_decisions_unchanged(before, after, work)?;
    let final_decisions = decisions(after, work)?;
    assert_admissions_unchanged(before, after, work)?;
    assert_partial_deliveries(after, a, b, work)?;
    ensure!(
        final_decisions.len() == 2
            && final_decisions
                .values()
                .all(|d| matches!(d, CallDecision::Final { .. })),
        "two-tool recovery did not finalize both original calls"
    );
    for id in final_decisions.keys() {
        ensure!(
            id.as_str() == a || id.as_str() == b,
            "recovery minted a new call identity"
        );
    }
    Ok(())
}

fn assert_partial_deliveries(
    evidence: &Evidence,
    a: &str,
    b: &str,
    work: &WorkIdentity,
) -> Result<()> {
    let deliveries = evidence
        .effects
        .iter()
        .filter(|effect| effect.get("label").is_some())
        .map(|effect| serde_json::from_value::<crate::node::tools::ToolDelivery>(effect.clone()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    ensure!(
        deliveries
            .iter()
            .filter(|delivery| delivery.label == "a")
            .count()
            == 1,
        "durable A body did not execute exactly once"
    );
    ensure!(
        deliveries.iter().any(|delivery| delivery.label == "b"),
        "unfinished B body has no delivery witness"
    );
    for delivery in &deliveries {
        let expected = match delivery.label.as_str() {
            "a" => a,
            "b" => b,
            label => anyhow::bail!("recovery delivered an unadmitted body {label}"),
        };
        ensure!(
            delivery.call_id.as_str() == expected
                && delivery.ordinal == 1
                && delivery
                    .logical_run
                    .as_ref()
                    .is_some_and(|run| run.as_str() == work.run),
            "recovery changed body call ID, attempt ordinal or logical Run"
        );
    }
    Ok(())
}

fn assert_admissions_unchanged(
    before: &Evidence,
    after: &Evidence,
    work: &WorkIdentity,
) -> Result<()> {
    let admin = before
        .journals
        .iter()
        .find(|fact| fact.work.run == work.run)
        .context("missing pre-fault admission journal")?
        .admin_url
        .as_str();
    let admissions = |entries: Vec<&RunJournalEntry>| {
        entries
            .into_iter()
            .flat_map(|entry| &entry.record.events)
            .filter_map(|event| match event {
                RunEvent::Admitted { round } => Some(round.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
    };
    let original = admissions(run_entries(before, work, admin)?);
    ensure!(
        !original.is_empty(),
        "pre-fault work has no admitted executable binding"
    );
    ensure!(
        admissions(run_entries(after, work, admin)?) == original,
        "recovery minted or changed an admitted executable binding"
    );
    Ok(())
}

fn assert_partition_receipts(
    evidence: &Evidence,
    work: &WorkIdentity,
    minority: u32,
    cluster: &ClusterReceipt,
) -> Result<()> {
    for node in &cluster.nodes {
        if node.node == minority {
            continue;
        }
        for (from, to) in [(minority, node.node), (node.node, minority)] {
            ensure!(
                evidence.faults.iter().any(|receipt| matches!(receipt.fault,
                Fault::PartitionLink { from: f, to: t } if f == from && t == to)
                    && receipt.proof.barrier.work.run == work.run
                    && !receipt.proof.artifact.is_empty()),
                "missing directed partition/drop-established-stream proof {from}->{to}"
            );
            ensure!(
                evidence.faults.iter().any(|receipt| matches!(receipt.fault,
                Fault::HealLink { from: f, to: t } if f == from && t == to)),
                "missing heal proof {from}->{to}"
            );
        }
    }
    Ok(())
}
