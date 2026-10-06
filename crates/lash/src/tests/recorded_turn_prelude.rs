//! FIG-5133: a turn's environment sync records its prelude, history and
//! prepared context included, in the store set under its digest before the
//! sync's outcome completes, and the outcome journals only the digest. So:
//!
//! - a turn whose handler died after the sync completed, before the turn
//!   committed, replays the sync from its journal and reads the same prelude
//!   back by the digest: the redriven model call is the call the first
//!   attempt made, and the journal never carries the transcript before it;
//! - a replay whose recorded prelude is gone refuses the run with a typed
//!   `ArtifactMissing`, and never prepares the turn again in its place.

use super::*;

const SEED: u64 = 0x5133;
const SESSION: &str = "recorded-prelude";
/// The turn whose handler dies.
const TURN: &str = "recorded-prelude-turn";
/// The turn before it: its input is transcript the crashed turn's prelude
/// carries in its history.
const EARLIER_TURN: &str = "recorded-prelude-earlier";
const EARLIER_INPUT: &str = "fig5133: an earlier turn's transcript";
const INPUT: &str = "fig5133: the turn whose handler dies";

/// What the laws' model does.
struct Script {
    /// The deployment whose run handler dies under the model call numbered
    /// `dies_at`, before the call's result is journaled: the turn's sync
    /// before it has completed.
    double: lash_restate_test::RestateTestBackend,
    dies_at: usize,
    /// Set, the dying call also releases the turn's recorded prelude, as a
    /// store that lost it would leave it.
    loses_prelude: bool,
    served: std::sync::Mutex<Vec<LlmRequest>>,
}

/// The `execution` referrer of the dying turn's journal.
fn turn_journal() -> lash_core::ArtifactReferrer {
    lash_core::ArtifactReferrer::Execution(
        lash_core::ExecutionScope::turn(SESSION, TURN)
            .journal_identity()
            .expect("the turn journal's identity"),
    )
}

fn scripted_provider(script: &Arc<Script>) -> ProviderHandle {
    let script = Arc::clone(script);
    crate::testing::TestProvider::builder()
        .kind("recorded-turn-prelude")
        .complete(move |request| {
            let script = Arc::clone(&script);
            let dies = {
                let mut served = script.served.lock_recover();
                served.push(request);
                served.len() == script.dies_at + 1
            };
            if dies {
                script
                    .double
                    .crash_run_execution(lash_restate_test::CrashPoint::BeforeRunResult {
                        name: None,
                    });
            }
            async move {
                if dies && script.loses_prelude {
                    script
                        .double
                        .lash_backend()
                        .turn_prelude_store()
                        .end_turn_prelude_referrer(&lash_core::ResolvedArtifactCleanup {
                            referrer: turn_journal(),
                            carries: Vec::new(),
                        })
                        .await
                        .expect("release the recorded prelude");
                }
                Ok(text_response("answered"))
            }
        })
        .build()
        .into_handle()
}

/// The laws' deployment, its model's script, and the core over it.
async fn deployment(
    dies_at: usize,
    loses_prelude: bool,
) -> Result<(lash_restate_test::RestateTestBackend, Arc<Script>, LashCore)> {
    let double = restate_double(SEED).await;
    let script = Arc::new(Script {
        double: double.clone(),
        dies_at,
        loses_prelude,
        served: std::sync::Mutex::new(Vec::new()),
    });
    let core = explicit_ephemeral_facets(LashCore::standard_builder(double.lash_backend()))
        .serve_test_llm_profile(scripted_provider(&script), mock_llm_profile_spec())
        .build(crate::testing::runtime_lease_owner())?;
    Ok((double, script, core))
}

/// Whether `haystack` contains `needle`.
fn carries(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Every prelude digest the turn's journal recorded.
fn journaled_prelude_refs(
    journal: &[lash_restate_test::JournalEntryView],
) -> Vec<lash_core::TurnPreludeRef> {
    const FIELD: &[u8] = br#""prelude":""#;
    let mut refs = Vec::new();
    for entry in journal {
        let payload = entry.payload.as_ref();
        for start in (0..payload.len().saturating_sub(FIELD.len()))
            .filter(|start| payload[*start..].starts_with(FIELD))
        {
            let digest = &payload[start + FIELD.len()..];
            let end = digest.iter().position(|byte| *byte == b'"').unwrap_or(0);
            let digest = String::from_utf8_lossy(&digest[..end]).into_owned();
            let prelude_ref: lash_core::TurnPreludeRef =
                serde_json::from_value(serde_json::Value::String(digest))
                    .expect("a journaled prelude reference");
            if !refs.contains(&prelude_ref) {
                refs.push(prelude_ref);
            }
        }
    }
    refs
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn a_turn_crashed_after_its_sync_replays_the_prelude_it_stored_by_digest()
-> Result<()> {
    let (double, script, core) = deployment(1, false).await?;
    let session = core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    session
        .send(TurnInput::text(EARLIER_INPUT))
        .id(lash_core::TurnId::fixture(EARLIER_TURN))
        .output()
        .await?;
    let output = session
        .send(TurnInput::text(INPUT))
        .id(lash_core::TurnId::fixture(TURN))
        .output()
        .await?;
    assert_eq!(output.assistant_message(), Some("answered"));

    // The handler died under the turn's model call, so its redrive made the
    // call again, from the prelude its journal recorded: the same request.
    let served = script.served.lock_recover().clone();
    assert_eq!(served.len(), 3, "the redrive makes the model call again");
    assert_eq!(
        serde_json::to_value(&served[1].messages)?,
        serde_json::to_value(&served[2].messages)?,
        "the replayed sync serves the prelude the first attempt recorded"
    );

    let server = double.server();
    let turn_run = server
        .turn_invocations(
            &lash_core::SessionId::from(SESSION),
            &lash_core::TurnId::fixture(TURN),
        )
        .into_iter()
        .next()
        .expect("the turn's recorded invocation");
    let journal = server.journal(&turn_run.id).unwrap_or_default();
    let refs = journaled_prelude_refs(&journal);
    assert_eq!(
        refs.len(),
        1,
        "the sync journals one prelude digest: {refs:?}"
    );
    let mut command = None;
    for entry in &journal {
        if entry.name.is_some() {
            command = entry.name.as_deref();
        }
        if entry.ty != lash_restate_test::protocol::MessageType::InputCommand {
            assert!(
                !carries(&entry.payload, EARLIER_INPUT.as_bytes()),
                "the turn's journal carries no transcript: a {:?} after {command:?}: {}",
                entry.ty,
                String::from_utf8_lossy(&entry.payload)
            );
        }
    }

    // The bytes the digest names are in the store set, written before the
    // sync's outcome completed, and they are the prelude the turn ran.
    let stores = double.lash_backend().turn_prelude_store();
    let bytes = stores
        .get_turn_prelude(&refs[0])
        .await
        .expect("the store answers")
        .expect("the recorded prelude is stored under its digest");
    assert!(refs[0].matches_store_bytes(&bytes));
    let prelude = refs[0]
        .read(stores.as_ref())
        .await
        .expect("the prelude reads back by its digest");
    let history = serde_json::to_vec(&prelude.history)?;
    assert!(
        carries(&history, EARLIER_INPUT.as_bytes()) && carries(&history, INPUT.as_bytes()),
        "the recorded prelude's history carries the transcript and the turn's input"
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
pub(super) async fn a_replayed_sync_whose_prelude_is_gone_refuses_the_run_typed() -> Result<()> {
    let (_double, script, core) = deployment(0, true).await?;
    let session = core
        .session(crate::SessionId::parse(SESSION).expect("nonblank host identity"))
        .created()
        .await
        .open()
        .await?;
    let outcome = session
        .send(TurnInput::text(INPUT))
        .id(lash_core::TurnId::fixture(TURN))
        .await?
        .outcome()
        .await?;
    let crate::SendOutcome::Refused { refusal, .. } = &outcome else {
        panic!("a prelude that is gone refuses the run: {outcome:?}");
    };
    assert_eq!(
        refusal.code,
        lash_core::RuntimeErrorCode::ArtifactMissing,
        "the refusal is the typed missing prelude: {refusal:?}"
    );
    assert_eq!(
        script.served.lock_recover().len(),
        1,
        "the replay prepares nothing in the lost prelude's place and calls no model"
    );
    Ok(())
}
