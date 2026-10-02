//! Reconcile a load run's witness ledgers against the plan the workload
//! regenerates. Nothing here reads lash's store: the evidence is what the
//! driver sent and read back, what the provider served, what the synthetic
//! tools committed, and the blob bytes the attachment tool put and the
//! workers read. Under a fault campaign (FIG-4169) the fault controller's
//! ledger places each fault on the same database clock, and every fault must
//! have hit operations in flight that then reached their durable terminals.
//! Under the rolling-upgrade campaign (FIG-3805) the same ledger places each
//! upgrade step, and every session must keep answering through the roll.

use super::fault_verify::{CampaignKind, campaign_kind};
use super::{
    CancelOutcome, DeleteReport, InputOutcome, LoadContext, LoadRequest, LoadResponse,
    ReportedStatus, TurnReport,
};
use anyhow::{Context, Result};
use lash_perf::workload::{OperationId, TurnPlan};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};

/// Every evidence class a load run must witness at least once.
pub const CLASSES: [&str; 27] = [
    "turns",
    "cells",
    "provider-retries",
    "tools",
    "child-processes",
    "host-processes",
    "host-signals",
    "host-cancels",
    "queued-inputs",
    "queued-cancels",
    "turn-cancels",
    "deletes",
    "attachment-puts",
    "attachment-reads",
    "shared-attachments",
    "peer-reads",
    "cron-setup",
    "cron-ticks",
    "cron-closed-after-delete",
    "provider-streams",
    "history-prefill",
    "admin-compaction",
    "context-pressure",
    "auxiliary-requests",
    "external-occurrences",
    "trigger-edits",
    "promotion-reads",
];

use super::ledger::{FaultEvidence, LoadEvidence};
pub use super::ledger::{fault_classes, upgrade_classes};

#[derive(Clone, Debug, PartialEq)]
pub struct LoadEventRow {
    pub subject: String,
    pub evidence: LoadEvidence,
    pub observer: String,
    pub detail: Value,
    pub content_digest: Option<String>,
    /// The witness database's clock when the row was appended.
    pub recorded_at_us: i64,
}

/// One `witness_load_faults` row.
#[derive(Clone, Debug, PartialEq)]
pub struct FaultLedgerRow {
    pub evidence: FaultEvidence,
    pub target: String,
    pub detail: Value,
    pub recorded_at_us: i64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WitnessSnapshot {
    pub events: Vec<LoadEventRow>,
    /// The fault controller's rows, in order; empty without a campaign.
    pub faults: Vec<FaultLedgerRow>,
    /// `(workflow_id, scenario)` of every provider receipt under the run.
    pub receipts: Vec<(String, String)>,
    /// `(logical_key, response_digest)` of every committed effect under the run.
    pub commits: Vec<(String, String)>,
    /// Logical key of every physical effect attempt under the run.
    pub attempts: Vec<String>,
}

/// Read the run's rows from the witness ledgers.
pub async fn load_snapshot(pool: &PgPool, run: &str) -> Result<WitnessSnapshot> {
    type EventColumns = (String, String, String, String, String, Option<String>, i64);
    let events = sqlx::query_as::<_, EventColumns>(
        "SELECT subject, operation, phase, observer, detail_json, content_digest, recorded_at_us
         FROM witness_load_events WHERE run_id = $1 ORDER BY event_id",
    )
    .bind(run)
    .fetch_all(pool)
    .await
    .context("read the load events")?
    .into_iter()
    .map(
        |(subject, operation, phase, observer, detail, content_digest, recorded_at_us)| {
            Ok(LoadEventRow {
                subject,
                evidence: format!("{operation}:{phase}").parse()?,
                observer,
                detail: serde_json::from_str(&detail).context("decode a load event's detail")?,
                content_digest,
                recorded_at_us,
            })
        },
    )
    .collect::<Result<Vec<_>>>()?;
    let faults = sqlx::query_as::<_, (String, String, String, String, i64)>(
        "SELECT kind, phase, target, detail_json, recorded_at_us
         FROM witness_load_faults WHERE run_id = $1 ORDER BY fault_event_id",
    )
    .bind(run)
    .fetch_all(pool)
    .await
    .context("read the fault ledger")?
    .into_iter()
    .map(|(kind, phase, target, detail, recorded_at_us)| {
        Ok(FaultLedgerRow {
            evidence: format!("{kind}:{phase}").parse()?,
            target,
            detail: serde_json::from_str(&detail).context("decode a fault row's detail")?,
            recorded_at_us,
        })
    })
    .collect::<Result<Vec<_>>>()?;
    // Keys under the run start with `run/`; `left` avoids LIKE wildcards in
    // run IDs.
    let receipts = sqlx::query_as::<_, (String, String)>(
        "SELECT workflow_id, scenario FROM witness_provider_receipts
         WHERE left(workflow_id, length($1) + 1) = $1 || '/' ORDER BY receipt_id",
    )
    .bind(run)
    .fetch_all(pool)
    .await
    .context("read the provider receipts")?;
    let commits = sqlx::query_as::<_, (String, String)>(
        "SELECT logical_key, response_digest FROM witness_effect_commits
         WHERE left(logical_key, length($1) + 1) = $1 || '/'",
    )
    .bind(run)
    .fetch_all(pool)
    .await
    .context("read the effect commits")?;
    let attempts = sqlx::query_scalar::<_, String>(
        "SELECT logical_key FROM witness_effect_attempts
         WHERE left(logical_key, length($1) + 1) = $1 || '/'",
    )
    .bind(run)
    .fetch_all(pool)
    .await
    .context("read the effect attempts")?;
    Ok(WitnessSnapshot {
        events,
        faults,
        receipts,
        commits,
        attempts,
    })
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct Tally {
    pub witnessed: u64,
    pub violations: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Verdict {
    pub run: String,
    pub classes: BTreeMap<&'static str, Tally>,
    /// Physical effect attempts beyond one per committed key: replays and
    /// retries the idempotent receiver absorbed.
    pub absorbed_effect_attempts: u64,
}

impl Verdict {
    pub fn passed(&self) -> bool {
        self.classes
            .values()
            .all(|tally| tally.witnessed > 0 && tally.violations.is_empty())
    }

    pub fn violations(&self) -> usize {
        self.classes
            .values()
            .map(|tally| tally.violations.len())
            .sum()
    }

    /// One line per class, then the verdict line the recipe checks.
    pub fn lines(&self) -> Vec<String> {
        let mut lines: Vec<String> = self
            .classes
            .iter()
            .map(|(class, tally)| {
                format!(
                    "load witness class={class} witnessed={} violations={}",
                    tally.witnessed,
                    tally.violations.len()
                )
            })
            .collect();
        for (class, tally) in &self.classes {
            for violation in &tally.violations {
                lines.push(format!("load witness violation class={class}: {violation}"));
            }
            if tally.witnessed == 0 {
                lines.push(format!(
                    "load witness violation class={class}: no evidence (coverage incomplete)"
                ));
            }
        }
        lines.push(format!(
            "load witness verdict={} run={} classes={} covered={} violations={} absorbed_effect_attempts={}",
            if self.passed() { "passed" } else { "failed" },
            self.run,
            self.classes.len(),
            self.classes
                .values()
                .filter(|tally| tally.witnessed > 0)
                .count(),
            self.violations(),
            self.absorbed_effect_attempts
        ));
        lines
    }
}

struct Evidence<'a> {
    sent: BTreeMap<(&'a str, &'a str), Vec<&'a Value>>,
    terminal: BTreeMap<(&'a str, &'a str), Vec<&'a Value>>,
    puts: BTreeMap<&'a str, Vec<&'a LoadEventRow>>,
    reads: BTreeMap<&'a str, Vec<&'a LoadEventRow>>,
    receipts: BTreeMap<&'a str, Vec<&'a str>>,
    commits: BTreeMap<&'a str, &'a str>,
}

impl<'a> Evidence<'a> {
    fn index(snapshot: &'a WitnessSnapshot) -> Self {
        let mut evidence = Self {
            sent: BTreeMap::new(),
            terminal: BTreeMap::new(),
            puts: BTreeMap::new(),
            reads: BTreeMap::new(),
            receipts: BTreeMap::new(),
            commits: BTreeMap::new(),
        };
        for event in &snapshot.events {
            let key = (event.evidence.operation(), event.subject.as_str());
            match event.evidence {
                LoadEvidence::Sent(_) => evidence.sent.entry(key).or_default().push(&event.detail),
                LoadEvidence::Terminal(_) => evidence
                    .terminal
                    .entry(key)
                    .or_default()
                    .push(&event.detail),
                LoadEvidence::AttachmentPut => evidence
                    .puts
                    .entry(event.subject.as_str())
                    .or_default()
                    .push(event),
                LoadEvidence::AttachmentRead => evidence
                    .reads
                    .entry(event.subject.as_str())
                    .or_default()
                    .push(event),
            }
        }
        for (key, scenario) in &snapshot.receipts {
            evidence
                .receipts
                .entry(key.as_str())
                .or_default()
                .push(scenario.as_str());
        }
        for (key, digest) in &snapshot.commits {
            evidence.commits.insert(key.as_str(), digest.as_str());
        }
        evidence
    }

    fn was_sent(&self, operation: &str, subject: &str) -> bool {
        // The map is covariant in its key lifetime, so it can be read with
        // keys that live shorter than the snapshot.
        let sent: &BTreeMap<(&str, &str), Vec<&Value>> = &self.sent;
        sent.contains_key(&(operation, subject))
    }

    /// The response of the operation's successful terminal, or why it has none.
    fn response(&self, operation: &str, subject: &str) -> Result<LoadResponse, String> {
        let terminal: &BTreeMap<(&str, &str), Vec<&Value>> = &self.terminal;
        let terminals = terminal
            .get(&(operation, subject))
            .ok_or_else(|| format!("{operation} `{subject}` was sent but never ended"))?;
        let last = terminals
            .last()
            .ok_or_else(|| format!("{operation} `{subject}` has no terminal"))?;
        match last.get("response") {
            Some(response) => serde_json::from_value(response.clone())
                .map_err(|error| format!("{operation} `{subject}` answered undecodably: {error}")),
            None => Err(format!(
                "{operation} `{subject}` failed: {}",
                last.get("error").unwrap_or(&Value::Null)
            )),
        }
    }

    fn receipted(&self, key: &str, scenario: &str) -> bool {
        self.receipts
            .get(key)
            .is_some_and(|scenarios| scenarios.contains(&scenario))
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn mentions(value: &Value, needle: &str) -> bool {
    match value {
        Value::String(text) => text == needle,
        Value::Array(items) => items.iter().any(|item| mentions(item, needle)),
        Value::Object(fields) => fields
            .iter()
            .any(|(name, item)| name == needle || mentions(item, needle)),
        _ => false,
    }
}

/// Reconcile `snapshot` for `run` against the plan `load` regenerates.
pub fn verify(load: &LoadContext, run: &str, snapshot: &WitnessSnapshot) -> Result<Verdict> {
    let generator = load.generator(run)?;
    let evidence = Evidence::index(snapshot);
    let campaign = campaign_kind(snapshot)?;
    let campaign_classes: &[&'static str] = match campaign {
        None => &[],
        Some(CampaignKind::Faults) => &fault_classes(),
        Some(CampaignKind::RollingUpgrade) => &upgrade_classes(),
    };
    let mut classes: BTreeMap<&'static str, Tally> = CLASSES
        .iter()
        .chain(campaign_classes)
        .map(|class| (*class, Tally::default()))
        .collect();
    let mut note = |class: &'static str, result: Result<(), String>| {
        let tally = classes.entry(class).or_default();
        match result {
            Ok(()) => tally.witnessed += 1,
            Err(violation) => tally.violations.push(violation),
        }
    };

    let answered_inputs = answered_inputs(&evidence);

    // Turns, and everything a turn drives.
    let mut answered: BTreeMap<(u64, u64), (TurnPlan, TurnReport)> = BTreeMap::new();
    let mut planned_effects = BTreeSet::new();
    for ((operation, subject), _) in evidence.sent.range(("turn", "")..("turn\u{1}", "")) {
        let (id, _) = OperationId::parse(subject)?;
        let plan = generator.plan(id.actor, id.ordinal)?;
        for call in plan.tool_batches.iter().flatten() {
            planned_effects.insert(call.idempotency_key.clone());
        }
        for process in &plan.child_processes {
            planned_effects.insert(process.idempotency_key.clone());
        }
        let report = match evidence.response(operation, subject) {
            Ok(LoadResponse::Turn(report)) => report,
            Ok(other) => {
                note("turns", Err(format!("turn `{subject}` answered {other:?}")));
                continue;
            }
            Err(violation) => {
                note("turns", Err(violation));
                continue;
            }
        };
        let status = report.outcome.status;
        let allowed = if plan.cancel {
            matches!(status, ReportedStatus::Answered | ReportedStatus::Cancelled)
        } else {
            status == ReportedStatus::Answered
        };
        note(
            "turns",
            if allowed && report.operation == *subject {
                Ok(())
            } else {
                Err(format!(
                    "turn `{subject}` ended {status:?} (cancel planned: {}): {}",
                    plan.cancel, report.outcome.outcome
                ))
            },
        );
        if status == ReportedStatus::Answered {
            note(
                "cells",
                if report.outcome.credits_input(subject) {
                    Ok(())
                } else {
                    Err(format!(
                        "turn `{subject}` finished with {} instead of its own cell",
                        report.outcome.final_value
                    ))
                },
            );
        }
        if plan.provider_streamed && status == ReportedStatus::Answered {
            note(
                "provider-streams",
                if evidence.receipted(
                    subject,
                    &format!("load_stream_chunks_{}", plan.provider_chunks),
                ) {
                    Ok(())
                } else {
                    Err(format!(
                        "streamed turn `{subject}` has no receipt for all {} chunks",
                        plan.provider_chunks
                    ))
                },
            );
        }
        if plan.retryable_first_attempt {
            let retried = evidence.receipted(subject, "load_retryable")
                && (evidence.receipted(subject, "load_turn")
                    || status == ReportedStatus::Cancelled);
            note(
                "provider-retries",
                if retried {
                    Ok(())
                } else {
                    Err(format!(
                        "turn `{subject}` planned a retryable first attempt but the provider saw {:?}",
                        evidence.receipts.get(*subject)
                    ))
                },
            );
        } else if !evidence.receipted(subject, "load_turn") && status == ReportedStatus::Answered {
            note(
                "cells",
                Err(format!(
                    "turn `{subject}` answered without a provider receipt"
                )),
            );
        }
        if plan.cancel {
            note(
                "turn-cancels",
                match report.cancel {
                    Some(CancelOutcome::Requested | CancelOutcome::AlreadySettled) => Ok(()),
                    other => Err(format!(
                        "turn `{subject}` planned a cancel but its receipt was {other:?}"
                    )),
                },
            );
        }
        verify_queued(&evidence, &answered_inputs, &plan, &report, &mut note);
        verify_host_processes(&plan, &report, &mut note);
        if status == ReportedStatus::Answered {
            for call in plan.tool_batches.iter().flatten() {
                let expected = generator
                    .tool_result(&call.idempotency_key, call.result_bytes)
                    .and_then(|result| Ok(sha256_hex(&serde_json::to_vec(&result)?)))?;
                note(
                    "tools",
                    match evidence.commits.get(call.idempotency_key.as_str()) {
                        Some(digest) if *digest == expected => Ok(()),
                        Some(_) => Err(format!(
                            "tool `{}` committed a result other than the regenerated one",
                            call.idempotency_key
                        )),
                        None => Err(format!(
                            "tool `{}` of an answered turn never committed",
                            call.idempotency_key
                        )),
                    },
                );
            }
            for process in &plan.child_processes {
                note(
                    "child-processes",
                    if evidence
                        .commits
                        .contains_key(process.idempotency_key.as_str())
                    {
                        Ok(())
                    } else {
                        Err(format!(
                            "child `{}` of an answered turn never marked its run",
                            process.idempotency_key
                        ))
                    },
                );
            }
            answered.insert((id.actor, id.ordinal), (plan, report));
        }
    }
    for (key, _) in &snapshot.commits {
        let is_turn_effect = key.contains("/tool/") || key.contains("/child/");
        if is_turn_effect && !planned_effects.contains(key) {
            note(
                "tools",
                Err(format!(
                    "effect `{key}` was committed but no sent turn planned it"
                )),
            );
        }
    }

    verify_attachments(&generator, &evidence, &answered, &mut note)?;
    verify_cron(load, &generator, &evidence, &mut note);

    for ((operation, subject), _) in evidence
        .sent
        .range(("delete-session", "")..("delete-session\u{1}", ""))
    {
        note(
            "deletes",
            match evidence.response(operation, subject) {
                Ok(LoadResponse::DeleteSession(DeleteReport {
                    reopen_refusal: Some(_),
                    ..
                })) => Ok(()),
                Ok(other) => Err(format!(
                    "deleted session `{subject}` still opened or answered {other:?}"
                )),
                Err(violation) => Err(violation),
            },
        );
    }

    match campaign {
        None => {}
        Some(CampaignKind::Faults) => super::fault_verify::verify_faults(snapshot, &mut note),
        Some(CampaignKind::RollingUpgrade) => {
            super::upgrade_verify::verify_upgrade(snapshot, &mut note);
        }
    }

    super::behavior_verify::verify(load, run, snapshot, &mut classes)?;
    let committed = snapshot.commits.len() as u64;
    Ok(Verdict {
        run: run.to_owned(),
        classes,
        absorbed_effect_attempts: (snapshot.attempts.len() as u64).saturating_sub(committed),
    })
}

fn verify_queued(
    evidence: &Evidence<'_>,
    answered_inputs: &BTreeMap<String, AnsweredInput>,
    plan: &TurnPlan,
    report: &TurnReport,
    note: &mut impl FnMut(&'static str, Result<(), String>),
) {
    for input in &plan.queued_inputs {
        let key = input.idempotency_key.as_str();
        let Some(queued) = report.queued.iter().find(|queued| queued.key == key) else {
            let class = if input.cancel {
                "queued-cancels"
            } else {
                "queued-inputs"
            };
            note(class, Err(format!("queued input `{key}` has no report")));
            continue;
        };
        let status = queued.outcome.status;
        if input.cancel {
            let typed = match queued.cancel {
                Some(CancelOutcome::Withdrawn) => {
                    status == ReportedStatus::Cancelled && !evidence.receipted(key, "load_queued")
                }
                Some(CancelOutcome::Requested) => {
                    matches!(status, ReportedStatus::Cancelled | ReportedStatus::Answered)
                }
                Some(CancelOutcome::AlreadySettled) => status == ReportedStatus::Answered,
                _ => false,
            };
            note(
                "queued-cancels",
                if typed {
                    Ok(())
                } else {
                    Err(format!(
                        "queued input `{key}` was cancelled with {:?} but ended {status:?}",
                        queued.cancel
                    ))
                },
            );
        } else {
            note(
                "queued-inputs",
                if status == ReportedStatus::Answered
                    && (evidence.receipted(key, "load_queued")
                        || answered_by_a_receipted_input(
                            evidence,
                            answered_inputs,
                            key,
                            &queued.outcome,
                        ))
                {
                    Ok(())
                } else {
                    Err(format!(
                        "queued input `{key}` ended {status:?} under root {:?} with final value {} and provider receipts {:?}",
                        queued.outcome.root,
                        queued.outcome.final_value,
                        evidence.receipts.get(key)
                    ))
                },
            );
        }
    }
}

/// An input a root answered, as its operation's terminal reported it.
struct AnsweredInput {
    root: String,
    /// The provider scenario that receipts a model call made for this input.
    scenario: &'static str,
}

/// Every input a turn operation reported Answered under a known root, by its
/// key: the turn's own input and each of its queued inputs.
fn answered_inputs(evidence: &Evidence<'_>) -> BTreeMap<String, AnsweredInput> {
    let mut inputs = BTreeMap::new();
    for ((operation, subject), _) in evidence.sent.range(("turn", "")..("turn\u{1}", "")) {
        let Ok(LoadResponse::Turn(report)) = evidence.response(operation, subject) else {
            continue;
        };
        let turn = (report.operation.as_str(), &report.outcome, "load_turn");
        let queued = report
            .queued
            .iter()
            .map(|queued| (queued.key.as_str(), &queued.outcome, "load_queued"));
        for (key, outcome, scenario) in std::iter::once(turn).chain(queued) {
            if let (ReportedStatus::Answered, Some(root)) = (outcome.status, &outcome.root) {
                inputs.insert(
                    key.to_owned(),
                    AnsweredInput {
                        root: root.clone(),
                        scenario,
                    },
                );
            }
        }
    }
    inputs
}

/// Whether `key`, which has no model call of its own, was answered by
/// another input's receipted model call.
///
/// One root answers every input it admitted (ADR 0101 §5.2): an idle root
/// admits the open prefix of accepted inputs, up to the turn-input admission
/// bound, and a running root admits inputs at its checkpoints. The driver's
/// sessions are open-loop, so a later turn's input can wait beside an earlier
/// turn's queued input and share its root. Every input in a model request
/// receives its own receipt and tool plan. An input admitted afterward at a
/// checkpoint may share the completed cell without appearing in that request;
/// it is witnessed by the receipted input answered under the same root.
fn answered_by_a_receipted_input(
    evidence: &Evidence<'_>,
    answered_inputs: &BTreeMap<String, AnsweredInput>,
    key: &str,
    outcome: &InputOutcome,
) -> bool {
    let (Some(root), Some(answering)) = (outcome.root.as_deref(), outcome.finished_operation())
    else {
        return false;
    };
    answering != key
        && answered_inputs.get(answering).is_some_and(|input| {
            input.root == root && evidence.receipted(answering, input.scenario)
        })
}

fn verify_host_processes(
    plan: &TurnPlan,
    report: &TurnReport,
    note: &mut impl FnMut(&'static str, Result<(), String>),
) {
    for process in &plan.host_processes {
        let key = process.idempotency_key.as_str();
        let Some(started) = report.host_processes.iter().find(|host| host.key == key) else {
            note(
                "host-processes",
                Err(format!("host process `{key}` has no report")),
            );
            continue;
        };
        if process.cancel {
            note(
                "host-cancels",
                if started.cancel_requested
                    && !started.output.is_null()
                    && !mentions(&started.output, "resumed")
                {
                    Ok(())
                } else {
                    Err(format!(
                        "cancelled host process `{key}` ended {}",
                        started.output
                    ))
                },
            );
            continue;
        }
        note(
            "host-processes",
            if !process.await_result || mentions(&started.output, key) {
                Ok(())
            } else {
                Err(format!(
                    "host process `{key}` ended {} without its key",
                    started.output
                ))
            },
        );
        if process.waits_for_signal() {
            note(
                "host-signals",
                if started.signalled && mentions(&started.output, "resumed") {
                    Ok(())
                } else {
                    Err(format!(
                        "signalled host process `{key}` ended {}",
                        started.output
                    ))
                },
            );
        }
    }
}

fn verify_attachments(
    generator: &lash_perf::workload::Generator<'_>,
    evidence: &Evidence<'_>,
    answered: &BTreeMap<(u64, u64), (TurnPlan, TurnReport)>,
    note: &mut impl FnMut(&'static str, Result<(), String>),
) -> Result<()> {
    let explicit_reads = u64::from(
        generator
            .workload()
            .spec()
            .attachments
            .explicit_reads_per_blob,
    );
    let mut shared_checked = BTreeSet::new();
    for ((actor, ordinal), (plan, report)) in answered {
        for (index, planned) in plan.attachments.iter().enumerate() {
            let blob = generator.attachment(plan, index)?;
            let key = blob.blob_key.as_str();
            let session = report.session_id.as_str();
            let in_session = |row: &&&LoadEventRow| row.detail["session_id"] == session;
            let puts: Vec<_> = evidence
                .puts
                .get(key)
                .map(|rows| rows.iter().filter(in_session).collect())
                .unwrap_or_default();
            note(
                "attachment-puts",
                if !puts.is_empty()
                    && puts
                        .iter()
                        .all(|row| row.content_digest.as_deref() == Some(blob.sha256.as_str()))
                {
                    Ok(())
                } else {
                    Err(format!(
                        "blob `{key}` of answered turn {actor}/{ordinal} has {} puts in `{session}` with digests {:?}",
                        puts.len(),
                        puts.iter()
                            .map(|row| &row.content_digest)
                            .collect::<Vec<_>>()
                    ))
                },
            );
            let reads: Vec<_> = evidence
                .reads
                .get(key)
                .map(|rows| rows.iter().filter(in_session).collect())
                .unwrap_or_default();
            let intact = reads.iter().all(|row| {
                row.content_digest.as_deref() == Some(blob.sha256.as_str())
                    && row.detail["committed"] == Value::Bool(true)
            });
            note(
                "attachment-reads",
                if reads.len() as u64 >= explicit_reads && intact {
                    Ok(())
                } else {
                    Err(format!(
                        "blob `{key}` in `{session}` has {} intact={intact} reads, {explicit_reads} planned",
                        reads.len()
                    ))
                },
            );
            let putters: BTreeSet<&str> = puts.iter().map(|row| row.observer.as_str()).collect();
            for read in &reads {
                if !putters.contains(read.observer.as_str()) {
                    note("peer-reads", Ok(()));
                }
            }
            // A blob both owners' answered turns attached is shared: one
            // content-addressed attachment referenced by two sessions.
            let owners = &planned.owner_actors;
            if owners.len() == 2
                && owners
                    .iter()
                    .all(|owner| answered.contains_key(&(*owner, *ordinal)))
                && shared_checked.insert(key.to_owned())
            {
                let sessions: BTreeSet<String> = evidence
                    .puts
                    .get(key)
                    .into_iter()
                    .flatten()
                    .map(|row| row.detail["session_id"].to_string())
                    .collect();
                let ids: BTreeSet<String> = evidence
                    .puts
                    .get(key)
                    .into_iter()
                    .flatten()
                    .map(|row| row.detail["attachment_id"].to_string())
                    .collect();
                note(
                    "shared-attachments",
                    if sessions.len() == 2 && ids.len() == 1 {
                        Ok(())
                    } else {
                        Err(format!(
                            "shared blob `{key}` was put in sessions {sessions:?} under ids {ids:?}"
                        ))
                    },
                );
            }
        }
    }
    Ok(())
}

fn verify_cron(
    load: &LoadContext,
    generator: &lash_perf::workload::Generator<'_>,
    evidence: &Evidence<'_>,
    note: &mut impl FnMut(&'static str, Result<(), String>),
) {
    let setup = generator.cron_setup_key();
    if evidence.was_sent("cron-setup", &setup) {
        let subscriptions = load.workload.spec().cron.subscriptions as usize;
        note(
            "cron-setup",
            match evidence.response("cron-setup", &setup) {
                Ok(LoadResponse::CronSetup(report))
                    if report.outcome.status == ReportedStatus::Answered
                        && report.outcome.final_value["schedules"]
                            .as_array()
                            .is_some_and(|schedules| schedules.len() == subscriptions) =>
                {
                    Ok(())
                }
                Ok(other) => Err(format!("cron setup answered {other:?}")),
                Err(violation) => Err(violation),
            },
        );
    }
    for ((operation, subject), details) in evidence
        .sent
        .range(("cron-tick", "")..("cron-tick\u{1}", ""))
    {
        let after_delete = details
            .iter()
            .any(|detail| detail["after_delete"] == Value::Bool(true));
        let report = match evidence.response(operation, subject) {
            Ok(LoadResponse::CronTick(report)) => report,
            Ok(other) => {
                note(
                    "cron-ticks",
                    Err(format!("tick `{subject}` answered {other:?}")),
                );
                continue;
            }
            Err(violation) => {
                note("cron-ticks", Err(violation));
                continue;
            }
        };
        let marked = evidence.commits.contains_key(*subject);
        if after_delete {
            note(
                "cron-closed-after-delete",
                if report.started_process_ids.is_empty() && !marked {
                    Ok(())
                } else {
                    Err(format!(
                        "tick `{subject}` after the owner's delete started {:?}",
                        report.started_process_ids
                    ))
                },
            );
        } else {
            note(
                "cron-ticks",
                if report.started_process_ids.len() == 1
                    && marked
                    && report
                        .outputs
                        .iter()
                        .any(|output| mentions(output, subject))
                {
                    Ok(())
                } else {
                    Err(format!(
                        "tick `{subject}` started {:?}, marked={marked}, outputs {:?}",
                        report.started_process_ids, report.outputs
                    ))
                },
            );
        }
    }
}

/// The request a `sent` row carried, for the driver's own bookkeeping.
pub fn sent_request(detail: &Value) -> Option<LoadRequest> {
    serde_json::from_value(detail.get("request")?.clone()).ok()
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::load::{
        CronSetupReport, CronTickReport, DeletionOutcome, HostProcessReport, InputOutcome,
        QueuedReport, actor_session_id, cron_session_id, turn_id_for,
    };
    use serde_json::json;

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL and witness SQL"]
    async fn witness_sql_rejects_illegal_event_and_fault_pairs() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&std::env::var("LASH_POSTGRES_DATABASE_URL").expect("PostgreSQL service"))
            .await
            .expect("connect");
        let sql = std::fs::read_to_string(
            std::env::var("LASH_LOAD_WITNESS_SQL").expect("witness SQL path"),
        )
        .expect("witness SQL");
        sqlx::raw_sql("CREATE OR REPLACE FUNCTION pg_temp.witness_clock_us() RETURNS BIGINT LANGUAGE sql AS 'SELECT 0::BIGINT'; SET search_path = pg_temp, public;").execute(&pool).await.expect("clock");
        for table in ["witness_load_events", "witness_load_faults"] {
            let start = sql.find(&format!("CREATE TABLE {table} (")).expect("table");
            let end = sql[start..].find("\n);").expect("table end") + start + 3;
            sqlx::raw_sql(
                &sql[start..end]
                    .replace("CREATE TABLE", "CREATE TEMP TABLE")
                    .replace(
                        "DEFAULT witness_clock_us()",
                        "DEFAULT pg_temp.witness_clock_us()",
                    ),
            )
            .execute(&pool)
            .await
            .expect("witness table");
        }
        let mut accepted = Vec::new();
        for (operation, phase) in [("turn", "put"), ("attachment", "sent"), ("turn", "snet")] {
            let inserted = sqlx::query("INSERT INTO witness_load_events (run_id, subject, operation, phase, observer, detail_json) VALUES ('law', 'subject', $1, $2, 'law', '{}')").bind(operation).bind(phase).execute(&pool).await;
            if inserted.is_ok() {
                accepted.push(format!("event {operation}:{phase}"));
            }
        }
        for (kind, phase) in [
            ("campaign", "injected"),
            ("worker-kill", "started"),
            ("roll", "complete"),
        ] {
            let query = "INSERT INTO witness_load_faults (run_id, kind, phase, target, detail_json) VALUES ('law', $1, $2, 'law', '{}')";
            if sqlx::query(query)
                .bind(kind)
                .bind(phase)
                .execute(&pool)
                .await
                .is_ok()
            {
                accepted.push(format!("fault {kind}:{phase}"));
            }
        }
        assert!(
            accepted.is_empty(),
            "invalid pairs were accepted: {accepted:?}"
        );
        let events = LoadEvidence::pairs();
        let event_operations: BTreeSet<_> = events.iter().map(|pair| pair.0).collect();
        let event_phases: BTreeSet<_> = events.iter().map(|pair| pair.1).collect();
        let mut refused = 0;
        for operation in event_operations {
            for phase in &event_phases {
                let inserted = sqlx::query("INSERT INTO witness_load_events (run_id, subject, operation, phase, observer, detail_json) VALUES ('law', 'subject', $1, $2, 'law', '{}')").bind(operation).bind(*phase).execute(&pool).await;
                assert_eq!(
                    inserted.is_ok(),
                    events.contains(&(operation, *phase)),
                    "event {operation}:{phase}: {inserted:?}"
                );
                refused += usize::from(inserted.is_err());
            }
        }
        assert_eq!(refused, 12);
        let faults = FaultEvidence::pairs();
        let kinds: BTreeSet<_> = faults.iter().map(|pair| pair.0).collect();
        let phases: BTreeSet<_> = faults.iter().map(|pair| pair.1).collect();
        let mut refused = 0;
        for kind in kinds {
            for phase in &phases {
                let inserted = sqlx::query("INSERT INTO witness_load_faults (run_id, kind, phase, target, detail_json) VALUES ('law', $1, $2, 'law', '{}')").bind(kind).bind(*phase).execute(&pool).await;
                assert_eq!(
                    inserted.is_ok(),
                    faults.contains(&(kind, *phase)),
                    "fault {kind}:{phase}: {inserted:?}"
                );
                refused += usize::from(inserted.is_err());
            }
        }
        assert_eq!(refused, 19);
    }

    #[tokio::test]
    #[ignore = "requires isolated PostgreSQL"]
    async fn witness_reader_rejects_unknown_event_and_fault_pairs() {
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(1)
            .connect(&std::env::var("LASH_POSTGRES_DATABASE_URL").expect("PostgreSQL service"))
            .await
            .expect("connect");
        sqlx::raw_sql("CREATE TEMP TABLE witness_load_events (event_id BIGSERIAL, run_id TEXT, subject TEXT, operation TEXT, phase TEXT, observer TEXT, detail_json TEXT, content_digest TEXT, recorded_at_us BIGINT); CREATE TEMP TABLE witness_load_faults (fault_event_id BIGSERIAL, run_id TEXT, kind TEXT, phase TEXT, target TEXT, detail_json TEXT, recorded_at_us BIGINT); CREATE TEMP TABLE witness_provider_receipts (receipt_id BIGSERIAL, workflow_id TEXT, scenario TEXT); CREATE TEMP TABLE witness_effect_commits (logical_key TEXT, response_digest TEXT); CREATE TEMP TABLE witness_effect_attempts (logical_key TEXT);").execute(&pool).await.expect("permissive corruption fixture");
        for (operation, phase) in [("turn", "snet"), ("turn", "put"), ("attachment", "sent")] {
            sqlx::query("INSERT INTO witness_load_events (run_id, subject, operation, phase, observer, detail_json, recorded_at_us) VALUES ('law', 'subject', $1, $2, 'law', '{}', 0)").bind(operation).bind(phase).execute(&pool).await.expect("corrupt event");
            assert!(
                load_snapshot(&pool, "law").await.is_err(),
                "corrupt event {operation}:{phase} was silently read"
            );
            sqlx::query("DELETE FROM witness_load_events")
                .execute(&pool)
                .await
                .expect("reset");
        }
        for (kind, phase) in [
            ("campaign", "injected"),
            ("worker-kill", "started"),
            ("rol", "complete"),
        ] {
            sqlx::query("INSERT INTO witness_load_faults (run_id, kind, phase, target, detail_json, recorded_at_us) VALUES ('law', $1, $2, 'law', '{}', 0)").bind(kind).bind(phase).execute(&pool).await.expect("corrupt fault");
            assert!(
                load_snapshot(&pool, "law").await.is_err(),
                "corrupt fault {kind}:{phase} was silently read"
            );
            sqlx::query("DELETE FROM witness_load_faults")
                .execute(&pool)
                .await
                .expect("reset");
        }
    }

    pub(crate) const RUN: &str = "verify";

    fn event(
        operation: &str,
        phase: &str,
        subject: &str,
        observer: &str,
        detail: Value,
        content_digest: Option<String>,
    ) -> LoadEventRow {
        LoadEventRow {
            subject: subject.to_owned(),
            evidence: format!("{operation}:{phase}")
                .parse()
                .expect("valid event pair"),
            observer: observer.to_owned(),
            detail,
            content_digest,
            recorded_at_us: 0,
        }
    }

    fn outcome(status: ReportedStatus, final_value: Value) -> InputOutcome {
        InputOutcome {
            status,
            root: None,
            final_value,
            outcome: Value::Null,
        }
    }

    fn operation(
        snapshot: &mut WitnessSnapshot,
        request: &LoadRequest,
        subject: &str,
        response: &LoadResponse,
        after_delete: bool,
    ) {
        let kind = crate::load::WitnessedOperation::of(request).as_str();
        snapshot.events.push(event(
            kind,
            "sent",
            subject,
            "driver",
            json!({ "request": request, "after_delete": after_delete }),
            None,
        ));
        snapshot.events.push(event(
            kind,
            "terminal",
            subject,
            "driver",
            json!({ "response": response }),
            None,
        ));
    }

    /// The evidence a correct smoke run of the first turns would leave.
    pub(crate) fn ideal(load: &LoadContext, turns: u64) -> WitnessSnapshot {
        let generator = load.generator(RUN).expect("generator");
        let spec = load.workload.spec();
        let mut snapshot = WitnessSnapshot::default();
        let commit = |snapshot: &mut WitnessSnapshot, key: &str, digest: String| {
            snapshot.commits.push((key.to_owned(), digest));
            snapshot.attempts.push(key.to_owned());
        };
        for actor in 0..u64::from(spec.sessions) {
            let mut generation = 0;
            for ordinal in 0..turns {
                let plan = generator.plan(actor, ordinal).expect("plan");
                let key = plan.operation.key();
                let session_id = actor_session_id(RUN, actor, generation);
                let status = if plan.cancel {
                    ReportedStatus::Cancelled
                } else {
                    ReportedStatus::Answered
                };
                if plan.retryable_first_attempt {
                    snapshot
                        .receipts
                        .push((key.clone(), "load_retryable".into()));
                }
                snapshot.receipts.push((key.clone(), "load_turn".into()));
                let queued = plan
                    .queued_inputs
                    .iter()
                    .map(|input| {
                        if !input.cancel {
                            snapshot
                                .receipts
                                .push((input.idempotency_key.clone(), "load_queued".into()));
                        }
                        QueuedReport {
                            key: input.idempotency_key.clone(),
                            during_active_turn: input.during_active_turn,
                            cancel: input.cancel.then_some(CancelOutcome::Withdrawn),
                            outcome: if input.cancel {
                                outcome(ReportedStatus::Cancelled, Value::Null)
                            } else {
                                outcome(
                                    ReportedStatus::Answered,
                                    json!({ "operation": input.idempotency_key }),
                                )
                            },
                        }
                    })
                    .collect();
                let host_processes = plan
                    .host_processes
                    .iter()
                    .map(|process| HostProcessReport {
                        key: process.idempotency_key.clone(),
                        process_id: format!("process-{}", process.idempotency_key),
                        created: true,
                        signalled: process.waits_for_signal() && !process.cancel,
                        cancel_requested: process.cancel,
                        output: if process.cancel {
                            json!({ "type": "settled", "output": { "error": "cancelled" } })
                        } else if process.waits_for_signal() {
                            json!({ "type": "settled", "output": {
                                "key": process.idempotency_key, "resumed": { "signal": "resume" }
                            } })
                        } else {
                            json!({ "type": "settled", "output": {
                                "key": process.idempotency_key, "synthetic": true
                            } })
                        },
                    })
                    .collect();
                if status == ReportedStatus::Answered {
                    for call in plan.tool_batches.iter().flatten() {
                        let result = generator
                            .tool_result(&call.idempotency_key, call.result_bytes)
                            .expect("tool result");
                        commit(
                            &mut snapshot,
                            &call.idempotency_key,
                            sha256_hex(&serde_json::to_vec(&result).expect("encode")),
                        );
                    }
                    for process in &plan.child_processes {
                        commit(&mut snapshot, &process.idempotency_key, "mark".into());
                    }
                    for index in 0..plan.attachments.len() {
                        let blob = generator.attachment(&plan, index).expect("blob");
                        let detail = json!({
                            "session_id": session_id,
                            "attachment_id": lash::attachments::content_id(&blob.bytes).to_string(),
                            "committed": true,
                        });
                        snapshot.events.push(event(
                            "attachment",
                            "put",
                            &blob.blob_key,
                            "worker-a",
                            detail.clone(),
                            Some(blob.sha256.clone()),
                        ));
                        for _ in 0..spec.attachments.explicit_reads_per_blob {
                            snapshot.events.push(event(
                                "attachment",
                                "read",
                                &blob.blob_key,
                                "worker-b",
                                detail.clone(),
                                Some(blob.sha256.clone()),
                            ));
                        }
                    }
                }
                let request = LoadRequest::Turn {
                    workload_sha256: load.sha256().to_owned(),
                    run: RUN.into(),
                    actor,
                    ordinal,
                    session_id: session_id.clone(),
                };
                let response = LoadResponse::Turn(TurnReport {
                    worker_id: "worker-a".into(),
                    operation: key.clone(),
                    session_id: session_id.clone(),
                    outcome: outcome(
                        status,
                        if status == ReportedStatus::Answered {
                            json!({ "operation": key })
                        } else {
                            Value::Null
                        },
                    ),
                    cancel: plan.cancel.then_some(CancelOutcome::Requested),
                    queued,
                    host_processes,
                });
                operation(&mut snapshot, &request, &key, &response, false);
                if plan.delete || plan.rotate {
                    let request = LoadRequest::DeleteSession {
                        run: RUN.into(),
                        session_id: session_id.clone(),
                    };
                    let response = LoadResponse::DeleteSession(DeleteReport {
                        worker_id: "worker-b".into(),
                        session_id: session_id.clone(),
                        deletion: DeletionOutcome::Deleted,
                        reopen_refusal: Some("UnknownSession".into()),
                        refused_after_ms: 0,
                        closing_waits: None,
                    });
                    operation(&mut snapshot, &request, &session_id, &response, false);
                    generation += 1;
                }
            }
        }
        let schedules: Vec<String> = (0..u64::from(spec.cron.subscriptions))
            .map(|subscription| generator.cron_schedule(subscription))
            .collect();
        operation(
            &mut snapshot,
            &LoadRequest::CronSetup {
                workload_sha256: load.sha256().to_owned(),
                run: RUN.into(),
                session_id: cron_session_id(RUN),
            },
            &generator.cron_setup_key(),
            &LoadResponse::CronSetup(CronSetupReport {
                worker_id: "worker-a".into(),
                outcome: outcome(ReportedStatus::Answered, json!({ "schedules": schedules })),
            }),
            false,
        );
        for subscription in 0..u64::from(spec.cron.subscriptions) {
            for (tick, after_delete) in [(0, false), (1, true)] {
                let key = generator.cron_tick_key(subscription, tick);
                let started = if after_delete {
                    Vec::new()
                } else {
                    commit(&mut snapshot, &key, "mark".into());
                    vec![format!("process-{key}")]
                };
                operation(
                    &mut snapshot,
                    &LoadRequest::CronTick {
                        workload_sha256: load.sha256().to_owned(),
                        run: RUN.into(),
                        subscription,
                        tick,
                    },
                    &key,
                    &LoadResponse::CronTick(CronTickReport {
                        worker_id: "worker-a".into(),
                        schedule: generator.cron_schedule(subscription),
                        key: key.clone(),
                        outputs: started.iter().map(|_| json!({ "key": key })).collect(),
                        started_process_ids: started,
                    }),
                    after_delete,
                );
            }
        }
        super::behavior_tests::add(&mut snapshot);
        snapshot
    }

    pub(crate) fn smoke() -> LoadContext {
        LoadContext::named("smoke-v1").expect("smoke workload")
    }

    pub(crate) fn verdict(snapshot: &WitnessSnapshot) -> Verdict {
        verify(&smoke(), RUN, snapshot).expect("verify")
    }

    pub(crate) fn violated(verdict: &Verdict, class: &str) -> bool {
        !verdict.classes[class].violations.is_empty()
    }

    #[test]
    fn a_correct_smoke_run_witnesses_every_class_without_violations() {
        let verdict = verdict(&ideal(
            &smoke(),
            lash_perf::workload::SMOKE_TURNS_PER_SESSION,
        ));
        for line in verdict.lines() {
            eprintln!("{line}");
        }
        assert!(verdict.passed(), "{:?}", verdict.lines());
        assert_eq!(verdict.classes.len(), CLASSES.len());
        assert!(
            verdict
                .lines()
                .last()
                .is_some_and(|line| line.starts_with("load witness verdict=passed"))
        );
    }

    #[test]
    fn missing_or_foreign_evidence_is_a_violation_in_its_class() {
        let load = smoke();
        let turns = lash_perf::workload::SMOKE_TURNS_PER_SESSION;
        let base = ideal(&load, turns);
        let tool_key = base
            .commits
            .iter()
            .find(|(key, _)| key.contains("/tool/"))
            .map(|(key, _)| key.clone())
            .expect("a tool commit");

        let mut lost = base.clone();
        lost.commits.retain(|(key, _)| *key != tool_key);
        assert!(violated(&verdict(&lost), "tools"));

        let mut forged = base.clone();
        for (key, digest) in &mut forged.commits {
            if *key == tool_key {
                *digest = "0".repeat(64);
            }
        }
        assert!(violated(&verdict(&forged), "tools"));

        let mut unplanned = base.clone();
        unplanned
            .commits
            .push((format!("{RUN}/0/99/tool/0/0"), "0".repeat(64)));
        assert!(violated(&verdict(&unplanned), "tools"));

        let mut corrupted = base.clone();
        if let Some(read) = corrupted
            .events
            .iter_mut()
            .find(|event| event.evidence.phase() == "read")
        {
            read.content_digest = Some("0".repeat(64));
        }
        assert!(violated(&verdict(&corrupted), "attachment-reads"));

        let mut unshared = base.clone();
        let shared = unshared
            .events
            .iter()
            .filter(|event| event.evidence.phase() == "put")
            .map(|event| event.subject.clone())
            .find(|subject| {
                base.events
                    .iter()
                    .filter(|event| event.evidence.phase() == "put" && &event.subject == subject)
                    .count()
                    == 2
            })
            .expect("a shared blob");
        let mut dropped = false;
        unshared.events.retain(|event| {
            let drop = !dropped && event.evidence.phase() == "put" && event.subject == shared;
            dropped |= drop;
            !drop
        });
        let unshared = verdict(&unshared);
        assert!(violated(&unshared, "shared-attachments"));

        let mut unsubscribed = base.clone();
        for event in &mut unsubscribed.events {
            if event.evidence.operation() == "cron-tick"
                && event.evidence.phase() == "terminal"
                && event.subject.ends_with("/tick/1")
            {
                event.detail["response"]["started_process_ids"] = json!(["process-late"]);
            }
        }
        assert!(violated(
            &verdict(&unsubscribed),
            "cron-closed-after-delete"
        ));

        let mut unanswered = base.clone();
        let first_turn = unanswered
            .events
            .iter()
            .position(|event| {
                event.evidence.operation() == "turn" && event.evidence.phase() == "terminal"
            })
            .expect("a turn terminal");
        unanswered.events[first_turn].detail = json!({ "error": "HTTP 500" });
        assert!(violated(&verdict(&unanswered), "turns"));

        let mut unreceipted = base.clone();
        unreceipted
            .receipts
            .retain(|(_, scenario)| scenario != "load_queued");
        assert!(violated(&verdict(&unreceipted), "queued-inputs"));

        let empty = verdict(&WitnessSnapshot::default());
        assert!(!empty.passed());
        assert!(
            empty
                .lines()
                .iter()
                .any(|line| line.contains("coverage incomplete"))
        );
    }

    /// Two answered queued inputs of one actor's answered turns, in their
    /// plans' order.
    fn queued_pair(load: &LoadContext) -> (String, String) {
        let generator = load.generator(RUN).expect("generator");
        for actor in 0..u64::from(load.workload.spec().sessions) {
            let answered: Vec<String> = (0..lash_perf::workload::SMOKE_TURNS_PER_SESSION)
                .map(|ordinal| generator.plan(actor, ordinal).expect("plan"))
                .filter(|plan| !plan.cancel)
                .flat_map(|plan| plan.queued_inputs)
                .filter(|input| !input.cancel)
                .map(|input| input.idempotency_key)
                .collect();
            if let [first, second, ..] = answered.as_slice() {
                return (first.clone(), second.clone());
            }
        }
        panic!("the smoke workload plans two answered queued inputs in one session");
    }

    /// Point the reported outcome of input `key` (a turn's own input or one
    /// of its queued inputs) at `root`, answered by the cell of `operation`.
    pub(crate) fn answer(snapshot: &mut WitnessSnapshot, key: &str, root: &str, operation: &str) {
        let answered = json!({ "operation": operation, "synthetic": true });
        for event in &mut snapshot.events {
            if event.evidence.operation() != "turn" || event.evidence.phase() != "terminal" {
                continue;
            }
            let response = &mut event.detail["response"];
            if response["operation"] == key {
                response["outcome"]["root"] = json!(root);
                response["outcome"]["final_value"] = answered;
                return;
            }
            if let Some(queued) = response["queued"]
                .as_array_mut()
                .and_then(|queued| queued.iter_mut().find(|queued| queued["key"] == key))
            {
                queued["outcome"]["root"] = json!(root);
                queued["outcome"]["final_value"] = answered;
                return;
            }
        }
        panic!("no reported outcome for `{key}`");
    }

    fn unreceipt(snapshot: &mut WitnessSnapshot, key: &str) {
        snapshot.receipts.retain(|(receipted, _)| receipted != key);
    }

    /// FIG-4249: one root answers every input it admitted (ADR 0101 §5.2),
    /// and the provider receipts its one model call under the latest input
    /// the request carries. The earlier input has no receipt of its own; its
    /// root's cell finished with the later input's key, which was answered
    /// under the same root and receipted.
    #[test]
    fn a_queued_input_answered_by_a_receipted_input_of_its_root_is_witnessed() {
        let load = smoke();
        let base = ideal(&load, lash_perf::workload::SMOKE_TURNS_PER_SESSION);
        let (earlier, later) = queued_pair(&load);

        let mut shared = base.clone();
        let root = turn_id_for(&earlier);
        answer(&mut shared, &earlier, &root, &later);
        answer(&mut shared, &later, &root, &later);
        unreceipt(&mut shared, &earlier);
        let shared = verdict(&shared);
        assert!(shared.passed(), "{:?}", shared.lines());

        // An input applied at a checkpoint of its own turn's root, which
        // then finished with the turn's cell.
        let (id, _) = OperationId::parse(&earlier).expect("a queued key");
        let turn = id.key();
        let mut folded = base.clone();
        let root = turn_id_for(&turn);
        answer(&mut folded, &turn, &root, &turn);
        answer(&mut folded, &earlier, &root, &turn);
        unreceipt(&mut folded, &earlier);
        let folded = verdict(&folded);
        assert!(folded.passed(), "{:?}", folded.lines());
    }

    /// Nothing but a receipted model call of the same root witnesses an
    /// answered input that has no receipt of its own.
    #[test]
    fn a_queued_input_without_a_receipted_answer_of_its_root_is_a_violation() {
        let load = smoke();
        let base = ideal(&load, lash_perf::workload::SMOKE_TURNS_PER_SESSION);
        let (earlier, later) = queued_pair(&load);
        let root = turn_id_for(&earlier);
        let mut shared = base.clone();
        answer(&mut shared, &earlier, &root, &later);
        answer(&mut shared, &later, &root, &later);
        unreceipt(&mut shared, &earlier);

        let mut other_root = shared.clone();
        answer(&mut other_root, &later, &turn_id_for(&later), &later);
        assert!(violated(&verdict(&other_root), "queued-inputs"));

        let mut unreceipted = shared.clone();
        unreceipt(&mut unreceipted, &later);
        assert!(violated(&verdict(&unreceipted), "queued-inputs"));

        let mut rootless = shared.clone();
        for event in &mut rootless.events {
            for queued in event.detail["response"]["queued"]
                .as_array_mut()
                .into_iter()
                .flatten()
            {
                if queued["key"] == earlier.as_str() {
                    queued["outcome"]["root"] = Value::Null;
                }
            }
        }
        assert!(violated(&verdict(&rootless), "queued-inputs"));

        let mut self_answered = shared.clone();
        answer(&mut self_answered, &earlier, &root, &earlier);
        assert!(violated(&verdict(&self_answered), "queued-inputs"));

        let mut unanswered = shared;
        answer(
            &mut unanswered,
            &earlier,
            &root,
            "an operation no input reported",
        );
        assert!(violated(&verdict(&unanswered), "queued-inputs"));
    }
}

#[cfg(test)]
#[path = "behavior_tests.rs"]
mod behavior_tests;
