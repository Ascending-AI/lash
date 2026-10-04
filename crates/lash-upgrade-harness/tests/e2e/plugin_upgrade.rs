//! S24/S25: L12/L19 across separately materialized candidate/synthetic-next hosts.
//! S33 deliberately selects the existing Phase A operator/wire/drain legs.

use anyhow::{Context, Result, ensure};
use lash_upgrade_harness::e2e::{
    case::{ArtifactIdentity, CaseLease, CaseSpec, Channel, StoreKind},
    control::{
        Barrier, BarrierKind, BarrierProof, Control, CoreControl, Fault, FileBarriers, WorkIdentity,
    },
    evidence::{CaseReceipt, DecodedRecord, Evidence, JournalFact, RestateEvidenceReader, Verdict},
};
use lash_upgrade_harness::harness::{Case, NodeBinary, NodeBuilds, Services, block_on, wait_for};
use lash_upgrade_harness::identity::BuildLabel;
use lash_upgrade_harness::node::plugin_upgrade::{Entry, OTHER, PLUGIN};
use serde_json::{Value, json};

fn services() -> Result<Services> {
    Ok(Services {
        ingress_url: std::env::var("RESTATE_INGRESS_URL")
            .context("private live Restate ingress")?,
        admin_url: std::env::var("RESTATE_ADMIN_URL").context("private live Restate admin")?,
        postgres_url: String::new(),
    })
}

fn setup(id: &str, terminal: &str, builds: &NodeBuilds) -> Result<(Case, CaseLease, CaseSpec)> {
    let root = std::path::PathBuf::from(
        std::env::var_os("LASH_PHASE_A_ARTIFACT_DIR").context("persistent scenario artifacts")?,
    )
    .join(format!("{id}-{}", std::process::id()));
    std::fs::create_dir_all(&root)?;
    let lease = CaseLease::new(
        id,
        root.join("lease"),
        std::time::Instant::now() + std::time::Duration::from_secs(120),
    )?;
    let case = Case::leased_sqlite(id, &services()?, &lease)?;
    // Expand the successor's additive store shape before either serving host
    // holds it open. SQLite migration requires exclusive ownership; admission
    // and plugin publication remain at the predecessor's rollback floor.
    record(
        &case,
        "store-expand.json",
        &serde_json::to_value(builds.next.probe(&case, None)?)?,
    )?;
    let mut artifacts = Vec::new();
    for (role, variable) in [
        ("candidate", lash_upgrade_harness::harness::NODE_N_ENV),
        (
            "synthetic-next",
            lash_upgrade_harness::harness::NODE_NEXT_ENV,
        ),
    ] {
        let path =
            std::path::PathBuf::from(std::env::var_os(variable).context("materialized binary")?);
        artifacts.push(ArtifactIdentity {
            role: role.into(),
            sha256: lash_core::stable_hash::sha256_hex(&std::fs::read(&path)?),
            path,
            candidate_sha: std::env::var("LASH_E2E_CANDIDATE_SHA")?,
            generation: role.into(),
        });
    }
    ensure!(
        artifacts[0].sha256 != artifacts[1].sha256,
        "candidate and successor must be separately materialized builds"
    );
    let spec = CaseSpec {
        id: id.into(),
        rules: vec!["L12".into(), "L19".into(), "L21".into()],
        host: lash_upgrade_harness::e2e::host::HostKind::UpgradeNode,
        store: StoreKind::SqliteFile,
        channel: Channel::Standard,
        provider: lash_upgrade_harness::e2e::provider::ProviderKind::Scripted,
        restate_nodes: 1,
        artifacts,
        cuts: Vec::new(),
        expected_terminal: terminal.into(),
        requires: Vec::new(),
    };
    spec.validate()?;
    record(&case, "case-spec.json", &serde_json::to_value(&spec)?)?;
    Ok((case, lease, spec))
}

fn note_process(
    lease: &mut CaseLease,
    node: &lash_upgrade_harness::harness::ServingNode,
    role: &str,
    incarnation: u32,
) -> Result<()> {
    let pid = node.pid()?;
    lease
        .ports
        .push(node.bind()?.parse::<std::net::SocketAddr>()?.port());
    lease
        .processes
        .push(lash_upgrade_harness::e2e::control::ProcessReceipt {
            role: role.into(),
            pid,
            incarnation,
            log: std::fs::read_link(format!("/proc/{pid}/fd/1"))?
                .display()
                .to_string(),
        });
    Ok(())
}

fn stopped(lease: &mut CaseLease, role: &str) {
    lease
        .cleanup
        .push(lash_upgrade_harness::e2e::control::CleanupReceipt {
            resource: role.into(),
            closed: true,
            detail: "owned host killed and reaped".into(),
        });
}

fn finish(case: &Case, lease: &CaseLease, spec: &CaseSpec) -> Result<()> {
    record(case, "case-spec.json", &serde_json::to_value(spec)?)?;
    record(
        case,
        "ownership.json",
        &json!({"gate":lease.gate_id,"namespace":lease.namespace,"authority":lease.authority,"ports":lease.ports,"processes":lease.processes,"cleanup":lease.cleanup}),
    )?;
    let mut evidence = Evidence::empty(spec.id.clone());
    evidence.artifacts = spec.artifacts.clone();
    evidence.cleanup = lease.cleanup.clone();
    let (cut, checkpoint) = if spec.id == "s24" {
        ("s24-b-durable.json", "s24-frontiers.json")
    } else {
        ("s25-cut.json", "s25-final.json")
    };
    let proof: BarrierProof = serde_json::from_slice(&std::fs::read(case.gate_dir().join(cut))?)?;
    evidence
        .journals
        .push(serde_json::from_slice(&std::fs::read(&proof.artifact)?)?);
    evidence.barriers.push(proof);
    evidence.stores.push(serde_json::from_slice(&std::fs::read(
        case.gate_dir().join(checkpoint),
    )?)?);
    evidence.effects = entries(case)?
        .into_iter()
        .map(serde_json::to_value)
        .collect::<std::result::Result<_, _>>()?;
    if spec.id != "s24" {
        evidence.faults.push(serde_json::from_slice(&std::fs::read(
            case.gate_dir().join("s25-kill.json"),
        )?)?);
    }
    CaseReceipt {
        evidence,
        verdict: Verdict::Passed,
    }
    .write(&case.gate_dir())?
    .reconcile()
}

fn entries(case: &Case) -> Result<Vec<Entry>> {
    let path = case.gate_dir().join("entries.jsonl");
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .map(|line| serde_json::from_str(line).map_err(Into::into))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}

fn control(
    node: &NodeBinary,
    case: &Case,
    action: &str,
    session: &str,
    extra: &[&str],
) -> Result<Value> {
    let mut args = vec!["--action", action, "--session", session];
    args.extend(extra);
    let output = node.plugin_upgrade(case, &args)?;
    if action == "follow" {
        for file in std::fs::read_dir(case.gate_dir())? {
            let path = file?.path();
            if path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with(".follow.json"))
            {
                let report: Value = serde_json::from_slice(&std::fs::read(path)?)?;
                ensure!(
                    report["converters"] == json!([0, 0]),
                    "durable follow reentered a converter: {report}"
                );
            }
        }
    }
    Ok(output)
}

fn quiesce(case: &Case) -> Result<()> {
    let view = case.view()?;
    wait_for("the plugin Run and its close to settle", || {
        Ok(block_on(view.open_invocations())?.is_empty().then_some(()))
    })
}

fn counts(case: &Case, build: BuildLabel, phase: &str) -> Result<usize> {
    Ok(entries(case)?
        .iter()
        .filter(|entry| entry.build == build && entry.phase == phase)
        .count())
}

fn state_value(state: &Value, plugin: &str, key: &str) -> Option<String> {
    state["state"][plugin]["values"][key]
        .as_str()
        .map(str::to_owned)
}

fn frontier(state: &Value, plugin: &str) -> Result<lash_core::tool_run::StateFrontier> {
    serde_json::from_value(state["state"][plugin]["publication"].clone()).map_err(Into::into)
}

fn record(case: &Case, name: &str, value: &Value) -> Result<()> {
    std::fs::write(
        case.gate_dir().join(name),
        serde_json::to_vec_pretty(value)?,
    )?;
    Ok(())
}

fn barriers(case: &Case) -> Result<FileBarriers> {
    FileBarriers::new(
        case.gate_dir().to_owned(),
        std::time::Instant::now() + std::time::Duration::from_secs(120),
    )
}

fn work_of(case: &Case, accepted: &Value, body: &Value) -> Result<(WorkIdentity, String)> {
    let input = accepted["input_id"].as_str().context("accepted ingress")?;
    let view = case.view()?;
    let (segment, invocation) = wait_for("the body's actual Run invocation", || {
        let invocations = block_on(view.invocations_like("LashTurn", input, "run"))?;
        ensure!(invocations.len() <= 1, "ambiguous physical Run segment");
        Ok(invocations.into_iter().next())
    })?;
    record(
        case,
        "admitted-invocation.json",
        &json!({"ingress":input,"segment":segment,"invocation":invocation.id,"deployment":invocation.pinned_deployment_id}),
    )?;
    Ok((
        WorkIdentity {
            ingress: input.into(),
            run: body["run"].as_str().context("body's logical Run")?.into(),
            segment,
            call: Some(body["call_id"].as_str().context("body's call")?.into()),
            ordinal: Some(u32::try_from(
                body["attempt"].as_u64().context("body's attempt")?,
            )?),
        },
        invocation.id,
    ))
}

fn bind_held_body(case: &Case, accepted: &Value, body: &Value) -> Result<Barrier> {
    let (work, _) = work_of(case, accepted, body)?;
    let barrier = Barrier {
        work,
        kind: BarrierKind::BodyEntered,
    };
    let controls = barriers(case)?;
    controls.hold(&barrier)?;
    record(
        case,
        &format!(
            "{}.binding",
            barrier.work.call.as_deref().context("held call")?
        ),
        &serde_json::to_value(&barrier.work)?,
    )?;
    let proof = block_on(controls.await_proof(&barrier))?;
    record(case, "body-barrier.json", &serde_json::to_value(proof)?)?;
    Ok(barrier)
}

fn b_controller(case: &Case, accepted: &Value) -> Result<(CoreControl, BarrierProof)> {
    let body = wait_for("B's actual body identity", || {
        Ok(entries(case)?
            .into_iter()
            .find(|entry| entry.phase == "body" && entry.detail["symbol"] == "B")
            .map(|entry| entry.detail))
    })?;
    let (work, invocation) = work_of(case, accepted, &body)?;
    let mut reader = RestateEvidenceReader::new("H5".into(), case.view()?, 7);
    reader.bind(&work, invocation)?;
    let mut control = CoreControl::new(barriers(case)?, Box::new(reader));
    let proof = block_on(control.await_barrier(&Barrier {
        work,
        kind: BarrierKind::DDurable,
    }))?;
    let fact: JournalFact = serde_json::from_slice(&std::fs::read(&proof.artifact)?)?;
    let Some(DecodedRecord::Run(entry)) = &fact.decoded else {
        anyhow::bail!("B cut has no decoded Run record");
    };
    ensure!(entry.record.events.iter().any(|event| matches!(event, lash_core::tool_run::RunEvent::Decided { call_id, decision: lash_core::tool_run::CallDecision::Final { .. }, .. } if Some(call_id.as_str()) == proof.barrier.work.call.as_deref())), "B decision is not Final");
    let observed_segment: u32 = proof
        .barrier
        .work
        .segment
        .rsplit_once('#')
        .context("actual segment key")?
        .1
        .parse()?;
    ensure!(
        entry.record.segment.0 == observed_segment,
        "journal belongs to another physical segment"
    );
    Ok((control, proof))
}

fn same_terminal(first: &Value, replayed: &Value) -> Result<()> {
    let first: lash::SendOutcome = serde_json::from_value(first.clone())?;
    let replayed: lash::SendOutcome = serde_json::from_value(replayed.clone())?;
    ensure!(first.status() == replayed.status() && first.run() == replayed.run());
    // Live activity gaps belong to each follower. The durable result is the
    // authoritative terminal, including its acceptance and checkpoint refs.
    ensure!(
        serde_json::to_value(&first.output().context("settled output")?.result)?
            == serde_json::to_value(&replayed.output().context("replayed output")?.result)?,
        "reattachment changed the durable terminal"
    );
    Ok(())
}

fn one_turn(node: &NodeBinary, case: &Case, session: &str) -> Result<Value> {
    let accepted = control(node, case, "send", session, &["--variant", "single"])?;
    let input = accepted["input_id"]
        .as_str()
        .context("durable input identity")?;
    let terminal = control(node, case, "follow", session, &["--input", input])?;
    record(case, &format!("terminal-{}.json", node.label()), &terminal)?;
    ensure!(
        serde_json::from_value::<lash::SendOutcome>(terminal.clone())?.status()
            == lash::TurnStatus::Answered,
        "plugin turn did not answer: {terminal}"
    );
    quiesce(case)?;
    let first = control(node, case, "read", session, &[])?;
    let before = entries(case)?;
    // Two cold processes read the checkpoint and reattach the completed input.
    // Neither read is a fresh submission, so completed code must not run again.
    for _ in 0..2 {
        let reopened = control(node, case, "read", session, &[])?;
        ensure!(
            reopened == first,
            "cold reopen changed the checkpoint/frontier"
        );
        let replayed = control(node, case, "follow", session, &["--input", input])?;
        same_terminal(&terminal, &replayed)?;
    }
    let after = entries(case)?;
    ensure!(
        serde_json::to_value(after)? == serde_json::to_value(before)?,
        "cold follow reentered completed code"
    );
    let publication = frontier(&first, PLUGIN)?;
    ensure!(
        publication.applied.is_some() && !publication.receipts.is_empty(),
        "no durable state frontier"
    );
    Ok(first)
}

#[test]
#[ignore = "needs exact candidate/synthetic-next binaries and private live Restate"]
fn s24_plugin_revision_rolls_back_without_reentering_completed_work() -> Result<()> {
    let builds = NodeBuilds::from_env()?;
    let (case, mut lease, mut spec) = setup("s24", "Answered", &builds)?;
    let session = case.session_id("plugin");
    let n = builds.n.serve_plugin_upgrade(&case, None)?;
    note_process(&mut lease, &n, "candidate", 1)?;
    spec.artifacts[0].generation = n.generation()?.into();
    let n_generation = n.generation()?.to_owned();
    let first = one_turn(&builds.n, &case, &session)?;
    ensure!(
        state_value(&first, PLUGIN, "value").as_deref() == Some("N")
            && state_value(&first, PLUGIN, "hooks").as_deref() == Some("H"),
        "candidate state: {first}"
    );
    ensure!(
        counts(&case, BuildLabel::N, "body")? == 1
            && counts(&case, BuildLabel::N, "reducer")? == 2
            && counts(&case, BuildLabel::N, "hook")? == 1
    );

    // A's body has entered under N's recorded binding before redeployment.
    let pending = control(&builds.n, &case, "send", &session, &["--variant", "same"])?;
    let a_body = wait_for(
        "the admitted predecessor tool body",
        || match std::fs::read(case.gate_dir().join("A.entered")) {
            Ok(bytes) => Ok(Some(serde_json::from_slice::<Value>(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        },
    )?;
    let a_barrier = bind_held_body(&case, &pending, &a_body)?;
    let (_, b_durable) = b_controller(&case, &pending)?;
    spec.cuts
        .extend([a_barrier.clone(), b_durable.barrier.clone()]);
    record(
        &case,
        "s24-b-durable.json",
        &serde_json::to_value(b_durable)?,
    )?;
    // Keep the predecessor deployment while the successor becomes newest.
    let next = builds.next.serve_plugin_upgrade(&case, None)?;
    note_process(&mut lease, &next, "successor", 1)?;
    spec.artifacts[1].generation = next.generation()?.into();
    barriers(&case)?.release(&a_barrier)?;
    let input = pending["input_id"].as_str().context("pending input")?;
    let pending_terminal = control(&builds.next, &case, "follow", &session, &["--input", input])?;
    ensure!(
        serde_json::from_value::<lash::SendOutcome>(pending_terminal.clone())?.status()
            == lash::TurnStatus::Answered,
        "old callback did not drain on N: {pending_terminal}"
    );
    quiesce(&case)?;
    let drained = control(&builds.next, &case, "read", &session, &[])?;
    ensure!(
        state_value(&drained, PLUGIN, "value").as_deref() == Some("NBA")
            && state_value(&drained, PLUGIN, "hooks").as_deref() == Some("HH"),
        "pending callbacks substituted the successor reducer: {drained}"
    );
    ensure!(
        counts(&case, BuildLabel::Next, "body")? == 0
            && counts(&case, BuildLabel::Next, "reducer")? == 0
            && counts(&case, BuildLabel::Next, "hook")? == 0,
        "new revision executed predecessor work"
    );
    // Next prepends S; rollback appends N. These operations do not commute.
    ensure!(
        next.generation()? != n_generation,
        "revision changed without changing the executable lane"
    );
    let rolled = one_turn(&builds.next, &case, &session)?;
    ensure!(
        state_value(&rolled, PLUGIN, "value").as_deref() == Some("SNBA")
            && state_value(&rolled, PLUGIN, "hooks").as_deref() == Some("JHH"),
        "successor reducer/order: {rolled}"
    );
    ensure!(
        first["config"] == rolled["config"],
        "redeploy changed recorded configuration"
    );
    ensure!(
        counts(&case, BuildLabel::Next, "body")? == 1
            && counts(&case, BuildLabel::Next, "reducer")? == 2
            && counts(&case, BuildLabel::Next, "hook")? == 1
    );
    ensure!(
        entries(&case)?
            .iter()
            .filter(|entry| entry.build == BuildLabel::Next && entry.plugin == PLUGIN)
            .all(|entry| entry.converter_calls == 2),
        "successor must convert state and config once before completed work"
    );
    ensure!(
        entries(&case)?
            .iter()
            .filter(|entry| entry.build == BuildLabel::N)
            .all(|entry| entry.converter_calls == 0),
        "candidate unexpectedly converted its native format"
    );
    ensure!(
        counts(&case, BuildLabel::N, "body")? == 3 && counts(&case, BuildLabel::N, "hook")? == 2,
        "redeploy reentered predecessor callbacks"
    );

    let rollback = builds.n.serve_plugin_upgrade(&case, None)?;
    note_process(&mut lease, &rollback, "candidate", 2)?;
    ensure!(
        rollback.generation()? == n_generation
            && rollback.uri()? != n.uri()?
            && rollback.uri()? != next.uri()?,
        "rollback lost the predecessor lane or reused a URI"
    );
    let onward = one_turn(&builds.n, &case, &session)?;
    ensure!(
        state_value(&onward, PLUGIN, "value").as_deref() == Some("SNBAN")
            && state_value(&onward, PLUGIN, "hooks").as_deref() == Some("JHHH"),
        "rollback reducer/order: {onward}"
    );
    ensure!(
        counts(&case, BuildLabel::N, "body")? == 4
            && counts(&case, BuildLabel::N, "reducer")? == 7
            && counts(&case, BuildLabel::N, "hook")? == 3
    );
    record(
        &case,
        "s24-frontiers.json",
        &json!({"candidate":first,"successor":rolled,"rollback":onward}),
    )?;
    rollback.stop()?;
    stopped(&mut lease, "rollback");
    next.stop()?;
    stopped(&mut lease, "successor");
    n.stop()?;
    stopped(&mut lease, "candidate");
    finish(&case, &lease, &spec)?;
    Ok(())
}

/// S25 uses the controller's decoded D-durable barrier and owned SIGKILL.
fn s25_cold_reopen(
    case: &Case,
    builds: &NodeBuilds,
    variant: &str,
    lease: &mut CaseLease,
    spec: &mut CaseSpec,
) -> Result<()> {
    let session = case.session_id(variant);
    let node = builds.n.serve_plugin_upgrade(case, None)?;
    note_process(lease, &node, "candidate", 1)?;
    spec.artifacts[0].generation = node.generation()?.into();
    let bind = node.bind()?;
    let accepted = control(&builds.n, case, "send", &session, &["--variant", variant])?;
    let input = accepted["input_id"]
        .as_str()
        .context("durable input identity")?;
    let a_body = wait_for("A's actual body entry", || {
        let bytes = std::fs::read(case.gate_dir().join("A.entered"));
        match bytes {
            Ok(bytes) => Ok(Some(serde_json::from_slice::<Value>(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    })?;
    let a_barrier = bind_held_body(case, &accepted, &a_body)?;
    let (mut controller, proof) = b_controller(case, &accepted)?;
    spec.cuts.extend([a_barrier.clone(), proof.barrier.clone()]);
    ensure!(
        proof.barrier.kind == lash_upgrade_harness::e2e::control::BarrierKind::DDurable
            && proof.journal_index.is_some(),
        "S25 requires a decoded durable B decision"
    );
    record(case, "s25-cut.json", &serde_json::to_value(&proof)?)?;
    controller.own_process("predecessor".into(), 1, node)?;
    let fault = block_on(controller.inject(
        Fault::KillHost {
            target: "predecessor".into(),
        },
        &proof,
    ))?;
    record(case, "s25-kill.json", &serde_json::to_value(fault)?)?;
    stopped(lease, "killed-candidate");

    // The successor controls cancellation. The predecessor keeps its drain
    // lane to finish its own journal, rather than decoding it as a new Run.
    let cancellation_host = builds.next.serve_plugin_upgrade(case, None)?;
    note_process(lease, &cancellation_host, "successor", 1)?;
    spec.artifacts[1].generation = cancellation_host.generation()?.into();
    control(&builds.next, case, "cancel", &session, &["--input", input])?;
    let predecessor = builds.n.serve_plugin_upgrade(case, Some(&bind))?;
    note_process(lease, &predecessor, "candidate", 2)?;
    // Public cancel has written its intent. Observe A's actual recorded-step
    // stop before release, so a ready body cannot win before the stop watch.
    let cancelled = wait_for("A's durable cancellation stop", || {
        match std::fs::read(case.gate_dir().join("A.cancelled")) {
            Ok(bytes) => Ok(Some(serde_json::from_slice::<Value>(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    })?;
    ensure!(
        cancelled["call_id"] == a_body["call_id"] && cancelled["run"] == a_body["run"],
        "another body observed the cancellation stop: {cancelled}"
    );
    record(case, "s25-cancel-stop.json", &cancelled)?;
    barriers(case)?.release(&a_barrier)?;
    let terminal = control(&builds.next, case, "follow", &session, &["--input", input])?;
    ensure!(
        serde_json::from_value::<lash::SendOutcome>(terminal.clone())?.status()
            == lash::TurnStatus::Cancelled,
        "cancelled A did not terminate: {terminal}"
    );
    quiesce(case)?;
    predecessor.stop()?;
    stopped(lease, "predecessor-drain");
    cancellation_host.stop()?;
    stopped(lease, "cancellation-host");
    let successor = builds.next.serve_plugin_upgrade(case, None)?;
    note_process(lease, &successor, "successor", 2)?;
    let state = control(&builds.next, case, "read", &session, &[])?;
    let b_plugin = if variant == "namespace" {
        OTHER
    } else {
        PLUGIN
    };
    let b_key = if variant == "disjoint" { "b" } else { "value" };
    ensure!(
        state_value(&state, b_plugin, b_key).as_deref() == Some("B"),
        "durable B lost/duplicated or A leaked: {state}"
    );
    if variant == "disjoint" {
        ensure!(
            state["state"][PLUGIN]["values"].get("a").is_none(),
            "unrecorded A published"
        );
    }
    if variant == "namespace" {
        ensure!(
            state["state"][PLUGIN]["values"].get("value").is_none(),
            "A's namespace published"
        );
    }
    let frontier = frontier(&state, b_plugin)?;
    ensure!(
        frontier.applied.is_some() && frontier.receipts.len() == 1,
        "B's exact frontier was not retained: {frontier:?}"
    );
    let before = entries(case)?;
    for _ in 0..2 {
        ensure!(
            control(&builds.next, case, "read", &session, &[])? == state,
            "cold checkpoint read changed state/frontier"
        );
        same_terminal(
            &terminal,
            &control(&builds.next, case, "follow", &session, &["--input", input])?,
        )?;
    }
    let after = entries(case)?;
    ensure!(
        serde_json::to_value(after)? == serde_json::to_value(&before)?,
        "completed replay invoked code"
    );
    ensure!(
        before
            .iter()
            .filter(|entry| entry.phase == "body" && entry.detail["symbol"] == "B")
            .count()
            == 1,
        "durable B's body reran"
    );
    ensure!(
        before
            .iter()
            .filter(|entry| entry.phase == "reducer" && entry.detail == "B")
            .count()
            == 1
            && before
                .iter()
                .all(|entry| entry.phase != "reducer" || entry.detail != "A"),
        "durable B reduced again or cancelled A reduced"
    );
    record(
        case,
        "s25-final.json",
        &json!({"variant":variant,"state":state,"terminal":terminal,"entries":before}),
    )?;
    successor.stop()?;
    stopped(lease, "cold-successor");
    record(
        case,
        "s25-composition.json",
        &json!({"leg":"D-durable/SIGKILL/cancel/cold-reopen", "held_terminal":"separate H4 invariant helper: pending drain, one terminal on release, SIGKILL redrive on the same seal without a fence raise or second segment", "refusal_laws":["a_stale_fence_writes_nothing", "a_checkpoint_refuses_a_stale_fence_whatever_its_caps"], "note":"Per ruling 14379, this composes recovery, held-terminal invariants and the store-tier refusal oracle instead of the plan's single-run wording. SIGKILL cannot deliver a stale store request, and a held terminal is not a quiet point admitting a higher fence. No live stale-refusal claim."}),
    )?;
    finish(case, lease, spec)?;
    Ok(())
}

fn cold_reopen(variant: &str) -> Result<()> {
    let builds = NodeBuilds::from_env()?;
    let (case, mut lease, mut spec) = setup(&format!("s25-{variant}"), "Cancelled", &builds)?;
    s25_cold_reopen(&case, &builds, variant, &mut lease, &mut spec)
}

#[test]
#[ignore = "needs actual binary pair and private live Restate"]
fn s25_same_key_cold_reopen_preserves_the_durable_sibling() -> Result<()> {
    cold_reopen("same")
}

#[test]
#[ignore = "needs actual binary pair and private live Restate"]
fn s25_disjoint_keys_cold_reopen_preserves_the_durable_sibling() -> Result<()> {
    cold_reopen("disjoint")
}

#[test]
#[ignore = "needs actual binary pair and private live Restate"]
fn s25_namespaces_cold_reopen_preserves_the_durable_sibling() -> Result<()> {
    cold_reopen("namespace")
}
