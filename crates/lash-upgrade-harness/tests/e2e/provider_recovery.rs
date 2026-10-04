//! H1 R1/R4/R5/L02/L17/L19/L22 recovery scenarios over an owned node,
//! file SQLite, real V7 transport, and an independently synced outside ledger.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, ensure};
use lash_upgrade_harness::e2e::{
    case::{ArtifactIdentity, CaseLease},
    cluster::{ClusterControl, LocalCluster},
    control::{
        Barrier, BarrierKind, BarrierProof, Control, CoreControl, FileBarriers, WorkIdentity,
        callback::BodyCallbacks, transport::V7Proxy,
    },
    evidence::{Evidence, RestateEvidenceReader},
    host::{HostAdapter, HostCommand},
    provider_http::{
        RecordedHttpFixture, TransportEvent,
        node_host::ProviderNodeHost,
        scenarios::{ProviderCaseEvidence, ProviderScenario, ProviderStoreObservation},
        transcript::{HttpTranscript, StreamEnd},
    },
};
use lash_upgrade_harness::node::{
    RestateArgs, StoreArgs,
    e2e_host::{ProviderHostCommand, ProviderHostConfig},
    e2e_tools::{BodyDelivery, ToolFixtureArgs},
};

const TIMEOUT: Duration = Duration::from_secs(120);

fn artifact(role: &str, variable: &str) -> Result<ArtifactIdentity> {
    let path = PathBuf::from(std::env::var(variable)?);
    Ok(ArtifactIdentity {
        role: role.into(),
        sha256: lash_core::stable_hash::sha256_hex(&std::fs::read(&path)?),
        path,
        candidate_sha: std::env::var("LASH_E2E_CANDIDATE_SHA")?,
        generation: "candidate".into(),
    })
}
fn write(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
fn bodies(path: &Path) -> Result<Vec<BodyDelivery>> {
    match std::fs::read_to_string(path) {
        Ok(text) => text
            .lines()
            .map(|line| serde_json::from_str(line).map_err(Into::into))
            .collect(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error.into()),
    }
}
async fn await_bodies(path: &Path, count: usize, deadline: Instant) -> Result<Vec<BodyDelivery>> {
    loop {
        let observed = bodies(path)?;
        if observed.len() >= count {
            return Ok(observed);
        }
        ensure!(Instant::now() < deadline, "fixture bodies never entered");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
fn call(base: &WorkIdentity, body: &BodyDelivery) -> Result<WorkIdentity> {
    ensure!(
        body.delivery.run == base.run,
        "body belongs to another admitted Run"
    );
    let mut work = base.clone();
    work.call = Some(body.delivery.call_id.clone());
    work.ordinal = Some(body.delivery.attempt);
    Ok(work)
}
async fn snapshot(host: &mut ProviderNodeHost, session: &str) -> Result<ProviderStoreObservation> {
    let value = host
        .request(&ProviderHostCommand::Snapshot {
            session: session.into(),
        })
        .await?;
    Ok(serde_json::from_value(value["observation"].clone())?)
}

async fn durable(
    restate: &RestateArgs,
    scenario: ProviderScenario,
    directory: &Path,
    deadline: Instant,
    barrier: &Barrier,
) -> Result<BarrierProof> {
    let view = lash_upgrade_harness::restate_view::RestateView::new(
        &restate.admin_url,
        &restate.namespace,
    )?;
    let mut reader = RestateEvidenceReader::new(scenario.id().into(), view, 7);
    reader.bind(&barrier.work, barrier.work.segment.clone())?;
    CoreControl::new(
        FileBarriers::new(directory.to_owned(), deadline)?,
        Box::new(reader),
    )
    .await_barrier(barrier)
    .await
}

async fn recovery(scenario: ProviderScenario, transcript: &[u8]) -> Result<()> {
    let slug = scenario.id().to_ascii_lowercase();
    let root = PathBuf::from(std::env::var("LASH_E2E_ARTIFACT_DIR")?);
    std::fs::create_dir_all(&root)?;
    let deadline = Instant::now() + TIMEOUT;
    let mut lease = CaseLease::new(&slug, root.join(&slug), deadline)?;
    let port: u16 = std::env::var("LASH_E2E_PORT_BASE")?.parse()?;
    let server = artifact("restate-server", "LASH_RESTATE_SERVER_BIN")?;
    let node = artifact("h1-provider-node", "LASH_UPGRADE_NODE_N")?;
    scenario
        .spec(vec![server.clone(), node.clone()])
        .validate()?;
    let mut cluster = LocalCluster::new(port, deadline);
    let boot = cluster.boot(&server, 1, &mut lease).await?;
    write(&lease.directory.join("boot.json"), &boot)?;
    let fixture = RecordedHttpFixture::start(
        ([127, 0, 0, 1], port + 32).into(),
        HttpTranscript::from_json(transcript)?,
        &lease.directory.join("effects.jsonl"),
    )
    .await?;
    let barrier_dir = lease.directory.join("barriers");
    let barriers = FileBarriers::new(barrier_dir.clone(), deadline)?;
    let mut callbacks = BodyCallbacks::start(
        std::net::TcpListener::bind(("127.0.0.1", port + 33))?,
        barrier_dir.clone(),
        deadline,
    )
    .await?;
    let body_path = lease.directory.join("bodies.jsonl");
    let mut proxy = if scenario != ProviderScenario::S03 {
        Some(
            V7Proxy::start(
                std::net::TcpListener::bind(("127.0.0.1", port + 34))?,
                ([127, 0, 0, 1], port + 30).into(),
                barrier_dir.clone(),
                deadline,
                Vec::new(),
            )
            .await?,
        )
    } else {
        None
    };
    let restate = RestateArgs {
        ingress_url: boot.nodes[0].ingress_url.clone(),
        admin_url: boot.nodes[0].admin_url.clone(),
        authority: lease.authority.clone(),
        namespace: lease.namespace.clone(),
    };
    let config = ProviderHostConfig {
        provider_url: fixture.base_url(),
        tools: ToolFixtureArgs {
            effect_url: fixture.effect_url(),
            bodies: body_path.clone(),
            backoff_ms: 30_000,
            callback_url: Some(callbacks.endpoint.clone()),
        },
        worker_bind: ([127, 0, 0, 1], port + 30).into(),
        control_bind: ([127, 0, 0, 1], port + 31).into(),
        deployment_uri: proxy.as_ref().map(|proxy| proxy.endpoint.clone()),
        ready_file: lease.directory.join("initial-ready.json"),
        timeout_ms: 120_000,
    };
    let store = StoreArgs {
        store: format!("sqlite:{}", lease.directory.join("store").display())
            .parse()
            .map_err(anyhow::Error::msg)?,
        data_dir: lease.directory.join("host-data"),
    };
    let mut host = ProviderNodeHost::new(config, store, restate.clone())?;
    let mut evidence = Evidence::empty(scenario.id().into());
    evidence.artifacts = vec![server, node.clone()];
    let proof: Result<ProviderCaseEvidence> = async {
        host.boot(&node,&mut lease).await?;
        let session = format!("h1-{slug}-session"); let input = format!("{slug}-input");
        let submitted = host.command(HostCommand::Submit { session:session.clone(),idempotency_key:input,input:serde_json::json!(format!("{slug} input")) }).await?;
        let base = submitted.work;
        let first = await_bodies(&body_path,if matches!(scenario,ProviderScenario::S06|ProviderScenario::S07) {2} else {1},deadline).await?;
        let work = call(&base,&first[0])?;
        let entered = Barrier { work:work.clone(),kind:BarrierKind::BodyEntered };
        let entries=first.iter().map(|body|Ok(Barrier {work:call(&base,body)?,kind:BarrierKind::BodyEntered})).collect::<Result<Vec<_>>>()?;
        for barrier in &entries { barriers.hold(barrier)?; }
        if let Some(proxy)=&proxy { proxy.bind_invocation(base.segment.clone(),base.clone())?; }
        callbacks.bind(base.run.clone(),base.clone())?;
        for barrier in &entries { evidence.barriers.push(barriers.await_proof(barrier).await?); }
        let view = lash_upgrade_harness::restate_view::RestateView::new(&restate.admin_url,&restate.namespace)?;
        let before = snapshot(&mut host,&session).await?;
        let prefix;
        match scenario {
            ProviderScenario::S03 => {
                fixture.hold_effect(work.call.as_deref().context("no call")?,1,"effect-accepted",StreamEnd::Disconnect)?;
                barriers.release(&entered)?;
                let event=fixture.wait_for(TIMEOUT,|event|matches!(event,TransportEvent::EffectAccepted { .. })).await?;
                let TransportEvent::EffectAccepted { acceptance }=event else { anyhow::bail!("missing effect acceptance") };
                ensure!(acceptance.delivery==first[0].delivery,"effect receipt belongs to another body");
                fixture.wait_for(TIMEOUT,|event|matches!(event,TransportEvent::BarrierEntered {barrier,..} if barrier=="effect-accepted")).await?;
                write(&lease.directory.join("outside-acceptance.json"),&acceptance)?;
                let cut=BarrierProof { barrier:Barrier {work,kind:BarrierKind::SideEffectAccepted},artifact:lease.directory.join("outside-acceptance.json").display().to_string(),journal_index:None };
                prefix=view.journal(&base,&base.segment,7).await?;
                evidence.faults.push(host.kill(&cut)?); evidence.barriers.push(cut);
                fixture.release("effect-accepted")?;
            }
            ProviderScenario::S06|ProviderScenario::S07 => {
                let a=first.iter().find(|body|body.label=="A").context("A never entered")?;
                let b=first.iter().find(|body|body.label=="B").context("B never entered")?;
                let a_work=call(&base,a)?; let b_work=call(&base,b)?;
                let backoff=Barrier {work:b_work.clone(),kind:BarrierKind::RetryBackoffEntered};
                barriers.hold(&backoff)?;
                proxy.as_ref().context("no actual V7 proxy")?.arm_retry(backoff.clone())?;
                barriers.release(&Barrier {work:a_work.clone(),kind:BarrierKind::BodyEntered})?;
                evidence.barriers.push(durable(&restate,scenario,&barrier_dir,deadline,&Barrier {work:a_work,kind:BarrierKind::RetryScheduleDurable}).await?);
                barriers.release(&Barrier {work:b_work.clone(),kind:BarrierKind::BodyEntered})?;
                let cut=barriers.await_proof(&backoff).await?;
                evidence.barriers.push(durable(&restate,scenario,&barrier_dir,deadline,&Barrier {work:b_work,kind:BarrierKind::RetryScheduleDurable}).await?);
                prefix=view.journal(&base,&base.segment,7).await?;
                if scenario==ProviderScenario::S07 {
                    let cancel=host.command(HostCommand::Cancel {run:base.run.clone()}).await?;
                    ensure!(cancel.output["status"]=="requested","pending owner cancellation was not accepted");
                    write(&lease.directory.join("cancel.json"),&cancel)?;
                } else {
                    for body in &first {
                        let mut second=call(&base,body)?;second.ordinal=Some(2);
                        barriers.hold(&Barrier {work:second,kind:BarrierKind::BodyEntered})?;
                    }
                }
                evidence.faults.push(host.kill(&cut)?);evidence.barriers.push(cut);
                proxy.as_ref().context("no proxy")?.disconnect(deadline).await?;
                barriers.release(&backoff)?;
            }
            _ => anyhow::bail!("unsupported provider recovery scenario"),
        }
        host.boot(&node,&mut lease).await?;
        if scenario==ProviderScenario::S06 {
            let recovered=await_bodies(&body_path,4,deadline).await?;
            for label in ["B","A"] {
                let second=recovered.iter().find(|body|body.label==label && body.delivery.attempt==2).context("reported retry did not enter")?;
                let recovered_work=call(&base,second)?;
                let gate=Barrier {work:recovered_work.clone(),kind:BarrierKind::BodyEntered};
                evidence.barriers.push(barriers.await_proof(&gate).await?);
                barriers.release(&gate)?;
                evidence.barriers.push(durable(&restate,scenario,&barrier_dir,deadline,&Barrier {work:recovered_work,kind:BarrierKind::XDurable}).await?);
            }
        }
        let output=host.command(HostCommand::Attach {run:base.run.clone()}).await?;
        write(&lease.directory.join("terminal.json"),&output)?;
        let terminal:lash::TurnOutput=serde_json::from_value(output.output.clone())?;
        if scenario==ProviderScenario::S07 { ensure!(terminal.result.outcome.cancellation().is_some(),"cancelled backoff did not terminate Cancelled"); }
        else { ensure!(terminal.assistant_message()==Some(format!("{slug} answer").as_str()),"wrong settled answer"); }
        let after=snapshot(&mut host,&session).await?;
        evidence.journals=view.journal(&base,&base.segment,7).await?;
        evidence.outputs=host.transcript()?;
        evidence.stores=vec![serde_json::to_value(&before)?,serde_json::to_value(&after)?];
        Ok(ProviderCaseEvidence {scenario,evidence:evidence.clone(),http:fixture.receipt()?,bodies:bodies(&body_path)?,before,after,journal_prefix:prefix})
    }.await;
    if let Ok(proof) = &proof {
        write(&lease.directory.join("pre-cleanup-evidence.json"), proof)?;
    }
    let host_cleanup = host.stop().await;
    let proxy_cleanup = if let Some(proxy) = &mut proxy {
        proxy.finish().await
    } else {
        Ok(())
    };
    let callback_cleanup = callbacks.finish().await;
    let http = fixture.finish().await;
    let cluster_cleanup = cluster.finish().await;
    write(
        &lease.directory.join("cleanup.json"),
        &serde_json::json!({"host":host_cleanup.as_ref().map_err(ToString::to_string),"proxy":proxy_cleanup.as_ref().map_err(ToString::to_string),"callback":callback_cleanup.as_ref().map_err(ToString::to_string),"cluster":cluster_cleanup.as_ref().map_err(ToString::to_string)}),
    )?;
    let mut proof = proof?;
    proof.http = http?;
    for (resource, result) in [
        ("h1-provider-node", host_cleanup),
        ("private-restate-cluster", cluster_cleanup),
    ] {
        match result {
            Ok(cleanup) => proof.evidence.cleanup.extend(cleanup),
            Err(error) => {
                proof
                    .evidence
                    .cleanup
                    .push(lash_upgrade_harness::e2e::control::CleanupReceipt {
                        resource: resource.into(),
                        closed: false,
                        detail: format!("{error:#}"),
                    })
            }
        }
    }
    for (resource, result) in [
        ("body-callback", callback_cleanup),
        ("v7-proxy", proxy_cleanup),
    ] {
        proof
            .evidence
            .cleanup
            .push(lash_upgrade_harness::e2e::control::CleanupReceipt {
                resource: resource.into(),
                closed: result.is_ok(),
                detail: result
                    .map(|()| "owned listener and connections reaped".into())
                    .unwrap_or_else(|error| format!("{error:#}")),
            });
    }
    let deployment_closed =
        tokio::net::TcpStream::connect(boot.nodes[0].admin_url.trim_start_matches("http://"))
            .await
            .is_err()
            && tokio::net::TcpStream::connect(
                boot.nodes[0].ingress_url.trim_start_matches("http://"),
            )
            .await
            .is_err();
    proof
        .evidence
        .cleanup
        .push(lash_upgrade_harness::e2e::control::CleanupReceipt {
            resource: "h1-private-deployment".into(),
            closed: deployment_closed,
            detail:
                "deployment owner reaped; actual private admin and ingress listeners probed closed"
                    .into(),
        });
    write(&lease.directory.join("evidence.json"), &proof)?;
    proof.verify()?;
    println!(
        "{} selected=1 executed=1 passed=1 failed=0 not_run=0",
        scenario.id()
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s03_ambiguous_acceptance_survives_host_sigkill() -> Result<()> {
    recovery(
        ProviderScenario::S03,
        include_bytes!("../../testdata/e2e/providers/s03-tools.json"),
    )
    .await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s06_reported_retries_preserve_backoff_and_reverse_ready_order() -> Result<()> {
    recovery(
        ProviderScenario::S06,
        include_bytes!("../../testdata/e2e/providers/s06-tools.json"),
    )
    .await
}
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn s07_cancellation_during_backoff_survives_cold_restart() -> Result<()> {
    recovery(
        ProviderScenario::S07,
        include_bytes!("../../testdata/e2e/providers/s07-tools.json"),
    )
    .await
}
