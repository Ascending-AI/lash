use crate::plugin::PluginError;

use super::events::{ProcessAwaitOutput, ProcessEvent};
use super::model::{
    ProcessCancelSummary, ProcessCompletionOutcome, ProcessHandleSummary, ProcessListMode,
    ProcessRecord, ProcessRegistration, ProcessStartOptions, ProcessStartRequest,
};
use super::op_scope::ProcessOpScope;

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
    /// observes (ADR 0019). Closure for work lash never executes — a
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
    ) -> Result<Vec<ProcessRecord>, PluginError>;

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

    async fn cancel_visible(
        &self,
        session_id: &str,
        process_id: &str,
        scope: ProcessOpScope<'_>,
    ) -> Result<ProcessRecord, PluginError> {
        self.validate_visible(session_id, &[process_id.to_string()], scope.clone())
            .await?;
        self.cancel(session_id, process_id, scope).await
    }

    async fn cancel_all_visible(
        &self,
        session_id: &str,
        scope: ProcessOpScope<'_>,
    ) -> Result<Vec<ProcessCancelSummary>, PluginError> {
        let entries = self
            .list_visible(session_id, ProcessListMode::Live, scope.clone())
            .await?;
        let mut cancelled = Vec::new();
        for record in entries {
            if record.is_terminal() {
                continue;
            }
            cancelled.push(
                self.cancel(session_id, &record.id, scope.clone())
                    .await
                    .map(ProcessCancelSummary::from_record)?,
            );
        }
        Ok(cancelled)
    }

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
    ) -> Result<Vec<ProcessRecord>, PluginError> {
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
        ProcessAwaitOutput, ProcessEvent, ProcessInput, ProcessProvenance, ProcessRegistration,
        ProcessStatus,
    };

    struct RecordingProcessService {
        visible: HashSet<String>,
        validate_calls: Mutex<Vec<Vec<String>>>,
        cancel_calls: Mutex<Vec<String>>,
        visible_entries: Vec<ProcessRecord>,
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
                    ProcessRecord::from_registration(ProcessRegistration::new(
                        process_id,
                        ProcessInput::External {
                            metadata: json!(null),
                        },
                        crate::RecoveryDisposition::ExternallyOwned,
                        ProcessProvenance::host(),
                    ))
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
        ) -> Result<Vec<ProcessRecord>, PluginError> {
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
        record.status = ProcessStatus::Cancelled;
        record.outcome = Some(ProcessAwaitOutput::Cancelled {
            message: "cancelled".to_string(),
            raw: None,
            control: None,
        });
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
    async fn cancel_visible_validates_visibility_and_calls_primitive() {
        let service =
            RecordingProcessService::new(["process-1".to_string()], cancelled_record("process-1"));

        let record = service
            .cancel_visible(
                "session-1",
                "process-1",
                test_process_scope("cancel-visible"),
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
    async fn cancel_visible_rejects_invisible_process_without_cancel() {
        let service = RecordingProcessService::new(Vec::<String>::new(), cancelled_record("p1"));

        let err = service
            .cancel_visible("session-1", "p1", test_process_scope("cancel-hidden"))
            .await
            .expect_err("hidden process should be rejected");

        assert!(err.to_string().contains("not visible"), "{err}");
        assert!(service.cancel_calls().is_empty());
    }

    #[tokio::test]
    async fn cancel_all_visible_cancels_each_visible_live_process() {
        let service = RecordingProcessService::new(
            ["process-1".to_string(), "process-2".to_string()],
            cancelled_record("template"),
        )
        .with_visible_entries(["process-1".to_string(), "process-2".to_string()]);
        let summaries = service
            .cancel_all_visible("session-1", test_process_scope("cancel-all"))
            .await
            .expect("cancel all visible");

        assert_eq!(
            summaries
                .iter()
                .map(|summary| summary.process_id.as_str())
                .collect::<Vec<_>>(),
            vec!["process-1", "process-2"]
        );
        assert!(service.validate_calls().is_empty());
        assert_eq!(
            service.cancel_calls(),
            vec!["process-1".to_string(), "process-2".to_string()]
        );
    }
}
