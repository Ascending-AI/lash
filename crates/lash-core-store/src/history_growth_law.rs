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
use lash_vocabulary::identity::NodeId;
use lash_vocabulary::identity::SessionId;

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

pub(crate) mod allocation_counter {
    //! Allocation requests on the measuring thread.
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;

    #[derive(Clone, Copy, Debug, Default)]
    pub(crate) struct Counts {
        pub(crate) allocated: usize,
        pub(crate) allocations: usize,
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
            // SAFETY: Allocations came from System with the same layout.
            unsafe { System.dealloc(ptr, layout) }
        }

        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            charge(new_size);
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
