//! H0 witnesses: R1 faults need actual work/journal provenance; R3 requires
//! real process and peer loss, retained data, observed leaders and cleanup.
mod fleet;
mod plugin_upgrade;
mod cancel;
mod h2;
mod tools;
use anyhow::{Context, Result, ensure};
use lash_upgrade_harness::e2e::control::process::{
    ProxyCommand, ProxyConfig, command as proxy_command,
};
use lash_upgrade_harness::e2e::control::transport::PublicationCut;
use lash_upgrade_harness::e2e::{
    case::{ArtifactIdentity, CaseLease},
    cluster::{ClusterControl, LocalCluster},
    control::{Barrier, BarrierKind, BarrierProof, FileBarriers, WorkIdentity},
    evidence::{Counts, Evidence},
};
use lash_upgrade_harness::harness::{Case, NodeBinary, ServeOptions, Services};
use lash_upgrade_harness::identity::BuildLabel;

#[tokio::test]
async fn core_r1_body_callback_waits_for_admitted_identity_and_controller_release() -> Result<()> {
    use lash_upgrade_harness::e2e::control::callback::{BodyCallbacks, ToolDelivery};
    let directory = tempfile::tempdir()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut callbacks = BodyCallbacks::start(
        std::net::TcpListener::bind("127.0.0.1:0")?,
        directory.path().to_owned(),
        deadline,
    )
    .await?;
    let mut work = identity();
    work.call = Some("actual-call".into());
    work.ordinal = Some(2);
    let barrier = Barrier {
        work: work.clone(),
        kind: BarrierKind::BodyEntered,
    };
    let barriers = FileBarriers::new(directory.path().to_owned(), deadline)?;
    let endpoint = callbacks.endpoint.clone();
    let request = tokio::spawn(async move {
        reqwest::Client::builder()
            .no_proxy()
            .build()?
            .post(endpoint)
            .json(&ToolDelivery {
                label: "A".into(),
                call_id: "actual-call".into(),
                ordinal: 2,
                logical_run: "accepted-run".into(),
                completion: serde_json::Value::Null,
                owner: lash_core::ExecutionOwner::SessionFrame {
                    session_id: "callback-session".into(),
                    agent_frame_id: lash_core::FrameNodeId::new("callback-frame")?,
                },
            })
            .send()
            .await?
            .error_for_status()
            .map_err(anyhow::Error::from)
    });
    callbacks.bind("accepted-run".into(), identity())?;
    let proof = callbacks
        .await_delivery("accepted-run", "A", 2, BarrierKind::BodyEntered)
        .await?;
    ensure!(
        proof.barrier.work == work && proof.journal_index.is_none(),
        "body receipt changed identity or asserted journal durability"
    );
    let held = std::fs::read_dir(directory.path())?
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "hold"))
        .any(|entry| {
            std::fs::read(entry.path())
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Barrier>(&bytes).ok())
                .as_ref()
                == Some(&barrier)
        });
    ensure!(
        held,
        "body identity was revealed before its controller hold"
    );
    ensure!(
        !request.is_finished(),
        "body returned before explicit release"
    );
    barriers.release(&barrier)?;
    tokio::time::timeout_at(deadline.into(), request).await???;
    callbacks.finish().await
}
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn artifact(role: &str, path: PathBuf) -> Result<ArtifactIdentity> {
    let sha256 = lash_core::stable_hash::sha256_hex(&std::fs::read(&path)?);
    Ok(ArtifactIdentity {
        role: role.into(),
        path,
        sha256,
        candidate_sha: std::env::var("LASH_E2E_CANDIDATE_SHA")?,
        generation: "candidate".into(),
    })
}
fn setup(name: &str) -> Result<(CaseLease, LocalCluster, ArtifactIdentity)> {
    let root = PathBuf::from(std::env::var("LASH_E2E_ARTIFACT_DIR")?);
    std::fs::create_dir_all(&root)?;
    let deadline = Instant::now() + Duration::from_secs(120);
    let lease = CaseLease::new(name, root.join(name), deadline)?;
    let base: u16 = std::env::var("LASH_E2E_PORT_BASE")?.parse()?;
    let binary = artifact(
        "restate-server",
        PathBuf::from(std::env::var("LASH_RESTATE_SERVER_BIN")?),
    )?;
    Ok((lease, LocalCluster::new(base, deadline), binary))
}
fn identity() -> WorkIdentity {
    WorkIdentity {
        ingress: "core-readiness".into(),
        run: "core-readiness".into(),
        segment: "0".into(),
        call: None,
        ordinal: None,
    }
}
fn write(path: &Path, value: &impl serde::Serialize) -> Result<()> {
    std::fs::write(path, serde_json::to_vec_pretty(value)?)?;
    Ok(())
}
fn finish_case(
    directory: &Path,
    name: &str,
    result: Result<Evidence>,
    cleanup: Result<Vec<lash_upgrade_harness::e2e::control::CleanupReceipt>>,
) -> Result<()> {
    use lash_upgrade_harness::e2e::evidence::{CaseReceipt, Verdict};
    let (mut evidence, mut reason) = match result {
        Ok(evidence) => (evidence, None),
        Err(error) => (Evidence::empty(name.into()), Some(error.to_string())),
    };
    match cleanup {
        Ok(cleanup) => evidence.cleanup.extend(cleanup),
        Err(error) => {
            evidence
                .cleanup
                .push(lash_upgrade_harness::e2e::control::CleanupReceipt {
                    resource: "cluster".into(),
                    closed: false,
                    detail: error.to_string(),
                });
            reason = Some(format!("{}; cleanup: {error}", reason.unwrap_or_default()));
        }
    }
    let verdict = reason
        .as_ref()
        .map_or(Verdict::Passed, |reason| Verdict::Failed {
            reason: reason.clone(),
        });
    let receipt = CaseReceipt { evidence, verdict };
    let counts = receipt.write(directory)?;
    if let Some(reason) = reason {
        anyhow::bail!("{reason}");
    }
    counts.reconcile()?;
    println!(
        "{name} selected={} executed={} passed={} failed={} not_run={}",
        counts.selected, counts.executed, counts.passed, counts.failed, counts.not_run
    );
    Ok(())
}
#[test]
fn core_r1_r3_refuses_missing_execution_and_false_durable_barriers() -> Result<()> {
    ensure!(
        Counts::default().reconcile().is_err(),
        "zero selection cannot pass"
    );
    ensure!(
        Counts {
            selected: 2,
            executed: 1,
            passed: 1,
            ..Default::default()
        }
        .reconcile()
        .is_err(),
        "missing execution cannot pass"
    );
    ensure!(
        Counts {
            selected: 1,
            not_run: 1,
            ..Default::default()
        }
        .reconcile()
        .is_err(),
        "unavailable infrastructure cannot pass"
    );
    let directory = tempfile::tempdir()?;
    let barriers = FileBarriers::new(
        directory.path().to_owned(),
        Instant::now() + Duration::from_secs(1),
    )?;
    let barrier = Barrier {
        work: identity(),
        kind: BarrierKind::XDurable,
    };
    ensure!(
        barriers
            .publish(&BarrierProof {
                barrier,
                artifact: "body-reached.json".into(),
                journal_index: None
            })
            .is_err(),
        "body evidence cannot certify durable X"
    );
    Counts {
        selected: 1,
        executed: 1,
        passed: 1,
        ..Default::default()
    }
    .reconcile()?;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn core_r1_one_node_host_sigkill_reopens_real_journal_and_cleans_up() -> Result<()> {
    let (mut lease, mut cluster, binary) = setup("one-node")?;
    let result: Result<Evidence> = async {
        let boot = cluster.boot(&binary,1,&mut lease).await?;
        write(&lease.directory.join("boot.json"),&boot)?;
        let host_artifact = artifact("upgrade-node", PathBuf::from(std::env::var("LASH_UPGRADE_NODE_N")?))?;
        host_artifact.verify()?;
        let host = NodeBinary::at(host_artifact.path.clone(),BuildLabel::N);
        let services = Services { ingress_url: boot.nodes[0].ingress_url.clone(), admin_url: boot.nodes[0].admin_url.clone(), postgres_url: String::new() };
        let scratch = lease.directory.clone();
        let (case, mut serving) = tokio::task::spawn_blocking(move || -> Result<_> {
            let case = Case::sqlite("host",&services,&scratch)?;
            let serving = host.serve_with(&case,&ServeOptions { unregistered:true,register_later:true,..Default::default() })?;
            Ok((case,serving))
        }).await??;
        let proxy_artifact=artifact("publication-proxy",PathBuf::from(std::env::var("LASH_E2E_PROXY_BIN")?))?;
        proxy_artifact.verify()?;
        let proxy_directory=lease.directory.join("publication-proxy");
        std::fs::create_dir(&proxy_directory)?;
        let socket=PathBuf::from(format!("target/h0-proxy-{}.sock",std::process::id()));
        let ready=proxy_directory.join("ready.json");
        let config=ProxyConfig { listen:format!("127.0.0.1:{}",cluster.port_base+20).parse()?,upstream:serving.bind()?.parse()?,directory:proxy_directory.clone(),control_socket:socket.clone(),ready_file:ready.clone(),deadline_secs:120,cuts:Vec::new() };
        let endpoint=format!("http://{}",config.listen);
        let config_path=proxy_directory.join("config.json");
        write(&config_path,&config)?;
        let mut command=std::process::Command::new(&proxy_artifact.path);
        command.arg(config_path);
        let mut proxy=lash_upgrade_harness::harness::ServingNode::spawn(&mut command,&proxy_directory.join("process.log"))?;
        loop {
            proxy.assert_running()?;
            if ready.is_file() { break; }
            ensure!(Instant::now()<lease.deadline,"independent publication holder never became ready");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let host_path=host_artifact.path.clone();
        let (case,serving,pending,session)=tokio::task::spawn_blocking(move || -> Result<_> {
            serving.register(&endpoint)?;
            let session=case.session_id("real-ingress");
            let pending=NodeBinary::at(host_path,BuildLabel::N).spawn_turn(&case,&session,"hold:core-r1")?;
            case.await_gate("core-r1")?;
            Ok((case,serving,pending,session))
        }).await??;
        let mut serving=serving;
        let view = case.view()?;
        let prefix = view.service_name("LashTurn");
        #[derive(serde::Deserialize)]
        struct Row { id: String, target_service_key: String, pinned_service_protocol_version: Option<u32> }
        let rows: Vec<Row> = view.query(&format!("SELECT id, target_service_key, pinned_service_protocol_version FROM sys_invocation WHERE target_service_name LIKE '{}%' AND target_handler_name = 'run' AND status <> 'completed'",prefix)).await?;
        ensure!(rows.len()==1,"held host must own one actual Run invocation: {}",rows.len());
        let row = &rows[0];
        let work = WorkIdentity { ingress: session.clone(), run: row.target_service_key.clone(), segment: row.id.clone(), call: None, ordinal: None };
        let protocol = row.pinned_service_protocol_version.context("invocation has no observed protocol")?;
        let pre = view.journal(&work,&row.id,protocol).await?;
        ensure!(!pre.is_empty(),"pre-fault journal must exist");
        write(&lease.directory.join("pre-fault-journal.json"),&pre)?;
        let slot=pre.iter().find(|fact|fact.entry_type=="Command: Run" && fact.name.as_deref().is_some_and(|name|name.contains(":llm_call:"))).context("provider gate has no actual LLM Run slot")?.name.clone().context("Run slot has no name")?;
        let publication=Barrier { work:work.clone(),kind:BarrierKind::PublicationRequest };
        let before_ack=Barrier { work:work.clone(),kind:BarrierKind::BeforeAck };
        let barriers=FileBarriers::new(proxy_directory.clone(),lease.deadline)?;
        barriers.hold(&publication)?;
        proxy_command(&socket,&ProxyCommand::Bind { invocation:row.id.clone(),work:work.clone() },lease.deadline).await?;
        proxy_command(&socket,&ProxyCommand::ArmPublication { cut:PublicationCut { invocation:row.id.clone(),journal_name:slot.clone(),proposal:publication.clone(),before_ack } },lease.deadline).await?;
        case.release("core-r1")?;
        let proof=barriers.await_proof(&publication).await?;
        ensure!(!view.journal(&work,&row.id,protocol).await?.iter().any(|fact|fact.entry_type=="Notification: Run" && fact.name.as_deref()==Some(&slot)),"held proposal was already durable");
        let bind=serving.bind()?;
        let predecessor_pid=serving.pid()?;
        serving.kill_and_reap()?;
        let killed=lash_upgrade_harness::e2e::control::FaultReceipt {
            fault:lash_upgrade_harness::e2e::control::Fault::KillHost { target:"upgrade-node".into() },
            proof:proof.clone(),
            target_incarnation:1,
        };
        proxy.assert_running()?;
        barriers.release(&publication)?;
        loop {
            let observed=view.journal(&work,&row.id,protocol).await?;
            if observed.iter().any(|fact|fact.entry_type=="Notification: Run" && fact.name.as_deref()==Some(&slot)) { break; }
            proxy.assert_running()?;
            ensure!(Instant::now()<lease.deadline,"independent holder did not deliver its retained completion after host SIGKILL");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let path=host_artifact.path.clone();
        let (case,serving)=tokio::task::spawn_blocking(move || -> Result<_> {
            let host=NodeBinary::at(path,BuildLabel::N);
            let serving=host.serve_with(&case,&ServeOptions { bind:Some(bind),unregistered:true,register_later:false })?;
            Ok((case,serving))
        }).await??;
        let deadline=lease.deadline;
        let report = tokio::task::spawn_blocking(move || pending.wait_until(deadline)).await??;
        ensure!(report.status == "Answered", "cold host did not answer: {report:?}");
        let post = case.view()?.journal(&work,&row.id,protocol).await?;
        ensure!(post.len() >= pre.len(), "retained journal shrank on host restart");
        write(&lease.directory.join("post-fault-journal.json"),&post)?;
        let host_endpoint = serving.uri()?.to_owned();
        let successor_pid=serving.pid()?;
        ensure!(successor_pid!=predecessor_pid,"cold restart retained the dead physical process");
        serving.stop()?;
        ensure!(tokio::net::TcpStream::connect(host_endpoint.trim_start_matches("http://")).await.is_err(), "host listener leaked");
        let mut evidence = Evidence::empty("H0-R1".into());
        evidence.artifacts = vec![binary.clone(),host_artifact,proxy_artifact];
        evidence.barriers.push(proof);
        evidence.faults.push(killed);
        evidence.stores.push(serde_json::json!({"predecessor_pid":predecessor_pid,"successor_pid":successor_pid,"reopened_bind":host_endpoint}));
        let effects=case.effects_of("hold:core-r1")?;
        ensure!(effects.len()==1,"accepted retained provider completion was executed again after cold restart");
        evidence.effects.push(serde_json::to_value(effects)?);
        proxy_command(&socket,&ProxyCommand::Stop,lease.deadline).await?;
        proxy.wait_success(lease.deadline).await?;
        ensure!(tokio::net::UnixStream::connect(&socket).await.is_err(),"proxy control socket leaked");
        evidence.cleanup.push(lash_upgrade_harness::e2e::control::CleanupReceipt { resource:"publication-proxy".into(),closed:true,detail:"independent process exited successfully; TCP and control listeners closed".into() });
        evidence.journals = post;
        evidence.stores.push(serde_json::to_value(report)?);
        evidence.cleanup.push(lash_upgrade_harness::e2e::control::CleanupReceipt { resource: "host".into(), closed: true, detail: "host reaped; listener refused connections; case Restate deployment reclaimed with cluster shutdown".into() });
        Ok(evidence)
    }.await;
    let cleanup = cluster.finish().await;
    finish_case(&lease.directory, "H0-R1", result, cleanup)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn core_r3_three_real_nodes_directed_partition_leader_loss_and_cleanup() -> Result<()> {
    let (mut lease, mut cluster, binary) = setup("three-node")?;
    let result: Result<Evidence> = async {
        let boot = cluster.boot(&binary,3,&mut lease).await?;
        write(&lease.directory.join("boot.json"),&boot)?;
        let prior = boot.leaders.first().context("no observed active partition leader")?.clone();
        ensure!(prior.epoch>0,"leader epoch must be observed");
        loop {
            let links = cluster.link_state()?;
            if (1..=3).all(|from| (1..=3).all(|to| from==to || links.active.values().any(|link| *link==(from,to)))) { break; }
            ensure!(Instant::now()<lease.deadline,"six directed peer links never connected");
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let closed = cluster.partition_receipt(1,2).await?;
        ensure!(closed>0,"directed cut missed every established stream");
        let partitioned = cluster.link_state()?;
        ensure!(!partitioned.active.values().any(|link| *link==(1,2)),"selected link remains connected");
        ensure!(partitioned.active.values().any(|link| *link==(2,3)),"reachable majority link was lost");
        write(&lease.directory.join("partition.json"),&serde_json::json!({"from":1,"to":2,"closed_streams":closed,"opened":format!("{:?}",partitioned.opened),"dropped":format!("{:?}",partitioned.dropped)}))?;
        cluster.heal(1,2).await?;
        let healed = cluster.converge().await?;
        write(&lease.directory.join("healed.json"),&healed)?;
        let proof = BarrierProof { barrier: Barrier { work: identity(),kind: BarrierKind::HostReady }, artifact: lease.directory.join("healed.json").display().to_string(), journal_index: None };
        let killed = cluster.kill(prior.node,&proof).await?;
        write(&lease.directory.join("kill.json"),&killed)?;
        let successor = cluster.await_leader_change(&prior).await?;
        ensure!(successor.node!=prior.node && successor.epoch>prior.epoch,"leader loss did not produce a new epoch");
        let restarted = cluster.restart(prior.node).await?;
        let original = boot.nodes.iter().find(|node| node.node==prior.node).context("killed node missing from boot")?;
        ensure!(restarted.data_directory==original.data_directory && restarted.incarnation==original.incarnation+1,"restart did not retain node identity/data");
        let final_cluster=cluster.converge().await?;
        write(&lease.directory.join("restarted.json"),&final_cluster)?;
        let mut evidence=Evidence::empty("H0-R3".into());
        evidence.artifacts.push(binary.clone());
        evidence.stores.push(serde_json::to_value(boot)?);
        evidence.stores.push(serde_json::to_value(final_cluster)?);
        evidence.faults.push(killed);
        Ok(evidence)
    }.await;
    let cleanup = cluster.finish().await;
    finish_case(&lease.directory, "H0-R3", result, cleanup)
}

/// R1: a physical host outage cannot remove the controller endpoint needed
/// for the same admitted invocation's cold reconnect (H1 S06 regression).
#[tokio::test]
async fn core_r1_proxy_reconnects_after_owned_upstream_downtime() -> Result<()> {
    use lash_upgrade_harness::e2e::control::transport::V7Proxy;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let directory = tempfile::tempdir()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let upstream = std::net::TcpListener::bind("127.0.0.1:0")?;
    let address = upstream.local_addr()?;
    drop(upstream);
    let mut proxy = V7Proxy::start(
        std::net::TcpListener::bind("127.0.0.1:0")?,
        address,
        directory.path().to_owned(),
        deadline,
        Vec::new(),
    )
    .await?;
    let endpoint = proxy.endpoint.trim_start_matches("http://");
    let mut refused = tokio::net::TcpStream::connect(endpoint).await?;
    let mut bytes = Vec::new();
    tokio::time::timeout_at(deadline.into(), refused.read_to_end(&mut bytes)).await??;
    ensure!(
        bytes.is_empty(),
        "unavailable upstream returned fabricated bytes"
    );
    let upstream = tokio::net::TcpListener::bind(address).await?;
    let serving = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await?;
        let mut request = Vec::new();
        stream.read_to_end(&mut request).await?;
        ensure!(
            request == b"GET /discover HTTP/1.1\r\nHost: fixture\r\n\r\n",
            "reconnect changed discovery bytes"
        );
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await?;
        stream.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    });
    let mut reconnected = tokio::net::TcpStream::connect(endpoint)
        .await
        .context("upstream outage killed the reconnect listener")?;
    reconnected
        .write_all(b"GET /discover HTTP/1.1\r\nHost: fixture\r\n\r\n")
        .await?;
    reconnected.shutdown().await?;
    let mut response = Vec::new();
    tokio::time::timeout_at(deadline.into(), reconnected.read_to_end(&mut response)).await??;
    ensure!(
        response.ends_with(b"\r\n\r\nok"),
        "cold reconnect did not forward the actual host reply"
    );
    tokio::time::timeout_at(deadline.into(), serving).await???;
    proxy.finish().await
}

/// R1: a targeted physical disconnect applies to existing streams, while a
/// later incarnation can reconnect (second H1 S06 regression).
#[tokio::test]
async fn core_r1_proxy_disconnect_preserves_new_connections() -> Result<()> {
    use lash_upgrade_harness::e2e::control::transport::V7Proxy;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    const REQUEST: &[u8] = b"GET /discover HTTP/1.1\r\nHost: fixture\r\n\r\n";
    let directory = tempfile::tempdir()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    let upstream = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let mut proxy = V7Proxy::start(
        std::net::TcpListener::bind("127.0.0.1:0")?,
        upstream.local_addr()?,
        directory.path().to_owned(),
        deadline,
        Vec::new(),
    )
    .await?;
    let endpoint = proxy.endpoint.trim_start_matches("http://");
    let mut original = tokio::net::TcpStream::connect(endpoint).await?;
    original.write_all(REQUEST).await?;
    let (mut accepted, _) = tokio::time::timeout_at(deadline.into(), upstream.accept()).await??;
    let mut received = vec![0; REQUEST.len()];
    tokio::time::timeout_at(deadline.into(), accepted.read_exact(&mut received)).await??;
    ensure!(
        received == REQUEST,
        "original stream was not established at the actual host"
    );
    ensure!(
        proxy.disconnect(deadline).await? == 1,
        "disconnect missed the established stream"
    );
    let serving = tokio::spawn(async move {
        let (mut stream, _) = upstream.accept().await?;
        let mut request = Vec::new();
        stream.read_to_end(&mut request).await?;
        ensure!(
            request == REQUEST,
            "new discovery stream did not survive the prior disconnect"
        );
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
            .await?;
        stream.shutdown().await?;
        Ok::<_, anyhow::Error>(())
    });
    let mut reconnected = tokio::net::TcpStream::connect(endpoint).await?;
    reconnected.write_all(REQUEST).await?;
    reconnected.shutdown().await?;
    let mut response = Vec::new();
    tokio::time::timeout_at(deadline.into(), reconnected.read_to_end(&mut response)).await??;
    ensure!(
        response.ends_with(b"\r\n\r\nok"),
        "prior disconnect cancelled the fresh reconnect"
    );
    tokio::time::timeout_at(deadline.into(), serving).await???;
    proxy.finish().await
}
