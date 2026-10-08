use lash_core_execution::FleetFormat;
use lash_vm_client::{
    PoolError,
    service::{Capture, CompiledModule, Request, Response, StateAction, StateMetadata, StateView},
};
use lash_vm_protocol::{Detail, EncodedPayload, PayloadKind, RunInput, RunRefusal, VmStateKind};
use lashlang::VmInstance;

pub(crate) fn perform(
    frontend: &dyn crate::Frontend,
    vm: &mut VmInstance,
    payload: &EncodedPayload,
) -> Result<EncodedPayload, PoolError> {
    let request: Request = rmp_serde::from_slice(&payload.0)
        .map_err(|error| PoolError::payload(PayloadKind::ServiceRequest, error))?;
    let response = match request {
        Request::VerifyArtifact { bytes } => {
            use lash_vm_client::service::ArtifactVerification;
            let verification = match lashlang::ModuleArtifact::from_store_bytes(&bytes) {
                Ok(_) => ArtifactVerification::Match,
                Err(error) => ArtifactVerification::Refused(error.into()),
            };
            Response::ArtifactVerification(verification)
        }
        Request::InspectArtifact { module_ref, bytes } => {
            match lashlang::ModuleArtifact::from_store_bytes(&bytes) {
                Ok(artifact) if artifact.module_ref() == &module_ref => {
                    Response::Artifact(inspect(&artifact)?)
                }
                Ok(artifact) => {
                    Response::ArtifactRefused(lashlang::ModuleArtifactRefusal::Corrupt(
                        lashlang::ModuleArtifactCorruption::StorageKeyMismatch {
                            expected: module_ref.to_string(),
                            actual: artifact.module_ref().to_string(),
                        },
                    ))
                }
                Err(error) => Response::ArtifactRefused(error.into()),
            }
        }

        Request::CreateDefinition {
            source,
            environment,
        } => match compile_module(frontend, &source, &environment, false)? {
            Response::Module(module) => {
                let artifact = lashlang::ModuleArtifact::from_store_bytes(&module.artifact.bytes)
                    .map_err(undecodable_artifact)?;
                let mut processes = artifact.exports().processes.keys();
                match (processes.next(), processes.next()) {
                    (Some(name), None) => {
                        let identity = lashlang::ProcessDefinitionIdentity::from_artifact_export(
                            &artifact, name,
                        )
                        .ok_or_else(|| inconsistent_artifact("missing process export"))?;
                        let signature = lash_core_execution::ProcessSignature::known(
                            lashlang::type_expr_to_json_schema(
                                &identity
                                    .resolve_process_type(&artifact)
                                    .map_err(inconsistent_artifact)?,
                            ),
                        );
                        let draft = lash_core_execution::ProcessDefinitionDraft::new(
                            "lashlang",
                            identity.to_process_value(),
                            [lash_core_execution::ArtifactName {
                                store: lash_core_execution::ArtifactStoreId::LashlangModule,
                                artifact_ref: artifact.module_ref().to_string(),
                            }],
                        )
                        .map_err(inconsistent_artifact)?;
                        Response::Definition(lash_vm_client::service::CreatedDefinition {
                            draft,
                            signature,
                            process_name: name.clone(),
                            module: lash_core_execution::DeclaredModuleArtifact {
                                module_ref: artifact.module_ref().to_string(),
                                bytes: String::from_utf8(module.artifact.bytes)
                                    .map_err(inconsistent_artifact)?,
                            },
                        })
                    }
                    _ => Response::Refused {
                        message: format!(
                            "create_process needs source that defines exactly one process; it defines {}",
                            artifact.exports().processes.len()
                        ),
                        policy: false,
                    },
                }
            }
            refused => refused,
        },
        Request::LinkAst {
            source,
            program,
            environment,
        } => linked_module(&source, program, &environment, false)?,
        Request::CompileAst { program, .. } => {
            match lashlang::ModuleArtifact::from_program(program) {
                Ok(artifact) => {
                    let introspection = artifact.introspect().map_err(inconsistent_artifact)?;
                    Response::Module(compiled_output(lashlang::ModuleCompileOutput {
                        module_ref: artifact.module_ref().clone(),
                        host_requirements_ref: artifact.host_requirements_ref().clone(),
                        artifact,
                        introspection,
                    })?)
                }
                Err(error) => Response::Refused {
                    message: error.to_string(),
                    policy: false,
                },
            }
        }
        #[cfg(feature = "testing")]
        Request::ContinuationProbe {
            bytes,
            remove_first_reference,
        } => {
            let mut parked = crate::worker::ParkedRun::decode(&bytes)?;
            let mut continuation = vm
                .open_continuation(&parked.vm.0)
                .map_err(|error| undecodable_state(VmStateKind::Continuation, error))?;
            let root = continuation
                .operand_stack
                .iter_mut()
                .chain(continuation.slots.iter_mut().flatten())
                .find(|value| matches!(value, lashlang::Value::Ref(_)));
            let closure_root = root.is_some();
            if remove_first_reference && let Some(root) = root {
                *root = lashlang::Value::Null;
            }
            parked.vm = EncodedPayload(
                continuation
                    .to_bytes()
                    .map_err(|error| PoolError::payload(PayloadKind::Continuation, error))?,
            );
            Response::ContinuationProbe {
                bytes: parked.encode()?,
                closure_root,
            }
        }
        Request::ContinuationInfo { bytes } => match crate::worker::ParkedRun::decode(&bytes)
            .and_then(|parked| {
                vm.open_continuation(&parked.vm.0)
                    .map_err(|error| undecodable_state(VmStateKind::Continuation, error))
            }) {
            Ok(continuation) => Response::ContinuationInfo {
                iterator_count: continuation.iterator_stack.len(),
            },
            Err(error) => Response::Refused {
                message: error.to_string(),
                policy: false,
            },
        },
        Request::References { source } => match frontend.parse(&source, None) {
            Ok(program) => Response::References(lashlang::referenced_receiver_call_paths(&program)),
            Err(error) => refusal(error),
        },
        Request::CompileModule {
            source,
            environment,
            cell,
        } => compile_module(frontend, &source, &environment, cell)?,
        Request::OpaqueBindings { snapshot, config } => {
            let snapshot = vm
                .open_snapshot(&snapshot)
                .map_err(|error| undecodable_state(VmStateKind::Snapshot, error))?;
            vm.replace_state(lashlang::State::from_snapshot(snapshot));
            Response::OpaqueBindings(vm.state().opaque_bindings_with(&config))
        }
        Request::State { snapshot, action } => {
            if let Some(snapshot) = snapshot {
                let snapshot = vm
                    .open_snapshot(&snapshot)
                    .map_err(|error| undecodable_state(VmStateKind::Snapshot, error))?;
                vm.replace_state(lashlang::State::from_snapshot(snapshot));
            }
            match action {
                StateAction::Inspect => {}
                StateAction::Insert { name, value } => {
                    vm.state_mut()
                        .insert_global(name, value)
                        .map_err(refused_global)?;
                }
                StateAction::Remove { names } => {
                    for name in names {
                        vm.state_mut().remove_global(&name);
                    }
                }
                StateAction::Defaults { values, protected } => {
                    let mut next = vm.state().clone();
                    for (name, value) in values {
                        if protected.contains(&name) || name == "history" {
                            return Err(PoolError::refused(RunRefusal::ProtectedGlobal));
                        }
                        if !next.binding_names().any(|bound| bound == name) {
                            next.insert_global(name, value).map_err(refused_global)?;
                        }
                    }
                    vm.replace_state(next);
                }
            }
            Response::State(view(vm)?)
        }
        Request::Restore {
            header,
            globals,
            fleet,
        } => {
            match vm.restore_durable_parts(
                &header,
                globals
                    .iter()
                    .map(|(name, bytes)| (name.as_str(), bytes.as_slice())),
                FleetFormat::from_version(fleet),
            ) {
                Ok(_) => Response::Restored {
                    view: view(vm)?,
                    baseline: capture(vm, &Default::default(), fleet)?.baseline,
                },
                Err(error) => Response::SnapshotRefused(error),
            }
        }
        Request::Capture {
            snapshot,
            baseline,
            fleet,
        } => {
            let snapshot = vm
                .open_snapshot(&snapshot)
                .map_err(|error| undecodable_state(VmStateKind::Snapshot, error))?;
            vm.replace_state(lashlang::State::from_snapshot(snapshot));
            Response::Captured(capture(vm, &baseline, fleet)?)
        }
    };
    Ok(EncodedPayload(rmp_serde::to_vec_named(&response).map_err(
        |error| PoolError::payload(PayloadKind::ServiceResponse, error),
    )?))
}
fn undecodable_artifact(error: impl std::fmt::Display) -> PoolError {
    PoolError::refused(RunRefusal::Undecodable {
        input: RunInput::Artifact,
        detail: Detail::new(error),
    })
}
fn undecodable_state(kind: VmStateKind, error: impl std::fmt::Display) -> PoolError {
    PoolError::refused(RunRefusal::Undecodable {
        input: RunInput::State { kind },
        detail: Detail::new(error),
    })
}
/// The artifact decoded and yet contradicts itself: the same on every attempt.
fn inconsistent_artifact(error: impl std::fmt::Display) -> PoolError {
    PoolError::refused(RunRefusal::Artifact {
        detail: Detail::new(error),
    })
}
fn refused_global(error: impl std::fmt::Display) -> PoolError {
    PoolError::refused(RunRefusal::Global {
        detail: Detail::new(error),
    })
}
fn refusal(error: crate::FrontendRefusal) -> Response {
    Response::CompileRefused {
        error: error.error,
        policy: error.policy,
    }
}
fn compile_module(
    frontend: &dyn crate::Frontend,
    source: &str,
    environment: &lashlang::LashlangHostEnvironment,
    cell: bool,
) -> Result<Response, PoolError> {
    match frontend.parse(source, cell.then_some(environment)) {
        Ok(program) => linked_module(source, program, environment, cell),
        Err(error) => Ok(refusal(error)),
    }
}
fn linked_module(
    source: &str,
    program: lashlang::Program,
    environment: &lashlang::LashlangHostEnvironment,
    cell: bool,
) -> Result<Response, PoolError> {
    match lashlang::LinkedModule::link(program, environment) {
        Ok(linked) => {
            let artifact = linked.artifact;
            let introspection = artifact.introspect().map_err(inconsistent_artifact)?;
            Ok(Response::Module(compiled_output(
                lashlang::ModuleCompileOutput {
                    module_ref: artifact.module_ref().clone(),
                    host_requirements_ref: artifact.host_requirements_ref().clone(),
                    artifact,
                    introspection,
                },
            )?))
        }
        Err(error) => {
            let policy = matches!(
                error,
                lashlang::LinkError::BareToolCall { .. }
                    | lashlang::LinkError::FeatureDisabled { .. }
                    | lashlang::LinkError::OpaqueHostDescriptorAccess { .. }
                    | lashlang::LinkError::ProcessLifecycleOutsideProcess { .. }
            );
            let mut diagnostic = lashlang::format_link_diagnostic(source, &error);
            if cell && let lashlang::LinkError::BareToolCall { suggestion, .. } = &error {
                let suffix = diagnostic
                    .find('\n')
                    .map(|i| &diagnostic[i..])
                    .unwrap_or("");
                diagnostic = format!(
                    "bare tool calls are not allowed; call the module operation instead.{suffix}"
                );
                if !suggestion.is_empty() {
                    diagnostic.push_str(&format!("\nhint: use `{suggestion}`"));
                }
            }
            Ok(Response::CompileRefused {
                error: lashlang::ModuleCompileError::Link(lashlang::ModuleCompileDiagnostic {
                    message: error.to_string(),
                    span: error.span(),
                    diagnostic: Some(diagnostic),
                }),
                policy,
            })
        }
    }
}
fn view(vm: &VmInstance) -> Result<StateView, PoolError> {
    Ok(StateView {
        snapshot: vm
            .state()
            .snapshot()
            .to_canonical_bytes()
            .map_err(|error| PoolError::payload(PayloadKind::Snapshot, error))?,
        metadata: state_metadata(vm),
    })
}

pub(crate) fn state_metadata(vm: &VmInstance) -> StateMetadata {
    StateMetadata {
        definition_ids: vm.state().referenced_definition_ids(),
        globals: vm.state().globals().clone(),
        names: vm.state().binding_names().map(str::to_string).collect(),
        expired: vm.state().expired_functions().clone(),
        opaque: vm.state().opaque_bindings(),
    }
}

fn capture(
    vm: &VmInstance,
    since: &std::collections::BTreeMap<String, String>,
    fleet: u32,
) -> Result<Capture, PoolError> {
    let parts = vm
        .state()
        .durable_parts(&Default::default(), FleetFormat::from_version(fleet))
        .map_err(|error| PoolError::payload(PayloadKind::Snapshot, error))?;
    let mut baseline = std::collections::BTreeMap::new();
    let fragments = parts
        .fragments
        .into_iter()
        .map(|(name, fragment)| match fragment {
            lashlang::DurableFragment::Changed(bytes) => {
                let digest = blake3::hash(&bytes).to_hex().to_string();
                let unchanged = since.get(&name) == Some(&digest);
                baseline.insert(name.clone(), digest);
                (
                    name,
                    if unchanged {
                        lashlang::DurableFragment::Unchanged
                    } else {
                        lashlang::DurableFragment::Changed(bytes)
                    },
                )
            }
            other => (name, other),
        })
        .collect();
    Ok(Capture {
        definition_ids: vm.state().referenced_definition_ids(),
        header: parts.header,
        fragments,
        baseline,
    })
}

fn compiled_output(
    output: lashlang::ModuleCompileOutput,
) -> Result<Box<CompiledModule>, PoolError> {
    Ok(Box::new(CompiledModule {
        module_ref: output.module_ref,
        host_requirements_ref: output.host_requirements_ref,
        artifact: inspect(&output.artifact)?,
        introspection: output.introspection,
    }))
}
fn inspect(
    artifact: &lashlang::ModuleArtifact,
) -> Result<lash_vm_client::InspectedArtifact, PoolError> {
    let mut processes = std::collections::BTreeMap::new();
    for name in artifact.exports().processes.keys() {
        let process = artifact
            .ir()
            .process(name)
            .ok_or_else(|| inconsistent_artifact("missing artifact export"))?;
        processes.insert(
            name.clone(),
            lash_vm_client::ProcessMetadata {
                lifted: process.origin.is_lifted(),
                params: process
                    .params
                    .iter()
                    .map(|param| (param.name.to_string(), artifact.resolve_type(&param.ty)))
                    .collect(),
                process_type: artifact.process_type(name),
            },
        );
    }
    Ok(lash_vm_client::InspectedArtifact {
        bytes: artifact.to_store_bytes().map_err(inconsistent_artifact)?,
        module_ref: artifact.module_ref().clone(),
        host_requirements_ref: artifact.host_requirements_ref().clone(),
        host_requirements: artifact.host_requirements().clone(),
        exports: artifact.exports().clone(),
        source_identity: artifact.source_identity(),
        processes,
        graph: lashlang::workflow_graph_from_artifact(artifact, &lashlang::NoStatementText),
    })
}
