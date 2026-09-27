//! The journaled answer of a drive admission for a session whose store could
//! not be opened at all (FIG-3822, FIG-3630, ADR 0104 O1): the session's
//! retirement is a settled fact the recorded step carries, not a derivation
//! fault an attempt retries. The step is emitted, its recorded body is the
//! retirement, and a second attempt of the same admission decodes that record
//! instead of running the body again — the wedge a session delete left
//! behind was an attempt that could never reach this step.
use super::effect::*;
use super::*;

#[tokio::test]
pub(super) async fn a_retired_sessions_drive_admission_step_records_the_retirement() {
    let backend = memory_backend().await;
    let session = lash_core::SessionId::from("retired-admission");
    let request = lash_core::engine::DriveRequest {
        session: session.clone(),
        request: lash_core::engine::DriveRequestId::new("retired-drive"),
        build_generation: backend.build_generation().clone(),
    };
    let recorder = RecordingEffectController::default().with_strict_replay_by_address();
    let scope = layered_scope(
        &backend,
        Arc::new(recorder.clone()),
        lash_core::engine::drive_admission_scope(&session, &request.request),
    );
    let stores = backend.session_store_factory();

    let abort = lash_core::drive::admit_drive_retired(&scope, &request, 0, Arc::clone(&stores))
        .await
        .expect_err("a session whose store cannot open refuses its admission");
    assert!(
        matches!(
            abort,
            lash_core::engine::DriveAbort::Refused(ref error)
                if error.code == lash_core::RuntimeErrorCode::SessionDeleted
        ),
        "the recorded step answers the session's retirement: {abort:?}"
    );
    assert_eq!(
        recorder.count_kind(lash_core::RuntimeEffectKind::AdmitDrive),
        1,
        "the admission step was journaled once"
    );

    // A redrive of the same admission decodes the recorded retirement rather
    // than running the body against the retired store again (ADR 0104 O1).
    let replayed = lash_core::drive::admit_drive_retired(&scope, &request, 0, stores)
        .await
        .expect_err("the replayed admission decodes the recorded retirement");
    assert!(
        matches!(
            replayed,
            lash_core::engine::DriveAbort::Refused(ref error)
                if error.code == lash_core::RuntimeErrorCode::SessionDeleted
        ),
        "the recorded body is what every replay answers: {replayed:?}"
    );
    assert_eq!(
        recorder.count_kind(lash_core::RuntimeEffectKind::AdmitDrive),
        1,
        "the second attempt replayed the journal; the body ran once"
    );
}
