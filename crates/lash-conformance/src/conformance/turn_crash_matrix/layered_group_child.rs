//! A layer over an effect host observes the effects of the group children the
//! host's turns open, on every engine.
//!
//! A turn's tool call runs as a group child. Where the host mints the child's
//! controller itself (the in-process tiers), the child's effects cross every
//! layer of the host's stack. A handler-driven engine (Restate) mints the
//! child's controller from the child invocation's own context instead, and
//! routes it through the stack of the host that routed the child
//! (`EffectHost::route_handler_child_controller`). Either way, a layer that
//! wraps a turn's host also wraps that turn's group children.
//!
//! The law layers only the host the runtime is built on, and runs the turn on
//! the controller the tier's runner lends, unlayered. So the layer sees no
//! effect the turn issues itself (the group open among them), and the tool
//! attempt it does see reached it along the child's path alone.

use super::*;
use pretty_assertions::assert_eq;

/// Run the reference turn over a layered host, its own controller unlayered,
/// and require the layer to have observed the group child's tool attempt, and
/// nothing the turn issued on its own controller.
#[expect(
    clippy::expect_used,
    reason = "conformance-law fixture: each result is established by the setup above"
)]
pub async fn a_host_layer_observes_its_group_childrens_effects<F, S>(
    stores: Arc<dyn crate::StoreSet>,
    make: F,
    host: Arc<dyn crate::EffectHost>,
    runner: Arc<dyn crate::ConformanceTurnRunner>,
) where
    F: Fn(&str) -> Arc<S>,
    S: RuntimePersistence + crate::store::StoreTestSupport + 'static,
{
    let scenario = "layered-group-child";
    let raw = make(scenario) as Arc<dyn RuntimePersistence>;
    let identity = ReferenceIdentity::for_scenario(scenario);
    seed_reference_ingress(&raw, &identity, scenario).await;
    let control = SeamControl::default();
    let executions = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let law_host = LawSeamHost::over(host);
    let seam = SeamLayer {
        control: control.clone(),
        executions: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        journal_faults: None,
    };
    let (reports, mut reported) = tokio::sync::mpsc::unbounded_channel();
    let attempt: crate::ConformanceTurnAttempt = {
        let stores = Arc::clone(&stores);
        let identity = identity.clone();
        let executions = Arc::clone(&executions);
        Arc::new(move |scoped| {
            let stores = Arc::clone(&stores);
            let raw = Arc::clone(&raw);
            let law_host = law_host.clone();
            let seam = seam.clone();
            let identity = identity.clone();
            let executions = Arc::clone(&executions);
            let reports = reports.clone();
            Box::pin(async move {
                law_host.route_to(&seam);
                let store = SeamStore::wrap(raw, seam.control.clone());
                let mut runtime = Box::pin(try_build_runtime_over_host(
                    Arc::clone(&stores),
                    store,
                    seam.control.clone(),
                    law_host.host(),
                    &identity,
                    TraceTool {
                        executed: executions,
                        ..TraceTool::default()
                    },
                    nominal_recovery_timings(),
                ))
                .await
                .expect("build the reference runtime");
                seam.control.clear();
                seam.control.pin_renewal_after_provider();
                // The turn's own controller is the runner's, unlayered.
                let drain = Box::pin(runtime.stream_next_queued_work(crate::TurnOptions::new(
                    tokio_util::sync::CancellationToken::new(),
                    scoped,
                )))
                .await;
                let end = crate::ConformanceTurnEnd::of(&drain);
                let _ = reports.send(drain);
                end
            })
        })
    };
    runner
        .run_turn(reference_admitted_scope(&identity), attempt)
        .await;
    let turn = reported
        .recv()
        .await
        .expect("the reference turn reported its drain")
        .map(crate::facade_support::QueuedTurnDrain::ran)
        .expect("reference turn succeeds")
        .expect("reference ingress produces a turn");
    assert_eq!(turn.assistant_output.safe_text, "trace turn complete");
    assert_eq!(
        executions.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the group child ran its tool once"
    );
    let observed: Vec<TurnSeamOperation> = control
        .trace()
        .into_iter()
        .filter(|operation| matches!(operation, TurnSeamOperation::Effect(_)))
        .collect();
    assert_eq!(
        observed,
        vec![TurnSeamOperation::Effect(EffectOperation::ToolAttempt {
            name: "trace_effect".to_string(),
        })],
        "the host's layer observes the group child's tool attempt, and none of the effects \
         the turn issued on its own, unlayered controller"
    );
}
