//! What the Phase A legs share: the two builds and the live services, a
//! scratch directory for each leg's evidence, and the retained source registry
//! the Restate legs use.

use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use lash_upgrade_harness::harness::{Case, NodeBuilds, Services, block_on, wait_for};
use lash_upgrade_harness::identity::BuildLabel;
use lash_upgrade_harness::node::objects::{CallOutcome, CallReport, HandlerRefusal};

/// Where a run keeps each leg's evidence: `just phase-a` points it at its
/// artifact directory. A bare run uses a temporary directory.
pub const ARTIFACT_DIR_ENV: &str = "LASH_PHASE_A_ARTIFACT_DIR";

/// The surviving source registry. Its object family participates in the
/// synthetic-next format sweep independently of aggregate transport.
pub const WAIT_INDEX: &str = "LashDurableWaitIndex";

/// One leg's builds, services and scratch directory.
pub struct Leg {
    pub services: Services,
    pub builds: NodeBuilds,
    pub scratch: PathBuf,
    _temporary: tempfile::TempDir,
}

impl Leg {
    /// The builds and services `just phase-a` exports, and a scratch
    /// directory named for the leg.
    pub fn start(name: &str) -> Result<Self> {
        let services = Services::from_env()?;
        let builds = NodeBuilds::from_env()?;
        ensure!(
            builds.n.label() == BuildLabel::N && builds.next.label() == BuildLabel::Next,
            "the builds are labelled {} and {}",
            builds.n.label(),
            builds.next.label()
        );
        let temporary = tempfile::tempdir()?;
        let scratch = std::env::var_os(ARTIFACT_DIR_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| temporary.path().to_path_buf())
            .join(name);
        std::fs::create_dir_all(&scratch)
            .with_context(|| format!("create {}", scratch.display()))?;
        Ok(Self {
            services,
            builds,
            scratch,
            _temporary: temporary,
        })
    }
}

/// The reply a call was answered with: its wire and its body.
pub fn replied(report: &CallReport) -> Result<(u32, &serde_json::Value)> {
    match &report.outcome {
        CallOutcome::Replied { wire, body } => Ok((*wire, body)),
        other => bail!("{} expected a reply, got {other:?}", report.caller),
    }
}

/// The typed refusal a call was answered with.
pub fn refused(report: &CallReport) -> Result<&HandlerRefusal> {
    match &report.outcome {
        CallOutcome::Refused { refusal, .. } => Ok(refusal),
        other => bail!("{} expected a typed refusal, got {other:?}", report.caller),
    }
}

/// Write a leg's report beside its evidence.
pub fn record(leg: &Leg, name: &str, report: &impl serde::Serialize) -> Result<()> {
    let path = leg.scratch.join(name);
    std::fs::write(&path, serde_json::to_vec_pretty(report)?)
        .with_context(|| format!("write {}", path.display()))
}

/// Wait until no invocation on `session` is still owed: a turn answers
/// before its shift completes, and a shift left on a node that stops stays
/// pinned to the node's dead deployment, ahead of every later turn.
pub fn quiesce(case: &Case, session: &str) -> Result<()> {
    let view = case.view()?;
    wait_for(&format!("{session}'s shift to complete"), || {
        let live = block_on(view.live_invocations("LashSession", session))?;
        Ok(live.is_empty().then_some(()))
    })
}
