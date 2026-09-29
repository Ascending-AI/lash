//! What the Phase A legs share: the two builds and the live services, a
//! scratch directory for each leg's evidence, and the effect-group calls
//! the Restate legs make.

use std::path::PathBuf;

use anyhow::{Context, Result, bail, ensure};
use lash_upgrade_harness::harness::{NodeBuilds, Services};
use lash_upgrade_harness::identity::BuildLabel;
use lash_upgrade_harness::node::objects::{CallOutcome, CallReport, HandlerRefusal};
use lash_upgrade_harness::restate_view::RestateView;

/// Where a run keeps each leg's evidence: `just phase-a` points it at its
/// artifact directory. A bare run uses a temporary directory.
pub const ARTIFACT_DIR_ENV: &str = "LASH_PHASE_A_ARTIFACT_DIR";

/// The effect-group index: the object family whose format the synthetic
/// N+1 moves, and whose handlers the Restate legs call.
pub const GROUP: &str = "EffectGroupIndex";

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

/// The body of an `open` that creates a fresh one-child effect group: the
/// index records it as `Preparing`, and nothing is dispatched.
pub fn open_group_body(view: &RestateView, key: &str) -> Result<serde_json::Value> {
    let request = lash_restate::EffectGroupOpenRequest {
        shape: lash_restate::EffectGroupShape {
            wake: lash_core::GroupWakePolicy::All,
            loser_disposition: lash_core::LoserPolicy::RunToCompletion,
            replay_keys: vec![format!("{key}-child-0")],
            wait_scope: lash_core::ExecutionScope::runtime_operation(key),
            membership: vec!["{}".to_owned()],
            opener: lash_core::AdmittedScope::turn(format!("{key}-session"), "turn"),
        },
        dispatch_route: view.service_name("EffectGroupDispatch"),
        content_checked: false,
    };
    serde_json::to_value(&request).context("encode an open request")
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
