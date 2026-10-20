use super::*;

use crate::testing::DurableCommitHook;
use lash_durable::domain::{OwnerKey, RunFactKind, RunFactWrite};
use lash_durable::{CommitLabel, DomainWrite};

/// How many outcomes `tx` records.
fn outcomes(tx: &lash_durable::ActorTx) -> usize {
    tx.domain()
        .iter()
        .filter(|write| {
            matches!(
                write,
                DomainWrite::RunFact(RunFactWrite::Append {
                    kind: RunFactKind::XOutcome,
                    ..
                })
            )
        })
        .count()
}

/// Every step `process` admitted entered its body exactly once and
/// completed, and there are `count` of them.
async fn each_ran_once(backend: &Backend, process: &ProcessId, count: usize) -> LawOutcome {
    let rows = backend
        .durable()
        .run_facts(&OwnerKey::Process(process.clone()))
        .await?;
    let waits = round::PinnedWaits::read(backend.durable().as_ref(), &rows).await?;
    let fold = round::fold(&rows, &PolicyView::default(), &waits)
        .map_err(|refusal| LawBroken(refusal.to_string()))?;
    let members: Vec<_> = fold
        .rounds()
        .flat_map(|view| view.members().to_vec())
        .collect();
    ensure!(
        members.len() == count,
        "the process admitted {} steps, not {count}",
        members.len()
    );
    for member in &members {
        ensure!(
            matches!(
                member.state(),
                MemberState::Final {
                    outcome: SettledOutput::Completed(_),
                    ..
                }
            ),
            "step {} ended {:?}, not completed",
            member.call(),
            member.state()
        );
        ensure!(
            step_entries(member.call()) == 1,
            "step {}'s body was entered {} times, not once",
            member.call(),
            step_entries(member.call())
        );
    }
    Ok(())
}

/// FIG-7046: a process owner collects every step completion already ready
/// when the first one wakes it, without waiting for one still running, and
/// commits them together, at most the group commit's member bound to one
/// outcome transaction. Ten steps whose bodies all finish together, under
/// a bound of four and no batch window, settle in three outcome commits of
/// four, four and two, each body entered once; collecting one completion
/// per wake commits ten of one.
///
/// # Errors
///
/// The first rule broken.
pub async fn ready_step_completions_commit_together_up_to_the_member_bound(
    backend: &Backend,
) -> LawOutcome {
    const STEPS: usize = 10;
    let mut settings = settings();
    settings.group_commit.max_members = 4;
    settings.group_commit.window = Duration::ZERO;
    let backend = Backend::assemble(BackendParts {
        formats: Vec::new(),
        stores: backend.stores(),
        settings,
        engines: vec![Arc::new(LawEngine { version: 0 })],
        providers: Arc::new(lash_execution_contracts::projection::NoProjectionProviders),
    })
    .map_err(|error| LawBroken(error.to_string()))?;
    let tag = tag("fan");
    READY_GATES
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .insert(tag.clone(), Arc::new(tokio::sync::Barrier::new(STEPS)));
    let hook = DurableCommitHook::over(Arc::clone(backend.durable()));
    let process = root(
        &backend,
        json!({ "tag": tag, "act": "fan", "steps": STEPS }),
    )
    .await?;
    let key = actor(&process)?;
    let batches: Arc<Mutex<Vec<usize>>> = Arc::default();
    {
        let batches = Arc::clone(&batches);
        hook.arm(move |tx, label| {
            if label == CommitLabel::STEP_OUTCOME && tx.actor() == &key {
                batches
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .push(outcomes(tx));
            }
            std::future::ready(Ok(()))
        });
    }
    let serving = serve_over(&backend, NODE, Vec::new(), Arc::new(hook));
    let result = async {
        eventually(SETTLE, "the fanned-out process ended", || async {
            Ok(terminal(&backend, &process).await?.is_some())
        })
        .await?;
        let batches = batches
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        ensure!(
            batches == [4, 4, 2],
            "ten ready completions committed as outcome batches {batches:?}, not [4, 4, 2]"
        );
        each_ran_once(&backend, &process, STEPS).await
    }
    .await;
    serving.stop().await;
    result
}

/// FIG-7046: a process owner keeps what its own acknowledged commits wrote
/// and reads one fresh view a pass. A ten-step chain hydrates no engine
/// state, reads no execution root, no process row and no clock of its own
/// beyond each body's one, and restores its facts once; a commit whose
/// acknowledgement is lost moves the actor past what the owner holds, and
/// the owner reads the runs in flight again, once, settling every step
/// once.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_step_chain_restores_its_state_once_and_reloads_after_a_lost_acknowledgement(
    backend: &Backend,
) -> LawOutcome {
    const STEPS: u64 = 10;
    let backend = law_backend(backend)?;
    let hook = DurableCommitHook::over(Arc::clone(backend.durable()));
    let tag = tag("chain");
    let process = root(
        &backend,
        json!({ "tag": tag, "act": "chain", "steps": STEPS }),
    )
    .await?;
    let key = actor(&process)?;
    let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
    {
        let store = Arc::clone(backend.durable());
        hook.arm(move |tx, label| {
            let lose = label == CommitLabel::STEP_OUTCOME
                && tx.actor() == &key
                && !lost.swap(true, Ordering::SeqCst);
            let tx = tx.clone();
            let store = Arc::clone(&store);
            async move {
                if lose {
                    store.commit(tx, label).await?;
                    return Err(DurableError::AckLost { label });
                }
                Ok(())
            }
        });
    }
    let serving = serve_over(&backend, NODE, Vec::new(), Arc::new(hook.clone()));
    let result = async {
        eventually(SETTLE, "the chained process ended", || async {
            Ok(terminal(&backend, &process).await?.is_some())
        })
        .await?;
        let outcome = terminal(&backend, &process).await?.unwrap_or_default();
        ensure!(
            find(&outcome, "settled") == Some(&json!(STEPS)),
            "the chain ended as {outcome}, not after its {STEPS} steps"
        );
        for (method, expected) in [
            ("hydrate", 0),
            ("exec_root", 0),
            ("process", 0),
            ("run_facts", 1),
            ("run_facts_of", 1),
            ("now", STEPS),
        ] {
            ensure!(
                hook.calls(method) == expected,
                "the chain's owner called `{method}` {} times, not {expected}",
                hook.calls(method)
            );
        }
        each_ran_once(&backend, &process, usize::try_from(STEPS).unwrap_or(0)).await
    }
    .await;
    serving.stop().await;
    result
}
