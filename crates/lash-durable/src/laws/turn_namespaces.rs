//! A run's plugin namespace rows (FIG-5301).

use std::sync::Arc;

use super::{LABEL, LawBroken, LawResult, create, ensure, node, session};
use crate::domain::{DomainRefusal, TurnNamespace, TurnWrite};
use crate::{DomainWrite, DurableError, DurableStore};
use lash_core_store::plugin_state::{NamespaceBody, NamespaceEntry, PluginNamespaceState};
use lash_core_store::store::{
    AdmittedTurnRows, ControlIntentId, RunAdmissionRecord, RunTerminalCause,
};
use lash_sansio::{SessionId, TurnId};

/// A namespace at `generation` holding `value` under `key`: its entry and
/// its values body.
fn namespace(generation: u64, value: &str) -> (NamespaceEntry, Arc<[u8]>) {
    let values = std::collections::BTreeMap::from([("key".to_owned(), value.into())]);
    let body = NamespaceBody::encode(&values);
    let entry = PluginNamespaceState {
        generation,
        ..PluginNamespaceState::default()
    }
    .entry(body.values);
    (entry, body.bytes)
}

/// An unfinished run's namespace rows hold what its commits wrote: a row
/// written without a body keeps the body it holds, and one with a body
/// replaces it. The run's terminal drops them with its phase row, so a
/// cancelled run's changes are gone and its head untouched; and a run that
/// ended records no row.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_runs_namespace_rows_hold_its_bodies_and_end_with_it(
    store: &dyn DurableStore,
) -> LawResult {
    let id = SessionId::try_from("namespace-session".to_owned())
        .map_err(|_| LawBroken("a constant session id".into()))?;
    let run = TurnId::try_from("namespace-turn".to_owned())
        .map_err(|_| LawBroken("a constant turn id".into()))?;
    let actor = session(id.as_str())?;
    create(store, std::slice::from_ref(&actor)).await?;
    let owner = node(store, "namespace-owner").await?;
    let epoch = store.claim(&owner, 1).await?[0].epoch;
    let (actor, id, run) = (&actor, &id, &run);
    let commit = |write: TurnWrite| async move {
        let mut tx = store.begin(actor, epoch).await?;
        tx.write(DomainWrite::Turn(write));
        store.commit(tx, LABEL).await
    };
    let write = |entry: &NamespaceEntry, body: Option<&Arc<[u8]>>| TurnWrite::Namespaces {
        session: id.clone(),
        run: run.clone(),
        namespaces: vec![TurnNamespace {
            plugin: "memory".to_owned(),
            entry: entry.clone(),
            body: body.cloned(),
        }],
    };
    let rows = || async move {
        let rows = store.turn_namespaces(id, run).await?;
        Ok::<_, LawBroken>(
            rows.into_iter()
                .map(|row| (row.plugin, row.entry, row.body))
                .collect::<Vec<_>>(),
        )
    };

    commit(TurnWrite::Admit {
        session: id.clone(),
        run: run.clone(),
        admission: RunAdmissionRecord::Turn {
            took: AdmittedTurnRows::Batch {
                id: lash_sansio::BatchId::from("namespace-batch"),
            },
        },
        turn_deadline: None,
    })
    .await?;
    let (first, first_body) = namespace(1, "first");
    commit(write(&first, Some(&first_body))).await?;
    let read = rows().await?;
    ensure!(
        read == [("memory".to_owned(), first.clone(), Some(first_body.clone()))],
        "the first row read back {read:?}"
    );

    // A refusal moves the metadata alone: the row keeps its body.
    let refused = NamespaceEntry {
        generation: 2,
        ..first.clone()
    };
    commit(write(&refused, None)).await?;
    let read = rows().await?;
    ensure!(
        read == [(
            "memory".to_owned(),
            refused.clone(),
            Some(first_body.clone())
        )],
        "a bodiless row read back {read:?}"
    );

    let (second, second_body) = namespace(3, "second");
    commit(write(&second, Some(&second_body))).await?;
    let read = rows().await?;
    ensure!(
        read == [("memory".to_owned(), second.clone(), Some(second_body))],
        "a replaced body read back {read:?}"
    );

    commit(TurnWrite::Terminal {
        session: id.clone(),
        run: run.clone(),
        cause: Box::new(RunTerminalCause::OperatorCancelled {
            intent: ControlIntentId::from_sequence(1),
        }),
        head_revision: None,
    })
    .await?;
    let read = rows().await?;
    ensure!(
        read.is_empty(),
        "the cancelled run's rows outlived it: {read:?}"
    );
    match commit(write(&first, Some(&first_body))).await {
        Err(DurableError::Domain(DomainRefusal::TurnNotOpen { .. })) => {}
        other => {
            return Err(LawBroken(format!(
                "an ended run recorded a namespace row: {other:?}"
            )));
        }
    }
    Ok(())
}
