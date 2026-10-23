//! Link a fresh cell against Current, or restore its admitted envelope.
use super::*;
use lash_core::core_internal::RuntimeExecutionContextRuntimeOps as _;

/// Lower a fresh cell against its surface, or restore its admitted envelope.
pub(super) async fn link_cell(
    state: &mut CodeModeExecutionState,
    ctx: &RuntimeExecutionContext<'_>,
    code: &str,
    services: &CellServices,
    session_projected_bindings: CodeModeProjectedBindings,
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
        return Ok(LinkedCell {
            document: envelope.document.clone(),

            envelope,
            // The machine resumes from its parked state, not from a start.
            host: Vec::new(),
            ledgers: resumed.ledgers,
            graph: resumed.graph,
            open_sites: resumed.open_sites,
            resumed: true,
        });
    }

    // The host's read-only bindings are recorded under the cell's effect
    // before it runs, so a redrive of the cell starts with the same ones.
    let projected = {
        let _phase = ctx.named_phase("codemode_lash_vm.resolve_projected_bindings");
        crate::projection::cell_host_bindings(ctx, session_projected_bindings, code)
            .await
            .map_err(|error| match error {
                crate::projection::HostBindingsError::Journal(error) => {
                    Refused::Nested(Box::new(error))
                }
                crate::projection::HostBindingsError::Refused(message) => host_failure(message),
            })?
    };

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
        attachment: lash_vocabulary::package::Attachment {
            members: BTreeSet::new(),
            packages: packages
                .snapshot
                .advertisements
                .iter()
                .filter_map(|accepted| match &accepted.advertisement {
                    lash_vocabulary::PackageAdvertisement::Enumerated(package) => {
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
                    lash_vocabulary::PackageAdvertisement::Deferred(package) => {
                        Some(package.path.clone())
                    }
                    _ => None,
                })
                .collect(),
        },
    };
    // Saved declarations are frame globals. Their immutable Document/Link
    // artifacts outlive each turn's journal and retire with that same frame.
    let authored = state
        .relink(
            &documents,
            &boundary,
            &ctx.frame_claim()
                .map_err(|error| host_failure(error.to_string()))?,
        )
        .await
        .map_err(|error| match error {
            session::RelinkError::Package(error) => package_error(error),
            other => host_failure(other.to_string()),
        })?;
    let namespaces = boundary
        .namespaces(&documents)
        .await
        .map_err(package_error)?;
    let mut names = state.names();
    names.extend(projected.keys().map(|name| Name::new(name.as_str())));

    let lowered = services
        .workers
        .request_for_run(Request::Lower {
            dialect: state.dialect().to_owned(),
            source: code.to_owned(),
            namespaces,
            bindings: names,
            functions: state.lowering_functions(),
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
            return Err(Refused::Cell(Box::new(observation.failure(
                code,
                None,
                services.channel,
                services.prompts.as_ref(),
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
        IdentifiedDocument::decode(&document, lash_vocabulary::KernelDataLimits::standard())
            .map_err(|error| host_failure(format!("the worker's document is not one: {error}")))?;
    let claim = ctx
        .execution_claim()
        .map_err(|error| host_failure(error.to_string()))?;
    let id = documents
        .admit_identified_document(&claim, &document)
        .await
        .map_err(package_error)?;
    let mut fulfilled = documents
        .fulfil(&lash_execution_contracts::process::linking::FulfilRequest {
            document: id,
            selector: lash_execution_contracts::process::linking::ReachSelector::Start(
                lash_vocabulary::names::StartSelector::Main,
            ),
            snapshot: packages.snapshot.clone(),
            context,
            purpose: lash_execution_contracts::process::linking::LinkPurpose::Cell,
        })
        .await
        .map_err(package_error)?;
    for link in &authored {
        let saved = documents
            .inspect_link(
                *link,
                &lash_execution_contracts::process::linking::ReachSelector::AllExports,
            )
            .await
            .map_err(package_error)?;
        fulfilled.inspection.operations.extend(saved.operations);
        fulfilled
            .inspection
            .machine
            .kernels
            .extend(saved.machine.kernels);
        fulfilled
            .inspection
            .machine
            .natives
            .extend(saved.machine.natives);
        fulfilled.inspection.artifacts.extend(saved.artifacts);
    }
    let mut environment = lash_core::facade_support::select_routes(
        &fulfilled.inspection,
        lash_vocabulary::claims::RegistryKind::Session,
        &packages.routes,
        packages.publications(),
        |grant, registry| {
            ctx.package_composition()
                .validate_deferred_grant(grant, registry)
        },
    )
    .map_err(package_error)?;
    environment.authored.extend(authored);
    environment.authored.insert(fulfilled.inspection.link);
    documents
        .hold(
            &claim,
            &lash_execution_contracts::process::linking::ContentRef::Link(
                fulfilled.inspection.link,
            ),
        )
        .await
        .map_err(package_error)?;
    let package_store = ctx.actor_context().backend().stores().package_store();
    for publication in &environment.publications {
        let key = publication.key();
        package_store
            .acquire_publication(&claim, &key)
            .await
            .map_err(|error| host_failure(error.to_string()))?;
        if package_store
            .publication(&key)
            .await
            .map_err(|error| host_failure(error.to_string()))?
            .as_ref()
            != Some(publication)
        {
            return Err(host_failure(
                "accepted publication differs from retained provenance",
            ));
        }
    }
    let execution = execution_id(snapshots.exec());
    let registration = lash_core::facade_support::registration(
        execution.clone(),
        &fulfilled.inspection,
        claim.referrer(),
        environment.requirements.clone(),
        environment.registry,
    );
    if !ctx.hold_cell_registration(&registration) {
        let mut tx = ctx
            .actor_context()
            .begin()
            .await
            .map_err(|error| host_failure(error.to_string()))?;
        tx.register_execution(registration);
        ctx.actor_context()
            .commit(tx, lash_durable::CommitLabel::new("execution.register"))
            .await
            .map_err(|error| host_failure(error.to_string()))?;
    }
    ctx.cell_execution_registered(execution);
    for function in state.lowering_functions().values() {
        if let Some(facts) = &function.written {
            written.insert(
                lash_vocabulary::sites::Site::new(
                    lash_vocabulary::sites::Unit::Library(function.callable.function),
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
        code: CellEnvelope::code_digest(code),
        dialect: state.dialect().to_owned(),
        link: fulfilled.inspection.link,
        inspection: fulfilled.inspection,
        environment,
        document,
        source_map,
        written,
        function_values,
        projected: projected.keys().cloned().collect(),
    };
    let document = envelope.document.clone();
    if let Some(map) = &envelope.source_map {
        map.validate_identified(&document, code).map_err(|error| {
            host_failure(format!(
                "the worker's source map is not the cell's: {error}"
            ))
        })?;
    }

    // `main` starts with the session's bindings and, over them, the host's
    // read-only ones.
    let host = projected
        .iter()
        .map(|(name, value)| {
            (
                Name::new(name.as_str()),
                crate::cell_value::json_datum(value, document.numbers),
            )
        })
        .collect();
    Ok(LinkedCell {
        envelope,
        document,
        host,
        ledgers: CellHostLedgers::default(),
        graph: host::HostGraph::default(),
        open_sites: BTreeMap::new(),
        resumed: false,
    })
}

fn package_error(error: lash_execution_contracts::process::linking::EnginePortError) -> Refused {
    Refused::Nested(Box::new(
        lash_execution_contracts::plugin_error::PluginError::Package(Box::new(error)).into(),
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
