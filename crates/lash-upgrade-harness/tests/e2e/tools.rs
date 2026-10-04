//! H2 real-host witnesses: R1/R7, L01/L02/L05/L14.
//!
//! H0 supplies the controller and the journal reader; H6 supplies production
//! host transports. No model log or locally reconstructed fold is journal proof.
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use anyhow::{Result, anyhow, ensure};
use lash_core::ToolCallId;
use lash_core::tool_run::{
    AttemptOrdinal, BusinessReceipt, CallDecision, LogicalTerminal, RunEvent, RunJournalEntry,
};
use lash_upgrade_harness::e2e::Step;
use lash_upgrade_harness::e2e::case::{ArtifactIdentity, CaseLease, CaseSpec, Channel, StoreKind};
use lash_upgrade_harness::e2e::control::{
    Barrier, BarrierKind, BarrierProof, Control, Fault, ToolControl, WorkIdentity,
};
use lash_upgrade_harness::e2e::evidence::{DecodedRecord, Evidence, JournalFact};
use lash_upgrade_harness::e2e::host::{HostAdapter, HostCommand, HostKind, HostReady};
use lash_upgrade_harness::e2e::provider::ProviderKind;
use lash_upgrade_harness::node::tools::ToolDelivery;

/// A read through H0's real journal/store evidence collector. The future owns
/// its cloned clients; it does not borrow an adapter while an effect is held.
pub type Snapshot = Arc<dyn Fn(WorkIdentity) -> Step<'static, Evidence> + Send + Sync>;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s01_agent_service_singleton() -> Result<()> {
    super::h2::run(super::h2::Row::Singleton).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s02_cold_partial() -> Result<()> {
    super::h2::run(super::h2::Row::Partial).await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s05_opposite_batch_order() -> Result<()> {
    super::h2::run(super::h2::Row::Batch).await
}

pub struct Scenario<'a> {
    pub host: &'a mut dyn HostAdapter,
    pub control: &'a mut dyn Control,
    pub snapshot: Snapshot,
    pub artifact: &'a ArtifactIdentity,
    pub lease: &'a mut CaseLease,
    pub ready: Option<HostReady>,
    pub work: Option<WorkIdentity>,
    pub proofs: Vec<BarrierProof>,
    pub faults: Vec<lash_upgrade_harness::e2e::control::FaultReceipt>,
}

pub fn spec(id: &str, store: StoreKind, artifacts: Vec<ArtifactIdentity>) -> Result<CaseSpec> {
    let rules = match id {
        "S01" => vec!["R1", "R7", "L02", "L14"],
        "S02" => vec!["R1", "L02"],
        "S05" => vec!["R1", "R6", "L01", "L05"],
        _ => return Err(anyhow!("unknown H2 tools scenario {id}")),
    };
    ensure!(
        id != "S02" || store != StoreKind::SqliteMemory,
        "S02's cold reopen needs a persistent store"
    );
    Ok(CaseSpec {
        id: id.to_owned(),
        rules: rules.into_iter().map(str::to_owned).collect(),
        host: HostKind::AgentService,
        store,
        channel: Channel::Standard,
        provider: ProviderKind::Scripted,
        restate_nodes: 1,
        artifacts,
        // Call identities and cuts are bound after real admission, never invented.
        cuts: Vec::new(),
        expected_terminal: "Answered".to_owned(),
        // F01 (FIG-4894) is in the candidate baseline.
        requires: Vec::new(),
    })
}

impl Scenario<'_> {
    pub fn work(&self) -> Result<&WorkIdentity> {
        self.work
            .as_ref()
            .ok_or_else(|| anyhow!("input was not admitted"))
    }

    pub async fn start(&mut self, spec: &CaseSpec) -> Result<Evidence> {
        ensure!(
            std::time::Instant::now() < self.lease.deadline,
            "case lease expired"
        );
        let ready = self.host.boot(self.artifact, self.lease).await?;
        ensure!(ready.protocol == 7, "H2 requires the pinned V7 protocol");
        ensure!(ready.process.pid > 0, "host did not boot a real process");
        self.ready = Some(ready);
        let admitted = self
            .host
            .command(HostCommand::Submit {
                session: format!("{}-{}", self.lease.namespace, spec.id.to_lowercase()),
                idempotency_key: format!("{}:{}", self.lease.gate_id, spec.id),
                input: serde_json::json!({"text": spec.id}),
            })
            .await?;
        ensure!(
            !admitted.work.ingress.is_empty() && !admitted.work.run.is_empty(),
            "submit returned no durable ingress/Run identity"
        );
        self.work = Some(admitted.work);
        self.wait_owner(BarrierKind::AdmissionDurable).await?;
        self.read().await
    }

    pub async fn read(&self) -> Result<Evidence> {
        let work = self.work()?.clone();
        let evidence = (self.snapshot)(work.clone()).await?;
        ensure!(
            !evidence.case.is_empty(),
            "snapshot has no scenario identity"
        );
        for fact in &evidence.journals {
            ensure!(
                fact.work.ingress == work.ingress && fact.work.run == work.run,
                "journal facts from another ingress/Run cannot prove this case"
            );
            ensure!(
                !fact.invocation.is_empty() && !fact.admin_url.is_empty() && fact.protocol == 7,
                "journal provenance is missing or not V7"
            );
            if let Some(decoded) = &fact.decoded {
                match decoded {
                    DecodedRecord::Attempt(entry) => ensure!(
                        serde_json::from_value::<lash_core::tool_run::RunAttemptEntry>(
                            record_payload(fact)?
                        )? == *entry,
                        "decoded independent X differs from retrieved journal bytes"
                    ),
                    DecodedRecord::Run(entry) => ensure!(
                        serde_json::from_value::<RunJournalEntry>(record_payload(fact)?)? == *entry,
                        "decoded Run record differs from retrieved journal bytes"
                    ),
                    DecodedRecord::Transfer(transfer) => ensure!(
                        contains_record(&record_payload(fact)?, &serde_json::to_value(transfer)?),
                        "decoded transfer differs from retrieved journal bytes"
                    ),
                }
            }
        }
        Ok(evidence)
    }

    pub async fn wait_owner(&mut self, kind: BarrierKind) -> Result<BarrierProof> {
        let mut work = self.work()?.clone();
        work.call = None;
        work.ordinal = None;
        self.wait(Barrier { work, kind }).await
    }

    pub async fn wait(&mut self, barrier: Barrier) -> Result<BarrierProof> {
        let proof = self.control.await_barrier(&barrier).await?;
        ensure!(
            proof.barrier == barrier && !proof.artifact.is_empty(),
            "barrier proof does not identify the requested work and phase"
        );
        if matches!(
            barrier.kind,
            BarrierKind::AdmissionDurable
                | BarrierKind::XDurable
                | BarrierKind::DDurable
                | BarrierKind::VDurable
        ) {
            ensure!(
                proof.journal_index.is_some(),
                "a durable barrier needs journal evidence"
            );
        }
        self.proofs.push(proof.clone());
        Ok(proof)
    }

    pub fn barrier(&self, call: &ToolCallId, kind: BarrierKind) -> Result<Barrier> {
        let mut work = self.work()?.clone();
        work.call = Some(call.to_string());
        work.ordinal = Some(1);
        Ok(Barrier { work, kind })
    }

    pub async fn release(&mut self, barrier: Barrier) -> Result<()> {
        self.control.tool(ToolControl::Release(barrier)).await
    }

    pub async fn cancel(&mut self) -> Result<()> {
        let observation = self
            .host
            .command(HostCommand::Cancel {
                run: self.work()?.run.clone(),
            })
            .await?;
        ensure!(
            observation.work.run == self.work()?.run,
            "cancel targeted another Run"
        );
        self.wait_owner(BarrierKind::RunCancelRecorded).await?;
        Ok(())
    }

    pub async fn kill_and_reopen(&mut self, proof: &BarrierProof) -> Result<()> {
        let ready = self
            .ready
            .as_ref()
            .ok_or_else(|| anyhow!("host is not ready"))?;
        let fault = Fault::KillHost {
            target: ready.process.role.clone(),
        };
        let receipt = self.control.inject(fault, proof).await?;
        ensure!(
            receipt.proof.barrier == proof.barrier && receipt.proof.artifact == proof.artifact,
            "host kill missed its requested cut"
        );
        ensure!(
            receipt.target_incarnation == ready.process.incarnation,
            "host kill hit another incarnation"
        );
        ensure!(
            matches!(&receipt.fault, Fault::KillHost {target} if target == &ready.process.role),
            "controller did not kill the selected host"
        );
        self.faults.push(receipt);
        let old_pid = ready.process.pid;
        let old_incarnation = ready.process.incarnation;
        let old_endpoint = ready.endpoint.clone();
        let namespace = self.lease.namespace.clone();
        let authority = self.lease.authority.clone();
        let directory = self.lease.directory.clone();
        let reopened = self.host.boot(self.artifact, self.lease).await?;
        ensure!(
            reopened.process.pid != old_pid && reopened.process.incarnation > old_incarnation,
            "cold reopen did not replace the killed host"
        );
        ensure!(
            reopened.endpoint == old_endpoint
                && self.lease.namespace == namespace
                && self.lease.authority == authority
                && self.lease.directory == directory,
            "cold reopen changed the admitted deployment or store identity"
        );
        self.ready = Some(reopened);
        Ok(())
    }

    pub async fn finish(
        &mut self,
        status: lash_remote_protocol::RemoteTurnStatus,
        answer: Option<&str>,
    ) -> Result<Evidence> {
        let outcome = self
            .host
            .command(HostCommand::Attach {
                run: self.work()?.run.clone(),
            })
            .await?;
        ensure!(
            outcome.work.ingress == self.work()?.ingress && outcome.work.run == self.work()?.run,
            "follow returned another input/Run"
        );
        let remote: lash_remote_protocol::RemoteSendOutcome = serde_json::from_value(
            outcome
                .output
                .get("outcome")
                .cloned()
                .ok_or_else(|| anyhow!("Attach has no typed outcome"))?,
        )?;
        let lash_remote_protocol::RemoteSendOutcome::Settled {
            report, input_id, ..
        } = remote
        else {
            return Err(anyhow!("host input did not settle: {:?}", outcome.output));
        };
        report.validate()?;
        ensure!(
            input_id == self.work()?.ingress && report.turn_id.to_string() == self.work()?.run,
            "host terminal is bound to another input/Run"
        );
        ensure!(
            report.status() == status,
            "host terminal status differs: {:?}",
            report.outcome
        );
        if let Some(answer) = answer {
            ensure!(
                report.assistant_output.safe_text == answer
                    && report.assistant_output.raw_text == answer,
                "host terminal answer differs: {:?}",
                report.assistant_output
            );
        }
        let mut evidence = self.read().await?;
        evidence.outputs.push(outcome);
        evidence.barriers.extend(self.proofs.clone());
        evidence.faults.extend(self.faults.clone());
        Ok(evidence)
    }
}

/// Transfer capture is nested in a native commit result. Compare its exact
/// serialized value with the retained bytes, independently of H0's decoder.
fn contains_record(value: &serde_json::Value, expected: &serde_json::Value) -> bool {
    if value == expected {
        return true;
    }
    match value {
        serde_json::Value::Object(fields) => fields.values().any(|v| contains_record(v, expected)),
        serde_json::Value::Array(values) => values.iter().any(|v| contains_record(v, expected)),
        serde_json::Value::String(encoded) => serde_json::from_str::<serde_json::Value>(encoded)
            .is_ok_and(|value| contains_record(&value, expected)),
        _ => false,
    }
}

/// Decode the raw V2 completion bytes independently of the collector's typed view.
fn record_payload(fact: &JournalFact) -> Result<serde_json::Value> {
    let raw = fact
        .value
        .pointer("/Notification/Completion/Run/result/Success")
        .ok_or_else(|| anyhow!("typed Run evidence is not a raw successful V2 completion"))?;
    let bytes: Vec<u8> = serde_json::from_value(raw.clone())?;
    let mut result: serde_json::Value = serde_json::from_slice(&bytes)?;
    let object = result
        .as_object_mut()
        .ok_or_else(|| anyhow!("Run record is not an object"))?;
    ensure!(
        object
            .remove("effect_journal_version")
            .and_then(|value| value.as_u64())
            == Some(u64::from(lash_restate::EFFECT_JOURNAL_VERSION)),
        "foreign effect generation"
    );
    object.remove("build_generation");
    Ok(result)
}

pub fn attempts(evidence: &Evidence) -> Vec<&lash_core::tool_run::RunAttemptEntry> {
    evidence
        .journals
        .iter()
        .filter_map(|fact| match &fact.decoded {
            Some(DecodedRecord::Attempt(entry)) => Some(entry),
            _ => None,
        })
        .collect()
}

pub fn entries(evidence: &Evidence) -> Result<Vec<&RunJournalEntry>> {
    let mut entries = Vec::new();
    let mut positions = BTreeSet::new();
    for fact in &evidence.journals {
        if let Some(DecodedRecord::Run(entry)) = &fact.decoded {
            ensure!(
                positions.insert((&fact.invocation, fact.index)),
                "duplicate raw journal position"
            );
            entries.push(entry);
        }
    }
    ensure!(
        !entries.is_empty(),
        "missing independently decoded Run records"
    );
    entries.sort_by_key(|entry| entry.record.first);
    Ok(entries)
}

pub fn events(evidence: &Evidence) -> Result<Vec<&RunEvent>> {
    Ok(entries(evidence)?
        .into_iter()
        .flat_map(|entry| &entry.record.events)
        .collect())
}

pub fn calls(evidence: &Evidence, labels: &[&str]) -> Result<BTreeMap<String, ToolCallId>> {
    let mut calls = BTreeMap::new();
    for event in events(evidence)? {
        if let RunEvent::Admitted { round } = event {
            for member in &round.members {
                ensure!(
                    labels.contains(&member.tool_name.as_str()),
                    "unexpected call admitted: {}",
                    member.tool_name
                );
                ensure!(
                    calls
                        .insert(member.tool_name.clone(), member.call_id.clone())
                        .is_none(),
                    "a tool was admitted twice"
                );
            }
        }
    }
    ensure!(
        calls.len() == labels.len(),
        "not every planned tool was admitted"
    );
    let identities: BTreeSet<_> = calls.values().collect();
    ensure!(
        identities.len() == labels.len(),
        "independent calls share an identity"
    );
    Ok(calls)
}

pub fn deliveries(evidence: &Evidence) -> Result<Vec<ToolDelivery>> {
    evidence
        .effects
        .iter()
        .filter(|value| {
            value.get("kind").and_then(|kind| kind.as_str()) == Some("h2_body_delivery")
        })
        .map(|value| serde_json::from_value(value["delivery"].clone()).map_err(Into::into))
        .collect()
}

pub fn assert_call(
    evidence: &Evidence,
    call: &ToolCallId,
    terminal: LogicalTerminal,
) -> Result<()> {
    let events = events(evidence)?;
    let receipts: Vec<_> = events
        .iter()
        .flat_map(|event| BusinessReceipt::for_event(event))
        .collect();
    ensure!(
        receipts
            .iter()
            .filter(|receipt| matches!(receipt,
        BusinessReceipt::Accepted {call_id} if call_id == call))
            .count()
            == 1,
        "original call needs one accepted receipt"
    );
    ensure!(
        receipts
            .iter()
            .filter(|receipt| matches!(receipt,
        BusinessReceipt::Terminal {call_id,..} if call_id == call))
            .count()
            == 1,
        "original call needs one terminal receipt"
    );
    ensure!(
        receipts.contains(&BusinessReceipt::Terminal {
            call_id: call.clone(),
            terminal
        }),
        "original call settled with the wrong terminal"
    );
    let trace: Vec<_> = evidence
        .effects
        .iter()
        .filter(|item| item.get("kind").and_then(|v| v.as_str()) == Some("h2_trace_record"))
        .map(|item| &item["record"])
        .filter(|record| record["type"] == "tool_receipt" && record["call_id"] == call.as_str())
        .collect();
    ensure!(
        trace
            .iter()
            .filter(|record| record["terminal"].is_null())
            .count()
            == 1,
        "actual host trace needs one original accepted receipt"
    );
    let terminal_name = match terminal {
        LogicalTerminal::Final => "final",
        LogicalTerminal::Cancelled => "cancelled",
        LogicalTerminal::Denied => "denied",
        LogicalTerminal::Aborted => "aborted",
    };
    ensure!(
        trace
            .iter()
            .filter(|record| !record["terminal"].is_null())
            .count()
            == 1
            && trace
                .iter()
                .any(|record| record["terminal"] == terminal_name),
        "actual host trace needs one original terminal receipt"
    );
    if terminal == LogicalTerminal::Final {
        ensure!(
            attempts(evidence)
                .iter()
                .filter(|entry| &entry.call_id == call && entry.attempt == AttemptOrdinal::FIRST)
                .count()
                == 1,
            "original call needs one independently durable X receipt"
        );
        for phase in ["X", "D", "V", "incorporation"] {
            let count = events
                .iter()
                .filter(|event| match (phase, event) {
                    (
                        "X",
                        RunEvent::AttemptRecorded {
                            call_id, attempt, ..
                        },
                    ) => call_id == call && *attempt == AttemptOrdinal::FIRST,
                    (
                        "D",
                        RunEvent::Decided {
                            call_id,
                            decision: CallDecision::Final { .. },
                            ..
                        },
                    ) => call_id == call,
                    ("V", RunEvent::Presented { call_id, .. })
                    | ("incorporation", RunEvent::Incorporated { call_id }) => call_id == call,
                    _ => false,
                })
                .count();
            ensure!(count == 1, "{phase} must occur once for {call}");
        }
    }
    Ok(())
}

pub fn assert_durable_prefix(before: &Evidence, after: &Evidence) -> Result<()> {
    for old in &before.journals {
        if old.decoded.is_some() {
            let new = after
                .journals
                .iter()
                .find(|new| new.invocation == old.invocation && new.index == old.index)
                .ok_or_else(|| {
                    anyhow!(
                        "cold recovery lost journal position {}:{}",
                        old.invocation,
                        old.index
                    )
                })?;
            ensure!(
                new.value == old.value && new.entry_type == old.entry_type && new.name == old.name,
                "cold recovery rewrote a durable journal fact"
            );
        }
    }
    Ok(())
}

pub fn assert_body_identity(
    evidence: &Evidence,
    call: &ToolCallId,
    count: Option<usize>,
) -> Result<()> {
    let deliveries: Vec<_> = deliveries(evidence)?
        .into_iter()
        .filter(|delivery| &delivery.call_id == call)
        .collect();
    ensure!(!deliveries.is_empty(), "real tool body never executed");
    let owner = &deliveries[0].owner;
    ensure!(
        deliveries.iter().all(|delivery| &delivery.owner == owner),
        "cold redelivery changed the recorded execution owner"
    );
    let work = evidence
        .journals
        .first()
        .ok_or_else(|| anyhow!("missing journal Run identity"))?;
    ensure!(
        deliveries.iter().all(|delivery| delivery.ordinal == 1
            && delivery
                .logical_run
                .as_ref()
                .is_some_and(|run| run.to_string() == work.work.run)),
        "crash redelivery changed its Run or advanced attempt ordinal"
    );
    if let Some(count) = count {
        ensure!(
            deliveries.len() == count,
            "unexpected body executions for {call}: {}",
            deliveries.len()
        );
    }
    Ok(())
}

pub async fn singleton(scenario: &mut Scenario<'_>, spec: &CaseSpec) -> Result<Evidence> {
    let initial = scenario.start(spec).await?;
    let ids = calls(&initial, &["echo"])?;
    let call = &ids["echo"];
    let entered = scenario.barrier(call, BarrierKind::BodyEntered)?;
    scenario.wait(entered.clone()).await?;
    scenario.release(entered).await?;
    scenario
        .wait(scenario.barrier(call, BarrierKind::VDurable)?)
        .await?;
    let evidence = scenario
        .finish(
            lash_remote_protocol::RemoteTurnStatus::Answered,
            Some("echo"),
        )
        .await?;
    assert_call(&evidence, call, LogicalTerminal::Final)?;
    assert_body_identity(&evidence, call, Some(1))?;
    Ok(evidence)
}

/// S02: both tools start; only A is acknowledged when the real host dies.
pub async fn cold_partial(scenario: &mut Scenario<'_>, spec: &CaseSpec) -> Result<Evidence> {
    ensure!(
        spec.store != StoreKind::SqliteMemory,
        "a memory store cannot prove cold recovery"
    );
    let initial = scenario.start(spec).await?;
    let ids = calls(&initial, &["a", "b"])?;
    let a = &ids["a"];
    let b = &ids["b"];
    let a_entered = scenario.barrier(a, BarrierKind::BodyEntered)?;
    let b_entered = scenario.barrier(b, BarrierKind::BodyEntered)?;
    scenario.wait(a_entered.clone()).await?;
    scenario.wait(b_entered.clone()).await?;
    scenario.release(a_entered).await?;
    let cut = scenario
        .wait(scenario.barrier(a, BarrierKind::XDurable)?)
        .await?;
    let before = scenario.read().await?;
    ensure!(
        !attempts(&before).iter().any(|entry| &entry.call_id == b),
        "B unexpectedly durable before kill"
    );
    scenario.kill_and_reopen(&cut).await?;
    scenario.wait(b_entered.clone()).await?;
    scenario.release(b_entered).await?;
    let evidence = scenario
        .finish(
            lash_remote_protocol::RemoteTurnStatus::Answered,
            Some("A|B"),
        )
        .await?;
    assert_durable_prefix(&before, &evidence)?;
    assert_call(&evidence, a, LogicalTerminal::Final)?;
    assert_call(&evidence, b, LogicalTerminal::Final)?;
    assert_body_identity(&evidence, a, Some(1))?;
    assert_body_identity(&evidence, b, None)?;
    ensure!(
        deliveries(&evidence)?
            .iter()
            .filter(|delivery| &delivery.call_id == b)
            .count()
            >= 2,
        "unfinished B was not redelivered on the cold host"
    );
    Ok(evidence)
}

/// S05: C/A settle before the kill; B is unfinished. Source order still wins
/// for the batch answer even when replay readiness differs from first execution.
pub async fn opposite_order(scenario: &mut Scenario<'_>, spec: &CaseSpec) -> Result<Evidence> {
    ensure!(
        spec.store != StoreKind::SqliteMemory,
        "S05 kills and reopens its store"
    );
    let initial = scenario.start(spec).await?;
    let ids = calls(&initial, &["a", "b", "c"])?;
    let mut body_gates = BTreeMap::new();
    for label in ["a", "b", "c"] {
        let gate = scenario.barrier(&ids[label], BarrierKind::BodyEntered)?;
        scenario.wait(gate.clone()).await?;
        body_gates.insert(label, gate);
    }
    let all_started = scenario.read().await?;
    for label in ["a", "b", "c"] {
        assert_body_identity(&all_started, &ids[label], Some(1))?;
    }
    ensure!(
        attempts(&all_started).is_empty(),
        "a body completed before every batch member started"
    );
    scenario.release(body_gates["c"].clone()).await?;
    scenario
        .wait(scenario.barrier(&ids["c"], BarrierKind::XDurable)?)
        .await?;
    scenario.release(body_gates["a"].clone()).await?;
    let cut = scenario
        .wait(scenario.barrier(&ids["a"], BarrierKind::XDurable)?)
        .await?;
    let before = scenario.read().await?;
    scenario.kill_and_reopen(&cut).await?;
    scenario.wait(body_gates["b"].clone()).await?;
    scenario.release(body_gates["b"].clone()).await?;
    let evidence = scenario
        .finish(
            lash_remote_protocol::RemoteTurnStatus::Answered,
            Some("A|B|C"),
        )
        .await?;
    assert_durable_prefix(&before, &evidence)?;
    for label in ["a", "b", "c"] {
        assert_call(&evidence, &ids[label], LogicalTerminal::Final)?;
        assert_body_identity(&evidence, &ids[label], (label != "b").then_some(1))?;
    }
    ensure!(
        deliveries(&evidence)?
            .iter()
            .filter(|delivery| delivery.call_id == ids["b"])
            .count()
            >= 2,
        "unfinished batch member B did not redeliver on the cold host"
    );
    let order: Vec<_> = events(&evidence)?
        .into_iter()
        .filter_map(|event| match event {
            RunEvent::Decided { call_id, .. } => Some(call_id),
            _ => None,
        })
        .collect();
    ensure!(
        order == [&ids["c"], &ids["a"], &ids["b"]],
        "replay lost recorded C/A/B selection"
    );
    Ok(evidence)
}

pub fn fact_for_call<'a>(evidence: &'a Evidence, call: &ToolCallId) -> Result<&'a JournalFact> {
    evidence
        .journals
        .iter()
        .find(|fact| fact.work.call.as_deref() == Some(call.as_str()))
        .ok_or_else(|| anyhow!("no journal provenance for {call}"))
}
