#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::collections::{BTreeMap, BTreeSet};

use lash_core::{ToolCallOutput, ToolCancellation};
use lash_core_store::checkpoint_graph::{
    DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphDecoder, GraphRefusal,
    GraphStaging, NodeId, NodeKind, parse_envelope,
};
use lash_vm_broker::round_graph::CallOutcome;
use lash_vocabulary::sites::EffectId;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;

use super::*;

fn staging() -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)))
}

/// What an immediate collection leaves: only the nodes `root` reaches, out
/// of every node `installed` holds.
fn collected(
    installed: &BTreeMap<NodeId, EncodedNode>,
    root: NodeId,
) -> BTreeMap<NodeId, EncodedNode> {
    let mut kept = BTreeMap::new();
    let mut pending = vec![root];
    while let Some(id) = pending.pop() {
        if kept.contains_key(&id) {
            continue;
        }
        let node = installed
            .get(&id)
            .expect("a reachable node is installed")
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

fn identity(occurrence: u64) -> EffectId {
    lash_vocabulary::sites::testkit::effect(
        lash_vocabulary::sites::testkit::root_task(),
        Site {
            unit: Unit::Main,
            path: vec![0],
        },
        occurrence,
        Vec::new(),
    )
}

fn call_id(ordinal: u64) -> ToolCallId {
    ToolCallId::fixture(&format!("call-{ordinal}"))
}

/// A call's admission: the summary only checks its kind.
fn admission(staging: &mut GraphStaging, ordinal: u64) -> NodeId {
    staging.encode_value(NodeKind::ADMISSION, &ordinal).unwrap()
}

fn outcome(
    staging: &mut GraphStaging,
    ordinal: u64,
    admission: NodeId,
    output: ToolCallOutput,
) -> NodeId {
    let presented =
        lash_core::facade_support::ModelToolReturn::from_output("read".to_owned(), &output);
    let completed = lash_core::CompletedToolCall {
        call_id: call_id(ordinal),
        provider_call_id: None,
        tool_name: "read".to_owned(),
        args: serde_json::json!({}),
        output,
        model_return: presented,
        intent_outcomes: Vec::new(),
        replay: None,
    };
    CallOutcome::of(&completed, Some(admission), 1)
        .stage(staging)
        .unwrap()
}

fn operation() -> EffectName {
    EffectName::new("read").unwrap()
}

/// The FIG-5887 §3 call summary: a dense radix tree of 32-entry pages.
/// Admitting 70 calls makes three pages under one index; each settlement,
/// in whatever order calls complete, writes only the page that holds the
/// call, the index above it and the root, reusing the other pages; and the
/// tree read back is the summary, whose bits match each outcome. A count
/// the tree disagrees with, or a root at another level, is refused.
#[test]
fn a_call_summary_pages_by_ordinal_and_settles_out_of_order() {
    let mut installed = BTreeMap::new();
    let mut host = HostState::default();
    let mut staging = staging();
    let mut admissions = Vec::new();
    for ordinal in 0..70 {
        let node = admission(&mut staging, ordinal);
        admissions.push(node);
        let at = host.admit(
            identity(ordinal),
            operation(),
            call_id(ordinal),
            node,
            false,
        );
        assert_eq!(at, ordinal);
    }
    let mut reads = BTreeSet::new();
    let mut root = host.stage(&mut staging, &mut reads).unwrap();
    assert!(reads.is_empty());
    installed.append(&mut staging.into_nodes());

    // Out of order: the last page, then the first, then the middle.
    for ordinal in [69, 3, 40, 0, 33] {
        let mut staging = self::staging();
        let output = if ordinal == 40 {
            ToolCallOutput::cancelled(ToolCancellation::runtime("stopped"))
        } else {
            ToolCallOutput::success(serde_json::json!(ordinal))
        };
        let node = outcome(
            &mut staging,
            ordinal,
            admissions[ordinal as usize],
            output.clone(),
        );
        let outcome_nodes = staging.nodes().len();
        assert!(host.settle(&call_id(ordinal), node, &output));
        assert!(
            !host.settle(&call_id(ordinal), node, &output),
            "settled once"
        );
        let mut reads = BTreeSet::new();
        root = host.stage(&mut staging, &mut reads).unwrap();
        // One page, the index and the root are new; the two other pages
        // are reused.
        assert_eq!(staging.nodes().len() - outcome_nodes, 3, "at {ordinal}");
        assert_eq!(reads.len(), 2);
        assert!(reads.iter().all(|page| installed.contains_key(page)));
        installed.append(&mut staging.into_nodes());
    }
    assert_eq!(host.cancelled, Some(40));

    let kept = collected(&installed, root);
    let mut decoder = GraphDecoder::new(&kept, DecodeLimits::within_total(1 << 24));
    let loaded = HostState::load(&mut decoder, root).unwrap();
    assert_eq!(loaded.calls.items(), host.calls.items());
    assert_eq!(loaded.cancelled, Some(40));

    // A root whose count disagrees with its tree is refused.
    let calls_root = host
        .calls
        .stage(&mut self::staging(), &mut BTreeSet::new())
        .unwrap();
    for count in [69, 71, 32] {
        let mut decoder = GraphDecoder::new(&kept, DecodeLimits::within_total(1 << 24));
        assert!(
            Paged::<CallSummary>::load(&mut decoder, root, calls_root, count).is_err(),
            "count {count}"
        );
    }
}

/// PC-OUTCOME's host half: the control attempt, the cancellation that
/// ends the cell, the `max_tool_calls` refusal, the prints, the started
/// processes and the wall time the cell held workers (FIG-6786) survive a
/// park and an immediate collection exactly; and a summary whose bits are
/// not its outcome's is refused.
#[test]
fn a_hosts_control_cancel_and_limit_facts_survive_collection() {
    let mut staging = staging();
    let mut host = HostState::default();
    for ordinal in 0..3 {
        let node = admission(&mut staging, ordinal);
        host.admit(
            identity(ordinal),
            operation(),
            call_id(ordinal),
            node,
            ordinal == 2,
        );
    }
    let first = host.calls.get(1).unwrap().admission;
    let cancelled = ToolCallOutput::cancelled(ToolCancellation::runtime("turn cancelled"));
    let settled = outcome(&mut staging, 1, first, cancelled.clone());
    assert!(host.settle(&call_id(1), settled, &cancelled));
    host.tool_call_limit = Some(ToolCallLimitExceeded {
        scope: ToolCallLimitScope::Cell,
        limit: std::num::NonZeroUsize::new(3).unwrap().into(),
        counted: 3,
        requested: 1,
    });
    for index in 0..40 {
        host.prints
            .push(PrintEvent(Datum::Text(format!("line {index}"))));
    }
    host.processes.push(ProcessEvent(ProcessId::fixture("p")));
    host.held_ms = 1_234;
    let root = host.stage(&mut staging, &mut BTreeSet::new()).unwrap();

    let kept = collected(staging.nodes(), root);
    let mut decoder = GraphDecoder::new(&kept, DecodeLimits::within_total(1 << 24));
    let loaded = HostState::load(&mut decoder, root).unwrap();
    assert_eq!(loaded.control_attempt, Some(2));
    assert_eq!(loaded.cancelled, Some(1));
    assert_eq!(loaded.tool_call_limit, host.tool_call_limit);
    assert_eq!(loaded.prints.items(), host.prints.items());
    assert_eq!(loaded.processes.items(), host.processes.items());
    assert_eq!(loaded.calls.items(), host.calls.items());
    assert_eq!(loaded.held_ms, 1_234);

    // A summary that calls a cancelled outcome a success is refused.
    let mut forged = host.clone();
    forged.unstaged();
    let mut summary = forged.calls.get(1).unwrap().clone();
    summary.status = CallStatus::Completed {
        outcome: settled,
        success: true,
        cancelled: false,
        control: None,
    };
    forged.calls.replace(1, summary);
    forged.cancelled = None;
    let root = forged.stage(&mut staging, &mut BTreeSet::new()).unwrap();
    let mut decoder = GraphDecoder::new(&staging, DecodeLimits::within_total(1 << 24));
    assert!(matches!(
        HostState::load(&mut decoder, root),
        Err(GraphRefusal::InvalidBody { .. })
    ));
}

/// A cell's envelope is staged once, as typed document, source map and
/// effect-table nodes: staging it again at a later park makes the same
/// node, so no park copies it. A cell's end names its terminal, its adopted
/// session state, its host root and its envelope, and after an immediate
/// collection under the end they all read back exactly.
#[test]
fn a_cells_envelope_is_one_node_and_its_end_reads_back_after_collection() {
    use crate::executor::envelope::{CellEnvelope, RecordedEffect};

    let document =
        lash_kernel_doc::parse_document("kernel 1\nnumbers float\nmain { return 1 }").unwrap();
    let source_map = lash_kernel_doc::SourceMap::new(
        &document,
        "1",
        lash_kernel_doc::SourceLanguage::TypeScript,
        vec![lash_kernel_doc::SourceSite {
            site: lash_vocabulary::sites::Site::new(lash_vocabulary::sites::Unit::Main, vec![0]),
            ranges: vec![lash_kernel_doc::Span::new(0, 1).unwrap()],
            implicit: None,
        }],
    )
    .unwrap();
    let envelope = CellEnvelope {
        code: CellEnvelope::code_digest("1"),
        dialect: "typescript".to_owned(),
        document: lash_kernel_doc::IdentifiedDocument::new(document.clone()),
        packages: lash_kernel_doc::PackageSetDigest::from_bytes([7; 32]),
        source_map: Some(source_map),
        written: std::collections::BTreeMap::new(),
        function_values: None,
        effects: [(
            operation(),
            RecordedEffect {
                tool: lash_vocabulary::tool_contract::ToolId::new("read"),
                requirement: lash_vocabulary::operations::OperationRequirement::new(
                    lash_vocabulary::types::Signature {
                        params: Vec::new(),
                        result: lash_vocabulary::types::Type::Any,
                    },
                    lash_vocabulary::operations::ContractId::new("1"),
                ),
                controls: lash_vocabulary::tool_declaration::TurnControls::default(),
            },
        )]
        .into_iter()
        .collect(),
        grants: std::collections::BTreeMap::new(),
        unavailable_notes: [("web.fetch".into(), "permission was revoked".into())].into(),
        projected: ["x".to_owned()].into_iter().collect(),
        reads: std::collections::BTreeMap::new(),
    };
    let mut staging = staging();
    let node = envelope.stage(&mut staging).unwrap();
    assert_eq!(envelope.stage(&mut staging).unwrap(), node);

    let mut host = HostState::default();
    host.prints.push(PrintEvent(Datum::Int(1.into())));
    // A session descriptor page: the end names it, and reads nothing of
    // what it reaches.
    let session = lash_vm_broker::kernel::pages::frame_page(&lash_kernel_state::paged::PageBody {
        kind: lash_kernel_state::paged::PageKind::SESSION_ROOT,
        tokens: lash_vocabulary::tokens::encode_body(&0_u64),
        children: Vec::new(),
    })
    .unwrap()
    .named()
    .into_node();
    let session_id = session.id;
    staging.adopt(session).unwrap();
    let end = CellEnd {
        end: lash_vm_broker::kernel::EndRecord::Failed(
            staging
                .encode_value(
                    NodeKind::FROZEN_VALUE,
                    &lash_kernel_doc::FrozenDatum(Datum::Int(2.into())),
                )
                .unwrap()
                .child_ref(),
        ),
        session: session_id,
        host: host.stage(&mut staging, &mut BTreeSet::new()).unwrap(),
        envelope: node,
    };
    let root = end.stage(&mut staging).unwrap();
    let kept = collected(staging.nodes(), root);
    let mut decoder = GraphDecoder::new(&kept, DecodeLimits::within_total(1 << 24));
    assert_eq!(CellEnd::load(&mut decoder, root).unwrap(), end);
    let loaded = CellEnvelope::load(&mut decoder, end.envelope).unwrap();
    assert_eq!(loaded.document, envelope.document);
    assert_eq!(loaded.packages, envelope.packages);
    assert_eq!(loaded.source_map, envelope.source_map);
    assert_eq!(loaded.effects, envelope.effects);
    assert_eq!(loaded.unavailable_notes, envelope.unavailable_notes);
    assert_eq!(loaded.projected, envelope.projected);
    assert_eq!(
        HostState::load(&mut decoder, end.host)
            .unwrap()
            .prints
            .items(),
        host.prints.items()
    );
}
