use super::*;

/// A7 (FIG-5226): a `Repeatable` step whose first attempt fails with a
/// known failure its pinned contract retries records a retry, starts its
/// next attempt at the run's next ordinal once the retry is due, and
/// settles from that second attempt: the engine is handed its completion,
/// never the first attempt's failure.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_repeatable_step_that_fails_retryably_once_succeeds_on_its_second_ordinal(
    backend: &Backend,
) -> LawOutcome {
    let backend = law_backend(backend)?;
    let serving = serve(&backend);
    let result = async {
        let process = root(&backend, payload(&tag("retry"), "retry")).await?;
        eventually(SETTLE, "the retried step's process ended", || async {
            Ok(terminal(&backend, &process).await?.is_some())
        })
        .await?;
        let outcome = terminal(&backend, &process).await?.unwrap_or_default();
        ensure!(
            find(&outcome, "settled") == Some(&json!("completed")),
            "the step settled as {outcome}, not completed by its retry"
        );
        let rows = backend
            .durable()
            .run_facts(&lash_durable::domain::OwnerKey::Process(process.clone()))
            .await?;
        let waits = round::PinnedWaits::read(backend.durable().as_ref(), &rows).await?;
        let fold = round::fold(&rows, &PolicyView::default(), &waits)
            .map_err(|refusal| LawBroken(refusal.to_string()))?;
        let member = fold
            .rounds()
            .next()
            .and_then(|view| view.members().first())
            .ok_or_else(|| LawBroken("the step was never admitted".to_owned()))?;
        ensure!(
            member.starts().len() == 2,
            "the step took {} attempts, not its failure and one retry",
            member.starts().len()
        );
        ensure!(
            matches!(
                member.state(),
                MemberState::Final { start, outcome: SettledOutput::Completed(_) }
                    if Some(start) == member.starts().get(1)
            ),
            "the step's final outcome {:?} is not its second ordinal's completion",
            member.state()
        );
        ensure!(
            step_entries(member.call()) == 2,
            "the step's body was entered {} times, not once per attempt",
            step_entries(member.call())
        );
        Ok(())
    }
    .await;
    serving.stop().await;
    result
}

/// A7 (FIG-5226): a step whose body parks on its completion wait records
/// `Waiting`, its process releases `waiting` holding nothing, and the host's
/// resolution of the step's key settles the step from that resolution: the
/// engine is handed it, and the parked body is never entered again.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_step_parked_on_its_wait_settles_when_the_wait_resolves(
    backend: &Backend,
) -> LawOutcome {
    let backend = law_backend(backend)?;
    let serving = serve(&backend);
    let result = async {
        let tag = tag("park");
        let process = root(&backend, payload(&tag, "park")).await?;
        eventually(
            SETTLE,
            "the parked step's process released waiting",
            || async {
                Ok(PARKED_KEYS
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .contains_key(&tag)
                    && settled_waiting(&backend, &process).await?)
            },
        )
        .await?;
        ensure!(
            terminal(&backend, &process).await?.is_none(),
            "the process ended before its parked step's wait resolved"
        );
        let key = PARKED_KEYS
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&tag)
            .cloned()
            .unwrap_or_default();
        let answer =
            waits::resolve_host(&backend, &key, Resolution::Ok(json!({ "approved": tag }))).await?;
        ensure!(
            answer == lash_durable::domain::ResolveAnswer::Resolved,
            "the host's resolution of the parked step's key answered {answer:?}"
        );
        eventually(SETTLE, "the parked step's process ended", || async {
            Ok(terminal(&backend, &process).await?.is_some())
        })
        .await?;
        let outcome = terminal(&backend, &process).await?.unwrap_or_default();
        ensure!(
            find(&outcome, "settled") == Some(&json!("completed"))
                && outcome.to_string().contains("approved"),
            "the parked step settled as {outcome}, not from its wait's resolution"
        );
        let rows = backend
            .durable()
            .run_facts(&lash_durable::domain::OwnerKey::Process(process.clone()))
            .await?;
        let waits = round::PinnedWaits::read(backend.durable().as_ref(), &rows).await?;
        let fold = round::fold(&rows, &PolicyView::default(), &waits)
            .map_err(|refusal| LawBroken(refusal.to_string()))?;
        let call = fold
            .rounds()
            .next()
            .and_then(|view| view.members().first())
            .map(|member| member.call().clone())
            .ok_or_else(|| LawBroken("the step was never admitted".to_owned()))?;
        ensure!(
            step_entries(&call) == 1,
            "the parked step's body was entered {} times",
            step_entries(&call)
        );
        Ok(())
    }
    .await;
    serving.stop().await;
    result
}
