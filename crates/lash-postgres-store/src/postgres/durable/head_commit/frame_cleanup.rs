//! Frame carries and cleanup obligations in a session head commit.

use super::*;

/// The writes of the frames the commit leaves (ADR 0113 §3.1), decided
/// under their referrer locks: a transition's carries into its successor,
/// each left frame's fence and its `Ended` cleanup, and the SQL share of an
/// ungated end.
pub(super) fn end_frames_left(
    program: &mut Program,
    commit: &lash_core_store::store::RuntimeCommit,
    read: &ReadFacts,
) -> Result<(), StoreError> {
    if let Some(transition) = &commit.frame_transition {
        let successor = lash_core_store::artifact_referrer::ArtifactReferrer::FrameEnvironment(
            transition.successor.clone(),
        );
        for (namespace, artifact_ref) in &read.carries {
            program.push(
                Step::any(
                    &CARRY_EDGE,
                    vec![
                        text(*namespace),
                        text(artifact_ref.as_str()),
                        text(successor.kind().as_str()),
                        text(successor.canonical_id()),
                    ],
                ),
                Refusal::Backstop,
            );
        }
    }
    let gate = commit
        .frame_transition
        .as_ref()
        .map(|transition| transition.gate.clone());
    for ended in &read.ended {
        let kind = ended.referrer.kind().as_str();
        let id = ended.referrer.canonical_id();
        program.push(
            Step::any(
                &FENCE_REFERRER,
                vec![text(kind), text(id.clone()), Value::Now],
            ),
            Refusal::Backstop,
        );
        let cleanup = lash_core_store::artifact_referrer::ArtifactCleanup::ended(
            ended.referrer.clone(),
            Vec::new(),
            gate.clone(),
        );
        let json = cleanup
            .to_json()
            .map_err(|error| StoreError::Backend(error.to_string()))?;
        let obligation = lash_core_store::store::ObligationKey::ArtifactCleanup {
            referrer: ended.referrer.clone(),
        }
        .id();
        use lash_core_store::store::CleanupUpsert;
        match ended.decision {
            CleanupUpsert::Insert => program.push(
                Step::any(
                    &CLEANUP_INSERT,
                    vec![
                        text(kind),
                        text(id.clone()),
                        text(json),
                        text(obligation.as_str()),
                        Value::Now,
                        Value::Text(cleanup.gate().map(|gate| gate.key().to_owned())),
                    ],
                ),
                Refusal::Backstop,
            ),
            CleanupUpsert::ReplaceGuard => program.push(
                Step::any(
                    &CLEANUP_REPLACE,
                    vec![
                        text(kind),
                        text(id.clone()),
                        text(json),
                        Value::Now,
                        Value::Text(cleanup.gate().map(|gate| gate.key().to_owned())),
                    ],
                ),
                Refusal::Backstop,
            ),
            CleanupUpsert::Keep => {}
        }
        if gate.is_none()
            && ended.decision != CleanupUpsert::Keep
            && ended.referrer.kind().holds_artifacts()
        {
            program.push(
                super::super::turns::end_local_step(&ended.referrer, None)?,
                Refusal::Backstop,
            );
        }
    }
    Ok(())
}
