//! Link a fresh cell under its selected package set, or restore its admitted envelope.
use super::*;

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
        if services.packages.is_some() {
            snapshots.stamp_parks_with(envelope.packages);
        }
        return Ok(LinkedCell {
            document: envelope.document.clone(),
            boundary: envelope.boundary().map_err(host_failure)?,
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

    // Each catalog tool is an offered operation, under its canonical name:
    // the effect a cell of any dialect performs it as.
    let mut boundary = HostBoundary::new();
    // The catalog tools that declare a read, by effect: what the cell's
    // read plan is drawn from.
    let mut read_tools = BTreeMap::new();
    for tool in ctx.tool_catalog().entries() {
        boundary
            .offer_operation(
                tool.manifest.operation.clone(),
                lash_vm_runtime::offered_tool(
                    &tool.manifest,
                    &tool.contract,
                    lash_kernel_dialect::OfferedSource::Catalog {
                        tool: tool.manifest.id.to_string(),
                    },
                ),
            )
            .map_err(|error| host_failure(format!("invalid host tool surface: {error}")))?;
        if tool.manifest.execution_policy == lash_core::ExecutionPolicy::Read {
            read_tools.insert(
                tool.manifest.operation.effect_name(),
                tool.manifest.id.clone(),
            );
        }
    }

    state
        .prepare_cell(ctx.session_pages())
        .await
        .map_err(|error| host_failure(format!("the session's state is not readable: {error}")))?;
    let packages = packages::select(state, services).await?;
    if services.packages.is_some() {
        snapshots.stamp_parks_with(packages.digest());
    }
    // Every carried saved function is relinked by name to the cell's
    // surface, or dropped naming it (RELINK-CLOSURE): what the cell lowers
    // and what it calls of the session come from the one surface
    // (CURRENT-RELEASE).
    state.relink(&packages).await.map_err(|error| {
        host_failure(format!(
            "the session's saved functions are not relinked: {error}"
        ))
    })?;
    let package_view = packages::prepare(&packages, &mut boundary);
    // On a path a package of the cell's surface owns, the package's
    // declared contract is authoritative: a tool that does not serve it is
    // refused (PKG-RUNNER).
    for tool in ctx.tool_catalog().entries() {
        let effect = tool.manifest.operation.effect_name();
        if let Some(owner) = lash_vm_runtime::owned_operation(&package_view, &effect) {
            lash_execution_contracts::process::check_runner(
                &effect,
                &owner,
                &tool.manifest,
                &tool.contract,
            )
            .map_err(|refusal| host_failure(format!("invalid host tool surface: {refusal}")))?;
        }
    }
    let mut names = state.names();
    names.extend(projected.keys().map(|name| Name::new(name.as_str())));

    // gather → decide → offer: the run's deferred provider matches the
    // paths no surface tool answers, and the matches commit before the cell
    // is lowered. A committed match is never resolved again.
    let mut discovery_failure = None;
    let outcomes =
        if ctx.parent_invocation().is_some() && !matches!(ctx.deferred_provider(), Ok(None)) {
            let _phase = ctx.named_phase("codemode_lash_vm.deferred_resolve");
            let calls = services
                .workers
                .request_accounted(Request::ToolCalls {
                    dialect: state.dialect().to_owned(),
                    source: code.to_owned(),
                    bindings: names.clone(),
                    functions: state.lowering_functions(),
                    operations: boundary.operations().clone(),
                    packages: crate::plugin::package_wire(&packages),
                })
                .await;
            let calls = match calls {
                Ok(Response::ToolCalls(calls)) => calls,
                Ok(Response::Lowered { .. } | Response::Printed(_) | Response::Read { .. }) => {
                    discovery_failure = Some(Err(lash_vm_client::PoolError::breach(
                        lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
                    )));
                    BTreeSet::new()
                }
                other => {
                    discovery_failure = Some(other);
                    BTreeSet::new()
                }
            };
            let candidates = calls
                .into_iter()
                .filter(|path| {
                    !lash_kernel_doc::EffectName::new(path.as_str())
                        .is_ok_and(|name| boundary.effect(&name).is_some())
                })
                .collect::<BTreeSet<_>>();
            if candidates.is_empty() {
                BTreeMap::new()
            } else {
                crate::deferred::journal_deferred_matches(candidates, ctx, &package_view)
                    .await
                    .map_err(|error| Refused::Nested(Box::new(error.runtime_effect_error())))?
                    .unwrap_or_default()
            }
        } else {
            BTreeMap::new()
        };
    if let Some((_, matched)) = outcomes
        .iter()
        .find(|(path, _)| lash_kernel_dialect::ExportView::owns_namespace(&package_view, path))
        && let lash_core::DeferredOutcome::Refused { reason } = &matched.outcome
    {
        return Err(host_failure(reason.clone()));
    }
    let grants = crate::deferred::offer_deferred_matches(&mut boundary, &outcomes, ctx)
        .map_err(|error| Refused::Nested(Box::new(error.runtime_effect_error())))?;
    let unavailable_notes = crate::deferred::offer_unavailable_notes(&outcomes, &mut boundary);
    // Each offered tool by its operation, as this dialect's cells call it:
    // what a repair names.
    let spelling = services.prompts.spelling();
    let grant_calls = boundary
        .operations()
        .iter()
        .filter(|(_, offered)| !matches!(offered.source, OfferedSource::Unavailable { .. }))
        .map(|(operation, _)| {
            (
                operation.effect_name().to_string(),
                lash_kernel_dialect::spell_operation(spelling, operation),
            )
        })
        .collect::<BTreeMap<_, _>>();
    let candidates = read_candidates(ctx, &boundary, &grants, read_tools);
    // The worker plans as reads exactly the candidates; every other
    // offered operation stays a recorded perform.
    let mut operations = boundary.operations().clone();
    for (operation, offered) in &mut operations {
        offered.read = candidates.contains_key(&operation.effect_name());
    }
    // The worker that lowers the cell starts its run (FIG-5921).
    let lowered = match discovery_failure {
        Some(failure) => failure,
        None => {
            services
                .workers
                .request_for_run(Request::Lower {
                    dialect: state.dialect().to_owned(),
                    source: code.to_owned(),
                    operations,
                    bindings: names,
                    functions: state.lowering_functions(),
                    packages: crate::plugin::package_wire(&packages),
                })
                .await
        }
    };
    let (document, source_map, written, function_values, reached) = match lowered {
        Ok(Response::Lowered {
            document,
            source_map,
            written,
            function_values,
            reads,
            ..
        }) => (document, source_map, written, function_values, reads),
        Ok(Response::DialectRefused(mut refusal)) => {
            let feedback =
                crate::feedback::contribute_repairs(&mut refusal, code, &grant_calls, ctx);
            let observation = CellObservation::of_refusal(refusal);
            trace::emit_step_trace(ctx, Err(&observation.code()));
            let mut failure =
                observation.failure(code, None, services.channel, services.prompts.as_ref());
            failure.feedback = feedback;
            return Err(Refused::Cell(Box::new(failure)));
        }
        Ok(Response::UnknownDialect { dialect }) => {
            return Err(host_failure(format!(
                "this session's cells are written in `{dialect}`, and no worker has that dialect installed"
            )));
        }
        Ok(Response::PackagesNotHeld { packages }) => {
            return Err(lash_core::CellFailure::new(
                lash_core::CellFailureKind::PackagesNotHeld { packages },
                format!("this cell is written under package set {packages}, and the worker does not hold it"),
            ).into());
        }
        Ok(Response::Printed(_) | Response::Read { .. } | Response::ToolCalls(_)) => {
            return Err(Refused::Worker(Box::new(
                lash_vm_client::PoolError::breach(
                    lash_vm_protocol::SequenceFault::UnexpectedServiceResponse,
                ),
            )));
        }
        Err(error) => {
            trace::emit_step_trace(ctx, Err(&error.to_string()));
            return Err(Refused::Worker(Box::new(error)));
        }
    };
    trace::emit_step_trace(ctx, Ok(()));
    // The worker's token body is decoded here, once: the identity is its
    // digest, and the envelope, the store and every checkout share all
    // three.
    let document =
        IdentifiedDocument::decode(&document, lash_kernel_doc::KernelDataLimits::standard())
            .map_err(|error| host_failure(format!("the worker's document is not one: {error}")))?;
    packages::admit(&document, &boundary, &packages)?;
    let envelope = CellEnvelope {
        code: CellEnvelope::code_digest(code),
        dialect: state.dialect().to_owned(),
        packages: packages.digest(),
        document,
        source_map,
        written,
        function_values,
        effects: CellEnvelope::record_effects(&boundary, |tool| {
            ctx.tool_catalog()
                .entries()
                .iter()
                .map(|entry| &entry.manifest)
                .chain(grants.values().map(lash_core::ToolExecutionGrant::manifest))
                .find(|manifest| &manifest.id == tool)
                .map(|manifest| manifest.declaration().controls.clone())
                .unwrap_or_default()
        }),
        grants,
        unavailable_notes,
        projected: projected.keys().cloned().collect(),
        reads: candidates
            .into_iter()
            .filter(|(effect, _)| reached.contains(effect))
            .collect(),
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
                crate::cell_value::json_datum(value, document.manifest.numbers),
            )
        })
        .collect();
    Ok(LinkedCell {
        envelope,
        document,
        boundary,
        host,
        ledgers: CellHostLedgers::default(),
        graph: host::HostGraph::default(),
        open_sites: BTreeMap::new(),
        resumed: false,
    })
}
