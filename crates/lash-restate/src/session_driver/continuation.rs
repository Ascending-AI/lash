//! A continuation retains the lane its recorded send names, so an attach
//! joins that invocation across build rolls and later handoffs.

use super::*;
use lash_core::engine::{DRIVE_CONTINUATION_PREFIX, drive_continuation_request};

/// Why a drive was neither sent nor attached to.
#[derive(Debug, thiserror::Error)]
pub enum SendDriveError {
    /// No core has bound the engine's generation yet, so there is nothing to
    /// stamp the drive with.
    #[error(transparent)]
    GenerationUnbound(#[from] lash_core::engine::GenerationUnbound),
    /// Restate did not accept the send.
    #[error(transparent)]
    Http(#[from] crate::RestateHttpError),
}

impl SendDriveError {
    /// Whether the send or attach timed out in transit
    /// ([`RestateHttpError::is_timeout`](crate::RestateHttpError::is_timeout)).
    pub fn is_timeout(&self) -> bool {
        matches!(self, Self::Http(error) if error.is_timeout())
    }
}

/// A send Restate refused keeps its cause's code and retry class. A
/// generation no core has bound is a deployment fact no retry changes.
impl crate::session_control::ControlFailure for SendDriveError {
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

/// Retain a generation continuation's lane in its journaled request identity.
/// The core owns the hash; stable continuations keep its spelling.
pub(super) fn session_drive_continuation(
    request: &DriveRequest,
    route: &crate::services::ServiceRoute,
) -> DriveRequestId {
    let next = drive_continuation_request(request);
    match route.lane() {
        crate::services::Lane::Stable => next,
        crate::services::Lane::Generation(generation) => {
            DriveRequestId::new(format!("{}:g{generation}", next.as_str()))
        }
    }
}

pub(super) fn continuation_generation(request: &DriveRequestId) -> Option<BuildGeneration> {
    let (digest, generation) = request
        .as_str()
        .strip_prefix(DRIVE_CONTINUATION_PREFIX)?
        .split_once(":g")?;
    (digest.len() == 64
        && digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
    .then(|| BuildGeneration::parse(generation).ok())
    .flatten()
}

/// Whether admission `ordinal` of `request`, on `route`, answers to its
/// build's drain (FIG-4639): every admission after the root the drive started
/// on does, so a drive whose build is draining admits no further root. The
/// drive's first admission answers to none, so the root it took always runs.
/// A continuation's first admission is the drive's next root: on a
/// generation lane it answers to the drain too, or a drive resumed there
/// would run its whole backlog in one-root legs on the draining build. Under
/// the stable name a new invocation is the newest build's, so its first root
/// runs.
pub(super) fn drain_answered(
    route: &crate::services::ServiceRoute,
    request: &DriveRequestId,
    ordinal: u32,
) -> bool {
    ordinal > 0
        || matches!(route.lane(), crate::services::Lane::Generation(_))
            && continuation_generation(request).is_some()
}

/// The request the admissions of `request`'s drive run under on the build
/// of `generation`, the one its invocation is pinned to. A root is stamped
/// with the generation of the build that admits it (FIG-4742), never the
/// request's: a continuation crosses builds under the stable name, and its
/// request keeps the generation that first sent the drive only as
/// provenance. Stamped with that one, a root the newest build admits from a
/// hand-off would count in, and park under, a generation it never ran on.
pub(super) fn admits(request: &DriveRequest, generation: &BuildGeneration) -> DriveRequest {
    DriveRequest {
        session: request.session.clone(),
        request: request.request.clone(),
        build_generation: generation.clone(),
    }
}

#[expect(
    clippy::result_large_err,
    reason = "the ingress client returns RestateHttpError unboxed across its public API"
)]
impl RestateSessionWork {
    /// Send `request`'s drive to `LashSession/{session}`, keyed by the
    /// request id: a repeated send of one request attaches to its first
    /// invocation instead of driving twice. A transient send failure retries
    /// under the same idempotency key before the ask is given up to the
    /// ingress relay. Resolves once Restate accepted the send, not once the
    /// drive ran.
    pub async fn send_drive(
        &self,
        session: &SessionId,
        request: DriveRequestId,
    ) -> Result<crate::RestateInvocationId, SendDriveError> {
        let body = RestateSessionDriveRequest {
            request: DriveRequest {
                session: session.clone(),
                request: request.clone(),
                build_generation: self.generation.get()?.clone(),
            },
            handed_off: None,
        };
        Ok(self
            .ingress
            .send_object_json_idempotent_bounded(
                &self.namespace.stable(LashService::SessionDriver).name(),
                session.as_str(),
                DRIVE_HANDLER,
                &Call::new(body),
                request.as_str(),
            )
            .await?)
    }

    /// Attach to `request`'s drive of `session` and return how it ended,
    /// sending it first if nothing sent it yet: the same idempotency key as
    /// [`send_drive`](Self::send_drive), so the call and an earlier send name
    /// one invocation. A continuation retains its generation lane in the
    /// recorded request id, so an attach joins that lane across build rolls.
    pub async fn attach_drive(
        &self,
        session: &SessionId,
        request: DriveRequestId,
    ) -> Result<DriveOutcome, super::SendDriveError> {
        let generation = continuation_generation(&request);
        let route = match &generation {
            Some(generation) => self
                .namespace
                .generation(LashService::SessionDriver, generation.clone()),
            None => self.namespace.stable(LashService::SessionDriver),
        };
        let body = RestateSessionDriveRequest {
            request: DriveRequest {
                session: session.clone(),
                request: request.clone(),
                build_generation: match generation {
                    Some(generation) => generation,
                    None => self.generation.get()?.clone(),
                },
            },
            handed_off: None,
        };
        Ok(self
            .ingress
            .call_object_json_idempotent::<_, Reply<DriveOutcome>>(
                &route.name(),
                session.as_str(),
                DRIVE_HANDLER,
                &Call::new(body),
                request.as_str(),
            )
            .await
            .map(Reply::into_body)?)
    }
    /// The leg a drive continues on after `leg` ended with `outcome`, when
    /// `leg` handed the drive on: at a root boundary, to its own lane, or
    /// because its build is draining, to the stable name.
    pub(super) fn continuation(
        &self,
        session: &SessionId,
        leg: &DriveRequestId,
        outcome: &DriveOutcome,
    ) -> Option<DriveRequestId> {
        // A leg ended, so a core bound the generation before it was sent.
        let build_generation = self.generation.get().ok()?.clone();
        let leg = DriveRequest {
            session: session.clone(),
            request: leg.clone(),
            build_generation,
        };
        match outcome.stop {
            DriveStop::HandedOff { .. } => {
                let route = match continuation_generation(&leg.request) {
                    Some(generation) => self
                        .namespace
                        .generation(LashService::SessionDriver, generation),
                    None => self.namespace.stable(LashService::SessionDriver),
                };
                Some(session_drive_continuation(&leg, &route))
            }
            DriveStop::Draining { .. } => Some(drive_continuation_request(&leg)),
            _ => None,
        }
    }
}
