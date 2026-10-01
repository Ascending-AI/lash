//! A cell's oversized prints and final value are retained before they enter
//! history, inside the cell's journaled `{cell}:outputs` step (FIG-1643).
//!
//! The step reads the retention policy and puts every output too long for
//! history as a session attachment; its recorded answer — the rendered
//! observations with each retained value's witness and reference, the
//! retained final value, and the policy — is what every replay reads. A
//! redrive under another policy is served the recorded decision verbatim: it
//! puts nothing, and the witnesses and references it hands history are the
//! live pass's.

use super::lifecycle_and_diagnostics::block_on;
use super::*;

const SEED: u64 = 0x1643_0001;
const SESSION: &str = "output-retention";
const TURN: &str = "turn-1";
type RecordedAttempt = (ExecResponse, Vec<lash_vm_client::service::WorkerReceipt>);

/// The live pass's policy: a 1 KiB inline limit and a 256-byte witness,
/// under which the cell's print and final value are both retained.
const LIVE_POLICY: lash_core::OutputRetentionPolicy = lash_core::OutputRetentionPolicy {
    inline_limit_bytes: 1024,
    witness_bytes: 256,
};

const REDRIVE_POLICY: lash_core::OutputRetentionPolicy = lash_core::OutputRetentionPolicy {
    inline_limit_bytes: 1024 * 1024,
    witness_bytes: 4 * 1024,
};

/// The facade's 3000-row retention payload, exercised through crash and redrive.
const CELL: &str = r#"
const rows = [];
for (let i = 0; i < 3000; i++) {
  rows.push({ index: i, text: "a row the cell prints and finishes with" });
}
print(rows);
finish({ rows });
"#;

fn attempt(
    backend: lash_core::Backend,
    policy: lash_core::OutputRetentionPolicy,
    crash: bool,
    responses: Arc<Mutex<Vec<RecordedAttempt>>>,
) -> lash_restate_test::HandlerAttempt {
    let invocation = lash_core::testing::exec_code_invocation(
        SESSION,
        TURN,
        0,
        0,
        "output retention",
        "exec-code:output-retention",
    );
    Arc::new(move |scoped| {
        let backend = backend.clone();
        let invocation = invocation.clone();
        let responses = Arc::clone(&responses);
        Box::pin(async move {
            let attachments = Arc::new(
                lash_core::facade_support::RuntimeAttachmentStore::ephemeral(
                    backend.attachment_store(),
                )
                .with_output_retention(policy),
            );
            let ctx = lash_core::testing::TestExecutionContextBuilder::new(
                crate::testing::attempt_ports(&backend, scoped),
            )
            .runtime_parent_invocation(invocation)
            .attachment_store(attachments)
            .build()
            .into_runtime();
            let workers = lash_vm_client::service::Service::default().with_worker_receipts();
            let mut state =
                RlmExecutionState::for_engine_with_workers("typescript", workers.clone());
            let response = execute_code_unbounded_with_test_render(
                &mut state,
                ctx,
                ExecRequest {
                    code: CELL.to_string(),
                },
                crate::testing::fresh_sqlite_memory_artifact_store().await,
                LashlangSurface::default(),
                None,
                RlmProjectedBindings::default(),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await;
            responses
                .lock_recover()
                .push((response, workers.worker_receipts()));
            assert!(
                !crash,
                "the attempt's deployment dies after the cell recorded its outputs"
            );
        })
    })
}

#[test]
pub(super) fn a_cells_retained_outputs_replay_verbatim_under_a_changed_policy() {
    block_on(async {
        let double =
            crate::testing::kernel_double(SEED, lash_restate_test::ServerConfig::default()).await;
        let backend = double.lash_backend();
        let responses = Arc::new(Mutex::new(Vec::new()));
        double
            .run_crashed_then_redriven(
                lash_core::AdmittedScope::turn(
                    lash_core::SessionId::from(SESSION),
                    lash_core::TurnId::from(TURN),
                ),
                attempt(backend.clone(), LIVE_POLICY, true, Arc::clone(&responses)),
                attempt(
                    backend.clone(),
                    REDRIVE_POLICY,
                    false,
                    Arc::clone(&responses),
                ),
            )
            .await
            .expect("the live pass crashes and its redrive completes");
        let responses = responses.lock_recover().clone();
        let [(live, live_workers), (redriven, replay_workers)] = responses.as_slice() else {
            panic!("one crashed pass and one redrive ran the cell: {responses:?}");
        };
        for response in [live, redriven] {
            assert!(response.error.is_none(), "{:?}", response.error);
        }
        for workers in [live_workers, replay_workers] {
            assert_eq!(
                workers.last().map(|receipt| receipt.path),
                Some(lash_vm_client::service::WorkerPath::Cell),
                "a completed cell must carry its state view without reopening its snapshot"
            );
        }

        // The live pass retained both outputs under its policy.
        let [observation] = live.observations.as_slice() else {
            panic!("the cell printed once: {:?}", live.observations);
        };
        let lash_core::OutputValue::Retained(print) = &observation.value else {
            panic!("the oversized print is retained: {:?}", observation.value);
        };
        let finish = live
            .terminal_finish_retained
            .as_ref()
            .expect("the oversized final value is retained");
        for retained in [print, finish] {
            assert!(
                retained.witness.len() <= 256,
                "the witness is bounded by the recorded policy: {} bytes",
                retained.witness.len()
            );
            assert!(retained.witness.contains(retained.reference.id.as_str()));
        }

        // The redrive, under a policy that would keep both inline, is served
        // the recorded decision verbatim.
        assert_eq!(redriven.observations, live.observations);
        assert_eq!(
            redriven.terminal_finish_retained,
            live.terminal_finish_retained
        );
        assert_eq!(redriven.terminal_finish, live.terminal_finish);

        // The references resolve to the exact encodings of the values.
        let value = live
            .terminal_finish
            .clone()
            .expect("the cell finished with its value");
        assert_eq!(value["rows"].as_array().map(Vec::len), Some(3000));
        let stored = backend
            .attachment_store()
            .get(&finish.reference.id, 32 * 1024 * 1024)
            .await
            .expect("the retained final value is stored");
        assert!(!REDRIVE_POLICY.retains(stored.bytes.len()));
        assert_eq!(
            stored.bytes,
            serde_json::to_vec(&value).expect("encode the final value")
        );
        let printed = backend
            .attachment_store()
            .get(&print.reference.id, 32 * 1024 * 1024)
            .await
            .expect("the retained print is stored");
        assert!(!REDRIVE_POLICY.retains(printed.bytes.len()));
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&printed.bytes).expect("a JSON print"),
            value["rows"]
        );
    });
}

#[tokio::test]
async fn a_permanent_rlm_retention_refusal_is_typed_and_terminal() {
    let double = lash_restate_test::backend(SEED + 9, Default::default())
        .await
        .expect("double");
    let attachments = lash_core::facade_support::RuntimeAttachmentStore::ephemeral(
        double.lash_backend().attachment_store(),
    )
    .with_max_attachment_bytes(Some(64));
    let value = serde_json::json!("x".repeat(900));
    let error = super::super::retain_oversized_value(
        &attachments,
        lash_core::OutputRetentionPolicy {
            inline_limit_bytes: 128,
            witness_bytes: 64,
        },
        &value,
        "law",
    )
    .await
    .expect_err("the output exceeds the attachment limit");
    assert!(
        error.is_terminal(),
        "the deterministic refusal is terminal: {error:?}"
    );
    assert!(
        !error
            .journal_disposition(lash_core::RuntimeEffectKind::LanguageRuntimeValue)
            .is_retryable_derivation()
    );
    assert!(
        error.cause.is_some(),
        "retain the typed attachment-store cause"
    );
}
