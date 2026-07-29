use crate::plugin::PluginError;

use super::events::{ProcessAwaitOutput, ProcessEvent};
use super::model::{
    ProcessCancelSummary, ProcessCompletionOutcome, ProcessHandleGrantEntry, ProcessHandleSummary,
    ProcessListMode, ProcessRecord, ProcessRegistration, ProcessStartOptions, ProcessStartRequest,
};
use super::op_scope::ProcessOpScope;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessCancelSource {
    Tool,
    Process,
    HostApi,
}

#[derive(Clone)]
pub struct ProcessCancelRequest<'scope> {
    pub session_id: &'scope str,
    pub process_id: &'scope str,
    pub handle: Option<serde_json::Value>,
    pub scope: ProcessOpScope<'scope>,
    pub reason: Option<String>,
    pub source: ProcessCancelSource,
}

impl<'scope> ProcessCancelRequest<'scope> {
    pub fn new(
        session_id: &'scope str,
        process_id: &'scope str,
        scope: ProcessOpScope<'scope>,
        source: ProcessCancelSource,
    ) -> Self {
        Self {
            session_id,
            process_id,
            handle: None,
            scope,
            reason: None,
            source,
        }
    }

    pub fn with_handle(mut self, handle: serde_json::Value) -> Self {
        self.handle = Some(handle);
        self
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

#[derive(Clone)]
pub struct ProcessCancelAllRequest<'scope> {
    pub session_id: &'scope str,
    pub scope: ProcessOpScope<'scope>,
    pub source: ProcessCancelSource,
    pub reason: Option<String>,
}

impl<'scope> ProcessCancelAllRequest<'scope> {
    pub fn new(
        session_id: &'scope str,
        scope: ProcessOpScope<'scope>,
        source: ProcessCancelSource,
    ) -> Self {
        Self {
            session_id,
            scope,
            source,
            reason: None,
        }
    }

    pub fn with_reason(mut self, reason: impl Into<String>) -> Self {
        self.reason = Some(reason.into());
        self
    }
}

#[async_trait::async_trait]
pub trait ProcessCancelAbility: Send + Sync {
    async fn cancel(
        &self,
        processes: &dyn ProcessService,
        request: ProcessCancelRequest<'_>,
    ) -> Result<ProcessRecord, PluginError>;

    async fn cancel_summary(
        &self,
        processes: &dyn ProcessService,
        request: ProcessCancelRequest<'_>,
    ) -> Result<ProcessCancelSummary, PluginError> {
        self.cancel(processes, request)
            .await
            .map(ProcessCancelSummary::from_record)
    }

    async fn cancel_all_visible(
        &self,
        processes: &dyn ProcessService,
        request: ProcessCancelAllRequest<'_>,
    ) -> Result<Vec<ProcessCancelSummary>, PluginError> {
        let entries = processes
            .list_visible(
                request.session_id,
                ProcessListMode::Live,
                request.scope.clone(),
            )
            .await?;
        let mut cancelled = Vec::new();
        for (grant, record) in entries {
            if record.is_terminal() {
                continue;
            }
            let mut cancel_request = ProcessCancelRequest::new(
                request.session_id,
                &grant.process_id,
                request.scope.clone(),
                request.source,
            );
            if let Some(reason) = request.reason.clone() {
                cancel_request = cancel_request.with_reason(reason);
            }
            cancelled.push(self.cancel_summary(processes, cancel_request).await?);
        }
        Ok(cancelled)
    }
}

#[derive(Clone, Copy, Debug, Default)]
pub struct DefaultProcessCancelAbility;

#[async_trait::async_trait]
impl ProcessCancelAbility for DefaultProcessCancelAbility {
    async fn cancel(
        &self,
        processes: &dyn ProcessService,
        request: ProcessCancelRequest<'_>,
    ) -> Result<ProcessRecord, PluginError> {
        let process_ids = [request.process_id.to_string()];
        processes
            .validate_visible(request.session_id, &process_ids, request.scope.clone())
            .await?;
        processes
            .cancel(request.session_id, request.process_id, request.scope)
            .await
    }
}

#[async_trait::async_trait]
pub trait ProcessService: Send + Sync {
    async fn start_from_request(
        &self,
        session_id: &str,
        request: ProcessStartRequest,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessHandleSummary, PluginError> {
        let _ = (session_id, request, scope);
        Err(PluginError::Session(
            "process start request composition is unavailable in this service".to_string(),
        ))
    }

    async fn start(
        &self,
        session_id: &str,
        registration: ProcessRegistration,
        options: ProcessStartOptions,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessRecord, PluginError>;

    /// Write the terminal outcome for an Externally-Owned process the session
    /// holds a grant for (ADR 0019). Closure for work lash never executes — a
    /// detached command records its immediately-terminal launch fact here. Only
    /// Externally-Owned rows may be completed this way. The typed completion
    /// outcome tells the caller whether this write committed, replayed an
    /// identical terminal, or lost to a different stored terminal.
    async fn complete_external(
        &self,
        session_id: &str,
        process_id: &str,
        await_output: ProcessAwaitOutput,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessCompletionOutcome, PluginError> {
        let _ = (session_id, process_id, await_output, scope);
        Err(PluginError::Session(
            "external process completion is unavailable in this service".to_string(),
        ))
    }

    async fn await_process(
        &self,
        process_id: &str,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessAwaitOutput, PluginError>;

    async fn list_visible(
        &self,
        session_id: &str,
        mode: ProcessListMode,
        scope: ProcessOpScope<'_>,
    ) -> Result<Vec<ProcessHandleGrantEntry>, PluginError>;

    async fn validate_visible(
        &self,
        session_id: &str,
        process_ids: &[String],
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError>;

    async fn cancel(
        &self,
        session_id: &str,
        process_id: &str,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessRecord, PluginError>;

    async fn signal(
        &self,
        session_id: &str,
        process_id: &str,
        signal_name: String,
        signal_id: String,
        payload: serde_json::Value,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessEvent, PluginError>;

    async fn transfer(
        &self,
        from_session_id: &str,
        to_session_id: &str,
        process_ids: Vec<String>,
        scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError>;
}

pub struct UnavailableProcessService;

#[async_trait::async_trait]
impl ProcessService for UnavailableProcessService {
    async fn start(
        &self,
        _session_id: &str,
        _registration: ProcessRegistration,
        _options: ProcessStartOptions,
        _scope: ProcessOpScope<'_>,
    ) -> Result<ProcessRecord, PluginError> {
        Err(PluginError::Session(
            "processes are unavailable in this runtime".to_string(),
        ))
    }

    async fn await_process(
        &self,
        _process_id: &str,
        _scope: ProcessOpScope<'_>,
    ) -> Result<ProcessAwaitOutput, PluginError> {
        Err(PluginError::Session(
            "process awaiting is unavailable in this runtime".to_string(),
        ))
    }

    async fn list_visible(
        &self,
        _session_id: &str,
        _mode: ProcessListMode,
        _scope: ProcessOpScope<'_>,
    ) -> Result<Vec<ProcessHandleGrantEntry>, PluginError> {
        Err(PluginError::Session(
            "process registry is unavailable in this runtime".to_string(),
        ))
    }

    async fn validate_visible(
        &self,
        _session_id: &str,
        _process_ids: &[String],
        _scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        Err(PluginError::Session(
            "process handle validation is unavailable in this runtime".to_string(),
        ))
    }

    async fn cancel(
        &self,
        _session_id: &str,
        _process_id: &str,
        _scope: ProcessOpScope<'_>,
    ) -> Result<ProcessRecord, PluginError> {
        Err(PluginError::Session(
            "process registry is unavailable in this runtime".to_string(),
        ))
    }

    async fn signal(
        &self,
        _session_id: &str,
        _process_id: &str,
        _signal_name: String,
        _signal_id: String,
        _payload: serde_json::Value,
        _scope: ProcessOpScope<'_>,
    ) -> Result<ProcessEvent, PluginError> {
        Err(PluginError::Session(
            "process signalling is unavailable in this runtime".to_string(),
        ))
    }

    async fn transfer(
        &self,
        _from_session_id: &str,
        _to_session_id: &str,
        process_ids: Vec<String>,
        _scope: ProcessOpScope<'_>,
    ) -> Result<(), PluginError> {
        if process_ids.is_empty() {
            return Ok(());
        }
        Err(PluginError::Session(
            "process handle transfer is unavailable in this runtime".to_string(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::{Arc, Mutex};

    use serde_json::json;

    use super::*;
    use crate::{
        ProcessAwaitOutput, ProcessEvent, ProcessHandleDescriptor, ProcessHandleGrant,
        ProcessInput, ProcessProvenance, ProcessRegistration, ProcessStatus,
    };

    struct RecordingProcessService {
        visible: HashSet<String>,
        validate_calls: Mutex<Vec<Vec<String>>>,
        cancel_calls: Mutex<Vec<String>>,
        visible_entries: Vec<ProcessHandleGrantEntry>,
        record: ProcessRecord,
    }

    impl RecordingProcessService {
        fn new(visible: impl IntoIterator<Item = String>, record: ProcessRecord) -> Self {
            Self {
                visible: visible.into_iter().collect(),
                validate_calls: Mutex::new(Vec::new()),
                cancel_calls: Mutex::new(Vec::new()),
                visible_entries: Vec::new(),
                record,
            }
        }

        fn with_visible_entries(mut self, process_ids: impl IntoIterator<Item = String>) -> Self {
            self.visible_entries = process_ids
                .into_iter()
                .map(|process_id| {
                    (
                        ProcessHandleGrant {
                            session_id: "session-1".to_string(),
                            process_id: process_id.clone(),
                            descriptor: ProcessHandleDescriptor::new(
                                Some("test"),
                                Some(process_id.clone()),
                            ),
                        },
                        ProcessRecord::from_registration(ProcessRegistration::new(
                            process_id,
                            ProcessInput::External {
                                metadata: json!(null),
                            },
                            crate::RecoveryDisposition::ExternallyOwned,
                            ProcessProvenance::host(),
                        )),
                    )
                })
                .collect();
            self
        }

        fn validate_calls(&self) -> Vec<Vec<String>> {
            self.validate_calls.lock().expect("validate calls").clone()
        }

        fn cancel_calls(&self) -> Vec<String> {
            self.cancel_calls.lock().expect("cancel calls").clone()
        }
    }

    #[derive(Default)]
    struct RecordingCancelAbility {
        requests: Mutex<Vec<(String, ProcessCancelSource, Option<String>)>>,
    }

    impl RecordingCancelAbility {
        fn requests(&self) -> Vec<(String, ProcessCancelSource, Option<String>)> {
            self.requests.lock().expect("cancel requests").clone()
        }
    }

    #[async_trait::async_trait]
    impl ProcessCancelAbility for RecordingCancelAbility {
        async fn cancel(
            &self,
            processes: &dyn ProcessService,
            request: ProcessCancelRequest<'_>,
        ) -> Result<ProcessRecord, PluginError> {
            self.requests.lock().expect("cancel requests").push((
                request.process_id.to_string(),
                request.source,
                request.reason.clone(),
            ));
            DefaultProcessCancelAbility.cancel(processes, request).await
        }
    }

    #[async_trait::async_trait]
    impl ProcessService for RecordingProcessService {
        async fn start(
            &self,
            _session_id: &str,
            _registration: ProcessRegistration,
            _options: ProcessStartOptions,
            _scope: ProcessOpScope<'_>,
        ) -> Result<ProcessRecord, PluginError> {
            Err(PluginError::Session("start not implemented".to_string()))
        }

        async fn await_process(
            &self,
            _process_id: &str,
            _scope: ProcessOpScope<'_>,
        ) -> Result<ProcessAwaitOutput, PluginError> {
            Err(PluginError::Session("await not implemented".to_string()))
        }

        async fn list_visible(
            &self,
            _session_id: &str,
            _mode: ProcessListMode,
            _scope: ProcessOpScope<'_>,
        ) -> Result<Vec<ProcessHandleGrantEntry>, PluginError> {
            Ok(self.visible_entries.clone())
        }

        async fn validate_visible(
            &self,
            _session_id: &str,
            process_ids: &[String],
            _scope: ProcessOpScope<'_>,
        ) -> Result<(), PluginError> {
            self.validate_calls
                .lock()
                .expect("validate calls")
                .push(process_ids.to_vec());
            if let Some(missing) = process_ids
                .iter()
                .find(|process_id| !self.visible.contains(*process_id))
            {
                return Err(PluginError::Session(format!(
                    "process handle `{missing}` is not visible"
                )));
            }
            Ok(())
        }

        async fn cancel(
            &self,
            _session_id: &str,
            process_id: &str,
            _scope: ProcessOpScope<'_>,
        ) -> Result<ProcessRecord, PluginError> {
            self.cancel_calls
                .lock()
                .expect("cancel calls")
                .push(process_id.to_string());
            let mut record = self.record.clone();
            record.id = process_id.to_string();
            Ok(record)
        }

        async fn signal(
            &self,
            _session_id: &str,
            _process_id: &str,
            _signal_name: String,
            _signal_id: String,
            _payload: serde_json::Value,
            _scope: ProcessOpScope<'_>,
        ) -> Result<ProcessEvent, PluginError> {
            Err(PluginError::Session("signal not implemented".to_string()))
        }

        async fn transfer(
            &self,
            _from_session_id: &str,
            _to_session_id: &str,
            _process_ids: Vec<String>,
            _scope: ProcessOpScope<'_>,
        ) -> Result<(), PluginError> {
            Err(PluginError::Session("transfer not implemented".to_string()))
        }
    }

    fn cancelled_record(process_id: &str) -> ProcessRecord {
        let mut record = ProcessRecord::from_registration(ProcessRegistration::new(
            process_id,
            ProcessInput::External {
                metadata: json!(null),
            },
            crate::RecoveryDisposition::ExternallyOwned,
            ProcessProvenance::host(),
        ));
        record.status = ProcessStatus::Cancelled {
            await_output: ProcessAwaitOutput::Cancelled {
                message: "cancelled".to_string(),
                raw: None,
                control: None,
            },
        };
        record
    }

    fn test_process_scope(id: &str) -> ProcessOpScope<'static> {
        ProcessOpScope::new(
            crate::ScopedEffectController::shared(
                Arc::new(crate::InlineRuntimeEffectController::default()),
                crate::ExecutionScope::runtime_operation(id),
            )
            .expect("test execution scope"),
        )
    }

    #[tokio::test]
    async fn default_process_cancel_ability_validates_visibility_and_calls_primitive() {
        let service =
            RecordingProcessService::new(["process-1".to_string()], cancelled_record("process-1"));

        let record = DefaultProcessCancelAbility
            .cancel(
                &service,
                ProcessCancelRequest::new(
                    "session-1",
                    "process-1",
                    test_process_scope("cancel-visible"),
                    ProcessCancelSource::HostApi,
                ),
            )
            .await
            .expect("cancel process");

        assert_eq!(record.status.label(), "cancelled");
        assert_eq!(
            service.validate_calls(),
            vec![vec!["process-1".to_string()]]
        );
        assert_eq!(service.cancel_calls(), vec!["process-1".to_string()]);
    }

    #[tokio::test]
    async fn default_process_cancel_ability_rejects_invisible_process_without_cancel() {
        let service = RecordingProcessService::new(Vec::<String>::new(), cancelled_record("p1"));

        let err = DefaultProcessCancelAbility
            .cancel(
                &service,
                ProcessCancelRequest::new(
                    "session-1",
                    "p1",
                    test_process_scope("cancel-hidden"),
                    ProcessCancelSource::Tool,
                ),
            )
            .await
            .expect_err("hidden process should be rejected");

        assert!(err.to_string().contains("not visible"), "{err}");
        assert!(service.cancel_calls().is_empty());
    }

    #[tokio::test]
    async fn process_cancel_ability_cancel_all_visible_uses_same_cancel_path() {
        let service = RecordingProcessService::new(
            ["process-1".to_string(), "process-2".to_string()],
            cancelled_record("template"),
        )
        .with_visible_entries(["process-1".to_string(), "process-2".to_string()]);
        let ability = RecordingCancelAbility::default();

        let summaries = ability
            .cancel_all_visible(
                &service,
                ProcessCancelAllRequest::new(
                    "session-1",
                    test_process_scope("cancel-all"),
                    ProcessCancelSource::Tool,
                )
                .with_reason("requested by tool"),
            )
            .await
            .expect("cancel all visible");

        assert_eq!(
            summaries
                .iter()
                .map(|summary| summary.process_id.as_str())
                .collect::<Vec<_>>(),
            vec!["process-1", "process-2"]
        );
        assert_eq!(
            ability.requests(),
            vec![
                (
                    "process-1".to_string(),
                    ProcessCancelSource::Tool,
                    Some("requested by tool".to_string())
                ),
                (
                    "process-2".to_string(),
                    ProcessCancelSource::Tool,
                    Some("requested by tool".to_string())
                )
            ]
        );
        assert_eq!(
            service.validate_calls(),
            vec![vec!["process-1".to_string()], vec!["process-2".to_string()]]
        );
        assert_eq!(
            service.cancel_calls(),
            vec!["process-1".to_string(), "process-2".to_string()]
        );
    }
}
