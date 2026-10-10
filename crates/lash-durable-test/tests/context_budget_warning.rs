//! The RLM protocol's context-budget warning and its `continue_as` path,
//! through a host's `send()` with the core's node serving each turn
//! (FIG-4042, FIG-4398).
//!
//! A session's first turn reports more prompt tokens than the protocol's
//! soft warning threshold. The next turn, which begins over it, warns the
//! model at its work checkpoint, names the frame switch path, and emits one
//! keyed status. The model then switches frames with `control.continue_as`:
//! the follow-on turn runs on a fresh frame that carries only the seed it
//! was handed, never the first frame's input.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::{Arc, Mutex};
use std::time::Duration;

use lash_core::llm::types::{LlmRequest, LlmResponse, LlmUsage};
use lash_core::{LlmOutputPart, PluginRuntimeEvent, TurnEvent};
use served::{Tier, WATCHDOG, World};

const OLD_INPUT: &str = "OLD-ONLY-4042";
const BATON: &str = "SEED-BATON-4042";
/// The threshold the protocol warns past.
const WARN_AT: usize = 100;

/// A cell of `source`, reporting `input_tokens` of prompt.
fn cell(source: &str, input_tokens: i64) -> LlmResponse {
    LlmResponse {
        parts: vec![LlmOutputPart::Text {
            text: format!("<typescript>\n{source}\n</typescript>"),
            response_meta: None,
        }],
        usage: LlmUsage {
            input_tokens,
            ..LlmUsage::default()
        },
        ..LlmResponse::default()
    }
}

/// The model: the pressure turn's cell reports more tokens than the
/// threshold; the next turn prints, then switches frames handing the baton;
/// the follow-on answers with it. Every request is kept, rendered.
fn model(requests: Arc<Mutex<Vec<String>>>) -> lash_core::facade_support::ProviderHandle {
    lash_core::testing::TestProvider::builder()
        .kind("context-budget-scripted")
        .complete(move |request: LlmRequest| {
            let requests = Arc::clone(&requests);
            async move {
                let mut seen = requests.lock().unwrap();
                let call = seen.len();
                seen.push(serde_json::to_string(&request).expect("the request renders"));
                Ok(match call {
                    0 => cell("await control.finish(\"pressure\");", 120),
                    1 => cell("console.log(\"warning observed\");", 8),
                    2 => cell(
                        &format!(
                            "await control.continue_as({{ task: \"answer from the baton\", seed: {{ baton: \"{BATON}\" }} }});"
                        ),
                        8,
                    ),
                    3 => cell(&format!("await control.finish(\"{BATON}\");"), 8),
                    call => panic!("unexpected model call {call}"),
                })
            }
        })
        .build()
        .into_handle()
}

/// An RLM core whose protocol warns past [`WARN_AT`] prompt tokens.
fn core(backend: &lash::Backend) -> lash::LashCoreBuilder {
    let mut config = lash::rlm::RlmProtocolPluginConfig::builder()
        .channel(lash::rlm::RlmChannel::Cell)
        .instruction_limit(lash::rlm::InstructionBound::instructions(1_000_000))
        .memory_limit(lash::rlm::MemoryBound::mebibytes(64))
        .build();
    config.continue_as_soft_warn_tokens = Some(WARN_AT);
    lash::LashCore::rlm_builder(
        backend.clone(),
        lash::rlm::RlmProtocolPluginFactory::new(config, lash::rlm::CellDialect::typescript())
            .with_worker_service(lash::vm::WorkerService::default()),
    )
}

/// A turn that begins past the threshold warns the model at its work
/// checkpoint toward `control.continue_as`, with one keyed status; the
/// switch's follow-on runs on a `continue_as` frame that carries only the
/// seed.
async fn a_budget_warning_reaches_the_model_and_continue_as_carries_only_the_seed(tier: Tier) {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let Some(world) = World::with_model(tier, Vec::new(), model(Arc::clone(&requests)), core).await
    else {
        return;
    };
    let session = world.session("context-budget", served::spec(1024)).await;
    served::assert_answered(OLD_INPUT, &world.send(&session, OLD_INPUT).await);
    let switch = world.send(&session, "switch now").await;
    tokio::time::timeout(WATCHDOG, async {
        loop {
            let page = session
                .committed_turns(None, std::num::NonZeroU32::new(8).unwrap())
                .await
                .unwrap();
            if page.turns.len() >= 3 {
                assert_eq!(page.turns.len(), 3, "{:#?}", page.turns);
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the follow-on commits");

    let requests = requests.lock().unwrap().clone();
    assert_eq!(
        requests.len(),
        4,
        "one pressure call, two warned calls, one follow-on call"
    );
    assert!(
        requests[1].contains("Past the frame switch threshold"),
        "{}",
        requests[1]
    );
    assert!(
        requests[1].contains("control.continue_as"),
        "{}",
        requests[1]
    );
    assert!(requests[3].contains(BATON), "{}", requests[3]);
    assert!(!requests[3].contains(OLD_INPUT), "{}", requests[3]);

    // The status is a function of recorded state (FIG-4398): the one work
    // checkpoint of the turn that began over the threshold emits it.
    let warnings = switch
        .activities
        .iter()
        .filter_map(|activity| match &activity.event {
            TurnEvent::PluginRuntime {
                event: PluginRuntimeEvent::Status { key, detail, .. },
                ..
            } if key == "rlm_context_budget_warning" => detail.clone(),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        warnings,
        vec!["120 tokens used; warn at 100; choose frame switch path".to_owned()],
        "{:#?}",
        switch.activities
    );

    let store = lash_core::store::SessionStore::new(
        world.backend.stores().session_store_factory(),
        session.session_id().clone(),
    )
    .unwrap();
    let state = lash_core::store::load_session_window_state(
        &store,
        lash_core::store::WindowSelector::Current,
    )
    .await
    .unwrap()
    .expect("the session has a head")
    .state;
    let frame = state
        .current_frame_node_id
        .clone()
        .expect("a current frame");
    let opened = state
        .session_graph
        .nodes
        .iter()
        .find(|node| node.node_id.as_str() == frame.as_str())
        .expect("the current frame's node");
    assert!(
        matches!(
            &opened.payload,
            lash_core::SessionNodePayload::FrameOpen { reason, .. } if reason.as_str() == "continue_as"
        ),
        "{:?}",
        opened.payload
    );
    world.shutdown().await;
}

tiered_laws!(a_budget_warning_reaches_the_model_and_continue_as_carries_only_the_seed);
