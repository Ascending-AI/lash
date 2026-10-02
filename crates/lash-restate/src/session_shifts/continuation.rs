//! A continuation retains its intended lane as typed request data, so an
//! attach joins that invocation across build rolls and later handoffs.

use super::*;
use lash_core::engine::{SHIFT_CONTINUATION_PREFIX, shift_continuation_request};

/// Why a shift was neither sent nor attached to.
#[derive(Debug, thiserror::Error)]
pub enum SendShiftError {
    /// No core has bound the engine's generation yet, so the deployment
    /// cannot serve the shift.
    #[error(transparent)]
    GenerationUnbound(#[from] lash_core::engine::GenerationUnbound),
    /// Restate did not accept the send.
    #[error(transparent)]
    Http(#[from] crate::RestateHttpError),
}

impl SendShiftError {
    /// Whether the send or attach timed out in transit
    /// ([`RestateHttpError::is_timeout`](crate::RestateHttpError::is_timeout)).
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Http(error) if error.is_timeout())
    }
}

/// A send Restate refused keeps its cause's code and retry class. A
/// generation no core has bound is a deployment fact no retry changes.
impl crate::session_control::ControlFailure for SendShiftError {
    fn into_refusal(self) -> lash_core::engine::EngineRefusal {
        match self {
            Self::Http(error) => error.into_refusal(),
            Self::GenerationUnbound(error) => lash_core::engine::EngineRefusal::permanent(
                lash_core::RuntimeErrorCode::EngineServiceUnregistered,
                error.to_string(),
            ),
        }
    }
}

pub(super) fn session_shift_continuation(
    request: &ShiftRequest,
    route: &crate::services::ServiceRoute,
) -> ShiftRequest {
    ShiftRequest {
        session: request.session.clone(),
        request: shift_continuation_request(request),
        intended_lane: match route.lane() {
            crate::services::Lane::Stable => None,
            crate::services::Lane::Generation(generation) => Some(generation.clone()),
        },
    }
}

pub(super) fn continuation_generation(request: &ShiftRequest) -> Option<BuildGeneration> {
    request.intended_lane.clone()
}

/// Every admission after the first answers the pinned build's drain. A
/// continuation on a generation lane answers it at its first admission too;
/// a newly resumed shift runs its first run before handing over.
pub(super) fn drain_answered(
    route: &crate::services::ServiceRoute,
    request: &ShiftRequestId,
    ordinal: u32,
) -> bool {
    ordinal > 0
        || matches!(route.lane(), crate::services::Lane::Generation(_))
            && request.as_str().starts_with(SHIFT_CONTINUATION_PREFIX)
}

#[expect(
    clippy::result_large_err,
    reason = "the ingress client returns RestateHttpError unboxed across its public API"
)]
impl RestateSessionWork {
    /// Send `request`'s shift to `LashSession/{session}`, keyed by the
    /// request id: a repeated send of one request attaches to its first
    /// invocation instead of executing twice. A transient send failure retries
    /// under the same idempotency key before the ask is given up to the
    /// ingress relay. Resolves once Restate accepted the send, not once the
    /// shift ran.
    pub async fn send_shift(
        &self,
        session: &SessionId,
        request: ShiftRequestId,
    ) -> Result<crate::RestateInvocationId, SendShiftError> {
        self.generation.get()?;
        let body = RestateSessionShiftRequest {
            request: ShiftRequest {
                session: session.clone(),
                request: request.clone(),
                intended_lane: None,
            },
            handed_off: None,
        };
        Ok(self
            .ingress
            .send_object_json_idempotent_bounded(
                &self.namespace.stable(LashService::SessionShifts).name(),
                session.as_str(),
                SHIFT_HANDLER,
                &Call::new(body),
                request.as_str(),
            )
            .await?)
    }

    /// Attach to a shift under the stable name, sending it if necessary.
    pub async fn attach_shift(
        &self,
        session: &SessionId,
        request: ShiftRequestId,
    ) -> Result<ShiftOutcome, SendShiftError> {
        self.attach_drive_request(&ShiftRequest {
            session: session.clone(),
            request,
            intended_lane: None,
        })
        .await
    }

    /// Attach to the lane retained by a shift request. A continuation's
    /// recorded request retains this lane across build rolls.
    pub async fn attach_drive_request(
        &self,
        request: &ShiftRequest,
    ) -> Result<ShiftOutcome, SendShiftError> {
        self.generation.get()?;
        let route = match continuation_generation(request) {
            Some(generation) => self
                .namespace
                .generation(LashService::SessionShifts, generation),
            None => self.namespace.stable(LashService::SessionShifts),
        };
        let body = RestateSessionShiftRequest {
            request: request.clone(),
            handed_off: None,
        };
        Ok(self
            .ingress
            .call_object_json_idempotent::<_, Reply<ShiftOutcome>>(
                &route.name(),
                request.session.as_str(),
                SHIFT_HANDLER,
                &Call::new(body),
                request.request.as_str(),
            )
            .await
            .map(Reply::into_body)?)
    }

    /// Follow every leg on its retained lane until the whole shift ends.
    pub async fn await_drive_request(
        &self,
        request: &ShiftRequest,
    ) -> Result<ShiftOutcome, ShiftAbort> {
        let mut leg = request.clone();
        let mut ran = Vec::new();
        loop {
            let outcome = self.attach_shift_leg(&leg).await?;
            let next = self.continuation(&leg, &outcome);
            ran.extend(outcome.ran);
            match next {
                Some(next) => leg = next,
                None => {
                    return Ok(ShiftOutcome {
                        ran,
                        stop: outcome.stop,
                    });
                }
            }
        }
    }

    pub(super) fn continuation(
        &self,
        leg: &ShiftRequest,
        outcome: &ShiftOutcome,
    ) -> Option<ShiftRequest> {
        let route = match outcome.stop {
            ShiftStop::HandedOff { .. } => match continuation_generation(leg) {
                Some(generation) => self
                    .namespace
                    .generation(LashService::SessionShifts, generation),
                None => self.namespace.stable(LashService::SessionShifts),
            },
            ShiftStop::Draining { .. } => self.namespace.stable(LashService::SessionShifts),
            _ => return None,
        };
        Some(session_shift_continuation(leg, &route))
    }
}
