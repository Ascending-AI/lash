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

/// The live pass's policy: a 1 KiB inline limit and a 256-byte witness,
/// under which the cell's print and final value are both retained.
const LIVE_POLICY: lash_core::OutputRetentionPolicy = lash_core::OutputRetentionPolicy {
    inline_limit_bytes: 1024,
    witness_bytes: 256,
};

/// A cell whose print and final value encode to roughly 30 KB each: past the
/// live policy's limit, well within the default's.
const CELL: &str = r#"
const rows = [];
for (let i = 0; i < 600; i++) {
  rows.push({ index: i, text: "a row the cell prints and finishes with" });
}
print(rows);
finish({ rows });
"#;

fn attempt(
    backend: lash_core::Backend,
    policy: lash_core::OutputRetentionPolicy,
    crash: bool,
    responses: Arc<Mutex<Vec<ExecResponse>>>,
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
                lash_core::facade_support::SessionAttachmentStore::ephemeral(
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
            let mut state = RlmExecutionState::new();
            let response = execute_code_unbounded_with_test_render(
                &mut state,
                ctx,
                ExecRequest {
                    code: CELL.to_string(),
                },
                crate::testing::fresh_memory_artifact_store().await,
                LashlangSurface::default(),
                None,
                RlmProjectedBindings::default(),
                RlmLashlangExecutionTraceConfig::default(),
            )
            .await;
            responses.lock_recover().push(response);
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
                    lash_core::OutputRetentionPolicy::DEFAULT,
                    false,
                    Arc::clone(&responses),
                ),
            )
            .await
            .expect("the live pass crashes and its redrive completes");
        let responses = responses.lock_recover().clone();
        let [live, redriven] = responses.as_slice() else {
            panic!("one crashed pass and one redrive ran the cell: {responses:?}");
        };
        for response in [live, redriven] {
            assert!(response.error.is_none(), "{:?}", response.error);
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
        let stored = backend
            .attachment_store()
            .get(&finish.reference.id)
            .await
            .expect("the retained final value is stored");
        assert_eq!(
            stored.bytes,
            serde_json::to_vec(&value).expect("encode the final value")
        );
        let printed = backend
            .attachment_store()
            .get(&print.reference.id)
            .await
            .expect("the retained print is stored");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&printed.bytes).expect("a JSON print"),
            value["rows"]
        );
    });
}
