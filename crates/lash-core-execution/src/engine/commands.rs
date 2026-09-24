//! Commands, and the executor registries that run them (ADR 0105 §10).
//!
//! Both registries resolve a body from serialized input alone and run it
//! inside the engine's recorded body. Neither ever runs in drive code. `Send`
//! sits on the executors, not on the drive: executor bodies are the execution
//! side, so an engine whose drive is `!Send` is still satisfied.

use std::sync::Arc;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};

use super::admission::{
    AdmitVerdict, Admitted, DriveFence, DriveRequestId, InheritVerdict, InheritedAuthority,
    SealVerdict,
};
use super::context::DriveObservation;
use crate::store::OperationId;
use crate::{
    CancellationToken, RuntimeEffectControllerError, RuntimeEffectEnvelope, RuntimeEffectOutcome,
    SessionId, TurnId,
};

/// A registered, serializable unit of I/O: today's envelope, unchanged.
pub type EffectCommand = RuntimeEffectEnvelope;

/// A recorded step's result. Domain failures ride the error arm; engine
/// failures never do.
pub type EffectResult = Result<RuntimeEffectOutcome, RuntimeEffectControllerError>;

/// The commands that exist before a fence exists.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdmissionCommand {
    AdmitDrive {
        session: SessionId,
        request: DriveRequestId,
        root: TurnId,
    },
    /// Carries the whole admission, not only its nonce: the seal advances the
    /// epoch admission observed.
    SealDriveAdmission {
        admitted: Admitted,
    },
    ValidateInherited {
        authority: InheritedAuthority,
    },
}

/// An admission command's recorded result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AdmissionResult {
    Admit { verdict: AdmitVerdict },
    Seal { verdict: SealVerdict },
    Inherit { verdict: InheritVerdict },
}

/// Runs admission commands inside the engine's recorded body.
pub trait AdmissionExecutors: Send + Sync + 'static {
    fn execute(
        &self,
        cmd: AdmissionCommand,
        cx: AdmissionStepContext,
    ) -> BoxFuture<'static, AdmissionResult>;
}

/// Runs effect commands inside the engine's recorded body.
pub trait EffectExecutors: Send + Sync + 'static {
    fn execute(&self, cmd: EffectCommand, cx: StepContext) -> BoxFuture<'static, EffectResult>;
}

/// Everything an effect step body may use that is not in its command.
pub struct StepContext {
    /// Checked transactionally by every store-writing step (ADR 0105 §9).
    pub fence: DriveFence,
    /// The stable external idempotency identity: the replay key, plus the
    /// attempt for a tool attempt. An epoch check cannot retract a request
    /// already sent; this identity is what makes a retried external operation
    /// safe.
    pub operation: OperationId,
    /// The engine's attempt number, when the engine exposes one.
    pub attempt: Option<u32>,
    pub heartbeat: Arc<dyn Heartbeat>,
    pub observe: Arc<dyn ObservationSink>,
    pub cancel: CooperativeCancel,
    /// Rehydrated per session id; no live-opener lookup on the durable path.
    pub services: Arc<SessionServices>,
}

/// Everything an admission step body may use that is not in its command.
/// It carries no fence: admission is what creates one.
pub struct AdmissionStepContext {
    pub operation: OperationId,
    pub attempt: Option<u32>,
    pub heartbeat: Arc<dyn Heartbeat>,
    pub services: Arc<SessionServices>,
}

/// Progress reporting from a running step to its engine.
pub trait Heartbeat: Send + Sync {
    fn beat(&self);
}

/// Where a step body publishes observations.
pub trait ObservationSink: Send + Sync {
    fn observe(&self, observation: DriveObservation);
}

/// An engine-delivered cooperative stop for a running step body.
#[derive(Clone, Debug, Default)]
pub struct CooperativeCancel {
    token: CancellationToken,
}

impl CooperativeCancel {
    pub fn new(token: CancellationToken) -> Self {
        Self { token }
    }

    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Resolves once the engine requests the stop.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }
}

/// The session services a step body is handed, rehydrated from the session
/// id alone on any worker. Its members are defined by the slice that
/// introduces the registered executors (ADR 0105 §10, slice P10a); until then no
/// value of it can be built, so no step context exists.
#[derive(Debug)]
pub struct SessionServices {
    _unbuilt: (),
}
