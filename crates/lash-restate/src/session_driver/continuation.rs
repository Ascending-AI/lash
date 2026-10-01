//! A continuation retains the lane its recorded send names, so an attach
//! joins that invocation across build rolls and later handoffs.

use super::*;
use lash_core::engine::{DRIVE_CONTINUATION_PREFIX, drive_continuation_request};

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

#[expect(
    clippy::result_large_err,
    reason = "the ingress client returns RestateHttpError unboxed across its public API"
)]
impl RestateSessionWork {
    /// Attach to `request`'s drive of `session` and return how it ended,
    /// sending it first if nothing sent it yet: the same idempotency key as
    /// [`send_drive`](Self::send_drive), so the call and an earlier send name
    /// one invocation. A continuation retains its generation lane in the
    /// recorded request id, so an attach joins that lane across build rolls.
    pub async fn attach_drive(
        &self,
        session: &SessionId,
        request: DriveRequestId,
    ) -> Result<DriveOutcome, crate::RestateHttpError> {
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
                build_generation: generation.unwrap_or_else(|| self.build_generation.clone()),
            },
            handed_off: None,
        };
        self.ingress
            .call_object_json_idempotent::<_, Reply<DriveOutcome>>(
                &route.name(),
                session.as_str(),
                DRIVE_HANDLER,
                &Call::new(body),
                request.as_str(),
            )
            .await
            .map(Reply::into_body)
    }
    /// The leg a drive continues on after `leg` ended with `outcome`, when
    /// `leg` handed off at a root boundary.
    pub(super) fn continuation(
        &self,
        session: &SessionId,
        leg: &DriveRequestId,
        outcome: &DriveOutcome,
    ) -> Option<DriveRequestId> {
        matches!(outcome.stop, DriveStop::HandedOff { .. }).then(|| {
            let route = match continuation_generation(leg) {
                Some(generation) => self
                    .namespace
                    .generation(LashService::SessionDriver, generation),
                None => self.namespace.stable(LashService::SessionDriver),
            };
            session_drive_continuation(
                &DriveRequest {
                    session: session.clone(),
                    request: leg.clone(),
                    build_generation: self.build_generation.clone(),
                },
                &route,
            )
        })
    }
}
