//! K5: a declared start is one recoverable obligation (FIG-4884 and
//! FIG-4885 implement it; the tool-run contract's other seams are in
//! `lash_core_store::tool_run`).
//!
//! A call that starts a process records one obligation before the start
//! can launch: a stable [`StartKey`], the registration that fixes the
//! process binding, lifetime and environment, and the consumer hold that
//! keeps the child from pruning while the call may redrive. Only a process
//! lash executes runs under a captured environment; an externally owned one
//! has none. A cancel before
//! admission forbids the start; a cancel after admission recovers that
//! same start under its key and discharges its cancel policy and hold. An
//! isolated call is such a start from the beginning, with no inline body.

use lash_sansio::ToolCallId;
use serde::{Deserialize, Serialize};

use super::model::{ProcessStartRegistration, StartKey};

/// A declared start, admitted with its call.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredStartObligation {
    pub call_id: ToolCallId,
    pub registration: ProcessStartRegistration,
}

/// Why a registration cannot be a declared start.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum DeclaredStartObligationRefusal {
    #[error("a declared start needs a stable start key")]
    Keyless,
    #[error("a declared start lash executes needs its captured execution environment")]
    NoEnvironment,
    #[error("a declared start needs the consuming call's hold")]
    NoConsumerHold,
}

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
    /// [`DeclaredStartObligationRefusal`] for a keyless start, a start lash
    /// executes without its environment, or one without its consumer hold.
    pub fn new(
        call_id: ToolCallId,
        registration: ProcessStartRegistration,
    ) -> Result<Self, DeclaredStartObligationRefusal> {
        if registration.start_key.is_none() {
            return Err(DeclaredStartObligationRefusal::Keyless);
        }
        if registration.env_ref.is_none() && !registration.input.is_externally_owned() {
            return Err(DeclaredStartObligationRefusal::NoEnvironment);
        }
        if registration.consumer_hold.is_none() {
            return Err(DeclaredStartObligationRefusal::NoConsumerHold);
        }
        Ok(Self {
            call_id,
            registration,
        })
    }

    /// The start's stable key.
    ///
    /// # Panics
    ///
    /// Never: [`Self::new`] refuses a keyless registration.
    #[must_use]
    #[expect(
        clippy::expect_used,
        reason = "construction refuses a keyless registration"
    )]
    pub fn start_key(&self) -> &StartKey {
        self.registration
            .start_key
            .as_ref()
            .expect("a declared start has a key")
    }

    /// What a cancel of the owning call at `phase` must do.
    #[must_use]
    pub fn on_cancel(&self, phase: DeclaredStartPhase) -> StartCancelDecision {
        match phase {
            DeclaredStartPhase::Declared => StartCancelDecision::ForbidStart,
            DeclaredStartPhase::Admitted | DeclaredStartPhase::Launched => {
                StartCancelDecision::RecoverAndDischarge {
                    start_key: self.start_key().clone(),
                    cancel_process: self
                        .registration
                        .consumer_hold
                        .as_ref()
                        .is_some_and(|hold| hold.cancels),
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
    use std::sync::Arc;

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
        let mut external = registration(false).with_input(Arc::new(
            ProcessInput::External {
                metadata: serde_json::Value::Null,
            }
            .into(),
        ));
        external.env_ref = None;
        assert!(
            DeclaredStartObligation::new(call_id.clone(), external).is_ok(),
            "an externally owned start runs under no environment"
        );
        let mut unheld = registration(false);
        unheld.consumer_hold = None;
        assert_eq!(
            DeclaredStartObligation::new(call_id.clone(), unheld).unwrap_err(),
            DeclaredStartObligationRefusal::NoConsumerHold
        );
        let obligation = DeclaredStartObligation::new(call_id, registration(false)).unwrap();
        let encoded = serde_json::to_value(&obligation).unwrap();
        let decoded: DeclaredStartObligation = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded.start_key(), &StartKey::for_host("declared-start"));
    }

    #[test]
    fn a_cancel_forbids_an_unadmitted_start_and_recovers_an_admitted_one() {
        let key = StartKey::for_host("declared-start");
        let obligation =
            DeclaredStartObligation::new(ToolCallId::fixture("start"), registration(true)).unwrap();
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
