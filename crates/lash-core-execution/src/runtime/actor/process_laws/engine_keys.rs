//! The laws of engine-pinned keys (FIG-5510): a key a process engine pins
//! is a wait like a parked call's. The process records that it waits on
//! it, and a host lists it from the process's wait rows, never from state
//! the node that pinned it kept in memory.

use super::*;

/// The key `process`'s engine pinned, which is the only one it has pending.
async fn only_key(
    backend: &Backend,
    process: &ProcessId,
) -> Result<waits::PinnedEngineKey, LawBroken> {
    let mut keys = waits::pinned_keys(backend, process).await?;
    ensure!(
        keys.len() == 1,
        "process {process} lists {} pending engine keys, not one",
        keys.len()
    );
    Ok(keys.remove(0))
}

/// A process whose engine awaits a key it pinned releases as waiting with a
/// `waiting` fact that names the key, and the fact ends with the wait.
///
/// # Errors
///
/// The first rule broken.
pub async fn an_awaited_engine_key_records_a_waiting_fact(backend: &Backend) -> LawResult {
    let backend = law_backend(backend)?;
    let serving = serve(&backend);
    let result = async {
        let process = root(&backend, payload(&tag("key-fact"), "key")).await?;
        eventually(SETTLE, "the process released waiting on its key", || {
            settled_waiting(&backend, &process)
        })
        .await?;
        let waiting = record(&backend, &process).await?;
        let key = crate::WaitKind::Key {
            name: KeyName(PEER_KEY.to_owned()),
        };
        ensure!(
            matches!(waiting.waits(), [wait] if wait.kind == key),
            "a process awaiting its engine key records the waits {:?}",
            waiting.waits()
        );
        let pinned = only_key(&backend, &process).await?;
        let answer =
            waits::resolve_host(&backend, pinned.key.as_str(), Resolution::Ok(json!("told")))
                .await?;
        ensure!(
            answer == waits::ResolveAnswer::Resolved,
            "resolving the engine key answered {answer:?}"
        );
        eventually(
            SETTLE,
            "the process ended once its key resolved",
            || async { Ok(terminal(&backend, &process).await?.is_some()) },
        )
        .await?;
        let ended = record(&backend, &process).await?;
        ensure!(
            ended.waits().is_empty(),
            "the ended process still records the waits {:?}",
            ended.waits()
        );
        Ok(())
    }
    .await;
    serving.stop().await;
    result
}

/// A pending engine key is discovered from the process's wait rows. A
/// backend assembled anew over the same stores, as a restarted operating
/// system process is, lists the key before it runs anything, with the name
/// and deadline it was pinned under; and the key it lists resolves the wait
/// on another node, which takes the process over without the engine being
/// handed `KeyPinned` again.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_pinned_engine_key_is_listed_from_its_wait_after_a_restart_and_a_handover(
    backend: &Backend,
) -> LawResult {
    let first = law_backend(backend)?;
    let serving = serve(&first);
    let tag = tag("key-restart");
    let pinned = async {
        let process = root(&first, payload(&tag, "key")).await?;
        eventually(SETTLE, "the process released waiting on its key", || {
            settled_waiting(&first, &process)
        })
        .await?;
        let pinned = only_key(&first, &process).await?;
        Ok::<_, LawBroken>((process, pinned))
    }
    .await;
    serving.stop().await;
    let (process, pinned) = pinned?;
    ensure!(
        pinned.name.0 == PEER_KEY && pinned.process == process && pinned.deadline.is_none(),
        "the key is listed as `{}` of {} due {:?}",
        pinned.name.0,
        pinned.process,
        pinned.deadline
    );

    let restarted = law_backend(backend)?;
    ensure!(
        only_key(&restarted, &process).await? == pinned,
        "a restarted node lists another key than the one the engine pinned"
    );

    let successor = serve_as(&restarted, "process-laws-successor");
    let result = async {
        let answer = waits::resolve_host(
            &restarted,
            pinned.key.as_str(),
            Resolution::Ok(json!("told")),
        )
        .await?;
        ensure!(
            answer == waits::ResolveAnswer::Resolved,
            "resolving the listed key answered {answer:?}"
        );
        eventually(SETTLE, "the successor ended the process", || async {
            Ok(terminal(&restarted, &process).await?.is_some())
        })
        .await?;
        let outcome = terminal(&restarted, &process).await?.unwrap_or_default();
        ensure!(
            find(&outcome, "resolved") == Some(&json!("told")),
            "the process ended with {outcome}, not the key's resolution"
        );
        let pins = advances(&tag)
            .iter()
            .filter(|event| event.starts_with("KeyPinned"))
            .count();
        ensure!(pins == 1, "the engine was handed `KeyPinned` {pins} times");
        ensure!(
            waits::pinned_keys(&restarted, &process).await?.is_empty(),
            "a resolved key is still listed"
        );
        Ok(())
    }
    .await;
    successor.stop().await;
    result
}
