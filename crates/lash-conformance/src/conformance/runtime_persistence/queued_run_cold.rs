//! Admission and physical-commit recovery in independently opened worker processes.
use super::*;
use lash_core::store::{
    BeginQueuedRun, QueuedRunAdmission, QueuedRunCommit, QueuedRunMember, QueuedRunOrigin,
    QueuedRunProgress, QueuedRunRequest, QueuedRunTerminal,
};
use tokio::io::{AsyncBufReadExt as _, BufReader};

#[derive(serde::Serialize, serde::Deserialize)]
struct Witness {
    admission: QueuedRunAdmission,
    first: crate::InputId,
    commit: Option<RuntimeCommit>,
    receipt: Option<crate::RuntimeCommitReceipt>,
}

#[expect(
    clippy::unwrap_used,
    reason = "cold-process certification helper: broken setup or durable evidence must fail the worker"
)]
pub async fn queued_run_cold_process_driver(
    store: Arc<dyn RuntimePersistence>,
    nonce: &str,
    action: &str,
    marker: &std::path::Path,
) {
    let session_id = SessionId::from(format!("queued-cold-{nonce}"));
    bind_conformance_session(&store, &session_id).await;
    let state = RuntimeSessionState {
        session_id: session_id.clone(),
        ..RuntimeSessionState::new(crate::SessionPolicy::new(crate::TurnBudget::Unbounded))
    };
    let lease = claim_session_execution_lease_for_test(
        &store,
        &session_id,
        if action == "queued_recover" {
            "cold-successor"
        } else {
            "cold-predecessor"
        },
    )
    .await;
    let request = BeginQueuedRun {
        session_id: session_id.clone(),
        identity: None,
        request: QueuedRunRequest::Automatic,
        configuration: RuntimeCommit::persisted_state_for_test(&state, &[]).config,
        expected_head_revision: 0,
        initial_turn_index: 1,
    };
    if action == "queued_recover" {
        let witness: Witness = serde_json::from_slice(&std::fs::read(marker).unwrap()).unwrap();
        let resumed = store
            .begin_or_resume_queued_run(
                &lease.authority(),
                BeginQueuedRun {
                    identity: Some(witness.admission.scope.clone()),
                    ..request
                },
            )
            .await
            .unwrap();
        let mut expected = witness.admission.clone();
        expected.origin = QueuedRunOrigin::Explicit;
        assert_eq!(
            serde_json::to_value(&resumed).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "cold reopen preserves admission and records explicit reentry"
        );
        if let (Some(mut commit), Some(mut receipt)) = (witness.commit, witness.receipt) {
            commit.session_execution_lease_fence = Some(lease.authority());
            let replay = store.commit_runtime_state(commit).await.unwrap();
            assert!(replay.receipt_replayed);
            receipt.receipt_replayed = true;
            assert_eq!(
                serde_json::to_value(replay).unwrap(),
                serde_json::to_value(receipt).unwrap(),
                "lost commit reply replays before revision checks"
            );
        }
        let later = store
            .enqueue_pending_turn_input(pending_next_turn_input_draft(
                &session_id,
                "after-worker-death",
            ))
            .await
            .unwrap();
        if resumed.terminal.is_none() {
            let selection = store
                .select_queued_run(
                    &lease.authority(),
                    &resumed.scope,
                    &lease.owner,
                    1,
                    &resumed.configuration,
                    lash_core::testing::queued_work_claim_policy(1),
                )
                .await
                .unwrap();
            if resumed.members.is_none() {
                assert_eq!(
                    selection.admission.members,
                    Some(vec![QueuedRunMember::Input(witness.first)])
                );
            } else {
                assert_eq!(selection.admission.members, resumed.members);
            }
            assert!(
                !selection
                    .admission
                    .members
                    .as_ref()
                    .unwrap()
                    .contains(&QueuedRunMember::Input(later.input_id.clone())),
                "new arrival cannot enter frozen selection"
            );
            assert_eq!(selection.admission.position, resumed.position);
        } else {
            assert!(
                store
                    .pending_queued_run(&session_id)
                    .await
                    .unwrap()
                    .is_none()
            );
            let next = store
                .begin_or_resume_queued_run(
                    &lease.authority(),
                    BeginQueuedRun {
                        expected_head_revision: 2,
                        session_id: session_id.clone(),
                        identity: None,
                        request: QueuedRunRequest::Automatic,
                        configuration: resumed.configuration.clone(),
                        initial_turn_index: 3,
                    },
                )
                .await
                .unwrap();
            assert_ne!(next.scope, resumed.scope);
            let selected = store
                .select_queued_run(
                    &lease.authority(),
                    &next.scope,
                    &lease.owner,
                    1,
                    &next.configuration,
                    lash_core::testing::queued_work_claim_policy(1),
                )
                .await
                .unwrap();
            assert_eq!(
                selected.admission.members,
                Some(vec![QueuedRunMember::Input(later.input_id)])
            );
        }
        println!("queued_recovered");
        return;
    }
    let first = store
        .enqueue_pending_turn_input(pending_next_turn_input_draft(
            &session_id,
            "before-worker-death",
        ))
        .await
        .unwrap();
    let mut admission = store
        .begin_or_resume_queued_run(&lease.authority(), request)
        .await
        .unwrap();
    let mut commit = None;
    let mut receipt = None;
    if action != "queued_admission" {
        let selected = store
            .select_queued_run(
                &lease.authority(),
                &admission.scope,
                &lease.owner,
                1,
                &admission.configuration,
                lash_core::testing::queued_work_claim_policy(1),
            )
            .await
            .unwrap();
        admission = selected.admission;
        if action != "queued_selection" {
            let mut advance = RuntimeCommit::persisted_state_with_operation_for_testing(
                &state,
                &[],
                crate::OperationId::new(admission.scope.clone(), "physical-0"),
            );
            advance.session_execution_lease_fence = Some(lease.authority());
            advance.completed_turn_input_claims = selected
                .inputs
                .iter()
                .map(|claim| claim.completion())
                .collect();
            advance
                .enqueued_queue_batches
                .push(checkpoint_claims::queued_draft(
                    &session_id,
                    "outbox-after-first-turn",
                    DeliveryPolicy::AfterCurrentTurnCommit,
                ));
            advance.queued_run = Some(Box::new(QueuedRunCommit {
                scope: admission.scope.clone(),
                expected_revision: admission.revision,
                progress: QueuedRunProgress::Advance {
                    position: admission.position.next(&admission.scope).unwrap(),
                    members: Vec::new(),
                    withheld_members: Vec::new(),
                    include_outbox: true,
                },
            }));
            let advanced = store.commit_runtime_state(advance.clone()).await.unwrap();
            admission = store
                .pending_queued_run(&session_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                admission.members,
                Some(vec![QueuedRunMember::Batch(
                    advanced.enqueued_queue_batches[0].batch_id.clone()
                )])
            );
            commit = Some(advance);
            receipt = Some(advanced);
            if action == "queued_terminal" {
                let selected = store
                    .select_queued_run(
                        &lease.authority(),
                        &admission.scope,
                        &lease.owner,
                        1,
                        &admission.configuration,
                        lash_core::testing::queued_work_claim_policy(1),
                    )
                    .await
                    .unwrap();
                let mut settle = RuntimeCommit::persisted_state_with_operation_for_testing(
                    &state,
                    &[],
                    crate::OperationId::new(admission.scope.clone(), "physical-1"),
                );
                settle.expected_head_revision = 1;
                settle.session_execution_lease_fence = Some(lease.authority());
                settle.completed_queue_claims = selected
                    .queued
                    .iter()
                    .map(|claim| claim.completion())
                    .collect();
                settle.queued_run = Some(Box::new(QueuedRunCommit {
                    scope: admission.scope.clone(),
                    expected_revision: admission.revision,
                    progress: QueuedRunProgress::Settle {
                        terminal: QueuedRunTerminal::Empty,
                    },
                }));
                let settled = store.commit_runtime_state(settle.clone()).await.unwrap();
                admission = store
                    .begin_or_resume_queued_run(
                        &lease.authority(),
                        BeginQueuedRun {
                            session_id: session_id.clone(),
                            identity: Some(admission.scope.clone()),
                            request: QueuedRunRequest::Automatic,
                            configuration: admission.configuration.clone(),
                            expected_head_revision: 0,
                            initial_turn_index: 1,
                        },
                    )
                    .await
                    .unwrap();
                commit = Some(settle);
                receipt = Some(settled);
            }
        }
    }
    let witness = Witness {
        admission,
        first: first.input_id,
        commit,
        receipt,
    };
    let mut file = std::fs::File::create(marker).unwrap();
    std::io::Write::write_all(&mut file, &serde_json::to_vec(&witness).unwrap()).unwrap();
    file.sync_all().unwrap();
    println!("crash_ready");
    std::io::Write::flush(&mut std::io::stdout()).unwrap();
    std::future::pending::<()>().await;
}

#[expect(
    clippy::unwrap_used,
    reason = "cold-process certification helper: broken setup or durable evidence must fail the worker"
)]
pub async fn assert_queued_run_cold_process_recovery(
    tempdir: &std::path::Path,
    mut command: impl FnMut(&str, &str, &std::path::Path) -> tokio::process::Command,
) {
    for action in [
        "queued_admission",
        "queued_selection",
        "queued_advance",
        "queued_terminal",
    ] {
        let nonce = uuid::Uuid::new_v4().to_string();
        let marker = tempdir.join(format!("{action}-{nonce}.json"));
        let mut child = command(action, &nonce, &marker)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let stdout = child.stdout.take().unwrap();
        let mut lines = BufReader::new(stdout).lines();
        assert_eq!(
            lines.next_line().await.unwrap().as_deref(),
            Some("crash_ready"),
            "{action} helper reaches durable boundary"
        );
        child.kill().await.unwrap();
        assert!(!child.wait().await.unwrap().success());
        let recovered = command("queued_recover", &nonce, &marker)
            .kill_on_drop(true)
            .output()
            .await
            .unwrap();
        assert!(
            recovered.status.success(),
            "{action} recovery failed: {}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        assert!(
            String::from_utf8_lossy(&recovered.stdout)
                .lines()
                .any(|line| line == "queued_recovered")
        );
    }
}
