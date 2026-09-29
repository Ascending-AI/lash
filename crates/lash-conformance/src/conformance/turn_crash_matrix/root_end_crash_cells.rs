//! A failed post-commit delivery ends a root whose terminal checkpoint
//! withheld a follow-on input. The root-end write is outside a journaled step.

use super::*;
use pretty_assertions::assert_eq;

#[expect(clippy::expect_used, reason = "conformance-law fixture assertions")]
async fn run_root_end_crash_cell<F, S>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
    placement: CrashPlacement,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimeStore + crate::store::StoreTestSupport + 'static,
{
    let scenario = match placement {
        CrashPlacement::Boundary => "root-end-before-write",
        CrashPlacement::InsideCall => "root-end-after-write",
        _ => unreachable!("root-end cells use only write placements"),
    };
    let identity = ReferenceIdentity::for_scenario(scenario);
    let reader: Arc<dyn RuntimeStore> = make(scenario);
    seed_reference_ingress_for_drive(&reader, &identity, scenario).await;
    reader
        .enqueue_pending_turn_input(PendingTurnInputDraft::new(
            &identity.session_id,
            crate::TurnInputIngress::active_turn(
                &identity.turn_id,
                crate::TurnInputCheckpointBoundary::BeforeCompletion,
            ),
            crate::TurnInput::text("withheld follow-on input"),
        ))
        .await
        .expect("seed the terminal-checkpoint input");
    let seeded = reader
        .list_pending_turn_inputs(&identity.session_id)
        .await
        .expect("read seeded inputs")
        .into_iter()
        .map(|row| row.input.input_id)
        .collect::<Vec<_>>();
    let before = reader
        .load_session_head_meta(&identity.session_id)
        .await
        .expect("read initial head")
        .map_or(0, |head| head.head_revision);

    let make = |scenario: &str| make(scenario) as Arc<dyn RuntimeStore>;
    let host = LawSeamHost::over(host);
    let law = MatrixLaw {
        stores: &stores,
        make: &make,
        host: &host,
        runner: &runner,
    };
    let point = TurnCrashPoint {
        operation: TurnSeamOperation::Store(StoreOperation::CommitRootEnd),
        placement,
    };
    let (report, redriven, executions, crashed) = Box::pin(
        admission_crash_cells::crash_then_redrive(&law, &identity, scenario, point, None, true),
    )
    .await;
    assert!(
        crashed.contains(&TurnSeamOperation::Store(StoreOperation::CommitRootEnd)),
        "the crash reached the root-end commit"
    );
    let drain = report.unwrap_or_else(|error| panic!("{scenario}: redrive failed: {error}"));
    let terminal = reader
        .root_terminal(&identity.session_id, &identity.turn_id)
        .await
        .expect("read root terminal")
        .unwrap_or_else(|| panic!("{scenario}: no root terminal after redrive: {drain:?}"));
    assert_eq!(terminal.head_revision, Some(before + 2));
    assert_eq!(executions, 1, "the redrive runs no tool again");
    assert_eq!(
        reader
            .load_session_head_meta(&identity.session_id)
            .await
            .expect("read final head")
            .expect("the root committed")
            .head_revision,
        before + 2,
        "one physical turn and one root-end write committed"
    );
    assert!(
        reader
            .unfinished_root(&identity.session_id)
            .await
            .expect("read unfinished root")
            .is_none(),
        "the session has no unfinished root"
    );
    let applications = reader
        .list_turn_input_applications(&identity.session_id)
        .await
        .expect("read input applications");
    for input in seeded {
        assert!(
            applications
                .iter()
                .filter(|row| row.input_id == input)
                .count()
                <= 1,
            "{scenario}: {input} was not settled twice: {applications:?}"
        );
    }
    assert!(
        redriven.iter().all(|operation| !matches!(
            operation,
            TurnSeamOperation::Provider(_)
                | TurnSeamOperation::Effect(EffectOperation::ToolAttempt { .. })
        )),
        "{scenario}: redrive runs no new model or tool call"
    );
}

pub async fn root_end_commit_crash_before_write_replays_once<F, S>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimeStore + crate::store::StoreTestSupport + 'static,
{
    Box::pin(run_root_end_crash_cell(
        stores,
        make,
        host,
        runner,
        CrashPlacement::Boundary,
    ))
    .await;
}

pub async fn root_end_commit_crash_after_write_replays_once<F, S>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimeStore + crate::store::StoreTestSupport + 'static,
{
    Box::pin(run_root_end_crash_cell(
        stores,
        make,
        host,
        runner,
        CrashPlacement::InsideCall,
    ))
    .await;
}
