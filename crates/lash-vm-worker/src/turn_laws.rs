//! What one turn costs a ready worker, counted on the thread that serves
//! it: the worker compiles its library before it is ready, so its first
//! turn allocates what a warm turn does (FIG-5922).
#![expect(
    clippy::expect_used,
    clippy::panic,
    reason = "a law fails on a broken fixture assumption"
)]

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lash_kernel_doc::{Datum, EffectName, Name, Param, Signature, Type};
use lash_kernel_vm::{Outcome, Request as RunRequest, Start as RunStart, Target};
use lash_vm_client::ipc::{Bootstrap, read_frame, write_frame};
use lash_vm_client::service::{Request, Response};
use lash_vm_client::wire::{self, OutcomeWire, ParkWire, StartWire};
use lash_vm_client::{PoolConfig, WorkerEntry};
use lash_vm_protocol::*;

/// The dialect the law's cells are written in.
const DIALECT: &str = lash_vm_releases::TYPESCRIPT;

thread_local! {
    /// The bytes this thread asked the allocator for.
    static ALLOCATED: Cell<u64> = const { Cell::new(0) };
}

/// The system allocator, counting what each thread asks of it.
struct Counting;

fn count(bytes: usize) {
    let _ = ALLOCATED.try_with(|allocated| allocated.set(allocated.get() + bytes as u64));
}

// SAFETY: every call forwards to the system allocator unchanged.
#[expect(
    unsafe_code,
    reason = "the law counts allocations by forwarding to the system allocator"
)]
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(new_size);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The parent's end of one worker's socket, driving it as the pool does.
struct Parent {
    pipe: UnixStream,
    codec: FrameCodec,
    outgoing: MessageFence,
    incoming: MessageFence,
    lease: u64,
}

impl Parent {
    fn lease(&mut self) {
        self.lease += 1;
        let lease = ExecutionLease(self.lease);
        self.outgoing = MessageFence::new(lease, OwnerEpoch(0), FrameEpoch(0));
        self.incoming = MessageFence::new(lease, OwnerEpoch(0), FrameEpoch(0));
    }

    /// The worker's answer to `message`, past its progress.
    fn exchange(&mut self, message: ParentMessage) -> WorkerMessage {
        let bytes = self
            .codec
            .encode_parent(&ParentFrame {
                header: self.outgoing.next_header(),
                message,
            })
            .expect("encode");
        write_frame(
            &mut self.pipe,
            &bytes,
            Instant::now() + Duration::from_secs(5),
        )
        .expect("send");
        loop {
            let bytes = read_frame(
                &mut self.pipe,
                &self.codec,
                Instant::now() + Duration::from_secs(60),
            )
            .expect("receive");
            let frame = self.codec.decode_worker(&bytes).expect("decode");
            self.incoming.admit(&frame.header).expect("fence");
            match frame.message {
                WorkerMessage::Progress { .. } => {}
                message => return message,
            }
        }
    }
}

fn tool() -> Signature {
    Signature {
        params: vec![Param {
            name: Name::new("input"),
            ty: Type::Any,
            optional: false,
        }],
        result: Type::Any,
    }
}

/// A cell that marks and finishes, as a host lowers it.
fn lower() -> EncodedPayload {
    let request = Request::Lower {
        dialect: "typescript".into(),
        source: r#"await tools.mark({ key: "probe" }); await control.finish({ operation: "op" });"#
            .into(),
        effects: ["tools.mark", "control.finish"]
            .into_iter()
            .map(|name| (EffectName::new(name).expect("a tool's name"), tool()))
            .collect(),
        tool_roots: ["tools", "control"].into_iter().map(Name::new).collect(),
        controls: BTreeMap::from([(
            EffectName::new("control.finish").expect("a tool's name"),
            BTreeSet::from([lash_kernel_dialect::EffectControl::Finish]),
        )]),
        bindings: BTreeSet::new(),
        functions: BTreeMap::new(),
        helpers: lash_vm_library::HELPER_RELEASE,
        reads: BTreeSet::new(),
    };
    EncodedPayload(rmp_serde::to_vec_named(&request).expect("request"))
}

/// One mark+finish turn: the cell lowered, and its run started on the same
/// lease, each park's outcomes delivered with the next slice, then a reset.
fn turn(parent: &mut Parent, bounds: RunBounds) {
    parent.lease();
    let WorkerMessage::Prepared { response } = parent.exchange(ParentMessage::Prepare {
        owner: VmOwner::new("pure-worker-work"),
        request: lower(),
    }) else {
        panic!("the cell lowers")
    };
    let Response::Lowered { document, .. } = rmp_serde::from_slice(&response.0).expect("response")
    else {
        panic!("the cell lowers")
    };
    let start = Start {
        owner: VmOwner::new("codemode:session"),
        dialect: DIALECT.into(),
        document: wire::wrap(PayloadKind::Document, &document).expect("document"),
        from: StartFrom::Fresh(
            wire::encode(
                PayloadKind::Start,
                &StartWire::of(
                    RunStart {
                        target: Target::Main,
                        args: Vec::new(),
                        bindings: Default::default(),
                    },
                    &lash_kernel_vm::EffectModes::recorded(),
                ),
            )
            .expect("start"),
        ),
        bounds,
        export: Some(EncodedPayload(lash_kernel_doc::graph_tokens::encode_body(
            &lash_core_store::checkpoint_graph::bodies::OwnerBinding::Cell {
                session: "session".into(),
                turn: "turn".into(),
                cell: "cell".into(),
            },
        ))),
        observe_sites: false,
    };
    let mut start = Some(Box::new(start));
    let mut deliveries = Vec::new();
    loop {
        let step = parent.exchange(ParentMessage::Run {
            start: start.take(),
            deliveries: std::mem::take(&mut deliveries),
            slice: 1_000_000,
            cancel: false,
        });
        match step {
            WorkerMessage::Parked { park, state, .. } => {
                assert!(state.is_some(), "the park carries its state");
                let park: ParkWire = wire::decode(PayloadKind::Park, &park).expect("park");
                let park = lash_kernel_vm::Park::try_from(park).expect("canonical park");
                for request in park.requests {
                    let RunRequest::Effect(effect) = request else {
                        panic!("the cell performs effects")
                    };
                    deliveries.push(Delivery {
                        wait: effect.wait.0,
                        outcome: wire::encode(
                            PayloadKind::Outcome,
                            &OutcomeWire::from(Outcome::Completed(Datum::Null)),
                        )
                        .expect("outcome"),
                    });
                }
            }
            WorkerMessage::Ended { .. } => break,
            other => panic!("the cell parks or ends: {other:?}"),
        }
    }
    assert!(matches!(
        parent.exchange(ParentMessage::Reset),
        WorkerMessage::ResetDone { .. }
    ));
}

/// FIG-5922: a ready worker holds its library compiled, so the first turn
/// it serves allocates, on its serving thread, within a megabyte of what
/// each warm turn after it does. Compiling the library on the first turn's
/// start asked for about 34 MB more.
#[test]
fn a_ready_worker_serves_its_first_turn_at_a_warm_turns_cost() {
    let config = PoolConfig::codemode(WorkerEntry::helper("unused"));
    let codec = FrameCodec::new(config.protocol.decode);
    let bootstrap = Bootstrap::from(&config);
    let (pipe, served) = UnixStream::pair().expect("socket pair");
    // The serving thread's count as each turn's lowering and reset arrive.
    let marks = Arc::new(Mutex::new(Vec::new()));
    let server = std::thread::spawn({
        let codec = codec.clone();
        let marks = Arc::clone(&marks);
        move || {
            let embedding =
                crate::embedding::tests::typescript_only(&bootstrap.tuning).expect("embedding");
            let mut server =
                crate::worker::Server::new(served, codec, bootstrap, &embedding).expect("server");
            let mut mark = |message: &ParentMessage| {
                if matches!(
                    message,
                    ParentMessage::Prepare { .. } | ParentMessage::Reset
                ) {
                    marks.lock().expect("marks").push(ALLOCATED.with(Cell::get));
                }
            };
            let mut hook: Option<&mut dyn FnMut(&ParentMessage)> = Some(&mut mark);
            server.run(&mut hook).expect("serve");
        }
    });
    let mut parent = Parent {
        pipe,
        codec: codec.clone(),
        outgoing: MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0)),
        incoming: MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0)),
        lease: 0,
    };
    let ready = read_frame(
        &mut parent.pipe,
        &codec,
        Instant::now() + Duration::from_secs(60),
    )
    .expect("ready");
    parent
        .incoming
        .admit(&codec.decode_worker(&ready).expect("ready").header)
        .expect("fence");
    for _ in 0..3 {
        turn(&mut parent, config.run_bounds);
    }
    // The parent hangs up; the worker ends at the stream's end.
    drop(parent);
    server.join().expect("the worker serves");
    let marks = marks.lock().expect("marks").clone();
    let turns: Vec<u64> = marks.chunks(2).map(|turn| turn[1] - turn[0]).collect();
    let [first, .., warm] = turns[..] else {
        panic!("three turns: {marks:?}")
    };
    assert!(
        first <= warm + 1024 * 1024,
        "the first turn allocated {first} bytes, a warm one {warm}: {turns:?}"
    );
}
