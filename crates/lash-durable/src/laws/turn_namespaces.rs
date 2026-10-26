//! A run's plugin namespace rows (FIG-5301).

use super::{LABEL, LawBroken, LawOutcome, create, ensure, node, session};
use crate::domain::{DomainRefusal, RunValuesWrite, TurnNamespaceWrite, TurnWrite};
use crate::{DomainWrite, DurableError, DurableStore};
use lash_core_store::plugin_state::{NamespaceBody, NamespaceEntry, PluginNamespaceState};
use lash_core_store::store::{
    AdmittedTurnRows, ControlIntentId, RunAdmissionRecord, RunTerminalCause,
};
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::TurnId;

/// A namespace at `generation` holding `value` under `key`: its entry and
/// its values body.
fn namespace(generation: u64, value: &str) -> (NamespaceEntry, NamespaceBody) {
    let values = std::collections::BTreeMap::from([("key".to_owned(), value.into())]);
    let body = NamespaceBody::encode(&values);
    let entry = PluginNamespaceState {
        generation,
        ..PluginNamespaceState::default()
    }
    .entry(body.values.clone());
    (entry, body)
}

/// A run's typed namespace writes have one body per current value: Body
/// replaces it, Held keeps it for metadata changes, and Base clears it.
/// A terminal drops the overlay and refuses subsequent namespace writes.
///
/// # Errors
///
/// The first rule broken.
pub async fn a_runs_namespace_write_modes_keep_only_current_values_and_end_with_it(
    store: &dyn DurableStore,
) -> LawOutcome {
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
    let write = |entry: NamespaceEntry, values: RunValuesWrite| async move {
        let mut tx = store.begin(actor, epoch).await?;
        tx.write_namespaces(
            id,
            run,
            &[TurnNamespaceWrite {
                plugin: "memory".to_owned(),
                entry,
                values,
            }],
        )
        .map_err(DurableError::from)?;
        store.commit(tx, LABEL).await
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
            took: AdmittedTurnRows::Inputs {
                ids: lash_core_store::store::AdmittedInputIds::one(
                    lash_vocabulary::identity::InputId::from("namespace-batch"),
                ),
            },
            trace: None,
        },
        turn_deadline: None,
    })
    .await?;
    let (first, first_body) = namespace(1, "first");
    write(first.clone(), RunValuesWrite::Body(first_body.clone())).await?;
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
    write(refused.clone(), RunValuesWrite::Held).await?;
    let read = rows().await?;
    ensure!(
        read == [(
            "memory".to_owned(),
            refused.clone(),
            Some(first_body.clone())
        )],
        "a held body read back {read:?}"
    );

    let (second, second_body) = namespace(3, "second");
    write(second.clone(), RunValuesWrite::Body(second_body.clone())).await?;
    let read = rows().await?;
    ensure!(
        read == [("memory".to_owned(), second.clone(), Some(second_body))],
        "a replaced body read back {read:?}"
    );

    let base = NamespaceEntry {
        generation: 4,
        ..first.clone()
    };
    write(base.clone(), RunValuesWrite::Base).await?;
    let read = rows().await?;
    ensure!(
        read == [("memory".to_owned(), base.clone(), None)],
        "returning to base kept an earlier body: {read:?}"
    );
    let base_metadata = NamespaceEntry {
        generation: 5,
        ..base
    };
    write(base_metadata.clone(), RunValuesWrite::Base).await?;
    let read = rows().await?;
    ensure!(
        read == [("memory".to_owned(), base_metadata, None)],
        "base metadata must keep the body absent: {read:?}"
    );
    write(first.clone(), RunValuesWrite::Body(first_body.clone())).await?;
    let read = rows().await?;
    ensure!(
        read == [("memory".to_owned(), first.clone(), Some(first_body.clone()))],
        "leaving base must install the current body: {read:?}"
    );

    commit(TurnWrite::Terminal {
        session: id.clone(),
        run: run.clone(),
        cause: Box::new(RunTerminalCause::OperatorCancelled {
            intent: ControlIntentId::from_sequence(1),
        }),
        head_revision: None,
        transcript: None,
    })
    .await?;
    let read = rows().await?;
    ensure!(
        read.is_empty(),
        "the cancelled run's rows outlived it: {read:?}"
    );
    match (write(first.clone(), RunValuesWrite::Body(first_body.clone()))).await {
        Err(DurableError::Domain(DomainRefusal::TurnNotOpen { .. })) => {}
        other => {
            return Err(LawBroken(format!(
                "an ended run recorded a namespace row: {other:?}"
            )));
        }
    }
    Ok(())
}
