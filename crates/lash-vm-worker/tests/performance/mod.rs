use super::*;
use crate::frontend::TypeScriptFrontend;
use lashlang::{AbilityOutcome, Record, Value};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct CountingAllocator;
thread_local! {
    static ALLOCATED: Cell<usize> = const { Cell::new(0) };
}

#[expect(
    unsafe_code,
    reason = "the test allocator counts allocations on only the law's thread"
)]
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let _ = ALLOCATED.try_with(|bytes| bytes.set(bytes.get().saturating_add(layout.size())));
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn allocated<T>(run: impl FnOnce() -> T) -> (T, usize) {
    let before = ALLOCATED.with(Cell::get);
    let result = run();
    (result, ALLOCATED.with(Cell::get) - before)
}

fn server<'a>(frontend: &'a dyn crate::Frontend) -> (Server<'a>, UnixStream) {
    let config =
        lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"));
    let (pipe, mut parent) = UnixStream::pair().expect("pipe");
    let codec = FrameCodec::new(config.protocol.decode);
    let server =
        Server::new(pipe, codec.clone(), Bootstrap::from(&config), frontend).expect("server");
    read_frame(
        &mut parent,
        &codec,
        Instant::now() + Duration::from_secs(30),
    )
    .expect("ready");
    (server, parent)
}

fn starts(frontend: &dyn crate::Frontend, count: usize) -> (Duration, Vec<Vec<u8>>) {
    let (mut server, _parent) = server(frontend);
    let limits =
        lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"))
            .vm_limits;
    let mut duration = Duration::ZERO;
    let mut snapshots = Vec::new();
    for n in 0..count {
        server.instance.reset();
        let input = Start {
            owner: VmOwner::new("session"),
            program: ProgramSource::Source {
                dialect: "typescript".into(),
                text: format!("let cell = {n}; cell + 1;"),
            },
            state: StartState::Fresh,
            contexts: Vec::new(),
            limits,
        };
        let began = Instant::now();
        let step = server.start(input).expect("Start");
        duration += began.elapsed();
        assert!(matches!(step, VmStep::Complete(_) | VmStep::Suspended(_)));
        snapshots.push(
            server
                .instance
                .state()
                .snapshot()
                .to_canonical_bytes()
                .expect("snapshot"),
        );
    }
    (duration, snapshots)
}

#[test]
fn starts_reuse_one_parser_thread_per_worker() {
    for count in [1, 32, 128] {
        let frontend = TypeScriptFrontend::default();
        starts(&frontend, count);
        let spawns = frontend.parser.lock().expect("parser").thread_spawn_count();
        println!("{count} Starts: {spawns} parser thread spawns");
        assert_eq!(spawns, 1, "{count} Starts must reuse the worker's parser");
        let mut parser = frontend.parser.lock().expect("parser");
        let sources = [
            "let ;".to_owned(),
            "1 + 2;".to_owned(),
            "(".repeat(30),
            format!("{}1;", " ".repeat(lash_typescript::MAX_SOURCE_BYTES - 2)),
            " ".repeat(lash_typescript::MAX_SOURCE_BYTES + 1),
            "3;".to_owned(),
        ];
        for source in sources {
            match (parser.parse(&source, None), lash_typescript::parse(&source)) {
                (Ok(actual), Ok(expected)) => assert_eq!(
                    rmp_serde::to_vec_named(&actual).expect("actual AST"),
                    rmp_serde::to_vec_named(&expected).expect("expected AST"),
                ),
                (Err(actual), Err(expected)) => {
                    assert_eq!(actual.code, expected.code);
                    assert_eq!(actual.message, expected.message);
                    assert_eq!(actual.span, expected.span);
                }
                _ => panic!("reusing the thread changed source acceptance"),
            }
        }
        assert_eq!(
            parser.thread_spawn_count(),
            1,
            "errors and a cap-sized source keep the same thread"
        );
    }
}

/// The pre-FIG-4565 frontend's source-sized thread on each parse. Valid cells
/// use exactly this path on main; diagnostics do not enter this measurement.
struct FreshThreadFrontend;
impl crate::Frontend for FreshThreadFrontend {
    fn language_id(&self) -> &'static str {
        "typescript"
    }
    fn parse(
        &self,
        source: &str,
        host: Option<&lashlang::LashlangHostEnvironment>,
    ) -> Result<lashlang::Program, crate::FrontendRefusal> {
        let parsed = match host {
            Some(host) => lash_typescript::parse_cell(source, host),
            None => lash_typescript::parse(source),
        };
        parsed.map_err(|error| crate::FrontendRefusal {
            policy: error.is_dialect_refusal(),
            error: lashlang::ModuleCompileError::parse_failure(
                error.span.map(|span| lashlang::Span {
                    start: span.start,
                    end: span.end,
                }),
                error.message.clone(),
                lash_typescript::format_diagnostic(source, &error),
            ),
        })
    }
}

#[test]
#[ignore = "Start latency measurement, run on the quiet host"]
fn start_latency_benchmark() {
    const STARTS: usize = 200;
    let mut before = Vec::new();
    let mut after = Vec::new();
    for pair in 0..7 {
        let frontend = TypeScriptFrontend::default();
        let (old, new) = if pair % 2 == 0 {
            (
                starts(&FreshThreadFrontend, STARTS),
                starts(&frontend, STARTS),
            )
        } else {
            let new = starts(&frontend, STARTS);
            (starts(&FreshThreadFrontend, STARTS), new)
        };
        assert_eq!(old.1, new.1, "every Start must produce identical VM state");
        assert_eq!(
            frontend.parser.lock().expect("parser").thread_spawn_count(),
            1
        );
        let us = |duration: Duration| duration.as_secs_f64() * 1_000_000.0 / STARTS as f64;
        before.push(us(old.0));
        after.push(us(new.0));
        println!(
            "pair {}: before {:.3} us/Start, after {:.3} us/Start; parser spawns {STARTS} -> 1; {STARTS} identical snapshots",
            pair + 1,
            before[pair],
            after[pair]
        );
    }
    before.sort_by(f64::total_cmp);
    after.sort_by(f64::total_cmp);
    println!(
        "median of 7 interleaved runs: before {:.3} us/Start, after {:.3} us/Start",
        before[3], after[3]
    );
}

#[test]
fn projection_free_outcomes_allocate_nothing() {
    use lashlang::{ResourceOperationBatchOutcome as Batch, ResourceOperationOutcome as Leaf};
    let frontend = TypeScriptFrontend::default();
    let (mut server, _parent) = server(&frontend);
    let wire = server.wire().expect("wire");
    let record: Record = [(
        "nested".into(),
        Value::List(
            vec![
                Value::Tuple(
                    vec![Value::Number(1.0), Value::String("x".repeat(4096).into())].into(),
                ),
                Value::Record(Arc::new(
                    [("leaf".into(), Value::Null)].into_iter().collect(),
                )),
            ]
            .into(),
        ),
    )]
    .into_iter()
    .collect();
    let value = Value::Record(Arc::new(record));
    let outcomes = [
        AbilityOutcome::Value(value.clone()),
        AbilityOutcome::ResourceOperationBatch(Batch::AllResults(vec![
            Leaf::Value(value.clone()),
            Leaf::Value(value.clone()),
        ])),
        AbilityOutcome::ResourceOperationBatch(Batch::Selected {
            leaf: 1,
            result: Leaf::Value(value),
        }),
    ];
    for outcome in outcomes {
        let expected = rmp_serde::to_vec_named(&outcome).expect("expected outcome");
        let (outcome, bytes) = allocated(|| wire.rebind_outcome(outcome));
        println!("projection-free outcome: {bytes} allocated bytes");
        assert_eq!(
            bytes, 0,
            "rebinding must move an unprojected answer unchanged"
        );
        assert_eq!(
            rmp_serde::to_vec_named(&outcome).expect("outcome"),
            expected
        );
    }
}

#[test]
fn delivering_a_request_keeps_the_original_payload_allocation() {
    let frontend = TypeScriptFrontend::default();
    let (mut server, mut parent) = server(&frontend);
    let payload = rmp_serde::to_vec_named(&AbilityOp::Print(Value::String(
        "x".repeat(512 * 1024).into(),
    )))
    .expect("request payload");
    let payload_size = payload.len();
    let original = payload.as_ptr() as usize;
    server.reissue = Some(RecordedRequest {
        kind: EffectKind::Print,
        payload: EncodedPayload(payload),
    });
    let codec = server.codec.clone();
    let reader = std::thread::spawn(move || {
        loop {
            let bytes = read_frame(
                &mut parent,
                &codec,
                Instant::now() + Duration::from_secs(30),
            )
            .expect("frame");
            let message = codec.decode_worker(&bytes).expect("worker frame").message;
            if let WorkerMessage::EffectRequest(request) = message {
                let op: AbilityOp = rmp_serde::from_slice(&request.payload.0).expect("operation");
                let AbilityOp::Print(Value::String(value)) = op else {
                    panic!("print request")
                };
                assert_eq!(value.as_str(), "x".repeat(512 * 1024));
                return;
            }
            assert!(matches!(message, WorkerMessage::Progress { .. }));
        }
    });
    let (delivered, bytes) = allocated(|| {
        server.deliver(VmStep::Suspended(lashlang::VmSuspended {
            request: VmRequest::Effect(AbilityOp::Print(Value::Null)),
            observations: Vec::new(),
        }))
    });
    delivered.expect("deliver");
    println!("{payload_size}-byte request: {bytes} bytes allocated during delivery");
    assert!(
        bytes < payload_size * 3 / 2,
        "delivery must allocate its frame without an additional payload copy: {bytes} bytes for {payload_size}"
    );
    let retained = server.pending.as_ref().expect("pending request");
    println!(
        "512 KiB request payload retains its allocation: {}",
        retained.payload.0.as_ptr() as usize == original
    );
    assert_eq!(
        retained.payload.0.as_ptr() as usize,
        original,
        "the worker must move its encoded payload into pending, without a clone"
    );
    reader.join().expect("reader");
}
