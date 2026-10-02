//! What one build keeps and delivers of the durable rows around a session:
//! module artifacts held by host pins, the artifact-cleanup ledger, and
//! the attachment store, as `retention_delivery_rollback` (ADR 0115 §6)
//! counts them.
//!
//! - `publish` links one of the harness modules and publishes it under a
//!   host pin; `release` ends the pin, arming its cleanup and its fence,
//!   exactly as a host's `HostArtifacts::release` does.
//! - `orphan` puts attachment bytes no session roots, so a GC pass has
//!   something it must delete.
//! - `relay` runs one due pass of the artifact-cleanup relay and reports
//!   what it claimed, delivered and stalled.
//! - `maintain` runs the retention sweep, the attachment GC and each named
//!   session's vacuum and GC, and reports what each reclaimed.
//! - `inspect` reports what this build reads: each named module, the
//!   cleanup ledger's stalled rows, and every attachment the store holds.
//! - `pin` pins a session's head revision, `revisions` lists what the
//!   session retains, and `fork` forks one retained revision and reports
//!   the head the new session reads back (FIG-4731).
//!
//! No command serves. The leg runs the rollback's steps with no deployment
//! up, so the only relay that delivers is the one it asks, and it counts
//! every delivery.

use lash_vm_client::service::runtime_ops::ServiceRuntimeOps as _;
use std::num::NonZeroUsize;
use std::sync::Arc;

use anyhow::{Context, Result, anyhow};
use clap::{Args, Subcommand, ValueEnum};
use lash_core::runtime::artifact_cleanup::{
    ArtifactCleanupPorts, ArtifactCleanupRelay, StoreSetAuthorities,
};
use lash_core::runtime::shift::relay::relay_due;
use lash_core::store::{ArtifactCleanupLedger, ObligationKind};
use serde::{Deserialize, Serialize};

use super::{RestateArgs, StoreArgs, engine, open_stores};
use crate::identity::BuildLabel;

/// The largest page one relay pass claims.
const RELAY_PAGE: usize = 64;

/// The harness modules a leg publishes: each links to its own module
/// reference, the same on both builds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HarnessModule {
    /// Held by two pins, so one pin's cleanup leaves it stored.
    Shared,
    /// Held by one pin, so that pin's cleanup reclaims it.
    Sole,
}

impl HarnessModule {
    fn source(self) -> &'static str {
        match self {
            Self::Shared => {
                r#"
const worker = async () => {
  return "shared";
};
finish(null);
"#
            }
            Self::Sole => {
                r#"
const worker = async () => {
  return "sole";
};
finish(null);
"#
            }
        }
    }

    async fn link(self) -> Result<lash_vm_client::InspectedArtifact> {
        let environment = lashlang::LashlangHostEnvironment::new(
            lashlang::LashlangHostCatalog::new(),
            lashlang::LashlangAbilities::all(),
        );
        match lash_vm_client::service::Service::default()
            .request_accounted(lash_vm_client::service::Request::CompileModule {
                source: self.source().into(),
                environment,
                cell: false,
            })
            .await
            .map_err(|error| anyhow!("link the {self:?} module: {error}"))?
        {
            lash_vm_client::service::Response::Module(module) => Ok(module.artifact),
            response => Err(anyhow!("link the {self:?} module: {response:?}")),
        }
    }
}

#[derive(Clone, Debug, Args)]
pub struct RetentionArgs {
    #[command(flatten)]
    pub store: StoreArgs,
    #[command(flatten)]
    pub restate: RestateArgs,
    #[command(subcommand)]
    pub step: RetentionStep,
}

#[derive(Clone, Debug, Subcommand)]
pub enum RetentionStep {
    /// Publish a harness module under a host pin.
    Publish {
        #[arg(long)]
        pin: String,
        #[arg(long, value_enum)]
        module: HarnessModule,
    },
    /// End a host pin: its `Ended` cleanup and its fence.
    Release {
        #[arg(long)]
        pin: String,
    },
    /// Put attachment bytes that no session roots.
    Orphan {
        #[arg(long)]
        text: String,
    },
    /// One due pass of the artifact-cleanup relay.
    Relay,
    /// The retention sweep, the attachment GC, and each session's vacuum
    /// and GC.
    Maintain {
        #[arg(long)]
        session: Vec<String>,
    },
    /// What this build reads of each module, of the cleanup ledger and of
    /// the attachment store.
    Inspect {
        #[arg(long, value_enum)]
        module: Vec<HarnessModule>,
    },
    /// Pin the session's head revision (FIG-4731).
    Pin {
        #[arg(long)]
        session: String,
    },
    /// The revisions the session retains.
    Revisions {
        #[arg(long)]
        session: String,
    },
    /// Fork a retained revision of the session into a new session.
    Fork {
        #[arg(long)]
        session: String,
        #[arg(long)]
        revision: u64,
        #[arg(long)]
        branch: String,
    },
}

/// One revision a session retains.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RetainedRow {
    pub revision: u64,
    pub leaf: Option<String>,
    pub checkpoint: Option<String>,
    pub head: bool,
    pub pinned: bool,
}

/// What `pin` and `revisions` report: every revision the session retains,
/// oldest first.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retained {
    pub build: BuildLabel,
    pub revisions: Vec<RetainedRow>,
}

/// What `fork` reports: the head the new session reads back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Forked {
    pub build: BuildLabel,
    pub leaf: Option<String>,
    pub checkpoint: Option<String>,
}

/// The revisions `session_id` retains, as this build reads them.
async fn retained(
    factory: &dyn lash_core::DeploymentStore,
    build: BuildLabel,
    session_id: &lash::SessionId,
) -> Result<Retained> {
    let revisions = factory
        .revisions(session_id)
        .await
        .with_context(|| format!("list the revisions of {session_id}"))?
        .into_iter()
        .map(|revision| RetainedRow {
            revision: revision.head_revision,
            leaf: revision.leaf_node_id.map(|leaf| leaf.to_string()),
            checkpoint: revision.checkpoint_ref.map(|blob| blob.as_str().to_owned()),
            head: revision.head,
            pinned: !revision.pinned_by.is_empty(),
        })
        .collect();
    Ok(Retained { build, revisions })
}

/// What `publish` reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Published {
    pub build: BuildLabel,
    pub module_ref: String,
}

/// What `release` reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Released {
    pub build: BuildLabel,
    pub obligation_id: String,
}

/// What `orphan` reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Orphaned {
    pub build: BuildLabel,
    pub attachment_id: String,
}

/// What `relay` reports: one due pass of the artifact-cleanup relay.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelayReport {
    pub build: BuildLabel,
    pub claimed: usize,
    pub delivered: usize,
    pub retried: usize,
    pub stalled: usize,
    pub claim_lost: usize,
}

/// What `maintain` reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MaintainReport {
    pub build: BuildLabel,
    /// The retention sweep's reclaimed rows, all kinds together.
    pub retention_reclaimed: usize,
    /// Attachment blobs the GC looked at and deleted.
    pub attachments_scanned: usize,
    pub attachments_reclaimed: usize,
    /// Each named session's vacuum and GC reclaimed rows, all kinds together.
    pub sessions_reclaimed: Vec<usize>,
}

/// One module as this build reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleRead {
    pub module: HarnessModule,
    pub module_ref: String,
    /// `Some(true)` once decoded and verified; `None` when no referrer holds
    /// it any more.
    pub verified: Option<bool>,
    /// The read's error, when it failed.
    pub error: Option<String>,
}

/// One stalled cleanup obligation as this build reads it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StalledRow {
    pub obligation_id: String,
    pub reason: String,
    /// The referrer kind the key names, when it decodes.
    pub referrer_kind: Option<String>,
    /// Why the key does not decode, when it does not.
    pub undecodable: Option<String>,
}

/// What `inspect` reports.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectReport {
    pub build: BuildLabel,
    pub modules: Vec<ModuleRead>,
    pub stalled: Vec<StalledRow>,
    /// Every attachment id the store holds, sorted.
    pub attachments: Vec<String>,
}

fn pin(text: &str) -> Result<lash_core::HostArtifactPin> {
    lash_core::HostArtifactPin::try_from(text.to_owned())
        .map_err(|error| anyhow!("host pin `{text}`: {error}"))
}

fn page() -> NonZeroUsize {
    NonZeroUsize::new(RELAY_PAGE).unwrap_or(NonZeroUsize::MIN)
}

/// Run one retention step and print its report.
pub async fn run(args: RetentionArgs) -> Result<()> {
    let build = BuildLabel::current();
    let stores = open_stores(&args.store).await?;
    match args.step {
        RetentionStep::Publish { pin: text, module } => {
            let artifact = module.link().await?;
            let claim = lash_core::ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                pin(&text)?,
            ))
            .map_err(|error| anyhow!("the module's referrer claim: {error}"))?;
            lashlang::LashlangArtifacts::new(stores.module_artifacts())
                .publish_module_artifact(&claim, &artifact)
                .await
                .map_err(|error| anyhow!("publish the {module:?} module: {error}"))?;
            super::print(&Published {
                build,
                module_ref: artifact.module_ref().to_string(),
            })
        }
        RetentionStep::Release { pin: text } => {
            let cleanup = lash_core::ArtifactCleanup::ended(
                lash_core::ArtifactReferrer::HostPin(pin(&text)?),
                Vec::new(),
                None,
            );
            let id = stores
                .artifact_cleanup()
                .arm_cleanup(&cleanup, stores.clock().timestamp_ms())
                .await
                .context("release the pin")?;
            super::print(&Released {
                build,
                obligation_id: id.as_str().to_owned(),
            })
        }
        RetentionStep::Orphan { text } => {
            let media_type = lash_core::MediaType::parse("text/plain")
                .map_err(|error| anyhow!("media type: {error}"))?;
            let stored = stores
                .attachment_store()
                .put(
                    text.into_bytes(),
                    lash_core::AttachmentCreateMeta::new(media_type, None, None),
                )
                .await
                .context("put the orphan attachment")?;
            super::print(&Orphaned {
                build,
                attachment_id: stored.id.to_string(),
            })
        }
        RetentionStep::Relay => {
            let backend = lash::Backend::new(engine(Arc::clone(&stores), &args.restate)?);
            let relay = ArtifactCleanupRelay::new(ArtifactCleanupPorts {
                ledger: stores.artifact_cleanup(),
                authorities: Arc::new(StoreSetAuthorities {
                    effect_host: backend.effect_host(),
                    sessions: stores.session_store_factory(),
                    processes: stores.process_registry(),
                    triggers: stores.trigger_store(),
                }),
                process_env: stores.process_env_store(),
                modules: stores.module_artifacts(),
                definitions: stores.definition_store(),
                engines: lash_core::ProcessEngineRegistry::new(),
                attachments: stores.attachment_referrers(),
                clock: stores.clock(),
            });
            let pass = relay_due(&relay, stores.clock().as_ref(), page())
                .await
                .context("relay the due cleanups")?;
            super::print(&RelayReport {
                build,
                claimed: pass.claimed,
                delivered: pass.delivered,
                retried: pass.retried,
                stalled: pass.stalled,
                claim_lost: pass.claim_lost,
            })
        }
        RetentionStep::Maintain { session } => {
            use lash_core::store::MaintenanceReport as _;
            let factory = stores.session_store_factory();
            let retention = factory
                .reclaim_retained_evidence(lash_core::store::RetentionBound {
                    committed_before_epoch_ms: stores.clock().timestamp_ms(),
                })
                .await
                .map_err(|failure| anyhow!("the retention sweep: {failure:?}"))?;
            let attachments = lash_core::facade_support::reclaim_unreferenced_attachments(
                factory.as_ref(),
                stores.attachment_store().as_ref(),
                lash_core::AttachmentReclamationPolicy {
                    grace_period_ms: 0,
                    empty_root_set: lash_core::EmptyRootSetPolicy::Refuse,
                },
            )
            .await
            .map_err(|failure| anyhow!("the attachment GC: {failure:?}"))?;
            let mut sessions_reclaimed = Vec::with_capacity(session.len());
            for id in session {
                let session_id = lash::SessionId::fixture(id.clone());
                if !matches!(
                    factory.lookup_session(&session_id).await?,
                    lash::persistence::SessionLookup::Live(_)
                ) {
                    anyhow::bail!("session {id} is not stored");
                }
                let vacuum = factory
                    .vacuum(&session_id)
                    .await
                    .map_err(|failure| anyhow!("vacuum {id}: {failure:?}"))?;
                let gc = factory
                    .gc_unreachable()
                    .await
                    .map_err(|failure| anyhow!("GC {id}: {failure:?}"))?;
                sessions_reclaimed.push(vacuum.reclaimed_count() + gc.reclaimed_count());
            }
            super::print(&MaintainReport {
                build,
                retention_reclaimed: retention.reclaimed_count(),
                attachments_scanned: attachments.scanned_blob_count,
                attachments_reclaimed: attachments.reclaimed_count,
                sessions_reclaimed,
            })
        }
        RetentionStep::Pin { session } => {
            let factory = stores.session_store_factory();
            let session_id = lash::SessionId::fixture(session);
            let head = retained(factory.as_ref(), build, &session_id)
                .await?
                .revisions
                .pop()
                .ok_or_else(|| anyhow!("session {session_id} retains no revision"))?;
            factory
                .pin(&session_id, &lash_core::Target::Revision(head.revision))
                .await
                .context("pin the head revision")?;
            super::print(&retained(factory.as_ref(), build, &session_id).await?)
        }
        RetentionStep::Revisions { session } => {
            let factory = stores.session_store_factory();
            super::print(
                &retained(factory.as_ref(), build, &lash::SessionId::fixture(session)).await?,
            )
        }
        RetentionStep::Fork {
            session,
            revision,
            branch,
        } => {
            let factory = stores.session_store_factory();
            let source = lash::SessionId::fixture(session);
            let branch = lash::SessionId::fixture(branch);
            let resolved = factory
                .resolve_target(&source, &lash_core::Target::Revision(revision))
                .await
                .with_context(|| format!("resolve revision {revision} of {source}"))?;
            let config = resolved.fork_config();
            factory
                .fork_session(&lash_core::ForkSessionRequest {
                    session_id: branch.clone(),
                    source_session_id: source.clone(),
                    head_revision: revision,
                    relation: lash_core::SessionRelation::Fork {
                        source_session_id: source,
                        source_node_id: resolved.leaf_node_id,
                    },
                    pending_observer_intents: Vec::new(),
                    config,
                })
                .await
                .with_context(|| format!("fork revision {revision} into {branch}"))?;
            let head =
                lash_core::SessionCommitStore::load_session_head_meta(factory.as_ref(), &branch)
                    .await?
                    .ok_or_else(|| anyhow!("the fork {branch} has no head"))?;
            super::print(&Forked {
                build,
                leaf: head.leaf_node_id.map(|leaf| leaf.to_string()),
                checkpoint: head.checkpoint_ref.map(|blob| blob.as_str().to_owned()),
            })
        }
        RetentionStep::Inspect { module } => {
            let artifacts = lashlang::LashlangArtifacts::new(stores.module_artifacts());
            let mut modules = Vec::with_capacity(module.len());
            for module in module {
                let artifact = module.link().await?;
                let module_ref = artifact.module_ref().clone();
                let (verified, error) = match lash_vm_client::service::Service::default()
                    .inspect_artifact(&artifacts, &module_ref)
                    .await
                {
                    Ok(Some(read)) => (Some(read == artifact), None),
                    Ok(None) => (None, None),
                    Err(error) => (Some(false), Some(error.to_string())),
                };
                modules.push(ModuleRead {
                    module,
                    module_ref: module_ref.to_string(),
                    verified,
                    error,
                });
            }
            let ledger: Arc<dyn ArtifactCleanupLedger> = stores.artifact_cleanup();
            debug_assert_eq!(ledger.kind(), ObligationKind::ArtifactCleanup);
            let stalled = ledger
                .list_stalled(None, page())
                .await
                .context("list the stalled cleanups")?
                .into_iter()
                .map(|row| {
                    let (referrer_kind, undecodable) = match &row.key {
                        Ok(lash_core::store::ObligationKey::ArtifactCleanup { referrer }) => {
                            (Some(referrer.kind().as_str().to_owned()), None)
                        }
                        Ok(other) => (Some(format!("{other:?}")), None),
                        Err(error) => (None, Some(error.detail.clone())),
                    };
                    StalledRow {
                        obligation_id: row.id.as_str().to_owned(),
                        reason: row.reason.as_str().to_owned(),
                        referrer_kind,
                        undecodable,
                    }
                })
                .collect();
            let mut attachments: Vec<String> = stores
                .attachment_store()
                .list()
                .await
                .context("list the attachments")?
                .into_iter()
                .map(|blob| blob.id.to_string())
                .collect();
            attachments.sort();
            super::print(&InspectReport {
                build,
                modules,
                stalled,
                attachments,
            })
        }
    }
}
