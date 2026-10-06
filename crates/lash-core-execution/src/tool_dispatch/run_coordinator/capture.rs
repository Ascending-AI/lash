//! Canonical X capture and declaration admission.
use super::*;

pub(super) async fn capture_attempt(
    owner: MaterialOwner,
    call: &SingletonToolCall,
    member: &AdmittedCall,
    request: &SingletonPreparedRequest,
    handlers: &dyn SingletonToolHandlers,
    ordinal: AttemptOrdinal,
    sources: AttemptSources<'_>,
) -> Result<crate::tool_run::RunAttemptEntry, String> {
    let AttemptSources {
        completion: completion_key,
        process: process_source,
    } = sources;
    let declaration = &member.declaration;
    // The obligation material a declared start owns in this record.
    let mut started = Vec::new();
    let recorder = AttemptStreamRecorder::start();
    let outcome = if request.isolation.is_some() {
        None
    } else {
        Some(
            handlers
                .execute(SingletonAttempt {
                    call_id: &call.call_id,
                    attempt: ordinal,
                    request,
                    stream: &recorder,
                    completion_key,
                })
                .await?,
        )
    };
    let stream = recorder.finish();
    let capture = match outcome {
        None => Ok(SingletonCapture::Isolated {
            binding: Box::new(
                request
                    .isolation
                    .clone()
                    .ok_or("the isolated route has no admission")?,
            ),
        }),
        Some(SingletonBodyOutcome::Pending { completion }) => {
            let admitted = declaration.admits(OutcomeShape::Deferred).and_then(|()| {
                if matches!(
                    completion.resolved_by,
                    Some(crate::PendingResolver::DeclaredStart(_))
                ) {
                    declaration.admits(OutcomeShape::Done {
                        intents: &[ToolIntentKind::StartProcess],
                    })
                } else {
                    Ok(())
                }
            });
            match admitted {
                Err(refusal) => Ok(SingletonCapture::Refused { refusal }),
                Ok(()) => {
                    let obligation = match &completion.resolved_by {
                        Some(crate::PendingResolver::DeclaredStart(start)) => bind_start(
                            call,
                            cancels_work(&member.policy, Some(completion.on_cancel)),
                            start.request().into_registration(),
                        )
                        .map(Some),
                        _ => Ok(None),
                    };
                    match obligation {
                        Err(refusal) => Ok(SingletonCapture::StartRefused { refusal }),
                        Ok(obligation) => {
                            let source = if completion.resolved_by.is_some() {
                                process_source
                            } else {
                                completion_key
                            }
                            .ok_or("the pending source was not reserved")?
                            .clone();
                            let mut materials = Vec::new();
                            let start = match obligation {
                                Some(obligation) => {
                                    let (reference, entry) = mint(
                                        &owner,
                                        MaterialRole::AttemptOutput,
                                        encode(&obligation)?,
                                    )?;
                                    materials.push(entry);
                                    Some(Box::new(crate::tool_run::PendingStart {
                                        start_key: obligation.start_key().clone(),
                                        obligation: reference,
                                    }))
                                }
                                None => None,
                            };
                            let (metadata, entry) = mint(
                                &owner,
                                MaterialRole::AttemptOutput,
                                encode(&RecordedPending {
                                    completion: *completion,
                                    stream,
                                })?,
                            )?;
                            materials.push(entry);
                            return Ok(crate::tool_run::RunAttemptEntry {
                                call_id: call.call_id.clone(),
                                attempt: ordinal,
                                result: AttemptResult::Pending {
                                    source,
                                    metadata,
                                    start,
                                },
                                materials,
                            });
                        }
                    }
                }
            }
        }
        Some(SingletonBodyOutcome::DeferredStart { start }) => {
            let admitted = declaration.admits(OutcomeShape::Deferred).and_then(|()| {
                declaration.admits(OutcomeShape::Done {
                    intents: &[ToolIntentKind::StartProcess],
                })
            });
            match admitted {
                Err(refusal) => Ok(SingletonCapture::Refused { refusal }),
                Ok(()) => match bind_start(call, cancels_work(&member.policy, None), *start) {
                    Err(refusal) => Ok(SingletonCapture::StartRefused { refusal }),
                    Ok(obligation) => {
                        let source = process_source
                            .ok_or("the process-terminal source was not reserved")?
                            .clone();
                        let (reference, entry) =
                            mint(&owner, MaterialRole::AttemptOutput, encode(&obligation)?)?;
                        return Ok(crate::tool_run::RunAttemptEntry {
                            call_id: call.call_id.clone(),
                            attempt: ordinal,
                            result: AttemptResult::DeferredStart {
                                source,
                                start_key: obligation.start_key().clone(),
                                obligation: reference,
                            },
                            materials: vec![entry],
                        });
                    }
                },
            }
        }
        Some(SingletonBodyOutcome::Deferred { source }) => {
            match declaration.admits(OutcomeShape::Deferred) {
                Ok(()) if completion_key == Some(&source) => Err(source),
                Ok(()) => Ok(SingletonCapture::Refused {
                    refusal: DeclarationRefusal::UnarmedSource,
                }),
                Err(refusal) => Ok(SingletonCapture::Refused { refusal }),
            }
        }
        Some(SingletonBodyOutcome::Done {
            commands,
            output,
            intents,
            start,
        }) => {
            // A declared start is a StartProcess intent of the result.
            let mut declared = intents.clone();
            if start.is_some() && !declared.contains(&ToolIntentKind::StartProcess) {
                declared.push(ToolIntentKind::StartProcess);
            }
            match declaration.admits(OutcomeShape::Done { intents: &declared }) {
                Err(refusal) => Ok(SingletonCapture::Refused { refusal }),
                Ok(()) => match start
                    .map(|start| bind_start(call, cancels_work(&member.policy, None), *start))
                {
                    None => Ok(SingletonCapture::Done {
                        output,
                        commands: commands.into_commands(),
                        intents,
                        stream,
                        start: None,
                    }),
                    Some(Err(refusal)) => Ok(SingletonCapture::StartRefused { refusal }),
                    Some(Ok(obligation)) => {
                        let (reference, entry) =
                            mint(&owner, MaterialRole::AttemptOutput, encode(&obligation)?)?;
                        started.push(entry);
                        Ok(SingletonCapture::Done {
                            output,
                            commands: commands.into_commands(),
                            intents,
                            stream,
                            start: Some(Box::new(SingletonStart {
                                start_key: obligation.start_key().clone(),
                                obligation: reference,
                            })),
                        })
                    }
                },
            }
        }
        Some(SingletonBodyOutcome::RetryableFailure { output, after_ms }) => {
            Ok(SingletonCapture::RetryableFailure {
                output,
                stream,
                after_ms,
            })
        }
        Some(SingletonBodyOutcome::Failed { output }) => {
            Ok(SingletonCapture::Failed { output, stream })
        }
    };
    let (result, materials) = match capture {
        Err(source) => (AttemptResult::Deferred { source }, Vec::new()),
        Ok(capture) => {
            let done = matches!(
                capture,
                SingletonCapture::Done { .. } | SingletonCapture::Isolated { .. }
            );
            let (output, entry) = mint(&owner, MaterialRole::AttemptOutput, encode(&capture)?)?;
            let result = if done {
                AttemptResult::Done { output }
            } else {
                AttemptResult::Failed {
                    output,
                    retryable: matches!(capture, SingletonCapture::RetryableFailure { .. }),
                }
            };
            started.insert(0, entry);
            (result, started)
        }
    };
    Ok(crate::tool_run::RunAttemptEntry {
        call_id: call.call_id.clone(),
        attempt: ordinal,
        result,
        materials,
    })
}
