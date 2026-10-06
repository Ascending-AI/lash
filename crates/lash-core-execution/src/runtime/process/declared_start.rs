//! K5: a declared start is one recoverable obligation (FIG-4884 and
//! FIG-4885 implement it; the tool-run contract's other seams are in
//! `lash_core_store::tool_run`).
//!
//! A call that starts a process records one obligation before the start
//! can launch: a stable [`StartKey`], the registration that fixes the
//! process binding, lifetime and environment, and the consumer hold that
//! keeps the child from pruning while the call may redrive. Every process
//! runs under a captured environment. A cancel before
//! admission forbids the start; a cancel after admission recovers that
//! same start under its key and discharges its cancel policy and hold. An
//! isolated call is such a start from the beginning, with no inline body.

use lash_sansio::ToolCallId;
use serde::{Deserialize, Serialize};

use super::model::{ConsumerHold, ProcessStartRegistration, StartKey};

/// The implementation and start an isolated call binds before execution.
#[derive(Clone, Debug)]
pub struct IsolatedToolStart {
    pub registration: ProcessStartRegistration,
}

pub use crate::tool_run::IsolatedStartRefusal;

/// A declared start, admitted with its call.
#[derive(Debug, Serialize, Deserialize)]
#[serde(try_from = "DeclaredStartObligationWire")]
pub struct DeclaredStartObligation {
    pub call_id: ToolCallId,
    pub registration: ProcessStartRegistration,
    // Keep validated facts off the stack of the Run's futures. The journal
    // retains its registration wire shape.
    #[serde(skip)]
    binding: Box<DeclaredStartBinding>,
}

#[derive(Debug)]
struct DeclaredStartBinding {
    start_key: StartKey,
    consumer_hold: ConsumerHold,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DeclaredStartObligationWire {
    call_id: ToolCallId,
    registration: ProcessStartRegistration,
}

impl TryFrom<DeclaredStartObligationWire> for DeclaredStartObligation {
    type Error = DeclaredStartObligationRefusal;

    fn try_from(wire: DeclaredStartObligationWire) -> Result<Self, Self::Error> {
        Self::new(wire.call_id, wire.registration)
    }
}

pub use crate::tool_run::DeclaredStartObligationRefusal;

/// How far a declared start has gone.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredStartPhase {
    /// Returned by the body; not yet admitted.
    Declared,
    /// Admitted: the start will happen under its key.
    Admitted,
    /// The process is registered and its start sent.
    Launched,
    /// Cancelled after admission, and the cancel policy and hold are
    /// discharged.
    Discharged,
}

/// What cancelling the owning call does to its start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StartCancelDecision {
    /// Not admitted: the start must never launch.
    ForbidStart,
    /// Admitted: recover the start under the same key, then cancel the
    /// process when the hold says the call owes it a cancel, and release
    /// the hold.
    RecoverAndDischarge {
        start_key: StartKey,
        cancel_process: bool,
    },
    /// Already discharged; nothing more is owed.
    Discharged,
}

impl DeclaredStartObligation {
    /// The obligation of `call_id` to start `registration`.
    ///
    /// # Errors
    ///
    /// [`DeclaredStartObligationRefusal`] for a keyless start, a start
    /// without its environment, or one without its consumer hold.
    pub fn new(
        call_id: ToolCallId,
        registration: ProcessStartRegistration,
    ) -> Result<Self, DeclaredStartObligationRefusal> {
        let start_key = registration
            .start_key
            .as_ref()
            .ok_or(DeclaredStartObligationRefusal::Keyless)?
            .clone();
        if registration.env_ref.is_none() {
            return Err(DeclaredStartObligationRefusal::NoEnvironment);
        }
        let consumer_hold = registration
            .consumer_hold
            .as_ref()
            .ok_or(DeclaredStartObligationRefusal::NoConsumerHold)?
            .clone();
        Ok(Self {
            call_id,
            registration,
            binding: Box::new(DeclaredStartBinding {
                start_key,
                consumer_hold,
            }),
        })
    }

    /// The start's stable key.
    #[must_use]
    pub fn start_key(&self) -> &StartKey {
        &self.binding.start_key
    }

    /// What a cancel of the owning call at `phase` must do.
    #[must_use]
    pub fn on_cancel(&self, phase: DeclaredStartPhase) -> StartCancelDecision {
        match phase {
            DeclaredStartPhase::Declared => StartCancelDecision::ForbidStart,
            DeclaredStartPhase::Admitted | DeclaredStartPhase::Launched => {
                StartCancelDecision::RecoverAndDischarge {
                    start_key: self.start_key().clone(),
                    cancel_process: self.binding.consumer_hold.cancels,
                }
            }
            DeclaredStartPhase::Discharged => StartCancelDecision::Discharged,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{
        ConsumerHold, Lifetime, ProcessExecutionEnvRef, ProcessInput, ProcessProvenance, ScopeId,
    };
    use super::*;

    fn registration(cancels: bool) -> ProcessStartRegistration {
        let mut registration = ProcessStartRegistration::of_target(
            ProcessInput::Engine {
                kind: "index".into(),
                payload: serde_json::json!({"job": "index"}),
            },
            ProcessProvenance::host(),
            Lifetime::Detached,
        )
        .with_start_key(Some(StartKey::for_host("declared-start")));
        registration.env_ref = Some(ProcessExecutionEnvRef::new("process-env:fixture"));
        registration.consumer_hold = Some(ConsumerHold {
            key: "completion-1".into(),
            owner: ScopeId::Session("session-1".into()),
            cancels,
        });
        registration
    }

    fn material_without(field: &str) -> serde_json::Value {
        let obligation =
            DeclaredStartObligation::new(ToolCallId::fixture("start"), registration(true)).unwrap();
        let mut material = serde_json::to_value(obligation).unwrap();
        material["registration"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        material
    }

    #[test]
    fn decoding_a_keyless_declared_start_refuses_before_replay_reads_its_key() {
        let error = match serde_json::from_value::<DeclaredStartObligation>(material_without(
            "start_key",
        )) {
            Ok(obligation) => panic!("decoded a keyless start: {:?}", obligation.start_key()),
            Err(error) => error,
        };
        assert_eq!(
            error.to_string(),
            DeclaredStartObligationRefusal::Keyless.to_string()
        );
    }

    #[test]
    fn decoding_a_declared_start_without_its_hold_refuses() {
        let error =
            serde_json::from_value::<DeclaredStartObligation>(material_without("consumer_hold"))
                .unwrap_err();
        assert_eq!(
            error.to_string(),
            DeclaredStartObligationRefusal::NoConsumerHold.to_string()
        );
    }

    #[test]
    fn decoding_a_lash_executed_declared_start_without_its_environment_refuses() {
        let error = serde_json::from_value::<DeclaredStartObligation>(material_without("env_ref"))
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            DeclaredStartObligationRefusal::NoEnvironment.to_string()
        );
    }

    #[test]
    fn a_declared_start_needs_its_key_environment_and_hold() {
        let call_id = ToolCallId::fixture("start");
        let mut keyless = registration(false);
        keyless.start_key = None;
        assert_eq!(
            DeclaredStartObligation::new(call_id.clone(), keyless).unwrap_err(),
            DeclaredStartObligationRefusal::Keyless
        );
        let mut no_env = registration(false);
        no_env.env_ref = None;
        assert_eq!(
            DeclaredStartObligation::new(call_id.clone(), no_env).unwrap_err(),
            DeclaredStartObligationRefusal::NoEnvironment
        );
        let mut unheld = registration(false);
        unheld.consumer_hold = None;
        assert_eq!(
            DeclaredStartObligation::new(call_id.clone(), unheld).unwrap_err(),
            DeclaredStartObligationRefusal::NoConsumerHold
        );
        let obligation = DeclaredStartObligation::new(call_id, registration(false)).unwrap();
        let encoded = serde_json::to_value(&obligation).unwrap();
        let decoded: DeclaredStartObligation = serde_json::from_value(encoded.clone()).unwrap();
        assert_eq!(decoded.start_key(), &StartKey::for_host("declared-start"));
        assert_eq!(serde_json::to_value(decoded).unwrap(), encoded);
    }

    #[test]
    fn a_cancel_forbids_an_unadmitted_start_and_recovers_an_admitted_one() {
        let key = StartKey::for_host("declared-start");
        let obligation =
            DeclaredStartObligation::new(ToolCallId::fixture("start"), registration(true)).unwrap();
        let obligation: DeclaredStartObligation =
            serde_json::from_value(serde_json::to_value(obligation).unwrap()).unwrap();
        assert_eq!(
            obligation.on_cancel(DeclaredStartPhase::Declared),
            StartCancelDecision::ForbidStart
        );
        for phase in [DeclaredStartPhase::Admitted, DeclaredStartPhase::Launched] {
            assert_eq!(
                obligation.on_cancel(phase),
                StartCancelDecision::RecoverAndDischarge {
                    start_key: key.clone(),
                    cancel_process: true,
                }
            );
        }
        assert_eq!(
            obligation.on_cancel(DeclaredStartPhase::Discharged),
            StartCancelDecision::Discharged
        );
        let ignoring =
            DeclaredStartObligation::new(ToolCallId::fixture("start"), registration(false))
                .unwrap();
        assert_eq!(
            ignoring.on_cancel(DeclaredStartPhase::Admitted),
            StartCancelDecision::RecoverAndDischarge {
                start_key: key,
                cancel_process: false,
            }
        );
    }
}
