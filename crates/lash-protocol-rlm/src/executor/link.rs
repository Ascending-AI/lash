//! Link a fresh cell against Current, or restore its admitted envelope.
use super::*;
use lash_core::core_internal::RuntimeExecutionContextRuntimeOps as _;

/// Lower a fresh cell against its surface, or restore its admitted envelope.
pub(super) async fn link_cell(
    state: &mut CodeModeExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
    source: &str,
    services: &CellServices,

    snapshots: &lash_vm_broker::DurableSnapshotStore,
    resumed: Option<envelope::ResumedCell>,
) -> Result<LinkedCell, Refused> {
    if let Some(resumed) = resumed {
        // A resumed cell runs what its first activation admitted: the
        // envelope carries the document and everything it was lowered
        // against, and nothing here is journaled or lowered again.
        ctx.restore_started_process_ids(&resumed.started_process_ids);
        let envelope = resumed.cell;

        if envelope.dialect != state.dialect() {
            return Err(host_failure(format!(
                "the cell was parked as `{}` and this session's dialect is `{}`",
                envelope.dialect,
                state.dialect()
            )));
        }
        // An end an earlier activation recorded names the session as that
        // activation left it, which a read-back end does not carry: the
        // session adopts it, and reads its pages under a lease on the
        // cell's ended root (FIG-6315), which the turn names once the cell
        // answers. The session names that root itself as it adopts the
        // end: its own reads before the turn's next one (the live catalog
        // check of the next sync) reach the pages no run of it holds
        // (FIG-6442).
        if let Some(session) = &resumed.session {
            state
                .restore_execution_graph(
                    session,
                    lash_core_store::execution_state::ExecutionGraphPurpose::Working,
                )
                .map_err(|error| {
                    host_failure(format!(
                        "the session the cell's recorded end names does not restore: {error}"
                    ))
                })?;
            state.note_pages(ended_root_pages(snapshots, resumed.root).as_ref());
        }
        ctx.cell_execution_registered(execution_id(snapshots.exec()));
        let documents = state
            .documents
            .as_ref()
            .ok_or_else(|| host_failure("the cell has no document owner"))?;
        let code = processes::load(
            documents,
            &lash_kernel_value::LinkedStart {
                link: envelope.link,
                selector: lash_kernel_value::StartSelector::Main,
            },
            &envelope.environment.authored,
        )
        .await
        .map_err(host_failure)?;
        let invoked = code
            .content
            .inspect_invocations(
                envelope.link,
                &lash_kernel_package::ReachSelection::Start(lash_kernel_value::StartSelector::Main),
                documents.helpers().as_ref(),
            )
            .map_err(|error| package_error(lash_vm_runtime::map_link_refusal(error)))?
            .operations
            .into_keys()
            .collect();
        return Ok(LinkedCell {
            invoked,
            document: envelope.document.clone(),

            envelope,
            // The machine resumes from its parked state, not from a start.
            ledgers: resumed.ledgers,
            graph: resumed.graph,
            open_sites: resumed.open_sites,
            resumed: true,
            code,
            frame_links: BTreeSet::new(),
        });
    }

    state
        .prepare_cell(ctx.session_pages())
        .await
        .map_err(|error| host_failure(error.to_string()))?;
    let packages = state
        .shown_packages()
        .ok_or_else(|| host_failure("the round recorded no package surface"))?;
    let context = packages
        .consumer
        .clone()
        .ok_or_else(|| host_failure("the round recorded no trusted consumer context"))?;
    let documents = state
        .documents
        .as_ref()
        .ok_or_else(|| host_failure("the cell has no document owner"))?
        .clone();
    let boundary = HostBoundary {
        snapshot: packages.snapshot.clone(),
        context: context.clone(),
        attachment: lash_kernel_value::package::Attachment {
            members: BTreeSet::new(),
            packages: packages
                .snapshot
                .advertisements
                .iter()
                .filter_map(|accepted| match &accepted.advertisement {
                    lash_kernel_value::PackageAdvertisement::Enumerated(package) => {
                        Some(package.path.clone())
                    }
                    _ => None,
                })
                .collect(),
            deferred: packages
                .snapshot
                .advertisements
                .iter()
                .filter_map(|accepted| match &accepted.advertisement {
                    lash_kernel_value::PackageAdvertisement::Deferred(package) => {
                        Some(package.path.clone())
                    }
                    _ => None,
                })
                .collect(),
        },
    };
    // A parked frame may have saved predecessor-format values as it ended.
    // Carry their own code before FIG-6926 fulfils them for this fresh cell.
    let frame_claim = ctx
        .frame_claim()
        .map_err(|error| host_failure(error.to_string()))?;
    state
        .succeed_saved_functions(Some(&frame_claim))
        .await
        .map_err(|error| match error {
            lash_core::SessionError::Plugin(
                lash_execution_contracts::plugin_error::PluginError::RuntimeEffectController(error),
            ) => Refused::Nested(Box::new(error)),
            other => host_failure(other.to_string()),
        })?;
    // Saved declarations belong to the frame, with each cell's current Link.
    let mut code = lash_vm_runtime::CellCode::default();
    let authored = state
        .relink(&documents, &boundary, &frame_claim, &mut code)
        .await
        .map_err(|error| match error {
            session::RelinkError::Package(error) => package_error(error),
            other => host_failure(other.to_string()),
        })?;
    let namespaces = boundary
        .namespaces(&documents)
        .await
        .map_err(package_error)?;
    let names = state.names();

    let lowered = services
        .workers
        .request_for_run(Request::Lower {
            dialect: state.dialect().to_owned(),
            source: source.to_owned(),
            namespaces,
            bindings: names,
            functions: state
                .lowering_functions()
                .into_iter()
                .map(|(name, function)| (name, function.written))
                .collect(),
        })
        .await;
    let (document, source_map, mut written, function_values) = match lowered {
        Ok(Response::Lowered {
            document,
            source_map,
            written,
            function_values,
            ..
        }) => (document, source_map, written, function_values),
        Ok(Response::DialectRefused(refusal)) => {
            let observation = CellObservation::of_refusal(refusal);
            trace::emit_step_trace(ctx, Err(&observation.code()));
            return Err(Refused::Cell(Box::new(observation.failure_with_repairs(
                source,
                None,
                services.prompts.as_ref(),
                Some(&packages.snapshot),
                ctx,
            ))));
        }
        Ok(Response::UnknownDialect { dialect }) => {
            return Err(host_failure(format!("no worker has dialect {dialect}")));
        }
        Ok(_) => {
            return Err(Refused::Worker(Box::new(
                lash_vm_client::PoolError::breach(
                    lash_protocol_types::engine_fault::SequenceFault::UnexpectedServiceResponse,
                ),
            )));
        }
        Err(error) => return Err(Refused::Worker(Box::new(error))),
    };
    trace::emit_step_trace(ctx, Ok(()));
    // The worker's token body is decoded here, once: the identity is its
    // digest, and the envelope, the store and every checkout share all
    // three.
    let document =
        IdentifiedDocument::decode(&document, lash_kernel_value::KernelDataLimits::standard())
            .map_err(|error| host_failure(format!("the worker's document is not one: {error}")))?;
    let claim = ctx
        .execution_claim()
        .map_err(|error| host_failure(error.to_string()))?;
    let mut fulfilled = documents
        .fulfil_cell(
            &document,
            &lash_execution_contracts::process::linking::FulfilRequest {
                document: document.id(),
                selector: lash_execution_contracts::process::linking::ReachSelector::Start(
                    lash_kernel_value::names::StartSelector::Main,
                ),
                snapshot: packages.snapshot.clone(),
                context,
                purpose: lash_execution_contracts::process::linking::LinkPurpose::Cell,
            },
            &mut code,
        )
        .await
        .map_err(package_error)?;
    for link in &authored {
        let saved = documents
            .inspect_cell(
                &code.content,
                *link,
                &lash_execution_contracts::process::linking::ReachSelector::AllExports,
            )
            .map_err(package_error)?;
        fulfilled.inspection.operations.extend(saved.operations);
        fulfilled.inspection.machine.extend(&saved.machine);
        fulfilled.inspection.artifacts.extend(saved.artifacts);
    }
    let mut environment = lash_core::facade_support::select_routes(
        &fulfilled.inspection,
        lash_vocabulary::claims::RegistryKind::Session,
        &packages.routes,
        &Default::default(),
        packages.publications(),
        ctx.package_composition().as_ref(),
    )
    .map_err(package_error)?;
    // Cancellable tool reads need broker admission before their body can run.
    // Only the immutable dialect service packages keep statement-local reads.
    for (use_, binding) in &mut environment.uses {
        if let lash_core_store::tool_surface::UseBinding::Fixed(operation) = binding
            && operation.mode == lash_kernel_value::OperationMode::Read
        {
            let local = fulfilled.inspection.operations.get(use_).is_some_and(|need| {
                matches!(need, lash_execution_contracts::process::linking::OperationNeed::Fixed { name, .. }
                    if lash_vm_runtime::DIALECT_SERVICE_PACKAGES.iter()
                        .any(|package| package.path == name.package
                            && matches!(package.members.get(&name.member),
                                Some(lash_kernel_value::package::Member { kind:
                                    lash_kernel_value::package::MemberKind::Operation(offer), .. })
                                    if offer.requirement == operation.requirement
                                        && offer.mode == lash_kernel_value::OperationMode::Read)))
            });
            if !local {
                operation.mode = lash_kernel_value::OperationMode::Waited;
            }
        }
    }
    environment.retained_code.extend(
        state
            .lowering_functions()
            .values()
            .map(|function| function.callable),
    );
    let frame_links = authored.clone();
    environment.authored.extend(authored);
    environment.authored.insert(fulfilled.inspection.link);
    documents
        .hold_cell_code(
            &claim,
            &code,
            &[fulfilled.inspection.link].into(),
            &environment.publications,
        )
        .await
        .map_err(package_error)?;
    let execution = execution_id(snapshots.exec());
    let registration = lash_core::facade_support::registration(
        execution.clone(),
        &fulfilled.inspection,
        claim.referrer(),
        environment.requirements.clone(),
        environment.registry,
    );
    if !ctx.hold_cell_registration(&registration) {
        let mut tx = lash_core::facade_support::execution_actor(ctx)
            .begin()
            .await
            .map_err(|error| host_failure(error.to_string()))?;
        tx.register_execution(registration);
        lash_core::facade_support::execution_actor(ctx)
            .commit(tx, lash_durable::CommitLabel::new("execution.register"))
            .await
            .map_err(|error| host_failure(error.to_string()))?;
    }
    ctx.cell_execution_registered(execution);
    for function in state.lowering_functions().values() {
        if let Some(facts) = &function.written {
            written.insert(
                lash_kernel_value::sites::Site::new(
                    lash_kernel_value::sites::Unit::Library(function.callable.function),
                    Vec::new(),
                ),
                facts.clone(),
            );
        }
    }
    let envelope = CellEnvelope {
        saved_written: state
            .lowering_functions()
            .values()
            .filter_map(|function| {
                function
                    .written
                    .clone()
                    .map(|origin| (function.environment, origin))
            })
            .collect(),
        code: CellEnvelope::code_digest(source),
        dialect: state.dialect().to_owned(),
        link: fulfilled.inspection.link,
        inspection: fulfilled.inspection,
        environment,
        document,
        source_map,
        written,
        function_values,
    };
    let document = envelope.document.clone();
    if let Some(map) = &envelope.source_map {
        map.validate_identified(&document, source)
            .map_err(|error| {
                host_failure(format!(
                    "the worker's source map is not the cell's: {error}"
                ))
            })?;
    }

    let invoked = code
        .content
        .inspect_invocations(
            envelope.link,
            &lash_kernel_package::ReachSelection::Start(lash_kernel_value::StartSelector::Main),
            documents.helpers().as_ref(),
        )
        .map_err(|error| package_error(lash_vm_runtime::map_link_refusal(error)))?
        .operations
        .into_keys()
        .collect();
    Ok(LinkedCell {
        invoked,
        envelope,
        document,
        ledgers: CellHostLedgers::default(),
        graph: host::HostGraph::default(),
        open_sites: BTreeMap::new(),
        resumed: false,
        code,
        frame_links,
    })
}

fn package_error(error: lash_execution_contracts::process::linking::EnginePortError) -> Refused {
    Refused::Nested(Box::new(
        lash_execution_contracts::plugin_error::PluginError::from(error).into(),
    ))
}

pub(super) fn execution_id(
    exec: &lash_vm_broker::ExecKey,
) -> lash_core_store::resolutions::ExecutionId {
    match exec {
        lash_vm_broker::ExecKey::Cell(session, turn, cell) => {
            lash_core_store::resolutions::ExecutionId::cell(session, turn, cell.as_str())
        }
        lash_vm_broker::ExecKey::Process(process) => {
            lash_core_store::resolutions::ExecutionId::process(process)
        }
    }
}
