use super::*;
use crate::frontend::TypeScriptFrontend;
use lashlang::Value;
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

fn starts(frontend: &dyn crate::Frontend, count: usize) {
    let (mut server, _parent) = server(frontend);
    let limits =
        lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"))
            .vm_limits;
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
        let step = server.start(input).expect("Start");
        assert!(matches!(step, VmStep::Complete(_) | VmStep::Suspended(_)));
    }
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
