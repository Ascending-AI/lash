//! A turn whose finish meets a refusal no retry can change ends its run with
//! that typed refusal, through a host's `send()` with the core's node serving
//! the turn (ADR 0069 §6, ADR 0078).
//!
//! - **After-turn refusal:** an after-turn callback's refusal is the run's
//!   terminal answer, as typed data: the model is asked once, the callback
//!   runs once, and the session commits no turn. A redrive under the run's
//!   id, and a handle attached by it, answer the recorded refusal and run
//!   nothing again.
//! - **After a refusal (FIG-4018):** a refused run is not left the session's
//!   unfinished run: the session's next send is admitted under a new run,
//!   which runs and commits.
#![allow(clippy::expect_used, clippy::unwrap_used)]

#[path = "support/served.rs"]
mod served;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use served::{Tier, World};

/// Long enough for a refused run to end, far short of a session that
/// retries its pass until it parks.
const ENDS_WITHIN: std::time::Duration = std::time::Duration::from_secs(60);

/// An after-turn callback that counts its calls and refuses with `fault`.
fn refusing_after_turn(
    calls: &Arc<AtomicUsize>,
    fault: fn() -> lash_core::PluginError,
) -> Arc<dyn lash_core::plugin::PluginFactory> {
    refusing_first_after_turns(calls, usize::MAX, fault)
}

/// An after-turn callback that counts its calls and refuses its first
/// `refusals` with `fault`.
fn refusing_first_after_turns(
    calls: &Arc<AtomicUsize>,
    refusals: usize,
    fault: fn() -> lash_core::PluginError,
) -> Arc<dyn lash_core::plugin::PluginFactory> {
    let calls = Arc::clone(calls);
    Arc::new(lash_core::plugin::StaticPluginFactory::new(
        lash_core::plugin::PluginDeclaration::initial("after-turn-refusal"),
        lash_core::facade_support::PluginSpec::new().with_after_turn(
            lash_core::hook_key!("after-turn-refusal"),
            Arc::new(move |_| {
                let calls = Arc::clone(&calls);
                Box::pin(async move {
                    if calls.fetch_add(1, Ordering::SeqCst) < refusals {
                        return Err(fault());
                    }
                    Ok(Default::default())
                })
            }),
        ),
    ))
}

/// A plugin's refusal over the turn it finalizes.
fn invoke_refusal() -> lash_core::PluginError {
    lash_core::PluginError::Invoke("the after-turn callback refuses this turn".to_owned())
}

/// A refusal under a plugin-minted code the plugin classes an outcome.
fn minted_refusal() -> lash_core::PluginError {
    lash_core::PluginError::Runtime(lash_core::RuntimeError::foreign(
        "after-turn-plugin.refused",
        lash_core::TurnFailureCause::Outcome,
        "the after-turn callback refuses this turn",
    ))
}

/// The run of a turn whose after-turn callback refuses with `fault` ends
/// `Refused`: the callback and the model each ran once, and no turn
/// committed.
async fn refused_terminal(tier: Tier, name: &str, fault: fn() -> lash_core::PluginError) {
    let calls = Arc::new(AtomicUsize::new(0));
    let Some(world) = World::new(tier, |backend| {
        lash::LashCore::standard_builder(backend.clone()).plugin(refusing_after_turn(&calls, fault))
    })
    .await
    else {
        return;
    };
    world.script(
        name,
        vec![served::response(vec![lash_core::LlmOutputPart::Text {
            text: "answered".to_owned(),
            response_meta: None,
        }])],
    );
    let session = world.session(name, served::spec(8)).await;
    let run = lash::TurnId::parse(name).unwrap();
    let send = || async {
        tokio::time::timeout(ENDS_WITHIN, async {
            session
                .send(lash::TurnInput::text(name))
                .id(run.clone())
                .await
                .expect("the input is accepted")
                .outcome()
                .await
        })
        .await
        .unwrap_or_else(|_| panic!("the refused run of `{name}` never ended"))
        .expect("the send answers")
    };
    let assert_refused = |outcome: &lash::SendOutcome, how: &str| {
        let lash::SendOutcome::Refused { refusal, .. } = outcome else {
            panic!("{how} answers the callback's refusal: {outcome:?}");
        };
        assert!(
            refusal.message.contains("refuses this turn"),
            "{how} answers the callback's refusal: {refusal:?}"
        );
    };
    assert_refused(&send().await, "the send");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "the callback ran once");
    assert_eq!(world.requests(name).len(), 1, "the model was asked once");
    // A redrive under the run's id answers the recorded refusal and runs
    // nothing again; so does a handle attached by the id.
    assert_refused(&send().await, "a redrive");
    let attached = session.attach_id(run.clone()).outcome().await.unwrap();
    assert_refused(&attached, "an attached handle");
    assert_eq!(calls.load(Ordering::SeqCst), 1, "nothing ran again");
    assert_eq!(
        world.requests(name).len(),
        1,
        "the model was not asked again"
    );
    let head = world
        .backend
        .stores()
        .session_store_factory()
        .load_session_head_meta(session.session_id())
        .await
        .unwrap();
    assert!(
        head.as_ref().is_none_or(|head| head.leaf_node_id.is_none()),
        "no turn committed: {head:?}"
    );
    world.shutdown().await;
}

async fn an_after_turn_refusal_ends_the_run_with_its_typed_refusal(tier: Tier) {
    refused_terminal(tier, "after-turn-refusal", invoke_refusal).await;
}

async fn a_minted_after_turn_refusal_ends_the_run_with_its_typed_refusal(tier: Tier) {
    refused_terminal(tier, "after-turn-minted-refusal", minted_refusal).await;
}

/// The session's next send after a refused run is admitted under a new run,
/// which runs and commits.
async fn the_send_after_a_refused_run_executes_a_new_run(tier: Tier) {
    const REFUSED: &str = "the refused run";
    const NEXT: &str = "the next run";
    let calls = Arc::new(AtomicUsize::new(0));
    let Some(world) = World::new(tier, |backend| {
        lash::LashCore::standard_builder(backend.clone()).plugin(refusing_first_after_turns(
            &calls,
            1,
            invoke_refusal,
        ))
    })
    .await
    else {
        return;
    };
    for name in [REFUSED, NEXT] {
        world.script(
            name,
            vec![served::response(vec![lash_core::LlmOutputPart::Text {
                text: format!("{name} answered"),
                response_meta: None,
            }])],
        );
    }
    let session = world.session("after-a-refusal", served::spec(8)).await;
    let refused = tokio::time::timeout(ENDS_WITHIN, async {
        session
            .send(lash::TurnInput::text(REFUSED))
            .await
            .unwrap()
            .outcome()
            .await
            .unwrap()
    })
    .await
    .expect("the refused run ends");
    assert!(
        matches!(refused, lash::SendOutcome::Refused { .. }),
        "{refused:?}"
    );
    served::assert_answered(NEXT, &world.send(&session, NEXT).await);
    assert_eq!(
        world.requests(NEXT).len(),
        1,
        "the next run asked the model"
    );
    let page = session
        .committed_turns(None, std::num::NonZeroU32::new(8).unwrap())
        .await
        .unwrap();
    assert_eq!(page.turns.len(), 1, "only the next run committed a turn");
    world.shutdown().await;
}

tiered_laws!(
    the_send_after_a_refused_run_executes_a_new_run,
    an_after_turn_refusal_ends_the_run_with_its_typed_refusal,
    a_minted_after_turn_refusal_ends_the_run_with_its_typed_refusal,
);
