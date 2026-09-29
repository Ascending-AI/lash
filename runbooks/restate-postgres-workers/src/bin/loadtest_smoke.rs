use anyhow::{Context, Result, ensure};
use lash_postgres_store::PostgresStorage;
use lash_restate_postgres_workers_e2e::{
    TurnRequest, TurnResponse, TurnScenario, e2e_backend, expected_attachment_bytes, required_env,
    s3_store_from_env, turn_session_id, witness,
};
use std::{sync::Arc, time::Duration};

#[tokio::main]
async fn main() -> Result<()> {
    let storage = PostgresStorage::connect(&required_env("DATABASE_URL")?).await?;
    let ingress = required_env("RESTATE_INGRESS_URL")?;
    let engine = e2e_backend(
        &storage,
        Arc::new(s3_store_from_env()?),
        ingress.clone(),
        required_env("RESTATE_ADMIN_URL")?,
        lash_restate::RestateAuthorityId::new(required_env("RESTATE_AUTHORITY_ID")?)?,
    );
    engine
        .register_deployment(&required_env("WORKER_DEPLOYMENT_URL")?)
        .await?;
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(180))
        .build()?;
    let witness = witness::connect_witness().await?;
    let workflow_id = format!("topology-{}", uuid::Uuid::new_v4());
    let request = TurnRequest {
        workflow_id: workflow_id.clone(),
        fail_once: false,
        scenario: TurnScenario::KitchenSink,
        signal: None,
    };
    witness::record_submission(&witness, &workflow_id, &serde_json::to_vec(&request)?).await?;
    let output = client
        .post(format!("{ingress}/E2eTurnWorkflow/{workflow_id}/run"))
        .json(&request)
        .send()
        .await?
        .error_for_status()?
        .bytes()
        .await?;
    witness::record_client_terminal(&witness, &workflow_id, "observed", &output).await?;
    let response: TurnResponse =
        serde_json::from_slice(&output).context("decode public turn outcome")?;
    ensure!(
        !response.attachment_id.is_empty(),
        "public turn produced no attachment"
    );
    let workers: Vec<String> = required_env("WORKER_CONTROL_URLS")?
        .split(',')
        .map(str::to_owned)
        .collect();
    ensure!(
        workers.len() >= 2,
        "cross-worker smoke requires at least two workers"
    );
    let mut peer_reads = 0;
    for worker in workers {
        let read: serde_json::Value = client
            .get(format!(
                "{worker}/topology/attachments/{}/{}",
                turn_session_id(&workflow_id),
                response.attachment_id,
            ))
            .send()
            .await?
            .error_for_status()?
            .json()
            .await?;
        let bytes: Vec<u8> = serde_json::from_value(read["bytes"].clone())?;
        ensure!(
            bytes == expected_attachment_bytes(&workflow_id),
            "peer attachment bytes changed"
        );
        ensure!(
            read["committed"] == true,
            "peer did not observe committed PG ownership"
        );
        if read["worker_id"] != response.worker_id {
            peer_reads += 1;
        }
        println!(
            "attachment read passed worker={} writer={} bytes={}",
            read["worker_id"],
            response.worker_id,
            bytes.len()
        );
    }
    ensure!(peer_reads >= 1, "no independent worker read the attachment");
    println!(
        "topology smoke passed: public_turns=1 peer_reads={peer_reads} pg_ownership=committed s3_bytes=identical workflow={workflow_id}"
    );
    Ok(())
}
