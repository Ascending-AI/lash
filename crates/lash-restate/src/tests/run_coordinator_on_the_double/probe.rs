use super::*;

impl Probe {
    pub(super) fn new(calls: &[(SingletonToolCall, Kind)]) -> Self {
        Self {
            kinds: calls
                .iter()
                .map(|(call, kind)| (call.call_id.clone(), kind.clone()))
                .collect(),
            materials: None,
            sources: Mutex::new(BTreeMap::new()),
            complete_sources: false,
            streams: BTreeMap::new(),
            cancel: AtomicBool::new(false),
            cancel_before: None,
            cancel_after: None,
            cancelled_calls: Mutex::new(Vec::new()),
            parallel: None,
            body_barrier: None,
            program_release: None,
            parallel_order: Vec::new(),
            parallel_completed: Default::default(),
            parallel_wake: Default::default(),
            retry: Default::default(),
            gate: None,
            unavailable: None,
            gate_open: AtomicBool::new(false),
            gate_after_crash: false,
            gate_wake: Default::default(),
            cancel_at_timer: false,
            handler_attempts: Default::default(),
            replay_delay: None,
            cancel_at_gate: false,
            plugin_host: None,
            state_seed: None,
            plugins: Mutex::new(None),
            executions: Mutex::new(Vec::new()),
            realized: Mutex::new(Vec::new()),
            held: BTreeSet::new(),
            processes: Mutex::new(BTreeMap::new()),
            launches: Mutex::new(Vec::new()),
            discharges: Mutex::new(Vec::new()),
            held_launch: BTreeSet::new(),
            held_after_realization: BTreeSet::new(),
            unrelated: AtomicBool::new(false),
            unrelated_ran: tokio::sync::Notify::new(),
            unrelated_gate: None,
            fault_after_first_intent: None,
            faulted: AtomicBool::new(false),
            seen: Mutex::new(Vec::new()),
            presentations: Mutex::new(Vec::new()),
            presentation_failure: false,
            declaration_drift_on_replay: false,
            emitted: Mutex::new(Vec::new()),
            always_replay: false,
            gates: BTreeMap::new(),
            after_gates: BTreeMap::new(),
            script: None,
        }
    }

    pub(super) fn executions_of(&self, call_id: &ToolCallId) -> usize {
        self.executions
            .lock()
            .unwrap()
            .iter()
            .filter(|(executed, _)| executed == call_id)
            .count()
    }

    pub(super) fn seen(&self) -> Vec<Seen> {
        self.seen.lock().unwrap().clone()
    }

    /// The unrelated effect's step: it runs while the Run drains.
    pub(super) fn run_unrelated(&self) {
        self.seen.lock().unwrap().push(Seen::Unrelated);
        self.unrelated.store(true, Ordering::SeqCst);
        self.unrelated_ran.notify_waiters();
    }
}

#[async_trait::async_trait]
impl SingletonToolHandlers for Probe {
    fn tool_material_store(&self) -> Option<&dyn lash_core::store::ToolMaterialStore> {
        self.materials.as_deref()
    }

    async fn prepare(&self, call: &SingletonToolCall) -> Result<serde_json::Value, String> {
        Ok(serde_json::json!({ "sealed": call.arguments }))
    }

    async fn before_checks(
        &self,
        call: &SingletonToolCall,
        _request: &SingletonPreparedRequest,
    ) -> Result<Vec<AttributedVerdict<BeforeCheckReply>>, String> {
        Ok(vec![AttributedVerdict {
            callback: binding().executable,
            verdict: if self.cancel_before.as_ref() == Some(&call.call_id) {
                BeforeCheckReply::Cancel {
                    cause: check_cancel_cause(),
                }
            } else if matches!(self.kinds[&call.call_id], Kind::Cached) {
                BeforeCheckReply::Cached {
                    output: output_of(&call.call_id),
                }
            } else {
                BeforeCheckReply::Allow
            },
        }])
    }

    async fn execute(&self, attempt: SingletonAttempt<'_>) -> Result<SingletonBodyOutcome, String> {
        self.executions
            .lock()
            .unwrap()
            .push((attempt.call_id.clone(), attempt.attempt));
        if let Some(gate) = self.gates.get(attempt.call_id) {
            gate.wait().await;
        }
        if let Some(program) = &self.program_release
            && program.call_id == *attempt.call_id
        {
            program.run(self).await;
            return Ok(SingletonBodyOutcome::Done {
                commands: Default::default(),
                output: output_of(attempt.call_id),
                intents: Vec::new(),
                start: None,
            });
        }
        if let Some(barrier) = &self.body_barrier
            && self.executions_of(attempt.call_id) == 1
        {
            barrier.wait().await;
        }
        for (ordinal, event) in self
            .streams
            .get(attempt.call_id)
            .into_iter()
            .flatten()
            .enumerate()
        {
            attempt.stream.observe(ShiftObservation {
                key: ReplayKey::new(format!("{}", attempt.call_id)),
                ordinal: u32::try_from(ordinal).unwrap(),
                event: ObservedEvent::Session(event.clone()),
            });
        }
        if let Some(barrier) = &self.parallel {
            tokio::time::timeout(Duration::from_secs(1), barrier.wait())
                .await
                .expect("L01: every body reaches its barrier before any can finish");
        }
        if !self.parallel_order.is_empty() {
            loop {
                let wake = self.parallel_wake.notified();
                tokio::pin!(wake);
                wake.as_mut().enable();
                if self.parallel_order[self.parallel_completed.load(Ordering::SeqCst)]
                    == *attempt.call_id
                {
                    break;
                }
                wake.await;
            }
        }
        if attempt.attempt == AttemptOrdinal::FIRST
            && self
                .gate
                .as_ref()
                .is_some_and(|(held, _)| held == attempt.call_id)
        {
            loop {
                let wake = self.gate_wake.notified();
                tokio::pin!(wake);
                wake.as_mut().enable();
                if self.gate_open.load(Ordering::SeqCst) {
                    break;
                }
                wake.await;
            }
        }
        if let Some(seed) = &self.state_seed {
            assert_eq!(
                attempt.request.state_snapshot.as_ref().unwrap().values,
                seed.plugins[PLUGIN].values
            );
        }
        let call_id = attempt.call_id;
        if self
            .unavailable
            .as_ref()
            .is_some_and(|(failed, restored)| failed == call_id && !restored.load(Ordering::SeqCst))
        {
            return Err("owner-local-X unavailable".to_owned());
        }
        Ok(match &self.kinds[call_id] {
            Kind::Declares(intents) => SingletonBodyOutcome::Done {
                commands: Default::default(),
                output: output_of(call_id),
                intents: intents.clone(),
                start: None,
            },
            Kind::Starts => SingletonBodyOutcome::Done {
                commands: Default::default(),
                output: output_of(call_id),
                intents: Vec::new(),
                start: Some(Box::new(
                    ProcessStartRegistration::of_target(
                        ProcessInput::Engine {
                            kind: "fig4977-index".into(),
                            payload: serde_json::json!({ "call": call_id.to_string() }),
                        },
                        ProcessProvenance::host(),
                        Lifetime::Detached,
                    )
                    .with_start_key(Some(StartKey::for_host(format!("fig4977-{call_id}")))),
                )),
            },
            Kind::Retry { after_ms } if attempt.attempt == AttemptOrdinal::FIRST => {
                SingletonBodyOutcome::RetryableFailure {
                    output: format!("failed {call_id}@1"),
                    after_ms: Some(*after_ms),
                }
            }
            Kind::Stateful { key } => {
                assert!(
                    attempt
                        .request
                        .state_snapshot
                        .as_ref()
                        .unwrap()
                        .values
                        .is_empty(),
                    "admission fixes the body's snapshot even after a sibling publishes"
                );
                SingletonBodyOutcome::Done {
                    output: output_of(call_id),
                    commands: lash_core::plugin::StateCommands::new().apply(
                        key,
                        "append",
                        serde_json::json!(call_id.to_string()),
                    ),
                    intents: Vec::new(),
                    start: None,
                }
            }
            Kind::Failed => SingletonBodyOutcome::Failed {
                output: format!("rejected {call_id}"),
            },
            Kind::Cached => panic!("a cached admission executes no body"),
            Kind::IntentFree | Kind::Retry { .. } => SingletonBodyOutcome::Done {
                commands: Default::default(),
                output: output_of(call_id),
                intents: Vec::new(),
                start: None,
            },
            Kind::Deferred => {
                let source = attempt
                    .completion_key
                    .expect("admission armed the source")
                    .clone();
                self.sources
                    .lock()
                    .unwrap()
                    .insert(call_id.clone(), source.clone());
                SingletonBodyOutcome::Deferred { source }
            }
        })
    }

    async fn after_checks(
        &self,
        call_id: &ToolCallId,
        _capture: &SingletonCapture,
    ) -> Result<Vec<AttributedVerdict<AfterCheckVerdict>>, String> {
        if let Some(gate) = self.after_gates.get(call_id) {
            gate.wait().await;
        }
        if self
            .program_release
            .as_ref()
            .is_some_and(|program| program.call_id == *call_id)
        {
            self.run_unrelated();
        } else if !self.parallel_order.is_empty() {
            let index = self.parallel_completed.fetch_add(1, Ordering::SeqCst);
            assert_eq!(self.parallel_order[index], *call_id);
            self.parallel_wake.notify_waiters();
        }
        Ok(if self.cancel_after.as_ref() == Some(call_id) {
            vec![AttributedVerdict {
                callback: binding().executable,
                verdict: AfterCheckVerdict::Cancel {
                    cause: check_cancel_cause(),
                },
            }]
        } else {
            Vec::new()
        })
    }

    fn plugin_session(&self) -> Option<Arc<lash_core::plugin::PluginSession>> {
        self.plugins.lock().unwrap().clone()
    }

    async fn run_cancel_requested(&self) -> Result<bool, String> {
        Ok(self.cancel.load(Ordering::SeqCst))
    }

    async fn cancel_call(
        &self,
        call_id: &ToolCallId,
        _source: Option<&lash_core::AwaitEventKey>,
    ) -> Result<(), String> {
        let mut calls = self.cancelled_calls.lock().unwrap();
        if !calls.contains(call_id) {
            calls.push(call_id.clone());
        }
        self.gate_open.store(true, Ordering::SeqCst);
        self.gate_wake.notify_waiters();
        Ok(())
    }

    async fn present(
        &self,
        call_id: &ToolCallId,
        capture: &SingletonCapture,
    ) -> Result<String, lash_core::tool_dispatch::SingletonPresentationError> {
        let declared = match capture {
            SingletonCapture::Done { intents, .. } => intents.clone(),
            _ => Vec::new(),
        };
        let realized = self.realized.lock().unwrap();
        for kind in declared {
            assert!(
                realized.contains(&(call_id.clone(), kind)),
                "{call_id}'s declarations settle before its presentation"
            );
        }
        drop(realized);
        if capture.start().is_some() {
            let launched = self.launches.lock().unwrap();
            let discharged = self.discharges.lock().unwrap();
            assert!(
                launched.iter().any(|(id, _)| id == call_id)
                    && discharged.iter().any(|(id, _, _)| id == call_id),
                "{call_id}'s launched start is discharged before its presentation"
            );
        }
        self.presentations.lock().unwrap().push(call_id.clone());
        if self.presentation_failure {
            return Err(
                lash_core::tool_dispatch::SingletonPresentationError::Refused {
                    cause: lash_core::tool_run::HookCause {
                        error_type: "fig4926.presentation_refused".into(),
                        error_version: std::num::NonZeroU32::MIN,
                        payload: serde_json::json!({"reason": "deterministic presentation refusal"}),
                    },
                },
            );
        }
        Ok(format!("fig4880 presented {call_id}"))
    }

    fn emit_stream(&self, call_id: &ToolCallId, stream: &AttemptStream) {
        self.emitted
            .lock()
            .unwrap()
            .push((call_id.clone(), stream.clone()));
    }

    async fn launch_start(
        &self,
        obligation: &DeclaredStartObligation,
    ) -> Result<lash_core::tool_dispatch::StartLaunch, String> {
        self.seen
            .lock()
            .unwrap()
            .push(Seen::LaunchBegin(obligation.call_id.clone()));
        if self.held_launch.contains(&obligation.call_id) {
            // A blocked launch: it holds until the unrelated effect has made
            // progress, which it must while this drains.
            loop {
                let ran = self.unrelated_ran.notified();
                if self.unrelated.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::timeout(Duration::from_millis(50), ran)
                    .await
                    .ok();
            }
        }
        let process = self
            .processes
            .lock()
            .unwrap()
            .entry(obligation.start_key().clone())
            .or_insert_with(|| {
                lash_core::ProcessId::fixture(&format!("fig4977-{}", obligation.call_id))
            })
            .clone();
        self.launches
            .lock()
            .unwrap()
            .push((obligation.call_id.clone(), process.clone()));
        Ok(lash_core::tool_dispatch::StartLaunch::Launched(
            lash_core::ProcessHandleView::new(
                process,
                lash_core::ProcessIdentity::new("fig4977.probe"),
                lash_core::ProcessStatus::Running,
            ),
        ))
    }

    async fn discharge_start(
        &self,
        obligation: &DeclaredStartObligation,
        process_id: &lash_core::ProcessId,
        cancel: bool,
    ) -> Result<(), String> {
        self.discharges.lock().unwrap().push((
            obligation.call_id.clone(),
            process_id.clone(),
            cancel,
        ));
        self.seen
            .lock()
            .unwrap()
            .push(Seen::Discharged(obligation.call_id.clone()));
        Ok(())
    }
}

impl Probe {
    pub(super) async fn record_intents(
        &self,
        call_id: &ToolCallId,
        intents: &[ToolIntentKind],
    ) -> Result<(), String> {
        self.seen
            .lock()
            .unwrap()
            .push(Seen::RealizeBegin(call_id.clone()));
        if self.held.contains(call_id) {
            // A blocked protected declaration: it holds until the unrelated
            // effect has made progress, which it must while this drains.
            loop {
                let ran = self.unrelated_ran.notified();
                if self.unrelated.load(Ordering::SeqCst) {
                    break;
                }
                tokio::time::timeout(Duration::from_millis(50), ran)
                    .await
                    .ok();
            }
        }
        for (position, kind) in intents.iter().enumerate() {
            {
                let mut realized = self.realized.lock().unwrap();
                if !realized.contains(&(call_id.clone(), *kind)) {
                    realized.push((call_id.clone(), *kind));
                }
            }
            if position == 0
                && self.fault_after_first_intent.as_ref() == Some(call_id)
                && !self.faulted.swap(true, Ordering::SeqCst)
            {
                return Err("a fault after the first intent".to_owned());
            }
        }
        if self.held_after_realization.contains(call_id) {
            loop {
                let acknowledged = self.unrelated_ran.notified();
                tokio::pin!(acknowledged);
                acknowledged.as_mut().enable();
                if self.unrelated.load(Ordering::SeqCst) {
                    break;
                }
                acknowledged.await;
            }
        }
        self.seen
            .lock()
            .unwrap()
            .push(Seen::RealizeEnd(call_id.clone()));
        Ok(())
    }
}

#[async_trait::async_trait]
impl lash_core::tool_dispatch::ToolRealizer for Probe {
    async fn realize(
        &self,
        request: lash_core::tool_dispatch::RealizationRequest,
        _scoped: lash_core::ScopedEffectController<'_>,
    ) -> Result<lash_core::tool_dispatch::RealizationReceipt, lash_core::RuntimeEffectControllerError>
    {
        let intents = match self.kinds.get(&request.call_id) {
            Some(Kind::Declares(intents)) => intents.clone(),
            _ => Vec::new(),
        };
        self.record_intents(&request.call_id, &intents)
            .await
            .map_err(|message| {
                lash_core::RuntimeEffectControllerError::from(
                    lash_core::PluginError::attempt_fault(message),
                )
            })?;
        Ok(Default::default())
    }
}
