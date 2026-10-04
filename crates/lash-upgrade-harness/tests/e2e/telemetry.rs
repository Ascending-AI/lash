//! S34 / R8 / L14: host-owned OTLP delivery, identity and outage evidence.
//! Socket proof is independent of the host JSONL sink and business result.

#[path = "../../../../examples/e2e-consumer/src/telemetry.rs"]
mod host_telemetry;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context as _, Result, ensure};
use host_telemetry::HostTelemetry;
use lash_upgrade_harness::e2e::otlp::OtlpReceiver;

const DEADLINE: Duration = Duration::from_secs(30);

#[test]
#[ignore = "prebuilt external consumer and private real Restate supplied by the E2E controller"]
fn s34_otlp_retry_transfer_and_shutdown() -> Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .thread_stack_size(8 * 1024 * 1024)
        .build()?
        .block_on(s34())
}

async fn s34() -> Result<()> {
    use lash_upgrade_harness::e2e::{
        case::{ArtifactIdentity, CaseLease},
        cluster::{ClusterControl as _, LocalCluster},
        control::WorkIdentity,
        evidence::{CaseReceipt, DecodedRecord, Evidence, Verdict},
        host::{HostAdapter as _, HostCommand},
        host_adapters::consumer::ConsumerHost,
    };
    use lash_upgrade_harness::restate_view::RestateView;
    use serde_json::json;
    use std::collections::{BTreeMap, BTreeSet};
    use std::path::PathBuf;
    use std::time::Instant;

    let required = |name: &str| -> Result<String> {
        std::env::var(name).with_context(|| {
            format!("{name} is required for S34; setup cannot pass without execution")
        })
    };
    let candidate = required("LASH_E2E_CANDIDATE_SHA")?;
    let root = PathBuf::from(required("LASH_E2E_ARTIFACT_DIR")?);
    std::fs::create_dir_all(&root)?;
    let deadline = Instant::now() + Duration::from_secs(180);
    let mut lease = CaseLease::new("s34", root.join("s34"), deadline)?;
    let artifact = |role: &str, path: PathBuf, generation: String| -> Result<ArtifactIdentity> {
        Ok(ArtifactIdentity {
            role: role.into(),
            sha256: lash_core::stable_hash::sha256_hex(&std::fs::read(&path)?),
            path,
            candidate_sha: candidate.clone(),
            generation,
        })
    };
    let restate = artifact(
        "restate-server",
        required("LASH_RESTATE_SERVER_BIN")?.into(),
        "V7".into(),
    )?;
    let controller = artifact(
        "e2e-controller",
        std::env::current_exe()?,
        "candidate".into(),
    )?;
    let consumer = ArtifactIdentity {
        role: "external-consumer".into(),
        path: required("LASH_E2E_CONSUMER_BIN")?.into(),
        sha256: required("LASH_E2E_CONSUMER_SHA256")?,
        candidate_sha: candidate,
        generation: required("LASH_E2E_CONSUMER_GENERATION")?,
    };
    restate.verify()?;
    consumer.verify()?;
    let port: u16 = required("LASH_E2E_PORT_BASE")?.parse()?;
    lease.ports.extend([port + 22, port + 23, port + 24]);
    let mut cluster = LocalCluster::new(port, deadline);
    let receiver = OtlpReceiver::bind(format!("127.0.0.1:{}", port + 24).parse()?).await?;
    let trace = lease.directory.join("trace.jsonl");
    let mut host = ConsumerHost::new(
        format!("http://127.0.0.1:{}", port + 1),
        format!("http://127.0.0.1:{}", port + 2),
        port + 22,
        port + 23,
    )?
    .configure(BTreeMap::from([
        ("E2E_CONSUMER_STORE".into(), "file".into()),
        ("E2E_CONSUMER_OTLP_ENDPOINT".into(), receiver.endpoint()),
        ("E2E_CONSUMER_TRACE".into(), trace.display().to_string()),
        ("E2E_CONSUMER_RUN_EFFECT_BUDGET".into(), "1".into()),
        ("E2E_CONSUMER_SCENARIO".into(), "S34".into()),
    ]))?;
    let mut evidence = Evidence::empty("S34".into());
    evidence.artifacts = vec![restate.clone(), consumer.clone(), controller];
    let mut execution = false;
    let result = async {
        let live = cluster.boot(&restate, 1, &mut lease).await?;
        let node = live.nodes.first().context("private cluster has no node")?;
        // The cluster's actual endpoint receipt is authoritative.
        host.ingress_url = node.ingress_url.clone();
        host.admin_url = node.admin_url.clone();
        let ready = host.boot(&consumer, &mut lease).await?;
        ensure!(ready.protocol == 7, "consumer did not negotiate V7");
        execution = true;
        let accepted = host
            .command(HostCommand::Submit {
                session: "s34-session".into(),
                idempotency_key: "s34-input".into(),
                input: json!("S34 retry and transfer"),
            })
            .await?;
        evidence.outputs.push(accepted.clone());
        let first = host
            .control(reqwest::Method::GET, "/control/s34/wait/1", None)
            .await?;
        evidence.effects.push(first.clone());
        let call = first["call_id"]
            .as_str()
            .context("body lacks original call identity")?
            .to_owned();
        let run = host.binding(&accepted.work.ingress).await?;
        let initial: host_telemetry::ExportReceipt = serde_json::from_value(
            host.control(reqwest::Method::POST, "/control/telemetry/flush", None)
                .await?,
        )?;
        evidence.effects.push(json!({"initial_export":initial}));
        ensure!(
            initial.dropped_spans == 0
                && initial.acknowledged_spans > 0
                && initial.flush_error.is_none(),
            "admission export was not acknowledged: {initial:?}"
        );
        receiver
            .wait_for(DEADLINE, |receipt| {
                receipt.spans.len() as u64 == initial.acknowledged_spans
            })
            .await?;

        receiver.disconnect();
        host.control(reqwest::Method::POST, "/control/s34/release/1", None)
            .await?;
        let second = host
            .control(reqwest::Method::GET, "/control/s34/wait/2", None)
            .await?;
        evidence.effects.push(second.clone());
        ensure!(
            second["call_id"] == first["call_id"]
                && first["ordinal"] == 1
                && second["ordinal"] == 2,
            "reported retry changed logical identity or re-used its ordinal"
        );
        let outage: host_telemetry::ExportReceipt = serde_json::from_value(
            host.control(reqwest::Method::POST, "/control/telemetry/flush", None)
                .await?,
        )?;
        evidence.effects.push(json!({"outage_export":outage}));
        receiver
            .wait_for(DEADLINE, |receipt| receipt.disconnected_requests > 0)
            .await?;
        ensure!(
            outage.dropped_spans > 0 && outage.acknowledged_spans == initial.acknowledged_spans,
            "actual disconnected export was not reported: {outage:?}"
        );
        receiver.reconnect();
        host.control(reqwest::Method::POST, "/control/s34/release/2", None)
            .await?;
        host.control(reqwest::Method::GET, "/control/s34/wait/3", None)
            .await?;

        let head = host
            .control(reqwest::Method::GET, "/control/s34/head/s34-session", None)
            .await?;
        let snapshot = read_only_head(
            &lease.directory.join("consumer-data/durable-core.db"),
            "s34-session",
        )?;
        ensure!(
            head["head_revision"] == snapshot["head_revision"]
                && head["pending_follow_on"] == snapshot["pending_follow_on"],
            "host store projection differs from independent SQLite head snapshot"
        );
        ensure!(
            head["published_by_shift"] == true
                && head["head_revision"]
                    .as_u64()
                    .is_some_and(|revision| revision > 0),
            "handover lacks a fenced published head"
        );
        let pending: lash::persistence::PendingFollowOn =
            serde_json::from_value(head["pending_follow_on"].clone())?;
        let transfer = pending
            .continuation
            .as_ref()
            .and_then(|continuation| continuation.opener.run.as_deref())
            .context("published follow-on lacks canonical Run transfer")?;
        std::fs::write(
            lease.directory.join("s34-retained-head.json"),
            serde_json::to_vec_pretty(&snapshot)?,
        )?;
        evidence
            .stores
            .push(json!({"control":head,"independent_sqlite":snapshot}));

        let view = RestateView::new(&node.admin_url, &lease.namespace)?;
        #[derive(serde::Deserialize)]
        struct Segment {
            target_service_name: String,
            target_service_key: String,
            #[serde(flatten)]
            invocation: lash_upgrade_harness::restate_view::Invocation,
        }
        let service = view.service_name("LashTurn");
        let rows: Vec<Segment> = view.query(&format!(
            "SELECT target_service_name, target_service_key, id, status, pinned_deployment_id, invoked_by_id, last_failure, retry_count FROM sys_invocation WHERE target_handler_name = 'run' AND target_service_name LIKE '{}%' ORDER BY created_at",
            service.replace('\'', "''")
        )).await?;
        let segments: Vec<_> = rows.into_iter().filter(|row|
            row.target_service_name == service || row.target_service_name.starts_with(&format!("{service}_g"))
        ).collect();
        ensure!(transfer.owner == lash_core_store::effect_opener::EffectOpener::turn("s34-session", lash::TurnId::fixture(run.clone())),
            "retained transfer belongs to another logical Run");
        ensure!(
            segments.len() >= 2,
            "Standard work never transferred to a successor"
        );
        let mut invocations = BTreeSet::new();
        for segment in &segments {
            let invocation = &segment.invocation;
            invocations.insert(invocation.id.clone());
            evidence.stores.push(json!({"actual_invocation":invocation.id,"target_service":segment.target_service_name,"target_key":segment.target_service_key}));
            let work = WorkIdentity {
                ingress: accepted.work.ingress.clone(),
                run: run.clone(),
                segment: invocation.id.clone(),
                call: Some(call.clone()),
                ordinal: None,
            };
            evidence
                .journals
                .extend(view.journal(&work, &invocation.id, ready.protocol).await?);
        }
        evidence.retain_follow_on(WorkIdentity {
            ingress: accepted.work.ingress.clone(), run: run.clone(),
            segment: segments.last().context("no successor invocation")?.invocation.id.clone(),
            call: Some(call.clone()), ordinal: None,
        }, &pending, lease.directory.join("s34-retained-head.json").display().to_string())?;
        for fact in &evidence.journals {
            if let Some(DecodedRecord::Run(entry)) = &fact.decoded
                && let Some(trace) = &entry.record.trace {
                ensure!(trace.owner == lash::tracing::TraceToolOwner::Turn {
                    session_id: lash::SessionId::fixture("s34-session"),
                    turn_id: lash::TurnId::fixture(run.clone()),
                }, "actual journal record names another logical Run");
            }
        }
        let scopes: Vec<_> = evidence
            .journals
            .iter()
            .filter_map(|fact| match &fact.decoded {
                Some(DecodedRecord::Run(entry)) => entry
                    .record
                    .trace
                    .as_ref()
                    .and_then(|trace| {
                        trace
                            .admissions
                            .iter()
                            .find(|(id, _)| id.to_string() == call)
                    })
                    .map(|(_, scope)| scope.clone()),
                _ => None,
            })
            .collect();
        let original = scopes
            .first()
            .context("real journal lacks the tool admission scope")?
            .clone();
        ensure!(
            scopes.iter().all(|scope| scope == &original),
            "recorded tool scope changed"
        );
        ensure!(
            transfer.entries.iter().any(|entry| {
                entry.record.trace.as_ref().is_some_and(|trace| {
                    trace
                        .admissions
                        .iter()
                        .any(|(id, scope)| id.to_string() == call && scope == &original)
                })
            }),
            "handover did not retain the original tool scope"
        );
        // The journal retains admission facts before observation emission.
        // The first-writer tool receipt selects and retains the SDK anchor.
        let receipt_snapshot = read_only_tool_receipt(
            &lease.directory.join("consumer-data/durable-core.db"),
            "s34-session", &call,
        )?;
        let request: lash::persistence::ToolRequestReceipt =
            serde_json::from_value(receipt_snapshot["request"].clone())?;
        let completion: lash::persistence::ToolCompletionReceipt =
            serde_json::from_value(receipt_snapshot["completion"].clone())?;
        let retained_scope = request.scope.as_ref()
            .context("accepted tool receipt has no retained scope")?.clone();
        ensure!(retained_scope.scope == original.scope
            && retained_scope.cause == original.cause
            && retained_scope.started_at_ms == original.started_at_ms,
            "selected tool anchor changed the recorded admission facts");
        ensure!(request.owner == lash::tracing::TraceToolOwner::Turn {
            session_id: lash::SessionId::fixture("s34-session"),
            turn_id: lash::TurnId::fixture(run.clone()),
        } && completion.owner == request.owner
            && completion.request_key == request.request_key
            && completion.payload_digest == request.payload_digest
            && request.payload["call_id"] == call
            && completion.result["call_id"] == call,
            "stored logical receipts changed owner, call or payload identity");
        std::fs::write(lease.directory.join("s34-tool-receipt.json"),
            serde_json::to_vec_pretty(&receipt_snapshot)?)?;
        evidence.stores.push(receipt_snapshot);
        let attempts: BTreeSet<_> = evidence
            .journals
            .iter()
            .filter_map(|fact| match &fact.decoded {
                Some(DecodedRecord::Attempt(attempt)) if attempt.call_id.to_string() == call => {
                    Some(attempt.attempt.get())
                }
                _ => None,
            })
            .collect();
        ensure!(
            attempts == BTreeSet::from([1, 2]),
            "journal attempts differ from the executed body ledger: {attempts:?}"
        );
        let actual: Vec<_> = evidence
            .journals
            .iter()
            .filter_map(|fact| match &fact.decoded {
                Some(DecodedRecord::Attempt(attempt)) if attempt.call_id.to_string() == call => {
                    Some(attempt)
                }
                _ => None,
            })
            .collect();
        ensure!(
            actual.len() == 2
                && actual.iter().any(|attempt| attempt.attempt.get() == 1
                    && matches!(
                        attempt.result,
                        lash_core_store::tool_run::AttemptResult::Failed {
                            retryable: true,
                            ..
                        }
                    ))
                && actual.iter().any(|attempt| attempt.attempt.get() == 2
                    && matches!(
                        attempt.result,
                        lash_core_store::tool_run::AttemptResult::Done { .. }
                    )),
            "reported retry did not produce one failure and one successful body receipt"
        );
        host.control(reqwest::Method::POST, "/control/s34/release/3", None)
            .await?;
        let completed = host
            .command(HostCommand::Attach {
                run: accepted.work.ingress.clone(),
            })
            .await?;
        let outcome: lash::remote::turn_result::RemoteSendOutcome =
            serde_json::from_value(completed.output.clone())?;
        outcome.validate()?;
        ensure!(
            outcome.status() == lash::remote::turn_result::RemoteTurnStatus::Answered,
            "telemetry outage became a business failure: {outcome:?}"
        );
        ensure!(
            outcome
                .report()
                .map(|report| report.assistant_output.safe_text.as_str())
                == Some("S34 one logical answer"),
            "business answer changed across retry/transfer"
        );
        evidence.outputs.push(completed);
        let bodies = host
            .control(reqwest::Method::GET, "/control/s34/receipts", None)
            .await?;
        ensure!(
            bodies
                .as_array()
                .context("body ledger is not an array")?
                .len()
                == 3,
            "retry/transfer restarted or omitted a body"
        );
        evidence.effects.push(bodies);
        Ok::<_, anyhow::Error>((call, retained_scope, invocations, outage.dropped_spans))
    }
    .await;

    // A failed oracle still releases its fixture gates so graceful teardown
    // can quiesce actual work. Its original failure remains the verdict.
    let mut gate_cleanup_errors = Vec::new();
    if result.is_err() && execution {
        receiver.reconnect();
        for phase in 1..=3 {
            if let Err(error) = host
                .control(
                    reqwest::Method::POST,
                    &format!("/control/s34/release/{phase}"),
                    None,
                )
                .await
            {
                gate_cleanup_errors.push(error.to_string());
            }
        }
    }
    // Every lifetime is closed before propagating the first oracle failure.
    let host_cleanup = host.stop().await;
    let shutdown = host.shutdown_receipt();
    let collector = receiver.finish().await;
    let cluster_cleanup = cluster.finish().await;
    if let Ok(receipts) = &host_cleanup {
        evidence.cleanup.extend(receipts.clone());
    }
    if let Ok(receipts) = &cluster_cleanup {
        evidence.cleanup.extend(receipts.clone());
    }
    if let Ok((_, cleanup)) = &collector {
        evidence.cleanup.push(cleanup.clone());
    }
    let mut detail = json!({
        "scenario":"S34","rules":["R8","L14"],"selected":1,"executed":usize::from(execution),
        "evidence":evidence,"failure":result.as_ref().err().map(ToString::to_string),
        "shutdown":shutdown.as_ref().ok(),"collector":collector.as_ref().ok().map(|(receipt, _)| receipt),
    "host_cleanup":host_cleanup.as_ref().ok(),"cluster_cleanup":cluster_cleanup.as_ref().ok(),
    "gate_cleanup_errors":gate_cleanup_errors,
        "cleanup_errors":[host_cleanup.as_ref().err().map(ToString::to_string),
            collector.as_ref().err().map(ToString::to_string), cluster_cleanup.as_ref().err().map(ToString::to_string)]
    });
    let verified = (|| -> Result<()> {
        let (call, original, invocations, dropped) = result?;
        ensure!(
            host_cleanup?.iter().all(|receipt| receipt.closed),
            "consumer cleanup incomplete"
        );
        ensure!(
            cluster_cleanup?.iter().all(|receipt| receipt.closed),
            "cluster cleanup incomplete"
        );
        let (collector, cleanup) = collector?;
        ensure!(cleanup.closed, "collector cleanup incomplete");
        let shutdown = shutdown?;
        let export: host_telemetry::ExportReceipt =
            serde_json::from_value(shutdown["telemetry"].clone())?;
        ensure!(
            export.flush_error.is_none() && export.shutdown_error.is_none(),
            "orderly exporter shutdown failed: {export:?}"
        );
        ensure!(
            export.dropped_spans == dropped
                && export.attempted_spans == export.acknowledged_spans + dropped,
            "shutdown hid or added export loss: {export:?}"
        );
        ensure!(
            collector.spans.len() as u64 == export.acknowledged_spans,
            "acknowledged export was not drained"
        );
        let records = lash::tracing::parse_jsonl_records(&std::fs::read_to_string(&trace)?)?;
        verify_s34_projection(
            &records,
            &collector,
            &call,
            original
                .anchor
                .context()
                .context("admission has no OTel anchor")?,
            &invocations,
        )?;
        eprintln!(
            "S34 selected=1 executed=1 passed=1; {} acknowledged spans, {} explicit dropped spans, {} real segment invocations",
            export.acknowledged_spans,
            dropped,
            invocations.len()
        );
        Ok(())
    })();
    detail["failure"] = json!(verified.as_ref().err().map(ToString::to_string));
    std::fs::write(
        lease.directory.join("s34-evidence.json"),
        serde_json::to_vec_pretty(&detail)?,
    )?;
    let verdict = match &verified {
        Ok(()) => Verdict::Passed,
        Err(error) if execution => Verdict::Failed {
            reason: error.to_string(),
        },
        Err(error) => Verdict::NotRun {
            reason: error.to_string(),
        },
    };
    let counts = CaseReceipt { evidence, verdict }.write(&lease.directory)?;
    verified?;
    counts.reconcile()?;
    Ok(())
}

fn read_only_head(path: &std::path::Path, session: &str) -> Result<serde_json::Value> {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let (revision, pending): (u64, Option<String>) = connection.query_row(
        "SELECT head_revision, pending_follow_on_json FROM session_head WHERE session_id = ?1",
        [session],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    Ok(serde_json::json!({
        "database":path,
        "query":"SELECT head_revision, pending_follow_on_json FROM session_head WHERE session_id = ?1",
        "session_id":session,
        "head_revision":revision,
        "pending_follow_on":pending.map(|text| serde_json::from_str::<serde_json::Value>(&text)).transpose()?
    }))
}

fn read_only_tool_receipt(
    path: &std::path::Path,
    session: &str,
    call: &str,
) -> Result<serde_json::Value> {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let query = "SELECT request_json, completion_json FROM tool_call_receipts WHERE session_id = ?1 AND json_extract(request_json, '$.payload.call_id') = ?2";
    let mut statement = connection.prepare(query)?;
    let mut rows = statement.query([session, call])?;
    let row = rows
        .next()?
        .context("logical tool receipt absent from actual store")?;
    let request: String = row.get(0)?;
    let completion: Option<String> = row.get(1)?;
    ensure!(
        rows.next()?.is_none(),
        "logical tool admitted more than once in the store"
    );
    Ok(serde_json::json!({
        "database":path, "query":query, "session_id":session, "call_id":call,
        "request":serde_json::from_str::<serde_json::Value>(&request)?,
        "completion":serde_json::from_str::<serde_json::Value>(
            &completion.context("logical tool never completed in the store")?)?,
    }))
}

fn verify_s34_projection(
    records: &[lash::tracing::TraceRecord],
    collector: &lash_upgrade_harness::e2e::otlp::CollectorReceipt,
    call_id: &str,
    anchor: &lash::tracing::TraceCarrier,
    invocations: &std::collections::BTreeSet<String>,
) -> Result<()> {
    use lash::tracing::TraceEvent;
    let receipts: Vec<_> = records
        .iter()
        .filter_map(|record| match &record.event {
            TraceEvent::ToolReceipt {
                call_id: id,
                terminal,
                ..
            } if id.to_string() == call_id => Some((record, terminal)),
            _ => None,
        })
        .collect();
    ensure!(
        receipts.len() == 2,
        "logical tool receipt duplicated or missing: {receipts:?}"
    );
    ensure!(
        receipts
            .iter()
            .filter(|(_, terminal)| terminal.is_none())
            .count()
            == 1,
        "logical accepted receipt must occur once"
    );
    let terminal = receipts
        .iter()
        .find(|(_, terminal)| terminal.is_some())
        .ok_or_else(|| anyhow::anyhow!("logical tool never terminated"))?
        .0;
    let accepted: Vec<_> = collector
        .spans
        .iter()
        .filter(|span| {
            span.name == "lash.tool.admitted" && span.span_id == anchor.span_id().to_string()
        })
        .collect();
    ensure!(
        accepted.len() == 1,
        "original tool admission was not exported exactly once"
    );
    let terminals: Vec<_> = collector
        .spans
        .iter()
        .filter(|span| {
            span.attribute("lash.event.type") == Some("tool_receipt")
                && span.attribute("gen_ai.tool.call.id") == Some(call_id)
        })
        .collect();
    ensure!(
        terminals.len() == 1,
        "logical terminal exported more or less than once"
    );
    ensure!(
        terminals[0].attribute("lash.record.id") == Some(terminal.id.as_str()),
        "socket terminal disagrees with JSONL terminal"
    );
    ensure!(
        terminals[0].trace_id == anchor.trace_id().to_string()
            && terminals[0].parent_span_id == anchor.span_id().to_string(),
        "logical terminal lost the admission anchor across transfer"
    );

    let attempts: Vec<_> = records
        .iter()
        .filter_map(|record| match &record.event {
            TraceEvent::LlmAttemptCompleted { attempt } => Some((record, attempt)),
            _ => None,
        })
        .collect();
    ensure!(
        attempts.len() == 4,
        "expected initial request, two spending retry bodies and one final request, got {}",
        attempts.len()
    );
    ensure!(
        attempts.iter().all(|(_, attempt)| matches!(
            attempt.outcome,
            lash::tracing::TraceLlmAttemptOutcome::Completed
        )),
        "a failed model request was mistaken for completed retry work"
    );
    let model_spans: Vec<_> = collector
        .spans
        .iter()
        .filter(|span| span.attribute("gen_ai.operation.name") == Some("chat"))
        .collect();
    ensure!(
        model_spans.len() == 3,
        "outage must drop just the first inner model observation"
    );
    let mut missing = 0;
    let mut delivered_invocations = std::collections::BTreeSet::new();
    for (record, attempt) in attempts {
        let exported: Vec<_> = model_spans
            .iter()
            .filter(|span| span.attribute("lash.record.id") == Some(record.id.as_str()))
            .collect();
        ensure!(
            exported.len() <= 1,
            "one actual execution exported repeatedly"
        );
        let Some(span) = exported.first() else {
            missing += 1;
            continue;
        };
        ensure!(
            span.integer("lash.model.attempt.ordinal") == Some(u64::from(attempt.ordinal)),
            "model attempt ordinal changed during export"
        );
        if let Some(usage) = &attempt.usage {
            ensure!(
                span.integer("gen_ai.usage.input_tokens")
                    == Some(u64::try_from(usage.input_tokens)?)
                    && span.integer("gen_ai.usage.output_tokens")
                        == Some(u64::try_from(usage.output_tokens)?),
                "usage changed from the recorded provider result"
            );
        }
        let invocation = span
            .attribute("lash.attempt.invocation_id")
            .ok_or_else(|| {
                anyhow::anyhow!("live execution lacks its Restate invocation identity")
            })?;
        ensure!(
            invocations.contains(invocation),
            "attempt span names no measured invocation: {invocation}"
        );
        delivered_invocations.insert(invocation);
        // A transport carrier is optional: H0's default real server supplies
        // the invocation id with tracing disabled. The socket receipt retains
        // any supplied links; the admitted anchor proves logical parentage.
    }
    ensure!(missing == 1, "dropped attempt must remain present in JSONL");
    ensure!(
        delivered_invocations.len() >= 2,
        "no exported execution from the successor"
    );
    ensure!(
        collector
            .spans
            .iter()
            .map(|span| (&span.trace_id, &span.span_id))
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == collector.spans.len(),
        "collector received duplicate span identities"
    );
    Ok(())
}

fn provider() -> lash_core::provider::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("s34-socket-fixture")
        .complete(|_| async {
            Ok(lash_core::llm::types::LlmResponse {
                parts: vec![lash_core::llm::types::LlmOutputPart::Text {
                    text: "socket fixture answer".into(),
                    response_meta: None,
                }],
                usage: lash_core::llm::types::LlmUsage {
                    input_tokens: 7,
                    output_tokens: 3,
                    ..Default::default()
                },
                terminal_reason: lash_core::LlmTerminalReason::Stop,
                terminal_diagnostic: None,
                provider_usage: None,
                request_body: None,
                http_summary: None,
                execution_evidence: None,
                generation_disposition: None,
                response_metadata: Default::default(),
                expose_thinking: None,
            })
        })
        .build()
        .into_handle()
}

async fn send(core: &lash::LashCore, id: &str) -> Result<()> {
    let session = core.session(lash::SessionId::fixture(id));
    session
        .create(lash::SessionCreation::root(lash::SessionSpec::new(
            "socket",
            lash::TurnBudget::Unbounded,
            lash::MaxToolCalls::new(16),
        )))
        .await?;
    let session = core.session(lash::SessionId::fixture(id)).durable().await?;
    let handle = session
        .send(lash::TurnInput::text("export this answer"))
        .into_future()
        .await?;
    let outcome = tokio::time::timeout(DEADLINE, handle.outcome()).await??;
    ensure!(
        matches!(outcome.status(), lash::TurnStatus::Answered),
        "telemetry failure changed the business terminal: {:?}",
        outcome.status()
    );
    ensure!(
        outcome
            .output()
            .and_then(|output| output.assistant_message())
            == Some("socket fixture answer"),
        "telemetry failure changed the answer"
    );
    Ok(())
}

/// R8's transport gap: a real Lash core sends through the production engine
/// on the cheapest server-double tier; its SDK crosses an actual HTTP socket.
/// This is the socket/flush witness, not the live retry/transfer S34 receipt.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn r8_socket_outage_is_counted_and_shutdown_drains_acknowledged_export() -> Result<()> {
    let receiver = OtlpReceiver::bind("127.0.0.1:0".parse()?).await?;
    let telemetry = HostTelemetry::new(&receiver.endpoint())?;
    let trace = tempfile::NamedTempFile::new()?;
    let backend =
        lash_restate_test::backend(0x4938, lash_restate_test::ServerConfig::default()).await?;
    let metadata = lash::LlmProfileMetadata::builder("s34-fixture")
        .context_window_tokens(8192)
        .build()?;
    let registry = lash::LlmProfileRegistry::new().register(
        "socket",
        lash::RegisteredLlmProfile::new(metadata, provider()),
    )?;
    let core = telemetry
        .install(
            lash::LashCore::standard_builder(backend.lash_backend())
                .llm_profiles(Arc::new(registry))
                .commit_budget(lash::CommitBudget::bounded(1024 * 1024, 512))
                .queued_work_batching(lash::QueuedWorkBatchingConfig::new(1024))
                .trace_jsonl_path(trace.path()),
        )
        .build(lash::persistence::LeaseOwnerIdentity::opaque(
            "s34", "socket",
        ))?;

    send(&core, "s34-connected").await?;
    let before = telemetry.flush();
    ensure!(
        before.dropped_spans == 0 && before.acknowledged_spans > 0 && before.flush_error.is_none(),
        "initial export was not acknowledged: {before:?}"
    );
    receiver
        .wait_for(DEADLINE, |receipt| {
            receipt.spans.len() as u64 == before.acknowledged_spans
        })
        .await?;

    receiver.disconnect();
    send(&core, "s34-disconnected").await?;
    let outage = telemetry.flush();
    receiver
        .wait_for(DEADLINE, |receipt| receipt.disconnected_requests > 0)
        .await?;
    ensure!(
        outage.dropped_spans > 0,
        "outage silently lost telemetry: {outage:?}"
    );
    ensure!(
        outage.acknowledged_spans == before.acknowledged_spans,
        "disconnected socket was acknowledged"
    );

    receiver.reconnect();
    send(&core, "s34-reconnected").await?;
    core.flush_trace_sink()?;
    drop(core);
    let final_export = telemetry.shutdown();
    let (collector, cleanup) = receiver.finish().await?;
    ensure!(cleanup.closed, "collector listener leaked");
    ensure!(
        final_export.flush_error.is_none() && final_export.shutdown_error.is_none(),
        "orderly host shutdown failed: {final_export:?}"
    );
    ensure!(
        final_export.acknowledged_spans > before.acknowledged_spans,
        "reconnected export never drained"
    );
    ensure!(
        final_export.attempted_spans
            == final_export.acknowledged_spans + final_export.dropped_spans,
        "unreported export loss"
    );
    ensure!(
        collector.spans.len() as u64 == final_export.acknowledged_spans,
        "shutdown acknowledgement disagrees with socket delivery"
    );

    let records: Vec<lash::tracing::TraceRecord> =
        lash::tracing::parse_jsonl_records(&std::fs::read_to_string(trace.path())?)?;
    for session in ["s34-connected", "s34-disconnected", "s34-reconnected"] {
        let attempts: Vec<_> = records
            .iter()
            .filter(|record| {
                record
                    .context
                    .session_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == session)
                    && matches!(
                        record.event,
                        lash::tracing::TraceEvent::LlmAttemptCompleted { .. }
                    )
            })
            .collect();
        ensure!(
            attempts.len() == 1,
            "actual provider execution missing or repeated in JSONL for {session}"
        );
        let exported: Vec<_> = collector
            .spans
            .iter()
            .filter(|span| span.attribute("lash.record.id") == Some(attempts[0].id.as_str()))
            .collect();
        ensure!(
            exported.len() == usize::from(session != "s34-disconnected"),
            "socket records disagree with outage evidence for {session}"
        );
        if let Some(span) = exported.first() {
            ensure!(
                span.integer("lash.model.attempt.ordinal") == Some(1),
                "actual attempt ordinal missing"
            );
            ensure!(
                span.integer("gen_ai.usage.input_tokens") == Some(7)
                    && span.integer("gen_ai.usage.output_tokens") == Some(3),
                "recorded provider usage changed during export"
            );
        }
    }
    ensure!(
        collector
            .spans
            .iter()
            .map(|span| (&span.trace_id, &span.span_id))
            .collect::<std::collections::BTreeSet<_>>()
            .len()
            == collector.spans.len(),
        "socket delivery duplicated a span identity"
    );
    eprintln!(
        "R8 socket receipt: {} spans acknowledged, {} spans explicitly dropped, {} disconnected requests; executed=1",
        final_export.acknowledged_spans,
        final_export.dropped_spans,
        collector.disconnected_requests
    );
    Ok(())
}
