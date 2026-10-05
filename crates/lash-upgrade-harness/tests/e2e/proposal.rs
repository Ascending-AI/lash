//! S04: a proposal is not durable publication. B's plugin-command result is
//! proposed (D) and stored by Restate, but its V7 ACK is held at the proxy,
//! the ACK stream drops and the host dies. Nothing speculative may reach a
//! consumer; cold recovery must accept the one recorded resolution, publish
//! it once and redeliver the unfinished body under its first identity.

use anyhow::{Context, Result, bail, ensure};
use lash_restate_test::protocol::generated::{
    ProposeRunCompletionAckMessage, ProposeRunCompletionMessage, propose_run_completion_message,
};
use lash_restate_test::protocol::{Frame, MessageType};
use lash_upgrade_harness::e2e::case::{ArtifactIdentity, CaseLease, CaseSpec, Channel, StoreKind};
use lash_upgrade_harness::e2e::control::transport::{TransportCut, V7Proxy};
use lash_upgrade_harness::e2e::control::{
    Barrier, BarrierKind, BarrierProof, CleanupReceipt, Control, CoreControl, Fault, FaultReceipt,
    FileBarriers, WorkIdentity,
};
use lash_upgrade_harness::e2e::evidence::{
    CaseReceipt, DecodedRecord, Evidence, JournalFact, RestateEvidenceReader, Verdict,
};
use lash_upgrade_harness::e2e::host::HostKind;
use lash_upgrade_harness::e2e::provider::ProviderKind;
use lash_upgrade_harness::harness::{Case, NodeBuilds, wait_for};
use lash_upgrade_harness::node::plugin_upgrade::{Entry, PLUGIN};
use serde_json::{Value, json};

use super::plugin_upgrade::{
    barriers, bind_held_body, control, entries, frontier, note_process, quiesce, record,
    same_terminal, services, state_value, stopped, work_of,
};

fn setup() -> Result<(Case, CaseLease, CaseSpec)> {
    let root = std::path::PathBuf::from(
        std::env::var_os("LASH_PHASE_A_ARTIFACT_DIR").context("persistent scenario artifacts")?,
    )
    .join(format!("s04-{}", std::process::id()));
    std::fs::create_dir_all(&root)?;
    let lease = CaseLease::new(
        "s04",
        root.join("lease"),
        std::time::Instant::now() + std::time::Duration::from_secs(180),
    )?;
    let case = Case::leased_sqlite("s04", &services()?, &lease)?;
    let path = std::path::PathBuf::from(
        std::env::var_os(lash_upgrade_harness::harness::NODE_N_ENV)
            .context("materialized candidate binary")?,
    );
    let spec = CaseSpec {
        id: "s04".into(),
        rules: vec!["L02".into(), "L19".into()],
        host: HostKind::UpgradeNode,
        store: StoreKind::SqliteFile,
        channel: Channel::Standard,
        provider: ProviderKind::Scripted,
        restate_nodes: 1,
        artifacts: vec![ArtifactIdentity {
            role: "candidate".into(),
            sha256: lash_core::stable_hash::sha256_hex(&std::fs::read(&path)?),
            path,
            candidate_sha: std::env::var("LASH_E2E_CANDIDATE_SHA")?,
            generation: "candidate".into(),
        }],
        cuts: Vec::new(),
        expected_terminal: "Answered".into(),
        requires: Vec::new(),
    };
    spec.validate()?;
    record(&case, "case-spec.json", &serde_json::to_value(&spec)?)?;
    Ok((case, lease, spec))
}

/// One captured HTTP/2 DATA frame's decoded service-protocol message.
fn wire_frame(path: &std::path::Path) -> Result<Frame> {
    let artifact: Value = serde_json::from_slice(&std::fs::read(path)?)?;
    let payload: Vec<u8> = serde_json::from_value(artifact["payload"].clone())?;
    Ok(Frame {
        ty: MessageType::from_code(u16::try_from(
            artifact["type"].as_u64().context("wire frame type")?,
        )?)?,
        requested_ack: artifact["requested_ack"].as_bool().unwrap_or_default(),
        payload: payload.into(),
    })
}

/// The journal entry a ProposeRunCompletion carries, envelope fields stripped
/// exactly like `tools.rs`' `record_payload`.
fn proposed_entry(frame: &Frame) -> Result<(ProposeRunCompletionMessage, Value)> {
    ensure!(
        frame.ty == MessageType::ProposeRunCompletion,
        "held frame {:?} is not a proposal",
        frame.ty
    );
    let message = frame.decode::<ProposeRunCompletionMessage>()?;
    let Some(propose_run_completion_message::Result::Value(bytes)) = &message.result else {
        bail!("held proposal is not a value result");
    };
    let mut value: Value = serde_json::from_slice(bytes)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("proposed result is not an object"))?;
    ensure!(
        object
            .remove("effect_journal_version")
            .and_then(|value| value.as_u64())
            == Some(u64::from(lash_restate::EFFECT_JOURNAL_VERSION)),
        "foreign effect generation"
    );
    object.remove("build_generation");
    Ok((message, value))
}

/// The value a proposal carries, when it is a journal entry at all. Other
/// services on this deployment propose their own records; they are not the
/// proposal this case's wire oracle counts.
fn proposal_value(frame: &Frame) -> Option<Value> {
    if frame.ty != MessageType::ProposeRunCompletion {
        return None;
    }
    let message = frame.decode::<ProposeRunCompletionMessage>().ok()?;
    let Some(propose_run_completion_message::Result::Value(bytes)) = message.result else {
        return None;
    };
    serde_json::from_slice(&bytes).ok()
}

fn bodies(ledger: &[Entry], symbol: &str) -> Vec<Value> {
    ledger
        .iter()
        .filter(|entry| entry.phase == "body" && entry.detail["symbol"] == json!(symbol))
        .map(|entry| entry.detail.clone())
        .collect()
}

fn ledger(ledger: &[Entry], phase: &str, detail: &Value) -> usize {
    ledger
        .iter()
        .filter(|entry| entry.phase == phase && &entry.detail == detail)
        .count()
}

#[allow(clippy::too_many_arguments)]
fn finish(
    case: &Case,
    lease: &CaseLease,
    spec: &CaseSpec,
    journals: Vec<JournalFact>,
    barriers_proofs: Vec<BarrierProof>,
    faults: Vec<FaultReceipt>,
    stores: Vec<Value>,
    effects: Vec<Value>,
) -> Result<()> {
    record(case, "case-spec.json", &serde_json::to_value(spec)?)?;
    record(
        case,
        "ownership.json",
        &json!({"gate":lease.gate_id,"namespace":lease.namespace,"authority":lease.authority,"ports":lease.ports,"processes":lease.processes,"cleanup":lease.cleanup}),
    )?;
    let mut evidence = Evidence::empty(spec.id.clone());
    evidence.artifacts = spec.artifacts.clone();
    evidence.journals = journals;
    evidence.barriers = barriers_proofs;
    evidence.faults = faults;
    evidence.stores = stores;
    evidence.effects = effects;
    evidence.cleanup = lease.cleanup.clone();
    CaseReceipt {
        evidence,
        verdict: Verdict::Passed,
    }
    .write(&case.gate_dir())?
    .reconcile()
}

#[test]
#[ignore = "needs the candidate binary and private live Restate"]
fn s04_dropped_ack_leaves_the_proposal_unpublished_until_cold_recovery() -> Result<()> {
    let builds = NodeBuilds::from_env()?;
    let (case, mut lease, mut spec) = setup()?;
    let session = case.session_id("plugin");
    let deadline = lease.deadline;
    // The proxy spawns its accept/relay tasks where it starts; keep one
    // multi-thread runtime alive for every controller/proxy step.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    let proxy_dir = case.gate_dir().join("proxy");
    let node_bind = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.local_addr()?
    };
    let proxy = rt.block_on(V7Proxy::start(
        std::net::TcpListener::bind("127.0.0.1:0")?,
        node_bind,
        proxy_dir.clone(),
        deadline,
        vec![],
    ))?;
    let proxy_endpoint = proxy.endpoint.clone();
    lease.ports.push(
        proxy_endpoint
            .trim_start_matches("http://")
            .rsplit_once(':')
            .context("proxy endpoint port")?
            .1
            .parse()?,
    );
    let node = builds.n.serve_plugin_upgrade(
        &case,
        Some(&node_bind.to_string()),
        Some(&proxy_endpoint),
    )?;
    note_process(&mut lease, &node, "candidate", 1)?;
    spec.artifacts[0].generation = node.generation()?.into();
    let mut reader = RestateEvidenceReader::new("H5".into(), case.view()?, 7);

    let accepted = control(
        &builds.n,
        &case,
        "send",
        &session,
        &["--variant", "proposal"],
    )?;
    let input = accepted["input_id"]
        .as_str()
        .context("durable input identity")?
        .to_owned();
    let entered = |symbol: &str| -> Result<Value> {
        wait_for(&format!("{symbol}'s entered body"), || match std::fs::read(
            case.gate_dir().join(format!("{symbol}.entered")),
        ) {
            Ok(bytes) => Ok(Some(serde_json::from_slice::<Value>(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        })
    };
    let a_body = entered("A")?;
    let b_body = entered("B")?;
    let a_call = a_body["call_id"].as_str().context("A's call")?.to_owned();
    let b_call = b_body["call_id"].as_str().context("B's call")?.to_owned();
    let a_barrier = bind_held_body(&case, &accepted, &a_body)?;
    let b_barrier = bind_held_body(&case, &accepted, &b_body)?;
    let (b_work, b_invocation) = work_of(&case, &accepted, &b_body)?;
    reader.bind(&b_work, b_invocation.clone())?;
    let mut controller = CoreControl::new(
        FileBarriers::new(proxy_dir.clone(), deadline)?,
        Box::new(reader),
    );
    controller.own_process("candidate".into(), 1, node)?;
    controller.own_proxy("v7".into(), 1, proxy)?;
    controller.proxy("v7")?.bind_invocation(
        b_invocation,
        WorkIdentity {
            call: None,
            ordinal: None,
            ..b_work.clone()
        },
    )?;
    let proposed = Barrier {
        work: b_work.clone(),
        kind: BarrierKind::DProposed,
    };
    let before_ack = Barrier {
        work: b_work.clone(),
        kind: BarrierKind::BeforeAck,
    };
    // Hold the ACK and arm the cut before B's body can produce its proposal.
    controller.barriers.hold(&before_ack)?;
    controller.proxy("v7")?.arm_cut(TransportCut {
        proposal: proposed.clone(),
        before_ack: before_ack.clone(),
    })?;
    spec.cuts.extend([
        a_barrier.clone(),
        b_barrier.clone(),
        proposed.clone(),
        before_ack.clone(),
    ]);

    barriers(&case)?.release(&b_barrier)?;
    let proposed_proof = rt.block_on(controller.await_barrier(&proposed))?;
    let ack_proof = rt.block_on(controller.await_barrier(&before_ack))?;
    let durable_proof = rt.block_on(controller.await_barrier(&Barrier {
        work: b_work.clone(),
        kind: BarrierKind::DDurable,
    }))?;
    spec.cuts.push(durable_proof.barrier.clone());
    record(
        &case,
        "s04-cut.json",
        &serde_json::to_value(json!({
            "proposed": proposed_proof,
            "before_ack": ack_proof,
            "durable": durable_proof,
        }))?,
    )?;

    // The held proposal is a Run record deciding B Final and carrying B's
    // applied state resolution; the held ACK answers exactly its completion.
    let (proposal, proposed_value) =
        proposed_entry(&wire_frame(std::path::Path::new(&proposed_proof.artifact))?)?;
    let held_entry: lash_core::tool_run::RunJournalEntry = serde_json::from_value(proposed_value)?;
    ensure!(
        held_entry.record.events.iter().any(|event| matches!(
            event,
            lash_core::tool_run::RunEvent::Decided {
                call_id,
                decision: lash_core::tool_run::CallDecision::Final { .. },
                ..
            } if call_id.as_str() == b_call
        )),
        "the held proposal does not decide B Final"
    );
    ensure!(
        !held_entry.state.is_empty()
            && held_entry.state.iter().any(|resolution| matches!(
                resolution.outcome,
                lash_core::tool_run::StateResolutionOutcome::Applied { .. }
            )),
        "the held proposal carries no applied resolution"
    );
    let ack = wire_frame(std::path::Path::new(&ack_proof.artifact))?
        .decode::<ProposeRunCompletionAckMessage>()?;
    ensure!(
        ack.completion_id == proposal.result_completion_id,
        "the held ACK does not answer the held proposal"
    );
    let durable_fact: JournalFact =
        serde_json::from_slice(&std::fs::read(&durable_proof.artifact)?)?;
    let Some(DecodedRecord::Run(stored)) = &durable_fact.decoded else {
        bail!("the durable fact is not a Run record");
    };
    ensure!(
        stored.state == held_entry.state,
        "Restate stored a resolution other than the held proposal's"
    );

    // Nothing reached a consumer, and the input is still running.
    let cut_read = control(&builds.n, &case, "read", &session, &[])?;
    record(&case, "s04-cut-read.json", &cut_read)?;
    ensure!(
        state_value(&cut_read, PLUGIN, "value").is_none(),
        "speculative state became visible: {cut_read}"
    );
    if !cut_read["state"][PLUGIN]["publication"].is_null() {
        let publication = frontier(&cut_read, PLUGIN)?;
        ensure!(
            publication.applied.is_none(),
            "the publication frontier advanced before the ACK: {publication:?}"
        );
    }
    let ledger_cut = entries(&case)?;
    ensure!(
        ledger_cut
            .iter()
            .filter(|entry| entry.phase == "provider")
            .count()
            == 1
            && ledger_cut
                .iter()
                .filter(|entry| entry.phase == "hook")
                .count()
                == 0
            && ledger(&ledger_cut, "reducer", &json!("B")) == 1
            && ledger(&ledger_cut, "reducer", &json!("A")) == 0
            && bodies(&ledger_cut, "A").len() == 1
            && bodies(&ledger_cut, "B").len() == 1,
        "ledger at the cut: {ledger_cut:?}"
    );
    let running = lash_upgrade_harness::harness::block_on(
        case.view()?.invocations_like("LashTurn", &input, "run"),
    )?;
    ensure!(
        running.len() == 1 && running[0].1.status != "completed",
        "the input settled before its ACK: {running:?}"
    );

    // Kill the host, then drop the stream still holding B's ACK.
    let kill = rt.block_on(controller.inject(
        Fault::KillHost {
            target: "candidate".into(),
        },
        &ack_proof,
    ))?;
    record(&case, "s04-kill.json", &serde_json::to_value(&kill)?)?;
    stopped(&mut lease, "killed-candidate");
    let drop_receipt = rt.block_on(controller.inject(
        Fault::DropConnection {
            target: "v7".into(),
        },
        &ack_proof,
    ))?;
    record(
        &case,
        "s04-drop.json",
        &serde_json::to_value(&drop_receipt)?,
    )?;
    let disconnected: Value = serde_json::from_slice(&std::fs::read(proxy_dir.join(format!(
        "disconnect-{}.json",
        lash_core::stable_hash::sha256_hex(b"v7")
    )))?)?;
    ensure!(
        disconnected["closed_streams"].as_u64().unwrap_or_default() >= 1,
        "the held ACK stream was not among the closed streams"
    );
    // Free the held ACK so nothing later can block on it; a re-proposal is
    // caught by the wire counts below instead.
    controller.barriers.release(&before_ack)?;

    // Cold recovery on the same bind and advertised URI.
    let reopened = builds.n.serve_plugin_upgrade(
        &case,
        Some(&node_bind.to_string()),
        Some(&proxy_endpoint),
    )?;
    note_process(&mut lease, &reopened, "candidate", 2)?;
    controller.own_process("candidate".into(), 2, reopened)?;
    wait_for("A's body to be redelivered once", || {
        let bodies = bodies(&entries(&case)?, "A");
        ensure!(bodies.len() <= 2, "A's body ran a third time: {bodies:?}");
        if bodies.len() != 2 {
            return Ok(None);
        }
        ensure!(
            bodies[0]["call_id"] == bodies[1]["call_id"]
                && bodies[0]["run"] == bodies[1]["run"]
                && bodies[0]["attempt"] == json!(1)
                && bodies[1]["attempt"] == json!(1),
            "redelivery changed A's identity: {bodies:?}"
        );
        Ok(Some(()))
    })?;
    barriers(&case)?.release(&a_barrier)?;
    let terminal = control(&builds.n, &case, "follow", &session, &["--input", &input])?;
    record(&case, "s04-terminal.json", &terminal)?;
    let outcome: lash::SendOutcome = serde_json::from_value(terminal.clone())?;
    ensure!(
        outcome.status() == lash::TurnStatus::Answered
            && outcome
                .output()
                .and_then(|output| output.assistant_message())
                == Some("recorded plugin state"),
        "cold recovery did not answer from the recorded state: {terminal}"
    );
    quiesce(&case)?;

    let final_read = control(&builds.n, &case, "read", &session, &[])?;
    ensure!(
        state_value(&final_read, PLUGIN, "value").as_deref() == Some("BA")
            && state_value(&final_read, PLUGIN, "hooks").as_deref() == Some("H"),
        "cold recovery did not publish the recorded resolution once: {final_read}"
    );
    let publication = frontier(&final_read, PLUGIN)?;
    ensure!(
        publication.applied.is_some() && publication.receipts.len() == 3,
        "expected B, A and the after-turn hook in the frontier: {publication:?}"
    );
    let ledger_final = entries(&case)?;
    ensure!(
        bodies(&ledger_final, "B").len() == 1
            && ledger(&ledger_final, "reducer", &json!("B")) == 1
            && bodies(&ledger_final, "A").len() == 2
            && ledger(&ledger_final, "reducer", &json!("A")) == 1
            && ledger_final
                .iter()
                .filter(|entry| entry.phase == "provider")
                .count()
                == 2
            && ledger_final
                .iter()
                .filter(|entry| entry.phase == "hook")
                .count()
                == 1
            && ledger_final.iter().all(|entry| entry.converter_calls == 0),
        "cold recovery reentered completed code: {ledger_final:?}"
    );

    // The final journal holds B's decision exactly once, carrying the held
    // proposal's state, and A's resolution reduced against B's publication.
    let journal = rt.block_on(controller.reader.collect(&b_work))?;
    let mut decided_b = 0usize;
    let mut a_resolution = None;
    let b_resolution = held_entry
        .state
        .iter()
        .find(|resolution| {
            matches!(
                &resolution.origin,
                lash_core::tool_run::StateCommandOrigin::ToolAttempt { call_id, .. }
                    if call_id.as_str() == b_call
            )
        })
        .context("the held proposal carries no resolution of B's attempt")?;
    for fact in &journal.journals {
        let Some(DecodedRecord::Run(entry)) = &fact.decoded else {
            continue;
        };
        for event in &entry.record.events {
            if matches!(event, lash_core::tool_run::RunEvent::Decided { call_id, .. } if call_id.as_str() == b_call)
            {
                decided_b += 1;
                ensure!(
                    entry.state == held_entry.state,
                    "B's durable decision lost the held proposal's state"
                );
            }
        }
        for resolution in &entry.state {
            if matches!(
                &resolution.origin,
                lash_core::tool_run::StateCommandOrigin::ToolAttempt { call_id, attempt }
                    if call_id.as_str() == a_call && attempt.get() == 1
            ) {
                ensure!(
                    a_resolution.replace(resolution).is_none(),
                    "A's attempt resolved twice"
                );
            }
        }
    }
    ensure!(decided_b == 1, "B was decided {decided_b} times");
    let a_resolution = a_resolution.context("A's attempt was never resolved")?;
    ensure!(
        a_resolution.predecessor == Some(b_resolution.ordinal),
        "A's resolution did not reduce against B's recorded publication"
    );

    // Across every connection, each completion was proposed exactly once:
    // B's stored D was never re-proposed after its dropped ACK.
    let mut proposals = [0usize; 4];
    for file in std::fs::read_dir(&proxy_dir)? {
        let path = file?.path();
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let parts: Vec<&str> = name.split('-').collect();
        if !(name.starts_with("wire-") && name.ends_with(".json") && parts.get(3) == Some(&"out")) {
            continue;
        }
        let Some(value) = proposal_value(&wire_frame(&path)?) else {
            continue;
        };
        if let Some(events) = value.pointer("/record/events").and_then(Value::as_array) {
            for event in events {
                if event.get("event").and_then(Value::as_str) == Some("decided") {
                    match event.get("call_id").and_then(Value::as_str) {
                        Some(call) if call == a_call => proposals[0] += 1,
                        Some(call) if call == b_call => proposals[1] += 1,
                        _ => {}
                    }
                }
            }
        } else {
            match (
                value.get("call_id").and_then(Value::as_str),
                value.get("attempt").and_then(Value::as_u64),
            ) {
                (Some(call), Some(_)) if call == a_call => proposals[2] += 1,
                (Some(call), Some(_)) if call == b_call => proposals[3] += 1,
                _ => {}
            }
        }
    }
    ensure!(
        proposals == [1, 1, 1, 1],
        "wire proposals D(A)={} D(B)={} X(A)={} X(B)={}: a completion was re-proposed",
        proposals[0],
        proposals[1],
        proposals[2],
        proposals[3]
    );
    record(
        &case,
        "s04-wire.json",
        &json!({"d_a":proposals[0],"d_b":proposals[1],"x_a":proposals[2],"x_b":proposals[3]}),
    )?;

    // Reattachment returns the same terminal and reenters nothing.
    let before = entries(&case)?;
    for _ in 0..2 {
        let replayed = control(&builds.n, &case, "follow", &session, &["--input", &input])?;
        same_terminal(&terminal, &replayed)?;
    }
    let after = entries(&case)?;
    ensure!(
        serde_json::to_value(&after)? == serde_json::to_value(&before)?,
        "a repeated follow reentered code"
    );
    record(
        &case,
        "s04-final.json",
        &json!({"state":final_read,"terminal":terminal,"ledger":after}),
    )?;

    let node2 = controller.take_process("candidate")?;
    node2.stop()?;
    stopped(&mut lease, "candidate");
    let mut proxy = controller.take_proxy("v7")?;
    rt.block_on(proxy.finish())?;
    lease.cleanup.push(CleanupReceipt {
        resource: "v7".into(),
        closed: true,
        detail: "transport proxy finished; listener refused connections".into(),
    });
    let mut effects: Vec<Value> = entries(&case)?
        .into_iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<_, _>>()?;
    effects.push(json!({
        "kind":"s04_wire_proposals",
        "d_a":proposals[0],"d_b":proposals[1],"x_a":proposals[2],"x_b":proposals[3],
    }));
    finish(
        &case,
        &lease,
        &spec,
        journal.journals,
        vec![proposed_proof, ack_proof, durable_proof],
        controller.receipts.clone(),
        vec![cut_read, final_read],
        effects,
    )?;
    Ok(())
}
