//! FIG-6889: what a turn's commit costs the store-read window it extends
//! does not grow with the history the window already holds.

use crate::Message;
use crate::MessageOrigin;
use crate::MessageRole;
use crate::Part;
use crate::session_graph::{SessionGraph, SessionNodePayload, SessionNodeRecord, WindowAnchor};
use crate::session_model::{ConversationRecord, SessionHistoryRecord};
use crate::shared_parts;
use crate::transcript::{SessionTranscript, TranscriptDecoders};
use lash_kernel_value::identity::SessionId;
use lash_vocabulary::identity::NodeId;

fn record(id: &str, parent: &str, role: MessageRole, turn: &str) -> SessionNodeRecord {
    let turn_id = lash_vocabulary::identity::TurnId::parse(turn).expect("turn id");
    let origin = match role {
        MessageRole::User => MessageOrigin::TurnInput {
            turn_id,
            input_id: None,
        },
        _ => MessageOrigin::TurnOutput {
            turn_id,
            source: crate::TurnOutputSource::Runtime,
            cell_id: None,
        },
    };
    SessionNodeRecord {
        node_id: NodeId::fixture(id),
        parent_node_id: Some(NodeId::fixture(parent)),
        timestamp: "2026-10-19T00:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
        payload: SessionNodePayload::Event {
            event: SessionHistoryRecord::Conversation(ConversationRecord::from_message(Message {
                id: id.to_owned(),
                role,
                parts: shared_parts(vec![Part::text(format!("{id}.p0"), id.to_owned(), None)]),
                origin: Some(origin),
                reply_marker: None,
            })),
        },
    }
}

fn turn_nodes(turn: usize, parent: &str) -> Vec<SessionNodeRecord> {
    let run = format!("turn-{turn}");
    let input = format!("input-{turn}");
    let output = format!("output-{turn}");
    vec![
        record(&input, parent, MessageRole::User, &run),
        record(&output, &input, MessageRole::Assistant, &run),
    ]
}

/// A proven window of one frame holding `turns` committed turns.
fn window(turns: usize) -> SessionGraph {
    let frame_key = crate::FrameKey::from_caller_material("history").expect("frame material");
    let base = SessionNodeRecord {
        node_id: NodeId::fixture(
            crate::session_graph::frame_node_id(&SessionId::from("history"), frame_key.as_str())
                .into_inner(),
        ),
        parent_node_id: None,
        timestamp: "2026-10-19T00:00:00.000000000Z"
            .parse()
            .expect("canonical node timestamp"),
        payload: SessionNodePayload::FrameOpen {
            frame_key,
            reason: crate::AgentFrameReason::initial(),
            assignment: crate::AgentFrameAssignment::unconfigured(crate::SessionPolicy::new(
                crate::TurnBudget::Unbounded,
                crate::MaxToolCalls::new(1024),
                crate::NoProgressBudget::bounded(12),
            )),
        },
    };
    let anchor = WindowAnchor {
        frame_node_id: crate::FrameNodeId::new(base.node_id.to_string()).expect("non-empty"),
        generation: 0,
        external_parent: None,
        previous_frame_node_id: None,
    };
    let mut leaf = base.node_id.to_string();
    let mut nodes = vec![base];
    for turn in 0..turns {
        nodes.extend(turn_nodes(turn, &leaf));
        leaf = nodes.last().expect("a turn's nodes").node_id.to_string();
    }
    let graph = SessionGraph::from_window(nodes, NodeId::fixture(leaf), anchor)
        .expect("a well-formed window");
    graph
        .validate_resident_integrity()
        .expect("the window is proven");
    graph
}

/// FIG-7222: reading one plugin's recovery facts must not allocate for
/// ordinary conversation nodes in the held frame.
#[test]
fn plugin_recovery_lookup_allocates_independently_of_history_depth() {
    use crate::session_graph::facade_ops::SessionGraphFacadeOps as _;
    let measured = [16, 1024].map(|turns| {
        let mut graph = window(turns);
        let marker = graph.append_plugin("overflow-recovery", serde_json::json!({"pending": true}));
        graph.append_plugin("other-plugin", serde_json::json!({"unrelated": true}));
        let measurement = allocation_counter::Measurement::start();
        let records = graph.active_plugin_nodes("overflow-recovery");
        let absent = graph.active_plugin_nodes("absent-plugin");
        let counts = measurement.finish();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].node_id, marker);
        assert!(absent.is_empty());
        counts
    });
    eprintln!("plugin recovery lookup at 16 and 1024 turns: {measured:?}");
    assert!(
        measured[1].allocated <= measured[0].allocated,
        "recovery facts must not copy the conversation path: {measured:?}"
    );
}

/// Extending a proven window by one turn's rows, validating it, adopting it
/// as durable and decoding the turn's transcript entries costs about the
/// same at 1024 resident turns as at 16: each persistent index copies at
/// most one trie path (32 slots a level) per write, so only a logarithmic
/// number of allocations separates the depths. Copying one id or position
/// per resident node, as rebuilding any index would, exceeds the bound
/// many times over.
#[test]
fn a_turns_window_extension_allocates_independently_of_history_depth() {
    let decoders = TranscriptDecoders::default();
    let measured = [16, 1024].map(|turns| {
        let base = window(turns);
        let parent = base.leaf_node_id.clone().expect("a window leaf");
        let appended = turn_nodes(turns, parent.as_str());
        let leaf = appended.last().expect("a turn's nodes").node_id.clone();
        let measurement = allocation_counter::Measurement::start();
        let head = base
            .extend_window(appended, &leaf)
            .expect("the turn's rows extend the window");
        head.validate_resident_integrity()
            .expect("the extended window stays proven");
        let mut durable = head.clone();
        durable.mark_all_durable();
        assert!(durable.retire_below_current_frame().is_none());
        let added =
            SessionTranscript::added_since(&base, &head, &decoders).expect("the turn's entries");
        let counts = measurement.finish();
        assert_eq!(added.entries().len(), 2);
        assert_eq!(durable.pending_nodes().len(), 0);
        assert_eq!(durable.durable_len(), 2 * turns + 3);
        counts
    });
    eprintln!("window extension at 16 and 1024 turns: {measured:?}");
    assert!(
        measured[1].allocations <= measured[0].allocations + 32,
        "a turn's window extension must not allocate per resident node: {measured:?}"
    );
    assert!(
        measured[1].allocated <= measured[0].allocated + 8192,
        "a turn's window extension must not copy resident history: {measured:?}"
    );
}

/// FIG-7221: commit receipts repeat the prepared suffix's timestamps. Even
/// when a published snapshot sees that suffix, acknowledgement must not
/// copy the frame's growing node-pointer prefix just to write equal values.
#[test]
fn acknowledging_prepared_timestamps_does_not_copy_held_history() {
    let measured = [16, 1024].map(|turns| {
        let base = window(turns);
        let parent = base.leaf_node_id.clone().expect("window leaf");
        let appended = turn_nodes(turns, parent.as_str());
        let leaf = appended.last().expect("turn output").node_id.clone();
        let mut prepared = base.extend_window(appended, &leaf).expect("prepared turn");
        let suffix = prepared.nodes[base.nodes.len()..]
            .iter()
            .map(|node| crate::session_graph::RealizedNodeTimestamp {
                node_id: node.node_id.clone(),
                timestamp: node.timestamp,
            })
            .collect::<Vec<_>>();
        let published = prepared.clone();
        let warm = prepared.read_model();
        let measurement = allocation_counter::Measurement::start();
        prepared.apply_realized_node_timestamps(&suffix);
        let counts = measurement.finish();
        assert!(lash_protocol_types::AppendVec::ptr_eq(
            &warm.messages,
            &prepared.read_model().messages,
        ));
        assert_eq!(prepared.leaf_node_id, published.leaf_node_id);

        // An unordered receipt still addresses identities, ignores absent
        // nodes and gives the last occurrence of an id its final timestamp.
        // Reverting one id to its original value must cancel its earlier edit.
        let changed = "2026-10-23T00:00:00.000000000Z"
            .parse()
            .expect("canonical timestamp");
        let stamp =
            |node_id, timestamp| crate::session_graph::RealizedNodeTimestamp { node_id, timestamp };
        prepared.apply_realized_node_timestamps(&[
            stamp(suffix[1].node_id.clone(), changed),
            stamp(NodeId::fixture("absent"), changed),
            stamp(suffix[0].node_id.clone(), changed),
            suffix[0].clone(),
        ]);
        assert_eq!(prepared.nodes.last().expect("output").timestamp, changed);
        assert_eq!(
            prepared.nodes[base.nodes.len()].timestamp,
            suffix[0].timestamp,
        );
        assert_eq!(
            published.nodes.last().expect("published output").timestamp,
            suffix[1].timestamp,
        );
        assert!(std::sync::Arc::ptr_eq(&base.nodes[0], &prepared.nodes[0]));
        counts
    });
    eprintln!("timestamp acknowledgement at 16 and 1024 turns: {measured:?}");
    assert!(
        measured[1].allocated * 100 <= measured[0].allocated * 105,
        "acknowledging a prepared suffix must not copy held history: {measured:?}",
    );
}

/// FIG-7382: a frame keeps a protocol payload's JSON bytes, rather than an
/// expanded object tree for every tiny field of every completed turn.
#[test]
fn protocol_payload_retention_is_bounded_by_its_json_bytes() {
    let value = serde_json::Value::Object(
        (0..24)
            .map(|index| {
                (
                    format!("field_{index}"),
                    serde_json::json!({"value": index, "ok": true}),
                )
            })
            .collect(),
    );
    let wire = serde_json::to_vec(&value).unwrap();
    let measurement = allocation_counter::Measurement::start();
    let payloads: Vec<_> = (0..256)
        .map(|_| lash_protocol_types::ProtocolPayload::new(value.clone()))
        .collect();
    for payload in &payloads {
        // Typed projection and an owned trace copy need the tree only while read.
        let fields = payload.derived(|value| value.as_object().unwrap().len());
        assert_eq!(*fields, 24);
        let observed = payload.clone().into_value();
        assert_eq!(serde_json::to_vec(&observed).unwrap(), wire);
        assert_eq!(serde_json::to_vec(payload).unwrap(), wire);
    }
    let counts = measurement.finish();
    let live = counts.allocated.saturating_sub(counts.deallocated);
    assert!(
        live <= 256 * wire.len() + 128 * 1024,
        "payload JSON trees outlive their publication: {live} live bytes for {} JSON bytes",
        256 * wire.len()
    );

    // Constructed values beyond the text parser's depth still keep their value,
    // and numeric equality keeps Value's signed-zero semantics.
    let mut deep = serde_json::Value::Null;
    for _ in 0..256 {
        deep = serde_json::json!([deep]);
    }
    let payload = lash_protocol_types::ProtocolPayload::new(deep.clone());
    assert_eq!(payload.value(), &deep);
    assert_eq!(
        serde_json::to_vec(&payload).unwrap(),
        serde_json::to_vec(&deep).unwrap()
    );
    assert_eq!(
        lash_protocol_types::ProtocolPayload::new(serde_json::json!(-0.0)),
        lash_protocol_types::ProtocolPayload::new(serde_json::json!(0.0)),
    );
}

pub(crate) mod allocation_counter {
    //! Allocation requests on the measuring thread.
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    #[derive(Clone, Copy, Debug, Default)]
    pub(crate) struct Counts {
        pub(crate) allocated: usize,
        pub(crate) allocations: usize,
        pub(crate) deallocated: usize,
    }

    thread_local! {
        static COUNTS: Cell<Option<Counts>> = const { Cell::new(None) };
    }

    fn charge(bytes: usize) {
        let _ = COUNTS.try_with(|cell| {
            if let Some(mut counts) = cell.get() {
                counts.allocated += bytes;
                counts.allocations += 1;
                cell.set(Some(counts));
            }
        });
    }

    struct Counter;

    #[global_allocator]
    static ALLOCATOR: Counter = Counter;

    #[expect(
        unsafe_code,
        reason = "test allocator forwards the unchanged GlobalAlloc contract to System"
    )]
    unsafe impl GlobalAlloc for Counter {
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            charge(layout.size());
            // SAFETY: The caller supplies a valid layout; System receives it unchanged.
            unsafe { System.alloc(layout) }
        }

        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            charge(layout.size());
            // SAFETY: The caller supplies a valid layout; System receives it unchanged.
            unsafe { System.alloc_zeroed(layout) }
        }

        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            let _ = COUNTS.try_with(|cell| {
                if let Some(mut counts) = cell.get() {
                    counts.deallocated += layout.size();
                    cell.set(Some(counts));
                }
            });
            // SAFETY: Allocations came from System with the same layout.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            charge(new_size);
            let _ = COUNTS.try_with(|cell| {
                if let Some(mut counts) = cell.get() {
                    counts.deallocated += layout.size();
                    cell.set(Some(counts));
                }
            });
            // SAFETY: The pointer, layout and new size keep the caller's contract.
            unsafe { System.realloc(ptr, layout, new_size) }
        }
    }

    pub(crate) struct Measurement;

    impl Measurement {
        pub(crate) fn start() -> Self {
            COUNTS.with(|cell| {
                assert!(cell.get().is_none(), "allocation measurements cannot nest");
                cell.set(Some(Counts::default()));
            });
            Self
        }

        pub(crate) fn finish(self) -> Counts {
            COUNTS.with(|cell| cell.take().expect("active allocation measurement"))
        }
    }

    impl Drop for Measurement {
        fn drop(&mut self) {
            COUNTS.with(|cell| cell.set(None));
        }
    }
}
