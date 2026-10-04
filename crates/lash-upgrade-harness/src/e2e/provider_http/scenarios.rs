//! H1 scenario selection and independent post-fault assertions. HTTP and
//! body logs prove deliveries; only decoded Restate journals prove X/D/V.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use lash_core_store::tool_run::{CallDecision, RunEvent, RunLifecycle};
use serde::{Deserialize, Serialize};

use super::HttpReceipt;
use crate::e2e::{
    case::{ArtifactIdentity, CaseSpec, Channel, StoreKind},
    control::{BarrierKind, Fault},
    evidence::{DecodedRecord, Evidence, JournalFact},
    host::HostKind,
    provider::ProviderKind,
};
use crate::node::e2e_tools::BodyDelivery;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderScenario {
    S03,
    S04,
    S06,
    S07,
    S26,
    S27,
}

impl ProviderScenario {
    pub fn id(self) -> &'static str {
        match self {
            Self::S03 => "S03",
            Self::S04 => "S04",
            Self::S06 => "S06",
            Self::S07 => "S07",
            Self::S26 => "S26",
            Self::S27 => "S27",
        }
    }

    /// F01, C02 and P58 are already landed. H0 availability never becomes
    /// an arc guard, and none of these six cases is selectable as a skip.
    pub fn spec(self, artifacts: Vec<ArtifactIdentity>) -> CaseSpec {
        let rules = match self {
            Self::S03 => vec!["R1", "L02", "L22"],
            Self::S04 => vec!["R1", "R4", "L02", "L19"],
            Self::S06 => vec!["R1", "L17"],
            Self::S07 => vec!["R1", "R5", "L03", "L17"],
            Self::S26 => vec!["R6", "R8", "L14", "L17"],
            Self::S27 => vec!["R6", "R7"],
        };
        CaseSpec {
            id: self.id().into(),
            rules: rules.into_iter().map(str::to_owned).collect(),
            host: if matches!(self, Self::S26 | Self::S27) {
                HostKind::Workbench
            } else {
                HostKind::UpgradeNode
            },
            store: StoreKind::SqliteFile,
            channel: Channel::Standard,
            provider: ProviderKind::RecordedHttp,
            restate_nodes: 1,
            artifacts,
            cuts: Vec::new(),
            expected_terminal: if self == Self::S07 {
                "Cancelled"
            } else {
                "Answered"
            }
            .into(),
            requires: Vec::new(),
        }
    }
}

pub const SCENARIOS: [ProviderScenario; 6] = [
    ProviderScenario::S03,
    ProviderScenario::S04,
    ProviderScenario::S06,
    ProviderScenario::S07,
    ProviderScenario::S26,
    ProviderScenario::S27,
];

/// A store-only read before or after the cut, supplied by the node's public
/// durable session read. No in-flight runtime snapshot substitutes for it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderStoreObservation {
    pub assistant_replies: usize,
    pub input_applications: usize,
    pub unfinished: bool,
    pub pending_inputs: usize,
    pub namespace_total: Option<u64>,
    pub input_tokens: i64,
    pub output_tokens: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ProviderCaseEvidence {
    pub scenario: ProviderScenario,
    pub evidence: Evidence,
    pub http: HttpReceipt,
    pub bodies: Vec<BodyDelivery>,
    pub before: ProviderStoreObservation,
    pub after: ProviderStoreObservation,
    /// Actual admin entries captured before killing the first incarnation.
    pub journal_prefix: Vec<JournalFact>,
}

impl ProviderCaseEvidence {
    pub fn verify(&self) -> Result<()> {
        ensure!(
            self.evidence.case == self.scenario.id(),
            "another scenario's evidence"
        );
        self.http.verify()?;
        ensure!(
            !self.evidence.artifacts.is_empty(),
            "missing prebuilt artifact identity"
        );
        ensure!(
            !self.evidence.journals.is_empty(),
            "missing actual journal evidence"
        );
        ensure!(
            self.evidence
                .journals
                .iter()
                .all(|fact| !fact.admin_url.is_empty() && fact.protocol == 7),
            "journal lacks V7/admin provenance"
        );
        ensure!(
            !self.evidence.cleanup.is_empty()
                && self.evidence.cleanup.iter().all(|receipt| receipt.closed),
            "case resources were not reaped"
        );
        ensure!(
            !self.after.unfinished && self.after.pending_inputs == 0,
            "active owner or queued input leaked"
        );
        let events = self.events();
        match self.scenario {
            ProviderScenario::S03 => {
                self.kill_at(BarrierKind::SideEffectAccepted)?;
                ensure!(
                    self.http.effects.len() >= 2 && self.http.mutations == 1,
                    "ambiguous delivery did not dedup externally"
                );
                let first = &self.http.effects[0].delivery;
                ensure!(
                    self.http
                        .effects
                        .iter()
                        .all(|effect| effect.delivery == *first),
                    "crash redelivery changed call identity or ordinal"
                );
                ensure!(
                    self.bodies.len() >= 2
                        && self.bodies.iter().all(|body| body.delivery == *first),
                    "body redelivery was not independently observed"
                );
                self.one_final(&events, &first.call_id)?;
                ensure!(
                    self.after.input_applications == 1 && self.after.assistant_replies == 1,
                    "Run settled or published twice"
                );
            }
            ProviderScenario::S04 => {
                let proposed = self
                    .evidence
                    .faults
                    .iter()
                    .find(|fault| {
                        matches!(fault.fault, Fault::KillHost { .. })
                            && matches!(
                                fault.proof.barrier.kind,
                                BarrierKind::XProposed | BarrierKind::DProposed
                            )
                    })
                    .ok_or_else(|| anyhow::anyhow!("missing host kill at actual X/D proposal"))?;
                ensure!(
                    proposed.proof.journal_index.is_none(),
                    "proposal was mislabeled durable"
                );
                ensure!(
                    self.evidence
                        .faults
                        .iter()
                        .any(|fault| matches!(fault.fault, Fault::DropConnection { .. })),
                    "ACK stream was not dropped"
                );
                ensure!(
                    self.before.namespace_total.unwrap_or(0) == 0
                        && self.before.assistant_replies == 0
                        && self.before.input_applications == 0,
                    "proposal published speculative state/output"
                );
                ensure!(
                    self.after.namespace_total == Some(1)
                        && self.after.assistant_replies == 1
                        && self.after.input_applications == 1,
                    "cold resolution did not publish once"
                );
                ensure!(!self.bodies.is_empty(), "no executed body");
                let call = &self.bodies[0].delivery.call_id;
                ensure!(
                    self.bodies
                        .iter()
                        .all(|body| body.delivery.call_id == *call && body.delivery.attempt == 1),
                    "unfinished-body redelivery changed identity"
                );
                self.one_final(&events, call)?;
                ensure!(
                    self.evidence
                        .journals
                        .iter()
                        .filter_map(|fact| match &fact.decoded {
                            Some(DecodedRecord::Run(entry)) => Some(entry.state.len()),
                            _ => None,
                        })
                        .sum::<usize>()
                        == 1,
                    "state resolution recorded more than once"
                );
            }
            ProviderScenario::S06 => {
                self.kill_at(BarrierKind::RetryBackoffEntered)?;
                self.retry_schedule(&events)?;
                self.preserved_prefix()?;
                let ready = self
                    .evidence
                    .barriers
                    .iter()
                    .filter(|proof| {
                        proof.barrier.kind == BarrierKind::XDurable
                            && proof.barrier.work.ordinal == Some(2)
                    })
                    .map(|proof| {
                        self.bodies
                            .iter()
                            .find(|body| {
                                Some(body.delivery.call_id.as_str())
                                    == proof.barrier.work.call.as_deref()
                            })
                            .map(|body| body.label.as_str())
                    })
                    .collect::<Vec<_>>();
                ensure!(
                    ready == vec![Some("B"), Some("A")],
                    "recovery did not independently observe reverse B/A durable readiness"
                );
                ensure!(
                    self.evidence
                        .faults
                        .iter()
                        .any(|fault| matches!(fault.fault, Fault::KillHost { .. })),
                    "backoff host was not killed"
                );
                ensure!(
                    self.after.input_applications == 1 && self.after.assistant_replies == 1,
                    "retry Run did not settle once"
                );
            }
            ProviderScenario::S07 => {
                self.kill_at(BarrierKind::RetryBackoffEntered)?;
                self.preserved_prefix()?;
                let terminal = self
                    .evidence
                    .outputs
                    .last()
                    .ok_or_else(|| anyhow::anyhow!("cancellation lacks public terminal"))?;
                let terminal: lash::TurnOutput = serde_json::from_value(terminal.output.clone())?;
                ensure!(
                    terminal.result.outcome.cancellation().is_some(),
                    "pending backoff lost typed cancellation"
                );
                ensure!(
                    self.evidence
                        .faults
                        .iter()
                        .any(|fault| matches!(fault.fault, Fault::KillHost { .. })),
                    "cancelled host was not restarted"
                );
                ensure!(
                    !self.bodies.is_empty()
                        && self.bodies.iter().all(|body| body.delivery.attempt == 1),
                    "cancelled backoff launched the next attempt"
                );
                ensure!(
                    !events
                        .iter()
                        .any(|event| matches!(event, RunEvent::DeclarationsIssued { .. })),
                    "cancelled backoff issued a declaration"
                );
                ensure!(
                    events.iter().any(|event| matches!(
                        event,
                        RunEvent::Lifecycle {
                            state: RunLifecycle::Closing
                        }
                    )),
                    "no journaled Run cancellation"
                );
                ensure!(
                    events
                        .iter()
                        .filter(|event| matches!(
                            event,
                            RunEvent::Lifecycle {
                                state: RunLifecycle::Settled
                            }
                        ))
                        .count()
                        == 1,
                    "issued handles did not settle once"
                );
                ensure!(
                    self.after.input_applications == 1 && self.after.assistant_replies == 0,
                    "cancellation published an answer"
                );
            }
            ProviderScenario::S26 | ProviderScenario::S27 => {
                anyhow::bail!(
                    "product-host S26/S27 evidence is deferred by the arc scope hold; cheap production-provider witnesses are separate"
                )
            }
        }
        Ok(())
    }

    fn events(&self) -> Vec<&RunEvent> {
        self.evidence
            .journals
            .iter()
            .filter_map(|fact| match &fact.decoded {
                Some(DecodedRecord::Run(entry)) => Some(entry.record.events.iter()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    fn kill_at(&self, kind: BarrierKind) -> Result<()> {
        ensure!(
            self.evidence
                .faults
                .iter()
                .any(|fault| matches!(fault.fault, Fault::KillHost { .. })
                    && fault.proof.barrier.kind == kind
                    && fault.target_incarnation > 0
                    && !fault.proof.artifact.is_empty()),
            "missing identity-bound host kill at {kind:?}"
        );
        Ok(())
    }

    fn one_final(&self, events: &[&RunEvent], call: &str) -> Result<()> {
        ensure!(events.iter().filter(|event| matches!(event,
            RunEvent::Decided { call_id, decision: CallDecision::Final { .. }, .. } if call_id.to_string() == call
        )).count() == 1, "call has other than one durable final");
        ensure!(
            events
                .iter()
                .filter(|event| matches!(event,
                    RunEvent::Presented { call_id, .. } if call_id.to_string() == call
                ))
                .count()
                == 1,
            "call has other than one durable presentation"
        );
        Ok(())
    }

    fn preserved_prefix(&self) -> Result<()> {
        ensure!(
            !self.journal_prefix.is_empty(),
            "no pre-crash dynamic command prefix"
        );
        for before in &self.journal_prefix {
            ensure!(
                self.evidence
                    .journals
                    .iter()
                    .any(|after| after.invocation == before.invocation
                        && after.index == before.index
                        && after.entry_type == before.entry_type
                        && after.name == before.name
                        && after.value == before.value),
                "recovery changed the recorded command prefix"
            );
        }
        Ok(())
    }

    fn retry_schedule(&self, events: &[&RunEvent]) -> Result<()> {
        let mut deliveries = BTreeMap::<&str, BTreeMap<u32, usize>>::new();
        for body in &self.bodies {
            *deliveries
                .entry(&body.label)
                .or_default()
                .entry(body.delivery.attempt)
                .or_default() += 1;
        }
        ensure!(
            deliveries.len() == 2
                && deliveries
                    .values()
                    .all(|attempts| attempts == &BTreeMap::from([(1, 1), (2, 1)])),
            "durable A/B attempts repeated or skipped"
        );
        let mut scheduled = BTreeSet::new();
        for event in events {
            if let RunEvent::RetryScheduled {
                call_id,
                failed,
                next,
                backoff_ms,
            } = event
            {
                ensure!(
                    failed.get() == 1 && next.get() == 2 && *backoff_ms > 0,
                    "reported retry changed ordinal or skipped real backoff"
                );
                ensure!(
                    scheduled.insert(call_id.to_string()),
                    "retry scheduled twice"
                );
            }
        }
        ensure!(
            scheduled.len() == 2,
            "A/B retries were not durably scheduled"
        );
        for call in &scheduled {
            self.one_final(events, call)?;
        }
        Ok(())
    }
}
