//! Tests of the native effect host's own group machinery: its scoped host
//! view and its supervisor. They go with the native host (FIG-3585); the
//! effect-group contract itself runs on every engine through
//! `effect_group_host_tests!`.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::conformance::StagedGroupExecutors;
use crate::*;
use crate::{GroupWakePolicy, LoserPolicy, RuntimeEffectGroup};
use pretty_assertions::assert_eq;

const SCOPE: &str = "fig1535-session";

fn child(key: &str, position: usize) -> RuntimeEffectEnvelope {
    RuntimeEffectEnvelope::new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(
                ExecutionScope::runtime_operation(SCOPE),
                format!("{key}:child:{position}"),
            )
            .expect("valid child effect address"),
            RuntimeAttribution::none(),
            "effect",
        ),
        RuntimeEffectCommand::Sleep {
            spec: lash_core::SleepSpec::For { duration_ms: 0 },
        },
    )
}

fn group(
    key: &str,
    children: usize,
    wake: GroupWakePolicy,
    disposition: LoserPolicy,
) -> RuntimeEffectGroup {
    RuntimeEffectGroup::try_new(
        RuntimeEffectInvocation::new(
            EffectAddress::new(
                ExecutionScope::runtime_operation(SCOPE),
                format!("{key}:group"),
            )
            .expect("valid group effect address"),
            RuntimeAttribution::none(),
            "group",
        ),
        key,
        (0..children).map(|position| child(key, position)).collect(),
        wake,
        disposition,
    )
    .expect("a group with at least one child assembles")
}

/// The resolver every host in this file is registered with.
///
/// Since FIG-1578 a group carries envelopes and nothing else: what runs a child
/// is the resolver its host was registered with. A test stages the executors it
/// wants under the children's replay keys and opens the group `staged` hands
/// back. One table for the file is safe — every test namespaces its own group
/// key, and a test's *second* host must answer the same routing question as its
/// first without inheriting the first's memory.
fn executors() -> Arc<StagedGroupExecutors> {
    static EXECUTORS: std::sync::OnceLock<Arc<StagedGroupExecutors>> = std::sync::OnceLock::new();
    Arc::clone(EXECUTORS.get_or_init(|| Arc::new(StagedGroupExecutors::new())))
}

fn staged(
    group: RuntimeEffectGroup,
    executors_for_children: Vec<RuntimeEffectLocalExecutor<'static>>,
) -> RuntimeEffectGroup {
    executors().stage(group, executors_for_children)
}

/// An executor that settles as soon as it is polled.
fn immediate() -> RuntimeEffectLocalExecutor<'static> {
    RuntimeEffectLocalExecutor::testing(|_| async { Ok(RuntimeEffectOutcome::Sleep) })
}

/// An executor gated on a release signal, plus the handles a test needs to
/// release it.
struct GatedChild {
    release: oneshot::Sender<()>,
}

fn gated() -> (RuntimeEffectLocalExecutor<'static>, GatedChild) {
    let (release, released) = oneshot::channel();
    let executor = RuntimeEffectLocalExecutor::testing(move |_| async move {
        let _ = released.await;
        Ok(RuntimeEffectOutcome::Sleep)
    });
    (executor, GatedChild { release })
}

/// Waits for a condition the host reaches on its own tasks, so a test never
/// depends on how many yields a settlement happens to take.
async fn until(mut condition: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("the host reaches the awaited state");
}

fn controller() -> NativeRuntimeEffectController {
    let controller = NativeRuntimeEffectController::default();
    controller
        .register_group_executors(executors() as Arc<dyn crate::GroupExecutors>)
        .expect("a fresh controller has no resolver yet");
    controller
}

/// An native host whose controller resolves grouped children through this
/// file's staging table.
fn native_host() -> crate::NativeEffectHost {
    crate::NativeEffectHost::with_native_controller(Arc::new(controller()))
}

/// The capability flag is the admission gate, and the scoped view a host
/// actually reaches groups through must answer it the same way.
///
/// A scoped wrapper that forwarded the flag but left the methods on their
/// fail-closed defaults would advertise support and then refuse every group, so
/// the flag is asserted through the same object that runs the group.
#[tokio::test]
async fn the_native_substrate_supports_groups_through_the_scoped_host_view() {
    use crate::EffectHost;

    let host = native_host();
    let scoped = host
        .scoped(admit(crate::ExecutionScope::runtime_operation(SCOPE)))
        .expect("scoped controller");
    let key = "fig1535:scoped";
    let mut handle = scoped
        .controller()
        .open_effect_group(staged(
            group(key, 1, GroupWakePolicy::All, LoserPolicy::RunToCompletion),
            vec![immediate()],
        ))
        .await
        .expect("a group opens through the scoped view");
    let settlement = scoped
        .controller()
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the scoped view serves the settlement rather than refusing it");
    assert_eq!(settlement.position, 0);
    scoped
        .controller()
        .close_effect_group(handle, LoserPolicy::RunToCompletion)
        .await
        .expect("the scoped view closes the group");
}

/// The supervisor owns its children's tasks for exactly the group's life:
/// both live in the group's task set while the loser drains, and the set is
/// gone once the last settlement reaps the group — never aborted mid-drain.
#[tokio::test]
async fn the_supervisor_owns_its_children_for_the_groups_life() {
    let controller = Arc::new(controller());
    let host = crate::NativeEffectHost::with_native_controller(Arc::clone(&controller));
    let scoped = host
        .scoped(admit(crate::ExecutionScope::runtime_operation(SCOPE)))
        .expect("a scope binds");
    let key = "fig2266:supervisor-life";
    let (slow, loser) = gated();
    let mut handle = scoped
        .controller()
        .open_effect_group(staged(
            group(key, 2, GroupWakePolicy::First, LoserPolicy::RunToCompletion),
            vec![immediate(), slow],
        ))
        .await
        .expect("the group opens");

    let settlement = scoped
        .controller()
        .await_next_settlement(&mut handle, CancellationToken::new())
        .await
        .expect("the first settlement arrives");
    assert_eq!(settlement.position, 0);
    assert_eq!(
        controller.open_group_task_count(key),
        Some(2),
        "the group still owns both child tasks while the loser drains"
    );
    scoped
        .controller()
        .close_effect_group(handle, LoserPolicy::RunToCompletion)
        .await
        .expect("the group closes");

    loser.release.send(()).expect("release the loser");
    until(|| controller.open_group_task_count(key).is_none()).await;
}
