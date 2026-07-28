use std::{sync::Arc, time::Duration};

use anyhow::{Context, Result};
use lash::TurnInput;
use lash_core::runtime::{RuntimeTurnPhase, RuntimeTurnPhaseProbe};
use lash_core::{LeaseOwnerIdentity, LeaseTimings, ProcessRegistry};
use lash_postgres_store::PostgresStorage;
use lash_provider_openai::OpenAiCompatibleProvider;
use serde_json::json;

use lash_restate_postgres_workers_e2e::{
    EXPECTED_FRAME_SWITCH_TEXT, env, required_env, s3_store_from_env,
};

const WORKFLOW_ID: &str = "e2e-frame-switch-crash";
const SESSION_ID: &str = "restate-postgres-workers-frame-crash-e2e";
const RECOVERY_LEASE_TTL: Duration = Duration::from_millis(300);
const RECOVERY_LEASE_RENEW_INTERVAL: Duration = Duration::from_millis(100);
const RECOVERY_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy)]
enum KillPoint {
    AfterSwitchCommit,
    FollowOnEffectLoop,
}

struct ExitProbe(KillPoint);

impl RuntimeTurnPhaseProbe for ExitProbe {
    fn begin(&self, phase: RuntimeTurnPhase) {
        if matches!(self.0, KillPoint::FollowOnEffectLoop) && phase == RuntimeTurnPhase::EffectLoop
        {
            std::process::exit(77);
        }
    }

    fn end(&self, phase: RuntimeTurnPhase) {
        if matches!(self.0, KillPoint::AfterSwitchCommit) && phase == RuntimeTurnPhase::FinalCommit
        {
            std::process::exit(76);
        }
    }
}

fn main() -> Result<()> {
    let mode = std::env::args()
        .nth(1)
        .context("expected frame-crash mode: commit, mid-follow, or recover")?;
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build frame-crash runtime")?
        .block_on(run(&mode))
}

async fn run(mode: &str) -> Result<()> {
    let database_url = required_env("DATABASE_URL")?;
    let storage = PostgresStorage::connect(&database_url)
        .await
        .context("connect frame-crash Postgres storage")?;
    let provider = lash_core::ProviderHandle::new(
        OpenAiCompatibleProvider::new(
            "e2e-key",
            format!(
                "{}/v1",
                env("MOCK_PROVIDER_BASE_URL", "http://mock-provider:18001").trim_end_matches('/')
            ),
        )
        .into_components(),
    );
    let factory = lash_protocol_rlm::RlmProtocolPluginFactory::new(
        lash_protocol_rlm::RlmProtocolPluginConfig::default(),
        Arc::new(storage.lashlang_artifact_store()),
    );
    let lease_timings = LeaseTimings::new(RECOVERY_LEASE_TTL, RECOVERY_LEASE_RENEW_INTERVAL)
        .context("validate frame-crash recovery lease timings")?;
    let core = lash::LashCore::rlm_builder(factory)
        .provider(provider)
        .model(
            lash::ModelSpec::from_token_limits("e2e-mock", Default::default(), 200_000, None)
                .map_err(anyhow::Error::msg)?,
        )
        .store_factory(Arc::new(storage.session_store_factory()))
        .attachment_store(Arc::new(s3_store_from_env()?))
        .process_env_store(Arc::new(storage.process_env_store()))
        .process_registry(Arc::new(storage.process_registry()) as Arc<dyn ProcessRegistry>)
        .trigger_store(Arc::new(storage.trigger_store()))
        .effect_host(Arc::new(lash::durability::InlineEffectHost::default()))
        .disable_queued_work_driver()
        .lease_timings(lease_timings)
        .build()
        .context("build frame-crash core")?;
    let owner = LeaseOwnerIdentity::opaque(
        "frame-crash-worker",
        format!("frame-crash-worker:{}", std::process::id()),
    );
    let session_builder = core.session(SESSION_ID).session_execution_owner(owner);
    let session = session_builder.open().await?;

    match mode {
        "commit" => {
            session
                .enqueue(TurnInput::text(format!(
                    "Run crash-recovered frame switch. workflow_id={WORKFLOW_ID} frame_switch_crash_start=true"
                )))
                .id(format!("{WORKFLOW_ID}:original"))
                .send()
                .await?;
            session
                .set_turn_phase_probe(Arc::new(ExitProbe(KillPoint::AfterSwitchCommit)))
                .await;
            let _ = session.queued_turn().run().await?;
            anyhow::bail!("commit crash probe did not terminate the process")
        }
        "mid-follow" => {
            session
                .set_turn_phase_probe(Arc::new(ExitProbe(KillPoint::FollowOnEffectLoop)))
                .await;
            let deadline = tokio::time::Instant::now() + RECOVERY_DEADLINE;
            loop {
                match session.queued_turn().run().await? {
                    None if tokio::time::Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    None => {
                        anyhow::bail!(
                            "mid-follow worker did not acquire the expired session lease within \
                             {RECOVERY_DEADLINE:?}"
                        );
                    }
                    Some(_) => {
                        anyhow::bail!(
                            "mid-follow reached a terminal result before the effect-loop crash probe"
                        );
                    }
                }
            }
        }
        "recover" => {
            let deadline = tokio::time::Instant::now() + RECOVERY_DEADLINE;
            let recovered = loop {
                match session.queued_turn().run().await? {
                    Some(recovered) => break recovered,
                    None if tokio::time::Instant::now() < deadline => {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    None => {
                        anyhow::bail!(
                            "recovery worker did not acquire the expired session lease within \
                             {RECOVERY_DEADLINE:?}"
                        );
                    }
                }
            };
            let value = recovered
                .final_value()
                .cloned()
                .context("recovered follow-on produced no final value")?;
            let queue_empty = session.queued_work().await?.is_empty();
            let inputs_empty = session.pending_turn_inputs().await?.is_empty();
            println!(
                "{}",
                json!({
                    "final": EXPECTED_FRAME_SWITCH_TEXT,
                    "seed_visible": value.get("seed_visible").cloned().unwrap_or_default(),
                    "follow_on": value.get("follow_on").cloned().unwrap_or_default(),
                    "recovered_after_commit_exit": true,
                    "mid_follow_on_recovered": true,
                    "queue_empty": queue_empty,
                    "inputs_empty": inputs_empty,
                })
            );
            Ok(())
        }
        other => anyhow::bail!("unknown frame-crash mode `{other}`"),
    }
}
