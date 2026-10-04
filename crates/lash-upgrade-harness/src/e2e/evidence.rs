//! Reconciled receipts, with explicit unavailable evidence.
use super::{
    case::ArtifactIdentity,
    control::{BarrierProof, CleanupReceipt, FaultReceipt, WorkIdentity},
    host::HostObservation,
};
use serde::{Deserialize, Serialize};
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum DecodedRecord {
    Attempt(lash_core_store::tool_run::RunAttemptEntry),
    Run(lash_core_store::tool_run::RunJournalEntry),
    Transfer(lash_core_store::tool_run::RunTransfer),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct JournalFact {
    pub work: WorkIdentity,
    pub invocation: String,
    pub index: u64,
    pub entry_type: String,
    pub name: Option<String>,
    pub value: serde_json::Value,
    pub decoded: Option<DecodedRecord>,
    pub admin_url: String,
    pub protocol: u32,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Evidence {
    pub case: String,
    pub artifacts: Vec<ArtifactIdentity>,
    pub journals: Vec<JournalFact>,
    pub barriers: Vec<BarrierProof>,
    pub faults: Vec<FaultReceipt>,
    pub stores: Vec<serde_json::Value>,
    pub effects: Vec<serde_json::Value>,
    pub outputs: Vec<HostObservation>,
    pub cleanup: Vec<CleanupReceipt>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    Passed,
    Failed { reason: String },
    NotRun { reason: String },
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Counts {
    pub selected: usize,
    pub executed: usize,
    pub passed: usize,
    pub failed: usize,
    pub not_run: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CaseReceipt {
    pub evidence: Evidence,
    pub verdict: Verdict,
}

pub trait EvidenceReader {
    fn collect<'a>(&'a mut self, work: &'a WorkIdentity) -> super::Step<'a, Evidence>;
}
impl Evidence {
    pub fn empty(case: String) -> Self {
        Self {
            case,
            artifacts: Vec::new(),
            journals: Vec::new(),
            barriers: Vec::new(),
            faults: Vec::new(),
            stores: Vec::new(),
            effects: Vec::new(),
            outputs: Vec::new(),
            cleanup: Vec::new(),
        }
    }
}
impl Counts {
    /// The execution policy is fail-closed, including setup and cleanup failures.
    pub fn reconcile(&self) -> anyhow::Result<()> {
        anyhow::ensure!(self.selected > 0, "zero scenarios selected");
        anyhow::ensure!(
            self.executed == self.passed + self.failed,
            "executed count differs from verdicts"
        );
        anyhow::ensure!(
            self.selected == self.executed + self.not_run,
            "selected count differs from execution receipts"
        );
        anyhow::ensure!(
            self.failed == 0 && self.not_run == 0,
            "required selection did not pass completely"
        );
        Ok(())
    }
}
/// Rows retained verbatim from sys_journal; V2 JSON contains the actual binary
/// result as a byte array, not a log line or a provider-side surrogate.
#[derive(Clone, Debug, Deserialize)]
pub struct JournalRow {
    pub index: u64,
    pub entry_type: String,
    pub name: Option<String>,
    pub version: u32,
    pub entry_json: Option<String>,
}
pub fn decode_journal(
    rows: Vec<JournalRow>,
    work: &WorkIdentity,
    invocation: &str,
    admin_url: &str,
    protocol: u32,
) -> anyhow::Result<Vec<JournalFact>> {
    anyhow::ensure!(protocol == 7, "Run attempt evidence requires observed V7");
    let mut completions = std::collections::BTreeMap::new();
    let mut facts = Vec::new();
    for row in rows {
        anyhow::ensure!(
            row.version == 2,
            "journal {}:{} is not V2",
            invocation,
            row.index
        );
        let value: serde_json::Value = serde_json::from_str(
            row.entry_json
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("journal row lacks entry_json"))?,
        )?;
        if let Some(command) = value.pointer("/Command/Run") {
            let id = command
                .get("completion_id")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| anyhow::anyhow!("Run command lacks completion identity"))?;
            completions.insert(
                id,
                row.name.clone().or_else(|| {
                    command
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .map(str::to_owned)
                }),
            );
        }
        let mut name = row.name;
        let mut decoded = None;
        if let Some(completion) = value.pointer("/Notification/Completion/Run") {
            let id = completion
                .get("completion_id")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| anyhow::anyhow!("Run completion lacks identity"))?;
            name = completions.get(&id).cloned().flatten();
            if let Some(bytes) = completion.pointer("/result/Success") {
                let bytes: Vec<u8> = serde_json::from_value(bytes.clone())?;
                let mut result: serde_json::Value = serde_json::from_slice(&bytes)?;
                if result.get("effect_journal_version").is_some()
                    && (result.get("record").is_some() || result.get("call_id").is_some())
                {
                    anyhow::ensure!(
                        result["effect_journal_version"].as_u64()
                            == Some(u64::from(lash_restate::EFFECT_JOURNAL_VERSION)),
                        "foreign effect generation before Run record decoding"
                    );
                    let object = result
                        .as_object_mut()
                        .ok_or_else(|| anyhow::anyhow!("Run result is not an object"))?;
                    object.remove("effect_journal_version");
                    object.remove("build_generation");
                    decoded = if result.get("record").is_some() {
                        Some(DecodedRecord::Run(serde_json::from_value(result)?))
                    } else {
                        Some(DecodedRecord::Attempt(serde_json::from_value(result)?))
                    };
                }
            }
        }
        facts.push(JournalFact {
            work: work.clone(),
            invocation: invocation.to_owned(),
            index: row.index,
            entry_type: row.entry_type,
            name,
            value,
            decoded,
            admin_url: admin_url.to_owned(),
            protocol,
        });
    }
    Ok(facts)
}

/// Map logical work to an actual invocation once ingress admitted it. The
/// collector refuses a missing mapping, journal or namespace-owned invocation.
pub struct RestateEvidenceReader {
    pub case: String,
    pub view: crate::restate_view::RestateView,
    pub protocol: u32,
    bindings: std::collections::BTreeMap<String, String>,
}
impl RestateEvidenceReader {
    pub fn new(case: String, view: crate::restate_view::RestateView, protocol: u32) -> Self {
        Self {
            case,
            view,
            protocol,
            bindings: Default::default(),
        }
    }
    pub fn bind(&mut self, work: &WorkIdentity, invocation: String) -> anyhow::Result<()> {
        let key = serde_json::to_string(work)?;
        if let Some(previous) = self.bindings.get(&key) {
            anyhow::ensure!(
                *previous == invocation,
                "work cannot be rebound to another invocation"
            );
        } else {
            self.bindings.insert(key, invocation);
        }
        Ok(())
    }
}
impl EvidenceReader for RestateEvidenceReader {
    fn collect<'a>(&'a mut self, work: &'a WorkIdentity) -> super::Step<'a, Evidence> {
        Box::pin(async move {
            let invocation = self
                .bindings
                .get(&serde_json::to_string(work)?)
                .ok_or_else(|| anyhow::anyhow!("work has no admitted invocation"))?;
            let mut evidence = Evidence::empty(self.case.clone());
            evidence.journals = self.view.journal(work, invocation, self.protocol).await?;
            anyhow::ensure!(
                !evidence.journals.is_empty(),
                "required journal evidence is absent"
            );
            Ok(evidence)
        })
    }
}
