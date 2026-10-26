use super::*;

/// A bound server and its parent's pipe, with preparation complete.
fn ready_server() -> (
    lash_vm_client::PoolConfig,
    FrameCodec,
    &'static Embedding,
    Server<'static>,
    UnixStream,
) {
    let config =
        lash_vm_client::PoolConfig::codemode(lash_vm_client::WorkerEntry::helper("unused"));
    let codec = FrameCodec::new(config.protocol.decode);
    let (pipe, parent) = UnixStream::pair().expect("pipe");
    let embedding: &'static Embedding = Box::leak(Box::new(
        crate::embedding::tests::typescript_only(&config.tuning).expect("embedding"),
    ));
    let server =
        Server::new(pipe, codec.clone(), Bootstrap::from(&config), embedding).expect("server");
    (config, codec, embedding, server, parent)
}

/// The `work` document and its `Start` the tests share.
fn work_start(
    config: &lash_vm_client::PoolConfig,
    embedding: &Embedding,
) -> (lash_kernel_doc::Document, Start) {
    let add = embedding
        .library()
        .iter()
        .find(|(name, _)| *name == "num.add")
        .map(|(_, id)| id)
        .expect("helper");
    use lash_kernel_doc::{Expr, Literal, NumberPolicy, Rhs, Stmt};

    use {
        lash_kernel_value::FunctionName, lash_kernel_value::Name, lash_kernel_value::Signature,
        lash_kernel_value::Type,
    };
    let native = embedding
        .registry()
        .unwrap()
        .get(&add)
        .unwrap()
        .definition()
        .as_ref()
        .clone();
    let body = lash_kernel_doc::FunctionDefinition {
        kernel: lash_kernel_doc::KERNEL_VERSION,
        name: FunctionName::new("test.work").unwrap(),
        signature: Signature {
            params: vec![],
            result: Type::Any,
        },
        errors: Default::default(),
        guard: None,
        implementation: lash_kernel_doc::Implementation::Guest {
            body: lash_kernel_doc::FunctionBody {
                functions: [(add, native.name.clone())].into(),
                imports: Default::default(),
                block: vec![
                    Stmt::Let {
                        name: Name::new("n"),
                        value: Rhs::Expr(Expr::Call {
                            function: add,
                            args: vec![
                                Expr::Literal(Literal::Int(3.into())),
                                Expr::Literal(Literal::Int(4.into())),
                            ],
                        }),
                    },
                    Stmt::Return {
                        value: Expr::Variable(Name::new("n")),
                    },
                ],
            },
            metering: lash_kernel_doc::Metering::Statements,
        },
        native_version: 1,
    };
    let function = body.identity();
    let mut document = lash_kernel_doc::Document::new(NumberPolicy::Float, vec![]);
    document.main = None;
    document.functions = [(add, native), (function, body)].into();
    document.requires.natives.insert(add);
    document.exports.insert(
        Name::new("work"),
        lash_kernel_doc::Export::Function(lash_kernel_doc::FunctionExport {
            function,
            signature: Signature {
                params: vec![],
                result: Type::Any,
            },
            convention: lash_kernel_doc::StartConvention::Direct,
            asynchronous: false,
            rest: false,
        }),
    );
    let link = lash_kernel_value::Link {
        document: document.identity(),
        fills: Default::default(),
    };
    let link_id = link.identity();
    let instances = vec![lash_kernel_vm::PreparedInstance {
        link: link_id,
        document: document.identity(),
        slots: Default::default(),
    }];
    let start = Start {
        owner: VmOwner::new("process:law"),
        dialect: lash_kernel_releases::TYPESCRIPT.to_owned(),
        document: lash_vm_protocol::message::StoredDocument::supplied(EncodedPayload(
            document.canonical_bytes(),
        )),
        link: link_id,
        instances: wire::encode(PayloadKind::Start, &instances).unwrap(),
        documents: vec![lash_vm_protocol::message::StoredDocument::Supplied {
            document: document.identity(),
            body: EncodedPayload(document.canonical_bytes()),
        }],
        definitions: Vec::new(),
        functions: document.functions.keys().copied().collect(),
        from: StartFrom::Fresh(
            wire::encode(
                PayloadKind::Start,
                &StartWire::of(
                    lash_kernel_vm::Target::Entry(lash_kernel_value::names::Name::new("work")),
                    Vec::new(),
                    None,
                    &Default::default(),
                    Vec::new(),
                ),
            )
            .expect("start"),
        ),
        bounds: config.run_bounds,
        binding: [3; 32],
        save: None,
        warm: Vec::new(),
        observe_sites: false,
        observe_counts: false,
        observe_path: false,
        memory_limiter: false,
        charge_limiter: false,
    };
    (document, start)
}

/// FIG-5497: the worker must obey the parent's response IO deadline.
#[test]
fn parent_serialization_deadline_bounds_worker_response_write() {
    let mut config =
        lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"));
    config.deadlines.serialization = Duration::from_millis(5);
    let codec = FrameCodec::new(config.protocol.decode);
    let (pipe, mut parent) = UnixStream::pair().expect("pipe");
    let embedding =
        crate::embedding::tests::typescript_only(&config.tuning).expect("a TypeScript embedding");
    let mut server =
        Server::new(pipe, codec.clone(), Bootstrap::from(&config), &embedding).expect("server");
    let drain = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        // Releasing backpressure lets the unfixed write finish too, so
        // this regression fails promptly instead of waiting thirty seconds.
        let _ = read_frame(&mut parent, &codec, Instant::now() + Duration::from_secs(1));
    });
    let result = server.send(WorkerMessage::Prepared {
        response: EncodedPayload(vec![0; 1024 * 1024]),
    });
    drop(server);
    drain.join().expect("drain");
    assert!(
        result.is_err(),
        "the host's five-millisecond IO deadline must refuse the write"
    );
}

#[test]
fn an_oversized_non_observation_frame_preserves_its_fence_and_typed_cause() {
    let mut config =
        lash_vm_client::PoolConfig::standard(lash_vm_client::WorkerEntry::helper("unused"));
    config.protocol.decode.max_frame_bytes = 1024;
    let codec = FrameCodec::new(config.protocol.decode);
    let (pipe, mut parent) = UnixStream::pair().expect("pipe");
    let embedding =
        crate::embedding::tests::typescript_only(&config.tuning).expect("a TypeScript embedding");
    let mut server =
        Server::new(pipe, codec.clone(), Bootstrap::from(&config), &embedding).expect("server");
    let mut fence = MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0));
    let message = WorkerMessage::Prepared {
        response: EncodedPayload(vec![0; 2048]),
    };
    let size = rmp_serde::to_vec_named(&WorkerFrame {
        header: fence.next_header_copy(),
        message: message.clone(),
    })
    .expect("measure")
    .len() as u64
        + FRAME_HEADER_BYTES as u64;
    let error = server.send(message).expect_err("the frame is too large");
    server.refuse(&error).expect("typed refusal frame");
    let bytes = read_frame(&mut parent, &codec, Instant::now() + Duration::from_secs(1))
        .expect("next frame");
    let refused = codec.decode_worker(&bytes).expect("frame");
    fence
        .admit(&refused.header)
        .expect("an encode refusal must not consume a transport sequence");
    let PoolError::Infrastructure(outcome) = error else {
        panic!("typed cause: {error:?}")
    };
    assert_eq!(
        serde_json::to_value(&outcome).expect("cause"),
        serde_json::json!({
            "worker_limit_exceeded": { "limit": { "frame": {
                "kind": "prepared", "size": size, "bound": 1024
            } } }
        })
    );
    let WorkerMessage::LimitExceeded { limit } = refused.message else {
        panic!("typed refusal")
    };
    assert_eq!(
        outcome,
        InfrastructureOutcome::WorkerLimitExceeded { limit }
    );
    assert!(!outcome.is_retryable());
}

/// FIG-6422: a corrupt page is permanent wherever it is detected: the
/// worker's own heap fault crosses the pipe as itself and ends the run
/// as terminally as the parent's `PageRefusal::Corrupt`, never
/// redriven.
#[test]
fn a_corrupt_page_is_permanent_wherever_it_is_detected() {
    let (config, codec, embedding, mut server, mut parent) = ready_server();
    let (_document, mut start) = work_start(&config, embedding);
    start.warm = vec![page::ReadPage {
        page: page::PageEnvelope::new(vec![0xde, 0xad]),
        children: Vec::new(),
    }];
    let error = server
        .start(start)
        .err()
        .expect("a corrupt warm page faults the start");
    server.refuse(&error).expect("typed refusal frame");
    let bytes = read_frame(&mut parent, &codec, Instant::now() + Duration::from_secs(1))
        .expect("next frame");
    let WorkerMessage::Refused { refusal } = codec.decode_worker(&bytes).expect("frame").message
    else {
        panic!("a heap fault crosses as a refusal")
    };
    let outcome = refusal.into_outcome();
    assert_eq!(outcome, error.into_outcome());
    let json = serde_json::to_value(&outcome).expect("json");
    assert!(
        json.pointer("/heap_fault/fault/corrupt").is_some(),
        "{json}: the worker's own corrupt page crosses as its heap fault"
    );
    for outcome in [
        outcome,
        InfrastructureOutcome::PageRefused {
            refusal: lash_protocol_types::engine_fault::PageRefusal::Corrupt,
        },
    ] {
        assert!(!outcome.is_retryable(), "{outcome:?} is permanent");
        assert!(
            !PoolError::from(outcome.clone()).is_host_verdict(),
            "{outcome:?} is no host verdict"
        );
    }
}

/// FIG-6422: a host-resource exhaustion crosses as the heap's typed
/// fault, not a machine breach.
#[test]
fn a_trace_budget_exhaustion_crosses_as_its_typed_heap_fault() {
    let (_config, codec, _embedding, mut server, mut parent) = ready_server();
    let error = heap_fault(lash_kernel_doc::heap::HeapFault::Exhausted {
        resource: lash_kernel_doc::heap::HostResource::TraceBudget,
    });
    server.refuse(&error).expect("typed refusal frame");
    let bytes = read_frame(&mut parent, &codec, Instant::now() + Duration::from_secs(1))
        .expect("next frame");
    let WorkerMessage::Refused { refusal } = codec.decode_worker(&bytes).expect("frame").message
    else {
        panic!("a heap fault crosses as a refusal")
    };
    assert_eq!(
        serde_json::to_value(refusal.into_outcome()).expect("json"),
        serde_json::json!({"heap_fault": {"fault": {"exhausted": {"resource": "trace_budget"}}}})
    );
}

/// The real resolver's cold request, under the run's transport fence.
fn cached_content(
    server: &mut Server<'_>,
    mut parent: UnixStream,
    codec: FrameCodec,
    start: &Start,
    reply: lash_vm_protocol::message::StoredDocument,
) -> Result<WorkerContent, PoolError> {
    let document = start.document.identity();
    server.fences.lock().unwrap().incoming = Some(MessageFence::new(
        ExecutionLease(0),
        OwnerEpoch(0),
        FrameEpoch(0),
    ));
    let parent = std::thread::spawn(move || {
        let bytes =
            read_frame(&mut parent, &codec, Instant::now() + Duration::from_secs(5)).unwrap();
        let frame = codec.decode_worker(&bytes).unwrap();
        assert_eq!(
            frame.message,
            WorkerMessage::Documents {
                documents: vec![document]
            }
        );
        let bytes = codec
            .encode_parent(&ParentFrame {
                header: MessageFence::new(ExecutionLease(0), OwnerEpoch(0), FrameEpoch(0))
                    .next_header(),
                message: ParentMessage::Documents {
                    documents: vec![reply],
                },
            })
            .unwrap();
        write_frame(&mut parent, &bytes, Instant::now() + Duration::from_secs(5)).unwrap();
    });
    let result = server.content(start);
    parent.join().unwrap();
    result
}

/// FIG-7354: a verified process-local body survives reset; another start
/// references its identity and sends no canonical body in either direction.
#[test]
fn a_second_start_reuses_verified_documents_without_body_transport() {
    use std::io::Read as _;
    let (config, codec, embedding, mut server, mut parent) = ready_server();
    let (document, mut start) = work_start(&config, embedding);
    let supplied = start.document.clone();
    start.document = message::StoredDocument::Cached {
        document: document.identity(),
    };
    start.documents.clear();
    cached_content(
        &mut server,
        parent.try_clone().unwrap(),
        codec,
        &start,
        supplied,
    )
    .unwrap();
    // Reset drops this wire and the run, keeping the embedding's pure code.
    server.wire = None;
    let content = server.content(&start).unwrap();
    assert_eq!(content.document.id(), document.identity());
    parent
        .set_read_timeout(Some(Duration::from_millis(20)))
        .unwrap();
    let error = parent.read(&mut [0]).unwrap_err();
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
}

/// FIG-7354: a cold process has no previous worker's verification. It asks
/// its parent for the exact body once, then owns that verified identity.
#[test]
fn a_cold_worker_requests_and_verifies_one_canonical_document() {
    let (config, codec, embedding, mut server, parent) = ready_server();
    let (document, mut start) = work_start(&config, embedding);
    let supplied = start.document.clone();
    start.document = message::StoredDocument::Cached {
        document: document.identity(),
    };
    start.documents.clear();
    let content = cached_content(&mut server, parent, codec, &start, supplied).unwrap();
    assert_eq!(content.document.body(), document.canonical_bytes());
    let checked = embedding
        .documents
        .get(document.identity(), config.tuning.kernel_data)
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(content.document.shared(), checked.shared()));
    let (_config, _codec, restarted, _server, _parent) = ready_server();
    assert!(
        restarted
            .documents
            .get(document.identity(), config.tuning.kernel_data)
            .unwrap()
            .is_none()
    );
}

/// FIG-7354: digest verification precedes cache insertion at first receipt;
/// a body with another identity is a typed refusal and cannot poison a hit.
#[test]
fn a_tampered_first_document_body_is_refused_before_it_is_cached() {
    let (config, codec, embedding, mut server, parent) = ready_server();
    let (document, mut start) = work_start(&config, embedding);
    let other = lash_kernel_doc::Document::new(lash_kernel_doc::NumberPolicy::Float, Vec::new());
    start.document = message::StoredDocument::Cached {
        document: document.identity(),
    };
    start.documents.clear();
    let result = cached_content(
        &mut server,
        parent,
        codec,
        &start,
        message::StoredDocument::Supplied {
            document: document.identity(),
            body: EncodedPayload(other.canonical_bytes()),
        },
    );
    assert!(matches!(
        result,
        Err(PoolError::Infrastructure(
            InfrastructureOutcome::RunRefused {
                refusal: RunRefusal::Undecodable {
                    input: RunInput::Document,
                    ..
                }
            }
        ))
    ));
    assert!(
        embedding
            .documents
            .get(document.identity(), config.tuning.kernel_data)
            .unwrap()
            .is_none()
    );
}

/// FIG-7354: the pending miss owns no cached admission. Losing the parent
/// at that boundary ends the read and leaves the document absent.
#[test]
fn a_dropped_parent_ends_a_document_miss_without_admitting_code() {
    let (config, codec, embedding, mut server, mut parent) = ready_server();
    let (document, mut start) = work_start(&config, embedding);
    start.document = message::StoredDocument::Cached {
        document: document.identity(),
    };
    start.documents.clear();
    server.fences.lock().unwrap().incoming = Some(MessageFence::new(
        ExecutionLease(0),
        OwnerEpoch(0),
        FrameEpoch(0),
    ));
    let parent = std::thread::spawn(move || {
        let bytes =
            read_frame(&mut parent, &codec, Instant::now() + Duration::from_secs(5)).unwrap();
        assert!(matches!(
            codec.decode_worker(&bytes).unwrap().message,
            WorkerMessage::Documents { .. }
        ));
        // The only peer closes with the identity request outstanding.
    });
    assert!(server.content(&start).is_err());
    parent.join().unwrap();
    assert!(
        embedding
            .documents
            .get(document.identity(), config.tuning.kernel_data)
            .unwrap()
            .is_none()
    );
}
