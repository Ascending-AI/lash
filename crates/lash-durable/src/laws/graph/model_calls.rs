//! Keyed model reservations and retained sealed records.
use super::*;

/// Turn-model ordinals and nested owner keys remain distinct, including a
/// replay of the enclosing outcome transaction.
pub async fn turn_and_owned_model_calls_survive_an_idempotent_outcome_commit(
    tier: &dyn GraphTier,
    peer: &dyn DurableStore,
) -> LawOutcome {
    use crate::domain::{ModelCallId, ModelCallWrite, PromptCallKey};
    use lash_core_store::model_calls::{ModelCallEnd, ModelCallFact};
    use lash_protocol_types::{llm::types::LlmCallId, llm::types::LlmCallRecord};

    let owner = owned(tier, "model-call-keys").await?;
    let run = turn("call-key-run");
    let mut nodes = staging();
    let mut writes = Vec::new();
    let mut reservations = Vec::new();
    let mut keys = Vec::new();
    let addresses = (1..=12)
        .map(|ordinal| {
            (
                ModelCallId::Turn {
                    run: run.clone(),
                    ordinal,
                },
                format!("turn-model-{ordinal}"),
                ordinal,
            )
        })
        .chain(std::iter::once((
            ModelCallId::Owned {
                owner: run.to_string(),
                key: "tool-attempt:1/direct:1".into(),
            },
            "nested-model".to_owned(),
            0,
        )));
    let mut expected = Vec::new();
    for (address, id, ordinal) in addresses {
        let call_id = LlmCallId(id);
        let fact = ModelCallFact {
            call: ordinal,
            call_id: call_id.clone(),
            end: ModelCallEnd::Answered,
            record: LlmCallRecord {
                call_id: call_id.clone(),
                label: None,
                attempts: Vec::new(),
                replay_drops: Vec::new(),
            },
        };
        expected.push(fact.clone());
        let record = fact
            .stage(&mut nodes)
            .map_err(|error| refused("model fact", error))?;
        let key = PromptCallKey {
            session: owner.session.clone(),
            call: address,
        };
        keys.push((key.clone(), call_id.clone()));
        reservations.push(DomainWrite::ModelCallPin(
            crate::domain::ModelCallPinWrite::Reserve {
                key: key.clone(),
                call_id: call_id.clone(),
                attempt: 1,
                enclosing: None,
            },
        ));
        writes.push(DomainWrite::ModelCall(ModelCallWrite {
            key,
            call_id,
            record,
        }));
    }
    let nodes = nodes.into_nodes();
    owner_commit(tier, &owner, BTreeMap::new(), reservations).await?;
    for (key, id) in keys.iter().take(1).chain(keys.iter().rev().take(1)) {
        let pin = peer
            .model_call_pin(key)
            .await?
            .ok_or_else(|| LawBroken("the selected reservation was lost".into()))?;
        ensure!(
            pin.key == *key && pin.call_id == *id,
            "a reservation key read selected another call"
        );
        ensure!(
            peer.model_call_pins_by_id(
                &owner.session,
                &ModelCallId::turn_owner(&run),
                std::slice::from_ref(id)
            )
            .await?
                == [pin],
            "reservation key and call-id reads disagree"
        );
    }
    let missing = PromptCallKey {
        session: owner.session.clone(),
        call: ModelCallId::Turn {
            run: run.clone(),
            ordinal: 99,
        },
    };
    ensure!(
        peer.model_call_pin(&missing).await?.is_none(),
        "an unknown reservation key read unrelated history"
    );
    owner_commit(tier, &owner, nodes.clone(), writes.clone()).await?;
    for (key, _) in &keys {
        ensure!(
            peer.model_call_pin(key).await?.is_none(),
            "a sealed model call still appeared pending"
        );
    }
    let (key, id) = &keys[0];
    let reopened = owner_commit(
        tier,
        &owner,
        BTreeMap::new(),
        vec![DomainWrite::ModelCallPin(
            crate::domain::ModelCallPinWrite::Reserve {
                key: key.clone(),
                call_id: id.clone(),
                attempt: 2,
                enclosing: None,
            },
        )],
    )
    .await;
    ensure!(
        reopened.is_err(),
        "a reservation resurrected a sealed model call"
    );
    // The owned partition sorts first; turn calls are numeric across 9/10.
    expected.rotate_right(1);
    let first = peer
        .model_calls(&owner.session, &ModelCallId::turn_owner(&run))
        .await?;
    ensure!(
        first
            .iter()
            .map(|row| ModelCallFact {
                call: row.call,
                call_id: row.call_id.clone(),
                end: row.end.clone(),
                record: row.record.clone()
            })
            .collect::<Vec<_>>()
            == expected,
        "cold read lost an identity, record or numeric ordinal: {first:?}"
    );
    // MODEL-CALL-KEYED-HYDRATION: corruption in an unrelated retained body
    // cannot affect a keyed read or identity-only notification catch-up.
    let DomainWrite::ModelCall(unrelated) = &writes[0] else {
        unreachable!()
    };
    let original = nodes[&unrelated.record].bytes.to_vec();
    tier.foreign(ForeignWrite::OverwriteBlob {
        node: unrelated.record,
        bytes: vec![0],
    })
    .await;
    let wanted = first
        .last()
        .ok_or_else(|| LawBroken("nonempty model history".into()))?;
    let selected = peer
        .model_calls_by_id(
            &owner.session,
            &ModelCallId::turn_owner(&run),
            std::slice::from_ref(&wanted.call_id),
        )
        .await?;
    ensure!(
        selected == [wanted.clone()],
        "keyed model read included unrelated bodies"
    );
    ensure!(
        peer.model_call_ids(&owner.session, &ModelCallId::turn_owner(&run))
            .await?
            .len()
            == first.len(),
        "identity-only notification catch-up hydrated a record"
    );
    tier.foreign(ForeignWrite::OverwriteBlob {
        node: unrelated.record,
        bytes: original,
    })
    .await;
    owner_commit(tier, &owner, nodes, writes).await?;
    let replayed = peer
        .model_calls(&owner.session, &ModelCallId::turn_owner(&run))
        .await?;
    ensure!(
        first == replayed,
        "replaying the owner changed its sealed model calls"
    );
    exact(tier, "replayed model-call outcome").await
}
