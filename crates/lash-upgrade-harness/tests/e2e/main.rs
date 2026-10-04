//! H0 witnesses: R1 faults need actual work/journal provenance; R3 requires
//! real process and peer loss, retained data, observed leaders and cleanup.
use anyhow::{Context, Result, ensure};
use lash_upgrade_harness::e2e::{
    case::{ArtifactIdentity, CaseLease},
    cluster::{ClusterControl, LocalCluster},
    control::{Barrier, BarrierKind, BarrierProof, FileBarriers, WorkIdentity},
    evidence::{Counts, Evidence},
};
use lash_upgrade_harness::harness::{Case, NodeBinary, ServeOptions, Services};
use lash_upgrade_harness::identity::BuildLabel;
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
        let (case, mut serving, pending, session) = tokio::task::spawn_blocking(move || -> Result<_> {
            let case = Case::sqlite("host",&services,&scratch)?;
            let serving = host.serve(&case)?;
            let session = case.session_id("real-ingress");
            let pending = host.spawn_turn(&case,&session,"hold:core-r1")?;
            case.await_gate("core-r1")?;
            Ok((case,serving,pending,session))
        }).await??;
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
        let bind = serving.bind()?;
        serving.kill_and_reap()?;
        let path = host_artifact.path.clone();
        let (case, serving) = tokio::task::spawn_blocking(move || -> Result<_> {
            let host = NodeBinary::at(path,BuildLabel::N);
            let serving = host.serve_with(&case,&ServeOptions { bind: Some(bind), ..Default::default() })?;
            case.release("core-r1")?;
            Ok((case,serving))
        }).await??;
        let deadline=lease.deadline;
        let report = tokio::task::spawn_blocking(move || pending.wait_until(deadline)).await??;
        ensure!(report.status == "Answered", "cold host did not answer: {report:?}");
        let post = case.view()?.journal(&work,&row.id,protocol).await?;
        ensure!(post.len() >= pre.len(), "retained journal shrank on host restart");
        write(&lease.directory.join("post-fault-journal.json"),&post)?;
        let deployment = case.view()?.deployment_at(serving.uri()?).await?;
        let host_endpoint = serving.uri()?.to_owned();
        serving.stop()?;
        case.view()?.retire_deployment(&deployment.id).await?;
        ensure!(tokio::net::TcpStream::connect(host_endpoint.trim_start_matches("http://")).await.is_err(), "host listener leaked");
        let mut evidence = Evidence::empty("H0-R1".into());
        evidence.artifacts = vec![binary.clone(),host_artifact];
        evidence.journals = post;
        evidence.stores.push(serde_json::to_value(report)?);
        evidence.cleanup.push(lash_upgrade_harness::e2e::control::CleanupReceipt { resource: "host-and-deployment".into(), closed: true, detail: "host reaped and non-forced deployment retirement accepted".into() });
        Ok(evidence)
    }.await;
    let cleanup = cluster.finish().await;
    let mut evidence = result?;
    evidence.cleanup.extend(cleanup?);
    write(&lease.directory.join("evidence.json"), &evidence)?;
    println!("H0-R1 selected=1 executed=1 passed=1 failed=0 not_run=0");
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn core_r3_three_real_nodes_directed_partition_leader_loss_and_cleanup() -> Result<()> {
    let (mut lease, mut cluster, binary) = setup("three-node")?;
    let result: Result<()> = async {
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
        write(&lease.directory.join("restarted.json"),&cluster.converge().await?)?;
        Ok(())
    }.await;
    let cleanup = cluster.finish().await;
    write(
        &lease.directory.join("cleanup.json"),
        &cleanup.as_ref().map_err(ToString::to_string),
    )?;
    result?;
    cleanup?;
    println!("H0-R3 selected=1 executed=1 passed=1 failed=0 not_run=0");
    Ok(())
}
