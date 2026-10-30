#![allow(clippy::expect_used, clippy::unwrap_used)]

use lash_core_store::tool_run::Material;
use std::collections::BTreeMap;

use lash_core_store::checkpoint_graph::{
    DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphDecoder, GraphRefusal,
    GraphStaging, NodeId, parse_envelope,
};
use lash_core_store::effect_opener::EffectOpener;
use lash_core_store::tool_run::{AttemptOutcome, MaterialOwner, MaterialRole, SettledOutput};

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
        read: None,
        control_origin: lash_sansio::ToolControlOrigin::OwnCall,
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
        reference: lash_protocol_types::AttachmentRef::new(
            lash_protocol_types::AttachmentId::parse("cd".repeat(32)).unwrap(),
            lash_protocol_types::MediaType::parse("text/plain").unwrap(),
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
                    execution: Default::default(),
                    read: None,
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
    let mut waiting = parked
        .material(owner(), MaterialRole::AttemptOutput)
        .parked("w".to_owned());
    waiting.await_timer(lash_core_store::tool_run::CompletionTimer {
        wait: "timer".into(),
        deadline_ms: 7250,
    });
    let report = ReportView::default().stage(&mut staging).unwrap();
    let request = Material::journal_local(owner(), MaterialRole::PreparedRequest, "r".to_owned());
    let callback = |key: &str| lash_vocabulary::providers::PluginCallbackIdentity {
        owner: "reference-host".into(),
        key: key.into(),
    };
    let provider = crate::ProviderKey::from_name(
        lash_vocabulary::claims::RegistryKind::Session,
        &lash_kernel_value::package::ProviderName {
            owner: "reference-host".into(),
            key: "tools".into(),
        },
    )
    .unwrap();
    let plan = super::super::ModelReadPlan {
        ordinal: 1,
        slot: 2,
        caller: lash_protocol_types::ToolReadCaller::ModelRound { round_key: "round-7".into() },
        provider: provider.clone(),
        definition: lash_vocabulary::tool_contract::ToolDefinition::raw(
            "document-read", "tools.lookup", "Read a versioned document.",
            serde_json::json!({"type":"object","properties":{"id":{"type":"string"}},"required":["id"]}),
            serde_json::json!({"type":"object","properties":{"version":{"type":"integer","minimum":0}},"required":["version"]}),
        ).unwrap().with_execution(std::time::Duration::from_secs(30))
            .with_execution_policy(lash_kernel_value::ExecutionPolicy::Read),
        call_id: ToolCallId::fixture("read-1"),
        presentation: lash_core_store::tool_run::PresentationBinding {
            presenter: Some(callback("presenter")), steps: vec![callback("retain")],
        },
        request: request.reference().clone(),
        refusal: Some(crate::ToolFailure::new("unavailable", "the document is unavailable")),
    };
    let output = ToolCallOutput::failure(plan.refusal.clone().unwrap());
    let saved = super::super::ModelReadResult {
        fact: lash_protocol_types::ReadFact {
            execution: lash_protocol_types::ToolCallExecution::Read,
            caller: plan.caller.clone(),
            ordinal: plan.ordinal,
            call_id: plan.call_id.clone(),
            provider,
            tool: "document-read".into(),
            args: serde_json::json!({"id":"pricing"}),
            outcome: output.outcome.clone(),
            site: "model-slot-2".into(),
            origin: "round-7".into(),
        },
        presentation: crate::runtime::effect::ToolPresentation {
            model_return: ModelToolReturn::from_output("document-read".into(), &output),
            artifacts: Vec::new(),
            retention: crate::OutputRetentionPolicy::STANDARD,
        },
    };
    let read = stage_model_read(&mut staging, &saved).unwrap();
    for fact in [
        RoundFact::Admit {
            reads: vec![plan],
            members: vec![AdmitMember::Pinned(Box::new(PinnedMember {
                admitted_at: 0,
                call: ToolCallId::fixture("a"),
                tool: "query".to_owned(),
                request: request.reference().clone(),
                policy: lash_kernel_value::tool_contract::ExecutionPolicy::Once,
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
        RoundFact::Admit {
            members: Vec::new(),
            reads: Vec::new(),
        },
        RoundFact::Present { reads: Vec::new() },
        RoundFact::Present { reads: vec![read] },
        RoundFact::CellReport { report },
    ] {
        let node = fact.stage(&mut staging).unwrap();
        let mut decoder = GraphDecoder::new(&staging, DecodeLimits::within_total(1 << 20));
        assert_eq!(RoundFact::load(&mut decoder, node).unwrap(), fact);
        if let RoundFact::Present { reads } = &fact {
            for read in reads {
                assert_eq!(load_model_read(&mut decoder, *read).unwrap(), saved);
            }
        }
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
/// pins/presentation can be reclaimed.
///
/// COMPLETION-MATERIAL-IDENTITY (FIG-7056): a completion stays typed from its
/// producer into the graph. Its material is named by the digest of its JSON
/// text, byte for byte the text material's; it stages as the outcome node
/// its value stages to, and reads back as that value, checked against the
/// digest; a node whose request arguments differ is refused.
#[test]
fn the_archival_call_body_is_shared_and_its_payload_digest_binds_the_request() {
    let output = ToolCallOutput::success(serde_json::json!({"answer": "a".repeat(70_000)}));
    let presented = ModelToolReturn::from_output("query".to_owned(), &output);
    let mut call = completed(output, presented);
    call.args = serde_json::json!({"query": "q\"\\\n\u{1}".repeat(17_500)});
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
        reads: Vec::new(),
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

    let text = serde_json::to_string(&call).unwrap();
    let typed =
        super::super::completed_material(&EffectOpener::turn("session", "turn"), call.clone())
            .unwrap();
    assert_eq!(
        typed.reference(),
        Material::journal_local(owner(), MaterialRole::AttemptOutput, text.clone()).reference(),
        "the typed digest is the text digest"
    );
    let material = stage_material(&mut staged, &typed).unwrap();
    assert_eq!(
        material,
        MaterialNode::CallPayload(CallOutcome::of(&call, None, 0).stage(&mut staged).unwrap()),
        "a typed completion stages as its value's outcome node"
    );
    let fact = SettledFact {
        outcome: AttemptOutcome::Completed(typed.reference().clone()),
        material: Some(material),
    };
    let mut decoder = GraphDecoder::new(&staged, DecodeLimits::within_total(1 << 22));
    fact.check(&mut decoder).unwrap();
    let SettledOutput::Completed(read) = fact.output(&mut decoder).unwrap() else {
        panic!("a completed output");
    };
    assert_eq!(read, typed);
    let answer = read.typed::<CompletedCall>().expect("read back typed");
    assert_eq!(serde_json::to_string(answer).unwrap(), text);
    assert_eq!(read.payload(), text, "its text projects byte for byte");

    call.args = serde_json::json!({"query": "changed"});
    let forged = SettledFact {
        material: Some(MaterialNode::CallPayload(
            CallOutcome::of(&call, None, 0).stage(&mut staged).unwrap(),
        )),
        ..fact
    };
    let mut decoder = GraphDecoder::new(&staged, DecodeLimits::within_total(1 << 22));
    assert_eq!(
        forged.check(&mut decoder),
        Err(GraphRefusal::PublicationMismatch)
    );
    assert_eq!(
        forged.output(&mut decoder),
        Err(GraphRefusal::PublicationMismatch)
    );
}

/// FIG-7554: saved read facts must refuse data that typed decoding discards.
#[test]
fn saved_read_fact_rejects_unknown_members() {
    let fact = lash_protocol_types::ReadFact {
        execution: lash_protocol_types::ToolCallExecution::Read,
        caller: lash_protocol_types::ToolReadCaller::ModelRound {
            round_key: "round".into(),
        },
        ordinal: 0,
        call_id: ToolCallId::fixture("read"),
        provider: crate::ProviderKey::new(
            lash_vocabulary::claims::RegistryKind::Session,
            "provider",
            lash_vocabulary::providers::ToolProviderKey::new("query").unwrap(),
        ),
        tool: "query".into(),
        args: serde_json::json!({}),
        outcome: crate::ToolCallOutcome::Failure(
            lash_protocol_types::ReadPanic {
                tool_name: "query".into(),
                message: "panic".into(),
            }
            .failure(),
        ),
        site: String::new(),
        origin: String::new(),
    };
    let mut staging = staging();
    let node = stage_read_fact(&mut staging, &fact).unwrap();
    let loaded = load_read_fact(
        &mut GraphDecoder::new(&staging, DecodeLimits::within_total(1 << 20)),
        node,
    )
    .unwrap();
    assert_eq!(loaded, fact);
    let output = fact.record().output;
    let saved = crate::runtime::actor::round::ModelReadResult {
        fact: fact.clone(),
        presentation: crate::runtime::effect::ToolPresentation {
            model_return: ModelToolReturn::from_output("query".into(), &output),
            artifacts: vec![],
            retention: crate::OutputRetentionPolicy::STANDARD,
        },
    };
    let node = stage_model_read(&mut staging, &saved).unwrap();
    assert_eq!(
        load_model_read(
            &mut GraphDecoder::new(&staging, DecodeLimits::within_total(1 << 20)),
            node
        )
        .unwrap(),
        saved
    );
    for pointer in ["", "/caller", "/outcome/payload/cause/panic"] {
        let mut tree = serde_json::to_value(&fact).unwrap();
        tree.pointer_mut(pointer)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("discarded_member".into(), serde_json::json!(true));
        let MaterialNode::Value(node) = stage_value(&mut staging, &tree).unwrap() else {
            panic!("value")
        };
        assert!(
            load_read_fact(
                &mut GraphDecoder::new(&staging, DecodeLimits::within_total(1 << 20)),
                node
            )
            .is_err(),
            "{pointer}"
        );
        let mut tree = serde_json::to_value(&saved).unwrap();
        tree.pointer_mut(&format!("/fact{pointer}"))
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("discarded_member".into(), serde_json::json!(true));
        let MaterialNode::Value(node) = stage_value(&mut staging, &tree).unwrap() else {
            panic!("value")
        };
        assert!(
            load_model_read(
                &mut GraphDecoder::new(&staging, DecodeLimits::within_total(1 << 20)),
                node
            )
            .is_err(),
            "model {pointer}"
        );
    }
}

/// FIG-7554: a view must refuse nonfinite priorities before writing a saved tree.
#[test]
fn tool_view_refuses_nonfinite_priority_before_storage() {
    for priority in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            lash_protocol_types::ToolViewPriority::new(priority),
            Err(lash_protocol_types::NonFiniteToolViewPriority)
        );
    }
    for priority in [f64::MIN, -0.0, 0.5, f64::MAX] {
        let view = lash_protocol_types::ToolView {
            blocks: vec![lash_protocol_types::ToolViewBlock::Text {
                text: "row".into(),
                meta: lash_protocol_types::ToolViewMeta {
                    priority: Some(lash_protocol_types::ToolViewPriority::new(priority).unwrap()),
                    last_modified: None,
                },
            }],
        };
        let output = ToolCallOutput::success(serde_json::json!(null)).with_view(view);
        let call = completed(
            output.clone(),
            ModelToolReturn::from_output("query".into(), &output),
        );
        let outcome = CallOutcome::of(&call, None, 0);
        let mut staging = staging();
        let node = outcome.stage(&mut staging).unwrap();
        let loaded = CallOutcome::load(
            &mut GraphDecoder::new(&staging, DecodeLimits::within_total(1 << 20)),
            node,
        )
        .unwrap();
        assert_eq!(loaded.output.view, output.view);
    }
}
