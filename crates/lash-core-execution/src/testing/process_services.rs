//! Process services supplied by execution fixtures.
use super::*;

#[async_trait::async_trait]
impl crate::ProcessService for EffectBackedProcessService {
    async fn list_visible_for_attempt(
        &self,
        owner: &crate::RuntimeOwner,
        mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::facade_support::ObservedProcess>, crate::PluginError> {
        let session_id = crate::plugin::require_session_owner(owner, "list_visible_for_attempt")?;
        let records = match mode {
            crate::ProcessListMode::Live => self.registry.list_live_observed_by(session_id).await,
            crate::ProcessListMode::All => {
                self.registry
                    .list_observed_by(
                        session_id,
                        &crate::ProcessListFilter {
                            status: crate::ProcessStatusFilter::Any,
                            ..Default::default()
                        },
                    )
                    .await
            }
        }?;
        crate::facade_support::observe_process_records(
            &crate::facade_support::ProcessWorkObserver::new(Arc::clone(&self.registry)),
            records,
        )
        .await
    }

    async fn start_from_request(
        &self,
        _session_id: &SessionId,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleView, crate::PluginError> {
        let observers = request.observers.clone();
        let registration = admitted_registration(request.into_registration(), &scope)?;
        let command = crate::ProcessCommand::Start {
            registration,
            observers: observers.into_iter().collect(),
            execution_context: Box::new(crate::ProcessExecutionContext::default()),
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Start { record, .. } => {
                Ok(crate::ProcessHandleView::from_observed(*record))
            }
            _ => unreachable!("start command returns start outcome"),
        }
    }

    async fn stage_recorded_start(
        &self,
        owner: &crate::RuntimeOwner,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::StagedProcessStart, crate::PluginError> {
        crate::plugin::require_session_owner(owner, "stage_recorded_start")?;
        let observers = request.observers.clone();
        let registration = admitted_registration(request.into_registration(), &scope)?;
        Ok(self
            .stager()?
            .stage_start(
                registration,
                observers.into_iter().collect(),
                crate::ProcessExecutionContext::default(),
            )
            .await?)
    }

    async fn start(
        &self,
        _session_id: &SessionId,
        registration: crate::ProcessStartRegistration,
        options: crate::ProcessStartOptions,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::facade_support::ObservedProcess, crate::PluginError> {
        let registration = admitted_registration(registration, &scope)?;
        let command = crate::ProcessCommand::Start {
            registration,
            observers: options.initial_observers.into_iter().collect(),
            execution_context: Box::new(crate::ProcessExecutionContext::default()),
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Start { record, .. } => Ok(*record),
            _ => unreachable!("start command returns start outcome"),
        }
    }

    async fn await_process(
        &self,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, crate::PluginError> {
        let command = crate::ProcessCommand::Await {
            process_id: process_id.clone(),
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Await { output } => Ok(*output),
            _ => unreachable!("await command returns await outcome"),
        }
    }

    async fn list_visible(
        &self,
        session_id: &SessionId,
        mode: crate::ProcessListMode,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<crate::facade_support::ObservedProcess>, crate::PluginError> {
        let command = crate::ProcessCommand::List {
            selection: crate::ProcessListSelection::Observed {
                session_scope: crate::SessionScope::new(session_id),
                mode,
            },
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::List { entries } => Ok(entries),
            _ => unreachable!("list command returns list outcome"),
        }
    }

    async fn validate_visible(
        &self,
        owner: &crate::RuntimeOwner,
        process_ids: &[ProcessId],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        if process_ids.is_empty() {
            return Ok(());
        }
        match self
            .execute(
                scope,
                crate::ProcessCommand::ValidateVisible {
                    owner: owner.clone(),
                    process_ids: process_ids.to_vec(),
                },
            )
            .await?
        {
            crate::ProcessEffectOutcome::ValidateVisible { not_visible: None } => Ok(()),
            crate::ProcessEffectOutcome::ValidateVisible {
                not_visible: Some(process_id),
            } => Err(crate::PluginError::ProcessNotVisible { process_id }),
            _ => unreachable!("visibility command returns visibility outcome"),
        }
    }

    async fn cancel(
        &self,
        _owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::facade_support::ObservedProcess, crate::PluginError> {
        let command = Self::cancel_command(
            process_id,
            crate::CancelOrigin::OperatorRequested,
            serde_json::to_string(scope.effect_controller.execution_scope())
                .expect("serializable effect scope"),
            None,
        );
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Cancel { record } => Ok(*record),
            _ => unreachable!("cancel command returns cancel outcome"),
        }
    }

    async fn cancel_recorded_intent(
        &self,
        _owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::facade_support::ObservedProcess, crate::PluginError> {
        let command = Self::cancel_command(
            process_id,
            crate::CancelOrigin::ModelRequested,
            identity.replay_key.clone(),
            Some(crate::RuntimeReplayAttribution::ToolIntent(identity)),
        );
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Cancel { record } => Ok(*record),
            _ => unreachable!("cancel command returns cancel outcome"),
        }
    }

    async fn transfer(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        let command = crate::ProcessCommand::Transfer {
            from_scope: crate::SessionScope::new(from_session_id),
            to_scope: crate::SessionScope::new(to_session_id),
            process_ids,
        };
        match self.execute(scope, command).await? {
            crate::ProcessEffectOutcome::Transfer => Ok(()),
            _ => unreachable!("transfer command returns transfer outcome"),
        }
    }

    async fn stage_bound(
        &self,
        _registration: crate::ProcessStartRegistration,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::StagedProcessStart, crate::PluginError> {
        Err(crate::PluginError::Session(
            "operation is absent from this fixture".into(),
        ))
    }
    async fn cancel_bound(
        &self,
        _process_id: &crate::ProcessId,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(
            "operation is absent from this fixture".into(),
        ))
    }
    async fn release_consumer_hold(
        &self,
        _process_id: &crate::ProcessId,
        _key: &str,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(
            "operation is absent from this fixture".into(),
        ))
    }
    async fn abandon_consumer_hold(
        &self,
        _key: &str,
        _owner: &crate::ScopeId,
    ) -> Result<Vec<crate::ProcessId>, crate::PluginError> {
        Err(crate::PluginError::Session(
            "operation is absent from this fixture".into(),
        ))
    }
}

#[async_trait::async_trait]
impl crate::ProcessService for MockSessionManager {
    async fn stage_recorded_start(
        &self,
        owner: &crate::RuntimeOwner,
        request: crate::ProcessStartRequest,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::StagedProcessStart, PluginError> {
        crate::plugin::require_session_owner(owner, "stage_recorded_start")?;
        let observers = request.observers.clone();
        let registration = admitted_registration(request.into_registration(), &scope)?;
        Ok(self
            .stager()?
            .stage_start(
                registration,
                observers.into_iter().collect(),
                crate::ProcessExecutionContext::default(),
            )
            .await?)
    }

    async fn start(
        &self,
        _session_id: &SessionId,
        registration: crate::ProcessStartRegistration,
        options: crate::ProcessStartOptions,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::facade_support::ObservedProcess, PluginError> {
        let registration = admitted_registration(registration, &scope)?
            .stating_input()
            .map_err(|registration| {
                PluginError::Session(format!(
                    "{} names a definition, which this mock executor cannot resolve",
                    registration.refusal_name()
                ))
            })?;
        // This mock stands in as the executor, so it completes the row under
        // the workflow-key authority an executing substrate holds.
        let observers = options.initial_observers;
        let id = self
            .registry()?
            .register_process_with_observers(registration, &observers)
            .await?
            .id;
        let authority = crate::ProcessCompletionAuthority::workflow_key(&id);
        let record = self
            .registry()?
            .complete_process(
                &id,
                crate::ProcessAwaitOutput::from_tool_output(crate::ToolCallOutput::success(
                    serde_json::json!({
                        "state": "completed"
                    }),
                )),
                authority,
            )
            .await
            .map(crate::ProcessCompletionOutcome::into_record)?;
        crate::runtime::process::ProcessWorkObserver::new(Arc::clone(self.registry()?))
            .observed(record)
            .await
    }

    async fn await_process(
        &self,
        process_id: &ProcessId,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessAwaitOutput, PluginError> {
        crate::NoProcessWork::for_registry(Arc::clone(self.registry()?))
            .await_terminal(process_id)
            .await
    }

    async fn list_visible(
        &self,
        session_id: &SessionId,
        mode: crate::ProcessListMode,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<Vec<crate::facade_support::ObservedProcess>, PluginError> {
        let _ = scope;
        let records = match mode {
            crate::ProcessListMode::Live => {
                self.registry()?.list_live_observed_by(session_id).await
            }
            crate::ProcessListMode::All => {
                self.registry()?
                    .list_observed_by(
                        session_id,
                        &crate::ProcessListFilter {
                            status: crate::ProcessStatusFilter::Any,
                            ..Default::default()
                        },
                    )
                    .await
            }
        }?;
        crate::runtime::process::ProcessWorkObserver::new(Arc::clone(self.registry()?))
            .observe_records(records)
            .await
    }

    async fn validate_visible(
        &self,
        owner: &crate::RuntimeOwner,
        handle_ids: &[ProcessId],
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        let _ = scope;
        let session_id = crate::plugin::require_session_owner(owner, "validate_visible")?;
        for handle_id in handle_ids {
            match self
                .registry()?
                .is_observer(session_id, &ProcessId::from(handle_id))
                .await
            {
                Ok(true) | Err(PluginError::ProcessNoLongerRetained { .. }) => {}
                Ok(false) => {
                    return Err(PluginError::Session(format!(
                        "process handle `{handle_id}` is not live or visible in this session"
                    )));
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    async fn cancel(
        &self,
        _owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::facade_support::ObservedProcess, PluginError> {
        let registry = self.registry()?;
        let process_id = registry.require_process_id(process_id).await?;
        let record = registry
            .request_process_cancel(
                &process_id,
                crate::CancelOrigin::OperatorRequested,
                serde_json::to_string(_scope.effect_controller.execution_scope())
                    .expect("serializable effect scope"),
                None,
            )
            .await?;
        crate::runtime::process::ProcessWorkObserver::new(Arc::clone(registry))
            .observed(record)
            .await
    }

    async fn cancel_recorded_intent(
        &self,
        _owner: &crate::RuntimeOwner,
        process_id: &ProcessId,
        identity: crate::ToolIntentIdentity,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::facade_support::ObservedProcess, PluginError> {
        let registry = self.registry()?;
        let process_id = registry.require_process_id(process_id).await?;
        let record = registry
            .request_process_cancel(
                &process_id,
                crate::CancelOrigin::ModelRequested,
                identity.replay_key.clone(),
                Some(crate::RuntimeReplayAttribution::ToolIntent(identity)),
            )
            .await?;
        crate::runtime::process::ProcessWorkObserver::new(Arc::clone(registry))
            .observed(record)
            .await
    }

    async fn transfer(
        &self,
        from_session_id: &SessionId,
        to_session_id: &SessionId,
        process_ids: Vec<ProcessId>,
        scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        let _ = scope;
        self.registry()?
            .transfer_observers(
                from_session_id,
                to_session_id,
                &process_ids,
                crate::ProcessObserverBy::host("mock-transfer"),
            )
            .await
    }

    async fn list_visible_for_attempt(
        &self,
        _owner: &crate::RuntimeOwner,
        _mode: crate::ProcessListMode,
    ) -> Result<Vec<crate::facade_support::ObservedProcess>, crate::PluginError> {
        Err(crate::PluginError::Session(
            "operation is absent from this fixture".into(),
        ))
    }
    async fn start_from_request(
        &self,
        _session_id: &crate::SessionId,
        _request: crate::ProcessStartRequest,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::ProcessHandleView, crate::PluginError> {
        Err(crate::PluginError::Session(
            "operation is absent from this fixture".into(),
        ))
    }
    async fn stage_bound(
        &self,
        _registration: crate::ProcessStartRegistration,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<crate::StagedProcessStart, crate::PluginError> {
        Err(crate::PluginError::Session(
            "operation is absent from this fixture".into(),
        ))
    }
    async fn cancel_bound(
        &self,
        _process_id: &crate::ProcessId,
        _scope: crate::ProcessOpScope<'_>,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(
            "operation is absent from this fixture".into(),
        ))
    }
    async fn release_consumer_hold(
        &self,
        _process_id: &crate::ProcessId,
        _key: &str,
    ) -> Result<(), crate::PluginError> {
        Err(crate::PluginError::Session(
            "operation is absent from this fixture".into(),
        ))
    }
    async fn abandon_consumer_hold(
        &self,
        _key: &str,
        _owner: &crate::ScopeId,
    ) -> Result<Vec<crate::ProcessId>, crate::PluginError> {
        Err(crate::PluginError::Session(
            "operation is absent from this fixture".into(),
        ))
    }
}
