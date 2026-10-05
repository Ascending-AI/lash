//! Live operator-floor, source-registry and park controls for S13/S21/S22.
//! Typed refusals are answers, never process failures, so a case can assert
//! the exact variant a real deployment returned.
use std::sync::Arc;

use anyhow::{Result, bail};
use lash::StoreSet as _;
use lash_core::ClockWallTime as _;
use lash_core::engine::BuildGeneration;
use lash_core::store::generation_drain::GenerationDrainStatus;
use lash_core::tool_run::{SealWriter, SegmentOrdinal, SourceSeal};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::retirement::{self, RetirementRefusal};
use super::{H3Args, SourceFixture};

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceOp {
    Describe,
    Arm,
    Retain {
        value: String,
    },
    /// An external completion: retain a Done capture under the source and
    /// seal it, the canonical result a Deferred Run drains.
    Complete {
        output: serde_json::Value,
    },
    Seal {
        writer: SealWriter,
        seal: SourceSeal,
    },
    Subscribe {
        owner: lash_core::EffectOpener,
        segment: SegmentOrdinal,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "drain", rename_all = "snake_case", deny_unknown_fields)]
pub enum DrainOp {
    Mark,
    Clear,
    Status,
    Retire { deployment: String },
    Finalize,
}

pub(super) async fn source(fixture: &SourceFixture, op: SourceOp) -> Result<Value> {
    Ok(match op {
        SourceOp::Describe => json!({"descriptor": fixture.descriptor}),
        SourceOp::Arm => json!({"reply": fixture.arm().await?}),
        SourceOp::Retain { value } => json!({"seal": fixture.retained(&value).await?}),
        SourceOp::Complete { output } => {
            let capture = lash_core::tool_dispatch::SingletonCapture::Done {
                output: serde_json::to_string(&output)?,
                commands: Vec::new(),
                intents: Vec::new(),
                stream: Default::default(),
                start: None,
            };
            let seal = match fixture.retained(&serde_json::to_string(&capture)?).await {
                Ok(seal) => seal,
                // An ended holder refuses the late result before any seal.
                Err(error) => {
                    match error.downcast_ref::<lash_core_store::tool_run::MaterialRetentionError>() {
                        Some(
                            refusal @ lash_core_store::tool_run::MaterialRetentionError::HolderEnded {
                                ..
                            },
                        ) => {
                            return Ok(json!({"retention_refused": {
                                "kind": variant(refusal),
                                "message": refusal.to_string(),
                            }}));
                        }
                        _ => return Err(error),
                    }
                }
            };
            let reply = fixture.seal(SealWriter::External, seal.clone()).await?;
            json!({"seal": seal, "reply": reply})
        }
        SourceOp::Seal { writer, seal } => json!({"reply": fixture.seal(writer, seal).await?}),
        SourceOp::Subscribe { owner, segment } => {
            json!({"reply": fixture.subscribe_sealed(owner, segment).await?})
        }
    })
}

/// The variant name of a typed error, as its `Debug` form starts.
fn variant(value: &impl std::fmt::Debug) -> String {
    let text = format!("{value:?}");
    text.split(|c: char| !c.is_alphanumeric() && c != '_')
        .next()
        .unwrap_or_default()
        .to_owned()
}

fn store_error(error: &lash_core::StoreError) -> Value {
    match error {
        lash_core::StoreError::StorageFailure { backend, message } => {
            json!({"kind": "storage_failure", "backend": backend, "message": message})
        }
        other => json!({"kind": variant(other), "message": other.to_string()}),
    }
}

fn status(status: &GenerationDrainStatus) -> Value {
    json!({
        "generation": status.generation,
        "drained": status.drained(),
        "draining_since_ms": status.draining_since_ms,
        "live_processes": status.live_processes,
        "parked_processes": status.parked_processes,
        "parked_turns": status.parked_turns,
        "in_flight_turns": status.in_flight_turns,
        "closing_sessions": status.closing_sessions,
        "unfinished_invocations": status.unfinished_invocations,
    })
}

pub(super) async fn drain(
    args: &H3Args,
    generation: &BuildGeneration,
    op: DrainOp,
    severed_admin_url: Option<String>,
) -> Result<Value> {
    let super::super::StoreSpec::Sqlite(dir) = &args.store.store else {
        bail!("H3 drain controls require a SQLite store");
    };
    let stores = Arc::new(super::super::open_sqlite(dir).await?);
    let now = lash_core::facade_support::SystemClock.timestamp_ms();
    let admin_url = severed_admin_url.unwrap_or_else(|| args.restate.admin_url.clone());
    let registry = lash_restate::RestateDeploymentRegistry::new(
        lash_restate::RestateAdminClient::new(admin_url.clone()),
    );
    let drain = stores.generation_drain();
    Ok(match op {
        DrainOp::Mark => {
            drain.mark_draining(generation, now).await?;
            json!({"generation": generation, "marked": true})
        }
        DrainOp::Clear => {
            json!({"generation": generation, "cleared": drain.clear_draining(generation).await?})
        }
        DrainOp::Status => match GenerationDrainStatus::collect(
            drain.as_ref(),
            stores.session_delete_ledger().as_ref(),
            |kind| stores.obligation_ledger(kind),
            &registry,
            generation,
            now,
        )
        .await
        {
            Ok(read) => json!({"status": status(&read)}),
            Err(error) => json!({"error": store_error(&error)}),
        },
        DrainOp::Retire { deployment } => {
            let restate = super::super::RestateArgs {
                admin_url: admin_url.clone(),
                ..args.restate.clone()
            };
            let set: Arc<dyn lash::StoreSet> = stores.clone();
            let engine = super::super::engine(set, &restate)?;
            let core = super::super::core(
                lash::Backend::new(engine),
                &super::super::ProviderArgs::default(),
            )?;
            let view = crate::restate_view::RestateView::new(&admin_url, &args.restate.namespace)?;
            match retirement::retire_live(&core, &view, generation, &deployment).await? {
                Ok(read) => json!({"retired": status(&read)}),
                Err(RetirementRefusal::OwnedWork { status: read }) => {
                    json!({"refused": "owned_work", "status": status(&read)})
                }
                Err(RetirementRefusal::DrainReadFailed { cause }) => {
                    let cause = match cause.as_ref() {
                        lash::EmbedError::Store(error) => store_error(error),
                        other => json!({"kind": variant(other), "message": other.to_string()}),
                    };
                    json!({"refused": "drain_read_failed", "cause": cause})
                }
                Err(RetirementRefusal::DeploymentMismatch { deployment }) => {
                    json!({"refused": "deployment_mismatch", "deployment": deployment})
                }
            }
        }
        DrainOp::Finalize => match stores.finalize(generation, &registry, &[], now).await {
            Ok(flip) => json!({"finalized": flip}),
            Err(lash_core_store::store::fleet_finalize::FinalizeError::Refused(refusal)) => {
                json!({"refused": variant(&refusal), "detail": refusal.to_string()})
            }
            Err(error) => json!({"error": variant(&error), "detail": error.to_string()}),
        },
    })
}

pub(super) async fn park_verb(
    core: &lash::LashCore,
    session_id: lash::SessionId,
    run: lash_core::TurnId,
    park: lash_core::store::ParkId,
    redrive: bool,
) -> Result<Value> {
    let target = lash::ParkedWorkRef::Turn {
        session_id,
        turn_id: run.clone(),
    };
    let verbs = core.parked_work();
    let answer = if redrive {
        verbs.redrive(&target, park).await.map(|accepted| match accepted {
            lash::RedriveAccepted::Run(accepted) => {
                json!({"redrive": {"intent": accepted.intent, "applied": accepted.applied, "run": accepted.run}})
            }
            lash::RedriveAccepted::Process { process, park } => {
                json!({"redrive": {"process": process, "park": park}})
            }
        })
    } else {
        verbs.cancel(&target, park).await.map(|cancelled| {
            json!({"cancel": {"intent": cancelled.intent, "applied": cancelled.applied, "terminal": cancelled.terminal}})
        })
    };
    Ok(match answer {
        Ok(value) => value,
        Err(refusal) => json!({"refused": variant(&refusal), "detail": refusal.to_string()}),
    })
}
