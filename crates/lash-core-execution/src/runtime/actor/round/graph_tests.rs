#![allow(clippy::expect_used, clippy::unwrap_used)]

use lash_core_store::tool_run::Material;
use std::collections::BTreeMap;

use lash_core_store::checkpoint_graph::{
    DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphDecoder, GraphRefusal,
    GraphStaging, NodeId, parse_envelope,
};
use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::{AttemptOutcome, MaterialOwner, MaterialRole};

use super::*;
use crate::{ModelToolReturn, ModelToolReturnPart, ToolCallId, ToolCallOutput};

fn staging() -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)))
}

/// What an immediate collection leaves: only the nodes `root` reaches.
fn collected(staging: &GraphStaging, root: NodeId) -> BTreeMap<NodeId, EncodedNode> {
    let mut kept = BTreeMap::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        if kept.contains_key(&id) {
            continue;
        }
        let node = staging
            .get(&id)
            .expect("a reachable node is staged")
            .clone();
        pending.extend(
            parse_envelope(&node.bytes, Some(id))
                .unwrap()
                .children
                .iter(),
        );
        kept.insert(id, node);
    }
    kept
}

fn owner() -> MaterialOwner {
    MaterialOwner::Run {
        opener: EffectOpener::turn("session", "turn"),
    }
}

fn completed(output: ToolCallOutput, model_return: ModelToolReturn) -> CompletedCall {
    CompletedCall {
        call_id: ToolCallId::fixture("call"),
        provider_call_id: Some("provider-7".to_owned()),
        tool_name: "query".to_owned(),
        args: serde_json::json!({"q": "rows"}),
        output,
        model_return,
        intent_outcomes: Vec::new(),
        replay: None,
    }
}

/// PC-OUTCOME (FIG-5887 §4, §11): a settled call is one outcome node, which
/// the run fact that settles it names by its material reference. After an
/// immediate collection under that fact alone, the outcome loads whole and
/// rebuilds the exact presentation the model saw, cut by retention, from
/// the typed output; the large answer is stored once, never beside a
/// rendered copy; and a fact whose reference names another node is refused.
#[test]
fn a_settled_call_is_one_outcome_node_that_rebuilds_its_presentation_after_collection() {
    let answer = "r".repeat(40_000);
    let output = ToolCallOutput::success(serde_json::json!({"rows": answer}));
    let mut presented = ModelToolReturn::from_output("query".to_owned(), &output);
    let rendered = presented.parts[0].visible_text().unwrap().to_owned();
    presented.parts = vec![ModelToolReturnPart::Retained(crate::RetainedOutput {
        reference: lash_sansio::AttachmentRef::new(
            lash_sansio::AttachmentId::parse("cd".repeat(32)).unwrap(),
            lash_sansio::MediaType::parse("text/plain").unwrap(),
            rendered.len() as u64,
            None,
            None,
        ),
        witness: format!("{}\n[output retained]", &rendered[..64]),
    })];
    let call = completed(output, presented.clone());

    let mut staging = staging();
    let outcome = CallOutcome::of(&call, None, 3);
    let node = outcome.stage(&mut staging).unwrap();
    // The borrowed encoder keeps the owned codec's complete canonical graph.
    let mut expected = GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)));
    let crate::ToolCallOutcome::Success(value) = &outcome.output.outcome else {
        panic!("successful fixture")
    };
    let answer_node = expected
        .encode_value(NodeKind::FROZEN_VALUE, &FrozenToolValue(value.clone()))
        .unwrap();
    let body_node = expected
        .encode_value(
            NodeKind::CALL_BODY,
            &call_body::StoredBody {
                record: crate::ToolCallRecord {
                    call_id: outcome.call_id.clone(),
                    provider_call_id: outcome.provider_call_id.clone(),
                    tool: outcome.tool_name.clone(),
                    args: outcome.args.clone(),
                    output: outcome.output.clone(),
                },
                answer: Some(answer_node),
            },
        )
        .unwrap();
    let presentation_node = expected
        .encode_value(NodeKind::PRESENTATION, &outcome.presentation)
        .unwrap();
    let expected_root = expected
        .encode_value(
            NodeKind::CALL_OUTCOME,
            &StoredOutcome {
                body: body_node,
                admission: outcome.admission,
                attempt: outcome.attempt,
                replay: outcome.replay.clone(),
                intent_outcomes: outcome.intent_outcomes.clone(),
                presentation: presentation_node,
            },
        )
        .unwrap();
    assert_eq!(node, expected_root);
    assert_eq!(staging.nodes(), expected.nodes());
    let json = serde_json::json!({"value": "encoded by borrow"});
    assert_eq!(
        stage_value(&mut expected, &json).unwrap(),
        MaterialNode::Value(
            expected
                .encode_value(NodeKind::FROZEN_VALUE, &FrozenJson(json))
                .unwrap()
        ),
    );
    assert_eq!(
        outcome.stage(&mut staging).unwrap(),
        node,
        "one outcome node"
    );
    let material = MaterialNode::Outcome(node);
    let named = material.material(owner(), MaterialRole::AttemptOutput);
    let fact = RoundFact::Outcome {
        start: 3,
        output: SettledFact {
            outcome: AttemptOutcome::Completed(named.reference().clone()),
            material: Some(material),
        },
        state: Vec::new(),
        ended_at_ms: Some(0),
        pages: None,
    };
    let root = fact.stage(&mut staging).unwrap();

    let kept = collected(&staging, root);
    let stored: u64 = kept.values().map(|node| node.bytes.len() as u64).sum();
    assert!(
        stored < answer.len() as u64 + 4_096,
        "the answer is stored once: {stored} bytes"
    );
    let mut decoder = GraphDecoder::new(&kept, DecodeLimits::within_total(1 << 20));
    let RoundFact::Outcome { output, .. } = RoundFact::load(&mut decoder, root).unwrap() else {
        panic!("an outcome fact");
    };
    let MaterialNode::Outcome(loaded) = output.material.unwrap() else {
        panic!("a call's material is its outcome");
    };
    let loaded = CallOutcome::load(&mut decoder, loaded).unwrap();
    assert_eq!(loaded, outcome);
    assert_eq!(loaded.presented().unwrap(), presented);
    assert_eq!(
        serde_json::to_value(loaded.completed(call.args.clone()).unwrap()).unwrap(),
        serde_json::to_value(&call).unwrap()
    );

    // A record whose reference was minted over another node is refused.
    let other = stage_value(&mut staging, &serde_json::json!(1)).unwrap();
    let forged = RoundFact::Outcome {
        start: 3,
        output: SettledFact {
            outcome: AttemptOutcome::Completed(named.reference().clone()),
            material: Some(other),
        },
        state: Vec::new(),
        ended_at_ms: Some(0),
        pages: None,
    };
    assert_eq!(
        forged.stage(&mut staging),
        Err(GraphRefusal::PublicationMismatch)
    );
}

/// Every run fact variant round-trips through its node with its material
/// checked, and a stopped attempt names no material.
#[test]
fn every_round_fact_round_trips_through_its_node() {
    let mut staging = staging();
    let parked = stage_value(&mut staging, &serde_json::json!({"wait": "w"})).unwrap();
    let waiting = parked
        .material(owner(), MaterialRole::AttemptOutput)
        .parked("w".to_owned());
    let report = ReportView::default().stage(&mut staging).unwrap();
    let request = Material::journal_local(owner(), MaterialRole::PreparedRequest, "r".to_owned());
    for fact in [
        RoundFact::Admit {
            members: vec![AdmitMember::Pinned(Box::new(PinnedMember {
                call: ToolCallId::fixture("a"),
                tool: "query".to_owned(),
                request: request.reference().clone(),
                policy: lash_sansio::ExecutionPolicy::Once,
                limit_expires_at_ms: 9,
                limit_max_slice_ms: 4,
                wait_id: Some("ab".to_owned()),
                trace: None,
                declaration: None,
                binding: None,
            }))],
        },
        RoundFact::Start {
            member: 0,
            attempt: 1,
        },
        RoundFact::Wait {
            start: 1,
            output: SettledFact {
                outcome: AttemptOutcome::Waiting(waiting.named().clone()),
                material: Some(parked),
            },
        },
        RoundFact::Retry {
            start: 1,
            output: SettledFact {
                outcome: AttemptOutcome::Interrupted,
                material: None,
            },
            due_at_ms: -5,
        },
        RoundFact::Present,
        RoundFact::CellReport { report },
    ] {
        let node = fact.stage(&mut staging).unwrap();
        let mut decoder = GraphDecoder::new(&staging, DecodeLimits::within_total(1 << 20));
        assert_eq!(RoundFact::load(&mut decoder, node).unwrap(), fact);
    }
    let stray = RoundFact::Retry {
        start: 1,
        output: SettledFact {
            outcome: AttemptOutcome::Interrupted,
            material: Some(parked),
        },
        due_at_ms: 0,
    };
    assert_eq!(
        stray.stage(&mut staging),
        Err(GraphRefusal::PublicationMismatch)
    );
}

/// FIG-6171: the turn and live outcome share one exact archival body, while
/// pins/presentation can be reclaimed. Typed payloads retain their original
/// material digest and reject a body that would change its request bytes.
#[test]
fn the_archival_call_body_is_shared_and_its_payload_digest_binds_the_request() {
    let output = ToolCallOutput::success(serde_json::json!({"answer": "a".repeat(70_000)}));
    let presented = ModelToolReturn::from_output("query".to_owned(), &output);
    let mut call = completed(output, presented);
    call.args = serde_json::json!({"query": "q".repeat(70_000)});
    let mut staged = staging();
    let admission = staged
        .encode_value(
            lash_core_store::checkpoint_graph::NodeKind::ADMISSION,
            &0_u64,
        )
        .unwrap();
    let outcome = CallOutcome::of(&call, Some(admission), 1)
        .stage(&mut staged)
        .unwrap();
    let mut decoder = GraphDecoder::new(&staged, DecodeLimits::within_total(1 << 22));
    let body = CallOutcome::body_node(&mut decoder, outcome).unwrap();
    let report = ReportView {
        entries: vec![body],
    }
    .stage(&mut staged)
    .unwrap();
    let kept = collected(&staged, report);
    assert!(!kept.contains_key(&outcome));
    assert!(!kept.contains_key(&admission));
    let mut decoder = GraphDecoder::new(&kept, DecodeLimits::within_total(1 << 22));
    let archive = CallBody::load(&mut decoder, body).unwrap().record;
    assert_eq!(archive.args, call.args);
    assert_eq!(archive.output, call.output);
    assert_eq!(archive.call_id, call.call_id);
    let payload = serde_json::to_string(&call).unwrap();
    let material = stage_payload(&mut staged, &payload).unwrap();
    assert!(matches!(material, MaterialNode::CallPayload(_)));
    let named = Material::journal_local(owner(), MaterialRole::AttemptOutput, payload.clone());
    let fact = SettledFact {
        outcome: AttemptOutcome::Completed(named.reference().clone()),
        material: Some(material),
    };
    let mut decoder = GraphDecoder::new(&staged, DecodeLimits::within_total(1 << 22));
    fact.check(&mut decoder).unwrap();
    assert_eq!(load_payload(&mut decoder, material).unwrap(), payload);
    call.args = serde_json::json!({"query": "changed"});
    let forged = stage_payload(&mut staged, &serde_json::to_string(&call).unwrap()).unwrap();
    let fact = SettledFact {
        material: Some(forged),
        ..fact
    };
    let mut decoder = GraphDecoder::new(&staged, DecodeLimits::within_total(1 << 22));
    assert_eq!(
        fact.check(&mut decoder),
        Err(GraphRefusal::PublicationMismatch)
    );
}
