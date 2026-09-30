//! Reconcile a load run's witness ledgers against the plan the workload
//! regenerates. Nothing here reads lash's store: the evidence is what the
//! driver sent and read back, what the provider served, what the synthetic
//! tools committed, and the blob bytes the attachment tool put and the
//! workers read.

use super::{
    CancelOutcome, DeleteReport, LoadContext, LoadRequest, LoadResponse, ReportedStatus, TurnReport,
};
use anyhow::{Context, Result};
use lash_perf::workload::{OperationId, TurnPlan};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use std::collections::{BTreeMap, BTreeSet};

/// Every evidence class a load run must witness at least once.
pub const CLASSES: [&str; 19] = [
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
];

#[derive(Clone, Debug, PartialEq)]
pub struct LoadEventRow {
    pub subject: String,
    pub operation: String,
    pub phase: String,
    pub observer: String,
    pub detail: Value,
    pub content_digest: Option<String>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct WitnessSnapshot {
    pub events: Vec<LoadEventRow>,
    /// `(workflow_id, scenario)` of every provider receipt under the run.
    pub receipts: Vec<(String, String)>,
    /// `(logical_key, response_digest)` of every committed effect under the run.
    pub commits: Vec<(String, String)>,
    /// Logical key of every physical effect attempt under the run.
    pub attempts: Vec<String>,
}

/// Read the run's rows from the witness ledgers.
pub async fn load_snapshot(pool: &PgPool, run: &str) -> Result<WitnessSnapshot> {
    let events = sqlx::query_as::<_, (String, String, String, String, String, Option<String>)>(
        "SELECT subject, operation, phase, observer, detail_json, content_digest
         FROM witness_load_events WHERE run_id = $1 ORDER BY event_id",
    )
    .bind(run)
    .fetch_all(pool)
    .await
    .context("read the load events")?
    .into_iter()
    .map(
        |(subject, operation, phase, observer, detail, content_digest)| {
            Ok(LoadEventRow {
                subject,
                operation,
                phase,
                observer,
                detail: serde_json::from_str(&detail).context("decode a load event's detail")?,
                content_digest,
            })
        },
    )
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
            let key = (event.operation.as_str(), event.subject.as_str());
            match event.phase.as_str() {
                "sent" => evidence.sent.entry(key).or_default().push(&event.detail),
                "terminal" => evidence
                    .terminal
                    .entry(key)
                    .or_default()
                    .push(&event.detail),
                "put" => evidence
                    .puts
                    .entry(event.subject.as_str())
                    .or_default()
                    .push(event),
                "read" => evidence
                    .reads
                    .entry(event.subject.as_str())
                    .or_default()
                    .push(event),
                _ => {}
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
    let mut classes: BTreeMap<&'static str, Tally> = CLASSES
        .iter()
        .map(|class| (*class, Tally::default()))
        .collect();
    let mut note = |class: &'static str, result: Result<(), String>| {
        let tally = classes.entry(class).or_default();
        match result {
            Ok(()) => tally.witnessed += 1,
            Err(violation) => tally.violations.push(violation),
        }
    };

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
                if report.outcome.finished_operation() == Some(subject) {
                    Ok(())
                } else {
                    Err(format!(
                        "turn `{subject}` finished with {} instead of its own cell",
                        report.outcome.final_value
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
        verify_queued(&evidence, &plan, &report, &mut note);
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

    let committed = snapshot.commits.len() as u64;
    Ok(Verdict {
        run: run.to_owned(),
        classes,
        absorbed_effect_attempts: (snapshot.attempts.len() as u64).saturating_sub(committed),
    })
}

fn verify_queued(
    evidence: &Evidence<'_>,
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
            // An input accepted while a root runs may be applied at one of
            // its checkpoints: lash then answers it under that root, whose
            // own receipts witness the model call.
            let folded =
                queued.outcome.root.is_some() && queued.outcome.root == report.outcome.root;
            note(
                "queued-inputs",
                if status == ReportedStatus::Answered
                    && (evidence.receipted(key, "load_queued") || folded)
                {
                    Ok(())
                } else {
                    Err(format!(
                        "queued input `{key}` ended {status:?} with provider receipts {:?}",
                        evidence.receipts.get(key)
                    ))
                },
            );
        }
    }
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
mod tests {
    use super::*;
    use crate::load::{
        CronSetupReport, CronTickReport, DeletionOutcome, HostProcessReport, InputOutcome,
        QueuedReport, actor_session_id, cron_session_id,
    };
    use serde_json::json;

    const RUN: &str = "verify";

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
            operation: operation.to_owned(),
            phase: phase.to_owned(),
            observer: observer.to_owned(),
            detail,
            content_digest,
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
    fn ideal(load: &LoadContext, turns: u64) -> WitnessSnapshot {
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
        snapshot
    }

    fn smoke() -> LoadContext {
        LoadContext::named("smoke-v1").expect("smoke workload")
    }

    fn verdict(snapshot: &WitnessSnapshot) -> Verdict {
        verify(&smoke(), RUN, snapshot).expect("verify")
    }

    fn violated(verdict: &Verdict, class: &str) -> bool {
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
            .find(|event| event.phase == "read")
        {
            read.content_digest = Some("0".repeat(64));
        }
        assert!(violated(&verdict(&corrupted), "attachment-reads"));

        let mut unshared = base.clone();
        let shared = unshared
            .events
            .iter()
            .filter(|event| event.phase == "put")
            .map(|event| event.subject.clone())
            .find(|subject| {
                base.events
                    .iter()
                    .filter(|event| event.phase == "put" && &event.subject == subject)
                    .count()
                    == 2
            })
            .expect("a shared blob");
        let mut dropped = false;
        unshared.events.retain(|event| {
            let drop = !dropped && event.phase == "put" && event.subject == shared;
            dropped |= drop;
            !drop
        });
        let unshared = verdict(&unshared);
        assert!(violated(&unshared, "shared-attachments"));

        let mut unsubscribed = base.clone();
        for event in &mut unsubscribed.events {
            if event.operation == "cron-tick"
                && event.phase == "terminal"
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
            .position(|event| event.operation == "turn" && event.phase == "terminal")
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
}
